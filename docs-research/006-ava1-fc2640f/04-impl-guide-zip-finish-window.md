# Implementation guide — close the zip "whole but not finished" resume window

**For:** the implementation agent (ava1 branch). **Reviewed design:** 006/01 §1, 005/01 §5.
**Severity:** correctness (avoids a needless full-archive restart). **Size:** small, one file.

## Problem
On a stored-zip download, if the connection drops after a file's final range is journaled
but before its `Done`/commit, resume calls `StoredZipSink::position` with a partial range that
already covers the whole file. Today `position` rejects that (`x >= size`, zip_stored.rs:592,
"file N is whole but not finished") and the receiver restarts the entire archive. It is one
batch wide per file boundary, and it is unnecessary.

## Why the fix is safe (already proven)
On resume the sender runs `seq.rs::one_pass`: a file whose durable ranges cover every group
and whose outboard CVs are known sends **only** `Read::Root`, never data (seq.rs:262-288), and
in an ordered download every file takes that large path. The receiver verifies the root and
calls `Sink::commit(id)` (recv.rs:1680). So the descriptor can be written from `commit`. If
instead the outboard was thin and the sender re-sends data from offset 0, `append` already
returns `zip_restart` (zip_stored.rs:453-455) — the same restart as today. So the change
removes the restart only in the root-only case and never regresses.

## Changes — `engine/crates/ps5upload-ava1/src/zip_stored.rs`

### 1. `position` — accept a whole-but-unfinished in-flight file (around line 585-598)
Replace the `x >= size` rejection with an `x > size` rejection, and let `x == size` through as
the in-flight slot:
- Keep the existing `[(0, x)]` prefix check.
- `if x > layout.slots[pk].size { return Err(bad(... "durable bytes exceed the file")) }`.
- For `x == size`: set `partial_bytes = x` (= size), `cut = data_off + size` (the descriptor
  was never written, so the cut sits exactly at end-of-data), `in_flight = Some(pk)` — the
  same as the `x < size` path. The CRC read-back loop below already rebuilds the running CRC
  over `partial_bytes` bytes, which now covers the whole file. No special-casing needed there.

After this, `st.current = Some(pk)`, `st.written = size`, `st.pos = data_off + size`,
`st.crc` = full-file CRC. That is precisely the state `append` would be in the instant before
it writes the descriptor.

### 2. `commit` — write the descriptor for a finished-but-uncommitted in-flight slot (line 552)
`commit` is a no-op today. Make it finalize the in-flight slot when its bytes are all present:
```
fn commit(&self, _id: u32) -> io::Result<()> {
    let mut st = self.st.lock().unwrap();
    let layout = st.layout.clone();
    if let Some(k) = st.current {
        let slot = &layout.slots[k];
        if st.written == slot.size {
            let crc = st.crc.clone().finalize();
            let file = Self::file(&st)?;
            write_all_at(&file, &slot.descriptor(crc), st.pos)?;
            st.pos += DESC_LEN;
            st.crcs[k] = Some(crc);
            st.current = None;
        }
    }
    Ok(())
}
```
This mirrors the completion block in `append` (zip_stored.rs:470-475). Keep it a no-op when
`current` is `None` or `written < size` (the ordinary case where `append` already wrote the
descriptor). `commit` takes the id but does not need it — the in-flight slot is unambiguous;
optionally `debug_assert_eq!(layout.slots[k].id, _id)`.

### 3. Leave `append` unchanged
For the in-flight slot after the fix, an offset-0 write is the existing `zip_restart` path and
any other write is past-size — both already correct.

## Edge cases to keep
- `x > size` must stay an error (corrupt journal).
- A file that is both whole and already has its descriptor (normal commit after a full
  `append`) must not double-write: that path has `current == None` by the time `commit` runs,
  so the new `commit` is a no-op. Verify this ordering holds in the receiver (commit is called
  after the batch that set `current = None`).

## Tests — add to the `tests` module in `zip_stored.rs`
1. **whole-but-unfinished resume:** journal a partial `[(0, size)]` for the last file (no
   `Done`), call `position`, assert it succeeds, `current == Some(last)`, `written == size`,
   and the archive length is `data_off + size` (descriptor not yet present). Then call
   `commit(last)` and assert the descriptor is written, `crcs[last].is_some()`, `current ==
   None`, and `finish` produces an archive that `unzip -t` / the existing bsdtar check accepts.
2. **no regression on data re-send:** after that `position`, call `append(last, 0, &bytes)` and
   assert it returns `zip_restart` (unchanged behaviour).
3. **x > size rejected:** journal `[(0, size+1)]`, assert `position` errors.

## Acceptance
- New tests green; existing `zip_stored` tests still green.
- Strike the "restart window" row in `protocol/ava1/CUTOVER.md` §2 and note the fix.

## Validate locally
```
cargo test -p ps5upload-ava1 zip_stored
cargo test -p ps5upload-ava1            # full crate, nothing else regressed
```
