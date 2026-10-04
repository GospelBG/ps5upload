# Session 007 fixes — branch `ava1-fixes-007` (cut from `ava1` e2208e6)

The cloud session implemented the session-007 findings that need no console, on a branch of
its own so the local session's in-progress 006 work (progress watchdog, nonce audit) is not
disturbed. Merge `ava1-fixes-007` into `ava1`; every change is small and carries a test.

## Code (console C, compiled and tested through `ava1-ctest`)

Branch pushed: `origin/ava1-fixes-007` (one commit on top of e2208e6).

| finding | change | test |
|---|---|---|
| HW-1 | `ava1_apply.c` `commit_large` and `finish`: `same_device` −1 ("unknown") is refused as `ERR_IO` ("could not verify the destination drive") instead of falling through to `rename()`. 0 stays `ERR_CROSS_DEVICE`. | `apply.rs::c_commit_refuses_an_unknown_device_answer_too` (part file stays, no rename) |
| HW-3 | Runtime durable-by-log switch: `ava1_data_log_small_for_job()` returns 0 when `/data/ps5upload/debug/ava1-log-small-off` exists (console) or `PS5UPLOAD_AVA1_LOG_SMALL_OFF` is set (host). Latched once per job in `ava1_job.c::create` into `j->log_small`; `apply_record` reads the job's latch. One stderr line when off. Recovery (`ava1_pack_recover`, called from `ava1_recv.c:330`) is not gated on it. | `log_small.rs::the_runtime_switch_turns_the_log_off_per_job_and_recovery_ignores_it` (OFF job: no segment, nothing unswept, files present even after the switch is cleared mid-job; next job logged) |
| build | `ava1_op.c:143`: `(int)AVA1_ERR_CANCELLED` — the constant is `15ULL`, `fn` returns `int`; gcc 13 `-Werror=sign-compare` rejected the mixed-sign `?:`. | build |
| build | `takeover_flag.c:14`: `<sys/sysctl.h>` only under `__APPLE__`/`__FreeBSD__`; glibc ≥ 2.32 has no such header and the only use is already `#ifdef KERN_ARND`. | build |

The two build items mean **`ava1-ctest` did not build on a modern Linux host before this**;
the workspace gate (CUTOVER §3) requires it. With both fixed, the C compiles with zero warnings
under gcc 13.3 (Ubuntu 24.04, glibc 2.39).

## Docs

| finding | change |
|---|---|
| HW-2 | CUTOVER §3: the stale "the default stays FTX2 for any release that goes to users" gate is replaced by "no release to users from this branch until every gate in this section is green; AVA1 is the only transport in this branch, no in-branch fallback; the previously shipped FTX2 release remains the version users get meanwhile". |
| HW-3 | CUTOVER §4.3 names the runtime switch file and env var. |
| C-1 | SPEC §11.5: a `Resume` does **not** restart the credit window (matches `ava1_data.c:758` and the engine, which never sends `Resume`); senders that lost credit state reopen with `JobOpen`. |
| HW-1 | SPEC error table row 16 (`ERR_CROSS_DEVICE`): an unreadable `st_dev` on either side is refused as `ERR_IO` rather than attempted. |

## Verified on this host (gcc 13.3, glibc 2.39)

- `cargo test -p ava1-ctest --test apply --test lifecycle -- --test-threads=1`: 25 + 11 passed
  (includes the HW-1 test).
- `cargo test -p ava1-ctest --test log_small --test log_small_fail -- --test-threads=1`:
  11 + 21 passed (includes the HW-3 test), default flags, so the C is -Werror-clean on gcc 13.
- `cargo fmt --check -p ava1-ctest`: clean.

Note for the harness: a `CApplyJob` holds the ctest `C_SERVER` lock until it is dropped, so a
test must keep one job alive at a time (the first draft of the HW-3 test deadlocked on this;
fixed by scoping).

## Not done here (needs the console or is the local session's)

- HW-0 (Pro outage): triage per `02-hardware-go-no-go.md` §2 when the console is up.
- 006/05 progress watchdog and 006/06 nonce audit: in progress in the local session.
