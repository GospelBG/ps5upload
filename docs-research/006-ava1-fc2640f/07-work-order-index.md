# Session 006 work order — areas that need work, for the implementation agent

From the evaluation checklist (`03-evaluation-checklist.md`), these are the items with a
concrete fix ready to implement. Each has a step-by-step guide in this directory. Suggested
order is by release risk, then by cost.

| # | area | guide | category | kind | size | blocks release? |
|---|---|---|---|---|---|---|
| 1 | Per-lane AEAD nonce/counter audit | `06-impl-guide-nonce-counter-audit.md` | B | confirm + harden | S (mostly reading) | **yes** |
| 2 | Receiver progress watchdog | `05-impl-guide-progress-watchdog.md` | F | new guard + test | S | no, but it is the only remaining hang |
| 3 | Zip whole-but-unfinished resume | `04-impl-guide-zip-finish-window.md` | H | correctness fix | S | no (avoids a needless restart) |

## Do these first (small, ready)
All three above are self-contained, each lands with its own tests, and none depends on the
others. Recommended sequence:
1. **Nonce audit (#1)** — it is a release gate and mostly investigation; do it before anything
   that could perturb the frame paths. Produces a written finding plus the ceiling guard and
   the reworded comment.
2. **Progress watchdog (#2)** — closes the one hang the liveness audit found.
3. **Zip finish window (#3)** — removes a needless archive restart; lets CUTOVER §2 be struck.

## Larger items — design exists, not a quick fix (do after the three above)
These are tracked but are not in scope for a quick pass; they have their own design notes:
- **Durable-by-log / pack segments** — `003-ava1-f660dcc/02-design-durable-by-log.md`. The big
  one; the tiny-file fsync ceiling cannot move without it. (Category G/L.)
- **Hardware lanes×chunk matrix** — `protocol/ava1/CUTOVER.md` §4.1. Measurement, needs both
  consoles. Decides the decrypt-off-reader step (`003-ava1-f660dcc/03`). (Category L.)
- **Remaining 27 MGMT_METHODS rows + hardware verification** — (Category K.)
- **Group delta v1.1** — `003-ava1-f660dcc/05-design-group-delta-v1_1.md`, after the cutover.

## Re-confirm (cheap checks, fold into the nearest relevant change)
- `fs.stat`/`fs.list` path-disclosure scope (D) — decide if it should be narrowed.
- Governor `best_rate` non-decay (L, 006/01 §2) — optional slow decay.
- job.run cancel / release-on-delivery semantics vs FTX2 (K, 004 O1/O2) — confirm resolved.
- A ceiling on concurrent `JobOpen`s (T) — confirm job admission is bounded.
- Negative/fuzz frame vectors (A) — confirm `ava1-chaos` covers truncated/oversized/misordered.

## Definition of done for this work order
The three guided fixes merged with green tests, category B marked PASS in the checklist once
its audit findings are written, and the CUTOVER zip row struck. Report back and the cloud
session will re-review on the next push.
