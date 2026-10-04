# AVA1 review: `ava1` f660dcc → da80f8f (2026-10-04)

Twenty-four commits, about 12k lines: the S1 pairing fix, the review-002 fixes, P3 Tasks 5–9
(`job.run`/`job.list`, 83 management methods ported, the console event log, diagnostics over
AVA1), and two integration passes that make the merged suite pass in parallel. Read in full:
the S1 diff on both ends (`handshake.rs`, `keys.rs`, `ava1_server.c`, `ava1_keys.c`, SPEC
§4.6/§5, `vectors/pairing.txt`), `ava1_op.c`, `fs_jobs.c`/`fs_jobs.h`, `mgmt_table.def`, the
`mgmt_rpc.c` parser and op runner, `ava1_events.c`, the archive-source fixes (`211e55a`), the
SPEC/CUTOVER/MGMT_METHODS diffs, and the `mgmt.rs` routing path.

## 1. Verdict

S1 is fixed and fixed well (§2). Nine of the eleven review-002 findings are closed, one is
deferred with a CUTOVER entry, and **M1 is still open** and now matters more: 83 methods
route through the code path it concerns. The operation jobs are a sound design with two
SPEC/code mismatches to settle (§4). One new finding on the delete operation (§5).

## 2. S1: commit-then-reveal — closed

`ServerInfo.pair_commit`, `ClientInfo.nonce_c` and `Welcome.nonce_s` are required base fields
(an old layout fails to decode, so nothing optional can be stripped); the client checks
`BLAKE2b-256(nonce_s)` against the commitment *before* any code exists and refuses with
`ERR_PROTOCOL`; the server computes the code only after `ClientInfo` decoded, and both ends
check `vectors/pairing.txt`. The fault-injection tests cover wrong reveal, missing commit,
missing reveal and a client without its nonce. This matches `001/02` §2 S1 exactly and closes
the release gate on the protocol side. Remaining on S1: nothing. Remaining on S2: unchanged
(the trust store carve-out; with `fs.write` and `job.run DELETE` now over AVA1 it is more
reachable than before).

## 3. Review-002 findings

| id | state | note |
|---|---|---|
| M1 CAP_MGMT routing | **open** | `AvaTransport::rpc` (`mgmt.rs:303-330`) still never reads `session.has_mgmt()`; `serves()` keeps the 30 s `no_mgmt` probe cache. `has_mgmt` is used only by ctest. Fix as in 002/01: after `pool().session(console)`, `if !session.has_mgmt() { return Ok(None) }`, delete the cache, keep `ERR_UNKNOWN_METHOD` as a hard error. With 83 methods and Task 4's filesystem block still `todo`, an engine ahead of a payload now silently falls back on dozens of calls. |
| M2 RAR order check | closed | `enforce_order: !solid && start > 0`; a fresh or solid pass binds by path. Test added. |
| M3 legacy-failure parse | closed | `top_value` is a real depth-1 scanner (strings and nesting honoured); `mgmt_legacy_failure` uses it. The three-way LC policy (convert / keep / probe) is a good refinement, and `b0bd401` fixed the probe's refusal token. |
| M4 short folder | closed | `Corrupt("the folder yielded N of M entries")`, unit-tested. |
| M5 7z fallback scope | closed, stricter | Only unsupported coder methods fall back; duplicates, file/dir clashes and `MaxMemLimited` are terminal on both archive kinds. Agreed. |
| L1 entry mtimes | closed, edge deferred | 7z FILETIME, zip DOS time, RAR DOS time via the host zone; the RAR resume edge is in CUTOVER §2. Acceptable; the cleaner fix later is to exclude `mtime` from the archive manifest hash. |
| L2 stats line | closed | opt-in via a flag file. |
| L3 BN_SOURCE | closed | parked-on-budget ticks excluded; the second commit fixed the short-circuit. |
| L4 gate timeout text | closed | "waited behind N calls". |
| L5 token pairs | closed | pinned in ctest. |
| L6, L7 | closed/noted | RAR plan-time decode logged. |
| P5 BLAKE3 -O2 | closed | |

## 4. Operation jobs (`job.run`, `job.list`) — new, sound, two mismatches

Design: an op is a job-table entry with its own worker on the 512 KiB stack, progress
counters read with atomics, result ≤ 128 KiB, 8 running at once, idempotent re-issue by id
(same owner, op and args → the current status), cancel by flag. The walkers
(`fs_jobs.c`) never cross a device, refuse roots and top folders, normalise paths, retry
`EBUSY` for a second (Sony's installer), and are host-tested on real trees. Good.

- **O1 (SPEC/code): cancel.** SPEC §7.1.1 says `job.cancel` "waits up to 2 s for the worker";
  `ava1_op_cancel` sets the flag and returns, by design (the comment explains why: it runs
  on a reader or RPC worker). Make the SPEC say what the code does: the cancel is signalled,
  the poller sees `state 2 / ERR_CANCELLED` when the operation stops at its next entry or
  block. (The job's eventual destruction joins the thread in `op_free`; a cancelled fsck
  therefore holds its job slot until the system call returns, which the SPEC should also say.)
- **O2 (SPEC/code): release on delivery.** `ava1_op_status_delivered` unlists a finished
  releasable op as soon as one `job.status`/`job.run` reply carried its terminal status, so
  the SPEC's "a repeat of `job.run` with the same id … answers the job's status whatever state
  it is in (nothing runs twice)" is not true after that first read: a repeat re-runs the op.
  For delete/chmod/hash/crc32 that is harmless (idempotent), and the reasoning (a loop of
  hashes must not fill the 32-slot table) is right; say so in §7.1.1, or keep the finished
  op listed for a short grace (10 s) after delivery so a lost reply still gets the stored
  answer.

## 5. New finding

**S4-op (low–medium): the delete and chmod operations resolve the policy lexically.**
`op_delete` normalises the path, refuses roots, checks `may_write(path)` and the device of
`path` against its parent, then walks with `lstat`. A symbolic link *in a parent component*
(`/data/x/link/y` where `link` points into `/system`) passes every check — `may_write` is
lexical, `lstat` does not follow the final component only — and the walk removes `/system/y`'s
tree, staying on one device as the guard requires. The same hole exists in FTX2's handler
and in the data plane (001/02 S4). Fix in one place: `realpath()` the request path (FreeBSD
has it), run `may_write` on the result, and refuse when the two differ in a component that
is a directory symlink; apply the same in `op_chmod_r` and in the receiver's `prepare` for
merge mode. Who can plant such a link is the question S2 answers (FTP, FTX2 :9114 today); the
fix costs one call.

## 6. Smaller notes

- `events.log` lives in `/data/ps5upload/ava/`; the S2 carve-out must keep it *readable*
  through `fs.read` (the bug bundle reads it) while `identity`, `peers`, `launch_tokens` and
  `jobs/` stay unreachable. Worth saying in the S2 fix so the two are not confused.
- `shell.exec` and `fs.write` are now reachable by any paired peer. That is strictly
  better than FTX2's unauthenticated :9114, and it is the reason S2 is the next gate.
- `MGMT_METHODS.md`: 83 ported, 27 todo (the filesystem block, `node.shutdown`, Task 4), 6
  n/a. The CUTOVER release gate wants every row hardware-verified; none is yet.
- Task 9 leftovers are honestly recorded in CUTOVER §2 (speedtest semantics, no copy event,
  syslog tail cap). No objection.
- The parallel-suite fixes serialise the ctest management rig behind a process-wide lock,
  which is right given the dispatcher's process-wide table.

## 7. Order of work

1. M1 (`has_mgmt`), one function, before Task 4 ports the filesystem block.
2. S2 carve-out, with the `events.log` readability note; S4-op `realpath` guard alongside.
3. O1/O2 SPEC wording (or the 10 s grace).
4. Then the performance programme of `003-ava1-f660dcc/` (measurement matrix first).
