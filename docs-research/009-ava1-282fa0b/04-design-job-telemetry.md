# Design 4 — structured per-job telemetry (opt-in)

## Goal
When a user's upload is slow, stalls, or a sweep fails, the answer is in one structured
record they can attach (or let the engine send), not scattered stderr lines on two machines.

## Non-goals
Always-on remote telemetry. Default is **off**; the record is written locally and shown in the UI
and the bug bundle; sending anywhere is a separate, explicit opt-in.

## Design
- **Record:** `job_summary` JSON, one per finished/failed job, written by the engine to
  `<data dir>/.ps5upload/jobs/<job id>.json` (rotated: keep the last 200). Fields:
  job id, kind, console (hash of the peer key, never the address), start/end, result code and
  message (`ERR_STALLED`, `ERR_CROSS_DEVICE`…), files/bytes, resumed flag, lanes/chunk history
  (the governor's `Decision` series, downsampled), the `JobSummary` time shares (credit-starved,
  source-starved, receiver-bound, last receiver bottleneck), settle time, `unswept` peak, the
  console's end-of-job line **verbatim** (it already arrives via `Status`/`JobDone` message, or
  add a `job.summary` management call returning the console's `ava1_apply_summary` text),
  engine version, helper build, drive (`/data`, `usb0`, …), slow-drive switch fired.
- **Source of truth already exists:** `governor.rs` `Sample`/`Decision`/`JobSummary`,
  `send.rs` settle wait, `recv.rs` stall line, console `ava1_apply_summary` (apply.c:391). This
  design only persists and structures what is printed today.
- **API:** `GET /api/jobs/{id}/summary` and `GET /api/jobs/summaries?limit=` (token-protected);
  the UI's job detail shows a "Why was this slow?" panel from it: the dominant share and a
  one-line interpretation (`receiver-bound 71 %: the drive's fsync rate; see slow-drive line`).
- **Bug bundle:** include the last 20 summaries.
- **Counters endpoint (optional, same data):** `GET /api/metrics` in Prometheus text format
  (jobs by result, bytes, stall count, sweep failures, BUSY refusals, auth refusals) for the
  homelab users who already run Prometheus; cheap once the summaries exist.

## Entry points
- Engine: where `JobSummary` is printed (`send.rs`/`governor.rs`), the job registry that the jobs
  API reads (`lib.rs` running-job snapshots with `bottleneck`/`settling`/`skipping`).
- Console: `ava1_apply_summary` (text) is enough for v1; a typed `job.summary` method later.
- Client: job detail screen.

## Tests
Summary written for success, cancel, stall, cross-device; rotation at 200; API returns it;
bundle includes them; a unit test for the interpretation line per dominant share.

## Acceptance
A stalled or slow job produces one JSON the user can attach, and the UI explains the dominant
cause in one sentence.
