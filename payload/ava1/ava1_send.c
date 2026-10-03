#include "ava1_send.h"

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#include "ava1_apply.h"
#include "ava1_b3.h"
#include "ava1_data.h"
#include "ava1_server.h"
#include "ava1_thread.h"

#define READ_AHEAD (32u << 20)
#define REFUND_RING 256u

/* ---- the reader --------------------------------------------------------------------- */

static int src_path(const ava1_reader_t *r, uint32_t id, char *out, size_t cap) {
    if (r->single) return snprintf(out, cap, "%s", r->src_root) >= (int)cap ? -1 : 0;
    return snprintf(out, cap, "%s/%s", r->src_root, ava1_mstore_path(&r->j->m, id)) >= (int)cap ? -1 : 0;
}

static int pread_all(int fd, uint8_t *p, size_t n, uint64_t off) {
    while (n) {
        ssize_t k = pread(fd, p, n, (off_t)off);
        if (k < 0 && errno == EINTR) continue;
        if (k <= 0) return k == 0 ? -EIO : -errno;
        p += k;
        n -= (size_t)k;
        off += (uint64_t)k;
    }
    return 0;
}

/* Flushes the bundle being built: encodes Bundle{job, records} and hands it over. */
static int flush_bundle(ava1_reader_t *r, ava1_w_t *recs) {
    ava1_bundle_t b;
    ava1_w_t w;
    size_t cap = recs->len + 64;
    uint8_t *msg;
    if (recs->len == 0) return 0;
    msg = malloc(cap);
    if (!msg) return -ENOMEM;
    memset(&b, 0, sizeof b);
    memcpy(b.job_id, r->j->id, 16);
    b.records = recs->buf;
    b.records_len = (uint32_t)recs->len;
    ava1_w_init(&w, msg, cap);
    if (ava1_bundle_encode(&b, &w) != 0) {
        free(msg);
        return -EIO;
    }
    recs->len = 0;
    return r->put(r->ctx, AVA1_TYPE_BUNDLE, msg, w.len);
}

static int send_chunk(ava1_reader_t *r, uint32_t id, uint64_t off, const uint8_t *data, size_t len) {
    ava1_chunk_t c;
    ava1_w_t w;
    size_t cap = len + 64;
    uint8_t *msg = malloc(cap);
    if (!msg) return -ENOMEM;
    memset(&c, 0, sizeof c);
    memcpy(c.job_id, r->j->id, 16);
    c.file_id = id;
    c.offset = off;
    c.data = data;
    c.data_len = (uint32_t)len;
    ava1_w_init(&w, msg, cap);
    if (ava1_chunk_encode(&c, &w) != 0) {
        free(msg);
        return -EIO;
    }
    return r->put(r->ctx, AVA1_TYPE_CHUNK, msg, w.len);
}

/* Reads a large file in runs of same "send" state (a durable range is not re-read for
 * sending, but every group's CV is still computed so the root can be produced even for
 * durable-only runs), at most one chunk long each. */
static int read_large(ava1_reader_t *r, uint32_t id, int fd, uint8_t *buf) {
    uint64_t size = r->j->m.e[id].size, n = (size + AVA1_GROUP_LEN - 1) / AVA1_GROUP_LEN, g = 0;
    const ava1_rset_t *dur = r->durable ? r->durable[id] : NULL;
    uint8_t (*cvs)[32] = n >= 2 ? malloc((size_t)n * 32u) : NULL;
    uint8_t root[32];
    int rc = 0;
    if (n >= 2 && !cvs) return -ENOMEM;
    while (g < n && rc == 0 && !*r->stop) {
        /* A run of groups with the same "send" state, at most one chunk long. */
        uint64_t off = g * AVA1_GROUP_LEN, len = 0, k;
        int send = !(dur && ava1_rset_covers(dur, off, off + (size - off < AVA1_GROUP_LEN ? size - off : AVA1_GROUP_LEN)));
        while (g < n && len < r->chunk) {
            uint64_t gs = g * AVA1_GROUP_LEN, gl = size - gs < AVA1_GROUP_LEN ? size - gs : AVA1_GROUP_LEN;
            int s2 = !(dur && ava1_rset_covers(dur, gs, gs + gl));
            if (s2 != send) break;
            len += gl;
            g++;
        }
        if ((rc = pread_all(fd, buf, (size_t)len, off)) != 0) break;
        for (k = 0; n >= 2 && k * AVA1_GROUP_LEN < len; k++) {
            uint64_t gl = len - k * AVA1_GROUP_LEN < AVA1_GROUP_LEN ? len - k * AVA1_GROUP_LEN : AVA1_GROUP_LEN;
            ava1_b3_group_cv(buf + k * AVA1_GROUP_LEN, (size_t)gl, off / AVA1_GROUP_LEN + k, cvs[off / AVA1_GROUP_LEN + k]);
        }
        if (n < 2) ava1_b3_hash(buf, (size_t)len, root);
        if (send) rc = send_chunk(r, id, off, buf, (size_t)len);
    }
    if (rc == 0 && !*r->stop) {
        if (n >= 2) ava1_b3_root_from_cvs((const uint8_t (*)[32])cvs, n, root);
        else if (size == 0) ava1_b3_hash(buf, 0, root);
        r->root(r->ctx, id, root);
    }
    free(cvs);
    return rc;
}

int ava1_read_files(ava1_reader_t *r, const uint32_t *ids, uint32_t count) {
    uint32_t i, total = ids ? count : r->j->m.n;
    size_t bcap = (size_t)r->bundle + AVA1_MAX_PATH + 128;
    uint8_t *buf = malloc(r->chunk > r->cutoff ? r->chunk : r->cutoff), *bundle = malloc(bcap);
    ava1_w_t recs;
    char path[AVA1_MAX_PATH + 600];
    int rc = 0;
    if (!buf || !bundle) {
        free(buf);
        free(bundle);
        return -ENOMEM;
    }
    ava1_w_init(&recs, bundle, bcap);
    for (i = 0; i < total && rc == 0 && !*r->stop; i++) {
        uint32_t id = ids ? ids[i] : i;
        const ava1_ment_t *e = &r->j->m.e[id];
        int fd;
        if (e->kind != AVA1_ENTRY_FILE || (r->skip && ava1_bits_get(r->skip, id))) continue;
        if (src_path(r, id, path, sizeof path) != 0) {
            rc = -ENAMETOOLONG;
            break;
        }
        fd = open(path, O_RDONLY);
        if (fd < 0) {
            rc = -errno;
            break;
        }
        if (e->size < r->cutoff) {
            ava1_bundle_record_t br;
            if ((rc = pread_all(fd, buf, (size_t)e->size, 0)) == 0) {
                memset(&br, 0, sizeof br);
                br.file_id = id;
                ava1_b3_hash(buf, (size_t)e->size, br.root);
                br.data = buf;
                br.data_len = (uint32_t)e->size;
                if (recs.len + e->size + 64 > r->bundle) rc = flush_bundle(r, &recs);
                if (rc == 0) rc = ava1_bundle_record_append(&recs, &br);
            }
        } else {
            rc = flush_bundle(r, &recs); /* keep file order for ordered receivers */
            if (rc == 0) rc = read_large(r, id, fd, buf);
        }
        close(fd);
    }
    if (rc == 0 && !*r->stop) rc = flush_bundle(r, &recs);
    free(buf);
    free(bundle);
    return rc;
}

/* ---- the network sender ------------------------------------------------------------ */

typedef struct sframe {
    struct sframe *next;
    uint8_t *msg;
    size_t len;
    uint8_t type;
    uint32_t seq;
    uint16_t lane;
} sframe_t;

typedef struct {
    uint64_t credit, queued;
    sframe_t *ready, *ready_tail, *inflight;
    uint32_t next_seq, next_page;
    uint32_t refund_seq[REFUND_RING];
    uint64_t refund_len[REFUND_RING];
    uint32_t refund_at;
    int pages_done, have_map, reading, stop, unsafe_read, single;
    ava1_bits_t skip;
    ava1_rset_t **durable;
    uint32_t *retry;
    uint32_t retry_n;
    pthread_t reader;
    int reader_started;
    pthread_t writer[AVA1_MAX_LANES + 1];
    int writer_up[AVA1_MAX_LANES + 1];      /* 0 down, 1 running, 2 start requested */
    int writer_started[AVA1_MAX_LANES + 1]; /* a joinable thread exists */
} snd_t;

static snd_t *S_(ava1_job_t *j) { return (snd_t *)j->role; }

static int put_frame(void *ctx, uint8_t type, uint8_t *msg, size_t len) {
    ava1_job_t *j = ctx;
    snd_t *s = S_(j);
    sframe_t *f = calloc(1, sizeof *f);
    if (!f) {
        free(msg);
        return -ENOMEM;
    }
    f->msg = msg;
    f->len = len;
    f->type = type;
    pthread_mutex_lock(&j->mu);
    while (s->queued >= READ_AHEAD && !s->stop && !j->stopping) pthread_cond_wait(&j->cv, &j->mu);
    if (s->ready_tail) s->ready_tail->next = f;
    else s->ready = f;
    s->ready_tail = f;
    s->queued += len;
    pthread_cond_broadcast(&j->cv);
    pthread_mutex_unlock(&j->mu);
    return s->stop ? -ECANCELED : 0;
}

static void put_root(void *ctx, uint32_t id, const uint8_t root[32]) {
    ava1_job_t *j = ctx;
    ava1_file_root_t r;
    uint8_t b[96];
    ava1_w_t w;
    memset(&r, 0, sizeof r);
    memcpy(r.job_id, j->id, 16);
    r.file_id = id;
    memcpy(r.root, root, 32);
    ava1_w_init(&w, b, sizeof b);
    if (ava1_file_root_encode(&r, &w) == 0) ava1_job_emit(j, AVA1_TYPE_FILE_ROOT, 0, b, w.len);
}

static void *reader_main(void *arg) {
    ava1_job_t *j = arg;
    snd_t *s = S_(j);
    const ava1_data_cfg_t *cfg = ava1_data_cfg();
    ava1_reader_t r;
    int rc;
    memset(&r, 0, sizeof r);
    r.j = j;
    r.src_root = j->src;
    r.single = s->single;
    r.cutoff = cfg->cutoff ? cfg->cutoff : (256u << 10); /* JobOpen carries no cutoff (C6) */
    r.chunk = 4u << 20;
    r.bundle = 1u << 20;
    r.skip = &s->skip;
    r.durable = s->durable;
    r.put = put_frame;
    r.root = put_root;
    r.ctx = j;
    r.stop = &s->stop;
    rc = ava1_read_files(&r, NULL, 0);
    /* FileRetry: the engine asked for whole files again. */
    while (rc == 0 && !s->stop && !j->stopping) {
        uint32_t *ids = NULL, n = 0;
        pthread_mutex_lock(&j->mu);
        if (s->retry_n) {
            ids = s->retry;
            n = s->retry_n;
            s->retry = NULL;
            s->retry_n = 0;
        } else {
            pthread_cond_wait(&j->cv, &j->mu);
        }
        pthread_mutex_unlock(&j->mu);
        if (ids) {
            r.skip = NULL;    /* a retry is a full re-read of the named files (C8) */
            r.durable = NULL;
            rc = ava1_read_files(&r, ids, n);
            free(ids);
        }
    }
    if (rc != 0 && rc != -ECANCELED) ava1_apply_fail(j, AVA1_ERR_IO, "reading the source failed", -rc, 0);
    return NULL;
}

typedef struct {
    ava1_job_t *j;
    uint16_t lane;
} wctx2_t;

static void *writer_main(void *arg) {
    wctx2_t *c = arg;
    ava1_job_t *j = c->j;
    uint16_t lane = c->lane;
    snd_t *s = S_(j);
    uint8_t sid[16];
    free(c);
    pthread_mutex_lock(&j->mu);
    memcpy(sid, j->sid, 16);
    for (;;) {
        sframe_t *f;
        int rc;
        while (!s->stop && !j->stopping && s->writer_up[lane] &&
               (!s->ready || s->ready->len > s->credit))
            pthread_cond_wait(&j->cv, &j->mu);
        if (s->stop || j->stopping || !s->writer_up[lane]) break;
        f = s->ready;
        s->ready = f->next;
        if (!s->ready) s->ready_tail = NULL;
        s->queued -= f->len;
        s->credit -= f->len;
        f->seq = ++s->next_seq;
        f->lane = lane;
        f->next = s->inflight;
        s->inflight = f;
        pthread_cond_broadcast(&j->cv); /* the reader may continue */
        pthread_mutex_unlock(&j->mu);
        /* The copying send keeps f->msg as plaintext, so a requeued frame can go out again
         * on another lane under that lane's keys. */
        rc = ava1_server_send(sid, lane, f->type, 0, f->seq, f->msg, f->len);
        pthread_mutex_lock(&j->mu);
        if (rc != 0) break; /* the lane is gone: on_lane_change requeues its frames */
    }
    pthread_mutex_unlock(&j->mu);
    return NULL;
}

/* Moves a lane's unreceived frames back to the front of the ready list (caller holds mu). */
static void requeue_lane(snd_t *s, uint16_t lane) {
    sframe_t **pp = &s->inflight;
    while (*pp) {
        sframe_t *f = *pp;
        if (f->lane != lane) {
            pp = &f->next;
            continue;
        }
        *pp = f->next;
        s->credit += f->len;
        s->refund_seq[s->refund_at % REFUND_RING] = f->seq;
        s->refund_len[s->refund_at % REFUND_RING] = f->len;
        s->refund_at++;
        f->next = s->ready;
        s->ready = f;
        if (!s->ready_tail) s->ready_tail = f;
        s->queued += f->len;
    }
}

static void lane_change(ava1_job_t *j, uint16_t lane, int up) {
    snd_t *s = S_(j);
    if (lane == 0 || lane > AVA1_MAX_LANES) return;
    /* Called under the job table lock: only mark state and signal; never join here. */
    pthread_mutex_lock(&j->mu);
    if (!up) {
        s->writer_up[lane] = 0;
        requeue_lane(s, lane);
    } else if (!s->writer_up[lane]) {
        s->writer_up[lane] = 2; /* "start me": the job thread starts the writer (on_tick) */
    }
    pthread_cond_broadcast(&j->cv);
    pthread_mutex_unlock(&j->mu);
}

static void emit_page_or_end(ava1_job_t *j) {
    snd_t *s = S_(j);
    uint8_t *out = malloc(64u * 1024u);
    size_t len;
    if (!out) return;
    while (s->next_page < j->m.n) {
        if (ava1_mstore_page(&j->m, j->id, &s->next_page, out, 64u * 1024u, &len) == 0)
            ava1_job_emit(j, AVA1_TYPE_MANIFEST_PAGE, 0, out, len);
        else break; /* transient (no memory): retried next tick, not a tight spin */
    }
    if (s->next_page >= j->m.n) { /* m.n == 0: no page, just the end */
        ava1_manifest_end_t e;
        ava1_w_t w;
        memset(&e, 0, sizeof e);
        memcpy(e.job_id, j->id, 16);
        e.files = j->m.files;
        e.bytes = j->m.bytes;
        ava1_mstore_hash(&j->m, e.manifest_hash);
        ava1_w_init(&w, out, 64u * 1024u);
        if (ava1_manifest_end_encode(&e, &w) == 0) ava1_job_emit(j, AVA1_TYPE_MANIFEST_END, 0, out, w.len);
        s->pages_done = 1;
    }
    free(out);
}

static void on_tick(ava1_job_t *j) {
    snd_t *s = S_(j);
    uint16_t l;
    int attached;
    /* C2: nothing may be emitted before the data layer attaches the job — net_emit would
     * drop the pages while next_page advanced, and the manifest would be lost. */
    pthread_mutex_lock(&j->cmu);
    attached = j->attached;
    pthread_mutex_unlock(&j->cmu);
    if (!attached) return;
    if (!s->pages_done) emit_page_or_end(j);
    pthread_mutex_lock(&j->mu);
    if (s->have_map && !s->reader_started) {
        s->reader_started = ava1_thread_start(reader_main, j, &s->reader) == 0;
    }
    for (l = 1; l <= AVA1_MAX_LANES; l++) {
        if (s->writer_up[l] == 2) {
            wctx2_t *c = malloc(sizeof *c);
            if (s->writer_started[l]) { /* the previous writer of this lane id has exited or is exiting */
                pthread_mutex_unlock(&j->mu);
                pthread_join(s->writer[l], NULL);
                pthread_mutex_lock(&j->mu);
                s->writer_started[l] = 0;
            }
            s->writer_up[l] = 1;
            if (c) {
                c->j = j;
                c->lane = l;
                if (ava1_thread_start(writer_main, c, &s->writer[l]) == 0) s->writer_started[l] = 1;
                else free(c);
            }
        }
    }
    pthread_mutex_unlock(&j->mu);
}

static int on_frame(ava1_job_t *j, uint8_t type, const uint8_t *body, size_t len) {
    snd_t *s = S_(j);
    switch (type) {
    case AVA1_TYPE_JOB_MAP: {
        ava1_job_map_t m;
        ava1_r_t it;
        ava1_file_run_t r;
        ava1_file_range_t g;
        if (ava1_job_map_decode(body, len, &m) != 0) return 1;
        if (m.status != AVA1_STATUS_OK) {
            /* A reader-thread hook never ends a job itself (C4): the job thread does. */
            ava1_data_fail_soon(j, m.status, "the receiver refused the job");
            return 0;
        }
        pthread_mutex_lock(&j->mu);
        ava1_r_init(&it, m.done, m.done_len);
        while (ava1_file_run_next(&it, &r) == 1) {
            uint32_t f;
            for (f = r.first; f < r.first + r.count && f < j->m.n; f++) ava1_bits_set(&s->skip, f);
        }
        ava1_r_init(&it, m.partial, m.partial_len);
        while (ava1_file_range_next(&it, &g) == 1) {
            if (g.file_id >= j->m.n) continue;
            if (!s->durable[g.file_id]) s->durable[g.file_id] = calloc(1, sizeof(ava1_rset_t));
            if (s->durable[g.file_id]) (void)ava1_rset_add(s->durable[g.file_id], g.offset, g.offset + g.len);
        }
        if (m.last) s->have_map = 1;
        pthread_mutex_unlock(&j->mu);
        return 0;
    }
    case AVA1_TYPE_RECEIVED: {
        ava1_received_t r;
        sframe_t **pp, *f = NULL;
        uint32_t k;
        if (ava1_received_decode(body, len, &r) != 0) return 1;
        pthread_mutex_lock(&j->mu);
        for (pp = &s->inflight; *pp; pp = &(*pp)->next)
            if ((*pp)->seq == r.seq) {
                f = *pp;
                *pp = f->next;
                break;
            }
        if (!f) /* a frame we refunded after its lane died had arrived after all */
            for (k = 0; k < REFUND_RING; k++)
                if (s->refund_seq[k] == r.seq && s->refund_len[k]) {
                    s->credit = s->credit > s->refund_len[k] ? s->credit - s->refund_len[k] : 0;
                    s->refund_len[k] = 0;
                }
        if (f) j->bytes_received += f->len;
        pthread_mutex_unlock(&j->mu);
        if (f) {
            free(f->msg);
            free(f);
        }
        return 0;
    }
    case AVA1_TYPE_CREDIT: {
        ava1_credit_t c;
        if (ava1_credit_decode(body, len, &c) != 0) return 1;
        pthread_mutex_lock(&j->mu);
        s->credit += c.bytes; /* Credit frames are incremental (ruling R2) */
        pthread_cond_broadcast(&j->cv);
        pthread_mutex_unlock(&j->mu);
        return 0;
    }
    case AVA1_TYPE_FILE_RETRY: {
        ava1_file_retry_t r;
        uint32_t *a;
        if (ava1_file_retry_decode(body, len, &r) != 0 || r.file_id >= j->m.n) return 1;
        pthread_mutex_lock(&j->mu);
        a = realloc(s->retry, (s->retry_n + 1) * sizeof *a);
        if (a) {
            s->retry = a;
            s->retry[s->retry_n++] = r.file_id;
        }
        pthread_cond_broadcast(&j->cv);
        pthread_mutex_unlock(&j->mu);
        return 0;
    }
    case AVA1_TYPE_JOB_DONE:
    case AVA1_TYPE_JOB_CANCEL:
        /* The peer concluded the job: stop, never answer with a JobDone of our own (C10). */
        pthread_mutex_lock(&j->mu);
        s->stop = 1;
        j->finished = 1;
        pthread_cond_broadcast(&j->cv);
        pthread_mutex_unlock(&j->mu);
        return 0;
    default:
        return 0; /* Durable, Status: progress the engine shows; nothing to do here */
    }
}

static void role_free(ava1_job_t *j) {
    snd_t *s = S_(j);
    sframe_t *f;
    uint32_t i;
    if (!s) return;
    pthread_mutex_lock(&j->mu);
    s->stop = 1;
    pthread_cond_broadcast(&j->cv);
    pthread_mutex_unlock(&j->mu);
    if (s->reader_started) pthread_join(s->reader, NULL);
    for (i = 1; i <= AVA1_MAX_LANES; i++)
        if (s->writer_started[i]) pthread_join(s->writer[i], NULL); /* each exits on stop */
    while ((f = s->ready) != NULL) {
        s->ready = f->next;
        free(f->msg);
        free(f);
    }
    while ((f = s->inflight) != NULL) {
        s->inflight = f->next;
        free(f->msg);
        free(f);
    }
    for (i = 0; s->durable && i < j->m.n; i++)
        if (s->durable[i]) {
            ava1_rset_clear(s->durable[i]);
            free(s->durable[i]);
        }
    free(s->durable);
    free(s->retry);
    ava1_bits_free(&s->skip);
    free(s);
    j->role = NULL;
}

ava1_job_t *ava1_send_open(const ava1_job_open_t *o, const uint8_t peer[32], ava1_job_open_ack_t *ack, char *msg,
                           size_t cap) {
    const ava1_data_cfg_t *cfg = ava1_data_cfg();
    ava1_job_t *j;
    snd_t *s;
    struct stat st;
    char root[AVA1_MAX_PATH + 1];
    int rc;
    memset(ack, 0, sizeof *ack);
    memcpy(ack->job_id, o->job_id, 16);
    msg[0] = 0;
    memcpy(root, o->root, o->root_len);
    root[o->root_len] = 0;
    if (!cfg->may_read || !cfg->may_read(root, (o->flags & AVA1_JF_UNSAFE_READ) != 0)) {
        ack->status = AVA1_ERR_PATH;
        snprintf(msg, cap, "reading %s is not allowed", root);
        return NULL;
    }
    if (stat(root, &st) != 0) {
        ack->status = AVA1_ERR_PATH;
        snprintf(msg, cap, "%s: %s", root, strerror(errno));
        return NULL;
    }
    ava1_job_free_one(o->job_id); /* a sender's state lives only in memory: start over */
    j = ava1_job_create(o->job_id, peer);
    s = j ? calloc(1, sizeof *s) : NULL;
    if (!j || !s) {
        if (j) {
            ava1_job_free_one(o->job_id);
            ava1_job_put(j);
        }
        free(s);
        ack->status = AVA1_ERR_BUSY;
        return NULL;
    }
    j->kind = AVA1_JOB_DOWNLOAD;
    j->flags = o->flags;
    snprintf(j->src, sizeof j->src, "%s", root);
    j->role = s;
    j->role_free = role_free;
    s->single = !S_ISDIR(st.st_mode);
    rc = s->single ? ava1_mstore_single(&j->m, root) : ava1_mstore_walk_ex(&j->m, root, AVA1_WALK_FOLLOW);
    s->credit = o->has_credit ? o->credit : (16u << 20);
    if (rc != 0 || ava1_bits_init(&s->skip, j->m.n) != 0 ||
        !(s->durable = calloc((size_t)j->m.n + 1, sizeof *s->durable)) ||
        !(j->lf = calloc((size_t)j->m.n + 1, sizeof *j->lf)) || ava1_bits_init(&j->done, j->m.n) != 0) {
        ack->status = rc == AVA1_E_BADPATH ? AVA1_ERR_PATH : AVA1_ERR_IO;
        snprintf(msg, cap, "cannot read %s", root);
        ava1_job_free_one(o->job_id);
        ava1_job_put(j);
        return NULL;
    }
    j->on_frame = on_frame;
    j->on_lane_change = lane_change;
    j->on_tick = on_tick;
    if (ava1_apply_start(j) != 0) {
        ack->status = AVA1_ERR_INTERNAL;
        ava1_job_free_one(o->job_id);
        ava1_job_put(j);
        return NULL;
    }
    ack->status = AVA1_STATUS_OK;
    return j;
}

void ava1_send_start(ava1_job_t *j) {
    snd_t *s = S_(j);
    uint16_t lanes[AVA1_MAX_LANES];
    int n, i;
    /* Lanes already up when the job was opened never produced a lane-change event for it. */
    n = ava1_server_lanes(j->sid, lanes);
    pthread_mutex_lock(&j->mu);
    for (i = 0; i < n; i++)
        if (!s->writer_up[lanes[i]]) s->writer_up[lanes[i]] = 2;
    pthread_mutex_unlock(&j->mu);
}
