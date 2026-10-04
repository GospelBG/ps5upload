# AVA1 review: `ava1` da80f8f → fc2640f (2026-10-04)

Twenty-three commits, about 6.8k lines. The local session took up session 003's designs and
the 004 findings: M1 (CAP_MGMT routing), S2 (trust store denied everywhere, symlink-safe
policy), socket buffers, the frame-buffer pool, one copy per sent frame, the lanes-first
governor with benchmark pins and the per-job bottleneck line, `dead_after` 12 s, stored zip
downloads with mid-entry resume (design 04), and Task 4's filesystem and node methods. Read in
full: `path_policy.c/.h`, the glue and `cross_device.h` changes, `ava1_frame.c`, the data-path
diffs, `route.rs`/`mgmt.rs`, `governor.rs`, `conn.rs`/`keys.rs`/`session.rs`, the `recv.rs`
`position` hook, `zip_stored.rs`, the `mgmt_fs.c` guards, and the SPEC/CUTOVER diffs.

## 1. Verdict

Everything taken from 003/004 was implemented as designed or better, and each change carries
its own tests. Both release-gate security items are now closed on the protocol and policy
side (S1 in the previous batch, S2 here). The remaining work is measurement (CUTOVER §4.1's
matrix, which the engine now reports on its own) and the two designs not yet started
(durable-by-log, group delta). One small fix closes the one CUTOVER regression the zip work
left open (§5).

## 2. Closed

| item | how |
|---|---|
| M1 | `route_in(pool, console, cap)`: management routes on `CAP_MGMT` from `ServerInfo`, nothing probed, no negative cache for a node that answered without the cap. Forced `Ava1` mode documented to skip the check. The one kept exception (a `CAP_MGMT` helper that predates `job.run` → FTX2 for ops only) is correct and bounded. |
| S2 | `path_policy.c`: the store is denied as written, lexically normalised and canonical (symlinks resolved, deepest existing ancestor for a path that does not exist yet, dangling links refused, `..` fails closed, case-insensitive compare); ancestors are refused for every tree operation (copy, move, delete, chmod, job roots); FTP RNFR/RNTO/DELE/RMD refuse it with a host self-test; the data plane's walk skips links into the store and opens sources `O_NOFOLLOW` with a canonical re-check; the cross-device guard judges a rename source by `lstat`. This is the design from 001/02 §2 S2 plus the S4 class of 004 §5. |
| 004 S4-op | covered by `path_resolve_allowed` (the symlinked-parent case is exactly the one its header comment names). |
| 003 §1 | 4 MiB `SO_RCVBUF`/`SO_SNDBUF` on both ends, effective sizes logged once. |
| 003 §3 | `ava1_frame.c`: 1/4/8/15/16 MiB classes with 4 KiB slack, exact allocation for every other size (so live memory equals the credit window's count), idle pool capped at the admit budget, double-free refused, counters asserted after uploads. The review fix (`829115b`) that kept 2 MiB chunks out of the 4 MiB class was the right call. |
| 003 §5 (01) | `FrameBody::Shared` + `seal_slice`: one buffer per sent frame, wire bytes unchanged, tested against `seal`. |
| 003 §5 (governor) | gain 1.05 below 90% of the best rate, chunk held at 4 MiB until 4 lanes or a failed probe, `lanes_capped` reset on a stall, pins honoured through everything, A/B switch. Twelve unit tests. |
| 003 §2.2 item 3 | `JobSummary`: one stderr line per upload with credit-starved, source-starved and receiver-bound shares, the receiver's last bottleneck, average lanes and chunk. CUTOVER §4.1 has the matrix and the empty table. |
| 06 #9 | `dead_after` 12 s on both ends, pinned by tests. |
| 04 zip resume | `StoredZipSink`: zip64 throughout, layout a pure function of the manifest, `Sink::position` cuts the archive to the journal's state and rebuilds the in-flight CRC, a sink that cannot honour the journal restarts the job, Deflate kept as a non-resuming option. Tests: 5 GiB entry, 70k entries read by bsdtar and unzip, exact-cut resume, bit rot caught. Better than 04 proposed: no sink journal record at all. |
| Task 4 | `mgmt_fs.c`: typed `fs.*` with `O_NOFOLLOW`/`O_EXCL` temp files, `fs.read` looping to `eof`, overwrite-aware `fs.rename` with the lstat device guard, `fs.stat` replacing 1-byte existence reads. 110 of the FTX2 frames now have a route; 27 rows still `todo` in MGMT_METHODS. |

## 3. Notes on what landed (no action needed unless stated)

- `path_policy.c` `lex_normalize` turns an over-long path into `/`, which `path_in_protected`
  does not flag; `path_resolve_allowed` still refuses it (the `len >= sizeof tmp` check) and
  `path_contains_protected` flags it, so every caller fails closed. Fine; a comment saying so
  would stop the next reader worrying.
- `best_rate` in the governor never decays. On a link whose capacity drops mid-job (Wi-Fi
  roam) the easier 1.05 bar then applies for the rest of the job, which only means more lanes
  are kept. Acceptable; a slow decay (1% per tick) would make the two bars symmetric.
- `is_read_only` now resends `net.speedtest` after a lost session, which repeats a
  multi-second measurement. Harmless; the engine could exclude it.
- `fs.stat` answers for any absolute `..`-free path (the `fs.list` policy): a paired peer
  learns the existence and size of system files. FTX2 allowed the same through unsafe reads;
  acceptable, and now behind pairing.
- The decrypt-off-reader step (03 §2) correctly waits for the matrix; nothing to do until the
  consoles are back.

## 4. Order of work

1. The §5 fix (small) and the CUTOVER row it closes.
2. Run CUTOVER §4.1's matrix on both consoles and fill the table; decide on 03 §2 from it.
3. Durable-by-log (03/02): the remaining large design; the tiny-file rows cannot move
   without it.
4. Task 4's remaining 27 methods; hardware verification of MGMT_METHODS rows.
5. Group delta (03/05) after the cutover.

## 5. One fix: the "whole but unfinished" zip restart window

CUTOVER §2 records that a drop after a file's last range is journaled but before its Done
record makes `StoredZipSink::position` refuse ("file N is whole but not finished") and the
receiver starts the archive over. It is one batch wide per file boundary, and it is fixable
inside the sink:

- In `position`, accept `x == size` for the in-flight slot: treat it as in flight with
  `written = size`, rebuild its CRC from all its bytes, cut at `data_off + size` (the descriptor
  was never written). Keep `x > size` as an error.
- In `commit(id)` (today a no-op), when `current` is that slot and `written == size`, write the
  descriptor with the rebuilt CRC, record `crcs[k]`, clear `current`. The receiver already calls
  `commit` once the root is known, and the sender re-sends `FileRoot` for a file whose every
  group is durable (`seq.rs`/`send.rs`: "a file whose every CV is known needs no decoding"), so
  no new frame is needed.
- `append` needs no change: for that slot an offset-0 write is already the restart error and
  any other write is past the size.

Test: journal a partial covering the whole last file, resume, assert the archive is not
truncated below the file's data, that only a `FileRoot` arrives, and that `finish` produces a
valid archive. Then the CUTOVER entry can be struck.
