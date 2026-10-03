# AVA1 design review, round 2

Second, independent pass over the `ava1` branch at `e64002f` (123 commits over `main`,
~69k lines added). Read in full: `protocol/ava1/SPEC.md`, `protocol/ava1/CUTOVER.md`,
the whole console data plane (`payload/ava1/ava1_{noise,aead,keys,server,conn,data,apply,
recv,send,copy,journal,manifest,calibrate,tune}.c`, `platform_ps5.c`, `src/ava1_glue.c`),
the Rust protocol crate (`handshake.rs`, `keys.rs`, `launch.rs`, `link.rs`, `conn.rs`,
`session.rs`, `send.rs`, `recv.rs`, `governor.rs`, `journal.rs`), the engine adapters
(`ps5upload-ava1/src/{upload,download,relay,pool,route}.rs`), `ps5upload-engine/src/ava1_api.rs`,
the payload `Makefile`, and the test inventories of `ava1-ctest`. The first review
(`ava1-design-review.md`, ran on Sonnet) is taken as input: §5 below says which of its findings
this pass confirms from the code, which it re-ranks, and what is new.

Nothing was run on hardware. The Rust crate and its tests build and pass locally; every
performance statement is either from `CUTOVER.md` §4 or marked **[hyp]** with the experiment
that decides it. Evidence tags: **[code]** read in the tree, **[spec]** from SPEC/CUTOVER,
**[hyp]** inference.

---

## 1. Verdict

The architecture is right and the implementation is unusually disciplined for its size:
one schema generating two codecs, shared vectors, differential C-vs-Rust tests of the
receiver, crash-point tests of the durability chain, fuzzers on both decoders, and a
sender whose window accounting survived three rounds of adversarial fixes with the
reasoning written down next to the code. The 223k-file result (AVA1 finishes and
verifies; FTX2 dies) is the project's justification and it holds.

Two things must change before any build that users run:

1. **The pairing code does not stop an active man-in-the-middle** (S1). This is a design
   gap in SPEC §4.6, not an implementation slip; the fix is a small addition to the
   handshake (a commitment), cheap because nothing has shipped.
2. **The console's AVA1 trust store sits inside the tree the legacy unauthenticated
   services can read and write** (S2). Until FTX2's management port and the FTP server
   are gone or fenced, pairing security is only as strong as "nobody hostile on the LAN",
   which is exactly what AVA1 set out to improve on.

The performance gap to FTX2 is small, explainable, and has one cheap experiment nobody has
run yet (P1: multi-GiB `posix_fallocate` held under the job mutex). Nothing is fundamentally
wrong with the protocol shape, the credit scheme, or the durability chain.

---

## 2. Findings

Severity is for a release to users. Each finding names the code, the fix, the test that
should pin it, and a rough effort.

### Security

#### S1 — High. An active MITM can make both devices show the same pairing code

**[spec][code]** SPEC §4.6: `code = BLAKE2b-256("AVA1 pairing" ‖ h)[0..4] mod 10⁶`,
"a man in the middle yields different `h`, so different codes". That is true of a
*passive* relay. An active attacker runs two Noise XX handshakes — as the server toward
the real client and as a client toward the real console — and finishes the second only
after the first is complete. In Noise XX the **last** thing mixed into `h` is message 3's
encrypted payload (`ava1_noise.c:165-169`: `s`, `se`, then `enc_hash(payload)`), and that
payload is `ClientInfo{name}`, chosen by the initiator (`handshake.rs:165-175`; the console
accepts any name up to its 512-byte payload buffer, `ava1_server.c:793,845-850`). Once the
attacker knows the client-side hash, it searches names until the console-side code
matches: ~10⁶ candidates, each one small AEAD plus two BLAKE2b calls, so seconds on a
laptop — inside the console's 10 s handshake window (`ava1_glue.c:174`). The user sees the
same six digits on both screens and confirms; the console stores the attacker's key, and
the client stores the attacker's key as "the console".

Preconditions: pairing window open on the console (first use, or `pairing.open`) and the
attacker able to put itself in the path (ARP/DNS spoofing, a rogue "PS5" answering first,
a mistyped address). The launch-token path (§5.2) avoids the code in the common desktop
case; the code path is what the Docker/web engine and "pair another device" use, i.e. the
deployments most likely to be on a shared network.

Short authentication strings are only safe with a **commit-then-reveal** round (ZRTP
RFC 6189 §4.4.1.1, Bluetooth numeric comparison). Recommended change, both sides, before
release (nothing has shipped, so version 1 can change in place):

- Server adds `commit = BLAKE2b-256(nonce_s)` to `ServerInfo` (message 2); client adds a
  fresh `nonce_c` (16–32 B) to `ClientInfo` (message 3); server reveals `nonce_s` inside
  the sealed `Welcome`; client checks `BLAKE2b-256(nonce_s) == commit` or closes.
- `code = BLAKE2b-256("AVA1 pairing" ‖ h ‖ nonce_c ‖ nonce_s)[0..4] mod 10⁶`.
- Make the three fields **required** (a peer that omits them is refused): an optional
  extension can be stripped by the attacker.

Why it works: facing the client, the attacker commits before it sees `nonce_c`; facing
the console, it must send its own nonce before the console reveals `nonce_s`. Each active
attempt is a 1-in-10⁶ guess, and every attempt costs the attacker a visible pairing
notification on the console (`on_pair_request`, rate-limited to one per 10 s).
Alternatives, not recommended: an 8-digit code only multiplies the search by 100; a
PSK from a typed code (`Noise_XXpsk3`) works but changes the UX for every pairing.

Tests to add: a vector for the new code derivation in `vectors/`; a `noise.rs` ctest that
a C server refuses a client without the nonce and a Rust client refuses a server whose
reveal does not match its commit; a unit test that the code depends on `nonce_s`.
Effort: ~1 day across schema, C, Rust, vectors, SPEC §4.6/§5.

#### S2 — High. The AVA1 trust store is writable through the legacy unauthenticated paths

**[code]** The console keeps its identity and peers in `/data/ps5upload/ava/{identity,peers}`
(`ava1_glue.c:29-30,168-169`). FTX2's management port (:9114, unauthenticated) allows every
filesystem operation under `/data` (`runtime.c:6462-6476`), and the optional FTP server
defaults to root `/` (`ftp_server.c:1254`). While the two protocols coexist (`CUTOVER.md`
project 3 is the removal), anyone on the LAN can: append a key to `peers` (paired, no
code, no notification); read `identity` (impersonate the console to an engine that pinned
its key — `pool.rs` `consoles` file — defeating `connect_expecting`); or replace `identity`
(every engine then refuses with `WrongPeer`). The engine's HTTP API is also unauthenticated
by design (README); with AVA1 the engine's identity file is now a credential, so any route
that can read `<data dir>/ava/identity` or `launch_tokens` is in scope too (the engine
guards the token endpoint to loopback, `ava1_api.rs:110-122` — good).

Fix (small): carve `/data/ps5upload/ava` out of `is_path_lexically_allowed` and of the FTP
view; make AVA1's own `may_write`/`may_read` refuse it as well (defence in depth against a
paired peer uploading a `peers` file). State in `SECURITY.md` that until the FTX2 port is
removed, the trust store's integrity depends on the LAN. Tests: an FTX2 integration test
that an fs_write under that path is refused; extend `i7_writing_where_it_is_not_allowed_is_refused`.

#### S3 — Low. Peer-supplied mode bits and a path-based mtime

**[code]** `fchmod(fd, mode & 07777)` applies setuid/setgid/sticky from the manifest
(`ava1_apply.c:805`, `:1458`); mask to `0777`. `ava1_platform_set_mtime` uses
`utimensat(AT_FDCWD, path, …)` with flags 0 after an `O_NOFOLLOW` open (`platform_ps5.c:39-46`):
a path lookup that follows symlinks on a file just opened without following them. Use
`futimens(fd)` if the SDK exposes it, else `AT_SYMLINK_NOFOLLOW`.

#### S4 — Low-Medium. Intermediate symlinks are checked once, at the root

**[code]** `may_write` runs `realpath` on the job *root* at open (`runtime.c:6498-6530`);
`prepare()` `lstat`s only the manifest's *directory entries*, and only in merge mode
(`ava1_recv.c:862-869`); a file whose parent is not a manifest entry gets `mkparents` and an
`open(O_NOFOLLOW)` that protects the last component only (`ava1_apply.c:793-794`). A symlink
planted at an intermediate component after `prepare` (by anything in S2's position)
redirects writes outside the allowlist. Mitigation that also pays for itself in P3:
open files through a cached parent-directory descriptor (`openat`) obtained once per
directory with `O_DIRECTORY|O_NOFOLLOW`, refusing a parent that is not a real directory.

### Durability and correctness

#### D1 — Confirmed sound. The durability chain

**[code]** `sync_batch` orders data fsyncs (striped over workers) → directory fsyncs →
journal append + fsync → in-memory state → `Durable` (`ava1_apply.c:1051-1292`); the commit
of a large file fsyncs, renames in the same directory after the `st_dev` check, fsyncs the
directory, then journals (`:1411-1522`); the staging rename is guarded the same way
(`:1546-1586`); `JnlDone` precedes `JobDone` (`:361-398`); `EIO` is never retried and a
retried fsync is distrusted until the bytes are re-read against their root or outboard
(`ava1_journal.c:43-95`, `ava1_apply.c:903-990`). The Rust twin has the same order
(`recv.rs:1276-1491`). The ctest crash-point suite (`c_crash_between_sync_and_journal_resends_the_batch`,
`c1_…commit_rename_and_its_journal`, `c2_…`, `i1_…staging_rename`, the fsync-retry
family) is the right kind of evidence. Keep all of it. The one caveat stands from the
first review (its F9): "verified" means verified in transit against the sender's root;
no read-back of what the drive holds except after a retried fsync.

#### D2 — Medium (process). CUTOVER §3 lists as open three items the code already does

**[code][spec]** "the console receiver's `ERR_CREDIT` handling ends the lane, not the
session" — it now closes only the lane (`ava1_data.c:1303-1317` returns 1 from `on_lane`;
`serve_loop` breaks; the session stays). "`Resume` does not re-send credit" — it does
(`ava1_data.c:1097-1116`). "the engine's host ignores `Resume`" — `recv.rs:487-505`
`resume_job` answers it. Update the checklist so the release gate is not blocked on done
work, and keep the one that is real: the engine re-hashes more than the console on resume
(§13.4 allows it).

#### D3 — Low. A move that leaves non-regular entries reports a failed job

**[code]** `delete_source` skips what the walk skipped (symlinks, devices) and reports
`ERR_IO` "source deletion left N paths" (`ava1_copy.c:120-160,197-210`) after the copy
succeeded and most of the source is gone. Give it its own status or message ("copied; N
entries left at the source"); the UI otherwise shows a failure for a successful copy.

#### D4 — Medium. The manifest cap is larger than the console's memory

**[spec][code]** SPEC §11.7 allows 4,000,000 entries and the receiver reserves them
up front (`ava1_recv.c:413`, `ava1_manifest.c:41-50`). At ~40 B per entry plus the path
arena (~60 B average) that is ~400 MB before any data buffer, on top of the data plane's
own ceiling (admitted lane bytes ≤ 96 MiB budget, held frames ≤ total credit ≤ 96 MiB,
control inboxes ≤ 64 MiB, sender read-ahead 32 MiB: `ava1_data.c:21-22,234,1263-1268`,
`ava1_send.c:18`, `ava1_job.h:214`). Nothing states the payload's memory budget. Lower the
cap to what a measured budget allows (1,000,000 is still 4× the largest known game) or
derive it from `budget_free`, and write the total ceiling into SPEC §8/§11.7. The 223k
corpus is fine today; this is about the failure mode being an OOM in a kernel-adjacent
process rather than `ERR_BUSY`.

### Performance

#### P1 — High value, one-line experiment. Preallocation under the job mutex

**[code]** `lfile_open` calls `posix_fallocate(fd, 0, size)` for every new large file
(`platform_ps5.c:33-37`, `ava1_apply.c:618-631`) **while holding `j->mu`** (`write_chunk`,
`:653-664`; acknowledged in `ava1_job.h:147-149`). Every worker's `enqueue`, the feeder's
`ava1_apply_chunk`, and the job thread take `j->mu`, so the whole apply engine of that
job waits for the call. On FreeBSD-family kernels a filesystem without an allocate fast
path gets the generic implementation, which **writes zeros over the whole range**: a 4 GiB
file costs a 4 GiB write before its first data byte lands, the 64 MiB window fills in
under a second and the sender stalls on credit for the rest. **[hyp]** The measured
large-file gap (5–8 %, 2–4 s of a ~40 s transfer) is the right size for a 4 GiB zero-fill
at 1–2 GB/s on the Pro's SSD. The CHANGELOG shows FTX2 adopted `posix_fallocate` to fix a
slowdown on long transfers (`CHANGELOG.md:5230`), so the call has history; the difference
here is the lock and the credit window behind it.

Experiment (an hour): log the call's duration around `ava1_platform_preallocate`, re-run
the 4 GiB upload; then try `ftruncate` only on `/data` and compare. If confirmed: either
preallocate off the mutex (open and `ftruncate` under `mu`, `fallocate` after releasing
it, on the worker), or drop to `ftruncate` (sparse part file; `ENOSPC` still surfaces at
`pwrite` and already maps to `ERR_NO_SPACE`), or preallocate in 256 MiB steps ahead of
the write cursor. Rank this above the first review's F4 (fsync cadence) because it costs
one line to test.

#### P2 — High value for the 223k case. Directory fsyncs are serialized on the job thread

**[code]** Data fsyncs are striped across workers (`ava1_apply.c:1147-1155`) but the
directories that gained entries are synced one after another on the job thread
(`sync_new_dirs` → `ava1_sync_dirset`, `:1009-1047`). The 223k corpus has 20,075
directories; with ~245 files per batch (P4) each batch touches many of them, and on UFS
with soft updates a directory fsync waits for dependent metadata — often the slowest
fsync there is. **[hyp]** This is a strong candidate for the unexplained 82.5 files/s
(first review F3). The job already prints `per batch ms: scan … data … dirs … journal …`
every 10 s (`log_stats`, `:1665-1679`): read that line from the 223k run before changing
anything. Fix: run the directory fsyncs through the same `ava1_apply_parallel` stripes as
the data fsyncs, and have the batch sort its small files by parent so fewer directories
appear per batch. Small change, receiver-local, covered by the existing
`new_small_files_have_their_directories_synced_before_the_journal` test.

#### P3 — Medium. Six syscalls per small file, two of them full path lookups

**[code]** `apply_record`: `open(O_CREAT|O_TRUNC|O_NOFOLLOW)`, `write`, `fchmod`,
`utimensat(path)`, later `fsync`, `close` (`ava1_apply.c:793-812`). Pass the mode to `open`
(set `umask(0)` once in the data layer) and drop `fchmod`; use `futimens(fd)` and skip it
when the manifest mtime is 0; open through a cached parent dirfd with `openat` (also S4).
On a 20k-directory tree the two path lookups per file are not free.

#### P4 — Medium, lower risk than deferring durability. Stop holding fds across the batch

**[code]** A small file keeps its descriptor open until the batch syncs it (`pend_add`,
`:725-762`), so the batch size is bounded by the open-file share — about 245 on firmware
13.60 (`ava1_pend_share`, `ava1_data.c:184-187`; SPEC §15.6) — and every per-batch fixed
cost (directory fsyncs, the journal append and its fsync, `Durable`) is paid once per ~245
files. **[hyp]** Fsync the file in the worker right after the write and close it; the
batch then only syncs directories and journals. The durability chain is unchanged (data
fsync still precedes the journal), the fsync parallelism is identical (it already runs on
the workers), the fd budget stops mattering, and `batch_max` can grow to 1,000+, halving
or better the per-file share of fixed costs. Measure with a `disk.calibrate` mode
"fsync-then-close in the writer" before and after. Try this before the first review's R1b
(write-ahead, verify-on-resume), which changes what `Durable` promises.

#### P5 — Confirmed, trivial. The BLAKE3 C sources are compiled at -O0

**[code]** `BLAKE3_SRCS` is part of `SRCS` (`payload/Makefile:16-23,152-162`) compiled in
the ELF rule with `CFLAGS` that carry no `-O` (`:171,191-192`); only AVA1's own objects
get `-O2` (`:60-61`). The assembly kernels are fine; the dispatcher, chunk state and
parent merges are not. Build the three `.c` files like the AVA1 objects. Also helps FTX2.

#### P6 — Confirmed. One reader thread on the console sender

**[code]** `ava1_read_files` / `reader_main` read every file serially (`ava1_send.c:123-171,254-296`).
First review R2 stands: N readers over disjoint id ranges for unordered jobs, one for
`JF_ORDERED`. The engine-side mirror (per-file `fsync` + per-batch drive flush in
`LocalSink::sync`, `recv.rs:218-249`) is the other half of the tiny-download gap, worst
on Windows.

#### P7 — Low. Socket buffers and `Received` threads

**[code]** The console sets only `TCP_NODELAY`/`SO_NOSIGPIPE` on accepted sockets
(`ava1_server.c:1056-1058`); set 1–2 MiB `SO_RCVBUF` so a lane thread busy on a 15 MiB
frame does not close the TCP window. `post_received` spawns a short-lived thread per ack
when the post queue runs low (`ava1_data.c:302-384`, bounded at 32): correct, but a
priority slot for `Received` in the connection's writer queue would avoid the churn.

### Robustness

#### R1 — Medium. `dead_after` is 6 s on both ends, fixed

**[code]** `ava1_glue.c:172-174`, `session.rs:33-42`. First review F7 stands: Wi-Fi scan
and roaming gaps of 3–10 s will end sessions, park jobs, and resume — correct but costly.
Make it adaptive (`max(6 s, 8·SRTT + 2 s)`), and prefer re-joining a dead lane while the
control connection still answers over ending the session.

#### R2 — Medium. The Pro outage: what AVA1 adds that FTX2 did not

**[code]** New kernel-facing behaviour on the console, in order of "unusual": multi-GiB
`posix_fallocate` under a mutex (P1), a 4096-descriptor probe at start (`fd_probe`,
`ava1_data.c:120-137`), up to 16 worker threads per job plus a thread per connection and
per RPC (256 KiB stacks), `sysctl(KERN_ARND)` for randomness, and the memory ceiling in D4.
`rename` is guarded (`same_device`) everywhere it is called. If the outage recurs, the
`[ava1]` stderr lines (fd budget, fsync retries, per-job stats) are the first place to look;
P1 is the one syscall pattern here with a plausible multi-second kernel-side effect.

#### R3 — Low. Governor decisions inside the noise

**[code]** A new lane is kept only if throughput rose ≥ 10 % after a single 1 s tick
(`governor.rs:14,201-207`), against a harness with ~25 % run-to-run drift (`CUTOVER.md` §4).
First review R6 stands: start at 4 lanes on wired links, judge over several ticks.

### Credit, liveness and deadlock — reviewed, no defect found

**[code]** The sender's window (`send.rs` `Window`, `lane_death`, `:207-310,962-1039`) holds
the charge of every frame the writer may have put on the wire and releases exactly the
frames it provably never took; the receiver's `w_avail` mirror (`ava1_data.c:1303-1317`)
and the apply-level `credit/outstanding` (`ava1_apply.c:418-444`) agree by construction
(the grant is the same number on both ledgers; frames enter the second only after the
first admitted them). The open-file share cannot wedge: a worker waiting for a slot runs
queued sync stripes itself (`pend_gate_idle`, `pend_add`, `:694-762`), and the job thread
syncs early when the share is full (`job_main`, `:1709-1711`). A stall with bytes
outstanding is treated as a slow receiver (10 min), one with nothing outstanding fails in
10 s (`send.rs:434-455`). The one accepted gap is documented in the code (`Stall`,
`:355-371`): a receiver that pings but never acknowledges parks the job; that receiver is
outside the protocol. This matches the first review §4.3.

---

## 3. The CUTOVER release gates, answered

- **Crypto and key handling** (`ava1_aead.c`, `ava1_chacha_avx2.c`, `ava1_noise.c`,
  `ava1_keys.c`, `handshake.rs`, `keys.rs`, `launch.rs`): the primitives and their use are
  sound. Noise XX is implemented per revision 34 and pinned by the cacophony vector on
  both sides; the AEAD nonce is per lane and direction from 0 with fresh lane keys per
  join (replayed joins can never reuse a key); low-order points are refused; a failed
  handshake poisons its state and yields no keys; the C decrypt-then-verify restores the
  ciphertext on a bad tag so plaintext never escapes; tag comparison is constant time;
  the launch proof is bound to the handshake hash and only sent to the slot's key. The
  two problems are above the primitives: S1 (the SAS has no commitment) and S2 (where the
  keys live). Both are fixable without touching the primitives.
- **Filesystem and kernel-touching code** (`ava1_apply.c`, `ava1_recv.c`, `ava1_data.c`,
  `ava1_copy.c`, `ava1_send.c`, the fd-budget commits): correct in the ways that matter
  (D1); every `rename` is `st_dev`-guarded; the staged-tree lock folder, the `.ava-part`
  placement and the remap of a changed manifest are careful. Open items: P1 (what
  `posix_fallocate` costs under the lock), S3/S4 (mode bits, symlinks in the middle),
  D4 (the memory ceiling).
- **Spec vs code**: D2 — three listed gaps are closed; update the document.
- **Hardware pass**: still required, and should now include the P1 timing line and the
  per-batch `dirs` vs `data` split from `log_stats` for the 223k corpus.

---

## 4. Answers to the design questions, briefly

- **Multi-TCP vs QUIC**: multi-TCP is right (first review §4.1); nothing in this pass
  changes that. The lanes buy receiver parallelism and loss isolation, not bandwidth.
- **Per-file fsync**: keep the promise; change where the fsync runs (P4) before changing
  what `Durable` means (first review R1b). Then measure.
- **Credit/ack deadlock-freedom**: none found; see the section above and the first review §4.3.
- **Liveness**: sound; 6 s is the one number to revisit (R1).
- **Pairing and trust**: not sound as specified (S1), and not enforceable while the
  trust store is reachable through FTX2/FTP (S2). Sound after both fixes for a LAN tool.
- **Forward-only archives (7z/RAR) and resume**: the first review §4.6 is the right plan
  (manifest from the archive index, seek to the nearest solid block, bounded skip-decode,
  temp-extract fallback when disk allows).

---

## 5. Relation to the first review (`ava1-design-review.md`)

| First review | This pass |
|---|---|
| F1 per-file fsync at the create ceiling | Confirmed from code. Prefer P4 (fsync in the worker, no held fd) over R1b as the first step. |
| F2 pairing code grindable | **Confirmed from the C and Rust handshake code; raised to the top blocker.** Concrete commit-reveal design in S1. |
| F3 223k at 82.5 files/s unexplained | New candidate: serialized directory fsyncs per batch (P2). Read `log_stats` first. |
| F4 large-file fsync cadence blocks `pwrite` | Plausible but untested; **P1 is the cheaper and likelier explanation** of the same gap. Test P1 first. |
| F5 single console reader; engine per-file fsync | Confirmed (P6). |
| F6 FTX2 never fsyncs: compare time-to-durable | Agreed. |
| F7 `dead_after` 6 s | Confirmed (R1). |
| F8 BLAKE3 at -O0, no socket buffers | Confirmed from the Makefile (P5, P7). |
| F9 "verified" is transit-only | Agreed. |
| F10 complexity | Agreed; the ctest differential suite is the right mitigation. |
| — | **New:** S2 trust store reachable via FTX2/FTP; S3/S4 mode bits and mid-path symlinks; D2 stale CUTOVER items; D3 move status; D4 manifest cap vs memory; P1 fallocate under the mutex; P2 serialized dir fsyncs; P3 syscalls per small file. |

---

## 6. Suggested order of work for the implementation session

Each item names its acceptance test. Items 1–2 gate any user-facing release; 3–6 are
the performance work in the order of cheapest evidence first.

1. **S1 — SAS commitment.** Schema fields, C and Rust handshake, SPEC §4.6/§5, vectors
   (`vectors/pairing.txt` with `h`, `nonce_c`, `nonce_s` → code), ctest: C refuses a Rust
   client that omits the nonce; Rust refuses a server whose reveal mismatches its commit.
2. **S2 — fence `/data/ps5upload/ava`.** `is_path_lexically_allowed`, the FTP view, AVA1's
   `may_write`/`may_read`; `SECURITY.md` paragraph; FTX2 integration test for the refusal.
3. **P1 — time `posix_fallocate`; try `ftruncate` only.** One stderr line and one A/B on
   the 4 GiB corpus on `/data` and `ext1`. Decide between "off the mutex" and "ftruncate".
4. **P5 — `-O2` for the BLAKE3 C files.** `make payload`; `cargo test -p ava1-ctest`.
5. **P2 — read the 223k `log_stats` line; stripe directory fsyncs.** Target: the `dirs`
   share of a batch falls to a fraction of `data`.
6. **P4 + P3 — fsync in the worker, close early; fewer syscalls per file.** Extend
   `disk.calibrate` with the mode first so the gain is measured, not assumed. Then
   re-run the tiny-file table.
7. **D2, D4, D3 — doc and cap hygiene.** CUTOVER §3; `AVA1_MAX_ENTRIES`; move status.
8. **R1, P6, P7, R3 — later.** Adaptive `dead_after`, console readers, socket buffers,
   governor start at 4 lanes on wired links.

---

## 7. What is good and should not be touched

- Two-level acknowledgement (`Received` before disk, `Durable` after the chain) and the
  rule that a requeue keeps its charge — the reason the 223k upload finishes.
- The durability chain and its tests (D1), the transient-fsync retry with read-back, and
  `EIO` as final.
- Staging with a lock folder, same-directory renames, `st_dev` guards, the `.ava-part`
  placement rules, and the remap of a changed manifest by path with no byte splicing.
- One schema, two codecs, shared vectors, fuzzers, differential C-vs-Rust tests, pure
  governors tested against models, and the monotonic-clock lint.
- Noise XX + ChaCha20-Poly1305, per-join lane keys, low-order refusal, launch tokens
  bound to the handshake and minted only for loopback callers.
- `CUTOVER.md`'s honesty about what was measured on which commit.
