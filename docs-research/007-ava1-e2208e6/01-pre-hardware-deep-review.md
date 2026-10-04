# AVA1 pre-hardware deep review: `ava1` fc2640f → e2208e6 (2026-10-04)

Sixty-six commits, 207 files, +20,018/−2,744. The branch is at the hardware/console test
phase, so this pass is weighted toward what can damage a console or lose data on real hardware,
and toward the release gates in `protocol/ava1/CUTOVER.md` §3. Read in full: `ava1_apply.c`
(+1,545, the console durable-by-log), `packlog.rs`, the `recv.rs` sink integration, both
console rename sites and `same_device`, `pack_recover`, the sweep, `ava1_data_cfg` knobs,
`conn.rs`/`ava1_conn.c` counters, the BUSY/open-ack fix, CUTOVER §3/§4.2/§4.3, the release
notes draft, the live harness, and the suspect commit before the Pro outage.

## 1. Verdict

The code is in very good shape: durable-by-log landed on both ends with crash recovery that
verifies content by hash, the one hang I had not found (an unanswered JobOpen after a cancel)
is closed, every 004/005 finding is fixed, and the console C under review is careful about the
kernel (both renames guarded, preallocation kept, memory bounded, no new kernel-facing
syscalls in the instrumented helper).

**But the branch is not releasable to users yet, by its own gates**, and two of those gates are
now in tension with the code. In order of consequence:

| # | finding | class | action |
|---|---|---|---|
| HW-0 | **Pro outage of 2026-10-03 unexplained** (stopped answering every port at ~09:10 after an instrumented helper; last keep-awake ack 09:10:50). Kernel panic is one named hypothesis. | release gate | triage per 02 §2 before any further console run with new helper code |
| HW-2 | **AVA1 is the only transport, no fallback or kill-switch** (`1a922f7`, 00:50), while CUTOVER §3 still says "the default stays FTX2 for any release that goes to users" until the hardware pass is green (edited after, 04:19). The doc contradicts the code. | release gate | reconcile explicitly (02 §1); do not cut a user release from this branch until §3 is green |
| HW-1 | `same_device` returns −1 when a stat fails and both rename sites refuse only on 0, so "unknown" falls through to `rename()`. The one error whose cost is a kernel panic is fail-open. | console safety (low likelihood, catastrophic) | fail closed on −1 (guide 03) |
| HW-3 | The durable-by-log console kill-switch `AVA1_LOG_SMALL_OFF` has **no runtime setter**; it is compile-time only, while CUTOVER §4.3 presents it as an operator knob. | hardware-phase operability | debug-file flag like `ava1-timing` (guide 04) |
| HW-4 | Session 006 guides **05 (progress watchdog) and 06 (nonce audit) have not landed**. 06 is part of CUTOVER's own "crypto re-review" gate. This pass closes several of its steps (below); the rest remain. | release gate (06), liveness (05) | land both (006/05, 006/06) |
| C-1 | SPEC §11.5 says a `Resume` restarts the credit window and re-sends `Credit`; the console only does that for `JobOpen` (`ava1_data.c:758`). Dormant: the engine sender never emits `Resume` (SPEC says engines reopen with `JobOpen`). | conformance | amend SPEC or console before the protocol is published as final |

## 2. What landed and was verified (console C, the hardware-critical part)

- **Both `rename()` sites guarded** (`commit_large` 2680-2687, `finish` 2807-2813): `same_device`
  checked, `ERR_CROSS_DEVICE` on a cross; the tree landing also refuses if the destination
  appeared mid-upload. Only the −1 case (HW-1) is open.
- **Preallocation preserved** via `ava1_platform_preallocate` in `lfile_open`, now outside
  `j->mu` with an `opening` flag so other workers wait on the file, not the job (003 §2.1).
  UFS sparse-collapse protection intact.
- **Pack write path** (`pack_roll`/`pack_append` 946-1031): consistent `pack_mu → mu` order;
  tail reserved under the lock so records are contiguous; a failed `pwrite` unrefs; the hole
  it leaves is harmless because recovery is reference-driven, not a sequential walk.
- **`pack_read`** validates length bounds, version byte, CRC32C and decode on every read.
- **`ava1_pack_recover`** (1799-1930): walks each journaled range, validates every record,
  **re-hashes the data against the recorded BLAKE3 root and checks the size**, re-makes the
  final file only if `small_matches` fails, resets any unswept file it cannot recover (resent),
  stops cleanly at a torn tail, and removes stray segments. This is strictly safe.
- **Sweep** runs as a worker-pool item (never the job thread), single in flight, age-gated,
  batch-bounded, backs off on failure (five failures → reported in `Status`), journals a
  sweep record after directory syncs. `sweep_one` re-makes a missing/wrong-size file from the
  log and re-makes again after an fsync retry that may have lost pages.
- **Memory**: every `lfl_snapshot` is freed (2390, 2545, 2777), index growth bounded by the
  manifest and pruned per batch, allocation failure signalled (`*n = UINT32_MAX`). No OOM
  vector in the new code.
- **Kill-switch** exists in code (`ava1_data_log_small()`), see HW-3 for reachability.

## 3. What landed and was verified (engine)

- `packlog.rs` mirrors the console: same `AVA1PCK1` magic, CRC32C framing, torn-tail stop,
  ref-driven `recover` with a `remake` callback. The two `unwrap`s in `append` are guarded
  (a roll precedes the write; `remove_if_free` only closes an already-closed segment).
- **Hang closed**: a `JobOpen` for a still-registered cancelled job used to go unanswered
  forever. Now the receiver answers `BUSY` at once; the sender has `OPEN_ACK_TIMEOUT` 30 s,
  retries like BUSY, and the BUSY retry is itself bounded (fixed count, jittered backoff
  250 ms→5 s, ~45 s, then a typed failure).
- **Settle bounded**: `SETTLE_MAX` 30 s (or cancel) for the receiver's files to settle after
  `JobDone`.
- 005 §5 zip whole-but-unfinished fixed (`be78641`), then extended to several whole-but-undone
  files (`0f45cee`). 005 §3: `BEST_DECAY` 0.99 landed; speedtest not resent (tested);
  path-policy comment. 004 O1/O2 fixed (10 s grace, SPEC 7.1.1 wording).
- Readiness UX for the no-fallback world: tested `helper_not_ava1` ("send the helper again")
  and `not_paired` ("open pairing") tokens; `legacy_guard` enforces a 60 s cooldown on helper
  replace (the comment notes faster restarts have taken consoles down before).
- Release notes draft is honest: marked DRAFT, numbers held for the hardware pass, "do not
  publish as is"; every claim matches verified behaviour.

## 4. Crypto gate: what this pass closes toward guide 006/06

Verified at e2208e6: writer and reader are separate structs with independent counters (both
Rust and C: `send_ctr`/`recv_ctr`, `ava1_conn.c:188/274`); **both sides use `uint64_t`/u64**,
so no width mismatch; keys are direction-specific and lane keys fold in lane + both fresh
nonces (parity vector-checked by ctest); the counter increments on every sealed frame
including pings; `set_key` is called only at handshake/lane setup. Still open from 006/06: the
counter ceiling guard, the misleading "all-zero nonces" comment (`keys.rs:228`, still present),
the lockstep test, and a written confirmation of the C nonce byte layout (ctest's
`ava1_test_conn_open_frame` covers it; state it).

## 5. Minor notes

- `sweep_one`'s first pass checks size only; `small_matches` (hash) exists and small files are
  bounded, so a content check there would be cheap belt-and-braces. Recovery already hashes.
- The pack log is sequential appends (no holes), so UFS sparse collapse should not apply;
  CUTOVER defers segment preallocation correctly.
- The `ava1_hw_integration.rs` file is a scripted mock of the hardware-readings methods, not
  hardware-in-the-loop. The real console checks are the `#[ignore]` tests in `ava1_live.rs`
  gated on `REAL_PS5_ADDR` (hash compare, recursive chmod, syslog tail, folder upload smoke
  and perf) plus the manual CUTOVER §4 table.
- Core-crate tests (`ava1`, `ps5upload-ava1`) were started on a worktree at e2208e6; result
  recorded in 02 §5 when the run finishes (the workspace is large).
