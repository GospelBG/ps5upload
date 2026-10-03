/* Console-local copy and move (SPEC.md §13.5). A copy is a receiver job that talks to
 * Task 18's reader instead of a socket: ava1_recv_open prepares it exactly like an upload,
 * the reader feeds it Chunk/Bundle through ava1_apply_*, and the map it gets back is the
 * resume state. A move deletes the source after a verified Done. */
#include "ava1_copy.h"

#include <dirent.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#include "ava1_apply.h"
#include "ava1_data.h"
#include "ava1_platform.h"
#include "ava1_recv.h"
#include "ava1_send.h"   /* Task 18: ava1_reader_t, ava1_read_files */
#include "ava1_thread.h"

static const uint8_t LOCAL_OWNER[32] = { 0 };

typedef struct {
    ava1_bits_t skip;
    ava1_rset_t **durable;
    int have_map, reader_started, move;
    volatile int stop;
    pthread_t reader;
} cp_t;

static cp_t *C_(ava1_job_t *j) { return (cp_t *)j->role; }

/* The in-process "network": apply the reader's message, waiting for credit. */
static int cp_put(void *ctx, uint8_t type, uint8_t *msg, size_t len) {
    ava1_job_t *j = ctx;
    while (ava1_apply_reserve(j, len) != 0) {
        if (C_(j)->stop || j->stopping || j->finished) {
            free(msg);
            return -ECANCELED;
        }
        ava1_platform_sleep_ms(1);
    }
    if (type == AVA1_TYPE_CHUNK) {
        ava1_chunk_t c;
        if (ava1_chunk_decode(msg, len, &c) != 0) return -EIO;
        return ava1_apply_chunk(j, msg, len, c.file_id, c.offset, c.data, c.data_len) == 0 ? 0 : -EIO;
    }
    {
        ava1_bundle_t b;
        if (ava1_bundle_decode(msg, len, &b) != 0) return -EIO;
        return ava1_apply_bundle(j, msg, len, &b) == 0 ? 0 : -EIO;
    }
}

static void cp_root(void *ctx, uint32_t id, const uint8_t root[32]) { (void)ava1_apply_root(ctx, id, root); }

static void *cp_reader(void *arg) {
    ava1_job_t *j = arg;
    cp_t *c = C_(j);
    const ava1_data_cfg_t *cfg = ava1_data_cfg();
    ava1_reader_t r;
    struct stat st;
    int rc;
    memset(&r, 0, sizeof r);
    r.j = j;
    r.src_root = j->src;
    r.single = stat(j->src, &st) == 0 && !S_ISDIR(st.st_mode);
    r.cutoff = cfg->cutoff ? cfg->cutoff : 256u << 10; /* must equal the receiver's */
    r.chunk = 8u << 20;
    r.bundle = 1u << 20;
    r.skip = &c->skip;
    r.durable = c->durable;
    r.put = cp_put;
    r.root = cp_root;
    r.ctx = j;
    r.stop = &c->stop;
    rc = ava1_read_files(&r, NULL, 0);
    if (rc != 0 && rc != -ECANCELED) ava1_apply_fail(j, AVA1_ERR_IO, "reading the source failed", -rc, 0);
    return NULL;
}

/* Deletes the source tree: files first, then directories deepest first. Never renames, and
 * never follows a link (unlink removes the link, not its target). Idempotent: a move whose
 * crash landed after the journaled Done but before this ran re-deletes nothing on resume. */
static void delete_source(ava1_job_t *j) {
    char p[AVA1_MAX_PATH * 2 + 8];
    struct stat st;
    uint32_t i;
    if (stat(j->src, &st) == 0 && !S_ISDIR(st.st_mode)) { /* stat follows: a link to a file */
        (void)unlink(j->src);                             /* is unlinked, not its target */
        return;
    }
    for (i = 0; i < j->m.n; i++)
        if (j->m.e[i].kind == AVA1_ENTRY_FILE) {
            snprintf(p, sizeof p, "%s/%s", j->src, ava1_mstore_path(&j->m, i));
            (void)unlink(p);
        }
    for (i = j->m.n; i-- > 0;)
        if (j->m.e[i].kind == AVA1_ENTRY_DIR) {
            snprintf(p, sizeof p, "%s/%s", j->src, ava1_mstore_path(&j->m, i));
            (void)rmdir(p);
        }
    (void)rmdir(j->src);
}

static void cp_emit(ava1_job_t *j, uint8_t type, uint8_t flags, const uint8_t *body, size_t len) {
    cp_t *c = C_(j);
    (void)flags;
    if (type == AVA1_TYPE_JOB_MAP) {
        ava1_job_map_t m;
        ava1_r_t it;
        ava1_file_run_t r;
        ava1_file_range_t g;
        if (ava1_job_map_decode(body, len, &m) != 0 || m.status != AVA1_STATUS_OK) return; /* apply_fail reports it */
        ava1_r_init(&it, m.done, m.done_len);
        while (ava1_file_run_next(&it, &r) == 1) {
            uint32_t f;
            for (f = r.first; f < r.first + r.count && f < j->m.n; f++) ava1_bits_set(&c->skip, f);
        }
        ava1_r_init(&it, m.partial, m.partial_len);
        while (ava1_file_range_next(&it, &g) == 1)
            if (g.file_id < j->m.n) {
                if (!c->durable[g.file_id]) c->durable[g.file_id] = calloc(1, sizeof(ava1_rset_t));
                if (c->durable[g.file_id]) (void)ava1_rset_add(c->durable[g.file_id], g.offset, g.offset + g.len);
            }
        if (m.last) c->have_map = 1;
    } else if (type == AVA1_TYPE_JOB_DONE) {
        ava1_job_done_t d;
        if (ava1_job_done_decode(body, len, &d) == 0) {
            /* The reaper reads parked_at_ms under the table lock, which the job thread does
             * not hold when JobDone is emitted: a release store makes the stamp visible
             * (ruling 6). Every finished copy gets the full park age from here; the delete
             * below may run on this call, so the stamp goes first. */
            __atomic_store_n(&j->parked_at_ms, ava1_mono_ms(), __ATOMIC_RELEASE);
            if (d.status == AVA1_STATUS_OK && c->move) delete_source(j);
        }
    } else if (type == AVA1_TYPE_FILE_RETRY) {
        /* Local bytes failed verification: the drive is not returning what was written. */
        ava1_apply_fail(j, AVA1_ERR_VERIFY, "a copied file did not verify", 0, 0);
    }
}

static void cp_tick(ava1_job_t *j) {
    cp_t *c = C_(j);
    if (c->have_map && !c->reader_started) c->reader_started = ava1_thread_start(cp_reader, j, &c->reader) == 0;
}

static void cp_free(ava1_job_t *j) {
    cp_t *c = C_(j);
    uint32_t i;
    if (!c) return;
    c->stop = 1;
    if (c->reader_started) pthread_join(c->reader, NULL);
    for (i = 0; c->durable && i < j->m.n; i++)
        if (c->durable[i]) {
            ava1_rset_clear(c->durable[i]);
            free(c->durable[i]);
        }
    free(c->durable);
    ava1_bits_free(&c->skip);
    free(c);
    j->role = NULL;
}

static int inside(const char *a, const char *b) { /* b == a or b under a */
    size_t n = strlen(a);
    return strncmp(a, b, n) == 0 && (b[n] == 0 || b[n] == '/');
}

/* C14, JF_OVERWRITE unset: a destination path the copy would write exists. The root itself
 * (which a single file replaces and a tree merges into) and every manifest path. lstat:
 * a dangling link at the destination counts (the receiver's O_NOFOLLOW writes would fail
 * on it too), and a link is never followed. */
static int dest_collides(const ava1_mstore_t *in, const char *dest) {
    char p[AVA1_MAX_PATH * 2 + 8];
    struct stat st;
    uint32_t i;
    if (lstat(dest, &st) == 0) return 1;
    for (i = 0; i < in->n; i++) {
        if (snprintf(p, sizeof p, "%s/%s", dest, ava1_mstore_path(in, i)) >= (int)sizeof p) continue;
        if (lstat(p, &st) == 0) return 1;
    }
    return 0;
}

ava1_job_t *ava1_copy_open(const ava1_job_copy_t *in, uint16_t *status, char *msg, size_t cap) {
    const ava1_data_cfg_t *cfg = ava1_data_cfg();
    char src[AVA1_MAX_PATH + 1], dest[AVA1_MAX_PATH + 1];
    ava1_recv_spec_t s;
    ava1_job_open_ack_t ack;
    ava1_job_t *j;
    cp_t *c;
    struct stat st;
    int rc;
    *status = AVA1_ERR_PATH;
    if (in->src_len > AVA1_MAX_PATH || in->dest_len > AVA1_MAX_PATH) return NULL;
    memcpy(src, in->src, in->src_len);
    src[in->src_len] = 0;
    memcpy(dest, in->dest, in->dest_len);
    dest[in->dest_len] = 0;
    if (!cfg->may_read || !cfg->may_read(src, 0) || stat(src, &st) != 0) {
        snprintf(msg, cap, "cannot read %s", src);
        return NULL;
    }
    /* CORRECTED (ruling 3): the destination must be writable, and a move must be able to
     * delete the source. */
    if (!cfg->may_write || !cfg->may_write(dest) || ((in->flags & AVA1_JF_MOVE) && !cfg->may_write(src))) {
        snprintf(msg, cap, "cannot write %s", dest);
        return NULL;
    }
    if (inside(src, dest)) { /* includes dest == src */
        snprintf(msg, cap, "the destination is inside the source");
        return NULL;
    }
    if (inside(dest, src)) { /* the written namespace would reach into the read tree */
        snprintf(msg, cap, "the destination contains the source");
        return NULL;
    }
    j = ava1_job_find(in->job_id);
    if (j) { /* listed: running, or finished and still answering (ruling 5) */
        *status = AVA1_STATUS_OK;
        return j;
    }
    memset(&s, 0, sizeof s);
    memcpy(s.id, in->job_id, 16);
    memcpy(s.owner, LOCAL_OWNER, 32);
    s.kind = AVA1_JOB_COPY;
    s.policy = AVA1_POLICY_REPLACE;
    s.flags = S_ISDIR(st.st_mode) ? 0 : AVA1_JF_SINGLE_FILE;
    s.root = dest;
    s.emit = cp_emit;
    s.emit_ctx = NULL;
    s.sid = NULL; /* a local job has no session; any paired device may poll it */
    j = ava1_recv_open(&s, &ack, msg, cap);
    if (!j) {
        *status = ack.status;
        return NULL;
    }
    c = calloc(1, sizeof *c);
    /* CORRECTED: `j->src` must be set before anything can start the reader (cp_tick waits
     * for the map, which needs the prepare the job thread runs after the manifest hash is
     * checked) — and the walk must not hold j->mu (it mallocs and does I/O). */
    snprintf(j->src, sizeof j->src, "%s", src);
    rc = c ? (S_ISDIR(st.st_mode) ? ava1_mstore_walk(&j->m_in, src) : ava1_mstore_single(&j->m_in, src))
           : -ENOMEM;
    if (rc != 0 || ava1_bits_init(&c->skip, j->m_in.n) != 0 ||
        !(c->durable = calloc((size_t)j->m_in.n + 1, sizeof *c->durable))) {
        free(c);
        *status = AVA1_ERR_IO; /* CORRECTED: an unwalkable source is I/O, not a path error */
        snprintf(msg, cap, "cannot list %s", src);
        ava1_job_free_one(in->job_id); /* no role was set: free_one + put destroys it */
        ava1_job_put(j);
        return NULL;
    }
    /* C14: without JF_OVERWRITE, refuse an existing destination with ERR_EXISTS before
     * anything is written (the receiver writes nothing until the manifest is adopted, which
     * needs the ev_end set at the end of this function, so this check is the last word).
     * The residual TOCTOU window — a path created after this walk but before the write — is
     * accepted for v1: the receiver then replaces that file (policy REPLACE), because a
     * partial copy followed by a refusal is the worse failure. With the flag, colliding
     * destination files are replaced per file and destination-only entries are left alone
     * (a copy never wipes a tree it was not asked to wipe). A re-issued job.copy that
     * resumed the same job from its journal is not refused: the paths found are its own
     * partials, which the receiver reconciles. */
    if (!(in->flags & AVA1_JF_OVERWRITE)) {
        int resumed;
        pthread_mutex_lock(&j->mu);
        resumed = j->have_manifest;
        pthread_mutex_unlock(&j->mu);
        if (!resumed && dest_collides(&j->m_in, dest)) {
            ava1_bits_free(&c->skip);
            free(c->durable);
            free(c);
            *status = AVA1_ERR_EXISTS;
            snprintf(msg, cap, "the destination already exists");
            ava1_job_free_one(in->job_id); /* no role was set: free_one + put destroys it */
            ava1_job_put(j);
            return NULL;
        }
    }
    c->move = (in->flags & AVA1_JF_MOVE) != 0;
    pthread_mutex_lock(&j->mu);
    j->role = c;
    j->role_free = cp_free;
    j->on_tick = cp_tick;
    j->end_files = j->m_in.files;
    j->end_bytes = j->m_in.bytes;
    ava1_mstore_hash(&j->m_in, j->end_hash);
    j->ev_end = 1; /* the receiver prepares and "sends" its map to cp_emit */
    pthread_mutex_unlock(&j->mu);
    *status = AVA1_STATUS_OK;
    return j;
}
