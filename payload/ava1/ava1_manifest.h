/* The manifest store: Rust's Manifest, in C, with the same hash (SPEC.md §11). */
#ifndef AVA1_MANIFEST_H
#define AVA1_MANIFEST_H

#include <stddef.h>
#include <stdint.h>

#include "ava1_gen.h" /* ava1_manifest_entry_t, ava1_manifest_page_t */

#define AVA1_MAX_PATH 1024
#define AVA1_E_BADPATH (-20)
#define AVA1_PAGE_BYTES (60u * 1024u)

typedef struct {
    uint64_t size, mtime;
    uint32_t mode, path_off;
    uint16_t path_len;
    uint8_t kind, has_root;
    uint8_t root[32];
} ava1_ment_t;

typedef struct {
    ava1_ment_t *e;
    uint32_t n, cap, files;
    uint64_t bytes;
    char *arena; /* NUL-terminated paths */
    size_t arena_len, arena_cap;
} ava1_mstore_t;

int ava1_path_ok(const uint8_t *p, size_t n);
int ava1_mstore_add(ava1_mstore_t *m, const ava1_manifest_entry_t *e);
int ava1_mstore_add_page(ava1_mstore_t *m, const ava1_manifest_page_t *p);
void ava1_mstore_hash(const ava1_mstore_t *m, uint8_t out[32]);
const char *ava1_mstore_path(const ava1_mstore_t *m, uint32_t id);
int ava1_mstore_blob(const ava1_mstore_t *m, uint8_t **blob, size_t *len); /* malloc'd */
int ava1_mstore_from_blob(ava1_mstore_t *m, const uint8_t *blob, size_t len);
/* One ManifestPage; `*next` is the first entry not written. */
int ava1_mstore_page(const ava1_mstore_t *m, const uint8_t job[16], uint32_t *next, uint8_t *out,
                     size_t cap, size_t *len);
int ava1_mstore_walk(ava1_mstore_t *m, const char *root);   /* depth first, sorted */
int ava1_mstore_single(ava1_mstore_t *m, const char *file); /* JF_SINGLE_FILE */
void ava1_mstore_free(ava1_mstore_t *m);

#endif
