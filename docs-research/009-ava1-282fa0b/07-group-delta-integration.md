# Design 7 — group delta v1.1: integration points after durable-by-log

The design is `003-ava1-f660dcc/05-design-group-delta-v1_1.md` (CAP_DELTA, `JF_DELTA`,
`FileCvs` 0x30 / `FileCvsAck` 0x31, `JnlAdopt`). Nothing of it has landed (`grep CAP_DELTA|
JF_DELTA|FileCvs|JnlAdopt protocol/ava1` is empty). Since it was written, durable-by-log and the
pack log landed, which changes where it plugs in:

- **Receiver CV source:** a file already on the console has its BLAKE3 group CVs in its
  outboard (`<id>.ob`) only while a job is open; for an *installed* file there is no outboard.
  v1.1 therefore computes CVs on demand on the console (`reread_ranges`/`read_root` machinery in
  apply.c already hashes groups) with a per-job budget, exactly as the design says; durable-by-
  log does not change this, but the **sweep must have settled** the file before its CVs are read
  (a logged small file may still be in the pack) — small files are below one group anyway, so
  delta applies to large files only; state this in the SPEC text.
- **Adopt vs. durable-by-log journal:** `JnlAdopt` (a group adopted from the existing file,
  not received) is a new journal record kind; the engine's `journal.rs` `Record` enum and the
  console's kinds table both gained `K_SWEEP` since — allocate `JnlAdopt` the next kind, and make
  `apply`/snapshot treat adopted groups as durable ranges (they are: the bytes are already on
  disk and fsynced).
- **Sender side:** `seq.rs` `one_pass` already has "a file whose every CV is known needs no
  decoding" — the adopted groups just become `durable` ranges in `Want`, so the existing
  partial-file path sends only the missing groups. Minimal new code.
- **Credit/flow:** `FileCvs` are control-lane frames bounded like `JobMap` pages
  (`MAP_PAGE_ITEMS`); reuse the paging.
- **Negotiation:** `CAP_DELTA` in `ServerInfo` caps, `JF_DELTA` on `JobOpen`; absence = today's
  behaviour, so it is backward compatible with every helper in the field.
- **Order of work:** console `FileCvs` (compute + page) → engine adopt + `JnlAdopt` → sender
  wants → tests (unchanged file re-upload sends only roots; one changed group sends one group;
  a truncated/grown file falls back to full send) → SPEC §delta.

Size M once the cutover ships. It is the feature that most moves "best transfer protocol":
re-uploading a patched 60 GB game sends the changed megabytes, not the game.
