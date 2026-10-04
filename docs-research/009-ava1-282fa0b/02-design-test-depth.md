# Design 2 — test depth: sanitizers in the gate, exhaustive torn writes, a soak

## Goal
Catch the leak / UB / use-after-free class in the console C before it ships to a device with
no debugger, make the crash-consistency claim exhaustive, and expose slow leaks.

## 2a. Run `ava1-ctest` under ASan + UBSan in CI (S)
Today only the two `make ava1-fuzz-c` targets build with `-fsanitize=fuzzer,address,undefined`;
the ctest suite (every console C path the host can run) never does.
- `engine/crates/ava1-ctest/build.rs`: when `AVA1_CTEST_SANITIZE=1`, add
  `.flag("-fsanitize=address,undefined").flag("-fno-omit-frame-pointer")` to **every** `cc::Build`
  (the ava1 block ≈60-110, monocypher, blake3, shims) and emit
  `cargo:rustc-link-arg=-fsanitize=address,undefined` so the test binaries link the runtime.
  Use clang (`CC=clang`) in that job; keep gcc for the normal job.
- Tests that intentionally crash (`ava1_apply_crash`, `crash_at`) use `_exit`/abort paths —
  confirm they do not trip LSan false positives; set `ASAN_OPTIONS=detect_leaks=1:abort_on_error=1`
  and `LSAN_OPTIONS=suppressions=engine/crates/ava1-ctest/lsan.supp` for the known
  intentional-exit tests.
- `.github/workflows/engine-ci.yml`: a new job `ava1-ctest-sanitize` (ubuntu-24.04, clang) running
  `AVA1_CTEST_SANITIZE=1 cargo test -p ava1-ctest -- --test-threads=1`. Allowed to be slow.
- Makefile: `make test-ava1-sanitize` for local use; add to the CUTOVER §3 workspace gate line.
- Prerequisite: `origin/ava1-fixes-007` (ctest does not build on ubuntu-24.04 without it).

## 2b. Exhaustive torn-write sweep for journal and pack recovery (S)
Today's tests cut at chosen points (`a_torn_log_tail_resends_only…`, `a_crash_after_the_pack_fsync…`).
- Rust (`journal.rs`, `packlog.rs` tests): build a small journal / pack with N records, then for
  **every** byte length L in `0..=len`, truncate a copy to L and assert: replay never errors,
  never yields a record that was not fully written, and `set_len` leaves the file at a record
  boundary; for the pack, `recover` re-makes exactly the files whose records are whole and
  resets the rest. O(N·len) with small inputs — a few hundred ms.
- Add bit-flip variants: flip one byte at every offset of the last record → CRC must reject it
  and recovery must treat that record as lost (never silently accept).
- C (`ava1-ctest`): the same sweep through the shim for `ava1_pack_recover` on one segment.

## 2c. Host soak (S to write, hours to run)
- `ava1-ctest/tests/soak.rs`, `#[ignore]`, driven by `AVA1_SOAK_MINUTES`: loop opening jobs
  (mixed small/large, uploads with cancel-and-resume, zip downloads, parked jobs reaped), each
  iteration asserting the data layer's counters return to baseline (`unswept 0`, `segments 0`,
  job table empty, open-fd count via `/proc/self/fd` unchanged, RSS growth bounded).
- Run under 2a's sanitizer build in a weekly scheduled CI job (not per PR).
- The console agent's current "small leaks / wall-clock job cleanup" fixes get regression tests
  here (a job created, parked past `park_ms` with the monotonic clock advanced, freed exactly once).

## Acceptance
CI has a green sanitizer job; torn-write sweeps exist for both logs on both sides; a weekly soak
runs 60+ minutes clean under ASan.
