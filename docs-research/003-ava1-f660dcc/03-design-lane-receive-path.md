# Design: the lane receive path and the governor's lane/chunk policy

Status: proposed for `ava1`; no wire change. Reasoning in `01-whole-system-review.md` §2.
Measure first (§4 of this note) — if the matrix shows no gap at 4 lanes × 4 MiB, only §1 and
§3 are worth doing.

## 1. Socket buffers (one line each)

- Console, `accept_main` (`ava1_server.c:1080`): `setsockopt(SO_RCVBUF, 4 MiB)` and
  `SO_SNDBUF 4 MiB` on every accepted socket before the first read (the kernel caps at
  `kern.ipc.maxsockbuf`; take what it gives, log once).
- Engine, `Joiner::join` (`session.rs`): `set_recv_buffer_size`/`set_send_buffer_size` 4 MiB
  via `socket2` before connect (tokio's `TcpSocket` exposes both).

Why: a lane thread that is not in `recv` for 10–20 ms must not close the sender's TCP
window; and on Wi-Fi the per-lane window is today's ceiling.

## 2. Decrypt off the reader thread (console)

Today `ava1_conn_recv_body` opens the frame on the lane thread. Change:

1. `ava1_conn_recv_body_sealed(c, buf, len, ctr_out)`: reads body + MAC, assigns
   `*ctr_out = c->recv_ctr++`, returns without opening. The reader keeps the per-lane order
   of counters; frames may be opened in any order because each carries its counter.
2. `serve_loop` hands `(sid, lane, seq, ctr, hdr[12], body, mac)` to the data layer
   (`data_on_lane_sealed`), which opens it on a data worker (`ava1_open` with the lane's
   recv key — the key is copied into the frame descriptor at receipt; a lane re-key on
   re-join cannot affect frames already received). On success the worker continues into
   today's `data_on_lane`; on a bad tag it posts `Error(ERR_PROTOCOL)` on the lane and asks
   the server to close it (`ava1_server_close_lane(sid, lane)`), exactly what the reader does
   today on `AVA1_E_TAG`.
3. `Received` is sent after the open succeeds (as today), so nothing unauthenticated is
   acknowledged. The admit budget (`data_admit`, 96 MiB) bounds sealed frames in flight.
4. Control lane (0) keeps inline open: its frames are small and ordering matters for RPCs.

## 3. Frame buffer pool (console)

`ava1_frame_alloc(len)` / `ava1_frame_free(p, cap)`: a per-size-class free list (1, 4, 8, 16
MiB classes) holding at most `budget / class` buffers, so memory never exceeds today's admit
budget. Buffers are reused without zeroing (the frame is read over them). Removes the
page-fault cost of a fresh 15 MiB allocation per frame.

## 4. Measurement before and after

Add two knobs to the lab harness (env on the engine): `PS5UPLOAD_AVA1_LANES=n` pins lanes,
`PS5UPLOAD_AVA1_CHUNK=m` pins the chunk (MiB). Run the 4 GiB upload on `/data` for
lanes ∈ {2, 4, 8} × chunk ∈ {1, 4, 15}, plus `crypto.bench mib=64`, and record per run the
sender's credit-starved tick share and the console's `data`/`dirs`/`journal` ms per batch
(already printed every 10 s). Keep the table in CUTOVER §4.

## 5. Governor changes (`governor.rs`), after the matrix confirms

- Lane probe gain `GAIN` 1.10 → 1.05 while the measured rate is below 90% of the link's
  best observed rate (the governor keeps `best_rate`), 1.10 otherwise.
- Prefer lanes before chunk: do not double the chunk past 4 MiB while `lanes < 4` and the
  bottleneck is the network; grow it only after lanes stop helping.
- Receiver hint: the console's `JobOpenAck.workers` is already a hint; add ext tag
  `open_us_per_mib` (from `crypto.bench` at start) so the sender can cap the chunk at
  `max(1 MiB, 10 ms / open_us_per_mib)` on a slow console. Optional; the matrix decides.

## 6. Slow drives (the Phat's usb0)

In `sync_batch`, measure the data fsync duration; when it exceeds the window's worth of data
at the current rate (`credit / rate`), switch the job to **per-chunk fsync**: the worker that
wrote a chunk fsyncs the part file immediately after its `pwrite` (small flushes keep the
device streaming and the vnode lock short), and the batch fsync becomes a no-op barrier. Also
time `posix_fallocate` in `lfile_open` and log it once per job; on a filesystem where it
takes longer than 1 s per GiB, log "preallocation on this drive is slow" so CUTOVER can
record it per drive. Preallocation itself stays (FTX2's lesson, `runtime.c:3593`), but move
it out from under `j->mu`: open and preallocate in the worker before taking the mutex to
publish the descriptor.
