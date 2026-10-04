# AVA1 review: `ava1` e2208e6 → 282fa0b (2026-10-04)

Ten commits: the local session landed everything from session 006 (nonce audit, progress
watchdog, the checklist's open items) and mirrored the 20-category checklist as
`protocol/ava1/EVAL-006.md`. Read in full: the `recv.rs` and `ava1_apply.c` watchdogs and the
follow-up fix (`5f8befc`), `conn.rs`/`ava1_conn.c` ceilings, `AUDIT-nonce.md`, the admission
bound in `server.rs`, SPEC §4.4/§12.8 and the fs.stat scope text, CUTOVER ticks, and the new
test files (`stall.rs`, `admission.rs`, `data_rust.rs`, `lint.rs`).

## Verdict

Every 006 item is closed, correctly and with tests. Category B (crypto) is now a verified PASS;
category F (liveness) has no known hang left. The session-007 fix branch merges onto this head
cleanly and all six console C suites pass on top of it (74 tests). What remains before a user
release is exactly the hardware list in 007/02: the Pro outage, the hardware pass, the
workspace gate.

## Verified

| item | what landed | assessment |
|---|---|---|
| Nonce audit (006/06, release gate) | `AUDIT-nonce.md` verdict PASS: every `set_key` site enumerated and pinned by `lint.rs`; `NONCE_CEILING = u64::MAX-1` on both stacks (writer refuses to seal, reader refuses to open, `checked_add`, C `AVA1_NONCE_CEILING`); comment reworded; lockstep/no-repeat tests in Rust and C; the `keys.rs:438` counter explained. SPEC §4.4 now states the counter semantics, including that a frame resent on another lane is resealed under that lane's key and counter. | **PASS.** Closes category B. |
| Progress watchdog (006/05) | Engine: progress = admitted data frame, root, finished write, or a sync batch that had work; fires only when the sender owes bytes, nothing of ours is in flight, and a lane is up; 36 s, 900 s for a resumed job (sender may hash durable groups for a long time without a frame; decided once at open/reattach). Console: the same as a progress signature on the job thread, armed only while attached and ready, `ERR_STALLED` with the journal kept so the sender resumes. SPEC §12.8. Tests: ping-only sender ends; slow-but-moving and slow-disk do not. | **Correct and conservative.** The resume allowance is long but bounded, and the follow-up (`5f8befc`) fixes the three things worth fixing (decide resume once, done-only resumes count, clock gated on lanes). The optional sender-side read deadline was rightly skipped: a blocking read cannot be cancelled; the receiver guard closes the hang. |
| Job admission (checklist T) | 32 jobs per session on the engine host (`MAX_JOBS_PER_SESSION`), `JobOpenAck{ERR_BUSY}` / `JobMap{ERR_BUSY}` past it, session untouched; console's 32-slot table confirmed with a test. | **PASS.** The sender's existing bounded BUSY retry covers it. |
| Negative frame vectors (checklist A) | Added to `conn` tests. | PASS. |
| `fs.stat`/`fs.list` scope (checklist D) | Decided not narrowed, with a written rationale in SPEC: metadata only, contents still policy-gated, the browsers need it, revisit if a read-only pairing tier appears. | **Agree.** A reasoned decision, not an omission. |
| Zip whole-but-unfinished (006/04) | Re-verified against the guide: `x == size` in flight, `x > size` refused, descriptor at commit, several whole files without Done, hole/second prefix refused. | Done. |

## Session-007 fix branch

`origin/ava1-fixes-007` was merged onto 282fa0b (clean, no conflicts: the watchdog's new job
fields and my `log_small` latch are orthogonal) and re-tested: `ava1-ctest` admission 1, apply 25,
lifecycle 11, log_small 11, log_small_fail 21, stall 5 — all green, default flags, gcc 13.3. It
carries HW-1 (fail-closed `same_device`), HW-3 (runtime durable-by-log switch), the two Linux
host-build fixes, the CUTOVER §3 reconciliation (HW-2) and the SPEC §11.5 `Resume` wording (C-1).
Ready to merge into `ava1`.

## Still open (unchanged from 007/02)

1. HW-0: the Pro outage, triage per 007/02 §2 when the console is up.
2. Hardware pass on both consoles, all drives, at the release commit (CUTOVER §3/§4.2/§4.3).
3. Workspace gate on the release commit. Note: `ava1-ctest` now builds on a Linux host only with
   the two fixes in `ava1-fixes-007`.
