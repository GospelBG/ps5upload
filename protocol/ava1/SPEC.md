# AVA1 wire protocol, version 1 (normative)

AVA1 = Adaptive Verified Assembly. Design rationale: ps5upload-docs
`superpowers/specs/2026-09-30-ava1-transfer-protocol-design.md`. Implementations:
`engine/crates/ava1` (Rust), `payload/ava1` (C). Both must pass `protocol/ava1/vectors/`.

## 1. Transport
TCP, port 9120. A session is one control connection (lane 0) plus up to 8 data
connections (lanes 1..=8). Integers are little-endian.

## 2. Frame header
16 bytes: `"A1"` (0x41 0x31), type u8, flags u8, channel u32, body_len u32,
CRC32C (Castagnoli, reflected, init/xorout 0xFFFFFFFF) of bytes 0..11 as u32.
body_len ≤ 16 MiB (receivers may enforce a lower cap: 64 KiB on control
connections). Bad magic or CRC: close the connection. Flags: bit 0 SEALED (the
body is AEAD ciphertext + 16-byte MAC, §4.4), bit 1 IGNORABLE (a receiver that
does not know the type skips the frame).

## 3. Message encoding
Messages are defined in `schema/ava1.toml` and generated for each language by
`ava1-gen`; never hand-encode. A body is the message's fixed fields in schema
order — u8/u16/u32/u64 little-endian; b16/b32 raw; bytes = u32 length + bytes;
str = u16 length + UTF-8 — followed by an extension block: u16 count, then per
extension u16 tag, u32 value length, value (encoded as a field of its type).
Encoders write extensions in ascending tag order. Decoders skip unknown tags,
reject a repeated known tag, reject invalid UTF-8, and reject trailing bytes
(both in the body and inside an extension value). The canonical encodings in
`vectors/messages.txt` must round-trip byte for byte.

Decoders accept a superset of what encoders write — inside a records item, an
extension may be unknown or out of order, and the decoder drops it — so a
re-encoder is only bound to canonical input, which is all an encoder ever
produces: it must reproduce canonical bytes exactly. For a message it accepted but
would not itself have written, a re-encoder may reproduce the captured item bytes
verbatim (the C decoder holds them) or re-encode each item canonically (the Rust
one does); the two agree wherever the input is canonical, and nothing in the
protocol re-encodes a peer's bytes.

**Records.** A field of type `records` holds a list of one struct — an *item* is that
struct's encoding per this section (its fields, then its extension block), and the
length written before it is exactly that. It is encoded as a
`bytes` field whose content is, for each item in order, `u32le(item length) ‖ item`.
An empty list is `00000000`. A decoder validates every item (a bad item makes the whole
message malformed). Records are not allowed as extensions. C decoders keep a pointer to
the encoded list and its item count; items are read with the generated `ava1_<struct>_next`.

## 4. Keys and sealing
4.1 Identity: a static X25519 key pair per node.

4.2 Handshake: `Noise_XX_25519_ChaChaPoly_BLAKE2b` (Noise revision 34), prologue
"AVA1 v1"; the client is the initiator. Implementations must reproduce
`vectors/noise_xx.json` (from the cacophony set). After message 3, Split() gives
c2s (initiator → responder) and s2c; `h` is the handshake hash. Low-order keys:
an implementation must abort the handshake when a DH result is all zero —
equivalently, when the peer's ephemeral or static key is a low-order point (the C
side checks each DH output; the Rust side checks each received key, since its Noise
library does not). A handshake step that fails poisons the state: no later message
is read or written and no keys are derived from it. Key material is wiped after use.

4.3 Lane keys: lane_key(dir, n, cn, sn) = BLAKE2b-256(key = dir, "AVA1 lane" ‖
u16le(n) ‖ cn ‖ sn), where cn and sn are the 16-byte client and server nonces of
the lane's join (§9). Both are fresh random per join, so a re-join of lane n — or a
replayed Join, even one the server no longer remembers — never gets a key already
used with counters restarted at 0. The control connection is lane 0, keyed once per
handshake, with cn = sn = 16 zero bytes. `vectors/keys.txt` pins these derivations.

4.4 Sealed frames: body = ChaCha20-Poly1305(lane key of this direction, nonce =
4 zero bytes ‖ u64le(counter), AD = header bytes 0..11) followed by the 16-byte
MAC; the counter is per lane and direction from 0. A frame that fails to open
closes the connection.

4.5 Join proofs: BLAKE2b-128(key = BLAKE2b-256(key = dir, "AVA1 join"), m):
the Join tag uses dir = c2s and m = "join" ‖ session_id ‖ u16le(lane) ‖ cn; the
JoinAck tag uses dir = s2c and m = "join-ack" ‖ session_id ‖ u16le(lane) ‖ cn ‖ sn.

4.6 Pairing code: u32le(BLAKE2b-256("AVA1 pairing" ‖ h)[0..4]) mod 10⁶, shown as
six digits. A man in the middle yields different h, so different codes.

## 5. Handshake and pairing
1. Client → `Hs1{noise}` (unsealed): Noise message 1, payload `HelloInfo`
   (version range, caps 0).
2. Server: no common version → `Error(ERR_UNSUPPORTED_VERSION)` unsealed, close.
   Else → `Hs2{noise}`: message 2, payload `ServerInfo` (version, caps, random
   session_id, name).
3. Client → `Hs3{noise}`: message 3, payload `ClientInfo` (name). Both sides
   now key lane 0 (§4.3) and every further frame is sealed. A client that expects
   a particular device (it knows the key it paired with at this address) compares
   the server's static key from message 2 and, if it differs, closes without
   sending `Hs3` — the wrong device never learns the client's key or name.
4. The server learned the client's key in message 3. Unknown key and pairing
   closed → sealed `Error(ERR_PAIRING_CLOSED)`, close. Else → sealed
   `Welcome{knows_you}`.
5. Pairing: while either side does not know the other, both show the pairing
   code (§4.6). After the user confirms, a client whose server sent knows_you = 0
   sends `PairConfirm` (channel = request id); the server answers
   `PairResult{accepted}` on the same channel, accepting only while its pairing
   window is open and its owner approves, then stores the client's key; a key
   that cannot be stored is not accepted. Until accepted, RPCs answer
   `ERR_NOT_PAIRED` and lanes are refused. A client sends nothing but
   `PairConfirm` (no RPC, no Join) while either side is unconfirmed: until the
   user has compared the codes, the server is unverified.
6. Pairing window: opens by itself for 5 minutes after start only while the node
   has no paired peer; otherwise `pairing.open` (method 2, body `PairingOpen`,
   ≤ 600 s) from a paired session opens it. Either kind of window closes as soon
   as one pairing succeeds. A session welcomed with knows_you = 0 that has not
   been accepted ends — sealed `Error(ERR_PAIRING_CLOSED)`, close — when the
   window closes or 60 s after its Welcome, whichever is first. At most 2 such
   sessions exist at a time; a third unknown client gets `Error(ERR_BUSY)` in
   place of Welcome. A node shows at most one pairing request per 10 s, and only
   for a client it has sent Welcome to: a client gone before its Welcome uses none.
7. Peer stores: `<64 hex key> <unix seconds> <name>` per line, ≤ 32 peers (oldest
   dropped), written atomically (temp file + rename in the same directory). A
   missing file is an empty store. A file that exists but cannot be read is not:
   the node runs, knows no peers, logs the failure, never opens its automatic
   window, accepts no pairing, and never writes the file.

5.1 Trust slot: the payload ELF carries a 64-byte array — "AVA1TRUST" (9 bytes),
state (0 empty, 1 key, 2 key + launch token), 6 zero bytes, 32-byte X25519 key,
and a 16-byte launch token in state 2 (§5.2), zero otherwise. An engine sending
the ELF writes its key into the single slot; the payload adds that key to its
peers at startup. Exactly one slot must exist. A slot counts as a slot in states
0 and 1 only while its last 16 bytes are zero, so data that happens to start
"AVA1TRUST" is not one; state 2 is identified by its state byte.

5.2 Launch token: whoever stamps the ELF may also write a fresh random 16-byte
token into it (state 2) and keep it for itself — the reference side stores them in
`<data dir>/ava/launch_tokens`, one `<32 hex token> <unix seconds>` per line, mode
0600, written atomically, at most 32 kept (oldest dropped) and each good for 24 h.
The payload keeps the token in memory only. When the handshake's client static key
equals the slot's key, and the client is one the server knows — a helper whose
peers file could not be written knows nobody and sends no proof — the server adds an
ignorable extension field `launch_proof` to `Welcome`: the first 16 bytes of
BLAKE2b-256(key = the token followed by 16 zero bytes, "AVA1 launch" ‖ h), h
being the handshake hash. No other client gets a
proof, a proof differs every handshake, and the token itself never travels.
`vectors/launch.txt` pins the derivation.

A client that recognises the proof — one of its unexpired tokens, on this
handshake — stores the server's key and treats the session as paired with no
pairing code. A proof counts only from a server it does not already know and
whose `Welcome` says it knows the client (`knows_you` ≠ 0); a server that does not
know the client was never given a proof, so one presenting a proof anyway is not
trusted. A client that does not recognise it (another token, expired, an old proof
replayed under a new h, or a `knows_you` of 0) pairs with the code as usual. The token proves
"this console is the helper I launched" to the side that sent it; an attacker who
read the ELF in transit can forge a proof, but that attacker could have replaced
the ELF outright — it is sent unauthenticated — so the token grants nothing a
pairing code would not, and removes one prompt from the common case.

## 6. Liveness
Every connection sends `Ping{seq, t_us}` every 2 s (default) on channel 0, also
while it is in the middle of reading a large frame; the receiver answers `Pong`
with the same values; the sender's RTT is now − t_us. t_us is taken when the Ping is
written, not when it is queued, so time spent behind other frames is not counted as
round trip. A sender may skip a Ping or Pong while other frames are queued or being
written: they are proof of life too.

Liveness counts bytes, not frames: a connection is dead after 6 s (default,
`dead_after`) with no byte received, so a 16 MiB frame on a slow link is never
mistaken for silence. Silence is judged by what can be read: a reader that was busy
elsewhere (writing, waiting to write) past `dead_after` checks the socket first and
carries on if bytes are waiting; a process that was not running (a late timer tick)
may put off the verdict for at most 2 ticks in a row. Each frame must also move at no less than a rate floor
(default 8 KiB/s) after a `dead_after` grace — its deadline is dead_after +
body_len / floor — so a peer cannot drip one frame forever. Writes obey the same
two limits: a peer that takes no bytes for `dead_after` (it stopped reading), or
takes one frame slower than the floor, has its connection closed. A reader never
waits on a socket write: replies are queued to the connection's writer (bounded),
and a peer whose replies back up is disconnected.

Clocks are monotonic. A server closes a connection whose handshake (first byte
to Welcome) does not finish within the handshake timeout (10 s default). `Bye`
ends a session; `Error` reports why and ends the connection.

## 7. RPC
`RpcRequest{method, body}` on the control connection, channel = request id
(chosen by the client, unique among its outstanding requests). The server
answers `RpcResponse{status, body}` on the same channel; a request that does not
decode is answered `Error(ERR_PROTOCOL)` and closes the connection. status 0 = OK; error
statuses are the `ERR_*` constants. Methods: 1 = node.info → body `NodeInfo`.

## 8. Limits
A server accepts at most 64 connections, 12 from one source address, and 16
sessions (2 of them unconfirmed, §5); past any of these it sends
`Error(ERR_BUSY)` and closes. The accept loop never stops on an accept error.

One session per device: when a client completes a handshake (message 3 proves its
key) while that key still has a session, the older session ends at once — its
control connection and lanes are closed and their per-address counts given back
before the new session's limits are checked. A client reconnecting after its link
died silently is therefore never refused for its own dead connections.

## 9. Data lanes
A client opens lane n (1..=8) by connecting and sending, unsealed,
`Join{session_id, lane_id, client_nonce, tag}` with a fresh random client_nonce
and the Join tag of §4.5. The server refuses (`Error(ERR_BAD_JOIN)`) an unknown
session, a lane id outside 1..=8, a wrong tag, or a client_nonce it has seen in
this session's last 64 joins; and `ERR_NOT_PAIRED` while the session is not
paired. Otherwise it draws a fresh random server_nonce and sends, unsealed,
`JoinAck{lane_id, server_nonce, tag}` (JoinAck tag, §4.5); the client checks the
tag. Both sides then seal everything after with lane_key(c2s|s2c, n, client_nonce,
server_nonce) (§4.3), counters from 0. The client sends a sealed Ping on the lane
as soon as it has checked the JoinAck. A join of a lane id that is still live
supersedes the older connection, but only once the new connection's first sealed
frame has opened under the new lane key (within the handshake timeout): a replayed
Join cannot prove the key and leaves the live lane alone. Lanes end with their
session.
In version 1 project 1, lanes carry only heartbeats; any other frame without the
IGNORABLE flag is answered `Error(ERR_PROTOCOL)` and closes the lane.

## 10. Version 1 scope
Project 1 (this spec): framing, codecs, keys, handshake, pairing, trust slot,
heartbeats, RPC `node.info`, data lanes carrying heartbeats only. Not yet in
version 1: data frames, jobs, journals, resume, bundles (project 2); Ed25519
signing keys and tickets, cross-network encryption, session parking (project 2);
management RPCs replacing FTX2 (project 3). Unknown frame types on a control
connection are a protocol error; new frame types require a version bump or a
negotiated `caps` bit.

## 11. Jobs and manifest

11.1 Every transfer is a job with a 16-byte `job_id`, chosen by the node that opens it
(the engine uses the HTTP API's `tx_id`). A job is bound to the static key of the peer
that opened it; only that key may resume or cancel it. Every data-plane message
(types 0x20–0x3F) has `job_id` as its first field.

11.2 Paths in a manifest are relative to the job root: UTF-8, '/'-separated, at most
1024 bytes, no empty, "." or ".." component, no NUL, no leading '/'. A receiver refuses
a manifest with any other path (`ERR_PATH`) before touching the filesystem.

11.3 Opening (`kind` as seen by the node that receives `JobOpen`):
- `JOB_UPLOAD`: opener → `JobOpen`; receiver → `JobOpenAck{credit, staged}`; opener →
  `ManifestPage`* → `ManifestEnd`; receiver → `JobMap`; then data.
- `JOB_DOWNLOAD`: opener → `JobOpen{root = source, ext credit}`; the other node becomes
  the sender: `JobOpenAck`, `ManifestPage`* → `ManifestEnd`; opener → `JobMap`; then data.
- `JOB_COPY` runs on one node (`job.copy` RPC, §7).
Entries are numbered 0.. in manifest order (`file_id`); directories are entries with
`kind = ENTRY_DIR`. `manifest_hash` = BLAKE3 over, for every entry in order,
`u32le(len) ‖ encoding of the entry without its ext`.

11.4 Policies: `replace` (send everything not in the map), `skip-existing` (the receiver
marks a file done when a file of the same size and mtime seconds exists), `verify` (the
sender puts each file's root in `ext root`; the receiver marks a file done when an
existing file of the same size hashes to it).

11.5 A `JobOpen` for a job the receiver already knows is a resume: the receiver matches
the new manifest against its stored one by path, keeps the progress of entries whose
size and mtime are unchanged, and restarts the others (never splicing old and new bytes).
`Resume{job_id, manifest_hash}` is the fast path when the sender still holds the same
manifest: the receiver answers `JobMap`, or `JobMap{status = ERR_UNKNOWN_JOB}` and the
sender falls back to `JobOpen`. A map larger than one control frame is sent as several
`JobMap` pages; `last = 1` marks the final one. `Durable` is never paged: each is complete.
Engines reopen with `JobOpen` after any interruption; `Resume` is optional for senders that
keep their manifest and credit state. Either way the sender's credit starts again from the
grant in that session's answer (`JobOpenAck.credit`); nothing outstanding carries across a
reconnect.

11.6 Staging: when the job root does not exist, the receiver writes the whole tree under
`<root>.ava-part/` and, after the last file, renames it to `<root>` (same parent, `st_dev`
checked). When the root exists, files are written in place; large files through
`<name>.ava-part` and a same-directory rename. `JF_SINGLE_FILE` writes `<root>.ava-part`. A staging
receiver takes `<root>` with `mkdir` before it journals the job (an existing `<root>` then
refuses it, `ERR_EXISTS`) and records that in `JnlOpen.staged` bit 1, so on resume the empty
`<root>` is its own; the final rename replaces only that empty folder (not empty: `ERR_EXISTS`).

## 12. Data frames and credit

12.1 `Chunk` and `Bundle` travel on lanes; everything else on the control connection.
The header `channel` of a lane data frame is the sender's per-job sequence number.

12.2 `Chunk.offset` is a multiple of 1 MiB (one verification group, §13); its length is
a multiple of 1 MiB unless the chunk ends the file. A file is a *large* file when its
size is at least `LARGE_CUTOFF` (256 KiB), a protocol constant both sides use (`JobOpen`
carries no cutoff); smaller files travel whole, as `BundleRecord`s. A receiver ends the job
with `ERR_PROTOCOL` on a `Chunk` for a small file or a `BundleRecord` for a large one.

12.3 `Received{lane, seq}` is sent as soon as the receiver has a lane frame in memory,
before any disk work. A sender requeues, on any lane, the frames of a lane that closed
before they were `Received`. Applying a frame twice is harmless. A lane's death releases
window credit only for frames that provably never left the sender: frames the lane's writer
never dequeued (still queued when the writer is confirmed dead) are dropped and their bytes
returned to the window. A frame the writer may have put on the wire stays charged until the
receiver accounts for it (its `Credit` after the apply) or the job ends — releasing those
would let the sender spend the same window twice, and the receiver's `ERR_CREDIT` (12.4)
would fail a healthy job.

12.4 Credit: `JobOpenAck.credit` (uploads) or `JobOpen.ext credit` (downloads) is the
number of lane-frame body bytes the sender may have outstanding; `Credit{bytes}` returns
space as the receiver frees buffers. A receiver that sees its credit exceeded sends a
sealed `Error{ERR_CREDIT}` on the offending lane and ends the job: the C receiver closes
that one lane itself, and a transport without a server-side lane close lets the session's
own lifecycle end it — the sender observes the same either way: its lane dies. A sender
never sends a piece larger than the credit already granted: pieces are sized at read time
to fit the window (whole verification groups, one group minimum; a file's final piece
keeps the whole-file rule), and a window that cannot hold one group ends the job
(`ERR_PROTOCOL`) instead of stalling.

12.5 Per lane, the sender keeps at most `max(chunk size, lane rate × 2 s)` bytes sent
and not yet `Received`.

12.6 `Durable{files, ranges}` follows the order: data synced → journal appended and
synced → `Durable`. `JobDone` follows the last durable commit (and the staging rename).
A failure after every byte is durable (`ERR_EXISTS`, `ERR_CROSS_DEVICE` on the final
rename) is reported in `JobDone` and never causes a resend. A rename is durable only once its
directory is synced: the receiver fsyncs the parent directory after every commit or staging
rename, before it journals that commit. Likewise for new names: before a batch is journaled, every directory
that gained a file in it is synced once, after the file data.

## 13. Verification

13.1 A file's root is its standard BLAKE3 hash. Files are hashed in groups of 1 MiB
(`GROUP_SHIFT` = 20): group i covers bytes [i·2^20, (i+1)·2^20). For a file of two or more
groups, each group's chaining value (BLAKE3 `finalize_non_root` of that subtree) is
computed where its bytes are, and the root is merged from the group CVs along BLAKE3's
tree (left subtree = the largest power of two of groups strictly less than the count);
every merge is a non-root parent compression except the last, whose compression carries
BLAKE3's ROOT flag. A file of zero or one group: root = BLAKE3(bytes).

13.2 Senders compute the root while reading. Small files carry it in their
`BundleRecord`; large files send `FileRoot` once their last group is read (or, on a
resume, once all group CVs are known from the sender's outboard).

13.3 Receivers compute each group's CV from the bytes they write and store it in an
outboard (32 bytes per group, in the job directory). At commit the root merged from the
outboard must equal the sender's root; otherwise the file is reset and `FileRetry` sent.

13.4 Resume verification: before answering a resumed job's map, the receiver re-hashes
each partial file's groups from its last durable batch and compares them with the
outboard; a mismatch drops those ranges from the map. Older durable groups are trusted
from the journal. The verify policy re-hashes whole files.

## 14. Journal and resume

14.1 The journal: `<job dir>/journal` = magic `AVA1JNL1` (8 bytes), then records
`u32le(len) ‖ u8 kind ‖ body ‖ u32le(crc32c(kind ‖ body))`, `len` = 1 + body length. Kinds:
1 `JnlOpen`, 2 `JnlBatch`, 3 `JnlReset`, 4 `JnlSnapshot`, 5 `JnlDone` (bodies are the generated
structs). Replay stops at the first record whose length runs past the file, whose CRC fails, or
that the visitor refuses; the file is truncated there before the next append. Every append is
followed by `fsync`. When the file passes 1 MiB it is compacted: `journal.tmp` =
magic + `JnlOpen` + `JnlSnapshot(current state)`, `fsync`, `rename` over `journal` (same
directory), `fsync` of the directory. The job's manifest is stored once as `<job dir>/manifest`:
exactly the content of a `records(ManifestEntry)` field (written as `manifest.tmp` → `fsync` →
rename).

14.2 State: a `JnlBatch` marks files done (dropping their ranges), adds durable ranges and
records roots; `JnlReset` forgets one file; `JnlSnapshot` replaces the whole state; `JnlDone`
records the job's final status. The map answered for a resume is the replayed state (done files;
durable ranges of the others), after the check in §13.4.

14.3 Location: the console keeps job directories under `/data/ps5upload/ava/jobs/`, an engine
under `<data dir>/ava/jobs/`. Each holds `journal`, `manifest` and one `<file_id>.ob` outboard
per large file. A directory is removed 7 days after its last write (directory or journal mtime),
on start.

## 15. Apply (receivers)

15.1 Directories first: every directory entry of the manifest is created before any file data
is applied, and the directories a batch created are fsynced before that batch is journaled. A
file whose parent is not a manifest entry still gets its parents created when it is opened.

15.2 Small files (a `BundleRecord` chunk): `open(O_WRONLY|O_CREAT|O_TRUNC|O_NOFOLLOW)` → write →
mode → mtime. The descriptor stays open until a sync batch covers the file; a duplicate record
for a file that is already durable or already pending is dropped without truncating it. A
record whose length disagrees with the manifest is answered with `FileRetry` reason
`RETRY_CHANGED`, and one whose BLAKE3 disagrees with the root it carries with `RETRY_VERIFY`;
the record is not applied and the job continues.

15.3 Large files: `pwrite` at the chunk's offset into the part file (preallocated when new),
group CVs into the outboard. Bytes are durable after a sync batch. When every byte is durable
and the root merged from the outboard equals the sender's `FileRoot`, the file commits: mode →
truncate to size → mtime → fsync → a same-device check (§12.6) → a rename in the same directory
→ an fsync of that directory. The part file of a single-file job is `<root>.ava-part`; in a
merge it is `<path>.ava-part` beside the final file; in a staged tree (§15.5) it is the file's
final relative path inside the staging tree, so the file's own commit renames nothing. A part file or
outboard that is missing at commit time is a reset (`FileRetry` `RETRY_IO`), never a new empty
file.

15.4 Sync batches: every 250 ms, or after `batch_max` small files (tuned 16–512, starting 256:
halved when a batch takes over 1.5 s, doubled when under 0.5 s) or 64 MiB of large-file bytes.
Data fsyncs run in parallel on the workers, then the new directories are fsynced, then the
batch (`JnlBatch`) is appended and the durable ranges are reported. A stop in the middle of a
sync journals and acknowledges nothing.

15.5 Staging and merge: a new destination is staged — the tree is written to `<root>.ava-part`,
a sibling of the destination, while `<root>` itself is created as an empty lock folder; at the
end the finished tree is renamed over that empty folder (never over content). If the
destination appeared in the meantime the job ends `ERR_EXISTS` and the files stay in
`<root>.ava-part`. A merge into an existing folder refuses a manifest directory that is a
symbolic link or not a directory (`ERR_PATH`), and a file already where one must go ends the
job with `ERR_EXISTS`.

Version 1 has two walk modes and they are deliberately different. The sender/download mode
follows directory symlinks (the engine walks the same tree the same way); a symlink cycle is
an error, never a spin, and a dangling link — the link's target cannot be stat'd — is an error
in both modes. The console-local copy/move mode skips directory symlinks: the copy writes into
a namespace it must not be able to be led out of by the source tree. Both are contracts, not
bugs; the sender's descend behaviour is not normative for the copy path.

A copy (`job.copy`) is a receiver job whose sender is the in-process reader, so this section
applies to it unchanged. A finished local job stays listed for the park age, so `job.status`
keeps answering for it.

`job.copy` uses the source and destination paths, a stable job id, and flags including
`JF_OVERWRITE` and `JF_MOVE`. Without `JF_OVERWRITE`, an existing destination root is refused
with `ERR_EXISTS`; with it, colliding files are replaced and destination-only files remain.
A move deletes source entries only after the destination rename, parent directory sync and
successful Done journal append. It checks each source file against the manifest before unlinking
and reports any paths left behind as a failed job; `job.status` stays running during deletion.

## 16. Governor

The sender and the receiver each keep one small control loop; both are pure functions of the
numbers they are fed, so both are tested against models rather than sockets.

- Lanes: add one while the bottleneck is the network and the last addition raised throughput by
  ≥ 10 %; otherwise revert it and hold for 30 s. A tick with a lane death or requeue drops one
  lane (min 1) and halves the chunk. At most 8 lanes: on a link that scales past that the count
  simply stops growing.
- Chunk: 1–15 MiB in whole groups; halved on a stall, doubled after 10 stable ticks; never more
  than half a second of one lane's throughput (min 1 MiB). 15 MiB, not 16: the frame cap (§2)
  counts the header and the MAC, which a 16 MiB body would not fit under.
- Bundle target: a quarter second of one lane's throughput, clamped to 256 KiB–15 MiB. It moves
  with that rate; the effect is that it grows while the network is the limit and shrinks when
  the rate falls (a receiver whose workers wait shows up as a receiver-reported bottleneck, §16.9
  — the target itself is not fed by worker pressure).
- In-flight cap per lane: `max(chunk, lane rate × 2 s)` — the same bound §12.5 states.
- Mixing check: once per job — after 3 warm-up ticks, if both classes still have work queued,
  the sender probes 5 s mixed, 5 s stream-only, 5 s bundle-only, then picks sequential if its
  estimated finish time is < 90 % of mixed. Sequential runs bundles first. The choice and reason
  go into `Status.sequential`. The probe never runs twice.
- Priority: beyond the bundle floor, the class with the longer estimated remaining time is
  preferred. The small/large cutoff is the protocol constant `LARGE_CUTOFF` (§12.2); it is not
  governed in project 2 — the design spec's 64 KiB–4 MiB auto-tuning is deferred.
- Receiver workers: every 2 s; add one while work is queued and the last addition raised
  files/s by ≥ 10 %; revert and hold 30 s otherwise; release one after 3 idle steps; range
  2–16, start 4.
- Bottleneck: source starved → `BN_SOURCE`; credit starved → the receiver's reported bottleneck
  (`BN_DISK`/`BN_WORKERS`), else `BN_CREDIT`; otherwise `BN_NETWORK`.

16.9 Status: the receiver sends `Status` (IGNORABLE) every 250 ms while a job is open: files
and bytes done, durable bytes, its own bottleneck (`BN_DISK` when workers are at their maximum
or adding one did not help, `BN_WORKERS` while it is still adding, `BN_NETWORK` when its queue
ran dry), workers, lanes, and whether it runs sequential. The engine shows the sender's
bottleneck, which already folds in the receiver's.

16.10 disk.calibrate: method 19 accepts `DiskCalibrate` and returns `DiskCalibrateResult` with
measurements at 1, 2, 4, 8 and 16 workers. The request allows at most 20,000 files of at most
1 MiB each, and `dir` must pass the node's write policy. The answer is a hint for the engine's
starting worker count, never a contract. The node deletes every file and directory it created.
