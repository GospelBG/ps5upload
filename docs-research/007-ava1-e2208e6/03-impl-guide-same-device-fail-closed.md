# Implementation guide — make `same_device` fail closed (HW-1)

**For:** the implementation agent (ava1 branch). **Severity:** console safety; low likelihood,
catastrophic impact (a cross-device `rename()` panics this kernel). **Size:** tiny.

## Problem
`payload/src/ava1_glue.c:52` `same_device(from, to_dir)` returns 1 same / 0 crosses / **−1
unknown** (either `xdev_lstat_dev(from)` or `xdev_stat_dev(to_dir)` failed). Both callers in
`payload/ava1/ava1_apply.c` refuse only on 0:
- `commit_large` ~2680: `if (cfg->same_device && cfg->same_device(part, parent) == 0)`
- `finish` ~2807: `if (cfg->same_device && cfg->same_device(j->base, parent) == 0)`
So −1 proceeds to `rename()`. Today a failed stat on the parent almost always means the rename
fails too (ENOENT), not a panic, but the guard is fail-open on the one error we cannot afford.

## Change (two one-line edits + one message)
In both call sites, refuse unless the answer is a definite 1:
```
if (cfg->same_device) {
    int sd = cfg->same_device(part, parent);
    if (sd == 0) { commit_fail(j, AVA1_ERR_CROSS_DEVICE, "the destination is on another drive", 0, 1); return; }
    if (sd < 0)  { commit_fail(j, AVA1_ERR_IO, "could not verify the destination drive", errno, 1); return; }
}
```
Same shape in `finish` with `ava1_apply_fail`. Keep `cfg->same_device == NULL` (no guard
available) as today's behaviour **only** for host test builds; on the console the glue always
sets it (`ava1_glue.c:211`). Consider asserting it is non-NULL when `AVA1_PLATFORM_PS5`.

## Why the cost is acceptable
A false refusal costs one retry of the file/tree (the `.ava-part` stays; a resume finishes it).
A false allow costs a kernel panic. −1 should be essentially unreachable right before a rename
that would succeed, so this changes nothing in the normal path.

## Tests
- Host build: a fault hook that makes `same_device` return −1 → the commit fails with
  `ERR_IO` and the message, the part file remains, no rename attempted (assert via the
  existing `AVA1_HOOK_RENAMED` not firing).
- Existing cross-device tests (0 → `ERR_CROSS_DEVICE`) unchanged.

## Acceptance
Both sites fail closed on −1; test green; SPEC §11.6/§12.6 wording "st_dev checked" gains
"(unknown is refused)".
