# Design 3 — nightly hardware-in-the-loop run

## Goal
Every night, a real console runs the live checks and a short perf row at the current `ava1`
head, and a regression (a failed check, a hang, or a throughput drop beyond a tolerance) is
visible the next morning without anyone typing anything.

## Non-goals
Replacing the manual CUTOVER §4 table at the release commit (that stays a human pass). This is
regression detection between releases.

## Design
- **Runner:** a small always-on host on the console's LAN (the homelab box the Docker image
  targets is ideal) registered as a GitHub self-hosted runner with label `ps5-lab`, or a cron
  job that pushes a result file. Secrets: `REAL_PS5_ADDR`, `REAL_PS5_SRC_DIR`, the paired
  engine identity (`/data/.ps5upload` on the runner), the engine API token (design 1).
- **Console state:** the run starts with `node.readiness` → if `helper_not_ava1`, send the
  current helper via the engine's launch path (the engine already embeds the ELFs); if
  `not_paired`, fail fast with a clear message (pairing is a one-time human step). Keep-awake on.
- **What runs (in order, each with a timeout):**
  1. `cargo test -p ps5upload-tests --test ava1_live -- --ignored` (hash compare, chmod, syslog
     tail, folder upload smoke/perf) — today's opt-in set.
  2. A fixed corpus upload per drive (`/data`, `/mnt/usb0`, ext): 2,000 tiny files; one 4 GiB
     file; the 20k-file sample of the game corpus. Record the engine's `JobSummary` line and the
     console's end-of-job line (`dirs`, `preallocate took`, `slow drive` lines).
  3. Pull-the-cable liveness: a scripted `ifdown`/`ifup` on the runner mid-upload (or a
     firewall rule): assert the job fails within `dead_after` + grace and resumes after
     reconnect without restarting the archive (stored zip) — the 007/02 §3 checks, automated.
  4. Crash-and-recover: `node.shutdown` mid-upload, relaunch helper, assert `recovered N logged
     files` and completion with hash verification.
  5. Zip download of a 5k-file folder; `unzip -t`.
- **Result:** one JSON per run (`results/YYYY-MM-DD.json`: pass/fail per step, MB/s per row,
  console stderr tail) committed to a results branch or uploaded as an artifact; a tiny script
  compares throughput to the trailing 7-day median and flags > 15 % drops. Post a summary to the
  PR/commit as a check (or a GitHub issue on failure).
- **Safety:** the run only ever writes under a dedicated `/data/ps5upload-nightly/` root on each
  drive and deletes it at the end; never touches game installs.

## Entry points
- `engine/crates/ps5upload-tests/tests/ava1_live.rs` (extend with steps 2-5 as `#[ignore]` tests
  taking env knobs).
- `Makefile` `test-ava1` (add `nightly-hil` target that sequences the above and writes the JSON).
- `.github/workflows/nightly-hil.yml` (`schedule: cron`, `runs-on: [self-hosted, ps5-lab]`).

## Acceptance
Three consecutive green nights on both consoles; one deliberately injected regression (e.g.
set `PS5UPLOAD_AVA1_LANES=1`) is flagged by the throughput comparison.
