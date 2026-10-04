# Session 009 — designs for what is still lacking (head 282fa0b)

Each note is an implementation-ready design for one gap from the 008 assessment: goal,
non-goals, the design, exact entry points in the code at 282fa0b, steps, tests, acceptance,
rollout. Sizes: XS < half a day, S ≈ a day, M ≈ 2–4 days, L ≈ a week.

| # | note | gap | size | depends on | blocks release? |
|---|---|---|---|---|---|
| 1 | `01-design-api-auth.md` | engine API/web UI has no authentication | S | — | **yes** for Docker/web deployments |
| 2 | `02-design-test-depth.md` | ctest never runs under ASan/UBSan; torn-write coverage sampled; no soak | S | fix branch `ava1-fixes-007` (Linux build) | gate quality |
| 3 | `03-design-hil-nightly.md` | no hardware-in-the-loop automation | M | a console on the network | process |
| 4 | `04-design-job-telemetry.md` | field diagnostics are stderr lines only | M | — | no |
| 5 | `05-design-download-parity.md` | download commit holds the sink lock across fsync+rename, serial | S | — | no |
| 6 | `06-design-conformance-suite.md` | no second implementation has spoken AVA1 | L | — | no (protocol maturity) |
| 7 | `07-group-delta-integration.md` | group delta (003/05) not started; integration points moved since | pointer + M | durable-by-log (landed) | no (post-cutover feature) |

Two findings from gathering this that need no design, only action:

- **Engine CI is red on `ava1` without the fix branch.** `.github/workflows/engine-ci.yml` runs on
  `ubuntu-24.04` and executes `cargo test -p ava1-ctest -- --test-threads=1`. `takeover_flag.c`
  (in the ctest build since `ac62364`, 2026-10-03 22:17) includes `<sys/sysctl.h>`, absent on
  glibc ≥ 2.32, and `ava1_op.c:143` trips gcc 13 `-Werror=sign-compare`. Both fixed on
  `origin/ava1-fixes-007`. Merge it first; nothing in CI's ctest job can pass until then.
- **The "engine never removes `ava/jobs`/`ava/send`" CUTOVER §2 item looks stale.**
  `Pool::gc_journals` runs `journal::gc_except` over both with a live-job exclusion
  (`pool.rs:370-388`), called from `ava1_api.rs:44` and `pool.rs:815/826`. Verify the cadence
  and tick the item (or narrow it to the `.ava-part`-after-failed-download case, which is real).

Suggested order: merge the fix branch → 1 (auth) → 2 (ASan in gate) → 5 (small) → 3 (nightly,
needs a console) → 4 → 7 → 6.
