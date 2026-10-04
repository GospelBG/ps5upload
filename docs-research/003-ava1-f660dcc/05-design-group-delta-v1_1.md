# Design: group-level delta for changed files (AVA1 v1.1, `CAP_DELTA`)

Status: proposed as the first post-cutover protocol feature. Rationale in
`01-whole-system-review.md` §6.1: a game update changes a fraction of a tree; AVA1 already
has 1 MiB verification groups, outboards and a `verify` policy, so sending only the changed
groups needs two messages and one flag.

## 1. Negotiation

`caps` bit 2 `CAP_DELTA` (4) in `ServerInfo`/`HelloInfo`. A sender uses delta only with a
receiver that advertised it. `JobOpen.flags` bit 5 `JF_DELTA` (32): "for files that exist with
the same size and a different root, exchange group CVs and send only the differing groups".
Valid with policy `verify` only (`ERR_PROTOCOL` otherwise). Receivers without the cap answer
`ERR_PROTOCOL` to the flag, which the sender never sets for them.

## 2. Receiver side (`prepare`, policy pass)

Today `file_matches` hashes an existing same-size file and marks it done when the root
matches. With `JF_DELTA`, for a same-size file whose root differs:

1. The hash pass keeps the group CVs it computed (`ava1_b3_group_cv` per group while hashing;
   the pass already reads the whole file) in `<job dir>/<id>.ob.have`.
2. The existing file is **renamed** to the part path (`<path>.ava-part`, same directory) and
   the rename journaled as `JnlAdopt{file_id}` (kind 7), so recovery knows the part file holds
   the user's previous bytes and must not be treated as a fresh download. A staged job (no
   existing files) never reaches this.
3. The file is listed in `JobMap` ext tag 2 `mismatch: records FileRun`.

## 3. Sender side

For each file in `mismatch`, the sender hashes it (it must read it anyway) and sends its group
CVs:

```
FileCvs   type 0x30, control, sender → receiver
  { job_id b16, file_id u32, first_group u32, cvs bytes }     cvs = 32 B per group,
  at most 1,800 groups per message (≤ 60 KiB); several messages for a larger file
FileCvsAck type 0x31, control, receiver → sender
  { job_id b16, file_id u32, adopted: records FileRange }     whole groups the receiver
  now holds durable; sent once per file after its last FileCvs page
```

The sender treats `adopted` like map `partial` ranges: those groups are neither read nor
sent; its outboard takes the receiver's CVs for them (they are the sender's own CVs, echoed
back as ranges). The remaining groups go as `Chunk`s. The sender must not send any `Chunk`
of that file before `FileCvsAck` arrives (the receiver would be comparing concurrently).

## 4. Receiver on `FileCvs`

Compare each CV with `<id>.ob.have`; copy matching CVs into the real outboard `<id>.ob`,
add the ranges to `durable` (they are on disk in the renamed file and were just hashed from
it), journal them as a `JnlBatch{ranges}` (fsync; the file's bytes are already durable, the
rename is made durable by the directory fsync in the same batch), answer `FileCvsAck`. Groups
that differ are simply not adopted; the sender's chunks overwrite them in the part file as in
any resume. Commit is today's: root from the outboard equals the sender's `FileRoot`, truncate,
fsync, rename back into place.

Different sizes: no delta in v1.1 (a `mismatch` entry is only ever same-size). Prefix
matching for grown files is a later extension.

## 5. Failure and cancel

- The job is cancelled or fails after the rename: the user's file is the part file. The
  resume path (`reconcile`) sees `JnlAdopt` and treats the part file as a valid partial with
  the adopted ranges; `job.cancel` is told by the engine to **rename it back** when the user
  abandons the job (`JobCancel{reason = ERR_CANCELLED}` triggers it; today's cancel leaves
  `.ava-part` behind for a resume).
- A `FileCvs` for a file not in `mismatch`: `ERR_PROTOCOL`.
- The receiver's hash pass costs one read of each candidate file; unchanged from `verify`.

## 6. Why this beats the alternatives

rsync's rolling checksum finds shifted content but costs CPU on both ends and a weak hash
pass; game assets are rewritten in place, not shifted, so fixed 1 MiB groups catch nearly all
of the savings at zero extra hashing on the receiver (it hashes for `verify` anyway) and 32
bytes per MiB on the wire. Syncthing's block exchange is the same idea with 128 KiB–16 MiB
blocks; AVA1's group size is already in that range.

## 7. Tests

- 4 GiB file, 3% of groups changed: bytes on the wire ≤ 4% of the file; `resent` groups equal
  the changed set; the result matches; the outboard equals a fresh hash.
- Cancel after adoption: the original file is back in place, byte-identical.
- Resume after a kill between `JnlAdopt` and the first chunk: no splice, adopted ranges kept.
- A receiver without `CAP_DELTA`: the flag is never sent; full upload.
