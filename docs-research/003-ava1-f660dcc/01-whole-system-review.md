# AVA1 whole-system review: beating FTX2 and reaching the state of the art

`ava1` at f660dcc, 2026-10-04. Third session. The two earlier sessions were correctness
reviews (`001-ava1-e64002f/`, `002-ava1-f660dcc/`); this one asks a different question: what
stands between AVA1 and (a) beating FTX2 on every row of CUTOVER §4, and (b) being the best
transfer protocol this console can run. It re-reads the hot paths on both ends with that
lens: `ava1_apply.c`, `ava1_data.c`, `ava1_recv.c`, `ava1_send.c`, `ava1_conn.c`,
`ava1_server.c`, `ava1_aead.c`, `ava1_b3.c`, `ava1_calibrate.c`, `platform_ps5.c`, the
Makefile, `governor.rs`, the `send.rs` lane and reader paths, `recv.rs`'s `LocalSink`,
`download.rs`, SPEC §6–§17 and CUTOVER §3–§4. FTX2's own notes in `runtime.c` were read
where they record a lesson (preallocation).

Everything marked **[measured]** is from CUTOVER §4; **[hyp]** is a mechanism I can name
but have not timed on a console, each with the one measurement that confirms or kills it.

---

## 1. Where AVA1 stands

**Already better than FTX2 [measured]:** resume of a 4 GiB upload (90–106 vs 66–67 MB/s);
the 223,000-file game (AVA1 finishes and verifies; FTX2 dies at 611 s and cannot clean up);
console-local copy (+78% on `/data`); tiny-file uploads to usb0 (+36%); downloads after
`b1be8059`/`ea3f68e` (75–132% of FTX2, run to run); everything FTX2 never had: verified
bytes, durable acks, a journal, authenticated and encrypted sessions, a governor.

**Still behind [measured]:** 4 GiB upload 5–8% below FTX2 on every drive of the Pro and on
the Phat's `/data`; the Phat's usb0 at a third of FTX2 (9–15 vs 34–35 MB/s); tiny-file
upload −13% on `/data`, −30% on ext1; the 223k-file corpus at 82.5 files/s, 3.5× below the
drive's own ceiling; zip downloads restart from zero on a reconnect (FTX2 resumes
mid-entry).

**Still open from the earlier sessions:** S1 (pairing code grindable: commit-then-reveal),
S2 (trust store under `/data`, now also reachable through `fs.write` over AVA1), and the
five M-findings of `002-ava1-f660dcc/01-review.md`.

The verdict in one line: the architecture is right and already ahead on everything that
is not raw speed; the three speed gaps have identifiable mechanisms, two of which are
fixable without touching the wire, and the third (tiny files) needs one real design
change on the receiver that would put AVA1 far ahead of FTX2 instead of 13% behind.

## 2. Large files: where the 5–8% goes

### 2.1 What is not the cause

- **Crypto.** The console seals and opens with an AVX2 ChaCha20 (`ava1_chacha_avx2.c`,
  selected by CPUID) and a 64-bit Poly1305 (`ava1_aead.c`), interleaved per 8 KiB so the
  frame stays in cache. At 118 MB/s that is a few percent of one core, and it runs on the
  lane's own thread, so it scales with lanes. BLAKE3's bulk compression is the project's
  assembly (`blake3_avx2_x86-64_unix.S` in `BLAKE3_SRCS`, Makefile:16-22); only the C glue
  is at -O0 (see §8). `crypto.bench` (method 3) already measures the real per-byte cost
  on the console; its number should be in CUTOVER §4 so this stays settled.
- **Preallocation.** My round-2 review suspected `posix_fallocate` under `j->mu`
  (`lfile_open`, `ava1_apply.c:625-631`). FTX2 preallocates too, and its comment
  (`runtime.c:3593-3610`) records why: a sparse file on PS5 UFS collapses from 60 to 2–3
  MiB/s minutes into a multi-GB upload under dirty-buffer throttling. Both protocols pay
  the same cost, so this is not the differential, and **AVA1 must keep preallocating**
  (and keep the ENOSPC-first behaviour). One improvement stands: do it *before* taking
  `j->mu` for the first write, so the other workers and the feeder are not held for its
  duration (a 4 GiB preallocation on a slow USB drive is minutes; today everything that
  touches `j->mu` waits for it).
- **The periodic fsync itself.** The batch every 250 ms or 64 MiB (`ava1_apply.c:19-21`,
  `job_main` 1709-1711) flushes ~25 MB on a 1 GbE link. On the NVMe that is a few
  milliseconds, absorbed by the 64 MiB credit window. It is also what keeps AVA1 immune to
  the throttling collapse FTX2 hit: dirty buffers never pile up. Keep it.

### 2.2 The mechanism that fits the numbers [hyp]

On a lane, one thread does everything in sequence (`run_lane` → `serve_loop` →
`ava1_conn_recv_body`, which calls `ava1_open` before returning): `malloc(body_len)` for the
frame, `recv` until the body is complete, decrypt in place, hand off. While it decrypts a
15 MiB frame (~10–20 ms at the console's AEAD rate) and faults in a fresh 15 MiB
allocation (~3–4 ms of page zeroing; jemalloc hands huge allocations back to the kernel on
free), it is not reading its socket. The console sets no `SO_RCVBUF` on lanes
(`ava1_server.c:1080-1083` sets only `TCP_NODELAY`/`SO_NOSIGPIPE`), so the kernel's receive
buffer is whatever the PS5 kernel autotunes, and the sender's TCP window on that lane
closes for the duration. With the governor's starting two lanes (`START_LANES`,
`governor.rs`) and a chunk it grows to 15 MiB after ten stable seconds, the two lanes
spend a meaningful fraction of each second not reading, and the wire idles whenever both
are decrypting at once. FTX2 ran four plaintext streams with nothing between `recv` and
`pwrite`.

Why the governor does not fix it by itself: a third lane adds a few percent, below the
`GAIN` 1.10 bar, so it is reverted and held for 30 s (`HOLD_TICKS`), and the chunk keeps
growing to 15 MiB, which lengthens every stall.

**The measurements (an afternoon, no code change for the first two):**
1. `crypto.bench` with `mib = 64` on both consoles: open_micros per MiB is the stall per
   MiB of frame.
2. The 4 GiB upload with lanes pinned at 2, 4, 8 and chunk pinned at 1, 4, 15 MiB (a test
   knob on `SendOptions`/the governor). If 4 lanes × 4 MiB beats 2 × 15 MiB by the missing
   5–8%, this is it.
3. The sender already tracks credit starvation and a `Stall` record per tick; log the
   fraction of ticks with `credit_starved` and the receiver's reported bottleneck per job
   (one line at the end), so every future table row says where the time went.

**The fixes, in order of cost:**
- `SO_RCVBUF` 4 MiB on lane sockets at accept (console) and `SO_SNDBUF` 4 MiB on lanes
  (engine, `session.rs` `join`): one line each; lets the kernel buffer through a decrypt.
  Also the right fix for Wi-Fi, where the per-lane window is the ceiling today
  (`governor.rs` test `a_single_window_limited_lane_still_gets_company` documents 52 MB/s
  per lane at 512 KiB / 10 ms).
- A frame-buffer pool on the console (reuse the last freed body buffer of the same or
  larger size; the window bounds the count at 64 MiB / chunk): removes the page-fault cost.
- Decrypt off the reader thread: the reader assigns the per-lane counter at receipt and
  hands the sealed frame to a data worker that opens it (nonce is known, so any order is
  fine; a bad tag ends the lane asynchronously). Then a lane thread does nothing but
  `recv`, as FTX2's did. This is the complete fix; the first two may be enough.
- Governor: cap the chunk at 4 MiB while the receiver's `open_micros` says decrypt time per
  frame exceeds a few ms, or simply prefer more lanes over bigger chunks when the link is
  the bottleneck; lower `GAIN` to 1.05 for the lane probe on links that are not saturated.

### 2.3 The Phat's usb0 (a third of FTX2) [hyp]

A ~35 MB/s device. Two effects stack. (a) `posix_fallocate` on a filesystem whose
`VOP_ALLOCATE` is the kernel's generic fallback writes 4 GiB of zeros before the first data
byte: on this device two minutes, during which the sender is parked on credit and the
device is busy doing nothing useful; if the drive is exFAT it may instead refuse and fall
to `ftruncate` (then this effect is absent — the log line `preallocate took N ms` tells).
(b) On a device slower than the link, each 64 MiB batch fsync takes ~1.8 s while the
window holds 0.64 s of data, so the sender stalls, and between fsyncs the device idles
while the window refills: utilization ~70%. Both are visible in the per-job stats line
(`data` ms per batch) and in a preallocation timing. Fixes: time and log the
preallocation; on a device whose fsync of a 64 MiB batch exceeds the window's worth of
data, make the window track the batch (grant more credit on slow drives, or fsync in
smaller units right after each chunk from the writing worker so the device streams and the
pauses are short). FTX2's 34–35 is the device speed; AVA1 should match it.

## 3. Tiny files: the fsync ceiling and the design that removes it

### 3.1 The accounting today

Per small file: `open(O_CREAT|O_TRUNC)`, `write`, `fchmod`, `utimensat` by path
(`apply_record`, `ava1_apply.c:793-806`), then in the batch an `fsync` on a worker stripe
(`sync_stripe`), then `close`. Per batch: N file fsyncs striped over the workers, then every
directory that gained a file fsynced **serially on the job thread** (`sync_new_dirs` →
`ava1_sync_dirset`, 1009-1047), then one journal append + fsync, then `Durable`.
`disk.calibrate` **[measured]** says what the drive allows for create+fsync: `/data`
182–188 files/s at 1 worker, 287–298 at 16; usb0 flat at ~575; ext1 282→407. UFS with soft
updates serialises most of an fsync, so 16 workers buy 1.6×. AVA1's 291 files/s on
`/data` *is* that ceiling. FTX2's 320–343 is above it, so FTX2 does not fsync per file.
There is no tuning that beats this: as long as each file needs its own fsync before its
`Durable`, 2,000 files cost ~7 s on `/data` and 10+ minutes on a USB hard drive (the Phat's
one run: 27 files/s).

### 3.2 Durable-by-log: write-ahead the small files

Make the *log* the thing that is durable per batch, and let the files become durable on
their own.

- The receiver appends each small file's record bytes (file_id, length, root, data, as
  they arrived in the `Bundle`) to a per-job **pack** segment in the job directory
  (`<job dir>/pack.<n>`, preallocated 64 MiB, sequential), *and* writes the file itself as
  today (open/write/close, no fsync).
- A batch is: `fsync(pack)` (one call for all its small files), journal append + fsync (as
  today, with a new `JnlPack{segment, first_offset, files...}` body or an extension on
  `JnlBatch`), then `Durable`. Two fsyncs per batch instead of N + D + 1.
- Recovery (on `JobOpen` resume, where the journal is replayed today): for every file the
  journal marks durable-in-pack but not yet swept, re-materialise it from the pack
  (open/write/close; the root in the record verifies the bytes), then continue. The
  pack is the write-ahead log; the files are the checkpoint.
- A **sweep** runs on idle workers during the job and after `JobDone`: `fsync` the files
  written more than a few seconds ago (the kernel's syncer has usually flushed them by
  then, so the call is cheap), and once every file of a pack segment is swept, delete the
  segment. Directory fsyncs move here too (one per unique directory, after its files),
  off the critical path. Unswept bytes are capped (say 256 MiB); past the cap a batch waits
  for the sweep, which is today's behaviour, never worse.
- The sender sees no change: `Durable` still means "never resent". `JobDone` is sent when
  the last batch's pack and journal are durable; the sweep finishes behind it.

Expected result: tiny-file upload bounded by create + write (thousands per second on
loopback **[measured]**: the engine receiver's floor is 3,500 files/s; `disk.calibrate`
already returns `create_us` separately from `fsync_us` per point — print both in the next
hardware pass to size this on the real drives). Likely 3–10× on `/data` and far more on
USB hard drives, where FTX2 completes nothing today.

Cost and risk: small-file bytes are written twice (they are a small share of any game);
pack space is bounded by the sweep cap; the C is a few hundred lines in `ava1_apply.c` and
`ava1_recv.c` plus a journal record; the fault-injection tests (`ava1_apply_crash`,
`AVA1_CRASH_*`) extend naturally with "crash after pack fsync, before any file write" and
"crash with an unswept segment".

**The one decision this needs from the project.** Today, after `JobDone` plus a power cut,
every file is on disk without the helper ever running again. With durable-by-log, a power
cut within the sweep's lag (seconds; at most the syncer's 30 s) can leave the last files
missing until the helper next starts and recovers them from the pack. FTX2 never offered
the stronger guarantee; the engine's resume logic is unaffected either way. Options:
(a) accept it and show "finishing on the console" in the UI until the sweep reports done
(a `Status` field); (b) keep the stronger guarantee by having `JobDone` wait for the sweep,
which keeps most of the gain on long jobs (the sweep overlaps the transfer) and gives
back part of it on a 6-second benchmark. I recommend (a) with the status, and the
measurement in both modes.

### 3.3 The intermediate step, if the design waits

Two of the per-batch costs can go today without the pack: (1) stripe the directory fsyncs
over the workers like the data fsyncs (they are independent descriptors), or drop them
per batch and fsync each unique directory once at the end, plus an `lstat` of every done
small file of the resumed job in `prepare`/`reconcile` (a name lost to a crash before the
syncer wrote it is then simply resent); (2) run `commit_large` on a worker instead of the
job thread (`ava1_apply_commit_ready`, 1524-1542, runs each commit's fsync + rename +
directory fsync + journal fsync inline, so a batch's commits stop every batch). Neither
moves the per-file fsync ceiling; both are what the 223k-file corpus needs (§4).

### 3.4 The same on the engine

Downloads into the engine fsync every small file per batch too (`LocalSink::sync`,
`recv.rs:219-250`; on macOS one `F_FULLFSYNC` per batch is already the drive-cache
amortisation). The identical pack design applies and is easier in Rust; it is what makes
AVA1 downloads beat FTX2 by the same margin as uploads rather than trading places run to
run.

## 4. The 223,000-file game

82.5 files/s **[measured]**, 3.5× under the drive, not reproducible on loopback. The
corpus has 20,075 directories and 1,641 files over 256 KiB; the code paths that scale with
exactly those two numbers:

1. `prepare` (`ava1_recv.c:837-921`) creates every manifest directory before the map is
   sent and fsyncs each unique parent **serially** (`ava1_sync_dirset`, 879). Several
   thousand unique parents at ~3 ms each is tens of seconds before the first byte, during
   which the sender has nothing to do.
2. Every batch's `sync_new_dirs` fsyncs the parents of that batch's files serially; a
   batch of 256 path-sorted small files in a deep tree touches tens of directories, so a
   good share of every 250 ms goes to directory fsyncs on the job thread, while `pend`
   fills (512 cap) and the workers block in `pend_add`.
3. 1,641 large files commit inline on the job thread (§3.3 item 2), four fsyncs each.
4. Each large file's `lfile_open` preallocates under `j->mu`.

The per-job stats line the helper now prints every 10 s (`log_stats`, `ava1_apply.c:1665`:
scan/data/dirs/journal/commit ms per batch) will show which dominates; my expectation is
`dirs`, then `commit`. §3.2 removes 1 and 2 outright (names are recovered from the log, so
directories are fsynced lazily, once each); §3.3 is the cheaper version of the same.

## 5. Downloads and the zip regression

- The console reads with one thread (`reader_main` → `ava1_read_files`, 4 MiB chunks,
  group CVs computed inline). Fine for 1 GbE; the first thing to parallelise for a faster
  link or Wi-Fi 6, after §2.
- The sender clones every frame body before sealing (`lane_task`, `send.rs`:
  `(*f.body).clone()`): a 15 MiB memcpy per frame on the engine. Harmless on a desktop,
  worth an `Arc<[u8]>`-to-`Bytes` style zero-copy when the Android client lands.
- **Zip downloads restart from zero on a reconnect** (CUTOVER §2, SPEC §10). The writer uses
  Deflate (`download.rs:344-348`), which is why: a Deflate stream cannot be re-entered. Game
  files are already compressed; writing entries *stored* makes the archive a plain
  concatenation, so the ordered receiver can truncate to its last durable byte and
  continue on reconnect, and it also removes the engine's only CPU-bound step in a
  download. FTX2's mid-entry resume is then matched with less code than an in-run resume
  of Deflate state.

## 6. Features the best transfer tools have that AVA1 can add on its own foundations

AVA1 already has what most of them lack: end-to-end BLAKE3 roots with group-level resume,
durable acks with a journal, authenticated and encrypted sessions, credit flow control
(the same shape as SMB3 multichannel), parallel lanes (bbcp/GridFTP), a self-tuning
governor, and streaming archive sources. What the field has that AVA1 does not, ranked by
value to this product:

1. **Group-level delta for changed files** (what rsync, casync and Syncthing's block
   exchange offer). A game update changes a fraction of a 40 GB tree. The receiver already
   computes CVs per 1 MiB group and keeps outboards; the `verify` policy already hashes an
   existing same-size file. The missing piece: when a root mismatches, keep the outboard
   the receiver just computed, let the sender send that file's CV list in a new control
   message (`FileCvs{file_id, first_group, cvs}`, 32 B per group; 128 KiB for a 4 GiB
   file), have the receiver answer the matching groups as "durable" in the map after
   copying the existing file into the part file on the console (a local copy at device
   speed, seconds), and the sender sends only the differing groups. A version-1.1 cap bit;
   no change to lanes or credit. This is the single feature that would make AVA1 visibly
   superior to every FTP/FTX2-class tool for updates.
2. **Durable-by-log** (§3.2) — the databases' answer to per-record fsync.
3. **Wi-Fi awareness**: `SO_RCVBUF`/`SO_SNDBUF` on lanes (§2.2) and a governor that keeps
   adding lanes while the per-lane rate is window-bound. AVA1's 8 lanes × a 4 MiB buffer is
   enough for 1.2 Gbps at 20 ms RTT.
4. **Stored zip downloads** (§5) for mid-entry resume.
5. **Adaptive `LARGE_CUTOFF`** (the design spec's 64 KiB–4 MiB range, deferred in SPEC
   §10): on a drive where create+fsync dominates, a bigger cutoff bundles more; with §3.2
   this matters less, so it stays deferred.
6. What *not* to add: on-wire compression (game data is compressed; CPU on the console is
   the scarcer resource), UDP transport (the console has no QUIC stack and 1 GbE LAN is
   where TCP lanes already win), small-file deduplication across files (ruled out of
   project 2 for good reason: little gain on game trees).

## 7. Security and reliability for "best"

- **S1 stays the release blocker**: commit-then-reveal for the pairing code
  (`001-ava1-e64002f/02-design-review-round2.md` §2 S1). Nothing in f660dcc touches it.
- **S2**: with management over AVA1, a *paired* peer's `fs.write` can overwrite
  `/data/ps5upload/ava/peers`; carve the trust store out of every write path (FTX2 :9114,
  FTP, and AVA1's own `may_write`).
- **The Pro outage of 2026-10-03** (CUTOVER §3) has no AVA1-specific hypothesis yet; two
  worth checking against the timeline: whether a large preallocation (gigabytes of zero
  pages through the buffer cache) or the 223k-file `prepare` (20,000 mkdirs with serial
  fsyncs) was running at 09:10. Both are heavy, unusual kernel work for this console; the
  timing lines the local session added will say.
- **Liveness margins**: `dead_after` 6 s with a 2 s ping is tight for a console in a
  10-second GC pause or a Wi-Fi roam (round-2 R1); 10–15 s costs nothing on the happy path.

## 8. Build and observability

- `BLAKE3_SRCS`' C files (`blake3.c`, `blake3_dispatch.c`, `blake3_portable.c`) compile at
  the payload's default -O0 (Makefile:94, 191-192) while the AVA1 objects get -O2
  (56-61). The assembly kernels are unaffected, but `blake3_hasher_update`'s chunk-state
  logic runs for every small file and every `verify` hash. Moving the three files into the
  -O2 group is free.
- `role_free` in `ava1_send.c` prints a stats line on every download unconditionally; gate
  it like the Rust `PS5UPLOAD_AVA1_TIMING`.
- CUTOVER §4 should carry, per row, the three numbers that explain a result: the sender's
  bottleneck share (§2.2 item 3), `crypto.bench` on that console, and `disk.calibrate`'s
  `create_us`/`fsync_us` for that drive. Then a regression is a diagnosis, not a mystery.

## 9. Plan

Measure first (one hardware session, no risk), then change what the numbers say:

| step | what | expected |
|---|---|---|
| 1 | `crypto.bench`, pinned lanes × chunk matrix, preallocation timing, bottleneck share per job (§2.2) | names the large-file mechanism |
| 2 | `SO_RCVBUF`/`SO_SNDBUF` on lanes; frame-buffer pool; preallocate before taking `j->mu` | closes most of the 5–8%; fixes the Phat usb0 if (a) of §2.3 is real |
| 3 | §3.3: striped or deferred directory fsyncs with the resume `lstat`; commits on workers | 223k-file corpus toward the drive's ceiling; `/data` tiny files unchanged |
| 4 | §3.2 durable-by-log on the console, then the engine; decision on `JobDone` semantics | tiny files 3–10× on `/data`; USB hard drives usable |
| 5 | Stored zip downloads (§5); decrypt off the reader thread if step 2 left a gap | zip resume regression gone |
| 6 | S1, S2 (release gate), then the group-level delta design (§6.1) as v1.1 | security done; the differentiating feature |

Steps 1–3 fit before the hardware pass that gates `auto` mode; step 4 is the one worth a
design note of its own on this branch once the project answers the `JobDone` question.
