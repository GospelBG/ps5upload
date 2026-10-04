# Session 007 work order (pre-hardware)

| # | item | guide | blocks user release? | size |
|---|---|---|---|---|
| 1 | Explain the Pro outage (triage) | 02 §2 | **yes** | investigation |
| 2 | Reconcile CUTOVER §3 "default stays FTX2" with AVA1-only code | 02 §1.2 | **yes** (doc) | XS |
| 3 | Nonce audit remaining steps (ceiling guard, comment, lockstep test, C layout note) | 006/06 + 01 §4 | **yes** | S |
| 4 | `same_device` fail closed on −1 | 03 | before next console run | XS |
| 5 | Runtime durable-by-log off-switch (debug file) | 04 | before next console run | S |
| 6 | Receiver progress watchdog | 006/05 | no (last hang) | S |
| 7 | SPEC §11.5 vs console `Resume` credit | 01 C-1 | before SPEC is final | XS |
| 8 | `sweep_one` first-pass content check (optional) | 01 §5 | no | XS |

Order: 4 and 5 first (they are the investigation's levers), then 1, then 2 and 3, then 6-8.
Then the hardware pass per 02 §3, then the workspace gate, then release.

Closed this round (no action): 005 §5 zip fix, 005 §3 trio, 004 O1/O2, durable-by-log on both
ends, the JobOpen-after-cancel hang, settle bound, sweep on workers, all 006 checklist PASS rows
re-confirmed at e2208e6.
