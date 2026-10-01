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

#endif
