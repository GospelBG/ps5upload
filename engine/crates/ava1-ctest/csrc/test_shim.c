/* Starts the payload's AVA1 server on the host with a node.info handler (tests only). */
#include <stdint.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#include "ava1_conn.h"

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
} ava1_test_opts_t;

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
