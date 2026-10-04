# AVA1 evaluation 006: status per category

Mirrors `docs-research/006-ava1-fc2640f/03-evaluation-checklist.md` (written against fc2640f),
re-checked on branch p3-r006 (from ava1 38676afe). PASS = solid with evidence, FIXED = a gap or
watch item closed in this pass (commit cited), OPEN = not done, with the reason.

| cat | area | status | notes |
|---|---|---|---|
| A | Protocol conformance and parity | PASS, FIXED | Vectors, ctest parity, SPEC/CUTOVER unchanged in kind. The WATCH item (negative vectors) is FIXED: `conn::tests::{a_frame_cut_at_any_byte_ends_the_stream_cleanly, misordered_dropped_and_duplicated_frames_do_not_open, a_body_past_the_reader_cap_is_refused_before_it_is_read}` and the C twin `the_c_reader_refuses_truncated_and_misordered_frames` (same wire bytes, same count opened, `AVA1_E_CLOSED` / `AVA1_E_TAG`). Header negatives were already in `frame.rs` (flipped bit, wrong magic, over 16 MiB) and the decoders are under libFuzzer (`make ava1-fuzz-c`). `ava1-chaos` is a link-fault proxy (latency, bandwidth, blackhole, kill), not a frame fuzzer; it was never meant to cover malformed frames. |
| B | Cryptography and handshake | FIXED (now PASS) | Nonce/counter audit written up in `AUDIT-nonce.md`: every `set_key` site, C and Rust counters, resend on another lane, pool and zero-copy sealing, the `keys.rs:438` counter (test code). Ceiling guard on both stacks, comment reworded, lockstep, no-reuse and ceiling tests (review 006 #1). |
| C | Pairing and trust | PASS | Unchanged. |
| D | Authorization and path safety | PASS, FIXED | The WATCH item (`fs.stat`/`fs.list` disclosure) is decided: not narrowed, documented in `SPEC.md` ("Scope of `fs.stat` and `fs.list`"): metadata only, paired peer only, FTX2 parity, the browsers need it. Revisit with a read-only pairing tier. |
| E | Flow control and backpressure | PASS | Unchanged. |
| F | Liveness / hang safety | FIXED | Progress watchdog (review 006 #2), both receivers, `SPEC.md` §12.8, `ERR_STALLED` = 18. Tests: ping-only sender cancelled (Rust and C), slow-but-moving sender not cut, slow disk batch not a stall. A resumed job with durable bytes waits 15 min, because its sender may hash groups it will not resend without producing a frame. The optional sender-side source-read deadline is OPEN: a blocking read cannot be cancelled, and the receiver guard already closes the hang. |
| G | Crash consistency and durability | PASS | Durable-by-log has landed (`CUTOVER.md` §4.3). Hardware measurement of it is OPEN (under L). |
| H | Resume correctness | FIXED | Zip whole-but-unfinished window: done in be78641a and 0f45ceeb; verified against guide 04 (review 006 #3), nothing missing. |
| I | Data integrity and verification | PASS | Unchanged. |
| J | Archive handling | PASS (RAR edge cases OPEN) | No archive code was touched. The RAR password/cancel edge cases from Task 11 were not re-examined in this pass: OPEN until a change touches archives. |
| K | Management plane | PASS (hardware OPEN) | 004 O1/O2 resolved: `SPEC.md` §7.1.1 states the 10 s grace after the first terminal delivery and `job.cancel` semantics, with `a_finished_op_stays_listed_for_a_grace_then_is_released_on_the_next_read` and the cancel tests in `ava1-ctest/tests/job_run.rs`. Every `MGMT_METHODS.md` row is `ported` (the 27 `todo` rows are gone). Hardware verification of the ported rows is OPEN: no consoles in this pass. |
| L | Performance and throughput | PASS (matrix OPEN) | Governor `best_rate` now decays (`BEST_DECAY`, governor.rs). The CUTOVER §4.1 lanes x chunk hardware matrix is OPEN: needs both consoles. |
| M | Resource bounds | PASS | Unchanged; see T for the new job cap. |
| N | Concurrency correctness | PASS | Unchanged. |
| O | Error handling and taxonomy | PASS (not exhaustive) | `ERR_STALLED` is a new typed code, surfaced as `Refused` (console) or `Disconnected("progress stalled")` (engine receiver). The "every `?` ends peer-visible" sweep is still sampled, not exhaustive: WATCH. |
| P | Observability | PASS | The stall logs one line (`progress stalled: no file data for N s`) on the engine and the console. |
| Q | Platform / console constraints | PASS | Unchanged. |
| R | Testing and verification coverage | FIXED | The two tests the review named exist (zip, heartbeat-without-data), plus the negative frame vectors and the nonce tests. |
| S | Build, cutover and migration | PASS (gates OPEN) | The zip row is struck. The hardware release gates in `CUTOVER.md` §3 stay OPEN. |
| T | Operability / configuration | FIXED | Job admission is bounded on both receivers. Console: the 32-job table answers `ERR_BUSY` (`ava1-ctest/tests/admission.rs`, new). Engine host: was unbounded; now `MAX_JOBS_PER_SESSION` = 32 per session, `JobOpen` gets `ERR_BUSY`, `Resume` a `JobMap{ERR_BUSY}` (`a_flood_of_job_opens_is_bounded_with_err_busy`); `SPEC.md` §8. |

## Re-confirm items from the work order (07)

* `fs.stat`/`fs.list` scope: decided, documented (D).
* Governor `best_rate` decay: done before this pass (L).
* 004 O1/O2: resolved (K).
* Concurrent `JobOpen` ceiling: confirmed on the console, added on the engine host (T).
* Negative/fuzz vectors: added (A).
