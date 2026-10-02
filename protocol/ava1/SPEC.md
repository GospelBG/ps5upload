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
