# AVA1 review: `ava1` e64002f → f660dcc (2026-10-03)

Twenty-five commits, about 15k lines. Reviewed against `docs-research/001-ava1-e64002f/02-design-review-round2.md`
(the round-2 design review) and `docs-research/001-ava1-e64002f/03-archive-sources.md` (the Task 11 research).
Everything below was read in full: `ava1/src/seq.rs`, `ps5upload-ava1/src/{seq,rar_source,mgmt,
upload}.rs`, the `send.rs`/`recv.rs`/`server.rs`/`session.rs`/`pool.rs` diffs, `core/mgmt.rs`,
the `transfer.rs` RAR walk, `payload/src/mgmt_rpc.c` + `mgmt_install.inc` + `mgmt_table.def`, the
`ava1_send.c`/`ava1_server.c`/`ava1_manifest.c`/`runtime.c` diffs, the vendored unrar change,
SPEC §5/§7.3–7.5/§12.7/§17, CUTOVER, `MGMT_METHODS.md`, and the new test files' shapes.

What landed: sequential sources and 7z/RAR uploads over AVA1 (Task 11 + fixes), skip-existing
wired into the Resume strategy, the download pipeline tune, the Task 1 RPC limits (8 in flight,
56 KiB request, 256 KiB reply, chunked `fs.write`), and the P3 management transport (engine seam,
AVA1 implementation, console dispatcher with six methods ported).

## 1. Verdict

The archive work is sound and matches the research closely; in two places it is better than
what the research proposed (§3). The download tune and the RPC-limit work are correct. The
management transport's design (capture sink, handlers unchanged behind `client_fd = -1`,
`MGMT_LONG` refusal, 512 KiB worker stack with a stack audit) is the right shape, and FTX2
already ran one detached thread per management connection, so eight concurrent AVA1 workers add
no reentrancy exposure the handlers did not already have.

Five things need fixing before the P3 tasks that build on them (§2, M1–M5); the two release
blockers from the round-2 review (pairing commit-reveal, trust store under `/data`) are still
open and are not touched by this batch.

## 2. Findings

### M1 (medium): the engine routes management by probing, not by `CAP_MGMT`

SPEC §5 now says "a client routes management calls by this bit instead of probing for
`ERR_UNKNOWN_METHOD`", and `Session::has_mgmt()` exists — but nothing in `ps5upload-ava1` or the
engine calls it (`git grep has_mgmt` finds only its definition). `AvaTransport::serves`
(`ps5upload-ava1/src/mgmt.rs`) goes through `route::use_ava1_in`, which checks `CAP_DATA_PLANE`
only, then sends the RPC and falls back to FTX2 on `ERR_UNKNOWN_METHOD` with a 30 s negative
cache (`NO_MGMT_TTL`).

Consequences: a console with the data plane but no management table is re-probed every 30 s on
every management call; and when Task 4+ adds a method to the engine before the payload carries
it, the mismatch is silently masked as an FTX2 fallback instead of surfacing as version skew.

Fix: in `AvaTransport::rpc`, after `pool.session(console)`, return `Ok(None)` when
`!session.has_mgmt()`; delete `no_mgmt`/`mark_no_mgmt`; keep the `ERR_UNKNOWN_METHOD` arm as a
hard error (the console advertised the capability and still does not know the method). Test:
`ava1-ctest` with `mgmt::uninstall()` → the transport answers `None` and the C server records
zero RPCs; with the table installed → one RPC and no probe.

### M2 (medium): the RAR listing-order check refuses archives it could upload

`rar_source.rs` `Adapter::visit` compares every entry's ordinal with the listing order on every
pass, including a fresh upload (`Restart::START`) and every solid-archive pass (solid always
restarts at START). The decode thread binds entries by path (`seq.rs` `Feeder::begin`), so a
listing/extraction disagreement is harmless whenever nothing is being skipped *by ordinal*. FTX2
tolerates it on a fresh upload for exactly that reason (`bind_plan_entry`, "reordered … harmless
for a fresh upload"). Here it is a terminal `ava1_rar_reordered` (the client lists `ava1_rar_` as
fatal) with no FTX2 fallback, so such an archive can never be uploaded over AVA1.

Fix: enforce the order only when `start > 0` on a non-solid archive (the one case where an
ordinal decides what is skipped); otherwise accept. Or, on a mismatch with `start > 0`, retry
the pass once from `Restart::START`. Test: the existing reordered fixture uploads completely with
`Restart(0)` and fails typed only with `Restart(3)`.

### M3 (medium): `mgmt_legacy_failure` only recognises `{"ok":false` as the first key

`payload/src/mgmt_rpc.c:149-165` matches `"ok"` immediately after `{`. A handler that emits
`{"err":"x","ok":false}` or `{"detail":{…},"ok":false}` passes through as `STATUS_OK` with a
failure body — the exact "porting bug" SPEC §7.3 names. The six ported handlers are fine today;
Tasks 4–9 port the `{"ok":false}` family (`fs_write_bytes`, `net_reach`, `toast_send`, TMDB, SDK,
cheats). Make the scan key-order independent (a depth-1 `"ok"` followed by `false` anywhere) and
pin it in the ctest token tests. Related, low: `mgmt_legacy_call` hands the request to the handler
as a C string, so a body with an embedded NUL is silently truncated; refuse it with
`ERR_PROTOCOL`.

### M4 (low-medium): 7z `!reached_last` is reported as `UnsupportedLayout`

`ps5upload-ava1/src/seq.rs` `pass`: when the block decoder yields fewer streamed entries than the
header lists for that folder, the source returns `UnsupportedLayout`, whose message tells the user
to re-pack with 7-Zip. Interleaved stream-less entries were already refused at `open`, so this path
means a damaged archive or a crate behaviour change: report `Corrupt` ("the folder yielded N of M
entries"), which is also what the FTX2 path would hit.

### M5 (low): the 7z `Unsupported` → FTX2 fallback covers cases FTX2 handles no better

`upload_7z_in` falls back to FTX2 (`SevenzUnsupported`) for every `SevenzFault::Unsupported`:
unsupported coder methods (right), but also a duplicate entry name, a path that is both a file and
a directory, and `MaxMemLimited`. FTX2's 7z plan does not refuse duplicates (both are written, the
last wins silently) and hits the same memory limit. Make duplicates and file/dir conflicts terminal
(`ava1_7z_unsupported`, as the RAR path does for `RarUnsupported`… which also falls back; the same
argument applies there: FTX2 RAR writes both case-variants) and keep the fallback for coder
methods only.

### Low / notes

- **L1** Entry timestamps and modes are dropped from archive manifests (`mtime: 0`, `0644`/`0755`).
  Content-only identity is right; losing the timestamps is a user-visible difference from
  extracting locally. Say so in SPEC §17 (or carry the entry's own `last_modified_date`, which is
  content, not container metadata — the research doc §2.5).
- **L2** `ava1_send.c` `role_free` prints the stage-timer line unconditionally on every download
  (console stderr → klog). Gate it like the Rust side's `PS5UPLOAD_AVA1_TIMING`.
- **L3** SPEC §17.4 says the bottleneck is `BN_SOURCE` while the decode thread is what the lanes
  wait on; the sender reports only the receiver's `Done.bottleneck` (`send.rs:1500`) and I found
  no sender-side attribution. Implement (the decode thread's `acquire` wait is the signal) or
  soften the SPEC sentence.
- **L4** `AvaTransport::rpc` wraps the gate wait inside the 30 s timeout: six slow calls make the
  seventh "time out" instead of queue. Acceptable; make the message say "waiting behind N calls".
- **L5** `mgmt_status_for_token` is substring-based ("already_running" → BUSY before "already" →
  EXISTS; "invalid_path" → PATH). Fine, but the ctest token table should pin the ambiguous pairs.
- **L6** `mgmt_capture_frame`: a success frame after an error frame is ignored (good); two success
  frames keep the last (fine for the six handlers; a progress-then-result handler would need
  `MGMT_LONG`).
- **L7** `measure_unknown_sizes` decodes a solid RAR up to its last unknown-size entry at plan
  time (before `JobOpen`). Rare (streaming archivers only); worth a log line so a long "planning"
  phase is explainable.

### Still open from the round-2 review (not touched here)

- **S1** pairing code grindable by an active man-in-the-middle → commit-then-reveal (release gate).
- **S2** trust store `/data/ps5upload/ava/{identity,peers}` writable through FTX2 :9114 and FTP.
  With `fs.write` now also served over AVA1, a *paired* peer can overwrite `peers` too; the carve-out
  in `may_write`/`is_path_lexically_allowed`/the FTP view is still the fix.
- **P1** `posix_fallocate` under the job mutex (large-file gap experiment), **P5** BLAKE3 C at -O0.

## 3. What matches the research, and what is better

| research item | landed as |
|---|---|
| bind by name, seen-set, exact size, duplicate refusal | `seq.rs` `Feeder::begin/data/end`: path→id map, `done` set ("appears twice"), `pos != size` → "changed while it was being sent", `one_pass` → "not found in the archive"; `SevenzSource::open` refuses duplicates and file/dir conflicts |
| archive identity from content, not mtime | `identity = BLAKE3(size ‖ start header ‖ next header)`, `wire job id = BLAKE3(tx_id ‖ identity)[..16]`; `a_touched_copy_of_the_archive_keeps_its_resume`, `changed_archive_restarts_not_splices` |
| plan-time guard for stream-less entries inside a block | `SevenzSource::open` → `UnsupportedLayout`; FTX2 refuses the same via `sevenz_check_layout`; crafted-archive tests on both paths |
| solid = ordered single reader, non-solid = per-block | one decode thread for both; `restart_for` = folder index; a folder with nothing wanted is never opened (non-solid resume O(1)); decoding stops after the last wanted file of a folder |
| memory bound = dictionary + read-ahead, threads = 1 | decode thread takes the shared `bytes_budget` permits; `sevenz_decode_threads()` default 1; `sevenz_rss_stays_bounded_at_one_thread` (`#[ignore]`, 400 MiB) |
| RAR password errors terminal, never a reconnect | `RarWalkError::Failed` → `RarFailure` in an `io::Error` → `SendError::Source` (`#[from]`, chain kept) → `upload_with_in` returns at once; client lists `ava1_rar_*` fatal |
| cancel reaches a solid skip | vendored `read_to_fn` + `-1` from `UCM_PROCESSDATA` aborts UnRAR mid-entry; `rar_cancel_ends_a_long_excluded_solid_skip_promptly` |
| test against the real console receiver | `ava1-ctest/tests/sevenz.rs` `sevenz_upload_to_a_c_server_verifies_every_file` |

Better than proposed:

- The manifest stays path-sorted (§11.3) and the decode thread maps decode-order entries to
  manifest ids; the research suggested a decode-order manifest for solid archives, which would
  have cost the receiver's sorted invariant and the `Resume` fast path.
- Small archive members still travel as `Record`s in bundles (the research proposed `cutoff = 0`,
  one `Chunk`+`FileRoot` per file); the `pieces` plan and `Keep::Ranges` reuse the large-reader
  logic exactly.
- RAR decodes on the decode thread through a callback sink instead of a worker + bounded
  channel, so there is no second thread to join and cancel is one callback away.

## 4. Download pipeline (ea3f68e) and the C sender

- Sending the empty map before the journal is created claims nothing durable early: `JobDone`
  and `Durable` still follow the sync; data that arrives meanwhile waits in the inbox under the
  credit window. Sound.
- Bundle writes on their own blocking tasks (`WRITE_PAR = 4`) with credit returned on write is the
  right backpressure; `finished` waits for `writes.is_empty()`; a bad root goes back as
  `FileRetry` from the loop; `write_bundle` is unit-tested.
- `sync_due(all_in)` removes the 250 ms tail. Fine.
- C sender: `kick()` on the map's arrival starts the reader/writers under `pump_mu` with
  `trylock`, so the connection thread never blocks on a job; lock order `pump_mu → j->mu` is the
  same in `on_tick`. `ava1_mstore_walk_ex` now stats once per entry (`lstat`, `stat` only for a
  symlink) and sorts `frec_t` records whose first member is the path, which is valid C for the
  existing `path_cmp`.

## 5. Management transport (P3 Tasks 1–3)

- Dispatcher: thread-local capture sink; the first error frame wins; a body that does not fit is
  `ERR_INTERNAL "reply truncated"`, never a clipped OK; `MGMT_LONG` refuses handlers that answer
  from another thread (the sink could not see them). Correct.
- Concurrency: FTX2 spawned `mgmt_client_thread` per connection (`runtime.c:16942-17052`), so the
  handlers already ran concurrently; eight RPC workers with a 6 + 2 engine gate is comparable.
- Limits: 8 × 256 KiB reply buffers per session, 16 sessions → 32 MiB worst case on the console.
  Acceptable; documented in SPEC §7.4.
- `fs.write` chunking leaves `<path>.ps5upload.tmp` on failure until Task 5 — tracked in CUTOVER.
- Pool: single-flight connect per console closes a real race (two concurrent first calls evicted
  each other's session under §8).

## 6. Order of work

1. M1 (`has_mgmt` routing) — before Task 4 adds methods.
2. M3 (legacy-failure parse) — before Tasks 4–9 port the `{"ok":false}` handlers.
3. M2 (RAR reorder) — small, unblocks real archives.
4. M4, M5, L1–L3.
5. S1, S2, then P1/P5 from the round-2 review.
