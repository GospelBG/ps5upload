/* AVA1 on the console: identity and peers under /data/ps5upload/ava, the launching
 * engine trusted through the ELF's trust slot. Pairing opens by itself for 5 minutes
 * only while nothing is paired; otherwise a paired device opens it (pairing.open). */
#include "ava1_glue.h"

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>

#include "ava1_gen.h"
#include "ava1_noise.h"
#include "ava1_server.h"
#include "ava1_store.h"
#include "ava1_trust.h"
#include "config.h"
#include "monocypher.h"
#include "runtime.h"

#define AVA1_DIR "/data/ps5upload/ava"

static void on_pair_request(const char *peer_name, uint32_t code) {
    char msg[160];
    snprintf(msg, sizeof msg, "PS5Upload: pairing request from %s. Code %06u", peer_name, (unsigned)code);
    pop_notification(msg);
}

static void on_log(const char *msg) { fprintf(stderr, "%s\n", msg); }

static uint64_t mono_us(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000u + (uint64_t)ts.tv_nsec / 1000u;
}

/* crypto.bench: seals `mib` MiB in memory and reports how long it took, so the
 * ChaCha20 cost is measured on this console (spec risk table). */
static int crypto_bench(const uint8_t *body, uint32_t body_len, uint8_t *out, size_t cap, size_t *out_len) {
    ava1_crypto_bench_t q;
    ava1_crypto_bench_result_t r;
    ava1_w_t w;
    uint8_t key[32], mac[16], *buf;
    uint64_t t0;
    unsigned i, mib;
    if (ava1_crypto_bench_decode(body, body_len, &q) != 0) return AVA1_ERR_PROTOCOL;
    mib = q.mib == 0 ? 1u : (q.mib > 256 ? 256u : q.mib);
    buf = malloc(1u << 20);
    if (!buf) return AVA1_ERR_INTERNAL;
    memset(buf, 0x5a, 1u << 20);
    memset(key, 0x11, sizeof key);
    t0 = mono_us();
    for (i = 0; i < mib; i++) ava1_seal(key, i, NULL, 0, buf, 1u << 20, mac);
    memset(&r, 0, sizeof r);
    r.bytes = (uint64_t)mib << 20;
    r.micros = mono_us() - t0;
    free(buf);
    ava1_w_init(&w, out, cap);
    if (ava1_crypto_bench_result_encode(&r, &w) != 0) return AVA1_ERR_INTERNAL;
    *out_len = w.len;
    return AVA1_STATUS_OK;
}

static int rpc(uint16_t method, const uint8_t *body, uint32_t body_len, uint8_t *out, size_t cap,
               size_t *out_len) {
    static const char version[] = PS5UPLOAD2_VERSION;
    ava1_node_info_t ni;
    ava1_w_t w;
    if (method == AVA1_METHOD_CRYPTO_BENCH) return crypto_bench(body, body_len, out, cap, out_len);
    if (method != AVA1_METHOD_NODE_INFO) return AVA1_ERR_UNKNOWN_METHOD;
    memset(&ni, 0, sizeof ni);
    ni.version = (const uint8_t *)version;
    ni.version_len = (uint16_t)(sizeof version - 1);
    ni.platform = (const uint8_t *)"ps5";
    ni.platform_len = 3;
    ni.name = (const uint8_t *)"PS5";
    ni.name_len = 3;
    ava1_w_init(&w, out, cap);
    if (ava1_node_info_encode(&ni, &w) != 0) return AVA1_ERR_INTERNAL;
    *out_len = w.len;
    return AVA1_STATUS_OK;
}

int ava1_payload_start(void) {
    ava1_server_cfg_t cfg;
    uint8_t launcher[32];
    memset(&cfg, 0, sizeof cfg);
    if (mkdir("/data/ps5upload", 0755) != 0 && errno != EEXIST) return -errno;
    if (mkdir(AVA1_DIR, 0755) != 0 && errno != EEXIST) return -errno;
    if (ava1_identity_load_or_create(AVA1_DIR "/identity", &cfg.identity) != 0) return -EIO;
    snprintf(cfg.peers_path, sizeof cfg.peers_path, "%s", AVA1_DIR "/peers");
    snprintf(cfg.name, sizeof cfg.name, "%s", "PS5");
    cfg.port = AVA1_DEFAULT_PORT;
    cfg.ping_every_ms = 2000;
    cfg.dead_after_ms = 6000;
    cfg.handshake_ms = 10000;
    cfg.pairing_window_s = 300;
    cfg.on_pair_request = on_pair_request;
    cfg.rpc = rpc;
    cfg.log = on_log;
    if (ava1_trust_slot_key(launcher) == 0) {
        /* Heap: ava1_peers_t is a few KB, more than this thread's stack should carry. */
        ava1_peers_t *peers = malloc(sizeof *peers);
        if (!peers) {
            on_log("ava1: launcher not trusted: out of memory");
        } else if (ava1_peers_load(peers, cfg.peers_path) != 0) {
            /* The server logs the unreadable file itself and keeps pairing closed. */
            on_log("ava1: launcher not trusted: the peers file could not be read");
        } else if (!ava1_peers_contains(peers, launcher) &&
                   ava1_peers_add(peers, launcher, "launcher", (uint64_t)time(NULL), cfg.peers_path) != 0) {
            on_log("ava1: launcher not trusted: cannot write " AVA1_DIR "/peers");
        }
        free(peers);
    }
    {
        /* The server keeps its own copy: do not leave the private key on this stack. */
        int rc = ava1_server_start(&cfg);
        crypto_wipe(&cfg.identity, sizeof cfg.identity);
        return rc;
    }
}
