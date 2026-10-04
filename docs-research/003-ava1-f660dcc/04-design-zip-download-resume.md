# Design: stored zip downloads with resume

Status: proposed for `ava1`; removes the CUTOVER §2 regression "zip downloads restart from
zero on a reconnect". No wire change: the ordered receiver already gets durable ranges.

## 1. Change the archive format: Stored entries

`download.rs` writes Deflate (`SimpleFileOptions::compression_method(Deflated)`, line 347).
Use `Stored`. Game data is already compressed (Deflate gains little and costs a CPU core on the
engine); with Stored the archive is a concatenation of `local header ‖ data` per entry plus
the central directory, so every entry's data lives at a known offset and the archive can be
truncated and continued.

## 2. Resume at entry granularity (first step)

The zip sink is an ordered receiver (`JF_ORDERED`). On a reconnect, today the engine opens a
fresh job. Instead:

- The sink journals, per completed entry, `(file_id, archive_offset_of_local_header,
  data_offset, crc32)` in the job's own journal (`recv.rs` already has the journal; add a sink
  state record `ZipEntry`), after the batch fsync.
- On resume (same job id, same manifest hash), the sink truncates the archive to the end of
  the last journaled entry, re-opens the writer positioned there with the recorded entries
  list (so the central directory is complete at the end), and reports those files done in
  the `JobMap` as any receiver does. The sender skips them.

This already matches FTX2 for every entry but the one in flight.

## 3. Mid-entry resume (second step)

With Stored entries the only per-entry state is the running CRC-32 and the byte count. Journal
`(file_id, bytes_written, crc_state)` with each batch (16 bytes). On resume, truncate to
`data_offset + bytes_written`, restore the CRC state, and report the entry's durable range
`[0, bytes_written)` in the map. The sender sends the rest of the groups; the receiver
appends. The local header's size and CRC fields are written when the entry completes (the
writer already uses data descriptors for streaming), so a truncated entry has no stale header.

## 4. Tests

- A download to zip killed at 40% resumes with ≤ one entry resent (step 2), then ≤ one group
  (step 3); the archive opens with `zip` and every file matches.
- The journal's `ZipEntry` records replay to the same offsets the file has (`fstat` size equals
  the last entry's end before truncation).
- Deflate stays available behind an option for users who want smaller archives of text-heavy
  trees; it does not resume mid-entry and says so.
