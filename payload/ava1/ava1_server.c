#include "ava1_server.h"

#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <poll.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

#include "ava1_conn.h"
#include "ava1_frame.h"
#include "ava1_gen.h"
#include "ava1_noise.h"
#include "ava1_platform.h"
#include "ava1_store.h"
#include "monocypher.h"

#define MAX_SESSIONS 16
#define MAX_CONNS 64
#define CTRL_MAX 65536u
#define NONCES 64
#define RPC_WORKERS 4
#define RPC_OUT_MAX 16384u
#define MAX_PAIRING_WINDOW_S 600u
#define THREAD_STACK (256u * 1024u)

static const uint8_t PROLOGUE[] = { 'A', 'V', 'A', '1', ' ', 'v', '1' };

/* A connection shared by its reader thread and any RPC workers: freed by the last. */
typedef struct {
    ava1_conn_t io;
    int refs; /* under mu */
} conn_t;

typedef struct {
    int used;
    uint8_t sid[16];
    uint8_t c2s[32];
    uint8_t s2c[32];
    int paired;
    uint8_t peer_key[32];
    char peer_name[64];
    int rpc_inflight;
    uint32_t lane_gen[AVA1_MAX_LANES + 1];
    uint8_t nonces[NONCES][16];
    unsigned nonce_next;
    unsigned nonce_count;
} sess_t;

static struct {
    ava1_server_cfg_t cfg;
    int listen_fd;
    uint16_t port;
    pthread_t accept_thread;
    int accept_started;
    volatile int stopping;
    sess_t sessions[MAX_SESSIONS];
    int conns;
    uint64_t pairing_until_ms;
    ava1_peers_t peers;
} S = { .listen_fd = -1 };

static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;

/* Monotonic only: a settimeofday jump on the console must not age or revive anything. */
static uint64_t now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000u + (uint64_t)ts.tv_nsec / 1000000u;
}

static void set_timeouts(int fd, uint32_t ms) {
    struct timeval tv;
    tv.tv_sec = (time_t)(ms / 1000u);
    tv.tv_usec = (suseconds_t)((ms % 1000u) * 1000u);
    (void)setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    (void)setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof tv);
}

static int spawn_detached(void *(*fn)(void *), void *arg) {
    pthread_attr_t attr;
    pthread_t t;
    int rc;
    if (pthread_attr_init(&attr) != 0) return -1;
    (void)pthread_attr_setstacksize(&attr, THREAD_STACK);
    (void)pthread_attr_setdetachstate(&attr, PTHREAD_CREATE_DETACHED);
    rc = pthread_create(&t, &attr, fn, arg);
    pthread_attr_destroy(&attr);
    return rc == 0 ? 0 : -1;
}

static void conn_get(conn_t *k) {
    pthread_mutex_lock(&mu);
    k->refs++;
    pthread_mutex_unlock(&mu);
}

/* The last reference closes the socket and frees the connection. */
static void conn_put(conn_t *k) {
    int last;
    pthread_mutex_lock(&mu);
    last = --k->refs == 0;
    if (last) S.conns--;
    pthread_mutex_unlock(&mu);
    if (!last) return;
    close(k->io.fd);
    ava1_conn_destroy(&k->io);
    crypto_wipe(k, sizeof *k);
    free(k);
}

static int send_error(ava1_conn_t *c, uint16_t code, const char *msg) {
    ava1_error_t e;
    uint8_t b[320];
    ava1_w_t w;
    size_t n = strlen(msg);
    if (n > 256) n = 256;
    memset(&e, 0, sizeof e);
    e.code = code;
    e.message = (const uint8_t *)msg;
    e.message_len = (uint16_t)n;
    ava1_w_init(&w, b, sizeof b);
    if (ava1_error_encode(&e, &w) != 0) return -1;
    return ava1_conn_send(c, AVA1_TYPE_ERROR, 0, b, w.len);
}

/* Ping and Pong share their fields, so one encoder serves both. Liveness frames never
 * wait for the write lock: whoever holds it is sending data, which is proof of life. */
static int send_liveness(ava1_conn_t *c, uint8_t type, uint32_t seq, uint64_t t_us) {
    ava1_ping_t p;
    uint8_t b[32];
    ava1_w_t w;
    memset(&p, 0, sizeof p);
    p.seq = seq;
    p.t_us = t_us;
    ava1_w_init(&w, b, sizeof b);
    if (ava1_ping_encode(&p, &w) != 0) return -1;
    return ava1_conn_try_send(c, type, 0, b, w.len);
}

/* Printable ASCII only: names end up in the peers file and in notifications. */
static void clean_name(const uint8_t *p, uint16_t n, char out[64]) {
    size_t i, k = 0;
    for (i = 0; i < n && k < 63; i++) {
        uint8_t ch = p[i];
        out[k++] = (ch < 0x20 || ch >= 0x7f) ? '?' : (char)ch;
    }
    out[k] = 0;
}

static int sess_add(const uint8_t sid[16], const uint8_t c2s[32], const uint8_t s2c[32], int paired,
                    const uint8_t peer[32], const char *name) {
    int i, idx = -1;
    pthread_mutex_lock(&mu);
    for (i = 0; i < MAX_SESSIONS; i++) {
        if (!S.sessions[i].used) {
            sess_t *s = &S.sessions[i];
            memset(s, 0, sizeof *s);
            s->used = 1;
            memcpy(s->sid, sid, 16);
            memcpy(s->c2s, c2s, 32);
            memcpy(s->s2c, s2c, 32);
            s->paired = paired;
            memcpy(s->peer_key, peer, 32);
            snprintf(s->peer_name, sizeof s->peer_name, "%s", name);
            idx = i;
            break;
        }
    }
    pthread_mutex_unlock(&mu);
    return idx;
}

static void sess_remove(int idx, const uint8_t sid[16]) {
    pthread_mutex_lock(&mu);
    if (S.sessions[idx].used && memcmp(S.sessions[idx].sid, sid, 16) == 0)
        crypto_wipe(&S.sessions[idx], sizeof S.sessions[idx]);
    pthread_mutex_unlock(&mu);
}

/* Caller holds mu. */
static int sess_find_locked(const uint8_t sid[16]) {
    int i;
    for (i = 0; i < MAX_SESSIONS; i++)
        if (S.sessions[i].used && memcmp(S.sessions[i].sid, sid, 16) == 0) return i;
    return -1;
}

/* Caller holds mu. */
static int sess_is_locked(int idx, const uint8_t sid[16]) {
    return S.sessions[idx].used && memcmp(S.sessions[idx].sid, sid, 16) == 0;
}

static int lane_current(int idx, const uint8_t sid[16], uint16_t lane, uint32_t gen) {
    int ok;
    pthread_mutex_lock(&mu);
    ok = sess_is_locked(idx, sid) && S.sessions[idx].lane_gen[lane] == gen;
    pthread_mutex_unlock(&mu);
    return ok;
}

/* Caller holds mu. */
static int nonce_seen(const sess_t *s, const uint8_t n[16]) {
    unsigned i;
    for (i = 0; i < s->nonce_count; i++)
        if (memcmp(s->nonces[i], n, 16) == 0) return 1;
    return 0;
}

/* Caller holds mu. */
static void nonce_add(sess_t *s, const uint8_t n[16]) {
    memcpy(s->nonces[s->nonce_next], n, 16);
    s->nonce_next = (s->nonce_next + 1) % NONCES;
    if (s->nonce_count < NONCES) s->nonce_count++;
}

static int send_status(ava1_conn_t *c, uint32_t ch, uint16_t status, const uint8_t *body, size_t len) {
    ava1_rpc_response_t resp;
    ava1_w_t w;
    uint8_t *frame = malloc(len + 64);
    int rc;
    if (!frame) return AVA1_E_IO;
    memset(&resp, 0, sizeof resp);
    resp.status = status;
    resp.body = body;
    resp.body_len = (uint32_t)len;
    ava1_w_init(&w, frame, len + 64);
    rc = ava1_rpc_response_encode(&resp, &w);
    if (rc == 0) rc = ava1_conn_send(c, AVA1_TYPE_RPC_RESPONSE, ch, frame, w.len);
    free(frame);
    return rc;
}

static int pair_confirm(ava1_conn_t *c, int idx, uint32_t ch) {
    ava1_pair_result_t r;
    uint8_t b[16];
    ava1_w_t w;
    int accepted = 0;
    pthread_mutex_lock(&mu);
    {
        sess_t *s = &S.sessions[idx];
        if (s->paired) {
            accepted = 1;
        } else if (now_ms() < S.pairing_until_ms &&
                   ava1_peers_add(&S.peers, s->peer_key, s->peer_name, (uint64_t)time(NULL),
                                  S.cfg.peers_path) == 0) {
            s->paired = 1;
            accepted = 1;
        }
    }
    pthread_mutex_unlock(&mu);
    memset(&r, 0, sizeof r);
    r.accepted = (uint8_t)accepted;
    ava1_w_init(&w, b, sizeof b);
    if (ava1_pair_result_encode(&r, &w) != 0 || ava1_conn_send(c, AVA1_TYPE_PAIR_RESULT, ch, b, w.len) != 0)
        return 1;
    return accepted ? 0 : 1;
}

typedef struct {
    conn_t *k;
    int idx;
    uint8_t sid[16];
    uint32_t ch;
    uint16_t method;
    uint8_t *body;
    uint32_t body_len;
} rpc_job_t;

static void *rpc_worker(void *arg) {
    rpc_job_t *j = arg;
    uint8_t *out = malloc(RPC_OUT_MAX);
    size_t out_len = 0;
    int status = AVA1_ERR_INTERNAL;
    if (out && S.cfg.rpc) status = S.cfg.rpc(j->method, j->body, j->body_len, out, RPC_OUT_MAX, &out_len);
    else if (out) status = AVA1_ERR_UNKNOWN_METHOD;
    (void)send_status(&j->k->io, j->ch, (uint16_t)status, out, status == AVA1_STATUS_OK ? out_len : 0);
    pthread_mutex_lock(&mu);
    if (sess_is_locked(j->idx, j->sid)) S.sessions[j->idx].rpc_inflight--;
    pthread_mutex_unlock(&mu);
    free(out);
    free(j->body);
    conn_put(j->k);
    free(j);
    return NULL;
}

/* Answers on the reader only what is instant (refusals, pairing.open); everything
 * else runs on a worker so liveness never waits for a call. */
static int do_rpc(conn_t *k, int idx, const uint8_t sid[16], uint32_t ch, const uint8_t *body, size_t len) {
    ava1_rpc_request_t q;
    rpc_job_t *j;
    int paired, slot;
    if (ava1_rpc_request_decode(body, len, &q) != 0) {
        (void)send_error(&k->io, AVA1_ERR_PROTOCOL, "bad RpcRequest");
        return 1;
    }
    pthread_mutex_lock(&mu);
    paired = S.sessions[idx].paired;
    slot = paired && S.sessions[idx].rpc_inflight < RPC_WORKERS;
    if (slot) S.sessions[idx].rpc_inflight++;
    pthread_mutex_unlock(&mu);
    if (!paired) return send_status(&k->io, ch, AVA1_ERR_NOT_PAIRED, NULL, 0) != 0;
    if (q.method == AVA1_METHOD_PAIRING_OPEN) {
        ava1_pairing_open_t o;
        uint16_t st = AVA1_ERR_PROTOCOL;
        pthread_mutex_lock(&mu);
        S.sessions[idx].rpc_inflight--;
        pthread_mutex_unlock(&mu);
        if (ava1_pairing_open_decode(q.body, q.body_len, &o) == 0) {
            ava1_server_open_pairing(o.seconds > MAX_PAIRING_WINDOW_S ? MAX_PAIRING_WINDOW_S : o.seconds);
            st = AVA1_STATUS_OK;
        }
        return send_status(&k->io, ch, st, NULL, 0) != 0;
    }
    if (!slot) return send_status(&k->io, ch, AVA1_ERR_BUSY, NULL, 0) != 0;
    j = calloc(1, sizeof *j);
    if (j) j->body = malloc(q.body_len ? q.body_len : 1);
    if (!j || !j->body) {
        if (j) free(j);
        pthread_mutex_lock(&mu);
        S.sessions[idx].rpc_inflight--;
        pthread_mutex_unlock(&mu);
        return send_status(&k->io, ch, AVA1_ERR_INTERNAL, NULL, 0) != 0;
    }
    if (q.body_len) memcpy(j->body, q.body, q.body_len);
    j->body_len = q.body_len;
    j->k = k;
    j->idx = idx;
    memcpy(j->sid, sid, 16);
    j->ch = ch;
    j->method = q.method;
    conn_get(k);
    if (spawn_detached(rpc_worker, j) != 0) {
        conn_put(k);
        free(j->body);
        free(j);
        pthread_mutex_lock(&mu);
        S.sessions[idx].rpc_inflight--;
        pthread_mutex_unlock(&mu);
        return send_status(&k->io, ch, AVA1_ERR_BUSY, NULL, 0) != 0;
    }
    return 0;
}

/* 0 = keep going; nonzero = close the connection. */
static int handle_frame(conn_t *k, int idx, const uint8_t sid[16], uint16_t lane, uint8_t type,
                        uint8_t flags, uint32_t ch, const uint8_t *body, size_t len) {
    switch (type) {
    case AVA1_TYPE_PING: {
        ava1_ping_t p;
        if (ava1_ping_decode(body, len, &p) != 0) return 1;
        return send_liveness(&k->io, AVA1_TYPE_PONG, p.seq, p.t_us) == AVA1_E_IO;
    }
    case AVA1_TYPE_PONG:
        return 0;
    case AVA1_TYPE_PAIR_CONFIRM:
        if (lane != 0) break;
        return pair_confirm(&k->io, idx, ch);
    case AVA1_TYPE_RPC_REQUEST:
        if (lane != 0) break;
        return do_rpc(k, idx, sid, ch, body, len);
    case AVA1_TYPE_BYE:
    case AVA1_TYPE_ERROR:
        return 1;
    default:
        if (flags & AVA1_FLAG_IGNORABLE) return 0;
        break;
    }
    (void)send_error(&k->io, AVA1_ERR_PROTOCOL, "unexpected frame");
    return 1;
}

/* Liveness: dead only after dead_after with no frame received. A Ping goes out when
 * the writer is free; a slow or blocked write is never a reason to close. */
static void serve_loop(conn_t *k, int idx, const uint8_t sid[16], uint16_t lane, uint32_t gen, uint8_t *buf) {
    uint64_t last_rx = now_ms(), last_ping = 0;
    uint32_t ping_seq = 0;
    set_timeouts(k->io.fd, S.cfg.dead_after_ms);
    while (!S.stopping) {
        struct pollfd p;
        uint64_t t = now_ms(), since;
        int wait, pr;
        if (t - last_rx > S.cfg.dead_after_ms) return;
        if (lane != 0 && !lane_current(idx, sid, lane, gen)) return;
        if (t - last_ping >= S.cfg.ping_every_ms) {
            if (send_liveness(&k->io, AVA1_TYPE_PING, ++ping_seq, t * 1000u) == AVA1_E_IO) return;
            last_ping = t;
        }
        since = now_ms() - last_ping;
        wait = since >= S.cfg.ping_every_ms ? 1 : (int)(S.cfg.ping_every_ms - since);
        p.fd = k->io.fd;
        p.events = POLLIN;
        p.revents = 0;
        pr = poll(&p, 1, wait);
        if (pr < 0) {
            if (errno == EINTR) continue;
            return;
        }
        if (pr == 0) continue;
        {
            uint8_t type, flags;
            uint32_t ch;
            size_t len;
            if (ava1_conn_recv(&k->io, &type, &flags, &ch, buf, CTRL_MAX, &len) != 0) return;
            last_rx = now_ms();
            if (handle_frame(k, idx, sid, lane, type, flags, ch, buf, len) != 0) return;
        }
    }
}

static int send_noise(ava1_conn_t *c, uint8_t type, const uint8_t *msg, size_t n) {
    ava1_hs2_t m; /* Hs1/Hs2/Hs3 share one shape */
    uint8_t out[700];
    ava1_w_t w;
    memset(&m, 0, sizeof m);
    m.noise = msg;
    m.noise_len = (uint32_t)n;
    ava1_w_init(&w, out, sizeof out);
    if (ava1_hs2_encode(&m, &w) != 0) return -1;
    return ava1_conn_send(c, type, 0, out, w.len);
}

/* Hs1 already in buf. Noise XX as responder, then Welcome or a sealed refusal. */
static void run_control(conn_t *k, uint8_t *buf, size_t len) {
    ava1_hs1_t m1;
    ava1_hs3_t m3;
    ava1_hello_info_t hello;
    ava1_server_info_t si;
    ava1_client_info_t ci;
    ava1_welcome_t wel;
    ava1_noise_t ns;
    ava1_identity_t eph;
    uint8_t secret[32], sid[16], pl[512], msg[600], c2s[32], s2c[32];
    char peer_name[64];
    size_t pn, mn;
    uint8_t type, flags;
    uint32_t ch;
    ava1_w_t w;
    int known, open, idx;

    memset(&ns, 0, sizeof ns);
    memset(&eph, 0, sizeof eph);
    if (ava1_hs1_decode(buf, len, &m1) != 0) {
        (void)send_error(&k->io, AVA1_ERR_PROTOCOL, "bad Hs1");
        return;
    }
    if (ava1_platform_random(secret, 32) != 0 || ava1_platform_random(sid, 16) != 0) {
        (void)send_error(&k->io, AVA1_ERR_INTERNAL, "no random source");
        return;
    }
    ava1_identity_from_secret(&eph, secret);
    crypto_wipe(secret, sizeof secret);
    ava1_noise_init(&ns, 0, &S.cfg.identity, &eph, PROLOGUE, sizeof PROLOGUE);
    if (ava1_noise_read(&ns, m1.noise, m1.noise_len, pl, sizeof pl, &pn) != 0 ||
        ava1_hello_info_decode(pl, pn, &hello) != 0) {
        (void)send_error(&k->io, AVA1_ERR_PROTOCOL, "bad handshake");
        goto out;
    }
    if (hello.version_min > AVA1_PROTOCOL_VERSION || hello.version_max < AVA1_PROTOCOL_VERSION) {
        (void)send_error(&k->io, AVA1_ERR_UNSUPPORTED_VERSION, "no protocol version in common");
        goto out;
    }
    memset(&si, 0, sizeof si);
    si.version = AVA1_PROTOCOL_VERSION;
    memcpy(si.session_id, sid, 16);
    si.has_name = 1;
    si.name = (const uint8_t *)S.cfg.name;
    si.name_len = (uint16_t)strlen(S.cfg.name);
    ava1_w_init(&w, pl, sizeof pl);
    if (ava1_server_info_encode(&si, &w) != 0 ||
        ava1_noise_write(&ns, pl, w.len, msg, sizeof msg, &mn) != 0 ||
        send_noise(&k->io, AVA1_TYPE_HS2, msg, mn) != 0)
        goto out;
    if (ava1_conn_recv(&k->io, &type, &flags, &ch, buf, CTRL_MAX, &len) != 0 || type != AVA1_TYPE_HS3 ||
        ava1_hs3_decode(buf, len, &m3) != 0 ||
        ava1_noise_read(&ns, m3.noise, m3.noise_len, pl, sizeof pl, &pn) != 0 ||
        ava1_client_info_decode(pl, pn, &ci) != 0)
        goto out;
    clean_name(ci.name, ci.has_name ? ci.name_len : 0, peer_name);
    ava1_noise_split(&ns, c2s, s2c);
    ava1_lane_key(c2s, 0, k->io.recv_key);
    ava1_lane_key(s2c, 0, k->io.send_key);
    k->io.keyed = 1;
    pthread_mutex_lock(&mu);
    known = ava1_peers_contains(&S.peers, ns.rs);
    open = now_ms() < S.pairing_until_ms;
    pthread_mutex_unlock(&mu);
    if (!known && !open) {
        (void)send_error(&k->io, AVA1_ERR_PAIRING_CLOSED, "this device is not paired and pairing is closed");
        goto out;
    }
    memset(&wel, 0, sizeof wel);
    wel.knows_you = known ? 1 : 0;
    ava1_w_init(&w, pl, sizeof pl);
    if (ava1_welcome_encode(&wel, &w) != 0 || ava1_conn_send(&k->io, AVA1_TYPE_WELCOME, 0, pl, w.len) != 0)
        goto out;
    idx = sess_add(sid, c2s, s2c, known, ns.rs, peer_name);
    if (idx < 0) {
        (void)send_error(&k->io, AVA1_ERR_BUSY, "too many sessions");
        goto out;
    }
    if (!known && S.cfg.on_pair_request) S.cfg.on_pair_request(peer_name, ava1_pairing_code(ns.h));
    serve_loop(k, idx, sid, 0, 0, buf);
    sess_remove(idx, sid);
out:
    crypto_wipe(&ns, sizeof ns);
    crypto_wipe(&eph, sizeof eph);
    crypto_wipe(c2s, sizeof c2s);
    crypto_wipe(s2c, sizeof s2c);
}

static void run_lane(conn_t *k, uint8_t *buf, size_t len) {
    ava1_join_t j;
    ava1_join_ack_t ack;
    uint8_t expect[16], c2s[32], s2c[32], out[64];
    ava1_w_t w;
    int idx, verified = 0, fresh = 0, paired = 0;
    uint32_t gen = 0;
    memset(c2s, 0, sizeof c2s);
    memset(s2c, 0, sizeof s2c);
    if (ava1_join_decode(buf, len, &j) != 0 || j.lane_id == 0 || j.lane_id > AVA1_MAX_LANES) {
        (void)send_error(&k->io, AVA1_ERR_BAD_JOIN, "bad Join");
        return;
    }
    pthread_mutex_lock(&mu);
    idx = sess_find_locked(j.session_id);
    if (idx >= 0) {
        sess_t *s = &S.sessions[idx];
        ava1_join_tag(s->c2s, "join", j.session_id, j.lane_id, j.nonce, expect);
        verified = crypto_verify16(expect, j.tag) == 0;
        fresh = verified && !nonce_seen(s, j.nonce);
        if (fresh) {
            nonce_add(s, j.nonce);
            paired = s->paired;
        }
        if (fresh && paired) {
            gen = ++s->lane_gen[j.lane_id];
            memcpy(c2s, s->c2s, 32);
            memcpy(s2c, s->s2c, 32);
        }
    }
    pthread_mutex_unlock(&mu);
    if (!(fresh && paired)) {
        if (fresh) (void)send_error(&k->io, AVA1_ERR_NOT_PAIRED, "pair first");
        else (void)send_error(&k->io, AVA1_ERR_BAD_JOIN, "join refused");
        return;
    }
    memset(&ack, 0, sizeof ack);
    ack.lane_id = j.lane_id;
    ava1_join_tag(s2c, "join-ack", j.session_id, j.lane_id, j.nonce, ack.tag);
    ava1_w_init(&w, out, sizeof out);
    if (ava1_join_ack_encode(&ack, &w) == 0 && ava1_conn_send(&k->io, AVA1_TYPE_JOIN_ACK, 0, out, w.len) == 0) {
        ava1_lane_key(c2s, j.lane_id, k->io.recv_key);
        ava1_lane_key(s2c, j.lane_id, k->io.send_key);
        k->io.keyed = 1;
        serve_loop(k, idx, j.session_id, j.lane_id, gen, buf);
    }
    crypto_wipe(c2s, sizeof c2s);
    crypto_wipe(s2c, sizeof s2c);
}

static void *conn_main(void *arg) {
    conn_t *k = arg;
    uint8_t *buf = malloc(CTRL_MAX);
    set_timeouts(k->io.fd, S.cfg.handshake_ms);
    if (buf) {
        uint8_t type, flags;
        uint32_t ch;
        size_t len;
        if (ava1_conn_recv(&k->io, &type, &flags, &ch, buf, CTRL_MAX, &len) == 0) {
            if (type == AVA1_TYPE_HS1) run_control(k, buf, len);
            else if (type == AVA1_TYPE_JOIN) run_lane(k, buf, len);
            else (void)send_error(&k->io, AVA1_ERR_PROTOCOL, "expected Hs1 or Join");
        }
        free(buf);
    }
    /* Wake any worker blocked writing to a peer that is gone, then drop our reference. */
    shutdown(k->io.fd, SHUT_RDWR);
    conn_put(k);
    return NULL;
}

static void refuse_busy(int fd) {
    ava1_conn_t c;
    ava1_conn_init(&c, fd);
    set_timeouts(fd, 1000);
    (void)send_error(&c, AVA1_ERR_BUSY, "too many connections");
    ava1_conn_destroy(&c);
    close(fd);
}

static void *accept_main(void *arg) {
    (void)arg;
    while (!S.stopping) {
        struct pollfd p;
        int fd, one = 1, admit, pr;
        conn_t *k;
        p.fd = S.listen_fd;
        p.events = POLLIN;
        p.revents = 0;
        pr = poll(&p, 1, 200);
        if (pr < 0) {
            ava1_platform_sleep_ms(50);
            continue;
        }
        if (pr == 0) continue;
        fd = accept(S.listen_fd, NULL, NULL);
        if (fd < 0) {
            /* Never leave this loop on an errno: Sony returns undocumented ones (163)
             * and once that killed the helper's accept loop. */
            ava1_platform_sleep_ms(50);
            continue;
        }
        (void)setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one);
#ifdef SO_NOSIGPIPE
        (void)setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, sizeof one);
#endif
        pthread_mutex_lock(&mu);
        admit = S.conns < MAX_CONNS;
        if (admit) S.conns++;
        pthread_mutex_unlock(&mu);
        if (!admit) {
            refuse_busy(fd);
            continue;
        }
        k = calloc(1, sizeof *k);
        if (!k) {
            close(fd);
            pthread_mutex_lock(&mu);
            S.conns--;
            pthread_mutex_unlock(&mu);
            continue;
        }
        ava1_conn_init(&k->io, fd);
        k->refs = 1;
        if (spawn_detached(conn_main, k) != 0) conn_put(k);
    }
    return NULL;
}

int ava1_server_start(const ava1_server_cfg_t *cfg) {
    struct sockaddr_in a;
    socklen_t alen = sizeof a;
    int one = 1, i, busy = 1;
    for (i = 0; i < 500 && busy; i++) {
        pthread_mutex_lock(&mu);
        busy = S.conns > 0 || S.accept_started;
        pthread_mutex_unlock(&mu);
        if (busy) ava1_platform_sleep_ms(10);
    }
    if (busy) return -EBUSY;
    pthread_mutex_lock(&mu);
    memset(S.sessions, 0, sizeof S.sessions);
    S.cfg = *cfg;
    S.stopping = 0;
    if (ava1_peers_load(&S.peers, cfg->peers_path) != 0) memset(&S.peers, 0, sizeof S.peers);
    /* Opens by itself only while nothing is paired (design review, flaw 2). */
    S.pairing_until_ms = (cfg->pairing_window_s && S.peers.n == 0)
                             ? now_ms() + (uint64_t)cfg->pairing_window_s * 1000u
                             : 0;
    pthread_mutex_unlock(&mu);
    S.listen_fd = socket(AF_INET, SOCK_STREAM, 0);
    if (S.listen_fd < 0) return -errno;
    (void)setsockopt(S.listen_fd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    memset(&a, 0, sizeof a);
    a.sin_family = AF_INET;
    a.sin_port = htons(cfg->port);
    a.sin_addr.s_addr = htonl(cfg->bind_loopback ? INADDR_LOOPBACK : INADDR_ANY);
    if (bind(S.listen_fd, (struct sockaddr *)&a, sizeof a) != 0 || listen(S.listen_fd, 64) != 0 ||
        getsockname(S.listen_fd, (struct sockaddr *)&a, &alen) != 0) {
        int e = errno;
        close(S.listen_fd);
        S.listen_fd = -1;
        return -e;
    }
    S.port = ntohs(a.sin_port);
    if (pthread_create(&S.accept_thread, NULL, accept_main, NULL) != 0) {
        close(S.listen_fd);
        S.listen_fd = -1;
        return -EAGAIN;
    }
    pthread_mutex_lock(&mu);
    S.accept_started = 1;
    pthread_mutex_unlock(&mu);
    return 0;
}

uint16_t ava1_server_port(void) { return S.port; }

void ava1_server_open_pairing(uint32_t seconds) {
    pthread_mutex_lock(&mu);
    S.pairing_until_ms = now_ms() + (uint64_t)seconds * 1000u;
    pthread_mutex_unlock(&mu);
}

int ava1_server_pairing_open(void) {
    int open;
    pthread_mutex_lock(&mu);
    open = now_ms() < S.pairing_until_ms;
    pthread_mutex_unlock(&mu);
    return open;
}

int ava1_server_conns(void) {
    int n;
    pthread_mutex_lock(&mu);
    n = S.conns;
    pthread_mutex_unlock(&mu);
    return n;
}

void ava1_server_stop(void) {
    int started;
    pthread_mutex_lock(&mu);
    started = S.accept_started;
    pthread_mutex_unlock(&mu);
    if (!started) return;
    S.stopping = 1;
    pthread_join(S.accept_thread, NULL);
    close(S.listen_fd);
    S.listen_fd = -1;
    pthread_mutex_lock(&mu);
    S.accept_started = 0;
    pthread_mutex_unlock(&mu);
}
