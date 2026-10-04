# AVA1 deep re-review at fc2640f (2026-10-04)

No new commits since session 005; `origin/ava1` is still fc2640f. This is a second,
code-level pass over the two items session 005 left open, to prove them out before the local
session acts on them. Read in full this pass: `zip_stored.rs` `append`/`position`/`commit`/
`finish`, `recv.rs` around the `commit` call site, `seq.rs` `one_pass`, and `governor.rs`
`observe_rate`.

## 1. The zip restart-window fix is a strict improvement, not a risk

Session 005 proposed closing the "file is whole but not finished" window
(`StoredZipSink::position` refusing a partial range that already covers the whole file). The
code path is now traced end to end, and the fix is safe to land as written:

- `position` (zip_stored.rs:592) rejects `x >= size`. Change it to accept `x == size`: set
  `in_flight = Some(pk)`, `partial_bytes = size`, `cut = data_off + size` (the descriptor was
  never written, so the cut sits exactly at the end of the data), and rebuild the running
  CRC over all `size` bytes. Keep `x > size` an error. After this `st.pos == data_off + size`,
  which is exactly where a descriptor belongs.
- `commit` (zip_stored.rs:552, today a no-op) gains: if `current` is that slot and
  `written == size`, write `slot.descriptor(crc)` at `st.pos`, advance `st.pos` by `DESC_LEN`,
  set `crcs[k]`, clear `current`. This mirrors the completion block already in `append`
  (lines 470-475).
- **Why no data re-arrives to fight it.** On resume the sender runs `one_pass` (seq.rs:262).
  A large file whose durable ranges already cover every group and whose outboard CVs are all
  known sends only `Read::Root` (seq.rs:283), never data; in an ordered download every file
  takes this large path (the batched-commit comment in recv.rs says so). The receiver then
  verifies the root and calls `Sink::commit(id)` (recv.rs:1680) — exactly the hook the fix
  uses. So the descriptor is written from the commit, with no new frame type.
- **Why it cannot regress.** If instead the outboard was not fully persisted at the crash,
  the sender re-sends the file's data from offset 0. That enters `append` with
  `off == 0 && st.written > 0`, which already returns `zip_restart` (zip_stored.rs:453-455) —
  the same job-restart that happens today. The fix removes the restart only in the case where
  the root alone comes back; every other case is unchanged.

Net: a one-batch-wide restart at each file boundary becomes a no-op in the common case and is
identical to today's behaviour otherwise. The CUTOVER §2 "restart window" row can be struck
once the test (journal a partial covering a whole last file, resume, assert only a `FileRoot`
arrives and the archive is not truncated below the file's data) is green.

## 2. Governor `best_rate` never decays (reconfirmed, still minor)

`observe_rate` (governor.rs:254) sets `best_rate = best_rate.max(rate)`. On a link whose
capacity drops mid-job the easier 1.05 lane-probe bar (`rate < best_rate * 0.90`) then holds
for the rest of the job, so the governor keeps probing lanes up against a ceiling it can no
longer reach. The only cost is carrying a few more lanes than ideal; throughput is not hurt
and pins still bound it. If it is ever worth touching, a slow multiplicative decay
(`best_rate *= 0.99` per sample, floored at the current rate) makes the raise-bar and
lower-bar symmetric. Not worth a change before the hardware matrix runs.

## 3. Nothing else changed

Everything else matches session 005. The scorecard, the closed security items (S1, S2), the
landed 003 lane-path and governor work, and the open design work (durable-by-log 003/02,
group delta 003/05) are unchanged. Next real review waits on the next push to `origin/ava1`.
