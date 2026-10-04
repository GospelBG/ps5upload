# AVA1 cutover checklist (documentation and tooling)

The `ava1` branch's user-facing docs already describe AVA1. These mentions of
FTX2 or ports 9113/9114 name tooling or settings that still exist on the branch
and change when that code does. The cutover release (project 3) ships only when
every box is ticked and `git grep -i ftx2 -- '*.md' ':!CHANGELOG.md'` is empty.

The CHANGELOG keeps its FTX2 entries: they describe releases that shipped FTX2.

- [ ] `README.md` "Test" section — "in-process mock FTX2 server" → the AVA1 mock/host-C tests
- [ ] `CONTRIBUTING.md:45` — "mock-FTX2 integration tests"
- [ ] `engine/README.md` — `ps5upload-tests` row ("mock FTX2 server"); dev commands using `:9113` / `:9114`
- [ ] `TESTING.md` — `PS5_ADDR=…:9113`, `make validate` waiting for `:9113`, curl examples with `:9114`
- [ ] `tests/README.md` — "full FTX2 stack", `PS5_ADDR` default `:9113`, `--ps5-addr` description
- [ ] `tests/lab/README.md` — `:9113`/`:9114`, `ftx2_control.py`, `ftx2_probe.py`
- [ ] `bench/README.md` — `run-ftx2-upload.mjs`, `check-ftx2-baseline.mjs`, `ftx2-upload-main.json` baselines, `--ps5-addr=…:9113`
- [ ] `FAQ.md` — `FTX2_ZIP_RAM_THRESHOLD_MB` and `FTX2_ARCHIVE_STAGE_MB` environment variables (rename to `PS5UPLOAD_*` and accept the old names for one release)
- [ ] `MGMT_METHODS.md` — every row `hw-verified` (or `n/a` for a retired frame) on both consoles
- [ ] In-app strings (`client/src/i18n/locales/*.ts`) that mention FTX2, ports 9113/9114 or "transfer port"

Release gate: the engine's `auto` mode must not ship before the Task 28 hardware pass.

# Project 2 hand-off: what the FTX2 removal needs

The removal itself is a later, separate change; this list is its prerequisite, not its execution.
Project 2 (the AVA1 data plane, `SPEC.md` §11–§16) ships beside FTX2 and deletes nothing. The boxes
above belong to the project 3 cutover release and stay unticked until it ships. Everything below is
checkable against the tree at the commit that adds this section.

## 1. FTX2 call sites that still exist

Find them again with `git grep -n -i ftx2 -- engine client/src payload`, then
`git grep -n "use_ava1\|route::mode"` for the routing seam.

**Engine handlers with an FTX2 branch** (`engine/crates/ps5upload-engine/src/lib.rs`). Each decides
`use_ava1` and otherwise runs the `ps5upload_core` path; the FTX2 branch is what is deleted.
- Routing and startup: `route::use_ava1` calls at 1972, 2045, 5072, 5378, 5930, 7909, 8260, 8523,
  9062; the startup line at 9616–9631; imports of `transfer::*` at 103–107 and `FrameType` at 70.
- Uploads: `transfer_file_handler` 4880 (FTX2 call 5082), `transfer_dir_handler` 5139 (5388),
  `transfer_zip_handler` 5739 (FTX2 closure 5917, also the fallback for zip entries above 256 MiB),
  `transfer_file_list_handler` 7730 (7920), `transfer_dir_reconcile_handler` 8754 (9080).
- Archives with no AVA1 path at all: `transfer_7z_handler` 7303 (7443), `transfer_rar_handler` 7529
  (7667), the inspect/plan calls at 5662, 6079, 7502, 7551.
- Downloads: `transfer_download_handler` 8175 (the FTX2 enumeration at 8295),
  `transfer_download_zip_handler` 8470 (8543, `download_to_zip_ex` 8612).
- Console file operations: `ps5_fs_move` 1935 (the same-drive rename is still an FTX2 management
  frame; only a cross-mount refusal becomes an AVA1 job), `ps5_fs_copy` 2016 (FTX2 `fs_copy_robust`
  at 2048).
- FTX2 tuning environment: `FTX2_INFLIGHT_SHARDS`, `FTX2_INFLIGHT_BYTES`, `FTX2_PACK_SIZE`,
  `FTX2_PACK_FILE_MAX`, `FTX2_BANDWIDTH_MBPS` (114–150), `FTX2_ZIP_RAM_THRESHOLD_MB` (1395, 5817).
- Routing code: `engine/crates/ps5upload-ava1/src/route.rs` (the whole `Mode` seam),
  and `PS5UPLOAD_TRANSFER` in `engine/crates/ps5upload-lab/src/bench.rs:2206`.

**Engine core** (`engine/crates/ps5upload-core/src`): `transfer.rs` (the FTX2 transfer pipeline, 7z and
RAR streaming), `download.rs`, `connection.rs` (FTX2 framing), `fs_ops.rs` and about twenty management
modules that speak FTX2 frames to :9114 (`hw.rs`, `smp.rs`, `notif.rs`, `users.rs`, `saves.rs`,
`volumes.rs`, `system_control.rs`, `sys_time.rs`, `remoteplay.rs`, `process_mgr.rs`,
`payload_lifecycle.rs`, `fan_curve.rs`, `backup.rs`, and the rest of `grep -l -i ftx2`). These are the
management RPCs below: they cannot go until AVA1 carries them.

**Crates and tests**: `engine/crates/ftx2-proto` (used by core, engine, bench, lab and tests), the mock
server and FTX2 integration tests in `engine/crates/ps5upload-tests/tests/` (`mock_server/mod.rs`,
`transfer_integration.rs`, `transfer_zip_integration.rs`, `transfer_7z_integration.rs`,
`hw_integration.rs`), `engine/crates/ps5upload-bench`, and the `--proto ftx2` arms of
`engine/crates/ps5upload-lab/src/bench.rs` (about 70 references; keep them until the last FTX2
measurement is no longer needed, then delete).

**Client** (`client/src`): `state/connection.ts:49-96` (the :9113 transfer-port probe and its comments),
`lib/addr.ts:20`, `api/ps5.ts:145`, `screens/Upload/index.tsx:1100`, `lib/uploadEta.ts:21`,
`lib/keepAwakeHold.ts:10`, `state/activityWiring.ts:323`, and the strings `About/index.tsx:53` /
`i18n/locales/*.ts` (`en.ts:35`, `en.ts:435`, and the translations of each).

**Payload** (`payload/`): `src/runtime.c` (about 770 references: the frame types from line 103, the
transaction table and its journal files, the spool, the transfer server loop at 16577 and the
management loop at 16876), `src/main.c` (86, 529, 679), `src/takeover.c:129`,
`include/config.h:20-34` (`PS5UPLOAD2_RUNTIME_PORT` 9113, `PS5UPLOAD2_MGMT_PORT` 9114,
`PS5UPLOAD2_TX_DIR`, `PS5UPLOAD2_SPOOL_DIR`), `include/runtime.h:141`, `include/wake_watchdog.h:35`.
The FTX2 journal directories are `/data/ps5upload/tx` (`tx_<id>.json`, `runtime_tx_state.txt`,
`events.log`) and `/data/ps5upload/spool` (`spool_<id>/<shard>`), created at `runtime.c:1398-1401`;
the cutover payload removes both on first start. AVA1's own state is `/data/ps5upload/ava` and is
kept.

## 2. Checklist

- [ ] 7z and RAR uploads still run on FTX2. They need sequential AVA1 sources (the decoders are
      forward-only, so the random-access `Source` of `SPEC.md` §10 does not fit) before FTX2 can be
      deleted.
- [ ] Zip entries above 256 MiB (`ZIP_MAX_ENTRY`, `ps5upload-ava1/src/upload.rs:36`) fall back to
      FTX2 (`ZipTooLarge`); they need a streaming entry reader.
- [ ] Management RPCs: every :9114 FTX2 frame the engine core sends (list above) needs an AVA1
      method. `SPEC.md` §7.1 defines only 1–3 and 16–19. This is project 3's main work.
- [ ] Task 9 leftovers: `net.speedtest` now measures round trips on the shared AVA1 session (gate and
      pool included), so its numbers are not comparable with the FTX2 one-connection figures; the AVA1
      event log has no line for a `job.copy` ending (only upload/download receivers and peer-ended
      senders log); a clamped `log.syslog` tail is a note plus the newest 256 KiB, the older kernel text
      is not reachable (FTX2 sent up to 1 MiB).
- [x] A failed multi-chunk `fs.write` (`ps5upload-ava1/src/mgmt.rs`, `write_chunks`) removes its `<path>.ps5upload.tmp` best-effort with a
  `job.run` DELETE (only after a chunk was accepted); if that fails too, the next write of the path truncates it (offset 0).
- [ ] `ps5_fs_move`'s same-drive rename moves to an AVA1 RPC with the `st_dev` guard (never an
      unguarded `rename()` across mounts: that panics the console's kernel).
- [x] NAS sources: `SourceFs` now has an `mtime` (SMB, FTP and SFTP report one; a backend that does
      not reports unknown), carried into the manifest. `upload::apply_existing_policy` picks
      `skip-existing` when every file has an mtime and `verify` (roots in the manifest) otherwise
      (`SPEC.md` §11.4). The engine's Resume strategy (`/api/transfer/dir-reconcile`, mode `fast`/`safe`) on an AVA1 console now runs
      the folder upload with that choice (`upload_dir_skip_existing`; `fast` = size+mtime or the verify fallback,
      `safe` = always verify), local or remote source; tests: `ava1-ctest/tests/nas_skip.rs`.
- [x] One session per identity (`SPEC.md` §8): two engines sharing an identity file (a copied
      `<data dir>/ava/identity`, a shared data directory, a Docker engine mounted on the
      desktop's directory) evict each other's console session. Give each engine its own data
      directory. The engine warns ("another ps5upload engine using the same identity is
      connected to this console") when a session is superseded 3 times in 120 s.
- [ ] Zip downloads restart the archive from zero on a reconnect (a fresh job per attempt; the
      reported progress stays monotonic). FTX2 resumes mid-entry, so this is a measured regression:
      implement an in-run resume, or accept it explicitly in the release notes.
- [ ] A failed download into an existing folder leaves its per-file `.ava-part` behind; cleanup is
      only done for new destinations.
- [ ] The engine never removes `<data dir>/ava/jobs/*` or `<data dir>/ava/send/*` (`SPEC.md` §14.3
      says a node removes a job directory 7 days after its last write; only the console does so, in
      `payload/src/ava1_glue.c:192`). `ava1::journal::gc` exists and has no caller in the engine.
- [ ] The Upload screen shows the transfer's `bottleneck` (the engine already carries it in
      `commit_ack` and `Progress`; no file in `client/src` reads it).
- [ ] PS5 → PS5 UI wiring (the relay and `/api/transfer/ps5-to-ps5` exist and the relay is tested; the
      screen does not offer it).
- [ ] `PS5UPLOAD_TRANSFER`: the default today is `auto` (probe the console, use AVA1 when it
      advertises `CAP_DATA_PLANE`, FTX2 otherwise). At the cutover `route.rs` and the variable are
      deleted along with the FTX2 branch of every call site above. To verify which protocol a run used,
      read the engine's startup line `transfer mode=<auto|ava1|ftx2> (<default|env PS5UPLOAD_TRANSFER=…>)
      ava_dir=…` (`ps5upload-engine/src/lib.rs:9631`) and each transfer's `protocol=` line; the
      benchmark harness refuses an ambiguous `PS5UPLOAD_TRANSFER`.
- [ ] The payload's FTX2 journal directories (`/data/ps5upload/tx`, `/data/ps5upload/spool`) are
      removed by the cutover payload on first start.
- [ ] Engine tests that stub or assert FTX2 (list in section 1) are replaced by their AVA1
      equivalents, and `git grep -n -i ftx2` over engine, client and payload is empty except for the
      CHANGELOG.

## 3. Release gates

- [ ] **Hardware pass on both consoles at the release commit.** The numbers below are from a mix of
      commits: the Pro's part 2 ran at `a0afe3d1`, the Phat's part 1 at `4876e512` (before the
      download fix `b1be8059`), and the Phat never ran part 2. Re-run the whole table on both, on all
      three drives each (Pro: `/data`, `/mnt/usb0`, `/mnt/ext1`; Phat: `/data`, `/mnt/usb0`,
      `/mnt/ext0`).
- [ ] **The engine's `auto` mode must not ship before that pass is green.** Until then the default
      stays FTX2 for any release that goes to users.
- [ ] **The Pro outage of 2026-10-03 is investigated.** At about 09:10 the Pro stopped answering ping and
      every port shortly after an instrumented helper (extra stderr timing lines only) was sent;
      the last keep-awake acknowledgement was 09:10:50. Cause unknown: the console may have gone to
      rest mode, or an AVA1 helper path may have panicked the kernel (see the cross-device rename
      and ShellUI ptrace incidents). After the console is powered on, read `/data/ps5upload/stderr.log`
      and the console's own crash notice, then repeat the instrumented run. No release until this has
      an explanation.
- [ ] **Opus re-review** (the subagents' Opus weekly limit resets 2026-10-07 11:00 PT; reviews since the
      limit was hit ran on Sonnet). Second pass on: crypto and key handling
      (`payload/ava1/ava1_aead.c`, `ava1_chacha_avx2.c`, `ava1_noise.c`, `ava1_keys.c`, the engine's
      `handshake.rs`, `keys.rs`, `launch.rs`); console code that touches the filesystem and the
      kernel (`ava1_apply.c`, `ava1_recv.c`, `ava1_data.c`, `ava1_copy.c`, `ava1_send.c`, and the
      open-file budget commits `1fdc3849` and `980d96c2`); the Codex-session commits `18d18e2c`,
      `1d1541c9`, `cca7b988`, `543205b5`, `9a3d934e`, `3d9b6344`, `966eb8af`; and the whole-branch
      diff.
- [ ] Code items the SPEC states and the code does not yet do (see the Task 29 report): the console
      receiver's `ERR_CREDIT` handling ends the lane, not the session (`ava1_data.c:1288`);
      `Resume` does not re-send credit and the engine's host ignores `Resume`;
      the engine's `LocalSink` re-hashes more than the console does on resume (allowed, see §13.4).
- [ ] The workspace gate is green on the release commit: `cargo fmt --check`, `cargo clippy --workspace
      --all-targets -- -D warnings`, `cargo test --workspace`, `cargo test -p ava1-ctest -- --test-threads=1`,
      `cargo check --locked`, the client lint and vitest, `make ava1-fuzz-c`.
      Two Docker-specific engine wording tests are excluded on Linux today.

## 4. Measured results (Task 28, 2026-10-03)

Medians of warm runs, every run verified. "AVA1" is the committed code at the commit named; "FTX2" is
the same corpus through the existing path. Corpora: **tiny** = 2,000 files of 1–64 KiB (64.5 MiB);
**large** = one 4 GiB file; **ppsa** = 223,000 files (the PPSA01342-shaped mix). The Pro is
192.168.86.100, the Phat 192.168.86.99, both firmware 13.60. Ranges are min–max across runs.

Pro, part 2 at `a0afe3d1` (download and copy) and part 1 at `4876e512` (uploads, resume):

| scenario | /data AVA1 | /data FTX2 | usb0 AVA1 | usb0 FTX2 | ext1 AVA1 | ext1 FTX2 |
|----------|-----------|-----------|-----------|-----------|-----------|-----------|
| 4 GiB upload (MB/s) | 91–104 | 106–110 | 97–108 | 110–113 | 88–105 | 105–108 |
| 2,000 tiny upload (files/s) | 291 | 320–343 | 533–539 | 393–396 | ~380 | ~543 |
| 2,000 tiny download (files/s) | 1,160 | 2,588 | 1,905 | 2,177 | 1,979 | 2,565 |
| console copy, 2,000 tiny (files/s) | 296 | 166 | 463 | 332 | 381 | 332 |
| resume 4 GiB after a helper kill (MB/s) | 90–106 | 66–67 | 99–106 | 57–59 | 88–97 | 58–61 |
| 4 GiB with the link cut every 10 s (MB/s) | 79.7 | not run | 80.6 | not run | 71.0 | not run |

- Download before the fix `b1be8059` was 140–297 files/s on `/data` (about 10x slower than FTX2). After
  it, an interleaved AVA1/FTX2 pairing of the same corpus gave 1,605/1,979, 1,829/2,435, 1,869/2,012 and
  1,606/1,620 files/s: AVA1 is 75–100% of FTX2 on the Pro, and one run to run drift of about 25%
  moved both protocols together. The 1,160 vs 2,588 line above did not reproduce.
- Tiny upload is bounded by the console's file-create rate, not by the protocol: `disk.calibrate`
  (4 KiB files at 1/2/4/8/16 workers) measured `/data` 182–188 / 250–270 / 279–285 / 284–295 /
  287–298 files/s, usb0 502 / 575 / 573 / 573 / 574, ext1 282 / 378 / 404 / 405 / 407. FTX2 reaches
  287 files/s on `/data` already. AVA1 pays for its durability (fsync per batch): -13% on
  `/data` and -30% on ext1, +36% on usb0.
- Large files: AVA1 is 5–8% below FTX2 (encryption and lane overhead); investigate before the release.
- Console copy (on the console, no network): AVA1 is +78% on `/data`, +40% on usb0, +15% on ext1.
- 223,000-file upload to `/data`: AVA1 completed and verified it at 82.5 files/s (2,702 s); FTX2 failed
  after 611 s on all four streams (`read frame header: Resource temporarily unavailable`) and could
  not clean its partial tree (the packed-shard file-count cliff). At game scale AVA1 is the only one
  that finishes. The 82.5 files/s is 3.5x below the drive's measured ceiling and did not reproduce
  on loopback (flat 3–6k files/s); the corpus's 20,075 directories and 1,641 files above 256 KiB are
  the likely cause. The helper now prints a per-job stats line every 10 s to find it.
- Real games over AVA1 to `/data` (cold, single run, verified): Minecraft PPSA17221, 35,260 files /
  1.33 GB in 246.7 s (142.9 files/s); Worms PPSA20052, 10,230 files / 2.64 GB in 62.2 s (42.4 MB/s);
  Minecraft Legends PPSA05510, 489 files / 7.64 GB in 79.4 s (96.3 MB/s). FTX2 runs of the same
  games were still in progress when the Pro went down and have no recorded result.

Phat, part 1 at `4876e512` (the download rows predate `b1be8059`; the Phat never ran part 2):

| scenario | /data AVA1 | /data FTX2 | usb0 AVA1 | usb0 FTX2 | ext0 AVA1 | ext0 FTX2 |
|----------|-----------|-----------|-----------|-----------|-----------|-----------|
| 4 GiB upload (MB/s) | 104–111 | 110–111 | 9–15 | 34–35 | 95–104 | 107–108 |
| 2,000 tiny upload (files/s) | 236–243 | 268–277 | one run, 27 | none completed | 600–671 | 426–442 |
| 2,000 tiny download (files/s) | 119–201 | 2,047–2,501 | 124–149 | 426–2,696 | 111–328 | none recorded |
| resume 4 GiB (MB/s) | 99–109 | none completed | 15 (one run) | none completed | 93–99 | not run |

The Phat's usb0 numbers are far below the Pro's for both protocols and its AVA1 large-file upload is a
third of FTX2's there; that is unexplained and is a reason to re-run the Phat before the release.
FTX2 resume failed in every Phat run (the harness did not wait for the helper's ports after a
restart; fixed in `a67ea278`, re-run pending on the Phat).
