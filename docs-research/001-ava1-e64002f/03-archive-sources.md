# AVA1 archive sources (7z, RAR, zip): research for the Task 11 fixes

Written against `ava1` at `e64002f` plus the Task 11 review notes relayed on 2026-10-03
(entry-to-file mismatch in mixed archives, archive identity tied to the file's date, missing
console-receiver and memory tests). The 7z and RAR AVA1 sources themselves are not pushed
yet; this document is built from the code they replace or reuse — the FTX2 7z and RAR
senders (`ps5upload-core/src/transfer.rs:3815-5066`), the AVA1 zip source
(`ps5upload-ava1/src/zip_source.rs`), the sender (`ava1/src/send.rs`), the receiver's resume
rules (`ava1_recv.c:478-618`) — and from probing `sevenz-rust2 0.23.0` (the pinned version)
with its own writer. Probe output is in §1; everything marked **[probed]** was observed here,
everything marked **[hyp]** needs the named experiment.

---

## 1. What `sevenz-rust2 0.23` actually does with entry order **[probed]**

The archive header lists `files` in writer order. Each *block* (7z "folder") holds the
sub-streams of consecutive `files` entries that have data; entries without data
(directories, zero-length files written as empty-stream items) sit in `files` wherever the
writer put them. `ArchiveReader::for_each_entries` (reader.rs:1650-1680) visits:

1. block 0's files in header order, then block 1's, … (`BlockDecoder::for_each_entries`,
   reader.rs:1860-1900: a `BoundedReader` of exactly `size` bytes per file, wrapped in a
   CRC-32 check when the header carries one; a zero-byte file *inside* a block is visited in
   place with an empty reader);
2. **then every entry without a stream, last** — directories and empty-stream files —
   whatever their header position.

Four layouts built with the crate's writer and read back (the probe program is reproduced
in §8; content of every file is its own name repeated, so a swap is visible):

| layout | header order | visit order | path-sorted (an AVA1 manifest) |
|---|---|---|---|
| A: non-solid; `a/` and `empty` pushed between files | `z.bin, a/, a/x.bin, empty, a/y.bin` | `z.bin, a/x.bin, empty, a/y.bin, a/` | `a/x.bin, a/y.bin, empty, z.bin` |
| B: one solid block, then `a/`, `empty` (7-Zip's own layout: empties appended) | `z.bin, a/x.bin, a/y.bin, a/, empty` | `z.bin, a/x.bin, a/y.bin, empty, a/` | `a/x.bin, a/y.bin, empty, z.bin` |
| C: zero-byte file as a sub-stream inside a solid block | `z.bin, empty, a/x.bin` | `z.bin, empty, a/x.bin` | `a/x.bin, empty, z.bin` |
| D: a directory passed *into* `push_archive_entries` | malformed: the writer gives the directory a sub-stream and `a/x.bin` comes back 0 bytes | — | — |

Consequences:

- **Three different orders exist** (header, visit, path-sorted) and they agree only by
  accident. Any mapping "k-th visited file ↔ k-th manifest file" pairs `z.bin`'s bytes with
  `a/x.bin`'s slot in every layout above; with equal sizes nothing downstream notices,
  because the bytes are valid 7z output and the receiver's size check passes. This is the
  mechanism of the Task 11 finding; the fix "match by name" is the right one and the
  FTX2 RAR sender already made the same change for the same reason
  (`transfer.rs:4822-4860`, `bind_plan_entry` with a `seen` set after a user hit a
  list/extract order mismatch).
- Directories arrive **after** every file, so a counter that includes directory visits
  drifts even when files alone would not.
- A per-file CRC failure surfaces (the crate wraps a `Crc32VerifyingReader`), a
  per-file *mapping* failure never does. The source must enforce the mapping itself.
- `read_file(name)` is O(archive) on a solid archive (its own doc comment); never use it
  per file. `BlockDecoder::new(threads, block_index, &archive, &pw, &mut source)` decodes
  one block independently and is the unit of random access.
- Never build a test archive with a directory inside `push_archive_entries` (layout D is
  the writer's bug, not a real layout); push directories with
  `push_archive_entry(ArchiveEntry::new_directory(..), None)`. Keep one small fixture made
  by real 7-Zip too (`7z a -ms=on`): its layout is B, but a second writer is the point.
- Multi-threaded decode is unsafe for memory: `Lzma2ReaderMt` buffers whole dictionary
  units, 2.3–4.4 GiB on a 2.5 GB corpus (`transfer.rs:3860-3876`). Keep
  `set_thread_count(sevenz_decode_threads())` (default 1).

## 2. The mapping fix, stated as invariants

The source keeps `name → (block index, position within the block, size, crc, mtime)` from
the header and never relies on visit order. Then, whatever the decode strategy (§4), check:

1. **Names are unique.** 7z permits duplicate paths; refuse the archive at plan time with
   the path named. (`ZipSource::open` silently keeps the last duplicate via
   `files.insert`, `zip_source.rs:105`; acceptable for zip where appended archives do this
   on purpose, but say so in a comment and log it.)
2. **Names are normalised the same way everywhere.** `sanitize_7z_entry` turns `\` into
   `/` and applies the zip-slip rules (`transfer.rs:3841`); after that
   `manifest::check_path`. The AVA1 zip source today applies only `check_path`, so a zip
   made on Windows with `\` in names lands on the console as one file literally named
   `a\b.bin` — harmless, but inconsistent with the 7z/RAR paths; normalise there too.
3. **Anti-items are skipped**, directories come from entries *and* implied parents
   (`zip_source.rs:71-82` does this), zero-length files are files (SPEC: a zero-byte file
   is complete without a frame; the C receiver finishes it up front).
4. **Each planned file is produced exactly once with exactly its declared size** —
   a short stream is corruption ("ends before its declared size", as `ZipEntryReader`
   reports), a long one is a header/stream disagreement; both terminal, typed like
   `ZipCorrupt` so the engine's `upload_zip_in` turns them into `ava1_7z_corrupt`, never a
   reconnect loop.
5. **mtime and mode come from the entry** (`last_modified_date` → Unix seconds, 0 when
   `!has_last_modified_date`; `windows_attributes` → mode when the Unix bits are present,
   else 0644), never from the archive file, never from the clock (§3).

## 3. Archive identity, and what "resume" keys on

Facts: an AVA1 job is identified by `job_id` = the HTTP request's `tx_id` (the queue entry's
id; `transfer_zip_handler`, `lib.rs:5739`, `:5931`, passed through `upload_zip_in` as the AVA1
job id). On the console a `JobOpen` for a known id is a resume; the stored manifest is
matched to the new one **by path**, and an entry keeps its progress only when `kind`, `size`
and `mtime` are equal (SPEC §11.5; `remap`, `ava1_recv.c:490-549`). `Resume` (the fast path)
additionally needs the manifest hash to be identical (`ava1_recv.c:1087-1091`).

So the reported behaviour — "copying or touching an otherwise identical archive made AVA1
treat it as a new archive" — can only come from the container file's mtime (or path)
leaking into the manifest entries or into the id. The rule:

- **Manifest entries carry only archive content**: entry mtime (0 if absent), entry size,
  entry mode, sanitised path. `ZipSource` uses mtime 0 throughout, which is correct.
- **Do not run `apply_existing_policy` on an archive source.** With mtime 0 it selects
  `verify`, which hashes every file *before* the first byte is sent
  (`upload.rs:235-263`): for a solid 7z that is a full extra decode of the archive. A
  "skip files the console has" feature for archives must use the roots computed during the
  one decode pass (they already travel in `BundleRecord`/`FileRoot`), i.e. the console's
  `RETRY`/`done` map, not a pre-pass.
- **A content fingerprint for the engine's own caches** (inspect/plan caches, the sender's
  `persist` outboards, and a stable id for "upload this archive again"):
  7z: `BLAKE3("ava1 7z" ‖ u64le(file size) ‖ the 32-byte start header ‖ the packed header
  bytes)` — the start header holds `NextHeaderOffset/Size/CRC`, so the header bytes are a
  cheap, content-derived summary of the whole directory. Zip: the central directory bytes
  plus size. RAR: the listing pass's `(name, size, crc)` per entry plus size. Never path or
  mtime. When a request carries no `tx_id`, deriving `job_id = BLAKE3(dest_root ‖
  fingerprint)[..16]` makes re-adding the same archive to the same destination resume on
  the console instead of starting a parallel job that `root_in_use` refuses with
  `ERR_BUSY` (`ava1_data.c:756-770`).
- **Manifest order must be deterministic across attempts** so `Resume`'s hash matches:
  path-sorted (what `ZipSource` and `walk` do) or decode order (§4), but one of them, fixed.
  A changed order still resumes through `JobOpen` + remap-by-path; it just loses the fast
  path.

Test: upload half, kill the session, copy the archive to another name, `touch` it, resume
with the same job id; assert `Manifest::hash` is equal across the two opens, the console's
`JobMap` is the old one, and `SendReport.resent == 0` (or a few groups).

## 4. A forward-only decoder behind a random-access `Source`

`run_upload` reads through `Source::open(rel) → ReadAt::read_at(off, buf)`: `readers` threads
pop *small* files from a shared queue in parallel and in no particular order, one thread
walks *large* files in manifest order, and on resume `pieces()` reads only the groups the
receiver lacks (`run_upload` splits the queues at `opts.cutoff`, `send.rs:1044-1110`; `spawn_readers` `:561`; `pieces` `:129`). That is random access; LZMA2 is not.

**Non-solid archives** (one block per file; `archive.is_solid == false`, or every block has
one sub-stream): each file is its own `BlockDecoder`. Mirror `ZipEntryReader`
(`zip_source.rs:177-296`): keep the decoder between sequential reads, restart it on a
backward seek, count restarts as a test seam, verify the CRC on a full front-to-back pass.
Parallel readers are fine (each opens its own `File`).

**Solid archives** (one block, many files): a per-file reader would decode the block from
its start for every file — O(n²). Read the block once, in order:

- `SendOptions { readers: 1, cutoff: 0 }` — every file is a "large" file (one `Chunk`
  stream plus a `FileRoot` each), read by the single large-file reader in manifest order.
  This is what `serve_download` already does for `JF_ORDERED` (`send.rs:1557-1570`). No
  bundling for small files: FTX2 made the same trade for 7z (`transfer.rs:3826-3831`).
- **The manifest order must be the decode order**: blocks in index order, files within a
  block in header order, directories and empty files anywhere (they carry no bytes). The
  receiver does not care about manifest order (only `JF_ORDERED` constrains it, and the
  console creates every directory before any data). With that, the one reader's queue walks
  the block front to back and the decoder never restarts.
- The `ReadAt` for a solid file is a view onto a shared block cursor: `read_at(off)` for the
  file the cursor is on returns its bytes; a request for an earlier file restarts the block
  (count it; a test asserts zero restarts on a clean run); a request for a later file skips
  forward by decoding into a discard buffer. The relay's `Turn`/`take` shape
  (`relay.rs:299-470`) is the reference for "readers take turns on one stream".
- **Resume** on a solid archive re-decodes the block up to the first missing group (CPU
  only; no network). `pieces()` with the sender's persisted outboards (`persist`) avoids
  re-hashing but cannot avoid re-decoding — the decoder has to produce the bytes to advance.
  Show "re-reading the archive" in the UI and prefer the nearest block start when the
  archive has several blocks (`-ms=…` splits). This matches the first design review §4.6.
- Pick the strategy per archive at open: non-solid → parallel per-block readers; solid →
  ordered single reader. A mixed archive (several solid blocks) is "solid" for this
  purpose.

**Memory**: the sender's read-ahead permits bound frames to `READ_AHEAD_KIB` (96 MiB,
`send.rs:424`); the decoder's dictionary is on top (64 MiB for 7-Zip's default ultra,
up to 1.5 GiB for `-md=1536m`), and `thread_count` must stay 1. Expected peak =
dictionary + ~100 MiB + constant, independent of archive size — the property the memory
test (§6.4) pins.

**Cancel**: `run_upload` calls `source.close()` before joining the readers (`send.rs:1469`).
A 7z reader blocks only on local file I/O, so `close()` can be a flag the next `read_at`
checks; the RAR source needs more (§5).

## 5. RAR

Keep the FTX2 worker (`spawn_rar_worker`, `transfer.rs:4640-4800`): UnRAR pushes bytes
through `read_to_sink`, the worker frames them as `Entry/Chunk/EntryEnd` on a 4-slot bounded
channel, and the consumer pulls (`rar_stream.rs`). Wrap it as a `Source` exactly like the
solid-7z case: `readers: 1, cutoff: 0`, manifest in archive order (the listing pass —
`rar_plan_entries` — gives it), each `open(rel)` waits its turn, `read_at` pulls the
entry's chunks. UnRAR's listing and processing passes can enumerate in different orders
(`transfer.rs:4938`, `:5488`): bind by name, keep a `seen` set, and refuse a stream whose
entry set is not the plan's.

- **Password failures are terminal.** `rar_password_required` / `rar_password_wrong`
  (`map_rar_err`, `transfer.rs:4293-4300`; BadData on an encrypted entry with a password,
  `:4767`) must reach the engine as `UploadFailure { reason }`, not as `SendError::
  Disconnected`: `upload_with_in` reconnects and retries only on `Disconnected`
  (`upload.rs:389-392`) and returns every other error at once (`:406`) — so the source must
  surface them as `SendError::Source(io::Error)` carrying the typed reason, and the
  terminal mapper must keep the reason string (the zip path's `is_zip_corrupt` chain walk
  is the pattern, `zip_source.rs:31-46`, `upload.rs:54-65`). Ideally the listing pass (no
  decode) fails first, before any `JobOpen`.
- **Cancel inside a solid entry.** UnRAR blocks in `read_to_sink`; the only way out is the
  sink's channel send failing (`StreamSink::disconnected`, `transfer.rs:4745-4760`). The
  source must own the `Receiver`, and `Source::close()` must drop it (and set a flag the
  forwarder checks), so the worker unwinds and `run_upload`'s join returns within the
  stall bound rather than after the whole entry. Test: cancel 1 s into a 512 MiB solid
  entry, assert the join returns in < 5 s and the worker thread is gone.
- Multi-volume: keep `missing_volume` so a missing part is named.

## 6. The tests Task 11 is missing

1. **Mixed-layout correctness against the real console receiver.** Build the tree
   `{z.bin, a/, a/x.bin, a/y.bin, empty, deep/er/f.bin}` with equal sizes and content =
   path; archive it three ways with the crate's writer (layouts A, B, C of §1) and once
   with real 7-Zip as a committed fixture; upload each through `ava1-ctest`'s C receiver
   (`CServer::start` + the `common::upload` loop, `ava1-ctest/tests/common/mod.rs:63-108`,
   `same_tree`), byte-compare every file. Then the same with `ChaosProxy::kill_all` after
   the first `Durable` and a resume on the same job id. Expect: all bytes equal, zero
   directory/empty-file frames, `resent` bounded.
2. **Identity across copy/touch** (§3's test).
3. **Solid resume cost**: count `BlockDecoder` constructions in the source; a clean solid
   upload constructs one per block; a resume constructs one more and sends only the missing
   groups.
4. **Memory**, `#[ignore]`d and run in its own process (peak RSS is process-wide): build a
   2 GiB solid archive of random bytes at test time (copy or fastest LZMA2 level — the
   point is size, not ratio), upload it to the C receiver with the default window, read
   `VmHWM` from `/proc/self/status` (Linux; `ru_maxrss` elsewhere — there is no helper in
   the tree yet) and assert `< dictionary + 256 MiB`. Run once with
   `PS5UPLOAD_7Z_THREADS=4` to document the failure mode it guards.
5. **Corruption and refusals**: truncated archive → `ava1_7z_corrupt`, no reconnect
   (count sessions); duplicate names → refused at plan; `..`/absolute/`\` paths →
   normalised or refused per §2; an anti-item ignored.
6. **RAR**: wrong password → terminal reason with one session; cancel during a solid entry
   returns promptly; a missing volume is named.

## 7. Recommendations, in order

1. Bind by name with the invariants of §2 (the fix in flight), plus the duplicate-name and
   exact-size checks the review did not mention.
2. Content-only manifests and fingerprints (§3); grep the new source for any use of the
   container's `metadata().modified()` and for `apply_existing_policy`.
3. Decide the two strategies of §4 at open time; make the manifest order the decode order
   for solid archives; `readers: 1, cutoff: 0` there.
4. RAR terminal errors and `close()` (§5).
5. The six tests of §6, 1–3 before the branch is merged, 4–6 before the cutover release.

## 8. The probe

`sz-span/src/main.rs`, a 90-line program against `sevenz-rust2 = { version = "0.23",
default-features = false, features = ["compress"] }`, builds layouts A–D with
`ArchiveWriter::{push_archive_entry, push_archive_entries}`, prints `Archive::open`'s
`files`, `stream_map.file_block_index`, `block_first_file_index`, then the names and byte
counts `ArchiveReader::for_each_entries` yields, checking each file's bytes against its
name. Its output for A–D is the table in §1. It is worth keeping as a regression test in
`ps5upload-core` (which already depends on the crate; add `compress` as a dev-feature) so a
crate upgrade that changes the visit order is caught before a user is.
