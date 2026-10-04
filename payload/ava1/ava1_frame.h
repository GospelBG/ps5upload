/* The 16-byte AVA1 frame header (SPEC.md §2). */
#ifndef AVA1_FRAME_H
#define AVA1_FRAME_H

#include <stddef.h>
#include <stdint.h>

#define AVA1_HEADER_LEN 16
#define AVA1_TAG_LEN 16
#define AVA1_MAX_BODY (16u * 1024u * 1024u)
#define AVA1_FLAG_SEALED 0x01u
#define AVA1_FLAG_IGNORABLE 0x02u

typedef struct {
    uint8_t type;
    uint8_t flags;
    uint32_t channel;
    uint32_t body_len;
} ava1_header_t;

uint32_t ava1_crc32c(const uint8_t *p, size_t n);
void ava1_header_encode(const ava1_header_t *h, uint8_t out[AVA1_HEADER_LEN]);
/* 0, AVA1_E_MAGIC, AVA1_E_CRC or AVA1_E_TOOLONG. */
int ava1_header_decode(const uint8_t in[AVA1_HEADER_LEN], ava1_header_t *h);

/* Lane frame-buffer pool (review 003 section 3). A lane body is read into a buffer of one of
 * four size classes (1, 4, 8, 16 MiB) that is kept for the next frame instead of being
 * returned to the allocator, which hands a fresh 15 MiB block back to the kernel on free
 * and faults it in again (page zeroing) on the next malloc. Buffers are reused without
 * zeroing: the frame is read over them. A class holds at most budget/class idle buffers and
 * all classes together at most `budget` bytes, so the pool never keeps more than the
 * data layer's admit budget. Bodies of at most half a class-1 buffer are plain malloc. */
#define AVA1_FRAME_CLASSES 4
/* The class capacity serving `len` (1, 4, 8 or 16 MiB; a buffer has 64 KiB of slack beyond it
 * for a chunk's message header), or 0 when `len` is not pooled. */
size_t ava1_frame_class(size_t len);
/* A buffer of at least `len` bytes; *cap is its capacity, to be handed back to
 * ava1_frame_free (0 for an unpooled one). NULL when out of memory. */
void *ava1_frame_alloc(size_t len, size_t *cap);
/* Returns a buffer from ava1_frame_alloc. cap == 0 frees it; a second free of a buffer the
 * pool already holds is refused (returns -1) rather than corrupting the list. */
int ava1_frame_free(void *p, size_t cap);
/* The idle-memory ceiling (0 = 96 MiB, the default admit budget); trims the pool to fit. */
void ava1_frame_pool_set_budget(uint64_t bytes);
/* Idle buffers of class i (0..3), buffers handed out and not yet freed, and a full drain. */
size_t ava1_frame_pool_idle(int cls);
size_t ava1_frame_pool_outstanding(void);
void ava1_frame_pool_trim(void);

#endif
