# AVA1 research and reviews

One directory per review session, numbered in order and named after the `ava1` commit the
session reviewed. Files inside are numbered in reading order. Written on the `ava1-design`
branch by the cloud research session; the local implementation sessions read them and act.

| dir | ava1 head reviewed | contents |
|---|---|---|
| `001-ava1-e64002f/` | e64002f | round-1 design review (Sonnet), round-2 design review (Fable 5.1), 7z/RAR archive-source research for Task 11 |
| `002-ava1-f660dcc/` | f660dcc | review of e64002f..f660dcc: sequential sources, 7z/RAR uploads, skip-existing, download tune, RPC limits, P3 management transport |
| `003-ava1-f660dcc/` | f660dcc | 01 whole-system review against FTX2 and the state of the art; designs: 02 durable-by-log small files, 03 lane receive path and governor, 04 stored zip downloads with resume, 05 group-level delta (v1.1); 06 consolidated SPEC/schema change list in apply order |
| `004-ava1-da80f8f/` | da80f8f | review of f660dcc..da80f8f: S1 commit-then-reveal closed, review-002 findings (M1 still open), job.run/job.list and 83 management methods, the delete operation's symlink guard, SPEC/code mismatches on operation cancel and release |
| `005-ava1-fc2640f/` | fc2640f | review of da80f8f..fc2640f: M1 and S2 closed, lane-path and governor changes from 003 landed with tests, stored zip resume (design 04) landed, Task 4 filesystem methods; one small fix closes the zip restart window |
| `006-ava1-fc2640f/` | fc2640f | deep re-review + liveness/hang audit + 20-category evaluation checklist + impl guides for the three ready fixes (nonce audit, progress watchdog, zip finish window) |
| `007-ava1-e2208e6/` | e2208e6 | pre-hardware deep review of 66 commits (durable-by-log on both ends verified incl. crash recovery; JobOpen-after-cancel hang closed); hardware go/no-go gates and Pro-outage triage; findings HW-0..HW-4 + conformance C-1; impl guides for same_device fail-closed and a runtime durable-by-log off-switch |
| `008-ava1-282fa0b/` | 282fa0b | review of e2208e6..282fa0b: all 006 items landed and verified (nonce audit PASS with ceilings, progress watchdog on both ends, admission bound, vectors, fs.stat decision); fix branch ava1-fixes-007 merged onto the head and re-tested green |
| `009-ava1-282fa0b/` | 282fa0b | designs for what is still lacking: API auth, sanitizers/torn-write/soak test depth, nightly hardware-in-the-loop, per-job telemetry, download commit parity, conformance suite, group-delta integration after durable-by-log; notes that engine CI needs ava1-fixes-007 to build ctest on ubuntu-24.04 |

Each new session adds `NNN-ava1-<short sha>/` with its own numbered notes and a row here.
