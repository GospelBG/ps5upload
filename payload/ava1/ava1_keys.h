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
/* keyed BLAKE2b-256(dir, "AVA1 lane" ‖ u16le(lane) ‖ client_nonce ‖ server_nonce): fresh
 * nonces per join, so a re-joined lane never reuses a key (SPEC.md §4.3). */
void ava1_lane_key(const uint8_t dir[32], uint16_t lane, const uint8_t client_nonce[16],
                   const uint8_t server_nonce[16], uint8_t out[32]);
/* The control connection: ava1_lane_key(dir, 0, zeros, zeros). */
void ava1_control_key(const uint8_t dir[32], uint8_t out[32]);
/* Join proof (c2s): "join" ‖ sid ‖ u16le(lane) ‖ client_nonce (SPEC.md §4.5). */
void ava1_join_tag(const uint8_t c2s[32], const uint8_t sid[16], uint16_t lane,
                   const uint8_t client_nonce[16], uint8_t out[16]);
/* JoinAck proof (s2c): "join-ack" ‖ sid ‖ u16le(lane) ‖ client_nonce ‖ server_nonce. */
void ava1_join_ack_tag(const uint8_t s2c[32], const uint8_t sid[16], uint16_t lane,
                       const uint8_t client_nonce[16], const uint8_t server_nonce[16], uint8_t out[16]);
uint32_t ava1_pairing_code(const uint8_t hash[64]);

#endif
