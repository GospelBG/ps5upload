#include "ava1_frame.h"

#include "ava1_wire.h"

uint32_t ava1_crc32c(const uint8_t *p, size_t n) {
    uint32_t crc = 0xFFFFFFFFu;
    size_t i;
    int k;
    for (i = 0; i < n; i++) {
        crc ^= p[i];
        for (k = 0; k < 8; k++) crc = (crc & 1u) ? (crc >> 1) ^ 0x82F63B78u : crc >> 1;
    }
    return crc ^ 0xFFFFFFFFu;
}

static void put32(uint8_t *b, uint32_t v) {
    b[0] = (uint8_t)v;
    b[1] = (uint8_t)(v >> 8);
    b[2] = (uint8_t)(v >> 16);
    b[3] = (uint8_t)(v >> 24);
}

static uint32_t get32(const uint8_t *b) {
    return (uint32_t)b[0] | ((uint32_t)b[1] << 8) | ((uint32_t)b[2] << 16) | ((uint32_t)b[3] << 24);
}

void ava1_header_encode(const ava1_header_t *h, uint8_t out[AVA1_HEADER_LEN]) {
    out[0] = 'A';
    out[1] = '1';
    out[2] = h->type;
    out[3] = h->flags;
    put32(out + 4, h->channel);
    put32(out + 8, h->body_len);
    put32(out + 12, ava1_crc32c(out, 12));
}

int ava1_header_decode(const uint8_t in[AVA1_HEADER_LEN], ava1_header_t *h) {
    if (in[0] != 'A' || in[1] != '1') return AVA1_E_MAGIC;
    if (get32(in + 12) != ava1_crc32c(in, 12)) return AVA1_E_CRC;
    h->type = in[2];
    h->flags = in[3];
    h->channel = get32(in + 4);
    h->body_len = get32(in + 8);
    if (h->body_len > AVA1_MAX_BODY) return AVA1_E_TOOLONG;
    return 0;
}
