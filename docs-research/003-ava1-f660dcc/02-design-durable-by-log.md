# Design: durable-by-log small files (receiver, console and engine)

Status: proposed for `ava1`, replaces per-file fsync for files below `LARGE_CUTOFF`. Wire
protocol unchanged except two optional extensions (§6). Decision taken here: `JobDone` is sent
when the log is durable; files settle behind it, and the engine shows that (§5). Reasoning in
`01-whole-system-review.md` §3.

## 1. Goal and invariants

Goal: a batch of N small files costs two fsyncs (log, journal), not N + D + 1, so tiny-file
throughput is bounded by file creation, not by the drive's fsync rate.

Invariants that must hold at every crash point:

- I1 A file reported `Durable` can always be reproduced by the receiver alone: its bytes are
  on stable storage either in the file itself (swept) or in a log segment named by a durable
  journal record.
- I2 `Durable` and `JobDone` are never sent before the journal record that proves I1 is fsynced.
- I3 A log segment is deleted only after every file it holds is swept and the sweep is
  journaled durably.
- I4 Large files (`>= LARGE_CUTOFF`) are unchanged: part file + outboard + commit.
- I5 Recovery is idempotent: re-materialising a file that already exists with the right bytes
  changes nothing; one that exists with wrong bytes is rewritten.

## 2. On-disk format

`<job dir>/pack.<n>` (n from 0, u32, never reused within a job), preallocated to
`PACK_SEGMENT` = 64 MiB (`posix_fallocate`, same policy as part files), appended sequentially:

```
magic "AVA1PCK1" (8)
record: u32le len ‖ u8 kind ‖ body ‖ u32le crc32c(kind ‖ body)     len = 1 + body length
  kind 1 = PackFile: body = BundleRecord encoding as received (file_id, root, data)
```

A reader stops at the first record whose length runs past the written extent or whose CRC
fails (the same rule as the journal). Records never span segments; a record that does not
fit starts a new segment.

Journal additions (`schema/ava1.toml`, journal structs, never on the wire):

```
JnlBatch   ext tag 1 pack_segment u32, tag 2 pack_offset u64, tag 3 pack_len u64
           (present when the batch wrote small files to the log: the byte range of this
            batch's PackFile records in that segment)
JnlSweep   kind 6: { files: records FileRun }   files now durable in place
JnlSnapshot ext tag 1 unswept: records FileRun  done files not yet swept (needed to keep
           pack segments on compaction); ext tag 2 segments: records PackRef
PackRef    struct { segment u32, offset u64, len u64, first_file u32, count u32 }
```

Replay: `done` grows as today; `unswept = done − swept` where `swept` is the union of
`JnlSweep.files`; `segments` is the list of batch pack ranges whose files are not all swept.

## 3. Receiver pipeline (console `ava1_apply.c`, engine `recv.rs`)

### 3.1 apply_record (worker)

1. Validate as today (length, root). Duplicate check as today.
2. Append the record to the current pack segment (one `pwrite` at the segment's tail; the
   tail offset is advanced under `j->mu`; the write itself runs outside it). Keep
   `(file_id, segment, offset, len)` in the batch's pending list instead of an open fd.
3. `open(O_CREAT|O_TRUNC|O_NOFOLLOW, mode)` → `write` → `futimens` → `close`. No fsync. (Use
   `open`'s mode argument with the process umask cleared at data-layer start, and `futimens`
   on the fd: four syscalls instead of six.)
4. The pending list has no descriptor, so `AVA1_PEND_MAX` and the open-file budget's pend
   share no longer bound small files; the batch triggers are time (250 ms), count
   (`batch_max`, keep it) and the **unswept cap** (§3.4).

### 3.2 sync_batch

1. `fsync(pack fd)` once (retry policy of §12.6 unchanged; on a retried success the batch's
   pack range is re-read and every record's root re-checked, replacing today's
   `reread_small`).
2. Large-file data fsyncs as today (striped).
3. **No directory fsyncs here.** They move to the sweep.
4. Journal `JnlBatch` with the pack ext, fsync. Mark the batch's files `done`; send
   `Durable`.

A segment is closed when `tail + record > PACK_SEGMENT`; the next record opens `pack.<n+1>`.
Closing needs no extra fsync (the batch fsync covers it).

### 3.3 Sweep (a worker-idle task on the console, a blocking task on the engine)

Runs whenever a worker has no apply work, and continuously after the last batch:

1. Pick up to 64 unswept files whose batch is at least `SWEEP_AGE` = 3 s old (the kernel's
   syncer has usually written them by then; fsync is then cheap).
2. For each: `open(O_RDONLY|O_NOFOLLOW)`, `fsync`, `close`. A missing file or a short file
   (`fstat` size ≠ manifest size) is re-materialised from the pack first (§4), then fsynced.
3. `fsync` each unique parent directory of the picked files (`ava1_sync_dirset`).
4. Journal `JnlSweep{files}`, fsync.
5. If a segment's files are all swept (and its last `JnlSweep` is durable), `unlink` it.

The sweep is the only place directories are fsynced for small files. Staged jobs: the
final rename's parent sync (`finish`) stays.

### 3.4 Backpressure

`unswept_bytes` (pack bytes whose files are not swept) is capped at `UNSWEPT_MAX` = 256 MiB.
A worker that would push a batch past the cap runs sweep work instead (the same shape as
today's `pend_gate_idle`). This bounds disk use to one cap of duplicated bytes and degrades
to today's throughput, never below it.

### 3.5 Job end

`finish` runs when every file is done (as today). `JobDone` is sent after the final
`JnlBatch`/`JnlDone` (I2). The job stays listed and its thread alive until `unswept` is
empty; `job.status` reports `Status.ext unswept` (§6) meanwhile. A session that ends during
the sweep does not stop it. `JnlDone` is appended as today; a resumed job whose journal has
Done but unswept files only runs the sweep (after recovery, §4) and answers the map.

## 4. Recovery (on `JobOpen` for a known job, where the journal is replayed today)

For every file in `unswept` after replay: locate its record by scanning the pack ranges in
`segments` (records are self-describing; a batch range is at most `batch_max` records), check
the record's root over its data, and if the file is missing or differs (size or a cheap
BLAKE3 of the file when the size matches), rewrite it from the record. Then run the sweep for
them. Files whose record cannot be found or fails its CRC are **not** marked done: they are
removed from `done` (a `JnlReset` each) and resent. A pack segment that is missing fails
every file in it the same way. Then the map is answered as today.

Cost: unswept is bounded by `UNSWEPT_MAX`, so recovery reads at most 256 MiB.

## 5. Semantics and UI

`Durable` keeps its meaning for the sender: never resent. `JobDone` means every byte is on
stable storage and the receiver can finish without the sender. The new, weaker point is that
a power cut within the sweep's lag leaves the last files to be re-created the next time the
helper starts (its data layer runs recovery for every job directory with `unswept` on start,
not only on `JobOpen`; one pass over `/data/ps5upload/ava/jobs`). The engine shows "finishing
on the console" until `Status.unswept` reaches 0, polled through `job.status`, and the Upload
screen's success state waits for it when the job finished less than 30 s ago. FTX2 offered
nothing comparable; AVA1 before this design offered the stronger guarantee at the cost
documented in CUTOVER §4.

## 6. Protocol additions (optional, backward compatible)

- `Status` ext tag 5 `unswept` u32: done files not yet durable in place. Absent = 0.
- `JobDone` ext tag 2 `settling` u8: 1 when files are still being swept. An old engine ignores
  both (IGNORABLE extension rules of §3).

## 7. Tests (ctest, both receivers)

1. 2,000 small files: batches do exactly two fsyncs each (count `fsync` through the test
   shim's hook), `Durable` arrives, files land, sweep deletes every segment, journal replays to
   the same state.
2. Crash points (`AVA1_CRASH_*`): after pack fsync and before any file write; after half the
   files of a batch were written; after `JnlSweep` was appended and before `unlink` of the
   segment; mid-record pack write (torn tail). Each resumes to a complete, verified tree with
   nothing resent except files whose record is unreadable.
3. `UNSWEPT_MAX` reached: throughput does not drop below the per-file-fsync baseline (the
   old path, kept behind a cfg flag for one release as the fallback).
4. Loopback floor: raise the engine receiver's files/s floor once measured.
5. Hardware: CUTOVER §4 tiny-file rows on `/data`, usb0, ext; the Phat's USB hard drive.

## 8. Rollout

Behind `ava1_data_cfg.log_small` (default on once tests 1–3 pass), with the old path kept for
one release. Console first (the gap is there), engine second.
