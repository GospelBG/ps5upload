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
   session_id, name). `caps` bit 0 is `CAP_DATA_PLANE` (1): the node hosts the jobs
   of §11–§16. A client sends no data-plane frame and no method 16–19 request to a
   node that did not advertise it.
   Bit 1 is `CAP_MGMT` (2): the node serves the management methods of §7.3 (numbers 4 and
   up other than 16–19). A client routes management calls by this bit instead of probing
   for `ERR_UNKNOWN_METHOD`; a node that does not advertise it answers
   `ERR_UNKNOWN_METHOD` to them. The payload advertises it when its management table is
   installed (`mgmt_rpc_installed()`); the Rust `Session::has_mgmt()` reads it.
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
   user has compared the codes, the server is unverified. A data-plane frame (§11–§16) on a
   control connection whose pairing is not accepted is a protocol error: the server answers a sealed
   `Error(ERR_NOT_PAIRED)` and closes the connection.
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
decode is answered `Error(ERR_PROTOCOL)` and closes the connection. status 0 (`STATUS_OK`) = OK;
error statuses are the `ERR_*` constants, and an error response's body is the cause as
UTF-8 text (not an encoded message). An unpaired session's RPCs answer `ERR_NOT_PAIRED`; a
session has at most 8 requests in flight (§7.4) and the next one answers `ERR_BUSY`. Only
`pairing.open` is answered on the reader; every other method runs on a worker, so a slow method never
delays liveness.

7.1 Methods:

| # | name | request body | response body (status 0) |
|---|------|--------------|--------------------------|
| 1 | `node.info` | empty | `NodeInfo{version, platform, name}`, ext `firmware` |
| 2 | `pairing.open` | `PairingOpen{seconds}` (≤ 600) | empty (§5.6) |
| 3 | `crypto.bench` | `CryptoBench{mib}` | `CryptoBenchResult{bytes, micros}`, ext `open_micros`, `backend` (a diagnostic) |
| 16 | `job.copy` | `JobCopy{job_id, src, dest, flags}` | `Status` (§16.9), ext `state` |
| 17 | `job.status` | `JobRef{job_id}` | `Status`, ext `state` |
| 18 | `job.cancel` | `JobRef{job_id}` | empty |
| 19 | `disk.calibrate` | `DiskCalibrate{dir, files, size}` | `DiskCalibrateResult` (§16.10) |
| 4–141 | management methods | see §7.3 and `MGMT_METHODS.md` | see §7.3 |

`Status.state` is 0 while the job runs, 1 when it finished OK and 2 when it failed (the cause is
in ext `current`, the `ERR_*` code in ext `code`; a finished `job.run` job's output is in ext `result`). Methods 16–19 are the version 1 data-plane RPCs and exist only on a node that
advertises `CAP_DATA_PLANE`; the management methods are §7.3 and exist on a node that advertises `CAP_MGMT`. The behaviour of 16–18 is §15.5; of
19, §16.10.

7.1.1 `job.run` and `job.list` (long management operations). `job.run{job_id, op, args}` starts operation
`op` (`JOB_OP_*`: DELETE 1, CHMOD_R 2, HASH 3, CRC32 4, FSCK 5, BACKUP_SNAPSHOT 6, BACKUP_RESTORE 7,
CLEANUP 8, SDK_SCAN 9) on its own worker thread (the 512 KiB management stack) and answers at once with a
`Status` (state 0). `args` is the operation's request body, the same legacy JSON the FTX2 frame carried
(at most 60 KiB). The job is an entry of the job table (counted against the 32-job limit, owned by the
peer that started it, no session, so a reconnect does not matter), at most 8 operations run at once
(`ERR_BUSY` for a ninth) and an unknown `op` is `ERR_PROTOCOL` (`unknown_op`).
* `job.status` returns the progress (`files_done/total`, `bytes_durable/total`: files are non-directories,
  bytes the regular files' sizes; both totals are 0 while unknown), the current step in ext `current`, and,
  once finished, `state` 1 with ext `result` (the operation's reply body, at most 128 KiB) or `state` 2 with
  ext `code` (the `ERR_*`) and the cause token in `current`. A repeat of `job.run` with the same id and the
  same owner, op and args answers the job's status whatever state it is in (nothing runs twice); other
  parameters are `ERR_PROTOCOL`, another owner `ERR_UNKNOWN_JOB`.
* `job.cancel` raises the job's cancel flag and waits up to 2 s for the worker. A delete, chmod, hash,
  crc32 or backup stops at the next directory entry or read block and the job ends `state 2`,
  `ERR_CANCELLED`; fsck, cleanup and sdk.scan are one system call and only honour a cancel that arrives
  before they start. Unlike a copy, a cancelled operation stays listed (finished) so a poller reads how
  it ended; it is collected a park age after it ended, like any finished job.
* An operation that wraps an FTX2 handler (fsck, backup, cleanup, sdk.scan) keeps the handler's
  `{"ok":false,...}` body as its result: the operation ran and the body is the answer. Only an ERROR
  frame is a failed job. DELETE refuses a path outside the writable roots and a mount point (a path on
  another device than its parent): `ERR_PATH`, `fs_delete_path_not_allowed` / `fs_delete_path_is_mount_point`.
* `job.list` returns the peer's jobs of every kind as `JobListResult` (`JobEntry.kind` 1 upload, 2
  download, 3 copy, 4 operation).

7.2 Error codes. The numbers below are generated from `schema/ava1.toml`, whose constants are the
normative table; the second column names the constant in the generated code.

| code | name | sent when |
|------|------|-----------|
| 1 | `ERR_NOT_PAIRED` | an RPC or a lane join from a session whose pairing is not accepted (§5) |
| 2 | `ERR_PAIRING_CLOSED` | an unknown client while the pairing window is closed; an unconfirmed session whose window or 60 s ended |
| 3 | `ERR_UNSUPPORTED_VERSION` | the version ranges of the two peers do not overlap (unsealed, §5) |
| 4 | `ERR_PROTOCOL` | a frame or body that does not decode, a frame type not allowed where it arrived, a `JobOpen` with an unknown kind, policy or flags, a `Chunk` for a small file or a `BundleRecord` for a large one (§12.2), a window that cannot hold one group (§12.4) |
| 5 | `ERR_BAD_JOIN` | an unknown session, lane id outside 1..=8, wrong tag or replayed nonce (§9) |
| 6 | `ERR_UNKNOWN_METHOD` | `RpcResponse` status for a method the node does not implement, including methods 16–19 on a node without the data plane |
| 7 | `ERR_INTERNAL` | the node could not do what the peer asked for a reason that is neither the peer's nor the disk's: out of memory, a thread that would not start |
| 8 | `ERR_BUSY` | a limit of §8 or §11.7: connections, sessions, an unconfirmed-session slot, in-flight RPCs (8 per session, §7.4), jobs, a destination another job is writing, no buffer budget left for another job |
| 9 | `ERR_PATH` | a manifest or RPC path that breaks §11.2, a root the node's write or read policy refuses, a source that cannot be stat'd or a staging parent that is not a directory |
| 10 | `ERR_NO_SPACE` | the destination drive is full (`ENOSPC` while writing) |
| 11 | `ERR_UNKNOWN_JOB` | `Resume`, `job.status` or `job.cancel` for a job the node does not list, or lists for another peer key (the two are not told apart) |
| 12 | `ERR_IO` | a disk or filesystem failure on the node's side: write, fsync, rename, journal append, reading a source, a failed `disk.calibrate` |
| 13 | `ERR_VERIFY` | a file or copy whose bytes do not match their root and that a `FileRetry` cannot fix |
| 14 | `ERR_EXISTS` | a destination that is already there and may not be replaced: the root without `JF_OVERWRITE`, a staging root that appeared meanwhile, a file where one must go in a merge |
| 15 | `ERR_CANCELLED` | the job was cancelled (`job.cancel`, or a `JobCancel` carrying this reason) |
| 16 | `ERR_CROSS_DEVICE` | a staged or part-file rename whose two sides are on different devices (`st_dev`); never attempted, because a cross-device `rename` panics the console's kernel |
| 17 | `ERR_CREDIT` | a lane frame larger than the credit the receiver granted (§12.4) |

7.3 Management methods (the console operations FTX2 carried on :9114). Numbers are assigned by
block; the tracked list, one row per FTX2 frame with its payload handler and engine caller, is
`MGMT_METHODS.md`. The generated constants `METHOD_*` in `schema/ava1.toml` are normative.

| numbers | block | bodies |
|---------|-------|--------|
| 4–11 | node and diagnostics: `node.status`, `node.shutdown`, `node.cleanup`, `log.klog`, `log.syslog`, `net.interfaces`, `net.reach`, `net.speedtest` | `node.status` replies `NodeStatus`; the rest `MgmtText` |
| 20–21 | `job.run`, `job.list` | `JobRun{job_id, op, args}` → `Status` (ext `state`, `result`, `code`); `job.list` → `JobListResult` |
| 32–43 | filesystem: `fs.volumes`, `fs.list`, `fs.stat`, `fs.mkdir`, `fs.rename`, `fs.chmod`, `fs.read`, `fs.write`, `fs.mount`, `fs.unmount`, `fs.mount_pkg`, `fs.mount_lwfs` | `fs.list`, `fs.stat`, `fs.mkdir`, `fs.rename`, `fs.chmod`, `fs.read`, `fs.write` are typed (`FsList` → `FsListResult`, `FsPath` → `FsStat`, `FsMkdir`, `FsRename`, `FsChmod`, `FsRead` → `FsReadResult`, `FsWrite`); the others `MgmtText` |
| 48–61 | apps, launch, install queries, processes | `MgmtText`; `app.list` pages with `offset`/`limit` and `more` (§7.4, the only text method that does not fit one reply) |
| 64–70 | saves, screenshots, videos, search index | `MgmtText` |
| 72–87 | hardware, power, time, peripherals, `shell.exec` | `MgmtText` |
| 88–100 | profiles, users, backups (97 and 99 are unassigned: backup snapshot and restore run as `job.run` ops) | `MgmtText` |
| 104–128 | cheats, SMP metadata, SDK changer, TMDB, FTP, firmware spoof, notifications, activity | `MgmtText` |
| 136–141 | Remote Play | `MgmtText` |

A `MgmtText` body is the payload handler's existing request or reply (UTF-8 text, in practice
JSON), carried unchanged in `MgmtText.body`; `more = 1` on a reply means the method is paged and
the caller asks again with the next `offset`. Typing the text methods is deferred (§10): the text
bodies are stable and tested, and the cutover does not need them typed.

`log.klog` and `log.syslog` are clamped tails, not paged reads: when the console's text is longer than
`RPC_TEXT_MAX` the reply is its newest `RPC_TEXT_MAX` bytes, starting at a line boundary (else a
UTF-8 boundary), with `more = 1` meaning "older text was left out"; a text that fits is returned whole
with `more` absent or 0. `net.reach` is a probe: its negative answer (`{"ok":false,"timed_out":..,
"errno":..,"ms":..}`) is the measurement and travels as an ordinary OK reply; only a malformed request
(`bad_request`, `bad_address`) is `ERR_PROTOCOL`. A console writes a human-readable job event log at
`/data/ps5upload/ava/events.log` (one line per job open, resume, done and fail with status, bytes,
files and lanes; 1 MiB, rolled to `events.log.old`), read with `fs.read` like the other log files.

Encoding overhead. A `MgmtText` is `u32 length + text + u16 ext count` (6 bytes), plus 7 bytes when
`more` is present (tag u16, length u32, value u8). It is the `RpcResponse` body, so the largest text
a handler may return is `RPC_REPLY_MAX - 16 = 262,128` bytes (`RPC_TEXT_MAX`, with 3 bytes to spare);
a text request is bounded the same way by 56 KiB. Typed bodies carry their own overhead
(`FsReadResult` is `data + 7`).

Errors: the response status is an `ERR_*` code (§7.2) and the body is the cause as UTF-8. A ported
handler's cause is its legacy token (`fs_move_cross_mount`, `cleanup_path_denied`, ...), so the
engine can build the same `payload rejected <LABEL>: <cause>` text FTX2 callers produced. No new
error codes were added for management methods. `fs.rename` answers `ERR_CROSS_DEVICE` when the
source and the destination's parent are on different devices (`st_dev`); it never calls `rename(2)`
across devices.

Legacy failure bodies. Many FTX2 handlers answered a failure as a *successful* frame with a
`{"ok":false,"err":"..."}` body (`handle_fs_write_bytes`, `handle_net_reach`, `handle_toast_send`,
the TMDB, SDK and cheats handlers, ...). A ported handler never does that: it answers an `ERR_*`
status (the closest of §7.2; `ERR_INTERNAL` when none fits) with the legacy token as the cause, and
`STATUS_OK` only when the operation succeeded. A body that still contains `"ok":false` under
`STATUS_OK` is a porting bug. Where the legacy body also carried data on failure (a partial list,
a detail object), the cause is that body's `err` token and the data is dropped.

Truncation. A ported handler must detect truncation and fail loudly. Every `snprintf` into a
reply buffer is checked (`n < 0` or `n >= cap` is an error, never clamped to `cap - 1` and sent), a
clamped read (`klog`, `syslog`, `fs.read`) reports a short read as a short read (`eof`, `more`), and
a buffer that cannot hold the whole answer answers `ERR_INTERNAL` with the cause `reply truncated`.
The payload helper is `ava1_rpc_text(out, cap, &out_len, fmt, ...)` (`ava1_data.h`): it returns
`STATUS_OK`, or `ERR_INTERNAL` with that cause, so a handler returns it directly. The harness pins
it (`ava1_rpc_text_answers_ok_when_it_fits_and_internal_when_truncated`) and the server answers
`ERR_INTERNAL` ("reply exceeds the 256 KiB RPC cap") for a handler that claims more than the cap.

Threads. The payload runs a management RPC (method 4 and up, except the data plane's 16-19) on a worker
with a 512 KiB stack (`AVA1_MGMT_STACK`, the FTX2 management thread's size); every other RPC keeps the
256 KiB `AVA1_THREAD_STACK`. Before each handler the worker re-applies the credential elevation and sets the
in-flight frame marker to the handler's legacy FTX2 frame number (the crash breadcrumb), and clears it
after. A ported handler still keeps stack buffers small: **no stack array of 16 KiB or more reachable from
a table handler**, enforced by `payload/tools/mgmt_audit.py stack` (run by the `ava1-ctest` test
`c_mgmt_handlers_never_read_the_socket_and_keep_small_stacks`), and the handler must not read `client_fd`
(it is called with -1 behind a capture sink, `payload/src/mgmt_rpc.c`). `mgmt_audit.py report` lists every
array of 2 KiB or more per handler (it is a tripwire, not a proof: it reads C text, follows calls and function-pointer arguments by name, sizes struct elements as a lower bound and unknown element types at 8 bytes, and cannot see calls through tables or dlsym; `mgmt_audit.py selftest` pins the shapes it must catch); Tasks 5 and 7 must heap-allocate the 64 KiB buffers it shows in
`handle_crc32_file`, `handle_shell_builtin` and `copy_file` before routing those handlers.

7.4 RPC limits. A session has at most **8** requests in flight; the ninth answers `ERR_BUSY`
(earlier drafts said 4). A request body is at most **56 KiB** and a reply body at most **256 KiB**,
both enforced by the server (`RPC_REQUEST_MAX`, `RPC_REPLY_MAX`): a larger request answers
`ERR_PROTOCOL` with the cause `request exceeds the 56 KiB RPC cap` and the session continues; a
handler that returns more than 256 KiB is answered `ERR_INTERNAL` with the cause `reply exceeds the
256 KiB RPC cap` rather than clipped. The control connection's frame cap is 64 KiB while a session
is being set up; once the handshake is done the client accepts replies up to
`RPC_REPLY_MAX + RPC_FRAME_SLACK` (1 KiB for status, length, extension count and the AEAD tag). The
worst case per session is 8 × 256 KiB = 2 MiB of reply buffers on the node (heap, per call). A
method whose reply can exceed the cap takes `offset` and `limit` and sets `more`; no management
method may return a larger body. `MGMT_METHODS.md` lists today's largest reply of every method and
says which fit and which page.

The engine side (a Task 4 requirement, not current behaviour): the engine's gate holds 6 permits per
console and reserves the other 2 for `node.status`, `job.status` and `job.cancel`, so a flood of
slow calls never hides a cancel or liveness; it retries `ERR_BUSY` with backoff (3 tries) and never
reports it as "payload failed".

7.5 Chunked and bounded filesystem calls.

`fs.read` (`FsRead{path, offset, len, flags}` -> `FsReadResult{data, eof}`): `len` is at most
`FS_READ_MAX = RPC_REPLY_MAX - 16 = 262,128` bytes (the reply is `data + 7`). A shorter reply with
`eof = 1` means the end of the file; `eof = 0` with fewer bytes than asked means the node chose a
short read, and the caller continues at `offset + data.len()`. A caller that needs more than
`FS_READ_MAX` (FTX2 allowed 2 MiB per call) loops until `eof` or the byte count it wanted, and the
core wrapper `fs_read_with_timeout` does that for every caller. Callers that can ask for more than
the cap: `ps5upload-engine/src/lib.rs:3974`, `ps5upload-engine/src/fakelibs_api.rs:423`,
`ps5upload-core/src/fs_ops.rs:1703`, `ps5upload-core/src/smp_image_rw.rs:174`,
`ps5upload-core/src/smp_checkout.rs:185`. Existence tests by 1-byte `FsRead` that become `fs.stat`
(Task 4): `lib.rs:4033`, `smp_checkout.rs:456`, `smp_image_rw.rs:229`, `fakelibs_api.rs:390` and
`fakelibs_api.rs:594`.

`fs.write` (`FsWrite{path, offset, flags, data}`, ext `mode`): FTX2 wrote up to 256 KiB atomically,
and the request cap is 56 KiB, so a larger file is written in chunks of at most
`FSW_CHUNK_MAX = 48 KiB` (49,152) of `data` (the rest of the request is the path, the header and
the extension). Flags: `FSW_APPEND` (write at the end of the temporary file, `offset` ignored),
`FSW_AT_OFFSET` (write at `offset`; a missing temporary file is created empty), `FSW_COMMIT`
(after this chunk: fsync the temporary file, then `rename` it over `path`), `FSW_CREATE` (at commit
fail with `ERR_EXISTS` when `path` exists) and `FSW_OVERWRITE` (replace it; the default when neither
is set; both set is `ERR_PROTOCOL`). Neither `FSW_APPEND` nor `FSW_AT_OFFSET` means "the whole file in
one call": `offset` must be 0, the file is written to the temporary file and committed in the same
call, exactly FTX2's atomic small write (`COMMIT` is implied). Chunked protocol: the temporary file
is `<path>.ps5upload.tmp` in the same directory as `path` (so the commit rename never crosses a
device, with the `st_dev` guard of `fs.rename`); the caller sends chunks with `FSW_AT_OFFSET`
(or `FSW_APPEND`) in order, the last one also carrying `FSW_COMMIT`. A caller that gives up
deletes the temporary file (`fs.rename` is not needed; `job.run` DELETE removes it). A chunk at offset 0 (or the first `FSW_APPEND`) truncates an abandoned temporary file first, so a retry
starts clean. `mode` (ext 1, the
permission bits applied at commit; absent = 0644) is optional. Callers that need chunking because
they write more than 48 KiB: `ps5upload-core/src/cheats.rs:701`, `ps5upload-core/src/profile.rs:664`,
`ps5upload-core/src/smp_image_rw.rs:158` (the others, `smp_checkout.rs` and `smp_image_rw.rs:305/339`,
write small state files). The core wrapper `fs_write_bytes` chunks transparently.

Typed bodies decoded by the adapters: `NodeStatus.ucred_elevated` is a `u8` on the wire; the engine
adapter restores the JSON boolean the client reads (`true`/`false`) and rebuilds the legacy
`/api/ps5/status` object (Tasks 3 and 4). `NodeStatus.prior_instance` is one of `clean`,
`killed_externally`, `wedged`, `stale` or `replaced` (the values of `instance_verdict_name`).
`FsEntry.kind` and `FsStat.kind` are `ENTRY_FILE` (0), `ENTRY_DIR` (1), `ENTRY_LINK` (2, a symbolic
link, not followed), `ENTRY_OTHER` (3, a device, socket or fifo) or `ENTRY_UNKNOWN` (4, the node could
not stat the entry; FTX2 said `"other"`). `FsListResult` carries no `path` and no returned-entry
count (FTX2's reply had both); the adapter reconstructs `path` from the request and the count from
`entries.len()`.

## 8. Limits
A server accepts at most 64 connections, 12 from one source address, and 16
sessions (2 of them unconfirmed, §5); past any of these it sends
`Error(ERR_BUSY)` and closes. The accept loop never stops on an accept error.

One session per device: when a client completes a handshake (message 3 proves its
key) while that key still has a session, the older session ends at once — its
control connection and lanes are closed and their per-address counts given back
before the new session's limits are checked. A client reconnecting after its link
died silently is therefore never refused for its own dead connections.

The key is the identity, not the process: two engines (two processes, two computers, a
desktop app and a Docker engine) that share one identity file are one device to the
console, and each new handshake ends the other's session and its jobs' lanes. They keep
evicting each other, and each sees its session end for no visible reason. Every engine
therefore needs its own identity (its own data directory). A client can recognise the
condition but not prove it: the session simply ends. The engine logs a warning naming
this cause when a console's session ends on its own 3 times within 120 s (at most once
per window per console).

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
A lane carries heartbeats and, on a node with `CAP_DATA_PLANE`, the lane data frames `Chunk`
and `Bundle` (§12) and `Error`; any other frame without the IGNORABLE flag is answered
`Error(ERR_PROTOCOL)` and closes the lane.

## 10. Version 1 scope
Version 1 is what this document specifies; the sections below say what that is and what it is
not. Unknown frame types on a control connection are a protocol error; new frame types require a
version bump or a negotiated `caps` bit.

In version 1:
- Project 1 (§1–§9): framing, codecs, keys, handshake, pairing, the trust slot and launch
  token, heartbeats, RPC `node.info` (and `pairing.open`, `crypto.bench`).
- Project 2 (§11–§16): jobs and manifests, chunks and bundles on lanes, credit, verification
  groups and outboards, journals, resume, staging, apply and the governor; uploads
  (folders, single files, file lists, zip archives read as sources, local and NAS sources),
  downloads (to a folder or to a zip), console-local copy and move, and PS5 → PS5 through
  an engine relay (the engine downloads from one console while it uploads to the other,
  with a bounded in-memory hand-off); the data RPCs `job.copy`, `job.status`, `job.cancel`
  and `disk.calibrate`.

Not in version 1, each with its reason:
- Direct PS5 → PS5 (tickets, Ed25519 signing, cross-network encryption): the relay is the only
  PS5 → PS5 path; direct transfer needs a trust model between two consoles. PS5 → PS5 has
  engine support but no UI wiring (project 3).
- Engine ↔ engine sharing: `host::FolderHost` exists as the receiving half, the sharing
  policy and the feature are deferred.
- Zstd bundles and small-file deduplication: ruled out of project 2.
- Zip entries larger than 256 MiB (`ZIP_MAX_ENTRY`) as AVA1 sources: entries are inflated
  on demand and there is no streaming entry reader yet, so an archive with a larger entry stays
  on FTX2.
- 7z and RAR sources: their decoders are forward-only, so there is no random-access `Source`
  for them; they stay on FTX2 until project 3.
- Full re-verification of durable groups on resume: §13.4 re-hashes only the last durable
  batch of each partial file and trusts older groups to the journal.
- Sources of unknown length: every file's size must be known when the manifest is built.
- Auto-tuning of the small/large cutoff (the design spec's 64 KiB–4 MiB range): the cutoff
  is the protocol constant `LARGE_CUTOFF`, §12.2.
- Resuming a zip download within a run: a dropped zip download restarts the archive with a
  fresh job per attempt (progress stays monotonic). FTX2 resumes mid-entry, so this is a
  regression against FTX2 and is listed as one in `CUTOVER.md`.
- Resuming a download whose remote manifest changed: the engine restarts that job.
- A same-drive `fs.move` over AVA1 is `fs.rename` (§7.3), with the `st_dev` guard; a cross-mount
  move (copy, verify, delete) is an AVA1 job.
- Typed bodies for the management text methods (§7.3): the filesystem, node and job methods are
  typed; the rest carry `MgmtText` and are typed method by method after the cutover.

## 11. Jobs and manifest

11.1 Every transfer is a job with a 16-byte `job_id`, chosen by the node that opens it
(the engine uses the HTTP API's `tx_id`). A job is bound to the static key of the peer
that opened it; only that key may resume or cancel it. Every data-plane message
(types 0x20–0x3F) has `job_id` as its first field, so a router reads it from body[0..16].

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

A sender that wants "skip files the console already has" picks the policy from its source:
when every file reports a real mtime (local disk; SMB, FTP and SFTP servers) it uses
`skip-existing`; when any file's mtime is unknown (`mtime = 0` in the manifest — a backend
that reports none) it uses `verify` for the whole job, computing each file's root before
the manifest is sent. `verify` costs one read of the source plus a hash of each existing
console file of the right size, but is correct without an mtime; the sender never guesses
an mtime. The engine's "safe" resume mode always uses `verify`. The known weakness of
`skip-existing`: a file whose content changed but whose size and mtime did not is skipped.
A directory's mtime is always 0 and is ignored by both policies.

11.5 A `JobOpen` for a job the receiver already knows is a resume: the receiver matches
the new manifest against its stored one by path, keeps the progress of entries whose
size and mtime are unchanged, and restarts the others (never splicing old and new bytes).
`Resume{job_id, manifest_hash}` is the fast path when the sender still holds the same
manifest: the receiver answers `JobMap`, or `JobMap{status = ERR_UNKNOWN_JOB}` and the
sender falls back to `JobOpen`. A map larger than one control frame is sent as several
`JobMap` pages; `last = 1` marks the final one. `Durable` is never paged: each is complete.
Engines reopen with `JobOpen` after any interruption; `Resume` is optional for senders that
keep their manifest and credit state. A receiver answers `Resume` with the `JobMap` of a parked job
of the same peer key whose stored manifest has that hash, else `JobMap{status = ERR_UNKNOWN_JOB}` —
never silence.
Credit restarts after any interruption and nothing outstanding carries across a reconnect: the
grant in a `JobOpenAck` is an absolute number that sets the sender's window (a `Credit` that
arrives later adds to it), and a `Resume` restarts the window the same way — the receiver resets
the job's outstanding-credit count to its current grant and re-sends that grant as `Credit`.

11.6 Staging: when the job root does not exist, the receiver writes the whole tree under
`<root>.ava-part/` and, after the last file, renames it to `<root>` (same parent, `st_dev`
checked). When the root exists, files are written in place; large files through
`<name>.ava-part` and a same-directory rename. `JF_SINGLE_FILE` writes `<root>.ava-part`. A staging
receiver takes `<root>` with `mkdir` before it journals the job (an existing `<root>` then
refuses it, `ERR_EXISTS`) and records that in `JnlOpen.staged` bit 1, so on resume the empty
`<root>` is its own; the final rename replaces only that empty folder (not empty: `ERR_EXISTS`).

11.7 Limits and lifetime. A node lists at most 32 jobs; past that, or when it has no buffer budget
left for another job (§12.4), a `JobOpen` is answered `ERR_BUSY`. A manifest has at most 4,000,000
entries (a receiver refuses growth past that), and its pages are sized to fit a control frame (the
console writes at most 60 KiB per page). At most one running job writes a destination: a `JobOpen`
or `job.copy` whose root is, or lies inside or around, the root of another job that has not ended
(and, for a move, its source) is answered `ERR_BUSY`; parked jobs count, because they can resume.
When a session ends its jobs are parked, not ended: they stay listed, detached, for 10 minutes
and then leave the table; their journal stays on disk (§14.3), so a later `JobOpen` resumes them.
A finished upload or download leaves the table 10 s after its session lets go of it; a finished local job
(`job.copy`) stays listed for the full park age so `job.status` can still answer. Either peer ends a
job with `JobCancel{job_id, reason}`, where `reason` is the `ERR_*` code the ender wants reported
(`ERR_CANCELLED` for a user's cancel, `ERR_IO` or `ERR_VERIFY` when a sender's source fails); the
receiver then ends the job with that status in `JobDone` and keeps the journal.

11.8 Job flags (`JobOpen.flags`, `JobCopy.flags`, `JnlOpen.flags`). A receiver answers
`ERR_PROTOCOL` to a flag it does not know.

| flag | value | meaning |
|------|-------|---------|
| `JF_SINGLE_FILE` | 1 | the root is a file path and the manifest has one file entry; the part file is `<root>.ava-part` (§11.6). On `job.copy` the node derives it from the source, and passing it for a directory source is `ERR_PROTOCOL` |
| `JF_ORDERED` | 2 | the receiver consumes the files in manifest order (a download written into a zip, a relay); the sender then reads with one reader |
| `JF_UNSAFE_READ` | 4 | a sender may read outside the roots its read policy allows (system files); the engine sets it only for a download the user marked unsafe |
| `JF_MOVE` | 8 | `job.copy`: delete each source file after its destination is durable (§12.6, §15.5) |
| `JF_OVERWRITE` | 16 | `job.copy`: replace destination files that already exist; unset, an existing destination root is refused with `ERR_EXISTS`. Not valid on `JobOpen` |

11.9 Messages. All are data-plane messages (§11.1). "Sender" and "receiver" are the roles of the two
peers for the job (§11.3), not who opened it.

| type | message | direction | where |
|------|---------|-----------|-------|
| 0x20 | `JobOpen` | opener → peer | control |
| 0x21 | `JobOpenAck{status, credit, staged, workers}` | answerer → opener | control |
| 0x22, 0x23 | `ManifestPage`, `ManifestEnd{files, bytes, manifest_hash}` | sender → receiver | control |
| 0x24 | `JobMap` | receiver → sender | control |
| 0x25 | `Resume` | sender → receiver | control |
| 0x26, 0x27 | `Chunk`, `Bundle` | sender → receiver | a lane |
| 0x28 | `Received{lane, seq}` | receiver → sender | control |
| 0x29 | `Credit` | receiver → sender | control |
| 0x2A | `Durable` | receiver → sender | control |
| 0x2B | `FileRoot` | sender → receiver | control |
| 0x2C | `FileRetry{file_id, reason}` | receiver → sender | control |
| 0x2D | `Status` | receiver → sender (IGNORABLE) | control |
| 0x2E | `JobDone{status, files, bytes}` | receiver → sender | control |
| 0x2F | `JobCancel` | either | control |

`FileRetry.reason` is `RETRY_VERIFY` (1, the root did not match), `RETRY_IO` (2, the receiver lost
the part file or outboard) or `RETRY_CHANGED` (3, the record's length disagrees with the manifest).

## 12. Data frames and credit

12.1 `Chunk` and `Bundle` travel on lanes; everything else on the control connection.
The header `channel` of a lane data frame is the sender's per-job sequence number.

12.2 `Chunk.offset` is a multiple of 1 MiB (one verification group, §13); its length is
a multiple of 1 MiB unless the chunk ends the file. A file is a *large* file when its
size is at least `LARGE_CUTOFF` (256 KiB), a protocol constant both sides use (`JobOpen`
carries no cutoff); smaller files travel whole, as `BundleRecord`s. A receiver ends the job
with `ERR_PROTOCOL` on a `Chunk` for a small file or a `BundleRecord` for a large one. A piece is
never larger than the credit the receiver granted (§12.4): the sender caps a piece at the smaller of
the chunk size and the granted window, floored to whole verification groups, and a grant below
one group fails the job loudly instead of stalling.

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
number of lane-frame body bytes the sender may have outstanding — the window counts the whole
body, so a `Chunk` costs its data plus 34 bytes of framing; `Credit{bytes}` returns space as the
receiver frees buffers. A version 1 receiver grants 64 MiB, and never less than 8 MiB: a node whose
global buffer budget cannot cover 8 MiB refuses the job with `ERR_BUSY`. A receiver that sees a
lane frame exceed the credit still outstanding sends a sealed `Error{ERR_CREDIT}` on the offending
lane and closes that lane only: the session, the job and its other lanes go on. Nothing of the frame is
buffered or acknowledged, and the sender observes ERR_CREDIT on that lane; it requeues the frames the
lane never had `Received`, as for any dead lane (§12.3). A sender never sends a piece larger than the credit already granted: pieces are sized at
read time to fit the window (whole verification groups, one group minimum; a file's final piece keeps
the whole-file rule), and a window that cannot hold one group — or whose smallest queued frame fits
no lane for 10 s with nothing sent, received or credited — ends the job (`ERR_PROTOCOL`) instead of
stalling. The 10 s applies while the receiver holds no window bytes (a window that can never fit the
frame). While it still holds some — a drive in a long flush has not yet returned its `Credit` — the
receiver is slow, not dead, and the sender waits (a dead one is caught by §6 liveness), failing only
after 10 minutes without progress. A lane's death does not refund window credit that the receiver has not accounted for
(§12.3): its un-received frames are requeued with their bytes still charged, and the charge is
released only when the receiver accounts for them (its `Credit` after the apply) or the job ends.

12.5 Per lane, the sender keeps at most `max(chunk size, lane rate × 2 s)` bytes sent
and not yet `Received`.

12.6 `Durable{files, ranges}` follows the order: data synced → journal appended and
synced → `Durable`. `JobDone` follows the last durable commit (and the staging rename).
A failure after every byte is durable (`ERR_EXISTS`, `ERR_CROSS_DEVICE` on the final
rename) is reported in `JobDone` and never causes a resend. A rename is durable only once its
directory is synced: the receiver fsyncs the parent directory after every commit or staging
rename, before it journals that commit. Likewise for new names: before a batch is journaled, every directory
that gained a file in it is synced once, after the file data. Where fsync does not reach stable
storage (macOS), a receiver flushes the drive's cache once per batch after the per-file fsyncs.

12.7 Download pipeline (receiver on the engine, informative; §12.6 is what binds). The pipeline is
tuned so a download is bound by the wire, not by the receiver: (a) a job with nothing to resume sends
its (empty) `JobMap` before the journal and the destination folders are made durable, so the sender's
turnaround to its first data frame overlaps that setup; (b) the files of one `Bundle` are root-checked
and written on a blocking task of their own, several bundles at once, while the receiver goes on
draining the inbox; a bundle's `Credit` is returned when its files are written, not when its frame
arrived; (c) the sync batch is due every 250 ms and at once when every file of the job has been
written (waiting for the next tick there only adds latency). None of this weakens §12.6: a file is
reported durable, and `JobDone` sent, only after its data, its names and its journal record are
synced. A sender starts its reader and writers when the map arrives, not at the next 25 ms tick, and
its walk stats each path once (`lstat`; `stat` only for a symlink).

A transient `fsync` error does not fail the job at once. A receiver retries a failed `fsync` (file
data, directory, journal append, final commit sync) up to four more times, waiting 20, 60, 200 and
600 ms, when the error is one a drive can recover from: `EINTR`, `EAGAIN`, `EBUSY`, `ETIMEDOUT`,
`ENOENT`, `ENXIO`, `ENODEV`, in Sony's `0x8002xxxx` form too (the console reports a USB drive's
hiccup as `0x80020002`). `EIO` is never retried: it is the kernel saying the data did not reach the
drive, and an fsync that "succeeds" afterwards proves nothing (the kernel may have dropped the dirty
pages that failed). A retry that finally succeeds is not trusted alone either: the receiver reads
back what that fsync covered — each small file is re-read and its BLAKE3 compared with the root it
arrived with, each large-file range of the batch is re-read and its group chaining values compared
with the outboard's — and a mismatch ends the job with `ERR_IO` before anything is journaled or
acknowledged. Nothing is acknowledged while a retry is pending; every retry is logged
(`[ava1] fsync failed (errno 0x80020002), retry 1 of 4`). Exhausted retries end the job with
`ERR_IO` ("fsync failed"), as before.

The console-local copy and move (§15.5) use the same standard in memory: a copy is a receiver job
fed by an in-process reader, with no read-back of the destination; a move deletes a source file only
after every destination group of that file is verified in memory, the file and its directory are
fsynced and the destination's `Done` is journaled; a copy that fails deletes nothing.

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

13.4 Resume verification: before answering a resumed job's map, the receiver re-hashes the groups
of each partial file that its last durable batch covers and compares them with the outboard; a
mismatch drops those ranges from the map. The console re-hashes only that batch and trusts older
durable groups to the journal — version 1 never re-verifies every durable group of a console
partial file. A receiver may check more: the engine's receiver re-hashes every durable group of
every partial file and resets a file on any mismatch. The verify policy re-hashes whole files.

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
per large file. A node removes a directory 7 days after its last write (directory or journal mtime), on
start and then once a day; the engine also sweeps `<data dir>/ava/send` (sender outboards) and never
touches the directory of a job that is running in the process. A job whose session ended is parked for 10 minutes first (§11.7).

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
applies to it unchanged. The job is owned by the peer key that issued it: `job.status` and
`job.cancel` from another key answer `ERR_UNKNOWN_JOB`. A `job.copy` for an id the node already
lists, from the same owner with the same parameters, answers with that job's `Status` instead of
starting a second one — which makes re-issuing after a lost connection safe; a failed job is retired
and restarted by a re-issue. `job.cancel` stops a job and unlists it (the journal stays), so a
caller never cancels a job whose terminal status it still needs. A finished local job stays listed for the park age, so `job.status`
keeps answering for it.

`job.copy` uses the source and destination paths, a stable job id, and flags including
`JF_OVERWRITE` and `JF_MOVE`. Without `JF_OVERWRITE`, an existing destination root is refused
with `ERR_EXISTS`; with it, colliding files are replaced and destination-only files remain.
A move deletes source entries only after the destination rename, parent directory sync and
successful Done journal append. It checks each source file against the manifest before unlinking
and reports any paths left behind as a failed job; `job.status` stays running (`state` 0, `current` =
"deleting source") during deletion, so `state` 1 is the only point at which a move is done.

15.6 Open files. The number of descriptors a process may hold open is a resource, and on the
console it is smaller than `RLIMIT_NOFILE` says: the measured ceiling on firmware 13.60 is about
619 open files while the limit reads about 13,952. At data-plane start a node therefore probes
(it opens `/dev/null` until the system refuses, bounded at 4096), and its open-file budget is the
smaller of the raised limit and the probed count, each less 128 descriptors held back for sockets
and the other services. Half of the budget is the share of pending small-file descriptors (§15.2) all
jobs together may hold, never fewer than 4; a worker that finds the share used up runs queued sync
work or waits, and the job thread syncs early so that the batch frees slots. A single job holds at
most 512 pending small files regardless of the budget. `disk.calibrate` (§16.10) works within the same
budget instead of opening every file at once.

## 16. Governor

The sender and the receiver each keep one small control loop; both are pure functions of the
numbers they are fed, so both are tested against models rather than sockets.

- Start: 2 lanes, a 4 MiB chunk and a 1 MiB bundle target; receiver workers start at 4.
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

## 17. Sequential sources (sender-local, no wire change)

A sender may read a source that can only be read forward (a 7z folder, a solid RAR): a
`SeqSource` (`engine/crates/ava1/src/seq.rs`). The receiver cannot tell: it sees ordinary
`Bundle`, `Chunk` and `FileRoot` frames, and the manifest (sorted, §11.3) is built from the
archive's headers before the job opens.

17.1 One decode thread replaces the random readers. It calls `SeqSource::pass`, which visits
entries in *decode* order, asks `want(path, size)` for each (`Keep::Skip`, `Keep::All` or
`Keep::Ranges(lacking)`) and feeds the wanted ones to an `EntrySink` (`begin`/`data`/`end`).
The thread maps the entry's path to its manifest id (the archive's order is unrelated to the
manifest's), cuts files below `LARGE_CUTOFF` into records and the rest into group-aligned
chunks exactly as the random readers do (§12.2, §13), and queues a `FileRoot` after a large
file's last chunk. It takes the same read-ahead permits, so a slow lane parks the decoder
and its memory stays bounded.

17.2 Resume (§14). Files the receiver reports done are `Skip`. The pass starts at the minimum
`restart_for(id)` over the unfinished files (7z: the folder's first entry; RAR non-solid: the
entry), so everything the receiver already has before that point is not decoded. A partly
durable large file is decoded from its start (a decoder cannot seek inside an entry); only the
groups the receiver lacks are sent, durable groups are hashed only when no persisted outboard
CV exists, and a file whose every CV is known needs no decoding at all. There are no decoder
checkpoints: a resume costs a decode of at most the restart folder's prefix.

17.3 `FileRetry` (§13) queues the file for a further pass over only the retried files
(`Keep::All`, their outboards dropped). A job makes at most 3 decoding passes (the first plus two retry passes; a pass that
has nothing to decode is not counted); a further retry request fails it. An entry that a pass never delivers, delivers twice, or delivers with a different size
than the manifest fails the job (the archive changed between listing and sending).

17.4 Cancellation. `pass` receives a flag raised by the job's cancel *or* by any other way the job ends (a lane,
protocol or receiver failure) and must poll it at least every 1 MiB of input, including while
skipping, and every `EntrySink` call fails
once the job is ending; `SeqSource::close` is called at teardown before the decode thread is
joined. The bottleneck is `BN_SOURCE` while the decode thread is what the lanes wait on.
