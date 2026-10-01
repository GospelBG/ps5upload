/* Identities and the AVA1 derivations (SPEC.md §4). Mirrors engine/crates/ava1/src/keys.rs. */
#ifndef AVA1_KEYS_H
#define AVA1_KEYS_H

#include <stddef.h>
#include <stdint.h>

typedef struct {
    uint8_t secret[32];
    uint8_t pub[32];
} ava1_identity_t;

void ava1_identity_from_secret(ava1_identity_t *id, const uint8_t secret[32]);
/* 0, or -1 when the shared secret is all-zero (low-order peer key). */
int ava1_dh(const ava1_identity_t *id, const uint8_t peer[32], uint8_t out[32]);
void ava1_lane_key(const uint8_t dir[32], uint16_t lane, uint8_t out[32]);
void ava1_join_tag(const uint8_t dir[32], const char *label, const uint8_t sid[16], uint16_t lane,
                   const uint8_t nonce[16], uint8_t out[16]);
uint32_t ava1_pairing_code(const uint8_t hash[64]);

#endif
