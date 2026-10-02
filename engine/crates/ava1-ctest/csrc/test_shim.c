/* Starts the payload's AVA1 server on the host with a node.info handler (tests only). */
#include <pthread.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#include "ava1_conn.h"
#include "ava1_platform.h"

#include "ava1_gen.h"
#include "ava1_server.h"

static uint32_t g_pair_requests, g_last_code, g_logs;

static void on_log(const char *msg) {
    (void)msg;
    __atomic_add_fetch(&g_logs, 1, __ATOMIC_SEQ_CST);
}

static void on_pair(const char *name, uint32_t code) {
    (void)name;
    __atomic_add_fetch(&g_pair_requests, 1, __ATOMIC_SEQ_CST);
    __atomic_store_n(&g_last_code, code, __ATOMIC_SEQ_CST);
}

static int rpc(uint16_t method, const uint8_t *body, uint32_t body_len, uint8_t *out, size_t cap,
               size_t *out_len) {
    ava1_node_info_t ni;
    ava1_w_t w;
    (void)body;
    (void)body_len;
    if (method != AVA1_METHOD_NODE_INFO) return AVA1_ERR_UNKNOWN_METHOD;
    memset(&ni, 0, sizeof ni);
    ni.version = (const uint8_t *)"test";
    ni.version_len = 4;
    ni.platform = (const uint8_t *)"host";
    ni.platform_len = 4;
    ni.name = (const uint8_t *)"C test server";
    ni.name_len = 13;
    ava1_w_init(&w, out, cap);
    if (ava1_node_info_encode(&ni, &w) != 0) return AVA1_ERR_INTERNAL;
    *out_len = w.len;
    return AVA1_STATUS_OK;
}

/* Mirrored by ava1_ctest::ffi::TestOpts. */
typedef struct {
    uint32_t pairing_s;
    uint32_t ping_ms;
    uint32_t dead_ms;
    uint32_t handshake_ms;
    uint32_t min_frame_rate;
    uint32_t max_conns_per_ip;
    uint32_t max_unpaired;
    uint32_t pair_confirm_ms;
    uint32_t notify_every_ms;
    uint32_t launch; /* 1: launch_key only; 2: launch_key and launch_token */
    uint8_t launch_key[32];
    uint8_t launch_token[16];
} ava1_test_opts_t;

size_t ava1_test_sizeof_opts(void) { return sizeof(ava1_test_opts_t); }

int ava1_test_server_start(const uint8_t secret[32], const char *peers_path, const ava1_test_opts_t *o) {
    ava1_server_cfg_t cfg;
    int rc;
    memset(&cfg, 0, sizeof cfg);
    ava1_identity_from_secret(&cfg.identity, secret);
    strncpy(cfg.name, "C test server", sizeof cfg.name - 1);
    strncpy(cfg.peers_path, peers_path, sizeof cfg.peers_path - 1);
    cfg.bind_loopback = 1;
    cfg.ping_every_ms = o->ping_ms;
    cfg.dead_after_ms = o->dead_ms;
    cfg.handshake_ms = o->handshake_ms;
    cfg.min_frame_rate = o->min_frame_rate;
    cfg.pairing_window_s = o->pairing_s;
    cfg.max_conns_per_ip = o->max_conns_per_ip;
    cfg.max_unpaired = o->max_unpaired;
    cfg.pair_confirm_ms = o->pair_confirm_ms;
    cfg.notify_every_ms = o->notify_every_ms;
    if (o->launch == 2) {
        cfg.has_launch = 1;
        memcpy(cfg.launch_key, o->launch_key, 32);
        memcpy(cfg.launch_token, o->launch_token, 16);
    }
    /* Deliberately asks for the data-plane cap with no hooks — a mistaken embedder. The
     * server must mask it off; `a_server_without_hooks_advertises_no_data_plane_cap`
     * pins that. */
    cfg.caps = AVA1_CAP_DATA_PLANE;
    cfg.on_pair_request = on_pair;
    cfg.log = on_log;
    cfg.rpc = rpc;
    __atomic_store_n(&g_logs, 0, __ATOMIC_SEQ_CST);
    __atomic_store_n(&g_pair_requests, 0, __ATOMIC_SEQ_CST);
    __atomic_store_n(&g_last_code, 0, __ATOMIC_SEQ_CST);
    rc = ava1_server_start(&cfg);
    return rc != 0 ? rc : (int)ava1_server_port();
}

uint32_t ava1_test_pair_requests(void) { return __atomic_load_n(&g_pair_requests, __ATOMIC_SEQ_CST); }
uint32_t ava1_test_logs(void) { return __atomic_load_n(&g_logs, __ATOMIC_SEQ_CST); }
uint32_t ava1_test_last_pair_code(void) { return __atomic_load_n(&g_last_code, __ATOMIC_SEQ_CST); }

int ava1_test_conn_open_frame(const uint8_t key[32], const uint8_t *frame, size_t len) {
    int sv[2], rc;
    ava1_conn_t c;
    uint8_t type, flags, buf[256];
    uint32_t ch;
    size_t n;
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0) return -100;
    if (write(sv[1], frame, len) != (ssize_t)len) {
        close(sv[0]);
        close(sv[1]);
        return -101;
    }
    close(sv[1]);
    ava1_conn_init(&c, sv[0]);
    memcpy(c.recv_key, key, 32);
    c.keyed = 1;
    rc = ava1_conn_recv(&c, &type, &flags, &ch, buf, sizeof buf, &n);
    ava1_conn_destroy(&c);
    close(sv[0]);
    return rc;
}

/* The generated per-struct records helpers (SPEC.md §3), which nothing else on the host
 * executes: count the items in `blob`, read each with _next, re-append them with _append
 * (which is also what drives ava1_w_len_begin/_len_end). 0 = ok, -1 = bad blob,
 * -2 = append failed, -3 = writer error. `count` and `out` are the results. */
int ava1_test_records_helpers(const uint8_t *blob, uint32_t len, uint8_t *out, size_t cap,
                              size_t *out_len, uint32_t *count) {
    ava1_r_t it;
    ava1_node_info_t item;
    ava1_w_t w;
    int rc;
    *count = 0;
    *out_len = 0;
    if (ava1_node_info_count(blob, len, count) != 0) return -1;
    ava1_w_init(&w, out, cap);
    ava1_r_init(&it, blob, len);
    while ((rc = ava1_node_info_next(&it, &item)) == 1) {
        if (ava1_node_info_append(&w, &item) != 0) return -2;
    }
    if (rc != 0) return rc;
    *out_len = w.len;
    return w.err != 0 ? -3 : 0;
}

/* ---------------------------------------------------------------------------
 * Echo data hooks (data_hooks.rs): acknowledge every Chunk; on a JOB_DOWNLOAD
 * open, push one 1 MiB chunk of 0x3c on the session's first lane. Admit at
 * most 12 MiB per frame.
 */

static int echo_send(const uint8_t sid[16], uint8_t type, int (*enc)(const void *, ava1_w_t *), const void *m) {
    uint8_t out[128];
    ava1_w_t w;
    ava1_w_init(&w, out, sizeof out);
    if (enc(m, &w) != 0) return -1;
    /* A hook runs on a reader thread: post, never a waiting send. */
    return ava1_server_post(sid, 0, type, 0, 0, out, w.len);
}

static int enc_received(const void *m, ava1_w_t *w) { return ava1_received_encode(m, w); }
static int enc_credit(const void *m, ava1_w_t *w) { return ava1_credit_encode(m, w); }
static int enc_ack(const void *m, ava1_w_t *w) { return ava1_job_open_ack_encode(m, w); }

static int echo_admit(const uint8_t sid[16], uint16_t lane, size_t len) {
    (void)sid;
    (void)lane;
    return len > (12u << 20);
}

static int echo_on_lane(const uint8_t sid[16], uint16_t lane, uint8_t type, uint32_t seq, uint8_t *body,
                        size_t len) {
    ava1_chunk_t c;
    int rc = 0;
    if (!body) return 0;
    if (type == AVA1_TYPE_CHUNK && ava1_chunk_decode(body, len, &c) == 0) {
        ava1_received_t r;
        ava1_credit_t cr;
        memset(&r, 0, sizeof r);
        memcpy(r.job_id, c.job_id, 16);
        r.lane = lane;
        r.seq = seq;
        memset(&cr, 0, sizeof cr);
        memcpy(cr.job_id, c.job_id, 16);
        cr.bytes = c.data_len;
        rc = echo_send(sid, AVA1_TYPE_RECEIVED, enc_received, &r) != 0 ||
             echo_send(sid, AVA1_TYPE_CREDIT, enc_credit, &cr) != 0;
    }
    free(body);
    return rc;
}

typedef struct {
    uint8_t sid[16];
    uint8_t job[16];
} push_arg_t;

/* Called from on_control, a reader thread: the lane write (and its wait for a live
 * lane) happens on a short-lived thread, never on a reader. */
static void *push_chunk_thread(void *arg) {
    push_arg_t *a = arg;
    uint16_t lanes[AVA1_MAX_LANES];
    ava1_chunk_t c;
    ava1_w_t w;
    size_t cap = AVA1_HEADER_LEN + (1u << 20) + 64 + AVA1_TAG_LEN;
    uint8_t *frame, *data;
    int i, nl = 0;
    for (i = 0; i < 400; i++) { /* the lane may still be proving its key: up to 4 s */
        nl = ava1_server_lanes(a->sid, lanes);
        if (nl > 0) break;
        ava1_platform_sleep_ms(10);
    }
    if (nl == 0) {
        free(a);
        return NULL;
    }
    frame = malloc(cap);
    data = malloc(1u << 20);
    if (!frame || !data) {
        free(frame);
        free(data);
        free(a);
        return NULL;
    }
    memset(data, 0x3c, 1u << 20);
    memset(&c, 0, sizeof c);
    memcpy(c.job_id, a->job, 16);
    c.data = data;
    c.data_len = 1u << 20;
    ava1_w_init(&w, frame + AVA1_HEADER_LEN, cap - AVA1_HEADER_LEN - AVA1_TAG_LEN);
    if (ava1_chunk_encode(&c, &w) == 0)
        (void)ava1_server_send_frame(a->sid, lanes[0], AVA1_TYPE_CHUNK, 1, frame, w.len);
    free(frame);
    free(data);
    free(a);
    return NULL;
}

static int push_chunk(const uint8_t sid[16], const uint8_t job[16]) {
    push_arg_t *a = malloc(sizeof *a);
    pthread_t t;
    if (!a) return -1;
    memcpy(a->sid, sid, 16);
    memcpy(a->job, job, 16);
    if (pthread_create(&t, NULL, push_chunk_thread, a) != 0) {
        free(a);
        return -1;
    }
    pthread_detach(t);
    return 0;
}

static int echo_on_control(const uint8_t sid[16], const uint8_t peer[32], uint8_t type, uint8_t flags,
                           const uint8_t *body, size_t len) {
    ava1_job_open_t o;
    ava1_job_open_ack_t a;
    (void)peer;
    (void)flags;
    if (type != AVA1_TYPE_JOB_OPEN || ava1_job_open_decode(body, len, &o) != 0) return 0;
    if (o.kind == AVA1_JOB_DOWNLOAD) return push_chunk(sid, o.job_id) != 0;
    memset(&a, 0, sizeof a);
    memcpy(a.job_id, o.job_id, 16);
    a.credit = 64u << 20;
    return echo_send(sid, AVA1_TYPE_JOB_OPEN_ACK, enc_ack, &a) != 0;
}

static const ava1_data_hooks_t ECHO = {
    echo_on_control, echo_admit, echo_on_lane, NULL, NULL,
};

int ava1_test_server_start_echo(const uint8_t secret[32], const char *peers_path, uint32_t ping_ms,
                                uint32_t dead_ms, uint32_t handshake_ms) {
    ava1_server_cfg_t cfg;
    int rc;
    memset(&cfg, 0, sizeof cfg);
    ava1_identity_from_secret(&cfg.identity, secret);
    strncpy(cfg.name, "C echo server", sizeof cfg.name - 1);
    strncpy(cfg.peers_path, peers_path, sizeof cfg.peers_path - 1);
    cfg.bind_loopback = 1;
    cfg.ping_every_ms = ping_ms;
    cfg.dead_after_ms = dead_ms;
    cfg.handshake_ms = handshake_ms;
    cfg.rpc = rpc;
    cfg.data = &ECHO;
    cfg.caps = AVA1_CAP_DATA_PLANE;
    rc = ava1_server_start(&cfg);
    return rc != 0 ? rc : (int)ava1_server_port();
}

/* The post queue (ava1_conn_post): post while the writer cannot send (wmu held,
 * nothing read), so nothing can be written; then read every frame back whole and in
 * order. A second conn proves the bound: with the writer parked (it pops under qmu,
 * then waits for wmu), the queue itself holds AVA1_Q_ENTRIES frames, one post more
 * returns AVA1_E_BUSY and breaks the connection. 0 = ok, negative = which check. */
int ava1_test_post_queue(const uint8_t key[32]) {
    int sv[2], rc = 0;
    ava1_conn_t c, peer;
    uint8_t body[64], buf[64];
    uint8_t type, flags;
    uint32_t ch;
    size_t n;
    unsigned i, tries;
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0) return -100;
    ava1_conn_init(&c, sv[0]);
    ava1_conn_init(&peer, sv[1]);
    memcpy(c.send_key, key, 32);
    memcpy(c.recv_key, key, 32);
    memcpy(peer.recv_key, key, 32);
    c.keyed = 1;
    peer.keyed = 1;
    pthread_mutex_lock(&c.wmu); /* the writer takes one frame, then waits here */
    for (i = 0; i < AVA1_Q_ENTRIES; i++) {
        memset(body, (int)i, sizeof body);
        if (ava1_conn_post(&c, AVA1_TYPE_CHUNK, 0, i, body, sizeof body) != 0) {
            rc = -101;
            goto out;
        }
    }
    pthread_mutex_unlock(&c.wmu);
    /* Everything queued while nothing could be written arrives whole and in order. */
    for (i = 0; i < AVA1_Q_ENTRIES; i++) {
        if (ava1_conn_recv(&peer, &type, &flags, &ch, buf, sizeof buf, &n) != 0) {
            rc = -102;
            goto out;
        }
        if (type != AVA1_TYPE_CHUNK || ch != i || n != sizeof body || buf[0] != (uint8_t)i || buf[63] != (uint8_t)i) {
            rc = -103;
            goto out;
        }
    }
out:
    ava1_conn_destroy(&peer);
    ava1_conn_destroy(&c);
    close(sv[0]);
    close(sv[1]);
    if (rc != 0) return rc;

    /* The bound, on a fresh conn: the writer pops under qmu and only then waits for
     * wmu, so let it take one frame first — parked, it cannot free room again, and
     * the queue itself is what one post more than AVA1_Q_ENTRIES must exceed. */
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0) return -104;
    ava1_conn_init(&c, sv[0]);
    memcpy(c.send_key, key, 32);
    memcpy(c.recv_key, key, 32);
    c.keyed = 1;
    pthread_mutex_lock(&c.wmu);
    memset(body, 0, sizeof body);
    if (ava1_conn_post(&c, AVA1_TYPE_CHUNK, 0, 0, body, sizeof body) != 0) {
        rc = -105;
        goto bound_out;
    }
    /* The queue's writer pops under qmu and only then waits for wmu, so wait (bounded, in
     * case the thread is starved) until it has taken that first frame: parked on wmu, it
     * cannot free more room. */
    for (tries = 0; tries < 5000; tries++) {
        pthread_mutex_lock(&c.qmu);
        n = c.q_n;
        pthread_mutex_unlock(&c.qmu);
        if (!n) break;
        ava1_platform_sleep_ms(1);
    }
    if (n) {
        rc = -109; /* the writer never took the frame */
        goto bound_out;
    }
    for (i = 0; i < AVA1_Q_ENTRIES; i++) {
        memset(body, (int)i, sizeof body);
        if (ava1_conn_post(&c, AVA1_TYPE_CHUNK, 0, i + 1, body, sizeof body) != 0) {
            rc = -105;
            goto bound_out;
        }
    }
    if (ava1_conn_post(&c, AVA1_TYPE_CHUNK, 0, 0xffffffffu, body, sizeof body) != AVA1_E_BUSY) rc = -106;
    if (rc == 0 && !c.broken) rc = -107; /* a full queue breaks the connection */
    if (rc == 0 && ava1_conn_post(&c, AVA1_TYPE_CHUNK, 0, 0xffffffffu, body, sizeof body) != AVA1_E_IO)
        rc = -108; /* and a broken one refuses everything */
bound_out:
    pthread_mutex_unlock(&c.wmu);
    ava1_conn_destroy(&c);
    close(sv[0]);
    close(sv[1]);
    return rc;
}
