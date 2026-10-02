/* Starts the payload's AVA1 server on the host with a node.info handler (tests only). */
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#include "ava1_conn.h"
#include "ava1_platform.h"

#include "ava1_gen.h"
#include "ava1_journal.h"
#include "ava1_manifest.h"
#include "ava1_ranges.h"
#include "ava1_server.h"
#include "ava1_thread.h"

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

/* Applies ops[2i]..ops[2i+1] and writes the resulting pairs to out; returns pair count. */
size_t ava1_test_rset_after(const uint64_t *ops, size_t nops, uint64_t *out, size_t cap_pairs) {
    ava1_rset_t r;
    size_t i, n;
    memset(&r, 0, sizeof r);
    for (i = 0; i < nops; i++) (void)ava1_rset_add(&r, ops[2 * i], ops[2 * i + 1]);
    n = r.n < cap_pairs ? r.n : cap_pairs;
    memcpy(out, r.v, n * 2 * sizeof *out);
    ava1_rset_clear(&r);
    return n;
}

/* FileRun pairs (first, count) for the given set bits; returns pair count. */
size_t ava1_test_bits_runs(uint32_t n, const uint32_t *set, size_t nset, uint32_t *out, size_t cap_pairs) {
    ava1_bits_t b;
    ava1_w_t w;
    ava1_r_t it;
    ava1_file_run_t r;
    uint8_t *blob = malloc(16 + 16 * (size_t)n);
    size_t i, k = 0;
    if (!blob || ava1_bits_init(&b, n) != 0) {
        free(blob);
        return 0;
    }
    for (i = 0; i < nset; i++) ava1_bits_set(&b, set[i]);
    ava1_w_init(&w, blob, 16 + 16 * (size_t)n);
    (void)ava1_bits_append_runs(&b, &w);
    ava1_r_init(&it, blob, w.len);
    while (k < cap_pairs && ava1_file_run_next(&it, &r) == 1) {
        out[2 * k] = r.first;
        out[2 * k + 1] = r.count;
        k++;
    }
    ava1_bits_free(&b);
    free(blob);
    return k;
}

/* Builds the same text the Rust test's c_style_dump builds, from a C replay. */
typedef struct {
    char *out;
    size_t cap, len;
    uint8_t done[4096];
    ava1_rset_t ranges[2048];
    uint8_t roots[2048][32];
    uint8_t has_root[2048];
    int finished;
    uint16_t status;
} jdump_t;

static void jdump_say(jdump_t *d, const char *s) {
    size_t n = strlen(s);
    if (d->len + n < d->cap) {
        memcpy(d->out + d->len, s, n);
        d->len += n;
    }
}

/* 0 on success; nonzero if a records list does not decode (treated as torn, like Rust). */
static int jdump_batch(jdump_t *d, const ava1_jnl_batch_t *b) {
    ava1_r_t it;
    ava1_file_run_t r;
    ava1_file_range_t g;
    ava1_root_item_t ri;
    int rc;
    ava1_r_init(&it, b->files, b->files_len);
    while ((rc = ava1_file_run_next(&it, &r)) == 1) {
        uint32_t f;
        for (f = r.first; f < r.first + r.count && f < 4096; f++) {
            d->done[f] = 1;
            if (f < 2048) ava1_rset_clear(&d->ranges[f]);
        }
    }
    if (rc < 0) return 1;
    ava1_r_init(&it, b->ranges, b->ranges_len);
    while ((rc = ava1_file_range_next(&it, &g)) == 1)
        if (g.file_id < 2048) (void)ava1_rset_add(&d->ranges[g.file_id], g.offset, g.offset + g.len);
    if (rc < 0) return 1;
    ava1_r_init(&it, b->roots, b->roots_len);
    while ((rc = ava1_root_item_next(&it, &ri)) == 1)
        if (ri.file_id < 2048) {
            memcpy(d->roots[ri.file_id], ri.root, 32);
            d->has_root[ri.file_id] = 1;
        }
    return rc < 0 ? 1 : 0;
}

static int jdump_visit(void *ctx, uint8_t kind, const uint8_t *body, size_t len) {
    jdump_t *d = ctx;
    if (kind == AVA1_JNL_BATCH) {
        ava1_jnl_batch_t b;
        if (ava1_jnl_batch_decode(body, len, &b) != 0) return 1;
        if (jdump_batch(d, &b) != 0) return 1;
    } else if (kind == AVA1_JNL_RESET) {
        ava1_jnl_reset_t r;
        if (ava1_jnl_reset_decode(body, len, &r) != 0 || r.file_id >= 2048) return 1;
        d->done[r.file_id] = 0;
        ava1_rset_clear(&d->ranges[r.file_id]);
        d->has_root[r.file_id] = 0;
    } else if (kind == AVA1_JNL_DONE) {
        ava1_jnl_done_t x;
        if (ava1_jnl_done_decode(body, len, &x) != 0) return 1;
        d->finished = 1;
        d->status = x.status;
    } else if (kind == AVA1_JNL_SNAPSHOT) {
        /* Test-only visitor: Rust's replay applies a snapshot, but the cross-tests never
         * write one. Accept it so replay does not stop; the real C replay (Task 13) must
         * apply it. */
    } else if (kind != AVA1_JNL_OPEN) {
        return 1; /* unknown kind: torn from here on, same as Rust's Record::decode */
    }
    return 0;
}

/* Test-only: dump_t is calloc'd, which is what the ava1_rset_t zero-init contract requires. */
size_t ava1_test_journal_dump(const char *dir, char *out, size_t cap) {
    jdump_t *d = calloc(1, sizeof *d);
    ava1_jnl_t j;
    char line[96];
    uint32_t f;
    size_t i, n;
    if (!d) return 0;
    d->out = out;
    d->cap = cap;
    if (ava1_jnl_open(&j, dir, jdump_visit, d) == 0) ava1_jnl_close(&j);
    for (f = 0; f < 4096; f++)
        if (d->done[f]) {
            snprintf(line, sizeof line, "done %u\n", f);
            jdump_say(d, line);
        }
    for (f = 0; f < 2048; f++)
        for (i = 0; i < d->ranges[f].n; i++) {
            snprintf(line, sizeof line, "range %u %llu %llu\n", f,
                     (unsigned long long)d->ranges[f].v[2 * i],
                     (unsigned long long)d->ranges[f].v[2 * i + 1]);
            jdump_say(d, line);
        }
    for (f = 0; f < 2048; f++)
        if (d->has_root[f]) {
            snprintf(line, sizeof line, "root %u %02x\n", f, d->roots[f][0]);
            jdump_say(d, line);
        }
    if (d->finished) snprintf(line, sizeof line, "finished=%u", d->status);
    else snprintf(line, sizeof line, "finished=none");
    jdump_say(d, line);
    for (f = 0; f < 2048; f++) ava1_rset_clear(&d->ranges[f]);
    n = d->len;
    free(d);
    return n;
}

/* Test-only: writes a journal a Rust replay can read. Returns 0 or a negative error, so a
 * failed create/append cannot masquerade as an empty journal. */
int ava1_test_journal_write_sample(const char *dir) {
    ava1_jnl_t j;
    ava1_jnl_open_t o;
    uint8_t body[256], blob[64];
    ava1_w_t w, bw;
    uint32_t i;
    int rc;
    memset(&o, 0, sizeof o);
    o.kind = 1;
    o.root = (const uint8_t *)"/data/c";
    o.root_len = 7;
    rc = ava1_jnl_create(&j, dir, &o);
    if (rc != 0) return rc;
    for (i = 0; i < 10; i++) {
        ava1_jnl_batch_t b;
        ava1_file_run_t r;
        memset(&b, 0, sizeof b);
        memset(&r, 0, sizeof r);
        r.first = i;
        r.count = 1;
        ava1_w_init(&bw, blob, sizeof blob);
        rc = ava1_file_run_append(&bw, &r);
        if (rc != 0) break;
        b.files = blob;
        b.files_len = (uint32_t)bw.len;
        ava1_w_init(&w, body, sizeof body);
        rc = ava1_jnl_batch_encode(&b, &w);
        if (rc != 0) break;
        rc = ava1_jnl_append(&j, AVA1_JNL_BATCH, body, w.len);
        if (rc != 0) break;
    }
    if (rc == 0) {
        ava1_jnl_reset_t r;
        r.file_id = 3;
        ava1_w_init(&w, body, sizeof body);
        rc = ava1_jnl_reset_encode(&r, &w);
        if (rc == 0) rc = ava1_jnl_append(&j, AVA1_JNL_RESET, body, w.len);
    }
    if (rc == 0) {
        ava1_jnl_done_t x;
        x.status = 0;
        ava1_w_init(&w, body, sizeof body);
        rc = ava1_jnl_done_encode(&x, &w);
        if (rc == 0) rc = ava1_jnl_append(&j, AVA1_JNL_DONE, body, w.len);
    }
    ava1_jnl_close(&j);
    return rc;
}

/* Test-only: compacts the journal at `dir` with bodies the Rust test built, so the C
 * compaction's byte layout and its Done-preservation are judged by the Rust reader.
 * Returns 0 or a negative error. */
int ava1_test_journal_compact(const char *dir, const uint8_t *open_b, size_t open_n,
                              const uint8_t *snap_b, size_t snap_n, const uint8_t *done_b,
                              size_t done_n) {
    ava1_jnl_t j;
    int rc = ava1_jnl_open(&j, dir, NULL, NULL);
    if (rc != 0) return rc;
    rc = ava1_jnl_compact(&j, open_b, open_n, snap_b, snap_n, done_b, done_n);
    ava1_jnl_close(&j);
    return rc;
}

/* ---------------------------------------------------------------------------
 * The manifest store (ava1_manifest.c): decode the pages a Rust test encoded,
 * or walk a real tree, and report the C store's hash, entry count and bytes.
 */

int ava1_test_mstore_pages(const uint8_t *const *pages, const size_t *lens, size_t n,
                           uint8_t hash[32], uint32_t *count, uint64_t *bytes) {
    ava1_mstore_t m;
    size_t i;
    int rc = 0;
    memset(&m, 0, sizeof m);
    for (i = 0; i < n && rc == 0; i++) {
        ava1_manifest_page_t p;
        rc = ava1_manifest_page_decode(pages[i], lens[i], &p);
        if (rc == 0) rc = ava1_mstore_add_page(&m, &p);
    }
    ava1_mstore_hash(&m, hash);
    *count = m.n;
    *bytes = m.bytes;
    ava1_mstore_free(&m);
    return rc;
}

int ava1_test_mstore_walk(const char *root, uint8_t hash[32], uint32_t *count) {
    ava1_mstore_t m;
    int rc;
    memset(&m, 0, sizeof m);
    rc = ava1_mstore_walk(&m, root);
    ava1_mstore_hash(&m, hash);
    *count = m.n;
    ava1_mstore_free(&m);
    return rc;
}

/* ---------------------------------------------------------------------------
 * ava1_thread_start: the mandated 256 KiB stacks (SPEC.md §15).
 */

static void *smoke(void *arg) {
    volatile uint8_t big[200 * 1024]; /* more than the 64 KiB default-alike rules allow */
    size_t sz = 0;
#if defined(__APPLE__)
    /* macOS pads the reported allocation (measured +12..20 KiB over the request);
     * the smoke's ceiling allows that slack below, and the default 512 KiB stack
     * still fails it. */
    sz = pthread_get_stacksize_np(pthread_self());
#else
    {
        pthread_attr_t a;
        if (pthread_getattr_np(pthread_self(), &a) == 0) {
            pthread_attr_getstacksize(&a, &sz);
            pthread_attr_destroy(&a);
        }
    }
#endif
    big[0] = 1;
    big[sizeof big - 1] = 2;
    *(size_t *)arg = sz;
    return (void *)(uintptr_t)(big[0] + big[sizeof big - 1] == 3 ? 0 : 1);
}

/* 0 only when the 200 KiB frame survived AND the observed stack is within
 * [200 KiB, AVA1_THREAD_STACK + 4 KiB] — 64 KiB of ceiling on macOS, whose
 * pthreads pad the reported allocation — the default pthread stack must not pass. */
int ava1_test_thread_smoke(size_t *stack_bytes) {
    pthread_t t;
    void *ret = (void *)1;
    size_t sz = 0, max = AVA1_THREAD_STACK + 4096u;
#if defined(__APPLE__)
    max = AVA1_THREAD_STACK + 64u * 1024u;
#endif
    if (ava1_thread_start(smoke, &sz, &t) != 0) return -2;
    pthread_join(t, &ret);
    *stack_bytes = sz;
    if ((uintptr_t)ret != 0) return -1;
    return (sz >= 200u * 1024u && sz <= max) ? 0 : -3;
}
