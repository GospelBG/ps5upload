#include "ava1_data.h"

#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "ava1_apply.h"
#include "ava1_job.h"
#include "ava1_platform.h"
#include "ava1_recv.h"
#include "ava1_thread.h"

#define IN_MAX (32u << 20)   /* control bytes one job's inbox holds */
#define OPEN_MAX (16u << 20) /* control bytes queued behind one JobOpen still opening */

uint32_t ava1_data_test_open_delay_ms;
uint32_t ava1_data_test_map_delay_ms;

static struct {
    ava1_data_cfg_t cfg;
    pthread_mutex_t mu;
    pthread_cond_t cv;
    uint64_t budget_free;
    uint64_t admitted; /* lane bytes between their header and on_lane */
    int bg;            /* ava1_data_spawn threads running */
    volatile int running;
    pthread_t house;
} D = { .mu = PTHREAD_MUTEX_INITIALIZER, .cv = PTHREAD_COND_INITIALIZER };

const ava1_data_cfg_t *ava1_data_cfg(void) { return &D.cfg; }

uint64_t ava1_budget_take(uint64_t want, uint64_t min) {
    uint64_t g;
    pthread_mutex_lock(&D.mu);
    g = want < D.budget_free ? want : D.budget_free;
    if (g < min) g = 0;
    D.budget_free -= g;
    pthread_mutex_unlock(&D.mu);
    return g;
}

void ava1_budget_give(uint64_t n) {
    pthread_mutex_lock(&D.mu);
    D.budget_free += n;
    pthread_mutex_unlock(&D.mu);
}

typedef struct {
    void *(*fn)(void *);
    void *arg;
} bg_t;

static void *bg_main(void *p) {
    bg_t b = *(bg_t *)p;
    free(p);
    b.fn(b.arg);
    pthread_mutex_lock(&D.mu);
    D.bg--;
    pthread_cond_broadcast(&D.cv);
    pthread_mutex_unlock(&D.mu);
    return NULL;
}

int ava1_data_spawn(void *(*fn)(void *), void *arg) {
    bg_t *b = malloc(sizeof *b);
    if (!b) return -1;
    b->fn = fn;
    b->arg = arg;
    pthread_mutex_lock(&D.mu);
    D.bg++;
    pthread_mutex_unlock(&D.mu);
    if (ava1_thread_start(bg_main, b, NULL) != 0) {
        free(b);
        pthread_mutex_lock(&D.mu);
        D.bg--;
        pthread_cond_broadcast(&D.cv);
        pthread_mutex_unlock(&D.mu);
        return -1;
    }
    return 0;
}

static void *house_main(void *arg) {
    (void)arg;
    while (D.running) {
        ava1_job_reap(ava1_mono_ms());
        ava1_platform_sleep_ms(1000);
    }
    return NULL;
}

int ava1_data_start(const ava1_data_cfg_t *cfg) {
    if (D.running) return -EBUSY; /* one housekeeping thread; a second start changes nothing */
    D.cfg = *cfg;
    if (!D.cfg.budget) D.cfg.budget = 96u << 20;
    if (!D.cfg.workers_start) D.cfg.workers_start = 4;
    if (!D.cfg.workers_min) D.cfg.workers_min = 2;
    if (!D.cfg.workers_max) D.cfg.workers_max = 16;
    /* job->workers[] holds 16; min <= start <= max */
    if (D.cfg.workers_max > 16) D.cfg.workers_max = 16;
    if (D.cfg.workers_min > D.cfg.workers_max) D.cfg.workers_min = D.cfg.workers_max;
    if (D.cfg.workers_start > D.cfg.workers_max) D.cfg.workers_start = D.cfg.workers_max;
    if (D.cfg.workers_start < D.cfg.workers_min) D.cfg.workers_start = D.cfg.workers_min;
    if (!D.cfg.cutoff) D.cfg.cutoff = 256u << 10;
    D.budget_free = D.cfg.budget;
    D.admitted = 0;
    ava1_data_test_open_delay_ms = ava1_data_test_map_delay_ms = 0;
    D.running = 1;
    if (ava1_thread_start(house_main, NULL, &D.house) != 0) {
        D.running = 0;
        return -EAGAIN;
    }
    return 0;
}

void ava1_data_stop(void) {
    if (!D.running) return;
    D.running = 0;
    pthread_mutex_lock(&D.mu); /* JobOpens in progress, last puts: they end on their own */
    while (D.bg) pthread_cond_wait(&D.cv, &D.mu);
    pthread_mutex_unlock(&D.mu);
    pthread_join(D.house, NULL);
    ava1_job_free_all();
}

/* ---- messages ---------------------------------------------------------------------- */

/* For hooks (reader threads): queued on the connection, never waiting for the socket. */
static void post_error(const uint8_t sid[16], uint16_t lane, uint16_t code, const char *msg) {
    ava1_error_t e;
    uint8_t b[256];
    ava1_w_t w;
    memset(&e, 0, sizeof e);
    e.code = code;
    e.message = (const uint8_t *)msg;
    e.message_len = (uint16_t)strlen(msg);
    ava1_w_init(&w, b, sizeof b);
    if (ava1_error_encode(&e, &w) == 0) (void)ava1_server_post(sid, lane, AVA1_TYPE_ERROR, 0, 0, b, w.len);
}

static void post_unknown_map(const uint8_t sid[16], const uint8_t job[16]) {
    static const char why[] = "no such job here: open it";
    ava1_job_map_t m;
    uint8_t b[128];
    ava1_w_t w;
    memset(&m, 0, sizeof m);
    memcpy(m.job_id, job, 16);
    m.status = AVA1_ERR_UNKNOWN_JOB;
    m.last = 1;
    m.has_message = 1;
    m.message = (const uint8_t *)why;
    m.message_len = (uint16_t)(sizeof why - 1);
    ava1_w_init(&w, b, sizeof b);
    if (ava1_job_map_encode(&m, &w) == 0) (void)ava1_server_post(sid, 0, AVA1_TYPE_JOB_MAP, 0, 0, b, w.len);
}

static void post_received(const uint8_t sid[16], const uint8_t job[16], uint16_t lane, uint32_t seq) {
    ava1_received_t r;
    uint8_t b[64];
    ava1_w_t w;
    memset(&r, 0, sizeof r);
    memcpy(r.job_id, job, 16);
    r.lane = lane;
    r.seq = seq;
    ava1_w_init(&w, b, sizeof b);
    if (ava1_received_encode(&r, &w) == 0) (void)ava1_server_post(sid, 0, AVA1_TYPE_RECEIVED, 0, 0, b, w.len);
}

/* JobOpenAck: a refusal carries why. `post` for hooks; otherwise the waiting send. */
static int send_ack(const uint8_t sid[16], const ava1_job_open_ack_t *ack, const char *msg, int post) {
    ava1_job_open_ack_t a = *ack;
    uint8_t b[256];
    ava1_w_t w;
    if (msg && *msg) {
        a.has_message = 1;
        a.message = (const uint8_t *)msg;
        a.message_len = (uint16_t)strnlen(msg, 160);
    }
    ava1_w_init(&w, b, sizeof b);
    if (ava1_job_open_ack_encode(&a, &w) != 0) return AVA1_E_SPACE;
    return post ? ava1_server_post(sid, 0, AVA1_TYPE_JOB_OPEN_ACK, 0, 0, b, w.len)
                : ava1_server_send(sid, 0, AVA1_TYPE_JOB_OPEN_ACK, 0, 0, b, w.len);
}

static void refuse_open(const uint8_t sid[16], const uint8_t job[16], uint16_t status, const char *msg) {
    ava1_job_open_ack_t a;
    memset(&a, 0, sizeof a);
    memcpy(a.job_id, job, 16);
    a.status = status;
    (void)send_ack(sid, &a, msg, 1);
}

/* A network job's emitter (job threads, workers, the feeder, the open thread: never a
 * reader). The destination is read under cmu, so a re-attach switches it safely; the
 * function itself never changes. Credit it hands out is counted before it is sent (the
 * sender may use it at once); the map lets held lane frames go once it is sent. */
static void net_emit(ava1_job_t *j, uint8_t type, uint8_t flags, const uint8_t *body, size_t len) {
    uint8_t sid[16];
    int attached, map_ok = 0;
    if (type == AVA1_TYPE_JOB_MAP) {
        ava1_job_map_t m;
        map_ok = ava1_job_map_decode(body, len, &m) == 0 && m.status == AVA1_STATUS_OK && m.last;
        if (map_ok && ava1_data_test_map_delay_ms) ava1_platform_sleep_ms(ava1_data_test_map_delay_ms);
    }
    pthread_mutex_lock(&j->cmu);
    attached = j->attached;
    memcpy(sid, j->sid, 16);
    if (attached && type == AVA1_TYPE_CREDIT) {
        ava1_credit_t c;
        if (ava1_credit_decode(body, len, &c) == 0) j->w_avail += c.bytes;
    }
    pthread_mutex_unlock(&j->cmu);
    if (!attached || ava1_server_send(sid, 0, type, flags, 0, body, len) != 0 || !map_ok) return;
    pthread_mutex_lock(&j->cmu);
    if (j->attached && memcmp(j->sid, sid, 16) == 0) {
        j->ready = 1;
        pthread_cond_broadcast(&j->ccv);
    }
    pthread_mutex_unlock(&j->cmu);
}

static void emit_credit(ava1_job_t *j, uint64_t n) {
    ava1_credit_t c;
    uint8_t b[64];
    ava1_w_t w;
    memset(&c, 0, sizeof c);
    memcpy(c.job_id, j->id, 16);
    c.bytes = n;
    ava1_w_init(&w, b, sizeof b);
    if (ava1_credit_encode(&c, &w) == 0) ava1_job_emit(j, AVA1_TYPE_CREDIT, 0, b, w.len);
}

/* A failure found off the job thread: recorded like a worker's (a nonzero final_status
 * on an unfinished job) and ended by the job thread, so JobDone has one emitter and never
 * overtakes a Durable it is still sending. */
static void fail_soon(ava1_job_t *j, uint16_t status, const char *what) {
    pthread_mutex_lock(&j->mu);
    if (!j->finished && !j->final_status) {
        j->final_status = status;
        snprintf(j->message, sizeof j->message, "%s", what);
    }
    pthread_mutex_unlock(&j->mu);
}

/* ---- the feeder: a receiver job's frames, off the reader threads ------------------ */

static void free_frame(ava1_inframe_t *f) {
    free(f->body);
    free(f);
}

static void feed_control(ava1_job_t *j, ava1_inframe_t *f) {
    switch (f->type) {
    case AVA1_TYPE_MANIFEST_PAGE: {
        ava1_manifest_page_t p;
        int rc = ava1_manifest_page_decode(f->body, f->len, &p) == 0 ? ava1_recv_page(j, &p) : AVA1_E_PROTO;
        if (rc == AVA1_E_BADPATH) fail_soon(j, AVA1_ERR_PATH, "the manifest has a path that is not allowed");
        else if (rc == AVA1_E_IO) fail_soon(j, AVA1_ERR_INTERNAL, "out of memory for the manifest");
        else if (rc != 0) fail_soon(j, AVA1_ERR_PROTOCOL, "a manifest page does not follow the rules");
        break;
    }
    case AVA1_TYPE_MANIFEST_END: {
        ava1_manifest_end_t e;
        if (ava1_manifest_end_decode(f->body, f->len, &e) == 0) (void)ava1_recv_end(j, &e);
        break;
    }
    case AVA1_TYPE_RESUME: {
        ava1_resume_t r;
        if (ava1_resume_decode(f->body, f->len, &r) == 0) (void)ava1_recv_resume(j, r.manifest_hash);
        break;
    }
    case AVA1_TYPE_FILE_ROOT: {
        ava1_file_root_t r;
        int rc = ava1_file_root_decode(f->body, f->len, &r) == 0 ? ava1_apply_root(j, r.file_id, r.root) : AVA1_E_PROTO;
        if (rc == AVA1_E_PROTO) fail_soon(j, AVA1_ERR_PROTOCOL, "a FileRoot names no file");
        else if (rc != 0) fail_soon(j, AVA1_ERR_INTERNAL, "out of memory");
        break;
    }
    default:
        break;
    }
}

/* One held lane frame to the apply engine (SPEC.md §12.2: the cutoff decides how a file
 * travels; the engine checks the rest). Takes the frame. */
static void feed_lane(ava1_job_t *j, ava1_inframe_t *f) {
    const uint64_t cutoff = D.cfg.cutoff;
    const char *bad = NULL;
    int ok, rc = 0;
    pthread_mutex_lock(&j->mu);
    ok = j->prepared && !j->finished && !j->stopping;
    if (ok && f->type == AVA1_TYPE_CHUNK) {
        ava1_chunk_t c;
        if (ava1_chunk_decode(f->body, f->len, &c) != 0) bad = "a malformed Chunk";
        else if (c.file_id < j->m.n && j->m.e[c.file_id].size < cutoff) bad = "a Chunk for a file below the cutoff";
    } else if (ok) {
        ava1_bundle_t b;
        ava1_r_t it;
        ava1_bundle_record_t r;
        if (ava1_bundle_decode(f->body, f->len, &b) != 0) {
            bad = "a malformed Bundle";
        } else {
            ava1_r_init(&it, b.records, b.records_len);
            while (!bad && ava1_bundle_record_next(&it, &r) == 1)
                if (r.file_id < j->m.n && j->m.e[r.file_id].size >= cutoff) bad = "a BundleRecord for a file at or above the cutoff";
        }
    }
    pthread_mutex_unlock(&j->mu);
    if (!ok) { /* ended: the frame goes; a job being re-prepared gets its sender the bytes back */
        int fin;
        pthread_mutex_lock(&j->mu);
        fin = j->finished || j->stopping;
        pthread_mutex_unlock(&j->mu);
        if (!fin) emit_credit(j, f->len);
        free_frame(f);
        return;
    }
    if (bad) {
        free_frame(f);
        fail_soon(j, AVA1_ERR_PROTOCOL, bad);
        return;
    }
    if (ava1_apply_reserve(j, f->len) != 0) { /* the job ended meanwhile (its credit is 0) */
        free_frame(f);
        return;
    }
    if (f->type == AVA1_TYPE_CHUNK) {
        ava1_chunk_t c;
        (void)ava1_chunk_decode(f->body, f->len, &c);
        rc = ava1_apply_chunk(j, f->body, f->len, c.file_id, c.offset, c.data, c.data_len);
    } else {
        ava1_bundle_t b;
        (void)ava1_bundle_decode(f->body, f->len, &b);
        rc = ava1_apply_bundle(j, f->body, f->len, &b);
    }
    free(f); /* the body is the engine's now */
    if (rc == AVA1_E_PROTO) fail_soon(j, AVA1_ERR_PROTOCOL, "a data frame does not fit the manifest");
    else if (rc != 0) fail_soon(j, AVA1_ERR_INTERNAL, "out of memory");
}

/* Control frames in order; a FileRoot names a file of the map, so it (and what follows
 * it) waits for the map like lane frames do. Lane frames go once the map is out. */
static void *feed_main(void *arg) {
    ava1_job_t *j = arg;
    pthread_mutex_lock(&j->cmu);
    for (;;) {
        ava1_inframe_t *f = NULL, *held = NULL;
        int over;
        while (!j->feed_stop && !j->in_overflow && !(j->held_head && j->ready) &&
               !(j->in_head && (j->ready || j->in_head->type != AVA1_TYPE_FILE_ROOT)))
            pthread_cond_wait(&j->ccv, &j->cmu);
        if (j->feed_stop) break;
        over = j->in_overflow;
        j->in_overflow = 0;
        if (j->in_head && (j->ready || j->in_head->type != AVA1_TYPE_FILE_ROOT)) {
            f = j->in_head;
            j->in_head = f->next;
            if (!j->in_head) j->in_tail = NULL;
            j->in_bytes -= f->len;
        } else if (j->ready) {
            held = j->held_head;
            j->held_head = j->held_tail = NULL;
        }
        pthread_mutex_unlock(&j->cmu);
        if (over) fail_soon(j, AVA1_ERR_PROTOCOL, "too many control messages are waiting");
        if (f) {
            feed_control(j, f);
            free_frame(f);
        }
        while (held) {
            ava1_inframe_t *n = held->next;
            feed_lane(j, held);
            held = n;
        }
        pthread_mutex_lock(&j->cmu);
    }
    pthread_mutex_unlock(&j->cmu);
    return NULL;
}

static void inbox_add(ava1_job_t *j, uint8_t type, const uint8_t *body, size_t len) {
    ava1_inframe_t *f = calloc(1, sizeof *f);
    uint8_t *b = malloc(len ? len : 1);
    if (!f || !b) {
        free(f);
        free(b);
        f = NULL;
    } else {
        memcpy(b, body, len);
        f->type = type;
        f->len = len;
        f->body = b;
    }
    pthread_mutex_lock(&j->cmu);
    if (!f || j->in_bytes + len > IN_MAX) {
        j->in_overflow = 1;
    } else {
        if (j->in_tail) j->in_tail->next = f;
        else j->in_head = f;
        j->in_tail = f;
        j->in_bytes += len;
        f = NULL;
    }
    pthread_cond_broadcast(&j->ccv);
    pthread_mutex_unlock(&j->cmu);
    if (f) free_frame(f);
}

int ava1_job_attach(ava1_job_t *j, const uint8_t sid[16]) {
    uint16_t lanes[AVA1_MAX_LANES];
    int nl = ava1_server_lanes(sid, lanes), start = 0;
    ava1_inframe_t *drop, *f;
    if (ava1_job_attach_sid(j, sid) != 0) return -1;
    pthread_mutex_lock(&j->cmu);
    j->emit = net_emit;
    /* Frames held for an earlier session name what its map said: they go, and the new
     * session's frames wait for its own map. Their bytes are the sender's again. */
    j->ready = 0;
    drop = j->held_head;
    j->held_head = j->held_tail = NULL;
    for (f = drop; f; f = f->next) j->w_avail += f->len;
    if (!j->on_frame && !j->feeder_started) start = j->feeder_started = 1;
    pthread_mutex_unlock(&j->cmu);
    __atomic_store_n(&j->lanes, (uint8_t)nl, __ATOMIC_RELAXED);
    while (drop) {
        f = drop->next;
        free_frame(drop);
        drop = f;
    }
    if (start && ava1_thread_start(feed_main, j, &j->feeder) != 0) {
        pthread_mutex_lock(&j->cmu);
        j->feeder_started = 0;
        pthread_mutex_unlock(&j->cmu);
        return -1;
    }
    return 0;
}

/* The credit a JobOpenAck grants: the sender may have this many lane bytes outstanding. */
static void grant(ava1_job_t *j, uint64_t credit) {
    pthread_mutex_lock(&j->cmu);
    j->w_avail = credit;
    if (credit > j->w_grant) j->w_grant = credit;
    pthread_mutex_unlock(&j->cmu);
}

/* ---- JobOpen ----------------------------------------------------------------------- */

static pthread_mutex_t g_open_mu = PTHREAD_MUTEX_INITIALIZER; /* one ava1_recv_open at a time */

typedef struct {
    const uint8_t *id;
    const char *root;
    ava1_job_t *hit[AVA1_MAX_JOBS];
    int n;
} root_q_t;

/* Under the table lock (ava1_job_foreach), so taking a reference is refs++. A job's root
 * is written once, by ava1_recv_open, which runs under g_open_mu like this check. */
static void root_each(ava1_job_t *j, void *ctx) {
    root_q_t *q = ctx;
    if (memcmp(j->id, q->id, 16) != 0 && j->kind == AVA1_JOB_UPLOAD && strcmp(j->root, q->root) == 0) {
        j->refs++;
        q->hit[q->n++] = j;
    }
}

/* Another job that has not ended writes to `root` (Task 13 review M6): two jobs must not
 * stage, lock or rename the same destination. Parked jobs count: they can resume. */
static int root_in_use(const uint8_t id[16], const char *root) {
    root_q_t q;
    int i, busy = 0;
    memset(&q, 0, sizeof q);
    q.id = id;
    q.root = root;
    ava1_job_foreach(root_each, &q);
    for (i = 0; i < q.n; i++) {
        pthread_mutex_lock(&q.hit[i]->mu);
        busy |= !q.hit[i]->finished && !q.hit[i]->stopping;
        pthread_mutex_unlock(&q.hit[i]->mu);
        ava1_job_put(q.hit[i]);
    }
    return busy;
}

/* Runs on the open thread: the work behind a JobOpen (stat, mkdir, journal replay, thread
 * starts) never runs on a reader. */
static void open_now(const uint8_t sid[16], const uint8_t peer[32], const uint8_t *body, size_t len) {
    ava1_job_open_t o;
    ava1_job_open_ack_t ack;
    char root[AVA1_MAX_PATH + 1], msg[160] = "";
    ava1_job_t *j = NULL;
    if (ava1_data_test_open_delay_ms) ava1_platform_sleep_ms(ava1_data_test_open_delay_ms);
    if (ava1_job_open_decode(body, len, &o) != 0) return; /* checked by the reader */
    memset(&ack, 0, sizeof ack);
    memcpy(ack.job_id, o.job_id, 16);
    if (!D.running) {
        ack.status = AVA1_ERR_BUSY;
        snprintf(msg, sizeof msg, "the console is stopping");
    } else if (o.root_len > AVA1_MAX_PATH || memchr(o.root, 0, o.root_len)) {
        ack.status = AVA1_ERR_PATH;
        snprintf(msg, sizeof msg, "the destination is not a valid path");
    } else if (o.kind != AVA1_JOB_UPLOAD) { /* Task 18 adds JOB_DOWNLOAD */
        ack.status = AVA1_ERR_PROTOCOL;
        snprintf(msg, sizeof msg, "unknown job kind");
    } else {
        ava1_recv_spec_t s;
        int tries;
        memcpy(root, o.root, o.root_len);
        root[o.root_len] = 0;
        memset(&s, 0, sizeof s);
        memcpy(s.id, o.job_id, 16);
        memcpy(s.owner, peer, 32);
        s.kind = o.kind;
        s.policy = o.policy;
        s.flags = o.flags;
        s.root = root;
        s.emit = net_emit;
        s.sid = sid;
        pthread_mutex_lock(&g_open_mu);
        if (root_in_use(o.job_id, root)) {
            ack.status = AVA1_ERR_BUSY;
            snprintf(msg, sizeof msg, "another transfer is writing to this destination");
        } else {
            /* Attach right after the open; a job the reaper unlisted in between (it was
             * parked long enough) is opened again, from its journal. */
            for (tries = 0; tries < 2 && !j; tries++) {
                j = ava1_recv_open(&s, &ack, msg, sizeof msg);
                if (j && ava1_job_attach(j, sid) != 0) {
                    ava1_job_put(j);
                    j = NULL;
                    ack.status = AVA1_ERR_BUSY;
                    snprintf(msg, sizeof msg, "the job could not be attached");
                } else if (!j) {
                    break;
                }
            }
            if (j) grant(j, ack.credit);
        }
        pthread_mutex_unlock(&g_open_mu);
    }
    if (send_ack(sid, &ack, j ? "" : msg, 0) == AVA1_E_CLOSED && j)
        ava1_job_park_session(sid); /* the session ended while we opened: on_session_end missed it */
    if (j) ava1_job_put(j);
}

/* A JobOpen in progress and the control frames for its job that arrived meanwhile: they
 * are applied after it, in order, by its thread. */
typedef struct {
    int used;
    uint8_t id[16], sid[16], peer[32];
    ava1_inframe_t *head, *tail;
    size_t bytes;
} opening_t;

static struct {
    pthread_mutex_t mu;
    opening_t o[AVA1_MAX_JOBS];
} O = { .mu = PTHREAD_MUTEX_INITIALIZER };

static int route(const uint8_t sid[16], const uint8_t peer[32], uint8_t type, const uint8_t *body, size_t len);

static void *open_main(void *arg) {
    opening_t *o = arg;
    uint8_t sid[16], peer[32];
    memcpy(sid, o->sid, 16);
    memcpy(peer, o->peer, 32);
    for (;;) {
        ava1_inframe_t *f;
        pthread_mutex_lock(&O.mu);
        f = o->head;
        if (f) {
            o->head = f->next;
            if (!o->head) o->tail = NULL;
            o->bytes -= f->len;
        } else {
            o->used = 0; /* from here on, frames for this job go to it directly */
        }
        pthread_mutex_unlock(&O.mu);
        if (!f) break;
        if (f->type == AVA1_TYPE_JOB_OPEN) open_now(sid, peer, f->body, f->len);
        else (void)route(sid, peer, f->type, f->body, f->len);
        free_frame(f);
    }
    return NULL;
}

/* Caller holds O.mu. */
static int open_append(opening_t *o, uint8_t type, const uint8_t *body, size_t len) {
    ava1_inframe_t *f;
    if (o->bytes + len > OPEN_MAX || !(f = calloc(1, sizeof *f))) return -1;
    if (!(f->body = malloc(len ? len : 1))) {
        free(f);
        return -1;
    }
    memcpy(f->body, body, len);
    f->type = type;
    f->len = len;
    if (o->tail) o->tail->next = f;
    else o->head = f;
    o->tail = f;
    o->bytes += len;
    return 0;
}

/* ---- control frames ---------------------------------------------------------------- */

static void *cancel_main(void *arg) {
    ava1_job_t *j = arg;
    ava1_recv_cancel(j); /* stops it (the journal stays); may join its threads */
    ava1_job_put(j);
    return NULL;
}

/* A control frame for an existing job (any thread; it never waits on the job). */
static int route(const uint8_t sid[16], const uint8_t peer[32], uint8_t type, const uint8_t *body, size_t len) {
    ava1_job_t *j = ava1_job_find(body);
    int rc = 0;
    if (!j) {
        if (type == AVA1_TYPE_RESUME) post_unknown_map(sid, body); /* SPEC.md §11.5: open it */
        return 0;                                                  /* a late frame for a job that is gone */
    }
    if (memcmp(j->owner, peer, 32) != 0) { /* never another device's job */
        if (type == AVA1_TYPE_RESUME) post_unknown_map(sid, body);
        ava1_job_put_nowait(j);
        return 0;
    }
    if (j->on_frame) { /* a sender job (Task 18): the receiving peer's acks and map */
        if (type == AVA1_TYPE_RESUME) (void)ava1_job_attach(j, sid);
        rc = j->on_frame(j, type, body, len);
        ava1_job_put_nowait(j);
        return rc;
    }
    switch (type) {
    case AVA1_TYPE_RESUME:
        if (ava1_job_attach(j, sid) != 0) post_unknown_map(sid, body);
        else inbox_add(j, type, body, len);
        break;
    case AVA1_TYPE_JOB_CANCEL:
        if (ava1_data_spawn(cancel_main, j) == 0) return 0; /* the thread has our reference */
        break;
    case AVA1_TYPE_MANIFEST_PAGE:
    case AVA1_TYPE_MANIFEST_END:
    case AVA1_TYPE_FILE_ROOT:
        inbox_add(j, type, body, len);
        break;
    default:
        break; /* nothing else is for a receiver */
    }
    ava1_job_put_nowait(j);
    return 0;
}

/* A malformed frame of the job conversation closes the session (as any malformed frame). */
static int well_formed(uint8_t type, const uint8_t *body, size_t len) {
    union {
        ava1_job_open_t o;
        ava1_manifest_page_t p;
        ava1_manifest_end_t e;
        ava1_resume_t r;
        ava1_file_root_t f;
        ava1_job_cancel_t c;
    } u;
    switch (type) {
    case AVA1_TYPE_JOB_OPEN: return ava1_job_open_decode(body, len, &u.o) == 0;
    case AVA1_TYPE_MANIFEST_PAGE: return ava1_manifest_page_decode(body, len, &u.p) == 0;
    case AVA1_TYPE_MANIFEST_END: return ava1_manifest_end_decode(body, len, &u.e) == 0;
    case AVA1_TYPE_RESUME: return ava1_resume_decode(body, len, &u.r) == 0;
    case AVA1_TYPE_FILE_ROOT: return ava1_file_root_decode(body, len, &u.f) == 0;
    case AVA1_TYPE_JOB_CANCEL: return ava1_job_cancel_decode(body, len, &u.c) == 0;
    default: return 1;
    }
}

/* Reader thread: decode, route, queue. Nothing here waits for a job or a disk. */
static int data_on_control(const uint8_t sid[16], const uint8_t peer[32], uint8_t type, uint8_t flags,
                           const uint8_t *body, size_t len) {
    int i, slot = -1, rc = 0, queued = 0;
    (void)flags;
    if (!D.running) return 0;
    if (len < 16 || !well_formed(type, body, len)) return 1;
    pthread_mutex_lock(&O.mu);
    for (i = 0; i < AVA1_MAX_JOBS; i++) {
        opening_t *o = &O.o[i];
        if (o->used && memcmp(o->id, body, 16) == 0 && memcmp(o->sid, sid, 16) == 0) {
            rc = open_append(o, type, body, len) != 0; /* a sender that floods an opening job */
            queued = 1;
            break;
        }
        if (!o->used && slot < 0) slot = i;
    }
    if (!queued && type == AVA1_TYPE_JOB_OPEN) {
        if (slot < 0) {
            pthread_mutex_unlock(&O.mu);
            refuse_open(sid, body, AVA1_ERR_BUSY, "too many jobs are opening");
            return 0;
        }
        memset(&O.o[slot], 0, sizeof O.o[slot]);
        memcpy(O.o[slot].id, body, 16);
        memcpy(O.o[slot].sid, sid, 16);
        memcpy(O.o[slot].peer, peer, 32);
        if (open_append(&O.o[slot], type, body, len) != 0) {
            pthread_mutex_unlock(&O.mu);
            refuse_open(sid, body, AVA1_ERR_INTERNAL, "out of memory");
            return 0;
        }
        O.o[slot].used = 1;
        pthread_mutex_unlock(&O.mu);
        if (ava1_data_spawn(open_main, &O.o[slot]) != 0) {
            pthread_mutex_lock(&O.mu);
            while (O.o[slot].head) {
                ava1_inframe_t *f = O.o[slot].head;
                O.o[slot].head = f->next;
                free_frame(f);
            }
            O.o[slot].used = 0;
            pthread_mutex_unlock(&O.mu);
            refuse_open(sid, body, AVA1_ERR_INTERNAL, "cannot start a thread");
        }
        return 0;
    }
    pthread_mutex_unlock(&O.mu);
    if (queued) return rc;
    return route(sid, peer, type, body, len);
}

/* ---- lanes ------------------------------------------------------------------------- */

typedef struct {
    const uint8_t *sid;
    uint64_t bound;
    int any;
} bound_t;

static void bound_each(ava1_job_t *j, void *ctx) {
    bound_t *b = ctx;
    uint64_t g;
    if (!j->attached || memcmp(j->sid, b->sid, 16) != 0) return;
    pthread_mutex_lock(&j->cmu);
    g = j->w_grant;
    pthread_mutex_unlock(&j->cmu);
    b->any = 1;
    if (g > b->bound) b->bound = g;
}

/* Before the body is read. The header names no job (the id is inside the sealed body), so
 * the bound here is the largest credit any job of this session was granted; the exact
 * per-job check follows in on_lane, before anything is done with the frame. The global
 * budget bounds what all readers hold at once. */
static int data_admit(const uint8_t sid[16], uint16_t lane, size_t len) {
    bound_t b;
    int ok;
    if (!D.running) return 1;
    memset(&b, 0, sizeof b);
    b.sid = sid;
    ava1_job_foreach(bound_each, &b);
    if (b.any && len > b.bound) {
        post_error(sid, lane, AVA1_ERR_CREDIT, "a frame larger than the credit granted");
        return 1;
    }
    pthread_mutex_lock(&D.mu);
    ok = D.admitted + len <= D.cfg.budget;
    if (ok) D.admitted += len;
    pthread_mutex_unlock(&D.mu);
    if (!ok) post_error(sid, lane, AVA1_ERR_CREDIT, "too much data in flight");
    return ok ? 0 : 1;
}

/* A lane data frame, in memory. Credit is charged to its job, Received goes out at once
 * (SPEC.md §12.3), and the frame waits in the job until its feeder takes it. */
static int data_on_lane(const uint8_t sid[16], uint16_t lane, uint8_t type, uint32_t seq, uint8_t *body,
                        size_t len) {
    ava1_job_t *j;
    ava1_inframe_t *f;
    int take, over = 0;
    pthread_mutex_lock(&D.mu);
    D.admitted -= len;
    pthread_mutex_unlock(&D.mu);
    if (!body) return 0;
    if (len < 16 || (type != AVA1_TYPE_CHUNK && type != AVA1_TYPE_BUNDLE) || !(j = ava1_job_find(body))) {
        free(body);
        return 0; /* a frame for a job that is gone: its credit died with it */
    }
    f = calloc(1, sizeof *f);
    pthread_mutex_lock(&j->cmu);
    take = f && j->attached && memcmp(j->sid, sid, 16) == 0 && !j->on_frame;
    if (take && len > j->w_avail) {
        over = 1;
        take = 0;
    }
    if (take) j->w_avail -= len;
    pthread_mutex_unlock(&j->cmu);
    if (!take) {
        free(f);
        free(body);
        ava1_job_put_nowait(j);
        if (!over) return 0;
        post_error(sid, lane, AVA1_ERR_CREDIT, "the frame exceeds the credit granted");
        return 1; /* SPEC.md §12.4: the lane closes; the session and the job stay */
    }
    post_received(sid, j->id, lane, seq); /* before any disk work */
    f->type = type;
    f->lane = lane;
    f->seq = seq;
    f->len = len;
    f->body = body;
    pthread_mutex_lock(&j->cmu);
    if (j->held_tail) j->held_tail->next = f;
    else j->held_head = f;
    j->held_tail = f;
    if (j->ready) pthread_cond_broadcast(&j->ccv);
    pthread_mutex_unlock(&j->cmu);
    ava1_job_put_nowait(j);
    return 0;
}

typedef struct {
    const uint8_t *sid;
    uint16_t lane;
    int up;
} lane_ev_t;

/* Under the table lock: counters and the sender's hook only (it marks state, Task 18). */
static void lane_each(ava1_job_t *j, void *ctx) {
    lane_ev_t *e = ctx;
    if (!j->attached || memcmp(j->sid, e->sid, 16) != 0) return;
    if (e->up) __atomic_add_fetch(&j->lanes, 1, __ATOMIC_RELAXED);
    else if (__atomic_load_n(&j->lanes, __ATOMIC_RELAXED)) __atomic_sub_fetch(&j->lanes, 1, __ATOMIC_RELAXED);
    if (j->on_lane_change) j->on_lane_change(j, e->lane, e->up);
}

static void data_on_lane_change(const uint8_t sid[16], uint16_t lane, int up) {
    lane_ev_t e = { sid, lane, up };
    ava1_job_foreach(lane_each, &e);
}

static void data_on_session_end(const uint8_t sid[16]) { ava1_job_park_session(sid); }

static const ava1_data_hooks_t HOOKS = {
    data_on_control, data_admit, data_on_lane, data_on_lane_change, data_on_session_end,
};

const ava1_data_hooks_t *ava1_data_hooks(void) { return &HOOKS; }
