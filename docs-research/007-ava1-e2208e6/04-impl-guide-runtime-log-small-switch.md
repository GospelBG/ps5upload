# Implementation guide — a runtime off-switch for durable-by-log on the console (HW-3)

**For:** the implementation agent (ava1 branch). **Severity:** hardware-phase operability (an
incident needs this lever). **Size:** small.

## Problem
`ava1_data_log_small()` (`ava1_data.c:288`) returns `D.cfg.log_small != AVA1_LOG_SMALL_OFF`,
and CUTOVER §4.3 says `AVA1_LOG_SMALL_OFF` "restores the per-file path for this release". But
nothing on the console ever sets `log_small` at runtime: no debug file, no management method,
and a console has no environment. The switch is compile-time only. If durable-by-log
misbehaves on a real drive during the hardware pass, the only remedy is a rebuilt helper.

## Design
Mirror the timing flag. `ava1_send.c:699` already defines
`AVA1_TIMING_FLAG "/data/ps5upload/debug/ava1-timing"` and reads it. Add
`AVA1_LOG_SMALL_OFF_FLAG "/data/ps5upload/debug/ava1-log-small-off"`: when the file exists,
`log_small = AVA1_LOG_SMALL_OFF`.

## Where
1. Define the path next to the timing flag (or in `ava1_data.h`).
2. In `ava1_data_cfg` initialisation (where `D.cfg` is populated at helper start) **and** at
   each `JobOpen` (so an operator can flip it between jobs without restarting the helper):
   `if (access(AVA1_LOG_SMALL_OFF_FLAG, F_OK) == 0) D.cfg.log_small = AVA1_LOG_SMALL_OFF; else
   D.cfg.log_small = <default on>;`. Evaluate once per job and cache on the job, so a job is
   consistently logged or consistently per-file; never switch mid-job.
3. Log one line at job start when it is off: `[ava1] job ...: durable-by-log OFF (debug flag)`,
   so a hardware run's stderr says which path produced its numbers.
4. Optionally expose the same through `Status` (a flag bit) so the engine's job summary shows
   it; not required for the hardware pass.

## Important: recovery must ignore the switch
`ava1_pack_recover` must still run when the flag is set: a crashed *logged* job's pack files
hold journaled data regardless of the current setting. Recovery is reference-driven and
already runs from the journal, so this is only a matter of not gating the recover call on
`ava1_data_log_small()`. Add a test: journal a logged batch, set the flag, restart → files
recovered.

## Tests
- Flag present → `apply_record` takes the per-file path (`AVA1_HOOK_*` or the stats line
  shows `dirs` > 0 and no pack segment created).
- Flag absent → pack segment created.
- Flag toggled between two jobs → each job consistent with the flag at its open.
- Recovery with the flag set (above).

## Acceptance
Switch reachable on a console by creating one file; CUTOVER §4.3 updated to name the file;
tests green.
