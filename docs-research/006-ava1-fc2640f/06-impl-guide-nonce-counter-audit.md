# Implementation guide — per-lane AEAD nonce/counter audit (release gate)

**For:** the implementation agent (ava1 branch). **Reviewed design:** 006/03 category B.
**Severity:** release gate. A ChaCha20-Poly1305 nonce reused under one key is a catastrophic
break (keystream reuse + forgeable tags). This task is **confirm-and-harden**, not a rewrite:
the design read as correct this pass, but it must be formally verified and nailed down with a
test before shipping.

## What is already in place (from this review, confirm each)
AVA1 nonces are `0x00000000 ‖ u64le(ctr)` (keys.rs:308). Safety therefore rests on: each
`(key, ctr)` pair being used for exactly one seal. The structure supports that:
- **Writer and reader are separate structs**, each with its own `key` and `ctr`
  (`conn.rs`: writer `set_key`/seal at 117/170/172; reader `set_key`/open at 273/308/311). A
  direction never shares a counter with the other direction.
- **Keys are direction-specific.** Control: `control_key(c2s)` on one side's writer,
  `control_key(s2c)` on its reader (handshake.rs:190-191, 299-300). Lanes:
  `lane_key(c2s, lane, cn, sn)` vs `lane_key(s2c, lane, cn, sn)` (server.rs:818-819,
  session.rs:519-520). So the same `ctr` value under c2s and under s2c are different keys —
  no reuse.
- **Lane keys fold in the lane number and both fresh per-session nonces**, so restarting each
  lane's counter at 0 is safe: every lane has a distinct key, and a new session has fresh
  `cn`/`sn` (keys.rs:225-242).
- **The counter increments on every sealed frame unconditionally** (conn.rs:172 send, 311
  recv), including pings and `FLAG_IGNORABLE` frames, so the two ends stay in lockstep per
  direction and no frame type can cause a silent reuse.
- **`set_key` zeroes the counter** (conn.rs:119, 275) and is called only at handshake and lane
  establishment (grep below) — never mid-stream.

## Steps to formally close it

### 1. Prove `set_key` is handshake/lane-setup only
```
grep -rn "set_key" engine/crates/ava1/src --include=*.rs | grep -v '#\[cfg(test)\]' 
```
Confirm every non-test call site is in `handshake.rs` (control keying), `server.rs`/
`session.rs` (lane keying), or `conn.rs` plumbing — and none is reachable twice for the same
live connection. Write the finding down (each site, why it runs once). Any mid-session
re-key would reset `ctr` to 0 under the same key = reuse: there must be none.

### 2. Prove the C payload matches
The console C receiver seals/opens the same frames. In `payload/ava1/` (and cross-checked by
`ava1-ctest`), confirm:
- The C side uses the same `0^4 ‖ u64le(ctr)` nonce and the same per-direction/per-lane key
  derivation (already vector-checked: `ava1_lane_key`, `ava1_control_key`, `ava1_noise_*`).
- The C counter also increments on every sealed frame including heartbeats, and is never
  reset except at (re)keying. Read the C send/recv frame paths and state it explicitly.

### 3. Second counter at keys.rs:438
There is a `*ctr += 1` in `keys.rs` (~438) outside `conn.rs` — identify what it belongs to
(looks like a MAC/stream sub-protocol or MgmtText chunking). Confirm it is either (a) not an
AEAD nonce, or (b) an AEAD nonce under its own unique key with the same once-per-key
discipline. Document which.

### 4. Harden: explicit counter ceiling (defense in depth)
`self.ctr += 1` on a `u64` wraps in release builds. 2^64 frames is physically unreachable, but
a wrap would be silent reuse, so make it impossible rather than improbable:
- In both `send_with_flags` (conn.rs ~170) and the reader open path (~308), before sealing/
  opening, refuse once `ctr` reaches a hard ceiling well below `u64::MAX` (e.g.
  `const NONCE_CEILING: u64 = u64::MAX - 1;`): return `Ava1Error::Lost("nonce space
  exhausted")` / `BadTag` and break the connection. This can never trigger in practice; it
  turns a catastrophic silent failure into a clean connection break if an invariant is ever
  violated upstream.
- Use `ctr = ctr.checked_add(1).ok_or(...)?` or the ceiling check; do not rely on debug-only
  overflow panics.

### 5. Clarify the misleading doc comment
keys.rs:227-228 says the control connection "uses all-zero nonces (`control_key`)". This reads
as "the AEAD nonce is always zero", which would be a bug. It actually means the *key
derivation* takes no per-lane nonces for lane 0; the AEAD nonce is still `nonce(ctr)` and
advances. Reword the comment so no future reader thinks control frames seal under a constant
nonce.

## Test to add (lockstep / no-reuse)
In `conn.rs` tests, add a test that sends a mix of data frames, `send_ignorable` frames, and
pings through a writer and reads them back through a reader keyed with the same key, and
asserts:
- every frame opens (counters stayed in lockstep across frame types), and
- two frames never seal to the same `(nonce)` — assert `ctr` strictly increased per sealed
  frame (expose `ctr` under `#[cfg(test)]` or assert via distinct ciphertext for identical
  plaintext).
Optionally a property test: N random frames, collect the nonces used, assert all distinct.

## Acceptance
- A short written confirmation (can live in this directory as `06a-nonce-audit-findings.md`)
  covering steps 1-3 with the exact call sites.
- The ceiling guard (step 4) and reworded comment (step 5) landed.
- The lockstep test green; `ava1-ctest` still green (Rust↔C parity).
- Only then mark category B **PASS** in `03-evaluation-checklist.md`.

## Validate locally
```
grep -rn "set_key" engine/crates/ava1/src --include=*.rs
cargo test -p ava1 conn
cargo test -p ava1-ctest     # Rust<->C parity, if the harness builds here
```
