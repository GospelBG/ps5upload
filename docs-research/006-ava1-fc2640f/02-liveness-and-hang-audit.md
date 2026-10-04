# AVA1 liveness / hang audit at fc2640f (2026-10-04)

Question put to this pass: can anything hang, and is the transfer loop solid? Read for it:
`conn.rs` (read/write deadlines), `link.rs` (heartbeat and dead-peer), `recv.rs` run loop and
the sync-batch task, `seq.rs` `run_passes`/`one_pass`, and the flow-control accounting.

## Verdict

Solid. The design defends liveness in layers, and every blocking primitive I traced has a
bound. One genuine gap: the only time-based abort is byte-level, so a peer that heartbeats but
never sends data keeps a job alive with no progress. Narrow, both peers are authenticated, but
worth one cheap guard. Details below.

## What makes it hard to hang (verified)

1. **Every socket read and write has a deadline.** `conn.rs` wraps each `write`/`flush`/`fill`
   in `tokio::time::timeout` of `idle + len/min_rate` (`Pace::frame_budget`). A peer that
   stops taking or sending bytes trips the timeout; it cannot block a task forever.
2. **A byte-level dead-peer watchdog.** `link.rs` pings when its outbox is idle and declares
   the peer dead after `dead_after` (12 s) without a received byte, with a bounded
   oversleep grace for a suspended machine. Torn-down link unblocks every waiter.
3. **Credit exhaustion degrades to a timeout, not a hang.** The sender blocks when it has no
   credit; that block is a bounded socket write, so a receiver that stops granting credit
   becomes a write timeout and then a dead peer. A frame larger than outstanding credit is
   refused with `ERR_CREDIT` (recv.rs:973), so nothing is buffered unboundedly.
4. **The receiver never stops reading (ruling 11).** Disk work, including the fsync batch,
   runs on its own `spawn_blocking` task (`batch_handle`); the run loop keeps draining the
   inbox and answering pings while a batch runs. A slow drive slows throughput, it does not
   make the job look hung (this is the T28 fix for the 223k-file download).
5. **No lock is held across an await in the receiver.** Every `Sink`/outboard `Mutex` is
   taken inside a `spawn_blocking` closure or a non-async block that drops the guard before
   the next await. The classic tokio executor stall is absent.
6. **Bounded buffers and concurrency.** The reorder map holds at most one credit window
   (64 MiB); `WRITE_PAR = 4` caps concurrent bundle writes via a semaphore.
7. **No re-send livelock.** `seq.rs` `run_passes` caps decode passes at `MAX_PASSES`; a file
   that keeps failing verification ends the job with `Read::Failed` rather than looping.
8. **No resume stall when the outboard is thin.** A resumed file whose durable ranges cover
   every group but whose persisted CVs are incomplete is not wrongly marked finished; it
   falls into the decode pass, which reads the source to recompute the root and send it
   (seq.rs `one_pass`). So resume always has a way forward.

## The one gap: no end-to-end progress watchdog

`link.rs` resets the dead clock on *any* received byte, and a Ping is a frame
(link.rs:425-426, "Any byte counts"). The receiver run loop's only time-based exits are
`o.cancel` and the link's byte-level death. So a sender that is alive enough to let its
heartbeat task ping, but whose data pump is wedged (for example a source read blocked on a
stuck network filesystem), keeps the job open indefinitely with zero file bytes moving. The
receiver waits forever; nothing aborts it.

- **Severity:** low. It needs a half-alive, authenticated peer that pings but cannot produce
  data, and stays that way. A cleanly dead or disconnected peer is caught by the byte
  watchdog as normal.
- **Fix (cheap):** add a progress deadline to the run loop. Track the time of the last
  *file-data* frame (or last `outstanding` decrease); on the 50 ms tick, if the window has
  been fully open with no data for some multiple of `dead_after` (say 3x), send `JobCancel`
  with a stall reason and return. Keep it generous so a legitimately slow-but-moving drive is
  never cut. Optionally mirror it on the sender: abort a source read that exceeds its own
  deadline rather than going quiet and pinging.
- **Test:** a mock sender that completes the handshake, opens a job, then only pings; assert
  the receiver aborts within the progress deadline instead of waiting forever.

## Carried from earlier (unchanged)

The governor `best_rate` non-decay (006/01 §2) and the minor notes in 005/01 §3 are not
liveness issues. The zip restart-window fix (006/01 §1) is correctness, not a hang.
