#include "ava1_conn.h"

#include <errno.h>
#include <poll.h>
#include <time.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/types.h>

#include "ava1_frame.h"
#include "ava1_noise.h"
#include "ava1_wire.h"
#include "monocypher.h"

#ifdef MSG_NOSIGNAL
#define SEND_FLAGS MSG_NOSIGNAL
#else
#define SEND_FLAGS 0
#endif

uint64_t ava1_now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000u + (uint64_t)ts.tv_nsec / 1000000u;
}

static int write_all(int fd, const uint8_t *p, size_t n, size_t *sent) {
    while (n > 0) {
        ssize_t k = send(fd, p, n, SEND_FLAGS);
        if (k < 0) {
            if (errno == EINTR) continue;
            return (errno == EAGAIN || errno == EWOULDBLOCK) ? AVA1_E_TIMEOUT : AVA1_E_IO;
        }
        p += k;
        n -= (size_t)k;
        *sent += (size_t)k;
    }
    return 0;
}

static int read_all(ava1_conn_t *c, uint8_t *p, size_t n) {
    int fd = c->fd;
    while (n > 0) {
        ssize_t k;
        if (c->deadline_ms) {
            struct pollfd pf;
            uint64_t now = ava1_now_ms();
            int pr;
            if (now >= c->deadline_ms) return AVA1_E_TIMEOUT;
            pf.fd = fd;
            pf.events = POLLIN;
            pf.revents = 0;
            pr = poll(&pf, 1, (int)(c->deadline_ms - now));
            if (pr < 0) {
                if (errno == EINTR) continue;
                return AVA1_E_IO;
            }
            if (pr == 0) return AVA1_E_TIMEOUT;
        }
        k = recv(fd, p, n, 0);
        if (k == 0) return AVA1_E_CLOSED;
        if (k < 0) {
            if (errno == EINTR) continue;
            return (errno == EAGAIN || errno == EWOULDBLOCK) ? AVA1_E_TIMEOUT : AVA1_E_IO;
        }
        p += k;
        n -= (size_t)k;
    }
    return 0;
}

void ava1_conn_init(ava1_conn_t *c, int fd) {
    memset(c, 0, sizeof *c);
    c->fd = fd;
    pthread_mutex_init(&c->wmu, NULL);
}

void ava1_conn_destroy(ava1_conn_t *c) {
    pthread_mutex_destroy(&c->wmu);
    crypto_wipe(c->send_key, sizeof c->send_key);
    crypto_wipe(c->recv_key, sizeof c->recv_key);
}

/* Caller holds wmu. */
static int send_locked(ava1_conn_t *c, uint8_t type, uint32_t channel, const uint8_t *body, size_t len) {
    ava1_header_t h;
    size_t mac = c->keyed ? AVA1_TAG_LEN : 0, total = AVA1_HEADER_LEN + len + mac;
    uint8_t *frame;
    size_t sent = 0;
    int rc;
    if (c->broken) return AVA1_E_IO;
    if (len + mac > AVA1_MAX_BODY) return AVA1_E_TOOLONG;
    frame = malloc(total);
    if (!frame) return AVA1_E_IO;
    h.type = type;
    h.flags = c->keyed ? AVA1_FLAG_SEALED : 0;
    h.channel = channel;
    h.body_len = (uint32_t)(len + mac);
    ava1_header_encode(&h, frame);
    if (len) memcpy(frame + AVA1_HEADER_LEN, body, len);
    if (c->keyed) {
        ava1_seal(c->send_key, c->send_ctr++, frame, 12, frame + AVA1_HEADER_LEN, len,
                  frame + AVA1_HEADER_LEN + len);
    }
    rc = write_all(c->fd, frame, total, &sent);
    if (rc != 0 && (sent > 0 || c->keyed)) {
        /* Half a frame, or a counter spent on a frame the peer never got. */
        c->broken = 1;
        shutdown(c->fd, SHUT_RDWR);
    }
    free(frame);
    return rc;
}

int ava1_conn_send(ava1_conn_t *c, uint8_t type, uint32_t channel, const uint8_t *body, size_t len) {
    int rc;
    pthread_mutex_lock(&c->wmu);
    rc = send_locked(c, type, channel, body, len);
    pthread_mutex_unlock(&c->wmu);
    return rc;
}

int ava1_conn_try_send(ava1_conn_t *c, uint8_t type, uint32_t channel, const uint8_t *body, size_t len) {
    int rc;
    if (pthread_mutex_trylock(&c->wmu) != 0) return AVA1_E_BUSY;
    rc = send_locked(c, type, channel, body, len);
    pthread_mutex_unlock(&c->wmu);
    return rc;
}

int ava1_conn_recv(ava1_conn_t *c, uint8_t *type, uint8_t *flags, uint32_t *channel, uint8_t *buf,
                   size_t cap, size_t *len) {
    uint8_t hb[AVA1_HEADER_LEN], mac[AVA1_TAG_LEN];
    ava1_header_t h;
    size_t body;
    int rc = read_all(c, hb, sizeof hb);
    if (rc != 0) return rc;
    rc = ava1_header_decode(hb, &h);
    if (rc != 0) return rc;
    if (c->keyed) {
        if (!(h.flags & AVA1_FLAG_SEALED) || h.body_len < AVA1_TAG_LEN) return AVA1_E_TAG;
        body = h.body_len - AVA1_TAG_LEN;
    } else {
        if (h.flags & AVA1_FLAG_SEALED) return AVA1_E_PROTO;
        body = h.body_len;
    }
    if (body > cap) return AVA1_E_TOOLONG;
    rc = read_all(c, buf, body);
    if (rc != 0) return rc;
    if (c->keyed) {
        rc = read_all(c, mac, sizeof mac);
        if (rc != 0) return rc;
        if (ava1_open(c->recv_key, c->recv_ctr, hb, 12, buf, body, mac) != 0) return AVA1_E_TAG;
        c->recv_ctr++;
    }
    *type = h.type;
    *flags = h.flags;
    *channel = h.channel;
    *len = body;
    return 0;
}
