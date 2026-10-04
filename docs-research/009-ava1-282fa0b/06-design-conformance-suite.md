# Design 6 — a standalone AVA1 conformance suite

## Goal
Any implementation (ours or a third party's) can be driven through the protocol by a tool that
needs nothing from this repo but the spec, and get a pass/fail per clause. This is what turns a
good in-house protocol into one others can implement — and it surfaces dormant divergences
like the `Resume`-credit case (007 C-1).

## Design
- **A crate `ava1-conform`** (binary), built on the existing `ava1` client/server code **only as a
  reference peer**: it can act as a conformance *client* against a device under test (DUT) that
  is a receiver, and as a conformance *server* for a DUT that is a sender.
- **Clauses as tests:** each SPEC section with observable behaviour becomes a scripted scenario
  with a stable id (`S4.4-counter-lockstep`, `S11.5-resume-no-credit-restart`,
  `S12.4-credit-overflow-ends-lane`, `S12.8-stall-ends-job`, `S15.7-settle-bounded`, …). The
  scenario drives frames, injects faults (drop a lane, send an oversized frame, ping without
  data, torn resume), and asserts the DUT's visible response and timing bounds.
- **Vectors:** reuse `protocol/ava1/vectors/*` as the first clause group (handshake, keys,
  pairing code, frame header); add negative vectors from `conn` tests.
- **Output:** a JUnit XML + a Markdown table keyed by clause id, so a third party can publish
  their conformance table. A `--profile console|engine` selects the clauses that apply to each
  role.
- **First DUTs:** the console helper (over the network, like `ava1_live`) and the engine receiver
  (loopback). Running the suite against both will immediately test SPEC claims from outside the
  codebase. Expect to find spec/code gaps; fix the spec or the code, never the test.
- **Independence check (optional, the real proof):** have an agent implement a minimal
  receiver (JobOpen/Chunk/Durable/JobDone, one lane, no resume) **from the SPEC only**, in a
  different language (Go or Python), and run the client profile against it. Every divergence
  found is a spec clarity bug.

## Entry points
- New crate `engine/crates/ava1-conform`; depends on `ava1` for framing/crypto; its own
  scenario runner. Start by porting `ava1_live.rs` and the `stall.rs`/`admission.rs` ctest
  scenarios to clause ids.
- SPEC: add a "Conformance" appendix listing clause ids ↔ sections.

## Acceptance
Both current implementations pass the suite; the SPEC gains clause ids; one external minimal
implementation has been exercised and every divergence it found is resolved in SPEC or code.
