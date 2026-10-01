/* Noise_XX_25519_ChaChaPoly_BLAKE2b (noiseprotocol.org, revision 34): the handshake,
 * plus the AEAD AVA1 frames use after it (SPEC.md §4). Checked against the cacophony
 * vector in protocol/ava1/vectors/noise_xx.json. */
#ifndef AVA1_NOISE_H
#define AVA1_NOISE_H

#include <stddef.h>
#include <stdint.h>

#include "ava1_keys.h"

typedef struct {
    uint8_t ck[64];
    uint8_t h[64];
    uint8_t k[32];
    int has_k;
    uint64_t n;
    ava1_identity_t s;
    ava1_identity_t e;
    uint8_t rs[32];
    uint8_t re[32];
    int initiator;
    int step; /* handshake messages processed: 0..3 */
} ava1_noise_t;

/* e = this side's ephemeral key pair (random in use, fixed only in tests). */
void ava1_noise_init(ava1_noise_t *ns, int initiator, const ava1_identity_t *s,
                     const ava1_identity_t *e, const uint8_t *prologue, size_t prologue_len);
/* The next handshake message carrying `payload`. 0 or AVA1_E_*. */
int ava1_noise_write(ava1_noise_t *ns, const uint8_t *payload, size_t payload_len, uint8_t *out,
                     size_t cap, size_t *out_len);
/* Reads the peer's next handshake message; its payload goes to `payload`. */
int ava1_noise_read(ava1_noise_t *ns, const uint8_t *msg, size_t len, uint8_t *payload, size_t cap,
                    size_t *payload_len);
/* After message 3. The initiator is always the client: k_i2r = c2s, k_r2i = s2c. */
void ava1_noise_split(const ava1_noise_t *ns, uint8_t k_i2r[32], uint8_t k_r2i[32]);

/* ChaCha20-Poly1305 (RFC 8439), nonce = 4 zero bytes ‖ u64le(n), in place. */
void ava1_seal(const uint8_t key[32], uint64_t n, const uint8_t *ad, size_t ad_len, uint8_t *buf,
               size_t len, uint8_t mac[16]);
/* 0, or AVA1_E_TAG (buf is then garbage). */
int ava1_open(const uint8_t key[32], uint64_t n, const uint8_t *ad, size_t ad_len, uint8_t *buf,
              size_t len, const uint8_t mac[16]);

#endif
