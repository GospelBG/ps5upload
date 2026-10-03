# AVA1 design review

Independent review of AVA1 (branch `ava1`; protocol/ava1/SPEC.md and CUTOVER.md read in full; `engine/crates/ava1` and `payload/ava1` read selectively at the points cited). No code was changed and nothing was run on hardware: every performance statement below is either taken from CUTOVER.md section 4 or is labelled a hypothesis with the experiment that would test it.

Evidence labels: **[code]** read in the tree, **[spec]** stated in SPEC.md/CUTOVER.md, **[ext]** external source (section 9), **[hyp]** my inference, needs an experiment.

---

## 1. Verdict in one paragraph

The design is sound and unusually careful for a purpose-built protocol. It fixes the real failure of FTX2 (ack-after-apply, no fsync, unbounded shard windows), the layering (control plus lanes, manifest, journal, outboards, credit) is coherent, and the 223k-file result (AVA1 finishes and verifies, FTX2 dies) justifies the project. The remaining speed gap is **not a protocol-shape problem**. It is (a) a per-file fsync that sits exactly on the console's serialized file-create ceiling, (b) a fsync cadence on large files that probably stalls writers, (c) a single-threaded small-file reader on the console and a per-file durability policy on the host receiver, and (d) a few cheap build and socket settings. The one security issue worth fixing before release is that the six-digit pairing code is grindable by an active man-in-the-middle. Nothing found is fundamentally wrong with the approach.

---

## 2. What the numbers actually say (CUTOVER.md section 4)

1. **FTX2's tiny-file win is the missing fsync, shown by the project's own tool.** `disk.calibrate` fsyncs every file (`payload/ava1/ava1_calibrate.c:71`) and measures a ceiling of about 287-298 files/s on `/data`. AVA1 gets 291 files/s there, so AVA1 is *at* the fsync-inclusive ceiling; FTX2's 320-343 files/s exceeds it because it does not fsync. Calibrate also shows creation scaling from 182 files/s (1 worker) to about 290 (16 workers): roughly 3.4 ms of serialized work per file, of which the measured 0.53 ms fsync is about 15%. That matches the observed -13% on `/data`. [spec][code][hyp on the causal reading]
2. **The comparison is not like-for-like on durability.** FTX2 never fsyncs, so its "done" means "in page cache". AVA1's means "on stable storage". Fair check: FTX2 plus a final `sync` (time-to-durable). "Beat FTX2" should be stated against both wall-clock to JobDone and wall-clock to durable.
3. **Large files: 5-8% slower.** Framing cannot be the reason: 16 B header, 34 B body header and 16 B MAC per up-to-15 MiB frame is under 0.001%. The loss is CPU on the lane thread, writer stalls from the fsync cadence, or governor ramp (section 5).
4. **223k files at 82.5 files/s is the biggest unexplained number**: 3.5x under the drive ceiling and not reproducible on loopback. It matters more than the 5-10% gaps because it is the real workload (R5).
5. **Tiny download 50-100% of FTX2** (after `b1be8059`; the Phat rows predate it). The console sender reads small files from one thread; the host receiver's durability policy is the other suspect (R2).

---

## 3. Comparison with established designs

| System | What it does | AVA1 relation | Verdict |
|---|---|---|---|
| **rsync** [1] | Rolling weak checksum plus strong hash per block finds moved data in a similar destination; one TCP stream; no per-file durability | AVA1 has no delta. It has the cheap half: per-file and per-group roots, `skip-existing` and `verify` policies (SPEC 11.4) | Right call. Delta needs a similar old copy; game installs are new or whole-file replaced. Keep `verify` for "already there". rsync's `--partial` resume is weaker than the journal. |
| **QUIC / HTTP/3** [2,3] | UDP, per-stream loss recovery, no cross-stream head-of-line blocking, migration, 0-RTT, TLS 1.3 | AVA1 uses N TCP lanes to get stream independence | Section 4.1: multi-TCP is right for this use. |
| **Aspera FASP / UDT / BBR** [4,5,6] | UDP with rate-based, loss-tolerant congestion control for long fat pipes | Not applicable: BDP of a gigabit LAN at about 0.3 ms RTT is about 40 KB; one TCP flow fills it | Correctly ignored. Host kernels own congestion control; the PS5 cannot run BBR. |
| **BitTorrent** [7,8] | Fixed pieces, per-piece hash (v2: per-file SHA-256 Merkle), verify before sharing | Same principle; AVA1's 1 MiB group CV in an outboard is a Merkle leaf level | Good. v2's per-file Merkle root is what AVA1 got via BLAKE3. |
| **Syncthing BEP** [9] | Blocks of 128 KiB-16 MiB (about 2000 per file), hashes in the index, receiver pulls blocks by request | AVA1 is sender-push with credit | Pull suits multi-source and simple resume; push is faster per RTT. For one sender and one receiver, push plus credit is right. |
| **restic / borg** [10,11] | Content-defined chunking for cross-version dedupe, encrypted repo | Fixed 1 MiB groups, no dedupe | Right: CDC pays across many versions over time. A patch workflow could use it later; out of scope. |
| **zstd-seekable** [12] | Independent frames plus a seek table | No compression in v1 | Low priority: game data is mostly already compressed, and the console budget is better spent elsewhere. Seekable frames would be the right shape if added (they keep random access, which resume needs). |
| **tus** [13] | `HEAD` returns offset, `PATCH` appends at offset | `JobMap` is the same idea generalised to sparse ranges and many files | Same shape; AVA1 strictly more general. tus's single offset is exactly the forward-only case of section 4.6. |
| **S3 multipart** [14] | Independent numbered parts, per-part ETag, completion call | Chunks at 1 MiB-aligned offsets plus a final `FileRoot` | Good: parts are independently retryable and the assembly is checked (`FileRetry`). |
| **Bao / BLAKE3** [15] | Tree-structured BLAKE3 encoding, outboard mode, verified streaming and slices | Group CVs in a `.ob` outboard are Bao-outboard in spirit at 1 MiB granularity | Good fit. Note it verifies **transit**, not the disk (F9). |
| **WireGuard / Noise** [16,17] | Noise IK, ChaCha20-Poly1305, 1-RTT; keys authenticated out of band | Noise XX with static-key pairing | Good primitive choice. WireGuard authenticates keys out of band; AVA1 replaces that with a six-digit comparison, where the weakness is (F2). |

---

## 4. Answers to the specific questions

### 4.1 Multi-TCP versus QUIC

- On 1 Gbps wired LAN **one** TCP flow already fills the pipe. The lanes exist for **receiver parallelism** (decrypt, hash, disk) and **loss isolation on Wi-Fi**, not bandwidth. Legitimate, but it means the governor's "add lanes while throughput rises 10%" is searching for a CPU/disk effect, not a network one.
- QUIC would need a userspace stack in a payload built on the console's toolchain, no UDP GSO/GRO on FreeBSD (one `sendmsg` per 1.2-1.4 KB packet, about 90k syscalls/s at 110 MB/s), per-packet AEAD, and userspace pacing, on a CPU shared with games and Sony services. [hyp from known FreeBSD UDP limits]
- QUIC's real advantage on flaky Wi-Fi is per-stream loss recovery and migration. Multi-TCP gets about half: a lost segment stalls only its lane, but all lanes share the radio and each backs off after a loss burst. Acceptable for a home Wi-Fi hop.
- **Conclusion: multi-TCP is right; do not move to QUIC.** Invest in tolerating short outages (F7), which QUIC would not solve either.

### 4.2 Per-file fsync versus cheaper alternatives

Facts: FreeBSD has no `syncfs`, and `sync(2)` "may return before the buffers are completely flushed" [18], so it is not a barrier. Whether `fsync` on an already-clean file skips the device cache flush on the PS5 filesystem is **unmeasured**.

| Option | Safe? | Saves | Notes |
|---|---|---|---|
| Per-file fsync (today) | Yes | 0 | 0.53 ms/file, about 15% of the per-file budget, serial. Also forces each pending small file to hold an fd until its batch syncs; with the 619-fd ceiling that caps a batch near 245 files (SPEC 15.6). |
| `sync()` then fsync only the last file | Not provably | up to all | Needs a measured A/B on 13.60 and a power-cut test; no kernel promise. |
| `fdatasync` | Data only | little | Skips mtime; AVA1 sets mtime before sync. Availability on this FreeBSD base unconfirmed. |
| **Write-ahead manifest plus verify-on-resume (small files)** | Yes, if done right | the per-file fsync | Journal records "applied, not yet durable" with the root. After a crash the page cache is gone, so resume re-reads each such file from disk, compares BLAKE3 to the journaled root, and re-sends mismatches. Costs a read pass on crash only. |
| Deferred barrier at end of job | Yes | nothing (moved) | Helps only if sorted, un-interleaved fsyncs coalesce better. |

Assessment: **keep the durability promise, but do not pay it per file on the hot path.** The danger of deferral is the window: if the console panics or loses power between `JobDone` and the kernel's own flush (it has panicked; see the Pro outage in CUTOVER section 3), a "done" job silently loses files and nothing triggers a resume. So any deferral must keep a **mandatory durable barrier before `JobDone`**, and the small-file `Durable` ack must be documented as "applied and journaled with root, verified on resume". Expected gain is bounded: 13-15% on `/data`, perhaps 30% on ext1, only if the final barrier costs less than N fsyncs; otherwise the gain is only freeing the fd budget (R1b).

### 4.3 Is credit and ack deadlock-free?

I could not construct a deadlock, and several classic ones are designed out:

- A dead lane's un-`Received` frames stay *charged* and are requeued (SPEC 12.3/12.4), so requeued frames need no new credit. That removes the classic case: window full of later-file frames while an ordered (`JF_ORDERED`) receiver waits for the one on the dead lane.
- A window below one group fails the job instead of stalling; a window that holds bytes but does not return them is a slow receiver caught by liveness, with a 10-minute no-progress cap.
- Every job needs at least 8 MiB of buffer budget or gets `ERR_BUSY` at open, so there is no hold-and-wait across jobs.

Residual risks (not deadlocks): (a) three stacked windows (receiver credit 64 MiB, per-lane `max(chunk, 2 s x rate)`, TCP) released by different events (`Received`, `Credit`, `Durable`). On a LAN only credit binds (the lane cap is about 220 MB at 110 MB/s), so the per-lane cap is dead weight and reasoning cost. (b) Credit returns after *apply*, and applying a small file waits on the open-file budget, which only a sync batch frees. "Workers wait for pend slots; sync needs a worker" is handled (SPEC 15.6: a worker runs queued sync work) but is the most intricate liveness path in the system; it deserves a model-checked or randomized-scheduler test. (c) CUTOVER lists the console's `ERR_CREDIT` handling as ending the lane rather than as specified; make sure that mismatch cannot strand charged credit.

### 4.4 Are the liveness rules sound?

Mostly. Bytes-not-frames, the rate floor, "a reader never writes", and bounded reply queues are the right set. Concerns:

- `dead_after` = 6 s is **short for Wi-Fi**: channel scans, power-save and roaming produce 3-10 s gaps. A false death ends the session, requeues all in-flight frames and costs a reconnect plus resume. The link-cut-every-10-s result (71-81 MB/s) shows hard cuts are survivable, but spurious deaths on a flaky link are a different failure. Make it adaptive, for example `max(6 s, 8 x SRTT + 2 s)` per lane, and prefer lane-level reconnect while the control connection still answers.
- The "at most 2 late ticks" guard against a starved console process is good; keep it.
- An 8 KiB/s floor with 6 s grace lets a 15 MiB frame hold a connection about 30 minutes; with a 64-connection limit this is a mild DoS surface, acceptable for a LAN tool.

### 4.5 Is pairing and trust sound for a LAN tool?

Overall yes, with these findings.

**F2 (real): the six-digit comparison is grindable by an active MITM.** The code is `BLAKE2b(h)[0..4] mod 10^6` of the final handshake hash (SPEC 4.6). A man-in-the-middle runs two handshakes and needs `code(h1) == code(h2)`. Once it has seen the client's message 3 it knows `h1`; its own message 3 to the server carries an attacker-chosen payload (`ClientInfo.name`) mixed into `h2`. Grinding that name over about 10^6 candidates costs one AEAD plus one hash each, so seconds, well inside the 10 s handshake timeout. Short-authentication-string schemes are only safe with commit-then-reveal (ZRTP [19], Bluetooth numeric comparison), which Noise XX does not give for free. This is analysis from the spec, not a demonstrated attack. It needs an attacker already able to intercept during the pairing window (for example by ARP spoofing). Fixes, cheapest first: (i) the user **types** the code shown on the console into the engine and it is mixed in as a PSK (`Noise_XXpsk3`), so each active attempt gets one guess; (ii) a commitment round (commit to a nonce in messages 2 and 3, reveal after, SAS = H(h || nonces)); (iii) at minimum, make `ClientInfo` fixed-length or hashed separately so it cannot be ground.

**Launch token.** Only as strong as the unauthenticated ELF send; SPEC 5.2 says so itself and the reasoning is correct. Keep it, and say in user docs that first launch trusts the LAN.

**Lifecycle.** 32 peers with oldest silently dropped is fine, but there is no user-visible revocation (a stolen laptop stays paired). Add `peer.forget` and list paired peers by name in the UI. The one-session-per-key eviction is a documented footgun for Docker plus desktop sharing one identity; the warning added is the right mitigation.

### 4.6 Resume for forward-only archives (7z, RAR)

The obstacle is the sender's **source**, not the wire (journal and `JF_ORDERED` already fit). Recommended design, in order of preference:

1. **Manifest from the archive index, resume file-granular.** 7z has a header database with sizes and offsets; RAR5 file headers carry sizes. Build the manifest in archive order without decompressing and run `JF_ORDERED`. The journal's done set tells the sender which files are durable.
2. **Seek to the nearest independent unit.** A 7z *folder* (solid block) is a self-contained coder chain with its own pack-stream offset; restart at the first incomplete folder. Non-solid RAR restarts at the file. Only a solid block forces decoding from its start.
3. **Inside a solid block, skip-forward decode into a discard sink.** Cost is CPU, not network: LZMA decodes at very roughly 50-100 MB/s of output per core, so skipping 100 GB costs 15-30 minutes. Bound it with the per-folder seek and warn in the UI on archives that are one huge solid block.
4. **Do not snapshot decoder state**: LZMA windows can be hundreds of MiB.
5. **Optional bounded spool** (a few GiB) of decoded output so a link drop does not stall the decoder.
6. **Simplest correct fallback when disk allows:** extract to a temp tree, then run an ordinary AVA1 folder job. Resume, retry and verification become trivial; it costs 1x extra space.

Encrypted archives add a key derivation per unit; do it once per folder or file.

---

## 5. Recommendations, ranked by expected impact

Gains are estimates against CUTOVER section 4; each is a hypothesis until measured.

### R1. Make small-file durability cheaper than one fsync per file, and stop holding fds for it
- **Gain:** `/data` tiny upload +10-15%, ext1 up to +30%; lifts the 245-pending-file batch cap. This is the lever that gets AVA1 past FTX2 on tiny uploads.
- **Step 0 (a day, no risk):** extend `disk.calibrate` (`payload/ava1/ava1_calibrate.c`) with modes: per-file fsync (today), `fdatasync`, `sync()` then one fsync, no sync, and fsync-before-close in the writing worker (no held fd).
- **R1a:** if a cheaper barrier is proven (with a power-cut test, not a unit test), use it in `sync_batch` (`payload/ava1/ava1_apply.c`, `sync_stripe` about lines 934-960 and `sync_batch` 1051-1160).
- **R1b:** else the write-ahead variant of 4.2: a `JnlBatch` flag "applied, unsynced, root journaled"; resume re-hashes those files (`ava1_journal.c`, `ava1_apply.c` resume path, Rust twin `engine/crates/ava1/src/journal.rs`, `recv.rs`); mandatory barrier before `JobDone`; schema change in `protocol/ava1/schema/ava1.toml`.
- **Risk:** medium (durability semantics; silent loss if the barrier is wrong). Needs fault injection; the repo already has `crash_at` hooks.

### R2. Fix tiny-file download on both ends
- **Gain:** from 50-90% of FTX2 to parity or better; largest remaining relative gap.
- **Console sender:** `ava1_read_files` (`payload/ava1/ava1_send.c:123-190`) and `reader_main` use **one** reader thread doing open, pread, BLAKE3, close per file. Use 3-4 readers over disjoint id ranges for unordered jobs (`JF_ORDERED` keeps one).
- **Host receiver:** `LocalSink::sync` (`engine/crates/ava1/src/recv.rs:218-260`) fsyncs every file plus a drive flush per batch. Cheap on macOS, but on **Windows `FlushFileBuffers` per file is typically milliseconds** [hyp, untested]. A download to the user's own PC does not need console-grade per-file durability: sync directories per batch, flush once at the end, journal without per-file fsync, and let resume verify by hash.
- **Risk:** low for readers; low-medium for host policy (changes what "Durable" means on downloads; put it in the spec).
- **Measure first:** the helper's per-job stats line plus a host trace of one 2,000-file download. I did not profile; this ranking is from code structure.

### R3. Change large-file sync cadence
- **Gain:** 3-8%, plausibly the whole large-file gap.
- **Why:** SPEC 15.4 syncs every 250 ms or 64 MiB. At 110 MB/s that is about 4 fsyncs/s of the same file, about 27 MB each. A file fsync on FreeBSD-family UFS holds the vnode lock while it flushes, so `pwrite` to that file waits. [hyp; not verified on 13.60]
- **Change:** for large-file bytes trigger by 256-512 MiB and at file end; keep the 250 ms rule for small files. Resume loses at most about 5 s of transfer. Locations: the batch trigger in `payload/ava1/ava1_apply.c`, and `recv.rs` for the engine.
- **Risk:** low. **Test:** A/B on the 4 GiB corpus, 3 interleaved runs (CUTOVER records 25% run-to-run drift).

### R4. Cheap receive-path and build fixes (no wire change)
- **Gain:** 1-4% on large files, some on tiny files.
- `payload/Makefile:94`: `CFLAGS` has no `-O`; the vendored `BLAKE3_SRCS` are compiled as part of `SRCS` at that default while AVA1's own objects get `-O2` via `AVA1_CFLAGS`. The assembly kernels are unaffected, but dispatch, chunk state and parent merges run unoptimised. Give `third_party/blake3` `-O2`.
- No `SO_RCVBUF`/`SO_SNDBUF` is set (`ava1_server.c` sets only `TCP_NODELAY`); set 2-4 MiB so a lane thread busy opening a 4 MiB frame does not close the TCP window. [code]
- `write_all`/`read_all` in `payload/ava1/ava1_conn.c` call `poll` then `send`/`recv` every iteration; poll only after `EAGAIN`.
- **Risk:** very low.

### R5. Chase the 82 files/s on the 223k-file corpus
- **Gain:** potentially 2-3x on the workload that matters most (game installs).
- Hypotheses, in order: (1) **workers striped by file index contend on the same directory** (`sync_stripe` strides `k = i; k += stripes`; `disk.calibrate` used one subdirectory per worker, so its ceiling does not represent a real tree); partition work by parent directory. (2) Full-path `open` on deep names; cache a few directory fds and use `openat`, inside the fd budget. (3) Per-batch fixed costs (directory fsyncs, journal fsync, pool-barrier tail latency) amplified by the 245-file batch cap and 20,075 directories. (4) Per small file the receiver does open(`O_CREAT|O_TRUNC`), write, `fchmod`, mtime, fsync, close (`ava1_apply.c` about 793-805): open with the final mode (set umask once), drop `O_TRUNC` for staged new files, skip mtime when the manifest has 0.
- **Risk:** low; receiver-local. **Test:** a synthetic 223k corpus with the same directory-size histogram on the console, and `disk.calibrate` using a shared directory.

### R6. Simplify the governor for the LAN case
- **Gain:** less variance and regression risk; maybe 1-3%.
- A lane is added only for a 10% throughput gain, while the project's own harness sees 25% run-to-run drift: the signal is below the noise, so decisions are partly random and each failed trial costs a revert and a 30 s hold. Default to 4 lanes on wired links, search upward only on proven per-lane stall, drive chunk and bundle from the receiver-reported bottleneck. `engine/crates/ava1/src/governor.rs`. **Risk:** low (pure function with model tests).

### R7. Harden pairing (F2)
- Closes a real MITM path. Wire change, medium effort; fold into the planned Opus re-review. `handshake.rs`, `keys.rs`, `payload/ava1/ava1_noise.c`, SPEC 4.6 and 5.

### R8. Wi-Fi resilience
- Adaptive `dead_after`, lane-only reconnect while control lives, re-send credit on `Resume` and make the engine host honour `Resume` (both listed in CUTOVER). `engine/crates/ava1/src/link.rs`, `session.rs`, `send.rs`.

### R9. Streaming manifest for huge trees
- A 223k-entry manifest is about 20-25 MB and the console must walk and `stat` everything before the first data byte. Allow data for entries in already-sent pages before `ManifestEnd` (the hash is checked at the end). Latency gain, not throughput.

### R10. Record-level sealing (version bump; defer)
- One AEAD tag covers up to 15 MiB, so the receiver buffers the whole frame, verifies, then decrypts, hashes and writes; receive and crypto serialise per lane. 64 KiB sealed records inside a frame would pipeline them and bound memory, at 16 B per record (about 0.02%). Worth doing only if R3 and R4 leave a gap.

### Plan to beat FTX2 everywhere
Tiny upload: R1 + R5. Tiny download: R2. Large: R3 + R4. Then re-run the full table with FTX2 given a final `sync`, reporting time-to-durable for both.

---

## 6. Findings, ranked

| # | Severity | Finding |
|---|---|---|
| F1 | High (performance) | Per-file fsync on the hot path puts AVA1 exactly at the fsync-inclusive file-create ceiling; explains the tiny-upload deficit on `/data` and ext1 and forces fd-holding batches of about 245 files. |
| F2 | High (security) | The six-digit pairing code can be ground by an active MITM: message 3's payload is attacker-controlled and the code has no commitment round (analysis, no PoC). |
| F3 | High (performance, unexplained) | 223k-file upload runs at 82.5 files/s, 3.5x under the drive ceiling and not reproducible on loopback; likely directory contention, syscall count or per-batch overhead. |
| F4 | Medium-High (performance) | Large-file fsync every 250 ms or 64 MiB probably blocks `pwrite` on the same vnode; plausible cause of the 5-8% large-file gap. |
| F5 | Medium-High (performance) | Console sender reads small files on one thread; host receiver fsyncs per file (likely worse on Windows). Matches the tiny-download gap. |
| F6 | Medium (benchmarking) | FTX2 never fsyncs, so the comparison mixes "in cache" with "on disk"; report time-to-durable for both. |
| F7 | Medium (robustness) | `dead_after` 6 s is short for Wi-Fi and a spurious death costs a session. Also open: the Pro outage of 2026-10-03, `Resume` credit not re-sent, zip downloads restart from zero. |
| F8 | Medium (build) | Vendored BLAKE3 C files compile without `-O`; no socket buffer sizing; `poll` plus `send` per segment. |
| F9 | Medium (claim) | "Verified" means verified in transit and against the sender's root; nothing proves what is on disk (read-back happens only after a retried fsync, and a post-write read is served from cache). State this precisely; offer an optional later cold scrub. |
| F10 | Low-Medium (complexity) | Three windows and three ack kinds; the per-lane cap is inert on a LAN; about 27k lines in two languages on a console with kernel-panic history. Keep the fuzz and vector discipline; add a model-checked test of the credit / fd-budget / sync interaction. |

Minor: no peer revocation UI; governor decisions inside measurement noise (R6).

---

## 7. What is good and should be kept

- **Two-level acks** (`Received` early, `Durable` after the chain): frees sender memory without lying about durability; this is what makes the 223k result possible.
- **The durability chain** data, directories, journal, ack, with the rule that a rename is durable only after its directory sync, the transient-fsync retry that refuses to trust a retried fsync without read-back, and never retrying `EIO`. Better than most production tools.
- **Staging with a single rename**, the lock-folder trick, and the `st_dev` guard (`ERR_CROSS_DEVICE`) that encodes the cross-device-rename kernel panic into the protocol.
- **Credit where requeue keeps the charge** (12.3/12.4): the right answer to double-spend and head-of-line cases.
- **Bytes-not-frames liveness**, the rate floor, "a reader never writes", bounded reply queues.
- **Manifest, group CVs and outboards**, CRC'd and compacted journal, `verify` and `skip-existing` policies.
- **Open-file budget probing** (619 measured versus the advertised 13,952) rather than trusting `RLIMIT_NOFILE`.
- **One schema, two generated codecs, shared vectors, fuzzing**, and governors as pure functions tested against models.
- **Noise XX + ChaCha20-Poly1305 with AVX2**, per-join fresh lane keys (a replayed Join never reuses a key), identity per data directory.
- **CUTOVER.md's honesty**: measured tables with ranges, listed regressions, and release gates.

---

## 8. Is anything fundamentally wrong?

No. The architecture is the right shape: well-understood ideas (S3/tus-style resumable ranges, BitTorrent/Bao hash trees, a WireGuard-family handshake, a journaled receiver) fitted to a platform with unusual limits. What I would call design *risks* rather than errors: durability defined per file at the hottest point of the pipeline; a pairing code too short to be a safe comparison without commitment; reliance on kernel behaviours (vnode locking during fsync, cache-flush coalescing) that nobody has measured on 13.60; and implementation size. All are fixable without changing the protocol's structure.

---

## 9. Sources

Retrieved and checked for this review: [18] FreeBSD `sync(2)`, [9] Syncthing BEP v1, [15] Bao repository. The rest are the canonical documents for the named systems, cited from knowledge and **not re-fetched**; confirm section-level claims before quoting them.

1. rsync algorithm: Tridgell and Mackerras, https://rsync.samba.org/tech_report/
2. RFC 9000, QUIC: https://www.rfc-editor.org/rfc/rfc9000
3. RFC 9114, HTTP/3: https://www.rfc-editor.org/rfc/rfc9114 ; RFC 9002, loss detection and congestion control: https://www.rfc-editor.org/rfc/rfc9002
4. IBM Aspera (FASP): https://www.ibm.com/products/aspera
5. UDT: Gu and Grossman, Computer Networks 2007
6. BBR: Cardwell et al., ACM Queue 2016, https://queue.acm.org/detail.cfm?id=3022184
7. BEP 3: https://www.bittorrent.org/beps/bep_0003.html
8. BEP 52: https://www.bittorrent.org/beps/bep_0052.html
9. Syncthing BEP v1: https://docs.syncthing.net/specs/bep-v1.html
10. restic references: https://restic.readthedocs.io/en/stable/100_references.html
11. borg internals: https://borgbackup.readthedocs.io/en/stable/internals.html
12. zstd seekable format: https://github.com/facebook/zstd/tree/dev/contrib/seekable_format
13. tus protocol: https://tus.io/protocols/resumable-upload
14. S3 multipart upload: https://docs.aws.amazon.com/AmazonS3/latest/userguide/mpuoverview.html
15. Bao: https://github.com/oconnor663/bao ; BLAKE3: https://github.com/BLAKE3-team/BLAKE3-specs
16. Noise Protocol Framework rev 34: https://noiseprotocol.org/noise.html
17. WireGuard, NDSS 2017: https://www.wireguard.com/papers/wireguard.pdf
18. FreeBSD sync(2): https://man.freebsd.org/cgi/man.cgi?query=sync&sektion=2
19. ZRTP, RFC 6189: https://www.rfc-editor.org/rfc/rfc6189
