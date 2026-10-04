/* Symlink-safe path policy (shared by runtime.c's is_path_allowed and the host tests). */
#ifndef PS5UPLOAD_PATH_POLICY_H
#define PS5UPLOAD_PATH_POLICY_H

/* Does `p` resolve to somewhere `lexical_ok` accepts?
 *
 * `lexical_ok` is the pure string rule (the allowed roots, no `..`). It is applied to `p` AND to
 * the canonical form of `p`, so a symlink that leads out of the allowed roots is refused.
 *
 * A path that does not exist yet (a write, a mkdir, a rename's destination) cannot be
 * realpath()ed, and falling back to the lexical rule alone let a NONEXISTENT LEAF UNDER A
 * SYMLINKED PARENT through (`/data/link/new` with `/data/link -> /system_ex`). So the deepest
 * EXISTING ancestor is resolved and the policy is re-run on that canonical ancestor joined with
 * the components that do not exist yet. A final component that is a symlink whose target is
 * missing (a dangling link) is refused outright: creating through it would write wherever it points.
 * Returns 1 allowed, 0 refused. */
int path_resolve_allowed(const char *p, int (*lexical_ok)(const char *));

/* The trust store. `/data/ps5upload/ava` holds this console's AVA1 identity and its list of paired peers;
 * a paired peer that could overwrite, delete or read them could hijack the console's trust. So the
 * directory and everything under it is denied to EVERY path policy that goes through this file
 * (runtime.c's is_path_allowed, so the FTX2 handlers and AVA1's fs.* / job ops, and the FTP server).
 *
 * path_in_protected: `p` is the directory or below it, judged on three forms of the path: as written,
 * lexically normalised (`//`, `.`, `..` collapsed), and canonical (symlinks resolved, with the deepest
 * existing ancestor resolved for a path that does not exist yet). Names compare case-insensitively
 * (a case-insensitive filesystem would otherwise open a hole; denying a harmless extra spelling on a
 * case-sensitive one costs nothing).
 * path_contains_protected: `p` IS the directory or one of its ANCESTORS (renaming or deleting
 * /data/ps5upload takes the trust store with it): destructive operations on a source path refuse these too. */
int path_in_protected(const char *p);
int path_contains_protected(const char *p);
/* Test hook: the protected directory (NULL restores /data/ps5upload/ava). */
void path_policy_set_protected(const char *dir);

#endif
