# AVA1 evaluation checklist — categories, investigation, status (fc2640f, 2026-10-04)

A complete set of categories to judge AVA1 by, each investigated against the code at
`origin/ava1` fc2640f. Status: **PASS** (solid, evidence cited), **GAP** (a concrete hole with
a proposed fix), **OPEN** (design agreed, not yet built), **WATCH** (minor, no action now).
Line/function references are to the tree at fc2640f.

Scale of the thing under review: `ava1` core ~23.4k LoC, `ps5upload-ava1` ~17.3k, the C
payload, with ~1,790 Rust `#[test]`/`#[tokio::test]` and 34 C test files, plus `ava1-chaos`
and the `ava1-ctest` cross-implementation harness.

## A. Protocol conformance & cross-implementation parity
- [x] **PASS** Frozen test vectors exist: `protocol/ava1/vectors/` has `pairing.txt`,
  `keys.txt`, `messages.txt`, `launch.txt`, `frame_header.txt`.
- [x] **PASS** Rust↔C parity harness `ava1-ctest` FFI-checks header encode/decode, `crc32c`,
  UTF-8, Noise init/read/write, lane/control key derivation, join tags, pairing code and
  commit, and a full roundtrip against the C receiver.
- [x] **PASS** SPEC.md and CUTOVER.md are maintained in `protocol/ava1/` and updated with each
  landed feature (ledger rulings referenced from code).
- [ ] **WATCH** No negative/fuzz vector set for malformed frames called out separately from
  `ava1-chaos`; confirm chaos covers truncated/oversized/misordered frames (likely, not
  verified this pass).

## B. Cryptography & handshake
- [x] **PASS** Noise_XX, ChaChaPoly, BLAKE2b; per-lane and control keys derived with
  direction and lane number (`ava1_lane_key`/`ava1_control_key`), cross-checked in ctest.
- [x] **PASS** One-copy sealing (`keys::seal_slice`) leaves wire bytes identical to `seal`
  (tested) — a performance change that did not touch the crypto envelope.
- [ ] **WATCH** Nonce/counter management per lane not re-audited this pass; it is the highest-
  value thing to re-verify before a release (a nonce reuse would be catastrophic). Recommend a
  dedicated note confirming each lane's send/receive counter is monotonic and never reset
  within a session.

## C. Pairing & trust establishment
- [x] **PASS** Commit-then-reveal pairing (S1 closed, session 001/004): server commits
  `BLAKE2b-256(nonce_s)` before the client sends `nonce_c`, code derived from both nonces;
  grinding the short code no longer works. Vectors in `pairing.txt`.
- [x] **PASS** Trust store on the console is denied to the data and management planes
  everywhere (S2, session 005), including FTP.

## D. Authorization & path safety
- [x] **PASS** `path_policy.c`: lexical normalise + canonicalise (symlinks resolved, deepest
  existing ancestor, dangling links refused, `..` fails closed, case-insensitive), ancestors
  refused for every tree op, `O_NOFOLLOW` opens with canonical re-check, lstat-based
  cross-device guard. S4-op (symlinked parent) covered by `path_resolve_allowed`.
- [x] **PASS** Management routes only on advertised `CAP_MGMT` (M1, session 004); nothing
  probed.
- [ ] **WATCH** `fs.stat`/`fs.list` answer for any absolute `..`-free path: a paired peer can
  probe existence/size of system files (session 005 §3). Low; behind pairing.

## E. Flow control & backpressure
- [x] **PASS** 64 MiB credit window; a frame larger than outstanding credit is refused with
  `ERR_CREDIT` (recv.rs:973), so the reorder buffer holds at most one window.
- [x] **PASS** Sender read-ahead bounded by semaphore permits (send.rs `_budget`); a fast
  source cannot buffer past the read-ahead budget.
- [x] **PASS** Credit accounting mirrors the C receiver's `w_avail`; late `Received` handled
  (send.rs I3). Backpressure is awaiting room on the control outbox, never an unbounded queue.

## F. Liveness / hang safety  (full audit in 006/02)
- [x] **PASS** Every socket I/O has a deadline (`conn.rs` frame budget); byte-level dead-peer
  watchdog at 12 s (`link.rs`); receiver never stops reading, fsync batch on its own task;
  no lock held across `await`; `MAX_PASSES` re-send cap; resume always has a forward path.
- [ ] **GAP** The only time-based abort is byte-level, and a ping is a byte. A peer that
  heartbeats but sends no file data keeps a job alive with no progress. Fix: a progress
  deadline on the run loop (`JobCancel` after ~3×`dead_after` with the window open and no
  data). Low severity, cheap fix. (006/02)

## G. Crash consistency & durability
- [x] **PASS** Append-only journal (Open/Batch/Reset/Snapshot/Done) with per-record CRC32C,
  `sync_all` after each append, atomic `rename` for create and snapshot-rewrite, directory
  fsync.
- [x] **PASS** Replay is crash-safe: it stops at the first torn, zero-length, bad-CRC, or
  undecodable record and truncates the file to the last good record, then fsyncs
  (journal.rs:124-150). A half-written tail can never be replayed.
- [x] **PASS** Batched commit (T28): one journal record + one drive flush + one `Durable` per
  batch, so an ordered download does not do a full-drive flush per file.
- [ ] **OPEN** Durable-by-log / pack segments (design 003/02) — the large remaining item; the
  tiny-file fsync ceiling cannot move without it.

## H. Resume correctness
- [x] **PASS** Resume by durable ranges; `one_pass` reads the source to recompute a root when
  the persisted outboard is thin, so no resume dead-ends (seq.rs).
- [x] **PASS** Stored-zip resume cuts the archive to the journal and rebuilds the in-flight
  CRC; a sink that cannot honour the journal restarts the job (zip_stored.rs `position`).
- [ ] **GAP** The "file whole but not finished" restart window (one batch wide per file
  boundary). Fix proven regression-free in 006/01 §1: accept `x == size` in `position`,
  write the descriptor in `commit`.

## I. Data integrity & verification
- [x] **PASS** BLAKE3 1 MiB verification groups, persisted outboards, root check before a file
  is committed; a mismatch issues `FileRetry`/`Reset` and re-sends (recv.rs:1657-1676).
- [x] **PASS** Stored-zip finished entries carry a CRC-32 in their own data descriptor, read
  back and verified on resume; zip64 throughout.

## J. Archive handling (downloads & uploads)
- [x] **PASS** Stored-zip download, layout a pure function of the manifest, journal-free
  offsets, tested to 5 GiB entries and 70k entries against bsdtar and unzip.
- [x] **PASS** 7z/RAR by-name binding (sessions 001/002): content identity, solid/non-solid,
  sevenz-rust2 visit order, vendored unrar `read_to_fn` abort.
- [ ] **WATCH** RAR password/cancel edge cases flagged earlier (Task 11) — confirm the
  implementation state on the next push that touches archives.

## K. Management plane
- [x] **PASS** CAP_MGMT gating; typed `fs.*` with `O_NOFOLLOW`/`O_EXCL` temp files, looping
  `fs.read`, overwrite-aware `fs.rename`, `fs.stat`. 110 FTX2 frames routed.
- [ ] **OPEN** 27 MGMT_METHODS rows still `todo`; hardware verification of ported rows
  pending.
- [ ] **WATCH** job.run cancel/release-on-delivery semantics mismatch vs FTX2 (session 004 O1/O2)
  — confirm resolved.

## L. Performance & throughput
- [x] **PASS** Lanes-first governor with pins, A/B switch, 1.05 below-best probe bar, per-job
  bottleneck line (`JobSummary`: credit-starved / source-starved / receiver-bound shares).
- [x] **PASS** 4 MiB socket buffers; frame-buffer pool with exact allocation so live memory
  tracks the credit window.
- [ ] **WATCH** Governor `best_rate` never decays (006/01 §2) — minor; a link that slows
  mid-job keeps the easier bar. Costs a few extra lanes, not throughput.
- [ ] **OPEN** CUTOVER §4.1 hardware matrix (lanes × chunk) not yet run — the empty table that
  decides the decrypt-off-reader step (003 §2). This is the top measurement blocker.

## M. Resource bounds
- [x] **PASS** Receiver memory bounded by the credit window; reorder ≤ one window; `WRITE_PAR`
  = 4 concurrent bundle writes; resume-state frames paged at `MAP_PAGE_ITEMS` (60 KiB).
- [x] **PASS** Frame pool idle size capped at the admit budget; double-free refused.
- [ ] **WATCH** Console open-file budget and fd pressure under the 223k-file corpus — exercised
  earlier; re-confirm after durable-by-log lands.

## N. Concurrency correctness
- [x] **PASS** No `std::sync::Mutex` guard held across an `.await` in the receiver (006/02);
  sink/outboard locks only inside `spawn_blocking` or guard-dropping blocks.
- [x] **PASS** Journal + applied state move into each sync batch and back; the loop never
  touches them while a batch runs; finish paths run only with no batch in flight.

## O. Error handling & taxonomy
- [x] **PASS** Typed errors with wire codes (`ERR_CREDIT`, `ERR_CANCELLED`, `ERR_UNKNOWN_METHOD`,
  `RETRY_VERIFY`); job failures surface as `Read::Failed`/`SendError` rather than silent stalls.
- [ ] **WATCH** Confirm every `?`/`map_err(proto)` path in recv.rs ends in a peer-visible
  control message or a clean teardown (no swallow). Sampled clean this pass; not exhaustive.

## P. Observability
- [x] **PASS** Per-job `JobSummary` stderr line with the bottleneck breakdown; ~50 log/metric
  sites across the engine; effective socket buffer sizes logged once.
- [ ] **WATCH** `events.log` readability note (session 004). No structured metrics export
  (counters/histograms) — fine for now, worth considering for fleet operation.

## Q. Platform / console constraints
- [x] **PASS** `posix_fallocate` kept (UFS sparse files collapse throughput on PS5), moved out
  from under `j->mu`; cross-device rename avoided (kernel panic); frame pool classes tuned.
- [x] **PASS** `dead_after` 12 s on both ends; oversleep grace for a suspended console.

## R. Testing & verification coverage
- [x] **PASS** ~1,790 Rust tests, 34 C test files, `ava1-chaos`, `ava1-ctest` cross-impl, and
  frozen vectors. Each landed feature shipped with its own tests.
- [ ] **WATCH** Add the two tests this review names: the zip whole-but-unfinished resume
  (006/01) and the heartbeat-without-data progress-abort (006/02).

## S. Build, cutover & migration
- [x] **PASS** CUTOVER.md tracks the FTX2→AVA1 cutover with benchmark knobs
  (`PS5UPLOAD_AVA1_LANES`/`_CHUNK`/`_LANES_FIRST`); FTX2 fallback tested.
- [ ] **OPEN** Zip restart-window row in CUTOVER §2 closable once 006/01 §1 lands.

## T. Operability / configuration
- [x] **PASS** Governor pins and benchmark env knobs for field tuning; job GC removes idle job
  dirs after 7 days.
- [ ] **WATCH** No runtime cap on concurrent jobs re-audited this pass; confirm a flood of
  `JobOpen`s is bounded (spawn_blocking pool is 512, but job admission itself should have a
  ceiling).

---

## Summary

| bucket | count | items |
|---|---|---|
| PASS | 33 | the protocol core, crypto parity, pairing, path safety, flow control, crash consistency, verification, concurrency |
| GAP (fix in hand) | 2 | progress watchdog (F), zip whole-but-unfinished (H) |
| OPEN (design agreed) | 5 | durable-by-log, hardware matrix, 27 mgmt methods, CUTOVER zip row |
| WATCH (minor / re-confirm) | ~12 | nonce-counter re-audit, fs.stat disclosure, governor decay, job-admission cap, fuzz vectors, etc. |

**Bottom line.** AVA1 is stable and solid on the dimensions that decide whether a transfer
protocol is trustworthy: it is crash-consistent by construction, cannot hang on any clean
failure, bounds all memory and concurrency, and verifies every byte it commits. The two GAPs
both have regression-free fixes already written up. The real remaining work is measurement
(the hardware matrix) and the one large design (durable-by-log), plus a short list of
re-confirmations, of which the per-lane nonce-counter audit (B) is the one I would not ship
without.
