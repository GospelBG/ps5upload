# Implementation guide — add an end-to-end progress watchdog to the receiver

**For:** the implementation agent (ava1 branch). **Reviewed design:** 006/02 (liveness audit).
**Severity:** liveness (closes the one way a job can sit open forever). **Size:** small, one
file plus one test.

## Problem
The only time-based abort in the receiver run loop is the link's byte-level dead-peer
watchdog (`link.rs`, 12 s without a received byte). A Ping is a byte (link.rs:425, "Any byte
counts"), so a peer that is alive enough to heartbeat but whose data pump is wedged — e.g. a
sender blocked on a stuck network-filesystem source read — keeps the job open indefinitely
with zero file bytes moving. Nothing aborts it.

## Design
Add a progress deadline to the receiver run loop in `recv.rs`. Track the time of the last
**file-data** arrival (distinct from any byte). On the existing 50 ms `tick`, if the job has
made no data progress for longer than the deadline **and** it is genuinely waiting on the
sender (window open, files outstanding, no disk batch in flight), send `JobCancel` with a
stall reason and return. Keep the deadline generous so a slow-but-moving drive is never cut.

## Where (all in `engine/crates/ava1/src/recv.rs`, `run_job`/run loop ~ lines 884-1030)
1. **Constant** near `WRITE_PAR` (recv.rs:1277):
   ```
   /// A job that makes no data progress for this long while the sender owes bytes is
   /// stalled: abort it rather than wait on the byte-level watchdog forever (006/02).
   const PROGRESS_DEADLINE: Duration = Duration::from_secs(60); // ~5x dead_after
   ```
   60 s is deliberately well above `dead_after` (12 s) so a legitimately slow link that still
   moves some data each `dead_after` is never cut. Tune later against the hardware matrix.

2. **State**, next to `let mut last_batch = Instant::now();` (recv.rs:892):
   ```
   let mut last_data = Instant::now();
   ```

3. **Mark progress** on every admitted data frame. The cleanest single point is right after
   `outstanding -= len;` (recv.rs:998), which runs only for an admitted lane data frame:
   ```
   outstanding -= len;
   last_data = Instant::now();
   ```
   (`outstanding -= len` runs for both Chunk and Bundle frames; both are real data. A frame
   for an already-`done` file still counts as the peer making progress, which is fine.)

4. **Check on the idle tick.** In the `match ev { ... }` the `None` arm is the tick (and the
   batch/write-join fall-through, which bind `None`). Add the check where `idle` is handled —
   after the batch-management block, guarded so it only fires when the sender actually owes
   data:
   ```
   if idle
       && last_data.elapsed() > PROGRESS_DEADLINE
       && done.len() < /* number of file entries */ n_files
       && batch_handle.is_none()
       && writes.is_empty()
   {
       let _ = link.control.send(&JobCancel {
           job_id,
           reason: gen::ERR_STALLED, // add this code (see below)
       }).await;
       return Err(SendError::Stalled); // or reuse an existing variant (see below)
   }
   ```
   The guard matters:
   - `done.len() < n_files` — files remain, so the sender still owes data. (Use whatever the
     loop already has for "all files finished"; if there is a `remaining` count or a
     `done.len() == m.file_count()` finish condition below, mirror it exactly.)
   - `batch_handle.is_none() && writes.is_empty()` — nothing is progressing on disk; we are
     purely waiting on the network. Without this, a long fsync batch on a huge file could be
     misread as a stall.

5. **Reset on any real progress** so a batch completing or a write finishing does not leave a
   stale `last_data`. Simplest: also set `last_data = Instant::now();` when the batch-join arm
   folds a completed batch and when a `writes.join_next()` completes successfully. That way
   the deadline measures "no data AND no disk progress", which is the true stall.

## New wire/error plumbing
- Add `ERR_STALLED` to the error-code set alongside `ERR_CREDIT`/`ERR_CANCELLED` (ava1-gen or
  wherever `gen::ERR_*` live). If adding a code is heavier than it's worth for a first cut,
  reuse `ERR_CANCELLED` with a distinct log line; prefer a dedicated code so the sender and
  operators can tell a stall from a user cancel.
- Add `SendError::Stalled` (or reuse `SendError::Disconnected("progress stalled")`). A
  dedicated variant reads better in the `JobSummary`/logs.
- The sender side: on receiving `JobCancel`/this error it already tears the job down like any
  cancel. Confirm the sender's decode thread exits via `gone(c)` (seq.rs) when the job ends —
  it does, so no sender change is required beyond recognising the code for logging.

## Tests
1. **Unit (preferred):** a mock sender that completes the handshake, opens a job with credit,
   then only sends control-lane pings (or nothing on the data lanes) while keeping the link
   alive. Assert the receiver returns a stall error / sends `JobCancel(ERR_STALLED)` within
   ~`PROGRESS_DEADLINE` (use a short deadline via a test seam, see below) rather than hanging.
2. **No false positive:** a sender that sends one small chunk every `PROGRESS_DEADLINE/2`
   keeps the job alive; assert it runs to completion.
3. **Slow-disk not cut:** with a sink whose `sync`/writes are artificially slow (batch in
   flight), assert no stall abort fires while disk work is progressing.

**Test seam:** make `PROGRESS_DEADLINE` overridable in tests — either a field on the job
options (`o.progress_deadline`, defaulting to the constant) or a `#[cfg(test)]` smaller value.
Prefer the options field so the benchmark/field path can tune it too.

## Acceptance
- The three tests green; full `cargo test -p ava1` green.
- `JobSummary` (governor) still prints; a stalled job logs the stall reason, not a generic
  disconnect.
- Note the change in `protocol/ava1/SPEC.md` (the job lifecycle / §12 area) and tick the
  liveness item.

## Validate locally
```
cargo test -p ava1 recv
cargo test -p ava1
```

## Notes
- Keep the deadline on the **receiver**. A sender-side source-read deadline (abort a read that
  exceeds its budget) is a good complementary hardening but is a separate, optional task; the
  receiver guard alone closes the hang.
- Do not shorten `dead_after` to paper over this — that would hurt real slow links. The
  progress deadline is a different axis (no *data* vs no *byte*).
