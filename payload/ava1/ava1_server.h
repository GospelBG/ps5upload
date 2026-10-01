/* The AVA1 server: one accept thread, one reader thread per connection, RPC worker
 * threads (SPEC.md §5–§9). */
#ifndef AVA1_SERVER_H
#define AVA1_SERVER_H

#include <stddef.h>
#include <stdint.h>

#include "ava1_keys.h"

typedef struct ava1_server_cfg {
    uint16_t port;              /* 0 = any free port (tests) */
    int bind_loopback;          /* 1 = 127.0.0.1 only (tests) */
    ava1_identity_t identity;
    char name[64];
    char peers_path[256];
    uint32_t ping_every_ms;
    uint32_t dead_after_ms;
    uint32_t handshake_ms;
    /* Bytes/s one frame must at least move at, after a dead_after grace; 0 = 8192. */
    uint32_t min_frame_rate;
    /* Pairing opens by itself for this long after start, but only while the peers
     * file is empty (SPEC.md §5 item 6). 0 = never by itself. */
    uint32_t pairing_window_s;
    /* An unknown device started pairing: show its name and the code. */
    void (*on_pair_request)(const char *peer_name, uint32_t code);
    /* Runs on a worker thread. Returns an AVA1 status; writes the body to out. */
    int (*rpc)(uint16_t method, const uint8_t *body, uint32_t body_len, uint8_t *out, size_t cap,
               size_t *out_len);
} ava1_server_cfg_t;

/* 0, or a negative errno. Waits up to 5 s for an earlier server's connections to end. */
int ava1_server_start(const ava1_server_cfg_t *cfg);
uint16_t ava1_server_port(void);
void ava1_server_open_pairing(uint32_t seconds);
int ava1_server_pairing_open(void);
int ava1_server_conns(void);
/* Stops accepting; open connections notice within one ping interval. */
void ava1_server_stop(void);

#endif
