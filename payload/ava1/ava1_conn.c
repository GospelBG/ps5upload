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

/* ms until `t`, at least 1 and at most INT_MAX-ish; `t` must be in the future. */
static int ms_until(uint64_t t, uint64_t now) {
    uint64_t d = t - now;
    return d > 0x3fffffffu ? 0x3fffffff : (d == 0 ? 1 : (int)d);
}

/* A frame's whole-body deadline under the rate floor: grace + len / min_rate. */
static uint64_t rate_deadline(uint64_t now, uint32_t grace_ms, uint32_t min_rate, size_t len) {
    return now + grace_ms + (uint64_t)len * 1000u / min_rate;
}

static int write_all(ava1_conn_t *c, const uint8_t *p, size_t n, size_t *sent) {
    int fd = c->fd;
    uint64_t deadline = 0;
    if (c->send_idle_ms && c->min_rate) deadline = rate_deadline(ava1_now_ms(), c->send_idle_ms, c->min_rate, n);
    while (n > 0) {
        ssize_t k;
        int flags = SEND_FLAGS;
        if (c->send_idle_ms) {
            /* Never block in send(): wait for room with a bound, then send what fits. */
            struct pollfd pf;
            uint64_t now = ava1_now_ms();
            int wait = (int)c->send_idle_ms, pr;
            if (deadline) {
                if (now >= deadline) return AVA1_E_TIMEOUT;
                if (ms_until(deadline, now) < wait) wait = ms_until(deadline, now);
            }
            pf.fd = fd;
            pf.events = POLLOUT;
            pf.revents = 0;
            pr = poll(&pf, 1, wait);
            if (pr < 0) {
                if (errno == EINTR) continue;
                return AVA1_E_IO;
            }
            if (pr == 0) return AVA1_E_TIMEOUT; /* the peer stopped reading */
            flags |= MSG_DONTWAIT;
        }
        k = send(fd, p, n, flags);
        if (k < 0) {
            if (errno == EINTR) continue;
            if ((errno == EAGAIN || errno == EWOULDBLOCK) && c->send_idle_ms) continue;
            return (errno == EAGAIN || errno == EWOULDBLOCK) ? AVA1_E_TIMEOUT : AVA1_E_IO;
        }
        p += k;
        n -= (size_t)k;
        *sent += (size_t)k;
    }
    return 0;
}

/* Reads exactly n bytes. Bounded by deadline_ms (absolute, the handshake or a frame's
 * rate floor) and idle_ms (since the last byte); calls tick while it waits. */
static int read_all(ava1_conn_t *c, uint8_t *p, size_t n) {
    int fd = c->fd;
    while (n > 0) {
        ssize_t k;
        if (c->deadline_ms || c->idle_ms || c->tick) {
            struct pollfd pf;
            uint64_t now = ava1_now_ms();
            int wait = -1, pr;
            if (c->deadline_ms) {
                if (now >= c->deadline_ms) return AVA1_E_TIMEOUT;
                wait = ms_until(c->deadline_ms, now);
            }
            if (c->idle_ms) {
                uint64_t dead = c->last_rx_ms + c->idle_ms;
                if (now >= dead) return AVA1_E_TIMEOUT;
                if (wait < 0 || ms_until(dead, now) < wait) wait = ms_until(dead, now);
            }
            if (c->tick && (wait < 0 || (int)c->tick_ms < wait)) wait = c->tick_ms ? (int)c->tick_ms : 1;
            pf.fd = fd;
            pf.events = POLLIN;
            pf.revents = 0;
            pr = poll(&pf, 1, wait);
            if (pr < 0) {
                if (errno == EINTR) continue;
                return AVA1_E_IO;
            }
            if (c->tick && c->tick(c->tick_arg) != 0) return AVA1_E_CLOSED;
            if (pr == 0) continue; /* the limits are checked at the top */
        }
        k = recv(fd, p, n, 0);
        if (k == 0) return AVA1_E_CLOSED;
        if (k < 0) {
            if (errno == EINTR) continue;
            return (errno == EAGAIN || errno == EWOULDBLOCK) ? AVA1_E_TIMEOUT : AVA1_E_IO;
        }
        c->last_rx_ms = ava1_now_ms();
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
    rc = write_all(c, frame, total, &sent);
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
    {
        /* Rate floor: a peer may be slow but must not drip one frame forever. */
        uint64_t saved = c->deadline_ms;
        if (c->min_rate && c->idle_ms) {
            uint64_t d = rate_deadline(ava1_now_ms(), c->idle_ms, c->min_rate, h.body_len);
            if (!c->deadline_ms || d < c->deadline_ms) c->deadline_ms = d;
        }
        rc = read_all(c, buf, body);
        if (rc == 0 && c->keyed) rc = read_all(c, mac, sizeof mac);
        c->deadline_ms = saved;
        if (rc != 0) return rc;
    }
    if (c->keyed) {
        if (ava1_open(c->recv_key, c->recv_ctr, hb, 12, buf, body, mac) != 0) return AVA1_E_TAG;
        c->recv_ctr++;
    }
    *type = h.type;
    *flags = h.flags;
    *channel = h.channel;
    *len = body;
    return 0;
}
