# Design 5 — download commit off the sink lock, bounded parallel

## Goal
Downloads (console → engine, `LocalSink`) stop paying a serial fsync+rename per large file under
the sink's state mutex, mirroring the console's commit-on-worker (003 §3.3).

## Today (`engine/crates/ava1/src/recv.rs`)
- `LocalSink::commit` (≈582-600) holds `self.st.lock()` while it `set_len`s, `sys_fsync`s and
  `rename`s. Every other sink call (writes on other files, `sync`) waits behind the fsync.
- Batched flush is already good (`sync` ≈432-456: cheap fsync per file, one drive-cache flush,
  directories once). Small files already go through `PackLog`. Only large-file commit is left.

## Design
1. In `commit`, take the lock only to `remove` the `File` from `st.open` and compute `(part,
   fin)`; drop the guard; then `set_len`, `sys_fsync`, `rename` with no lock held. Nothing else
   reads `st.open[id]` after removal, so this is safe. (Mirrors apply.c's "commit on a worker".)
2. The receiver already calls `commit` inside `spawn_blocking` per file, sequentially, after each
   root verifies (recv.rs ≈1680 area). Allow up to `COMMIT_PAR` (4, same as `WRITE_PAR`) commits in
   flight: collect the verified ids of a batch, run their `commit`s on a `JoinSet` bounded by a
   semaphore, then write the ONE batch journal record for all that succeeded (the record is
   already batched: "ONE journal record, one drive flush and one Durable for every file this
   batch committed (T28)"). A failed commit becomes a `Reset` + `FileRetry` as today.
3. Keep the ordering invariant: the journal record naming a file as done is written only after
   that file's `commit` returned (its rename is in place and fsynced). Parallelism is between
   files, never between a file's commit and its own record.

## Non-goals
Preallocation on the engine (it never did; host filesystems do not have the UFS sparse problem).

## Tests
- Existing LocalSink tests pass unchanged.
- A sink whose `commit` sleeps 200 ms: 8 large files commit in ≈2 rounds, not 8 (timing test with
  a generous bound), and other sink writes proceed during a commit (no lock held).
- Crash between a commit and its journal record (fault hook): resume re-verifies that file
  (allowed by §13.4) and completes.

## Acceptance
An ordered download of 200 × 64 MiB files to a slow disk shows commit time overlapping, and the
`JobSummary` receiver-bound share drops; no change to on-disk layout or protocol.
