/* Frames on a socket, sealed after the handshake (SPEC.md §2, §4.4). One reader
 * thread per connection; writers (that thread and RPC workers) share `wmu`. */
#ifndef AVA1_CONN_H
#define AVA1_CONN_H

#include <pthread.h>
#include <stddef.h>
#include <stdint.h>

typedef struct {
    int fd;
    int keyed;
    uint8_t send_key[32];
    uint8_t recv_key[32];
    uint64_t send_ctr;
    uint64_t recv_ctr;
    pthread_mutex_t wmu;
} ava1_conn_t;

void ava1_conn_init(ava1_conn_t *c, int fd);
/* Wipes keys and destroys the lock; does not close fd. */
void ava1_conn_destroy(ava1_conn_t *c);
int ava1_conn_send(ava1_conn_t *c, uint8_t type, uint32_t channel, const uint8_t *body, size_t len);
/* Like send, but returns AVA1_E_BUSY instead of waiting when another writer holds the
 * connection: for liveness Pings and Pongs, which data in flight makes unnecessary. */
int ava1_conn_try_send(ava1_conn_t *c, uint8_t type, uint32_t channel, const uint8_t *body, size_t len);
/* Reads one frame; the opened body goes to buf. Reader thread only. AVA1_E_* on failure. */
int ava1_conn_recv(ava1_conn_t *c, uint8_t *type, uint8_t *flags, uint32_t *channel, uint8_t *buf,
                   size_t cap, size_t *len);

#endif
