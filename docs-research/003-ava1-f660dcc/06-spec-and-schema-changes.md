# Consolidated SPEC and schema changes proposed by session 003

In the order to apply them. Each names the design note that justifies it. Numbers for new
messages, journal kinds, caps and flags are chosen to not collide with f660dcc.

| # | where | change | from |
|---|---|---|---|
| 1 | SPEC §4.6, §5 steps 2–4; `ava1_noise.c`, `handshake.rs` | Pairing code = BLAKE2b-256("AVA1 pairing" ‖ h ‖ nonce_c ‖ nonce_s)[0..4] mod 10⁶; server commits `BLAKE2b-256(nonce_s)` in `ServerInfo`, client sends `nonce_c` in `ClientInfo`, server reveals `nonce_s` in the sealed `Welcome`; both fields required, a missing one is `ERR_PROTOCOL` | 001/02 §2 S1 (release gate) |
| 2 | payload `is_path_lexically_allowed`, FTP root view, AVA1 `may_write`/`may_read` | `/data/ps5upload/ava/{identity,peers,launch_tokens}` and `/data/ps5upload/ava/jobs` are never writable or readable through any transfer or management path | 001/02 §2 S2 |
| 3 | SPEC §12.6, §15.2, §15.4, new §15.7; schema journal structs | Durable-by-log: `JnlBatch` ext `pack_segment`/`pack_offset`/`pack_len`; `JnlSweep` (kind 6); `JnlSnapshot` ext `unswept`, `segments`; struct `PackRef`; `Status` ext 5 `unswept`; `JobDone` ext 2 `settling`; §12.6 wording "a file reported durable can be reproduced by the receiver alone" | 02 |
| 4 | SPEC §15.3 | `commit` runs on a worker; preallocation happens before the job mutex is taken | 01 §3.3, 03 §6 |
| 5 | SPEC §6 (informative), §9 | Lanes set 4 MiB socket buffers both ends; a receiver may open sealed lane frames out of receipt order (counters assigned at receipt) | 03 §1–2 |
| 6 | SPEC §16 | Lane probe gain 1.05 below 90% of the best observed rate; chunk held at 4 MiB until lanes ≥ 4 or lanes stop helping; `JobOpenAck` ext `open_us_per_mib` (optional) | 03 §5 |
| 7 | SPEC §10, CUTOVER §2 | Zip downloads are Stored; entry-level then mid-entry resume via sink journal records | 04 |
| 8 | SPEC §5 caps, §11.8 flags, §11.9 messages; schema | `CAP_DELTA` (4); `JF_DELTA` (32); `FileCvs` 0x30, `FileCvsAck` 0x31; `JobMap` ext 2 `mismatch`; `JnlAdopt` (kind 7) | 05 |
| 9 | SPEC §6 | `dead_after` default 6 s → 12 s; the ping stays at 2 s | 001/02 R1 |
| 10 | `payload/Makefile` | `BLAKE3_SRCS` C files compiled with the AVA1 `-O2` group | 01 §8 |
| 11 | SPEC §17.4 | Either implement sender-side `BN_SOURCE` attribution for the decode thread or drop the sentence | 002/01 L3 |

Items 1–2 gate the release. 3–6 are the performance programme, measured per
`03-design-lane-receive-path.md` §4 before and after. 7–8 are the first post-cutover
features. 9–11 are small and independent.
