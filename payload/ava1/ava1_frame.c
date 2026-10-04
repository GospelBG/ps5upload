#include "ava1_frame.h"

#include "ava1_wire.h"

#include <pthread.h>
#include <stdlib.h>

#define MIB (1024u * 1024u)
#define DEFAULT_POOL_BUDGET (96u * MIB)

static const size_t CLASS_CAP[AVA1_FRAME_CLASSES] = { 1u * MIB, 4u * MIB, 8u * MIB, 16u * MIB };
/* A chunk of N MiB arrives as a body of N MiB plus its message header: the buffer is a little
 * larger than the class so that body still belongs to class N rather than the next one up
 * (which would hold twice the memory the admit budget counted). */
#define CLASS_SLACK (64u * 1024u)

typedef struct idle_node {
    struct idle_node *next;
} idle_node_t;

static struct {
    pthread_mutex_t mu;
    idle_node_t *head[AVA1_FRAME_CLASSES];
    size_t n[AVA1_FRAME_CLASSES];
    uint64_t budget; /* 0 = default */
    uint64_t idle_bytes;
    size_t outstanding;
} P = { PTHREAD_MUTEX_INITIALIZER, { 0 }, { 0 }, 0, 0, 0 };

static uint64_t budget_locked(void) { return P.budget ? P.budget : DEFAULT_POOL_BUDGET; }

static int class_of_len(size_t len) {
    int i;
    if (len <= CLASS_CAP[0] / 2) return -1;
    for (i = 0; i < AVA1_FRAME_CLASSES; i++)
        if (len <= CLASS_CAP[i] + CLASS_SLACK) return i;
    return -1;
}

static int class_of_cap(size_t cap) {
    int i;
    for (i = 0; i < AVA1_FRAME_CLASSES; i++)
        if (cap == CLASS_CAP[i]) return i;
    return -1;
}

size_t ava1_frame_class(size_t len) {
    int i = class_of_len(len);
    return i < 0 ? 0 : CLASS_CAP[i];
}

void *ava1_frame_alloc(size_t len, size_t *cap) {
    int i = class_of_len(len);
    void *p = NULL;
    if (i < 0) {
        p = malloc(len ? len : 1);
        if (cap) *cap = 0;
        if (p) {
            pthread_mutex_lock(&P.mu);
            P.outstanding++;
            pthread_mutex_unlock(&P.mu);
        }
        return p;
    }
    pthread_mutex_lock(&P.mu);
    if (P.head[i]) {
        idle_node_t *n = P.head[i];
        P.head[i] = n->next;
        P.n[i]--;
        P.idle_bytes -= CLASS_CAP[i];
        p = n;
    }
    pthread_mutex_unlock(&P.mu);
    if (!p) p = malloc(CLASS_CAP[i] + CLASS_SLACK);
    if (!p) return NULL;
    pthread_mutex_lock(&P.mu);
    P.outstanding++;
    pthread_mutex_unlock(&P.mu);
    if (cap) *cap = CLASS_CAP[i];
    return p;
}

int ava1_frame_free(void *p, size_t cap) {
    int i;
    idle_node_t *n;
    uint64_t b;
    if (!p) return 0;
    i = class_of_cap(cap);
    if (i < 0) {
        pthread_mutex_lock(&P.mu);
        if (P.outstanding) P.outstanding--;
        pthread_mutex_unlock(&P.mu);
        free(p);
        return 0;
    }
    pthread_mutex_lock(&P.mu);
    for (n = P.head[i]; n; n = n->next) {
        if ((void *)n == p) {
            pthread_mutex_unlock(&P.mu);
            return -1; /* a double free: the buffer is already idle in the pool */
        }
    }
    if (P.outstanding) P.outstanding--;
    b = budget_locked();
    if (P.n[i] < b / CLASS_CAP[i] && P.idle_bytes + CLASS_CAP[i] <= b) {
        n = p;
        n->next = P.head[i];
        P.head[i] = n;
        P.n[i]++;
        P.idle_bytes += CLASS_CAP[i];
        pthread_mutex_unlock(&P.mu);
        return 0;
    }
    pthread_mutex_unlock(&P.mu);
    free(p);
    return 0;
}

static void trim_locked(uint64_t keep_bytes) {
    int i;
    for (i = AVA1_FRAME_CLASSES - 1; i >= 0 && P.idle_bytes > keep_bytes;) {
        idle_node_t *n = P.head[i];
        if (!n) {
            i--;
            continue;
        }
        P.head[i] = n->next;
        P.n[i]--;
        P.idle_bytes -= CLASS_CAP[i];
        free(n);
    }
}

void ava1_frame_pool_set_budget(uint64_t bytes) {
    pthread_mutex_lock(&P.mu);
    P.budget = bytes;
    trim_locked(budget_locked());
    pthread_mutex_unlock(&P.mu);
}

size_t ava1_frame_pool_idle(int cls) {
    size_t n = 0;
    if (cls < 0 || cls >= AVA1_FRAME_CLASSES) return 0;
    pthread_mutex_lock(&P.mu);
    n = P.n[cls];
    pthread_mutex_unlock(&P.mu);
    return n;
}

size_t ava1_frame_pool_outstanding(void) {
    size_t n;
    pthread_mutex_lock(&P.mu);
    n = P.outstanding;
    pthread_mutex_unlock(&P.mu);
    return n;
}

void ava1_frame_pool_trim(void) {
    pthread_mutex_lock(&P.mu);
    trim_locked(0);
    pthread_mutex_unlock(&P.mu);
}

uint32_t ava1_crc32c(const uint8_t *p, size_t n) {
    uint32_t crc = 0xFFFFFFFFu;
    size_t i;
    int k;
    for (i = 0; i < n; i++) {
        crc ^= p[i];
        for (k = 0; k < 8; k++) crc = (crc & 1u) ? (crc >> 1) ^ 0x82F63B78u : crc >> 1;
    }
    return crc ^ 0xFFFFFFFFu;
}

static void put32(uint8_t *b, uint32_t v) {
    b[0] = (uint8_t)v;
    b[1] = (uint8_t)(v >> 8);
    b[2] = (uint8_t)(v >> 16);
    b[3] = (uint8_t)(v >> 24);
}

static uint32_t get32(const uint8_t *b) {
    return (uint32_t)b[0] | ((uint32_t)b[1] << 8) | ((uint32_t)b[2] << 16) | ((uint32_t)b[3] << 24);
}

void ava1_header_encode(const ava1_header_t *h, uint8_t out[AVA1_HEADER_LEN]) {
    out[0] = 'A';
    out[1] = '1';
    out[2] = h->type;
    out[3] = h->flags;
    put32(out + 4, h->channel);
    put32(out + 8, h->body_len);
    put32(out + 12, ava1_crc32c(out, 12));
}

int ava1_header_decode(const uint8_t in[AVA1_HEADER_LEN], ava1_header_t *h) {
    if (in[0] != 'A' || in[1] != '1') return AVA1_E_MAGIC;
    if (get32(in + 12) != ava1_crc32c(in, 12)) return AVA1_E_CRC;
    h->type = in[2];
    h->flags = in[3];
    h->channel = get32(in + 4);
    h->body_len = get32(in + 8);
    if (h->body_len > AVA1_MAX_BODY) return AVA1_E_TOOLONG;
    return 0;
}
