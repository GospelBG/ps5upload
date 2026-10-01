#include "ava1_keys.h"

#include <string.h>

#include "monocypher.h"

void ava1_identity_from_secret(ava1_identity_t *id, const uint8_t secret[32]) {
    memcpy(id->secret, secret, 32);
    crypto_x25519_public_key(id->pub, id->secret);
}

int ava1_dh(const ava1_identity_t *id, const uint8_t peer[32], uint8_t out[32]) {
    uint8_t acc = 0;
    int i;
    crypto_x25519(out, id->secret, peer);
    for (i = 0; i < 32; i++) acc |= out[i];
    return acc ? 0 : -1;
}

void ava1_lane_key(const uint8_t dir[32], uint16_t lane, uint8_t out[32]) {
    static const char label[] = "AVA1 lane";
    crypto_blake2b_ctx c;
    uint8_t l[2];
    l[0] = (uint8_t)lane;
    l[1] = (uint8_t)(lane >> 8);
    crypto_blake2b_keyed_init(&c, 32, dir, 32);
    crypto_blake2b_update(&c, (const uint8_t *)label, sizeof label - 1);
    crypto_blake2b_update(&c, l, 2);
    crypto_blake2b_final(&c, out);
}

void ava1_join_tag(const uint8_t dir[32], const char *label, const uint8_t sid[16], uint16_t lane,
                   const uint8_t nonce[16], uint8_t out[16]) {
    static const char join[] = "AVA1 join";
    crypto_blake2b_ctx c;
    uint8_t jk[32], l[2];
    crypto_blake2b_keyed(jk, 32, dir, 32, (const uint8_t *)join, sizeof join - 1);
    l[0] = (uint8_t)lane;
    l[1] = (uint8_t)(lane >> 8);
    crypto_blake2b_keyed_init(&c, 16, jk, 32);
    crypto_blake2b_update(&c, (const uint8_t *)label, strlen(label));
    crypto_blake2b_update(&c, sid, 16);
    crypto_blake2b_update(&c, l, 2);
    crypto_blake2b_update(&c, nonce, 16);
    crypto_blake2b_final(&c, out);
    crypto_wipe(jk, sizeof jk);
}

uint32_t ava1_pairing_code(const uint8_t hash[64]) {
    static const char label[] = "AVA1 pairing";
    crypto_blake2b_ctx c;
    uint8_t d[32];
    crypto_blake2b_init(&c, 32);
    crypto_blake2b_update(&c, (const uint8_t *)label, sizeof label - 1);
    crypto_blake2b_update(&c, hash, 64);
    crypto_blake2b_final(&c, d);
    return ((uint32_t)d[0] | ((uint32_t)d[1] << 8) | ((uint32_t)d[2] << 16) | ((uint32_t)d[3] << 24)) %
           1000000u;
}
