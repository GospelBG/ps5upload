# Hardware phase: go/no-go gates, outage triage, test plan (e2208e6)

## 1. Gates to clear before any user-facing release (ordered)

1. **HW-0 Pro outage explained** (CUTOVER §3; no release until it has an explanation). §2 below.
2. **HW-2 reconciled.** Pick one and write it into CUTOVER §3 so the doc and code agree:
   - (a) *Recommended:* keep AVA1-only in code, rewrite the gate to "no release from this branch
     until the §3 gates are green; the last FTX2 release remains the shipped version until
     then." This acknowledges there is no in-branch fallback and makes the hardware pass the
     hard gate it already is.
   - (b) Restore a kill-switch. Large code churn against a decision already made; only if the
     hardware pass surfaces something that cannot be fixed quickly.
3. **Nonce audit complete** (006/06 remaining steps; see 01 §4 for what is already verified).
4. **HW-1 and HW-3 landed** (guides 03, 04). Both are small and reduce hardware risk directly.
5. **Progress watchdog landed** (006/05): the one remaining hang.
6. **Hardware pass on both consoles at the release commit**, all three drives each, per
   CUTOVER §3 bullet 1 and the §4.2/§4.3 "not yet measured" tables. Run with the debug timing
   flag on so the end-of-job line and `recovered N logged files` lines are captured.
7. **Workspace gate green** on the release commit (fmt, clippy −D warnings, workspace tests,
   `ava1-ctest` single-threaded, `cargo check --locked`, client lint + vitest, `make ava1-fuzz-c`).
8. Conformance C-1 reconciled (SPEC §11.5 vs console `Resume`) before the SPEC is called final.

## 2. Pro outage triage (read this before the next console run)

Facts: ~09:10 on 2026-10-03 the Pro stopped answering ping and every port shortly after an
instrumented helper ("extra stderr timing lines only") was sent; last keep-awake ack 09:10:50.
Last payload commit before it: `4b57d75` 07:13 ("index large files... stats").

What this pass established about that commit: it is purely userland (index arrays, snapshots,
mutexes, a stats `fprintf`); it adds **no** kernel-facing syscalls (no rename/fallocate/ptrace/
mmap); every allocation is bounded and freed. On its own it can crash the *process* (which
leaves a signal breadcrumb in `stderr.log` via `write_fatal_breadcrumb`), not the network
stack. ptrace is confined to the legacy management files (`cheats.c`, `register.c`,
`shellui_rpc.c`, `hw_info.c`), none in `payload/ava1/`.

Triage, in order, when the console is back:
1. Read `/data/ps5upload/stderr.log` (and `.old`; rotation is at 512 KB, same-device rename).
   - Ends with a **signal breadcrumb** → userland crash; the frame type named is the lead.
   - Ends **mid-transfer with no breadcrumb and no clean shutdown** → kernel panic or power
     event; the process never got to write. Check the console's own crash notice.
   - Shows a **clean stop / rest-mode path** → keep-awake failed; the last ack at 09:10:50 says
     keep-awake was live until then, so look at what changed in the keep-awake sender.
2. If kernel panic: list which kernel-sensitive paths the run exercised — any rename (both
   guarded, but see HW-1's −1 case), any ptrace-backed management method called during the
   run (hardware readings, launch, register), helper replace timing (the 60 s cooldown exists
   now), and memory pressure (README notes kstuff + other payloads can knock consoles off).
3. Repeat the instrumented run **with HW-1 fixed and HW-3's runtime switch available**, so a
   second incident can be bisected by turning durable-by-log off without reflashing.
4. Only then continue the §4 table.

## 3. Hardware test plan (what to run, what to read)

- **Smoke (automated, opt-in):** `REAL_PS5_ADDR=<host> cargo test -p ps5upload-tests --test
  ava1_live -- --ignored`: hash compare, recursive chmod, syslog tail, folder upload smoke/perf.
- **Pairing:** engine-launched helper pairs silently; hand-loaded helper shows the 6-digit code;
  two engines sharing one identity file must be warned, not silently fight.
- **CUTOVER §4 table, every row, both consoles, three drives each**, with
  `/data/ps5upload/debug/ava1-timing` present. For each row capture the end-of-job line
  (`dirs` near 0 is the durable-by-log signature), `preallocate took N ms`, and any
  `slow drive: ... fsync per chunk from now on`.
- **Durable-by-log specific:** tiny-file corpus and the 223k-file game on each drive; then a
  **crash-and-recover**: kill the helper mid-upload (and once mid-sweep), restart, confirm the
  `recovered N logged files, M lost (resent)` line and that the job completes with every file
  hash-verified (the `live_ps5_hash_compare_file` test per sampled file).
- **Settling:** a new-folder upload must show "Finishing on the console..." and complete within
  `SETTLE_MAX`; a merge/single file settles behind JobDone.
- **Liveness on hardware:** pull the Ethernet mid-upload → the job must fail within
  `dead_after` 12 s (+ grace), not sit. Resume after reconnect must not restart the archive on
  a stored-zip download (005 §5 fix). Cancel then immediately re-upload must be answered
  (BUSY → retry), not hang.
- **No-fallback path:** point the engine at a console running an *old* helper → must surface
  `helper_not_ava1` promptly, not spin; at an unpaired console → `not_paired`.
- **Cross-device:** upload to `/mnt/usb0` and `/mnt/ext*` (different `st_dev` from `/data`)
  must land via same-directory rename, never `EXDEV`, never a panic.

## 4. Decision rule

Go for user release only when §1 items 1-7 are all green on the same commit. Until then the
branch is fine for engineering hardware runs **with HW-1 and HW-3 landed first**, because those
two are exactly the levers an incident investigation needs.

## 5. Core-crate test result (worktree at e2208e6)

Pending at the time of writing (`cargo test -p ava1 -p ps5upload-ava1 --no-fail-fast`); the
cloud session records the outcome in a follow-up note when the run completes.
