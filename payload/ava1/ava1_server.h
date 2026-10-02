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
    /* Connections one source address may hold; 0 = 12. */
    uint32_t max_conns_per_ip;
    /* Sessions welcomed during a pairing window but not yet confirmed; 0 = 2. */
    uint32_t max_unpaired;
    /* Such a session ends when the window closes or after this long; 0 = 60 000 ms. */
    uint32_t pair_confirm_ms;
    /* At most one on_pair_request per this long; 0 = 10 000 ms. */
    uint32_t notify_every_ms;
    /* The trust slot carried a launch token (SPEC.md §5.2): a known client whose key is
     * launch_key gets ava1_launch_proof(launch_token, h) in its Welcome. No other does. */
    int has_launch;
    uint8_t launch_key[32];
    uint8_t launch_token[16];
    /* An unknown device started pairing: show its name and the code. */
    void (*on_pair_request)(const char *peer_name, uint32_t code);
    /* What an operator should know (an unreadable peers file, a pairing not stored). */
    void (*log)(const char *msg);
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
