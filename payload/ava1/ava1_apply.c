#include "ava1_apply.h"
#include "ava1_events.h"

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

#include "ava1_b3.h"
#include "ava1_data.h"
#include "ava1_frame.h" /* AVA1_FLAG_IGNORABLE */
#include "ava1_internal.h"
#include "ava1_platform.h"
#include "ava1_thread.h"

#define CREDIT_FLUSH (4u << 20)
#define BATCH_BYTES (64ull << 20)
#define BATCH_MS 250u
#define TICK_MS 25u
#define MAX_ITEMS 2000u
#define PATH_CAP AVA1_PATH_CAP

typedef struct {
    ava1_job_t *j;
    uint32_t idx;
} wctx_t;

void (*ava1_apply_hook)(ava1_job_t *j, int point, uint32_t id);
int (*ava1_apply_fault)(ava1_job_t *j, int point, uint32_t id);
int ava1_apply_hold_batches;
#define HOOK(j, point, id) \
    do { \
        if (ava1_apply_hook) ava1_apply_hook((j), (point), (id)); \
    } while (0)

static int is_stopping(ava1_job_t *j) {
    int s;
    pthread_mutex_lock(&j->mu);
    s = j->stopping;
    pthread_mutex_unlock(&j->mu);
    return s;
}

int ava1_sync_dir(const char *dir) {
    int fd = open(dir, O_RDONLY | O_DIRECTORY), rc = 0;
    if (fd < 0) return errno;
    rc = ava1_fsync_retry(fd, NULL, NULL, NULL);
    if (rc == EINVAL || rc == ENOTSUP || rc == EOPNOTSUPP) rc = 0; /* a filesystem that cannot sync a directory */
    close(fd);
    return rc;
}

/* A final rename that failed because something is already where it goes. */
static int in_the_way(int e) { return e == EEXIST || e == ENOTEMPTY || e == ENOTDIR || e == EISDIR; }

#define groups_of ava1_groups_of
#define sync_dir ava1_sync_dir
#define parent_of ava1_parent_of
#define mkparents ava1_mkparents

/* A path that does not fit comes back empty (open() then fails) rather than truncated
 * (which would name some other file). */
void ava1_apply_path(const ava1_job_t *j, uint32_t id, int part, char *out, size_t cap) {
    const char *rel = ava1_mstore_path(&j->m, id);
    int n;
    if (j->flags & AVA1_JF_SINGLE_FILE) n = snprintf(out, cap, "%s%s", j->root, part ? ".ava-part" : "");
    else if (rel) n = snprintf(out, cap, "%s/%s%s", j->base, rel, part && !j->staged ? ".ava-part" : "");
    else n = -1;
    if (cap && (n < 0 || (size_t)n >= cap)) out[0] = 0;
}

static void ob_path(const ava1_job_t *j, uint32_t id, char *out, size_t cap) {
    snprintf(out, cap, "%s/%u.ob", j->dir, id);
}

void ava1_parent_of(const char *p, char *out, size_t cap) {
    const char *s = strrchr(p, '/');
    if (!s) snprintf(out, cap, ".");
    else if (s == p) snprintf(out, cap, "/");
    else snprintf(out, cap, "%.*s", (int)(s - p), p);
}

/* mkdir -p up to the last '/' (and of the whole path when `self`). */
static int mkdirs_to(const char *path, int self, int sync) {
    char p[PATH_CAP], parent[PATH_CAP];
    size_t i, n;
    if (!path[0] || (n = strlen(path)) >= sizeof p) return -ENAMETOOLONG;
    snprintf(p, sizeof p, "%s", path);
    for (i = 1; i <= n; i++) {
        int rc;
        if (i < n ? p[i] != '/' : !self) continue;
        p[i] = 0;
        if (mkdir(p, 0755) == 0) {
            if (sync) {
                parent_of(p, parent, sizeof parent);
                if ((rc = sync_dir(parent)) != 0) return -rc;
            }
        } else if (errno != EEXIST) {
            return -errno;
        }
        if (i < n) p[i] = '/';
    }
    return 0;
}

int ava1_mkparents(const char *path) { return mkdirs_to(path, 0, 0); }
int ava1_mkdirs(const char *path, int sync) { return mkdirs_to(path, 1, sync); }

/* ---- the large-file index (see ava1_job.h: lfl) ----------------------------------- */

static int lfl_push(ava1_job_t *j, uint32_t id) {
    if (j->lfl_all) return 0; /* every scan walks the whole manifest anyway */
    if (j->lfl_n == j->lfl_cap) {
        uint32_t c = j->lfl_cap ? j->lfl_cap * 2 : 64;
        uint32_t *q = realloc(j->lfl, (size_t)c * sizeof *q);
        if (!q) return -1;
        j->lfl = q;
        j->lfl_cap = c;
    }
    j->lfl[j->lfl_n++] = id;
    return 0;
}

ava1_lfile_t *ava1_lfile_get(ava1_job_t *j, uint32_t id) {
    int fresh = 0;
    if (!j->lf[id]) {
        j->lf[id] = calloc(1, sizeof(ava1_lfile_t));
        if (!j->lf[id]) return NULL;
        j->lf[id]->fd = j->lf[id]->ob_fd = -1;
        fresh = 1;
    }
    if (!j->lf[id]->in_list) {
        if (lfl_push(j, id) != 0) {
            /* Out of memory: the index can no longer be trusted, so the scans take every
             * manifest entry from now on (slower, never wrong). */
            j->lfl_all = 1;
            (void)fresh;
        }
        j->lf[id]->in_list = 1;
    }
    return j->lf[id];
}

void ava1_lflist_rebuild(ava1_job_t *j) {
    uint32_t i;
    j->lfl_n = 0;
    j->lfl_all = 0;
    for (i = 0; j->lf && i < j->m.n; i++) {
        if (!j->lf[i]) continue;
        j->lf[i]->in_list = 1;
        if (lfl_push(j, i) != 0) {
            j->lfl_all = 1;
            return;
        }
    }
}

void ava1_lflist_reset(ava1_job_t *j, int release) {
    j->lfl_n = 0;
    j->lfl_all = 0;
    if (release) {
        free(j->lfl);
        j->lfl = NULL;
        j->lfl_cap = 0;
    }
}

static int u32cmp(const void *a, const void *b);

/* A lf with nothing left for a batch, a commit or a snapshot to do. */
static int lf_idle(const ava1_job_t *j, uint32_t id, const ava1_lfile_t *lf) {
    return (lf->committed || ava1_bits_get(&j->done, id)) && !lf->written.n && !(lf->has_root && !lf->root_journaled);
}

/* The ids to scan, ascending and unique, as a malloc'd array (caller holds j->mu). `*n` is
 * 0 with NULL when there is nothing to do, or UINT32_MAX with NULL when out of memory. */
static uint32_t *lfl_snapshot(ava1_job_t *j, uint32_t *n) {
    uint32_t *v, k, c = 0;
    *n = 0;
    if (j->lfl_all) {
        if (!j->m.n) return NULL;
        v = malloc((size_t)j->m.n * sizeof *v);
        if (!v) {
            *n = UINT32_MAX;
            return NULL;
        }
        for (k = 0; k < j->m.n; k++)
            if (j->lf[k]) v[c++] = k;
    } else {
        if (!j->lfl_n) return NULL;
        v = malloc((size_t)j->lfl_n * sizeof *v);
        if (!v) {
            *n = UINT32_MAX;
            return NULL;
        }
        memcpy(v, j->lfl, (size_t)j->lfl_n * sizeof *v);
        qsort(v, j->lfl_n, sizeof *v, u32cmp);
        for (k = 0; k < j->lfl_n; k++) {
            if (v[k] >= j->m.n || !j->lf[v[k]]) continue; /* stale: its lf was freed */
            if (c && v[c - 1] == v[k]) continue;
            v[c++] = v[k];
        }
    }
    if (!c) {
        free(v);
        return NULL;
    }
    *n = c;
    return v;
}

/* After a batch: the index keeps only the files that still have work (ids in `v`, sorted). */
static void lfl_prune(ava1_job_t *j, const uint32_t *v, uint32_t n) {
    uint32_t k;
    if (j->lfl_all) return;
    j->lfl_n = 0;
    for (k = 0; k < n; k++) {
        ava1_lfile_t *lf = j->lf[v[k]];
        if (lf_idle(j, v[k], lf)) lf->in_list = 0;
        else j->lfl[j->lfl_n++] = v[k]; /* never more than it held */
    }
}

static uint64_t mono_us(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (uint64_t)t.tv_sec * 1000000u + (uint64_t)t.tv_nsec / 1000u;
}

static int write_all(int fd, const uint8_t *p, size_t n) {
    while (n) {
        ssize_t k = write(fd, p, n);
        if (k < 0) {
            if (errno == EINTR) continue;
            return -errno;
        }
        p += k;
        n -= (size_t)k;
    }
    return 0;
}

static int pwrite_all(int fd, const uint8_t *p, size_t n, uint64_t off) {
    while (n) {
        ssize_t k = pwrite(fd, p, n, (off_t)off);
        if (k < 0) {
            if (errno == EINTR) continue;
            return -errno;
        }
        p += k;
        n -= (size_t)k;
        off += (uint64_t)k;
    }
    return 0;
}

/* ---- messages -------------------------------------------------------------------- */

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

static void emit_retry(ava1_job_t *j, uint32_t id, uint16_t reason) {
    ava1_file_retry_t r;
    uint8_t b[64];
    ava1_w_t w;
    memset(&r, 0, sizeof r);
    memcpy(r.job_id, j->id, 16);
    r.file_id = id;
    r.reason = reason;
    ava1_w_init(&w, b, sizeof b);
    if (ava1_file_retry_encode(&r, &w) == 0) ava1_job_emit(j, AVA1_TYPE_FILE_RETRY, 0, b, w.len);
}

static void emit_done(ava1_job_t *j) {
    ava1_job_done_t d;
    uint8_t b[256];
    ava1_w_t w;
    memset(&d, 0, sizeof d);
    memcpy(d.job_id, j->id, 16);
    d.status = j->final_status;
    d.files = j->files_done;
    d.bytes = j->bytes_durable;
    if (j->message[0]) {
        d.has_message = 1;
        d.message = (const uint8_t *)j->message;
        d.message_len = (uint16_t)strlen(j->message);
    }
    ava1_w_init(&w, b, sizeof b);
    if (ava1_job_done_encode(&d, &w) == 0) ava1_job_emit(j, AVA1_TYPE_JOB_DONE, 0, b, w.len);
}

/* Durable{files, ranges}, split so each message stays under a control frame. */
static void emit_durable(ava1_job_t *j, const ava1_file_run_t *runs, uint32_t nr, const ava1_file_range_t *rg,
                         uint32_t ng) {
    uint8_t *fb = malloc(48u * MAX_ITEMS), *rb = malloc(48u * MAX_ITEMS), *out = malloc(64u * 1024u);
    uint32_t i = 0, k = 0;
    if (!fb || !rb || !out) goto done;
    while (i < nr || k < ng || (nr == 0 && ng == 0 && i == 0 && k == 0)) {
        ava1_w_t fw, rw, w;
        ava1_durable_t d;
        uint32_t items = 0;
        ava1_w_init(&fw, fb, 48u * MAX_ITEMS);
        ava1_w_init(&rw, rb, 48u * MAX_ITEMS);
        for (; i < nr && items < MAX_ITEMS; i++, items++) (void)ava1_file_run_append(&fw, &runs[i]);
        for (; k < ng && items < MAX_ITEMS; k++, items++) (void)ava1_file_range_append(&rw, &rg[k]);
        memset(&d, 0, sizeof d);
        memcpy(d.job_id, j->id, 16);
        d.files = fb;
        d.files_len = (uint32_t)fw.len;
        d.ranges = rb;
        d.ranges_len = (uint32_t)rw.len;
        ava1_w_init(&w, out, 64u * 1024u);
        if (ava1_durable_encode(&d, &w) == 0) ava1_job_emit(j, AVA1_TYPE_DURABLE, 0, out, w.len);
        if (nr == 0 && ng == 0) break;
    }
done:
    free(fb);
    free(rb);
    free(out);
}

void ava1_apply_status(ava1_job_t *j) {
    ava1_status_t st;
    uint8_t b[128];
    ava1_w_t w;
    uint8_t bn;
    const ava1_data_cfg_t *cfg = ava1_data_cfg();
    memset(&st, 0, sizeof st);
    memcpy(st.job_id, j->id, 16);
    pthread_mutex_lock(&j->mu);
    if (j->ticks && j->q_busy_ticks * 2 >= j->ticks)
        bn = (j->want_workers >= cfg->workers_max || j->tune.hold) ? AVA1_BN_DISK : AVA1_BN_WORKERS;
    else bn = AVA1_BN_NETWORK;
    j->ticks = j->q_busy_ticks = 0;
    st.files_done = j->files_done;
    st.files_total = j->m.files;
    st.bytes_received = j->bytes_received;
    st.bytes_durable = j->bytes_durable;
    st.bytes_total = j->m.bytes;
    st.bottleneck = bn;
    st.workers = j->want_workers;
    st.lanes = j->lanes;
    pthread_mutex_unlock(&j->mu);
    ava1_w_init(&w, b, sizeof b);
    if (ava1_status_encode(&st, &w) == 0) ava1_job_emit(j, AVA1_TYPE_STATUS, AVA1_FLAG_IGNORABLE, b, w.len);
}

/* Every end of a job goes through here, success included (finish() passes AVA1_STATUS_OK):
 * one place sets `finished`, journals Done and sends JobDone, exactly once. */
void ava1_apply_fail(ava1_job_t *j, uint16_t status, const char *what, int err, int journal_done) {
    int first;
    int done_written = 0;
    uint64_t credit = 0;
    pthread_mutex_lock(&j->mu);
    first = !j->finished;
    if (first) {
        j->finished = 1;
        j->final_status = status;
        snprintf(j->message, sizeof j->message, "%s%s%s", what, err ? ": " : "", err ? strerror(err) : "");
        /* An ended job takes no more frames (reserve now fails): its credit goes back to
         * the data budget at once, not when the job is finally freed. */
        credit = j->credit;
        j->credit = 0;
    }
    pthread_mutex_unlock(&j->mu);
    if (!first) return;
    ava1_log_job_event(status == AVA1_STATUS_OK ? "done" : "fail", j, status);
    if (credit) ava1_budget_give(credit);
    if (journal_done) {
        ava1_jnl_done_t d;
        uint8_t b[16];
        ava1_w_t w;
        d.status = status;
        ava1_w_init(&w, b, sizeof b);
        if (ava1_jnl_done_encode(&d, &w) == 0 && ava1_jnl_append(&j->jnl, AVA1_JNL_DONE, b, w.len) == 0)
            done_written = 1;
    }
    pthread_mutex_lock(&j->mu);
    /* A journaled Done(OK) that a restart replayed is itself the proof: the rename, the
     * directory sync and the Done append all succeeded before the crash. */
    j->durable_ok = status == AVA1_STATUS_OK && err == 0 && (done_written || j->replay_done);
    if (status == AVA1_STATUS_OK && journal_done && !done_written) {
        j->final_status = AVA1_ERR_IO;
        snprintf(j->message, sizeof j->message, "journal append failed");
    }
    pthread_mutex_unlock(&j->mu);
    emit_done(j);
}

void ava1_apply_done_again(ava1_job_t *j) {
    if (j->finished) emit_done(j);
}

/* Workers never end the job themselves: they record the first failure here (a nonzero
 * final_status on an unfinished job) and the job thread ends it, so JobDone has one
 * emitter and can never overtake a Durable the job thread is still sending. */
static void worker_fail(ava1_job_t *j, uint16_t status, const char *what, int err) {
    pthread_mutex_lock(&j->mu);
    if (!j->finished && !j->final_status) {
        j->final_status = status;
        snprintf(j->message, sizeof j->message, "%s%s%s", what, err ? ": " : "", err ? strerror(err) : "");
    }
    pthread_mutex_unlock(&j->mu);
}

/* ---- credit ------------------------------------------------------------------------ */

int ava1_apply_reserve(ava1_job_t *j, size_t n) {
    int ok;
    pthread_mutex_lock(&j->mu);
    ok = j->outstanding + n <= j->credit;
    if (ok) j->outstanding += n;
    pthread_mutex_unlock(&j->mu);
    return ok ? 0 : -1;
}

void ava1_apply_unreserve(ava1_job_t *j, size_t n) {
    pthread_mutex_lock(&j->mu);
    j->outstanding -= n;
    pthread_mutex_unlock(&j->mu);
}

static void give_back(ava1_job_t *j, size_t n) {
    uint64_t flush = 0;
    pthread_mutex_lock(&j->mu);
    j->outstanding -= n;
    j->credit_back += n;
    if (j->credit_back >= CREDIT_FLUSH) {
        flush = j->credit_back;
        j->credit_back = 0;
    }
    pthread_mutex_unlock(&j->mu);
    if (flush) emit_credit(j, flush);
}

/* ---- the queue --------------------------------------------------------------------- */

static int enqueue(ava1_job_t *j, ava1_work_t *w, int front) {
    pthread_mutex_lock(&j->mu);
    if (front) {
        w->next = j->q_head;
        j->q_head = w;
        if (!j->q_tail) j->q_tail = w;
    } else {
        w->next = NULL;
        if (j->q_tail) j->q_tail->next = w;
        else j->q_head = w;
        j->q_tail = w;
    }
    j->q_len++;
    pthread_cond_broadcast(&j->cv);
    pthread_mutex_unlock(&j->mu);
    return 0;
}

static void release_owned(uint8_t *owned, size_t cap) {
    if (cap) (void)ava1_frame_free(owned, cap);
    else free(owned);
}

int ava1_apply_chunk(ava1_job_t *j, uint8_t *owned, size_t owned_len, uint32_t file_id, uint64_t off,
                     const uint8_t *data, size_t len) {
    return ava1_apply_chunk_pooled(j, owned, owned_len, 0, file_id, off, data, len);
}

int ava1_apply_bundle(ava1_job_t *j, uint8_t *owned, size_t owned_len, const ava1_bundle_t *b) {
    return ava1_apply_bundle_pooled(j, owned, owned_len, 0, b);
}

int ava1_apply_chunk_pooled(ava1_job_t *j, uint8_t *owned, size_t owned_len, size_t owned_cap, uint32_t file_id,
                            uint64_t off, const uint8_t *data, size_t len) {
    ava1_work_t *w;
    uint64_t size;
    /* Inside the file (written so a hostile offset cannot wrap), group-aligned, and a whole
     * number of groups unless it ends the file: write_chunk hashes whole groups out of `data`. */
    if (file_id >= j->m.n || j->m.e[file_id].kind != AVA1_ENTRY_FILE || off % AVA1_GROUP_LEN != 0 ||
        off > (size = j->m.e[file_id].size) || len > size - off ||
        (len % AVA1_GROUP_LEN != 0 && off + len != size)) {
        release_owned(owned, owned_cap);
        give_back(j, owned_len);
        return AVA1_E_PROTO;
    }
    w = calloc(1, sizeof *w);
    if (!w) {
        release_owned(owned, owned_cap);
        give_back(j, owned_len);
        return AVA1_E_IO;
    }
    w->kind = AVA1_W_CHUNK;
    w->owned = owned;
    w->owned_cap = owned_cap;
    w->owned_len = owned_len;
    w->file_id = file_id;
    w->offset = off;
    w->data = data;
    w->len = len;
    return enqueue(j, w, 0);
}

int ava1_apply_bundle_pooled(ava1_job_t *j, uint8_t *owned, size_t owned_len, size_t owned_cap,
                            const ava1_bundle_t *b) {
    ava1_work_t *w;
    ava1_r_t it;
    ava1_bundle_record_t r;
    uint32_t n = 0;
    int k;
    /* The whole bundle up front: every record well formed and naming a file, and as many
     * as it claims. A bad one is refused here, never half-applied by a worker. */
    ava1_r_init(&it, b->records, b->records_len);
    while ((k = ava1_bundle_record_next(&it, &r)) == 1) {
        if (r.file_id >= j->m.n || j->m.e[r.file_id].kind != AVA1_ENTRY_FILE) {
            k = -1;
            break;
        }
        n++;
    }
    if (k != 0 || n != b->records_count) {
        release_owned(owned, owned_cap);
        give_back(j, owned_len);
        return AVA1_E_PROTO;
    }
    w = calloc(1, sizeof *w);
    if (!w) {
        release_owned(owned, owned_cap);
        give_back(j, owned_len);
        return AVA1_E_IO;
    }
    w->kind = AVA1_W_BUNDLE;
    w->owned = owned;
    w->owned_cap = owned_cap;
    w->owned_len = owned_len;
    w->data = b->records;
    w->len = b->records_len;
    return enqueue(j, w, 0);
}

int ava1_apply_root(ava1_job_t *j, uint32_t file_id, const uint8_t root[32]) {
    int rc = 0;
    pthread_mutex_lock(&j->mu);
    if (file_id >= j->m.n || j->m.e[file_id].kind != AVA1_ENTRY_FILE) {
        rc = AVA1_E_PROTO;
    } else {
        if (!ava1_lfile_get(j, file_id)) rc = AVA1_E_IO;
        else if (!j->lf[file_id]->has_root || memcmp(j->lf[file_id]->root, root, 32) != 0) {
            memcpy(j->lf[file_id]->root, root, 32);
            j->lf[file_id]->has_root = 1;
            j->lf[file_id]->root_journaled = 0;
            j->roots_new++;
        }
    }
    pthread_mutex_unlock(&j->mu);
    return rc;
}

int ava1_apply_parallel(ava1_job_t *j, void (*fn)(ava1_job_t *, void *, uint32_t), void *arg, uint32_t n) {
    uint32_t i;
    int dropped = 0;
    pthread_mutex_lock(&j->mu);
    j->calls_left += n;
    pthread_mutex_unlock(&j->mu);
    for (i = 0; i < n; i++) {
        ava1_work_t *w = calloc(1, sizeof *w);
        if (!w) { /* run it here rather than lose it */
            fn(j, arg, i);
            pthread_mutex_lock(&j->mu);
            j->calls_left--;
            pthread_mutex_unlock(&j->mu);
            continue;
        }
        w->kind = AVA1_W_CALL;
        w->fn = fn;
        w->arg = arg;
        w->i = i;
        enqueue(j, w, 1);
    }
    pthread_mutex_lock(&j->mu);
    while (j->calls_left) {
        if (j->stopping) {
            /* Workers no longer take work: drop our items they never started, then wait
             * only for the ones running (they still read `arg`, which the caller frees). */
            ava1_work_t **pp = &j->q_head, *prev = NULL;
            while (*pp) {
                ava1_work_t *w = *pp;
                if (w->kind == AVA1_W_CALL && w->fn == fn && w->arg == arg) {
                    *pp = w->next;
                    if (j->q_tail == w) j->q_tail = prev;
                    j->q_len--;
                    j->calls_left--;
                    dropped = 1;
                    free(w);
                } else {
                    prev = w;
                    pp = &w->next;
                }
            }
            if (!j->calls_left) break;
        }
        pthread_cond_wait(&j->cv, &j->mu);
    }
    pthread_mutex_unlock(&j->mu);
    return dropped ? -1 : 0;
}

/* ---- applying ---------------------------------------------------------------------- */

/* The large file's state with open descriptors; caller holds j->mu. NULL + *err on failure. */
/* create == 0 (the commit's reopen): a missing part file or outboard is ENOENT, never a new
 * empty file — an empty part would hash to nothing and be renamed over the real file. */
static ava1_lfile_t *lfile_open(ava1_job_t *j, uint32_t id, int *err, int create) {
    ava1_lfile_t *lf = ava1_lfile_get(j, id);
    const ava1_ment_t *e = &j->m.e[id];
    char path[PATH_CAP];
    struct stat st;
    *err = 0;
    if (!lf) {
        *err = ENOMEM;
        return NULL;
    }
    if (lf->fd >= 0) return lf;
    ava1_apply_path(j, id, 1, path, sizeof path);
    if (!path[0]) {
        *err = ENAMETOOLONG;
        return NULL;
    }
    lf->fd = open(path, O_RDWR | (create ? O_CREAT : 0) | O_NOFOLLOW, 0600);
    if (lf->fd < 0 && create && errno == ENOENT && mkparents(path) == 0)
        lf->fd = open(path, O_RDWR | O_CREAT | O_NOFOLLOW, 0600);
    if (lf->fd < 0) {
        *err = errno;
        return NULL;
    }
    if (fstat(lf->fd, &st) == 0 && st.st_size == 0 && e->size > 0) {
        int rc = ava1_platform_preallocate(lf->fd, e->size);
        if (rc == ENOSPC) {
            *err = ENOSPC;
            goto fail;
        }
    }
    if (groups_of(e->size) >= 2) {
        ob_path(j, id, path, sizeof path);
        lf->ob_fd = open(path, O_RDWR | (create ? O_CREAT : 0), 0600);
        if (lf->ob_fd < 0) {
            *err = errno;
            goto fail;
        }
        if (fstat(lf->ob_fd, &st) == 0 && (uint64_t)st.st_size < groups_of(e->size) * 32u)
            (void)ftruncate(lf->ob_fd, (off_t)(groups_of(e->size) * 32u));
    }
    return lf;
fail: /* never leave a data fd open without its outboard: CVs would go unwritten */
    close(lf->fd);
    lf->fd = -1;
    return NULL;
}

static int write_chunk(ava1_job_t *j, uint32_t id, uint64_t off, const uint8_t *d, size_t len) {
    ava1_lfile_t *lf;
    int err, fd, ob;
    uint64_t size = j->m.e[id].size, g;
    pthread_mutex_lock(&j->mu);
    if (ava1_bits_get(&j->done, id) || (j->lf[id] && j->lf[id]->committed)) {
        pthread_mutex_unlock(&j->mu);
        return 0; /* a late duplicate */
    }
    lf = lfile_open(j, id, &err, 1);
    /* Our own descriptors: the job thread may close lf's at commit while we write, and a
     * reused descriptor number would then take these bytes into some other file. */
    fd = lf ? dup(lf->fd) : -1;
    ob = lf && lf->ob_fd >= 0 ? dup(lf->ob_fd) : -1;
    if (lf && (fd < 0 || (lf->ob_fd >= 0 && ob < 0))) err = errno;
    pthread_mutex_unlock(&j->mu);
    if (!lf || err) {
        if (fd >= 0) close(fd);
        if (ob >= 0) close(ob);
        return -err;
    }
    err = pwrite_all(fd, d, len, off);
    if (err == 0 && ob >= 0) {
        for (g = off / AVA1_GROUP_LEN; g * AVA1_GROUP_LEN < off + len; g++) {
            uint64_t gs = g * AVA1_GROUP_LEN, glen = size - gs < AVA1_GROUP_LEN ? size - gs : AVA1_GROUP_LEN;
            uint8_t cv[32];
            ava1_b3_group_cv(d + (gs - off), (size_t)glen, g, cv);
            if ((err = pwrite_all(ob, cv, 32, g * 32u)) != 0) break;
        }
    }
    close(fd);
    if (ob >= 0) close(ob);
    if (err) return err;
    pthread_mutex_lock(&j->mu);
    if (!lf->committed) (void)ava1_rset_add(&lf->written, off, off + len); /* else: a late duplicate */
    j->unsynced_bytes += len;
    j->bytes_received += len;
    j->applied_since_tune++;
    pthread_mutex_unlock(&j->mu);
    return 0;
}

/* The open-file budget (ava1_data.h): a slot is taken before a small file is opened and given
 * back once its fd is closed. While the share is used up, a worker runs queued sync stripes
 * (the batch that frees slots needs workers) or waits; the job thread syncs early (pend_full). */
static int pend_gate_stopping(void *a) {
    ava1_job_t *j = a;
    int st;
    pthread_mutex_lock(&j->mu);
    st = j->stopping;
    pthread_mutex_unlock(&j->mu);
    return st;
}

static void pend_gate_idle(void *a) {
    ava1_job_t *j = a;
    ava1_work_t *w;
    pthread_mutex_lock(&j->mu);
    w = j->q_head;
    if (w && w->kind == AVA1_W_CALL) {
        j->q_head = w->next;
        if (!j->q_head) j->q_tail = NULL;
        j->q_len--;
        pthread_mutex_unlock(&j->mu);
        w->fn(j, w->arg, w->i);
        pthread_mutex_lock(&j->mu);
        if (--j->calls_left == 0) pthread_cond_broadcast(&j->cv);
        free(w);
        pthread_mutex_unlock(&j->mu);
    } else {
        pthread_mutex_unlock(&j->mu);
        ava1_platform_sleep_ms(2);
    }
}

/* Small files wait here (holding their fd) until a sync batch covers them. */
static void pend_add(ava1_job_t *j, uint32_t id, int fd, const uint8_t root[32]) {
    pthread_mutex_lock(&j->mu);
    while (j->pend_n >= AVA1_PEND_MAX && !j->stopping) {
        /* Run sync stripes ourselves: if every worker waited here, nobody would. */
        ava1_work_t *w = j->q_head;
        if (w && w->kind == AVA1_W_CALL) {
            j->q_head = w->next;
            if (!j->q_head) j->q_tail = NULL;
            j->q_len--;
            pthread_mutex_unlock(&j->mu);
            w->fn(j, w->arg, w->i);
            pthread_mutex_lock(&j->mu);
            if (--j->calls_left == 0) pthread_cond_broadcast(&j->cv);
            free(w);
        } else {
            pthread_cond_wait(&j->cv, &j->mu);
        }
    }
    if (j->pend_n == j->pend_cap) {
        uint32_t c = j->pend_cap ? j->pend_cap * 2 : 256;
        uint32_t *a = realloc(j->pend_small, c * sizeof *a);
        int *b = a ? realloc(j->pend_fd, c * sizeof *b) : NULL;
        uint8_t(*r)[32] = b ? realloc(j->pend_root, (size_t)c * sizeof *r) : NULL;
        if (a) j->pend_small = a;
        if (b) j->pend_fd = b;
        if (r) j->pend_root = r;
        if (a && b && r) j->pend_cap = c;
    }
    if (j->pend_n < j->pend_cap) {
        j->pend_small[j->pend_n] = id;
        memcpy(j->pend_root[j->pend_n], root, 32);
        j->pend_fd[j->pend_n++] = fd;
    } else {
        close(fd); /* out of memory: the file will be in no batch and is sent again on resume */
        ava1_pend_release(1);
    }
    pthread_mutex_unlock(&j->mu);
}

/* 0, -errno, or BAD_RECORD: a record naming no file (the rest of its bundle is dropped).
 * Positive so it can never collide with a -errno. */
#define BAD_RECORD 1

static int apply_record(ava1_job_t *j, const ava1_bundle_record_t *r) {
    const ava1_ment_t *e;
    uint8_t root[32];
    char path[PATH_CAP];
    int fd, rc;
    if (r->file_id >= j->m.n || j->m.e[r->file_id].kind != AVA1_ENTRY_FILE) return BAD_RECORD;
    e = &j->m.e[r->file_id];
    if (r->data_len != e->size) {
        emit_retry(j, r->file_id, AVA1_RETRY_CHANGED);
        return 0;
    }
    ava1_b3_hash(r->data, r->data_len, root);
    if (memcmp(root, r->root, 32) != 0) {
        emit_retry(j, r->file_id, AVA1_RETRY_VERIFY);
        return 0;
    }
    ava1_apply_path(j, r->file_id, 0, path, sizeof path);
    /* Right before the O_TRUNC: a duplicate must not truncate a file that is already
     * durable, or written and waiting for its batch. */
    pthread_mutex_lock(&j->mu);
    rc = ava1_bits_get(&j->done, r->file_id);
    for (fd = 0; !rc && (uint32_t)fd < j->pend_n; fd++) rc = j->pend_small[fd] == r->file_id;
    pthread_mutex_unlock(&j->mu);
    if (rc) return 0;
    if (!ava1_pend_reserve(pend_gate_stopping, pend_gate_idle, j)) return 0; /* stopping */
    fd = open(path, O_WRONLY | O_CREAT | O_TRUNC | O_NOFOLLOW, 0600);
    if (fd < 0 && errno == ENOENT && mkparents(path) == 0) fd = open(path, O_WRONLY | O_CREAT | O_TRUNC | O_NOFOLLOW, 0600);
    if (fd < 0) {
        rc = -errno;
        ava1_pend_release(1);
        return rc;
    }
    if ((rc = write_all(fd, r->data, r->data_len)) != 0) {
        close(fd);
        ava1_pend_release(1);
        return rc;
    }
    (void)fchmod(fd, (mode_t)(e->mode & 07777));
    ava1_platform_set_mtime(fd, path, e->mtime);
    pthread_mutex_lock(&j->mu);
    j->bytes_received += r->data_len;
    j->applied_since_tune++;
    pthread_mutex_unlock(&j->mu);
    pend_add(j, r->file_id, fd, root);
    return 0;
}

static void run_work(ava1_job_t *j, ava1_work_t *w) {
    int rc = 0;
    if (w->kind == AVA1_W_CHUNK) {
        rc = write_chunk(j, w->file_id, w->offset, w->data, w->len);
    } else if (w->kind == AVA1_W_BUNDLE) {
        ava1_r_t it;
        ava1_bundle_record_t r;
        int k;
        ava1_r_init(&it, w->data, w->len);
        while (rc == 0 && (k = ava1_bundle_record_next(&it, &r)) == 1) rc = apply_record(j, &r);
    }
    if (rc == -ENOSPC) worker_fail(j, AVA1_ERR_NO_SPACE, "the destination drive is full", ENOSPC);
    else if (rc == BAD_RECORD) worker_fail(j, AVA1_ERR_PROTOCOL, "a bundle record names no file", 0);
    else if (rc < 0) worker_fail(j, AVA1_ERR_IO, "write failed", -rc);
    release_owned(w->owned, w->owned_cap);
    give_back(j, w->owned_len);
}

static void *worker_main(void *arg) {
    wctx_t *c = arg;
    ava1_job_t *j = c->j;
    uint32_t idx = c->idx;
    free(c);
    pthread_mutex_lock(&j->mu);
    for (;;) {
        ava1_work_t *w;
        while (!j->stopping && (!j->q_head || idx >= j->want_workers)) pthread_cond_wait(&j->cv, &j->mu);
        if (j->stopping) break;
        w = j->q_head;
        j->q_head = w->next;
        if (!j->q_head) j->q_tail = NULL;
        j->q_len--;
        j->busy++;
        pthread_mutex_unlock(&j->mu);
        if (w->kind == AVA1_W_CALL) {
            w->fn(j, w->arg, w->i);
            pthread_mutex_lock(&j->mu);
            if (--j->calls_left == 0) pthread_cond_broadcast(&j->cv);
            pthread_mutex_unlock(&j->mu);
        } else {
            run_work(j, w);
        }
        free(w);
        pthread_mutex_lock(&j->mu);
        j->busy--;
    }
    pthread_mutex_unlock(&j->mu);
    return NULL;
}

static int add_workers(ava1_job_t *j, uint8_t n) {
    while (j->nworkers < n && j->nworkers < 16) {
        wctx_t *c = malloc(sizeof *c);
        if (!c) return -1;
        c->j = j;
        c->idx = j->nworkers;
        if (ava1_thread_start(worker_main, c, &j->workers[j->nworkers]) != 0) {
            free(c);
            return -1;
        }
        j->nworkers++;
    }
    return 0;
}

/* ---- sync batches ------------------------------------------------------------------ */

/* One large file's share of a batch, kept so its ranges can be re-read after a retried fsync. */
typedef struct {
    uint32_t id, fd_idx, rg_first, rg_n;
    int fd, ob_fd, ob_idx; /* ob_idx < 0: no outboard */
} chk_t;

typedef struct {
    ava1_job_t *job;
    int *fds;
    const uint32_t *ids;          /* the first n_small fds belong to these small files */
    const uint8_t (*roots)[32];   /* ... and carry these BLAKE3 roots */
    uint8_t *retried;             /* per fd: a retry is what made its fsync succeed */
    uint32_t n, n_small, stripes;
    int err, cut; /* cut: a stripe gave up because the job is stopping */
} fdlist_t;

static int stopping_cb(void *a) { return is_stopping(a); }

/* A small file whose fsync needed a retry is read back and compared with the root it arrived
 * with: if the kernel dropped dirty pages on the failed attempt, the retry succeeds on
 * nothing, and this is where it shows. 0, or an errno. */
static int reread_small(ava1_job_t *j, uint32_t id, const uint8_t root[32]) {
    uint64_t size = j->m.e[id].size, got = 0;
    uint8_t *buf = malloc(size ? (size_t)size : 1), h[32];
    char path[PATH_CAP];
    int rc = 0, fd;
    if (!buf) return ENOMEM;
    ava1_apply_path(j, id, 0, path, sizeof path); /* the pending descriptor is write-only */
    fd = open(path, O_RDONLY | O_NOFOLLOW);
    if (fd < 0) {
        free(buf);
        return errno;
    }
    while (got < size) {
        ssize_t k = pread(fd, buf + got, (size_t)(size - got), (off_t)got);
        if (k < 0 && errno == EINTR) continue;
        if (k <= 0) {
            rc = k < 0 ? errno : EIO;
            break;
        }
        got += (uint64_t)k;
    }
    if (!rc) {
        ava1_b3_hash(buf, (size_t)size, h);
        if (memcmp(h, root, 32) != 0) rc = EIO;
    }
    close(fd);
    free(buf);
    return rc;
}

static void sync_stripe(ava1_job_t *j, void *arg, uint32_t i) {
    fdlist_t *l = arg;
    uint32_t k;
    uint32_t delay = ava1_data_cfg()->fsync_delay_us;
    for (k = i; k < l->n; k += l->stripes) {
        uint32_t ms = delay ? (delay / 1000u ? delay / 1000u : 1u) : 0;
        int e, retried = 0;
        while (ms && !is_stopping(j)) { /* a slow disk, in slices a stop can cut */
            uint32_t step = ms < 50u ? ms : 50u;
            ava1_platform_sleep_ms(step);
            ms -= step;
        }
        if (is_stopping(j)) {
            __atomic_store_n(&l->cut, 1, __ATOMIC_RELAXED);
            return;
        }
        e = ava1_fsync_retry(l->fds[k], stopping_cb, j, &retried);
        if (e) {
            if (is_stopping(j)) __atomic_store_n(&l->cut, 1, __ATOMIC_RELAXED);
            else __atomic_store_n(&l->err, e, __ATOMIC_RELAXED);
        } else if (retried) {
            l->retried[k] = 1;
            if (k < l->n_small && (e = reread_small(l->job, l->ids[k], l->roots[k])) != 0)
                __atomic_store_n(&l->err, e, __ATOMIC_RELAXED);
        }
    }
}

/* A large file whose data or outboard fsync needed a retry: every range of this batch is read
 * back and each group's chaining value compared with the outboard's. 0, or an errno. */
static int reread_ranges(const chk_t *c, const ava1_file_range_t *rg, uint64_t size) {
    uint32_t r;
    uint8_t *buf = malloc(AVA1_GROUP_LEN);
    int rc = 0;
    if (!buf) return ENOMEM;
    for (r = c->rg_first; r < c->rg_first + c->rg_n && !rc; r++) {
        uint64_t g, end = rg[r].offset + rg[r].len;
        for (g = rg[r].offset / AVA1_GROUP_LEN; g * AVA1_GROUP_LEN < end && !rc; g++) {
            uint64_t gs = g * AVA1_GROUP_LEN, glen = size - gs < AVA1_GROUP_LEN ? size - gs : AVA1_GROUP_LEN, got = 0;
            uint8_t cv[32], want[32];
            while (got < glen) {
                ssize_t k = pread(c->fd, buf + got, (size_t)(glen - got), (off_t)(gs + got));
                if (k < 0 && errno == EINTR) continue;
                if (k <= 0) {
                    rc = k < 0 ? errno : EIO;
                    break;
                }
                got += (uint64_t)k;
            }
            if (rc) break;
            if (c->ob_fd < 0) continue; /* a one-group file: its root is the CV, checked at commit */
            ava1_b3_group_cv(buf, (size_t)glen, g, cv);
            if (pread(c->ob_fd, want, 32, (off_t)(g * 32u)) != 32 || memcmp(cv, want, 32) != 0) rc = EIO;
        }
    }
    free(buf);
    return rc;
}

static int u32cmp(const void *a, const void *b) {
    uint32_t x = *(const uint32_t *)a, y = *(const uint32_t *)b;
    return x < y ? -1 : x > y;
}

/* Tests: stop dead between two steps of the durability chain, as a power cut would. */
void ava1_apply_crash(ava1_job_t *j) {
    pthread_mutex_lock(&j->mu);
    j->stopping = 1;
    pthread_cond_broadcast(&j->cv);
    pthread_mutex_unlock(&j->mu);
}

static int dirent_cmp(const void *a, const void *b) {
    return strcmp(((const ava1_dirent_t *)a)->dir, ((const ava1_dirent_t *)b)->dir);
}

int ava1_sync_dirset(ava1_job_t *j, ava1_dirent_t *d, uint32_t n, int hook_point) {
    uint32_t i;
    int rc = 0;
    if (n) qsort(d, n, sizeof *d, dirent_cmp);
    for (i = 0; i < n && !rc; i++) {
        if (i && strcmp(d[i].dir, d[i - 1].dir) == 0) continue;
        if (is_stopping(j)) rc = -1;
        else if ((rc = sync_dir(d[i].dir)) == 0 && hook_point) HOOK(j, hook_point, d[i].id);
    }
    for (i = 0; i < n; i++) free(d[i].dir);
    return rc;
}

/* Syncs, once each, the directories that gained an entry in this batch: the small files'
 * (every one was just created or truncated) and the part files opened for the first time.
 * 0, an errno, or -1 when the job is stopping. */
static int sync_new_dirs(ava1_job_t *j, const uint32_t *small, uint32_t n_small, const uint32_t *large,
                         uint32_t n_large) {
    ava1_dirent_t *d = calloc((size_t)n_small + n_large + 1, sizeof *d);
    char p[PATH_CAP], parent[PATH_CAP];
    uint32_t i, n = 0;
    int rc = 0;
    if (!d) return ENOMEM;
    for (i = 0; i < n_small + n_large && !rc; i++) {
        uint32_t id = i < n_small ? small[i] : large[i - n_small];
        ava1_apply_path(j, id, i >= n_small, p, sizeof p);
        if (!p[0]) continue; /* its write failed already */
        parent_of(p, parent, sizeof parent);
        if (!(d[n].dir = strdup(parent))) rc = ENOMEM;
        else d[n++].id = id;
    }
    if (rc) {
        for (i = 0; i < n; i++) free(d[i].dir);
    } else {
        rc = ava1_sync_dirset(j, d, n, AVA1_HOOK_BATCH_DIR_SYNCED);
    }
    free(d);
    return rc;
}

/* The durability chain (SPEC.md §12.6), in this order and no other: (1) data fsync, then
 * the directories that gained entries, (2) journal append + fsync, (3) state, then Durable. */
static void sync_batch(ava1_job_t *j) {
    const ava1_data_cfg_t *cfg = ava1_data_cfg();
    uint32_t *ids, n_small, i, nr = 0, ng = 0, nroots = 0, cap_g = 0, nlf = 0, nnew = 0;
    uint32_t *snap = NULL, nsnap = 0, k; /* the large files with work (lfl_snapshot) */
    uint8_t (*sroots)[32];
    uint8_t *retried = NULL;
    chk_t *chk = NULL;
    uint32_t nchk = 0;
    uint64_t u0 = mono_us(), u1 = 0, u2 = 0, u3 = 0;
    uint32_t nfiles;
    uint32_t *newlf = NULL; /* large files whose part file's directory entry is not yet synced */
    int *sfds, rc;
    ava1_file_run_t *runs = NULL;
    ava1_file_range_t *rg = NULL;
    ava1_root_item_t *roots = NULL;
    fdlist_t l;
    uint64_t t0 = ava1_mono_ms(), new_bytes = 0;
    uint8_t *body = NULL;
    memset(&l, 0, sizeof l);
    pthread_mutex_lock(&j->mu);
    ids = j->pend_small;
    sfds = j->pend_fd;
    sroots = j->pend_root;
    n_small = nfiles = j->pend_n;
    j->pend_small = NULL;
    j->pend_fd = NULL;
    j->pend_root = NULL;
    j->pend_n = j->pend_cap = 0;
    snap = lfl_snapshot(j, &nsnap);
    if (nsnap == UINT32_MAX) {
        pthread_mutex_unlock(&j->mu);
        ava1_apply_fail(j, AVA1_ERR_IO, "out of memory in a sync batch", ENOMEM, 0);
        goto out;
    }
    for (k = 0; k < nsnap; k++) {
        ava1_lfile_t *lf = j->lf[snap[k]];
        if (lf->committed || lf->fd < 0) {
            ava1_rset_clear(&lf->written); /* a late duplicate's range: nothing left to sync */
        } else if (lf->written.n) {
            cap_g += (uint32_t)lf->written.n;
            nlf++;
        }
        if (lf->has_root && !lf->root_journaled) nroots++;
    }
    l.fds = malloc(((size_t)n_small + 2u * nlf + 1u) * sizeof *l.fds);
    rg = malloc(((size_t)cap_g + 1u) * sizeof *rg);
    roots = malloc(((size_t)nroots + 1u) * sizeof *roots);
    runs = malloc(((size_t)n_small + 1u) * sizeof *runs);
    newlf = malloc(((size_t)nlf + 1u) * sizeof *newlf);
    retried = calloc((size_t)n_small + 2u * nlf + 1u, 1);
    chk = malloc(((size_t)nlf + 1u) * sizeof *chk);
    if (!l.fds || !rg || !roots || !runs || !newlf || !retried || !chk) {
        pthread_mutex_unlock(&j->mu);
        ava1_apply_fail(j, AVA1_ERR_IO, "out of memory in a sync batch", ENOMEM, 0);
        goto out;
    }
    for (i = 0; i < n_small; i++) l.fds[l.n++] = sfds[i];
    nroots = 0;
    for (k = 0; k < nsnap; k++) {
        ava1_lfile_t *lf = j->lf[snap[k]];
        size_t r;
        i = snap[k];
        if (lf->written.n) {
            chk[nchk].id = i;
            chk[nchk].fd = lf->fd;
            chk[nchk].ob_fd = lf->ob_fd;
            chk[nchk].fd_idx = l.n;
            chk[nchk].ob_idx = lf->ob_fd >= 0 ? (int)l.n + 1 : -1;
            chk[nchk].rg_first = ng;
            chk[nchk].rg_n = (uint32_t)lf->written.n;
            nchk++;
            for (r = 0; r < lf->written.n; r++) {
                rg[ng].file_id = i;
                rg[ng].offset = lf->written.v[2 * r];
                rg[ng].len = lf->written.v[2 * r + 1] - lf->written.v[2 * r];
                new_bytes += rg[ng].len;
                ng++;
            }
            ava1_rset_clear(&lf->written);
            l.fds[l.n++] = lf->fd;
            if (lf->ob_fd >= 0) l.fds[l.n++] = lf->ob_fd;
            if (!lf->dir_synced) newlf[nnew++] = i;
        }
        if (lf->has_root && !lf->root_journaled) {
            roots[nroots].file_id = i;
            memcpy(roots[nroots].root, lf->root, 32);
            nroots++;
        }
    }
    j->roots_new = 0;
    j->unsynced_bytes = 0;
    lfl_prune(j, snap, nsnap);
    u1 = mono_us();
    pthread_cond_broadcast(&j->cv); /* workers waiting for pend space */
    pthread_mutex_unlock(&j->mu);

    /* 1. data sync, spread over the workers */
    l.stripes = j->want_workers ? j->want_workers : 1;
    l.job = j;
    l.ids = ids;
    l.roots = (const uint8_t(*)[32])sroots;
    l.retried = retried;
    l.n_small = n_small;
    /* A stop mid-sync: some data may be unsynced, so nothing is journaled or acknowledged. */
    if ((l.n && ava1_apply_parallel(j, sync_stripe, &l, l.stripes) != 0) || l.cut || is_stopping(j)) goto out;
    for (k = 0; k < nchk && !l.err; k++) {
        if (retried[chk[k].fd_idx] || (chk[k].ob_idx >= 0 && retried[chk[k].ob_idx]))
            l.err = reread_ranges(&chk[k], rg, j->m.e[chk[k].id].size);
    }
    if (l.err) {
        ava1_apply_fail(j, AVA1_ERR_IO, "fsync failed", l.err, 0);
        goto out;
    }
    HOOK(j, AVA1_HOOK_BATCH_SYNCED, UINT32_MAX);
    u2 = mono_us();
    /* A new file's bytes are durable, its name only once its directory is synced. */
    if ((rc = sync_new_dirs(j, ids, n_small, newlf, nnew)) != 0) {
        if (rc > 0) ava1_apply_fail(j, AVA1_ERR_IO, "syncing a folder failed", rc, 0);
        goto out; /* rc < 0: stopped */
    }
    u3 = mono_us();
    pthread_mutex_lock(&j->mu);
    for (i = 0; i < nnew; i++)
        if (j->lf[newlf[i]]) j->lf[newlf[i]]->dir_synced = 1;
    pthread_mutex_unlock(&j->mu);
    if (cfg->crash_at == AVA1_CRASH_AFTER_SYNC) {
        ava1_apply_crash(j);
        goto out;
    }

    /* 2. journal */
    if (n_small) qsort(ids, n_small, sizeof *ids, u32cmp);
    for (i = 0; i < n_small; i++) {
        if (nr && runs[nr - 1].first + runs[nr - 1].count == ids[i]) runs[nr - 1].count++;
        else if (!nr || runs[nr - 1].first + runs[nr - 1].count < ids[i]) {
            runs[nr].first = ids[i];
            runs[nr].count = 1;
            nr++;
        }
    }
    {
        size_t cap = 64u + 16u * nr + 32u * ng + 48u * nroots;
        ava1_w_t fw, rw, ow, w;
        uint8_t *fb = malloc(16u * nr + 8), *rb = malloc(32u * ng + 8), *ob = malloc(48u * nroots + 8);
        ava1_jnl_batch_t b;
        body = malloc(cap);
        if (!fb || !rb || !ob || !body) {
            free(fb);
            free(rb);
            free(ob);
            ava1_apply_fail(j, AVA1_ERR_IO, "out of memory in a sync batch", ENOMEM, 0);
            goto out;
        }
        ava1_w_init(&fw, fb, 16u * nr + 8);
        ava1_w_init(&rw, rb, 32u * ng + 8);
        ava1_w_init(&ow, ob, 48u * nroots + 8);
        for (i = 0; i < nr; i++) (void)ava1_file_run_append(&fw, &runs[i]);
        for (i = 0; i < ng; i++) (void)ava1_file_range_append(&rw, &rg[i]);
        for (i = 0; i < nroots; i++) (void)ava1_root_item_append(&ow, &roots[i]);
        memset(&b, 0, sizeof b);
        b.files = fb;
        b.files_len = (uint32_t)fw.len;
        b.ranges = rb;
        b.ranges_len = (uint32_t)rw.len;
        b.roots = ob;
        b.roots_len = (uint32_t)ow.len;
        ava1_w_init(&w, body, cap);
        i = (uint32_t)ava1_jnl_batch_encode(&b, &w);
        if (i == 0 && ava1_jnl_append(&j->jnl, AVA1_JNL_BATCH, body, w.len) != 0) i = 1;
        free(fb);
        free(rb);
        free(ob);
        if (i != 0) {
            ava1_apply_fail(j, AVA1_ERR_IO, "journal append failed", EIO, 0);
            goto out;
        }
    }
    HOOK(j, AVA1_HOOK_BATCH_JOURNALED, UINT32_MAX);
    if (cfg->crash_at == AVA1_CRASH_AFTER_JOURNAL) {
        ava1_apply_crash(j);
        goto out;
    }

    /* 3. state, then Durable */
    pthread_mutex_lock(&j->mu);
    for (i = 0; i < n_small; i++) {
        if (!ava1_bits_get(&j->done, ids[i])) {
            ava1_bits_set(&j->done, ids[i]);
            j->files_done++;
            j->bytes_durable += j->m.e[ids[i]].size;
        }
    }
    for (i = 0; i < ng; i++) {
        ava1_lfile_t *lf = j->lf[rg[i].file_id];
        if (lf) (void)ava1_rset_add(&lf->durable, rg[i].offset, rg[i].offset + rg[i].len);
    }
    for (i = 0; i < nroots; i++)
        if (j->lf[roots[i].file_id] && memcmp(j->lf[roots[i].file_id]->root, roots[i].root, 32) == 0)
            j->lf[roots[i].file_id]->root_journaled = 1;
    j->bytes_durable += new_bytes;
    pthread_mutex_unlock(&j->mu);
    for (i = 0; i < n_small; i++) close(sfds[i]);
    ava1_pend_release(n_small);
    n_small = 0;
    if (nr || ng) emit_durable(j, runs, nr, rg, ng);
    {
        uint64_t u4 = mono_us();
        j->st_batches++;
        j->st_files += nfiles;
        j->st_scan_us += u1 - u0;
        j->st_data_us += u2 - u1;
        j->st_dirs_us += u3 - u2;
        j->st_jnl_us += u4 - u3;
    }
    if (ava1_jnl_len(&j->jnl) > AVA1_JNL_COMPACT_AT) {
        uint64_t c0 = mono_us();
        ava1_apply_compact(j);
        j->st_compact_us += mono_us() - c0;
        j->st_compacts++;
    }
    {
        uint64_t dt = ava1_mono_ms() - t0;
        if (dt > 1500 && j->batch_max > 16) j->batch_max /= 2;
        /* past AVA1_PEND_MAX the count trigger could never fire: pend_add caps there */
        else if (dt < 500 && j->batch_max < AVA1_PEND_MAX) j->batch_max *= 2;
    }
out:
    for (i = 0; i < n_small; i++) close(sfds[i]);
    ava1_pend_release(n_small);
    free(snap);
    free(ids);
    free(sfds);
    free(sroots);
    free(retried);
    free(chk);
    free(l.fds);
    free(rg);
    free(roots);
    free(runs);
    free(newlf);
    free(body);
}

void ava1_apply_compact(ava1_job_t *j) {
    ava1_jnl_open_t o;
    ava1_jnl_snapshot_t s;
    ava1_w_t ow, fw, rw, tw, sw;
    size_t nr = 0, cap;
    uint32_t *snap, nsnap, k;
    uint8_t ob[1200], *fb, *rb, *tb, *sb;
    memset(&o, 0, sizeof o);
    memcpy(o.job_id, j->id, 16);
    memcpy(o.manifest_hash, j->manifest_hash, 32);
    o.kind = j->kind;
    o.flags = j->flags;
    o.staged = (uint8_t)((j->staged ? 1 : 0) | (j->dest_held ? AVA1_STAGED_HELD : 0));
    o.root = (const uint8_t *)j->root;
    o.root_len = (uint16_t)strlen(j->root);
    ava1_w_init(&ow, ob, sizeof ob);
    if (ava1_jnl_open_encode(&o, &ow) != 0) return;
    pthread_mutex_lock(&j->mu);
    snap = lfl_snapshot(j, &nsnap);
    if (nsnap == UINT32_MAX) { /* out of memory: keep the longer journal, it is still whole */
        pthread_mutex_unlock(&j->mu);
        return;
    }
    for (k = 0; k < nsnap; k++) nr += j->lf[snap[k]]->durable.n + 1;
    cap = 16u * (size_t)j->m.n + 8;
    fb = malloc(cap);
    rb = malloc(32u * nr + 8);
    tb = malloc(48u * (size_t)j->m.n + 8);
    sb = malloc(cap + 32u * nr + 48u * (size_t)j->m.n + 64);
    if (fb && rb && tb && sb) {
        ava1_w_init(&fw, fb, cap);
        ava1_w_init(&rw, rb, 32u * nr + 8);
        ava1_w_init(&tw, tb, 48u * (size_t)j->m.n + 8);
        (void)ava1_bits_append_runs(&j->done, &fw);
        for (k = 0; k < nsnap; k++) {
            uint32_t i = snap[k];
            ava1_lfile_t *lf = j->lf[i];
            size_t r;
            if (ava1_bits_get(&j->done, i)) continue;
            for (r = 0; r < lf->durable.n; r++) {
                ava1_file_range_t g;
                g.file_id = i;
                g.offset = lf->durable.v[2 * r];
                g.len = lf->durable.v[2 * r + 1] - g.offset;
                (void)ava1_file_range_append(&rw, &g);
            }
            if (lf->has_root && lf->root_journaled) {
                ava1_root_item_t r;
                r.file_id = i;
                memcpy(r.root, lf->root, 32);
                (void)ava1_root_item_append(&tw, &r);
            }
        }
        memset(&s, 0, sizeof s);
        s.done = fb;
        s.done_len = (uint32_t)fw.len;
        s.ranges = rb;
        s.ranges_len = (uint32_t)rw.len;
        s.roots = tb;
        s.roots_len = (uint32_t)tw.len;
        ava1_w_init(&sw, sb, cap + 32u * nr + 48u * (size_t)j->m.n + 64);
        if (ava1_jnl_snapshot_encode(&s, &sw) == 0) (void)ava1_jnl_compact(&j->jnl, ob, ow.len, sb, sw.len, NULL, 0);
    }
    pthread_mutex_unlock(&j->mu);
    free(snap);
    free(fb);
    free(rb);
    free(tb);
    free(sb);
}

/* ---- commit ------------------------------------------------------------------------ */

void ava1_apply_reset(ava1_job_t *j, uint32_t id, uint16_t reason) {
    ava1_jnl_reset_t r;
    uint8_t b[16];
    ava1_w_t w;
    ava1_lfile_t *lf = j->lf[id];
    r.file_id = id;
    ava1_w_init(&w, b, sizeof b);
    if (ava1_jnl_reset_encode(&r, &w) == 0) (void)ava1_jnl_append(&j->jnl, AVA1_JNL_RESET, b, w.len);
    pthread_mutex_lock(&j->mu);
    j->bytes_durable -= ava1_rset_covered(&lf->durable);
    ava1_rset_clear(&lf->written);
    ava1_rset_clear(&lf->durable);
    lf->has_root = lf->root_journaled = 0;
    pthread_mutex_unlock(&j->mu);
    if (lf->ob_fd >= 0) {
        uint64_t n = groups_of(j->m.e[id].size) * 32u;
        (void)ftruncate(lf->ob_fd, 0);
        (void)ftruncate(lf->ob_fd, (off_t)n);
    }
    if (reason) emit_retry(j, id, reason);
}

static int read_root(ava1_job_t *j, uint32_t id, uint8_t root[32]) {
    ava1_lfile_t *lf = j->lf[id];
    uint64_t size = j->m.e[id].size, n = groups_of(size);
    uint8_t *buf;
    ssize_t k;
    if (n >= 2) {
        buf = malloc((size_t)n * 32u);
        if (!buf) return -ENOMEM;
        k = pread(lf->ob_fd, buf, (size_t)n * 32u, 0);
        if (k == (ssize_t)(n * 32u)) ava1_b3_root_from_cvs((const uint8_t (*)[32])buf, n, root);
    } else {
        buf = malloc(size ? (size_t)size : 1);
        if (!buf) return -ENOMEM;
        k = pread(lf->fd, buf, (size_t)size, 0);
        if (k == (ssize_t)size) ava1_b3_hash(buf, (size_t)size, root);
        else k = -1;
        if (k >= 0) k = (ssize_t)(n * 32u);
    }
    free(buf);
    return k == (ssize_t)(n * 32u) ? 0 : -EIO;
}

static void commit_large(ava1_job_t *j, uint32_t id) {
    const ava1_data_cfg_t *cfg = ava1_data_cfg();
    ava1_lfile_t *lf = j->lf[id];
    const ava1_ment_t *e = &j->m.e[id];
    char part[PATH_CAP], fin[PATH_CAP], parent[PATH_CAP], ob[600];
    uint8_t root[32], want[32];
    int err = 0;
    pthread_mutex_lock(&j->mu);
    if (cfg->crash_at == AVA1_CRASH_BEFORE_COMMIT) {
        pthread_mutex_unlock(&j->mu);
        ava1_apply_crash(j);
        return;
    }
    if (lf->fd < 0 && !lfile_open(j, id, &err, 0)) {
        pthread_mutex_unlock(&j->mu);
        if (err == ENOENT) { /* its bytes are gone: start the file over */
            ava1_apply_reset(j, id, AVA1_RETRY_IO);
            return;
        }
        ava1_apply_fail(j, AVA1_ERR_IO, "reopen for commit failed", err, 0);
        return;
    }
    memcpy(want, lf->root, 32); /* ava1_apply_root may replace it from another thread */
    pthread_mutex_unlock(&j->mu);
    if (read_root(j, id, root) != 0) {
        ava1_apply_fail(j, AVA1_ERR_IO, "reading the outboard failed", EIO, 0);
        return;
    }
    if (memcmp(root, want, 32) != 0) {
        ava1_apply_reset(j, id, AVA1_RETRY_VERIFY);
        return;
    }
    ava1_apply_path(j, id, 1, part, sizeof part);
    ava1_apply_path(j, id, 0, fin, sizeof fin);
    if (!part[0] || !fin[0]) {
        ava1_apply_fail(j, AVA1_ERR_IO, "path too long", ENAMETOOLONG, 0);
        return;
    }
    HOOK(j, AVA1_HOOK_COMMIT_VERIFIED, id);
    /* From here on a late duplicate chunk is dropped (write_chunk checks `committed`), and
     * one that slipped in before this point is forgotten: the fd is about to close. */
    pthread_mutex_lock(&j->mu);
    lf->committed = 1;
    ava1_rset_clear(&lf->written);
    pthread_mutex_unlock(&j->mu);
    /* Truncate (preallocate may have reserved more) before the mtime: on Linux ftruncate
     * itself sets the mtime. The fsync makes all three durable. */
    (void)fchmod(lf->fd, (mode_t)(e->mode & 07777));
    if (ftruncate(lf->fd, (off_t)e->size) != 0) {
        ava1_apply_fail(j, AVA1_ERR_IO, "final truncate failed", errno, 0);
        return;
    }
    ava1_platform_set_mtime(lf->fd, part, e->mtime);
    if ((err = ava1_fsync_retry(lf->fd, stopping_cb, j, NULL)) != 0) {
        ava1_apply_fail(j, AVA1_ERR_IO, "final sync failed", err, 0);
        return;
    }
    close(lf->fd);
    lf->fd = -1;
    if (lf->ob_fd >= 0) close(lf->ob_fd);
    lf->ob_fd = -1;
    if (strcmp(part, fin) != 0) {
        parent_of(fin, parent, sizeof parent);
        /* Same directory by construction; checked anyway (SPEC.md §12.6, the kernel panic). */
        if (cfg->same_device && cfg->same_device(part, parent) == 0) {
            ava1_apply_fail(j, AVA1_ERR_CROSS_DEVICE, "the destination is on another drive", 0, 1);
            return;
        }
        if (rename(part, fin) != 0) {
            int e = errno;
            if (in_the_way(e)) ava1_apply_fail(j, AVA1_ERR_EXISTS, "something is already where the file goes", e, 1);
            else ava1_apply_fail(j, AVA1_ERR_IO, "rename into place failed", e, 1);
            return;
        }
        HOOK(j, AVA1_HOOK_RENAMED, id);
        if ((err = sync_dir(parent)) != 0) {
            ava1_apply_fail(j, AVA1_ERR_IO, "syncing the folder failed", err, 1);
            return;
        }
        HOOK(j, AVA1_HOOK_DIR_SYNCED, id);
        if (cfg->crash_at == AVA1_CRASH_COMMIT_RENAMED) {
            ava1_apply_crash(j);
            return;
        }
    }
    {
        ava1_jnl_batch_t b;
        ava1_file_run_t r = { id, 1 };
        uint8_t rb[32], body[64];
        ava1_w_t rw, w;
        ava1_w_init(&rw, rb, sizeof rb);
        (void)ava1_file_run_append(&rw, &r);
        memset(&b, 0, sizeof b);
        b.files = rb;
        b.files_len = (uint32_t)rw.len;
        ava1_w_init(&w, body, sizeof body);
        if (ava1_jnl_batch_encode(&b, &w) != 0 || ava1_jnl_append(&j->jnl, AVA1_JNL_BATCH, body, w.len) != 0) {
            ava1_apply_fail(j, AVA1_ERR_IO, "journal append failed", EIO, 0);
            return;
        }
        HOOK(j, AVA1_HOOK_JOURNALED, id);
        /* only once the commit is journaled: until then a replay may still need the CVs */
        ob_path(j, id, ob, sizeof ob);
        (void)unlink(ob);
        HOOK(j, AVA1_HOOK_OB_UNLINKED, id);
        pthread_mutex_lock(&j->mu);
        ava1_bits_set(&j->done, id);
        j->files_done++;
        pthread_mutex_unlock(&j->mu);
        emit_durable(j, &r, 1, NULL, 0);
    }
}

void ava1_apply_commit_ready(ava1_job_t *j) {
    uint32_t *snap, n, k;
    pthread_mutex_lock(&j->mu);
    snap = lfl_snapshot(j, &n);
    pthread_mutex_unlock(&j->mu);
    if (n == UINT32_MAX) return; /* out of memory: the next batch asks again */
    for (k = 0; k < n && !j->finished; k++) {
        uint32_t i = snap[k];
        ava1_lfile_t *lf;
        int ready;
        pthread_mutex_lock(&j->mu);
        lf = j->lf[i]; /* the same lf the snapshot saw: only this thread frees one at a commit */
        ready = lf && !lf->committed && !ava1_bits_get(&j->done, i) && lf->has_root && lf->root_journaled &&
                !lf->written.n && (j->m.e[i].size == 0 || ava1_rset_covers(&lf->durable, 0, j->m.e[i].size));
        pthread_mutex_unlock(&j->mu);
        if (ready) commit_large(j, i);
    }
    free(snap);
}

/* ---- finishing --------------------------------------------------------------------- */

static void finish(ava1_job_t *j) {
    const ava1_data_cfg_t *cfg = ava1_data_cfg();
    char parent[PATH_CAP];
    struct stat st;
    int e;
    if (j->staged && !(j->flags & AVA1_JF_SINGLE_FILE)) {
        parent_of(j->root, parent, sizeof parent);
        /* A held root is our own empty lock folder (SPEC.md §11.6): the rename below
         * replaces it only while it is still empty. */
        if (!j->dest_held && stat(j->root, &st) == 0) {
            ava1_apply_fail(j, AVA1_ERR_EXISTS, "the destination appeared during the upload; the files are in .ava-part", 0, 1);
            return;
        }
        if (cfg->same_device && cfg->same_device(j->base, parent) == 0) {
            ava1_apply_fail(j, AVA1_ERR_CROSS_DEVICE, "the destination is on another drive", 0, 1);
            return;
        }
        if (rename(j->base, j->root) != 0) {
            e = errno;
            if (in_the_way(e)) ava1_apply_fail(j, AVA1_ERR_EXISTS, "the destination is not empty; the files are in .ava-part", e, 1);
            else ava1_apply_fail(j, AVA1_ERR_IO, "renaming the finished folder failed", e, 1);
            return;
        }
        HOOK(j, AVA1_HOOK_RENAMED, UINT32_MAX);
        e = ava1_apply_fault ? ava1_apply_fault(j, AVA1_HOOK_DIR_SYNCED, UINT32_MAX) : 0;
        if (!e) e = sync_dir(parent);
        if (e) {
            /* The tree is in place and complete; only the rename's own durability is in
             * doubt. Reporting ERR_IO would make the sender resend a finished tree. */
            ava1_apply_fail(j, j->kind == AVA1_JOB_COPY ? AVA1_ERR_IO : AVA1_STATUS_OK,
                            "the folder is in place; syncing its parent failed", e, 1);
            return;
        }
        HOOK(j, AVA1_HOOK_DIR_SYNCED, UINT32_MAX);
        if (cfg->crash_at == AVA1_CRASH_STAGED_RENAMED) {
            ava1_apply_crash(j);
            return;
        }
    }
    ava1_apply_fail(j, AVA1_STATUS_OK, "", 0, 1); /* the success path: see ava1_apply_fail */
}

void ava1_apply_finish_landed(ava1_job_t *j) {
    char parent[PATH_CAP];
    int err;
    parent_of(j->root, parent, sizeof parent);
    err = sync_dir(parent);
    if (err) ava1_apply_fail(j, AVA1_ERR_IO, "syncing the landed folder failed", err, 1);
    else ava1_apply_fail(j, AVA1_STATUS_OK, "", 0, 1);
}

void ava1_apply_quiesce(ava1_job_t *j) {
    uint32_t i;
    int pend, ok;
    pthread_mutex_lock(&j->mu);
    while ((j->q_len || j->busy) && !j->stopping) {
        /* A worker may be waiting in pend_add for room: make it, or nobody ever will. */
        int sync = j->pend_n != 0, can = j->prepared && !j->finished && !j->final_status;
        if (sync && !can) {
            for (i = 0; i < j->pend_n; i++) close(j->pend_fd[i]); /* unsynced: sent again */
            ava1_pend_release(j->pend_n);
            j->pend_n = 0;
            pthread_cond_broadcast(&j->cv);
        }
        pthread_mutex_unlock(&j->mu);
        if (sync && can) sync_batch(j);
        else ava1_platform_sleep_ms(2);
        pthread_mutex_lock(&j->mu);
    }
    pend = j->pend_n || j->unsynced_bytes || j->roots_new;
    ok = j->prepared && !j->finished && !j->final_status && !j->stopping;
    pthread_mutex_unlock(&j->mu);
    if (pend && ok) sync_batch(j);
    /* Whatever could not be made durable is dropped: it is not in the map, so it is sent again. */
    pthread_mutex_lock(&j->mu);
    for (i = 0; i < j->pend_n; i++) close(j->pend_fd[i]);
    ava1_pend_release(j->pend_n);
    j->pend_n = 0;
    if (j->lf) {
        uint32_t *snap, n, k;
        snap = lfl_snapshot(j, &n);
        if (n == UINT32_MAX) { /* out of memory: clear them all, as before the index */
            for (i = 0; i < j->m.n; i++)
                if (j->lf[i]) ava1_rset_clear(&j->lf[i]->written);
        } else {
            for (k = 0; k < n; k++) ava1_rset_clear(&j->lf[snap[k]]->written);
        }
        free(snap);
    }
    j->unsynced_bytes = 0;
    pthread_mutex_unlock(&j->mu);
}

static int all_done(ava1_job_t *j) {
    int d;
    pthread_mutex_lock(&j->mu);
    d = j->prepared && !j->finished && !j->final_status && j->files_done >= j->m.files && j->pend_n == 0 && j->q_len == 0 && j->busy == 0;
    pthread_mutex_unlock(&j->mu);
    return d;
}

static void tune_workers(ava1_job_t *j, uint64_t now) {
    uint8_t want;
    double secs = (double)(now - j->tune_ms) / 1000.0;
    pthread_mutex_lock(&j->mu);
    want = ava1_wtune_step(&j->tune, secs > 0 ? j->applied_since_tune / secs : 0,
                           j->tune_ticks && j->tune_busy * 2 >= j->tune_ticks);
    j->applied_since_tune = 0;
    j->tune_ticks = j->tune_busy = 0;
    pthread_mutex_unlock(&j->mu);
    j->tune_ms = now;
    if (want > j->nworkers) (void)add_workers(j, want);
    pthread_mutex_lock(&j->mu);
    j->want_workers = want <= j->nworkers ? want : j->nworkers;
    pthread_cond_broadcast(&j->cv);
    pthread_mutex_unlock(&j->mu);
}

/* One line of stderr.log per ten seconds of batches: where this job's time goes. */
static void log_stats(ava1_job_t *j, uint64_t now) {
    double b = (double)j->st_batches;
    j->st_log_ms = now;
    fprintf(stderr,
            "[ava1] job %02x%02x%02x%02x: %u/%u files, %llu batches (%.0f files each), per batch ms: "
            "scan %.2f data %.1f dirs %.1f journal %.1f commit %.1f; %llu compactions %.1f ms total; "
            "%u large in flight\n",
            j->id[0], j->id[1], j->id[2], j->id[3], j->files_done, j->m.files,
            (unsigned long long)j->st_batches, (double)j->st_files / b, (double)j->st_scan_us / b / 1000.0,
            (double)j->st_data_us / b / 1000.0, (double)j->st_dirs_us / b / 1000.0,
            (double)j->st_jnl_us / b / 1000.0, (double)j->st_commit_us / b / 1000.0,
            (unsigned long long)j->st_compacts, (double)j->st_compact_us / 1000.0, j->lfl_n);
    j->st_batches = j->st_files = j->st_data_us = j->st_dirs_us = j->st_jnl_us = j->st_scan_us = 0;
    j->st_commit_us = j->st_compact_us = j->st_compacts = 0;
}

static void *job_main(void *arg) {
    ava1_job_t *j = arg;
    for (;;) {
        uint64_t now, flush;
        int ev, batch, failed;
        uint16_t fail_status = 0;
        char fail_msg[sizeof j->message];
        ava1_platform_sleep_ms(TICK_MS);
        now = ava1_mono_ms();
        pthread_mutex_lock(&j->mu);
        if (j->stopping) {
            pthread_mutex_unlock(&j->mu);
            break;
        }
        j->ticks++;
        j->tune_ticks++;
        if (j->q_len) {
            j->q_busy_ticks++;
            j->tune_busy++;
        }
        ev = j->ev_end || j->ev_resume;
        flush = j->credit_back;
        j->credit_back = 0;
        failed = !j->finished && j->final_status != 0; /* a worker's failure (worker_fail) */
        if (failed) {
            fail_status = j->final_status;
            memcpy(fail_msg, j->message, sizeof fail_msg);
        }
        batch = j->prepared && !j->finished && !failed && !__atomic_load_n(&ava1_apply_hold_batches, __ATOMIC_SEQ_CST) &&
                ((j->pend_n && (j->pend_n >= j->batch_max || ava1_pend_full())) || j->unsynced_bytes >= BATCH_BYTES ||
                 ((j->pend_n || j->unsynced_bytes || j->roots_new) && now - j->last_batch_ms >= BATCH_MS));
        pthread_mutex_unlock(&j->mu);
        if (failed) ava1_apply_fail(j, fail_status, fail_msg, 0, 0);
        if (ev && j->on_events) j->on_events(j);
        if (j->on_tick) j->on_tick(j);
        if (flush) emit_credit(j, flush);
        if (batch) {
            sync_batch(j);
            j->last_batch_ms = now;
            if (!j->stopping) {
                uint64_t c0 = mono_us();
                ava1_apply_commit_ready(j);
                j->st_commit_us += mono_us() - c0;
            }
        }
        if (j->st_batches && now - j->st_log_ms >= 10000) log_stats(j, now);
        if (all_done(j)) finish(j);
        if (now - j->status_ms >= 250) {
            j->status_ms = now;
            if (!j->finished) ava1_apply_status(j);
        }
        if (now - j->tune_ms >= 2000 && j->prepared && !j->finished) tune_workers(j, now);
    }
    return NULL;
}

int ava1_apply_start(ava1_job_t *j) {
    const ava1_data_cfg_t *cfg = ava1_data_cfg();
    ava1_wtune_init(&j->tune, cfg->workers_start, cfg->workers_min, cfg->workers_max);
    j->batch_max = 256;
    j->tune_ms = j->status_ms = j->last_batch_ms = ava1_mono_ms();
    if (add_workers(j, cfg->workers_start) != 0 && j->nworkers == 0) return -1;
    j->want_workers = j->nworkers;
    if (ava1_thread_start(job_main, j, &j->thread) != 0) return -1;
    j->thread_started = 1;
    return 0;
}
