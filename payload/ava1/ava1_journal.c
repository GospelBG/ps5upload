#include "ava1_journal.h"

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#include "ava1_frame.h"
#include "ava1_wire.h"

static const uint8_t MAGIC[8] = { 'A', 'V', 'A', '1', 'J', 'N', 'L', '1' };

static int write_all_fd(int fd, const uint8_t *p, size_t n) {
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

static void put32(uint8_t *p, uint32_t v) {
    p[0] = (uint8_t)v;
    p[1] = (uint8_t)(v >> 8);
    p[2] = (uint8_t)(v >> 16);
    p[3] = (uint8_t)(v >> 24);
}

static uint32_t get32(const uint8_t *p) {
    return (uint32_t)p[0] | ((uint32_t)p[1] << 8) | ((uint32_t)p[2] << 16) | ((uint32_t)p[3] << 24);
}

/* len ‖ kind ‖ body ‖ crc(kind ‖ body) in one heap buffer. */
static uint8_t *frame_rec(uint8_t kind, const uint8_t *body, size_t len, size_t *out_len) {
    uint8_t *f = malloc(len + 9);
    if (!f) return NULL;
    put32(f, (uint32_t)(len + 1));
    f[4] = kind;
    if (len) memcpy(f + 5, body, len);
    put32(f + 5 + len, ava1_crc32c(f + 4, len + 1));
    *out_len = len + 9;
    return f;
}

static int sync_dir(const char *dir) {
    int fd = open(dir, O_RDONLY);
    if (fd < 0) return -errno;
    /* The directory fsync is the step that makes a rename durable — the one failure it
     * exists to catch (EIO) must not be swallowed, or callers report durability they
     * do not have. Rust propagates it the same way (SPEC.md §14.1). */
    int rc = fsync(fd) != 0 ? -errno : 0;
    close(fd);
    return rc;
}

static void path_in(const char *dir, const char *name, char *out, size_t cap) {
    snprintf(out, cap, "%s/%s", dir, name);
}

/* Bounded copy of `dir` into the journal handle: the stored copy is what compact()
 * later renames through, so a silent truncation would act on the wrong path. */
static int set_dir(ava1_jnl_t *j, const char *dir) {
    int w = snprintf(j->dir, sizeof j->dir, "%s", dir);
    if (w < 0 || (size_t)w >= sizeof j->dir) return -ENAMETOOLONG;
    return 0;
}

/* tmp → fsync → rename → fsync(dir). The caller owns same-directory placement. */
static int write_file_atomic(const char *dir, const char *name, const uint8_t *a, size_t an,
                             const uint8_t *b, size_t bn) {
    char tmp[600], fin[600];
    int fd, rc;
    snprintf(tmp, sizeof tmp, "%s/%s.tmp", dir, name);
    path_in(dir, name, fin, sizeof fin);
    fd = open(tmp, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) return -errno;
    rc = write_all_fd(fd, a, an);
    if (rc == 0 && bn) rc = write_all_fd(fd, b, bn);
    if (rc == 0 && fsync(fd) != 0) rc = -errno;
    close(fd);
    if (rc == 0 && rename(tmp, fin) != 0) rc = -errno; /* same directory */
    if (rc == 0) rc = sync_dir(dir);
    return rc;
}

int ava1_jnl_create(ava1_jnl_t *j, const char *dir, const ava1_jnl_open_t *o) {
    uint8_t body[1200];
    ava1_w_t w;
    uint8_t *rec;
    size_t rn;
    int rc;
    char p[600];
    memset(j, 0, sizeof *j);
    j->fd = -1;
    rc = set_dir(j, dir);
    if (rc != 0) return rc;
    if (mkdir(dir, 0755) != 0 && errno != EEXIST) return -errno; /* parent must exist */
    ava1_w_init(&w, body, sizeof body);
    if (ava1_jnl_open_encode(o, &w) != 0) return AVA1_E_SPACE;
    rec = frame_rec(AVA1_JNL_OPEN, body, w.len, &rn);
    if (!rec) return -ENOMEM;
    rc = write_file_atomic(dir, "journal", MAGIC, sizeof MAGIC, rec, rn);
    free(rec);
    if (rc != 0) return rc;
    path_in(dir, "journal", p, sizeof p);
    j->fd = open(p, O_WRONLY | O_APPEND);
    if (j->fd < 0) return -errno;
    j->len = sizeof MAGIC + rn;
    return 0;
}

int ava1_jnl_open(ava1_jnl_t *j, const char *dir, ava1_jnl_visit_fn visit, void *ctx) {
    char p[600];
    struct stat st;
    uint8_t *b;
    size_t at = sizeof MAGIC, n;
    int fd;
    memset(j, 0, sizeof *j);
    j->fd = -1;
    {
        int rc = set_dir(j, dir);
        if (rc != 0) return rc;
    }
    path_in(dir, "journal", p, sizeof p);
    fd = open(p, O_RDWR);
    if (fd < 0) return -errno;
    if (fstat(fd, &st) != 0) {
        int e = errno;
        close(fd);
        return -e;
    }
    n = (size_t)st.st_size;
    b = malloc(n ? n : 1);
    if (!b) {
        close(fd);
        return -ENOMEM;
    }
    if (pread(fd, b, n, 0) != (ssize_t)n || n < sizeof MAGIC ||
        memcmp(b, MAGIC, sizeof MAGIC) != 0) {
        free(b);
        close(fd);
        return AVA1_E_PROTO;
    }
    while (n - at >= 8) {
        uint32_t len = get32(b + at);
        /* A record is 8 + len bytes. `n - at >= 8` makes the subtraction safe, and
         * comparing against the remaining bytes (rather than len + 8, which wraps on a
         * 32-bit host) keeps a hostile length from walking off the buffer. */
        if (len == 0 || len > n - at - 8) break;
        if (ava1_crc32c(b + at + 4, len) != get32(b + at + 4 + len)) break;
        if (visit && visit(ctx, b[at + 4], b + at + 5, len - 1) != 0) break;
        at += 8 + len;
    }
    free(b);
    if (ftruncate(fd, (off_t)at) != 0 || fsync(fd) != 0) {
        int e = errno;
        close(fd);
        return -e;
    }
    close(fd);
    j->fd = open(p, O_WRONLY | O_APPEND);
    if (j->fd < 0) return -errno;
    j->len = at;
    return 0;
}

int ava1_jnl_append(ava1_jnl_t *j, uint8_t kind, const uint8_t *body, size_t len) {
    size_t rn;
    uint8_t *rec = frame_rec(kind, body, len, &rn);
    int rc;
    if (!rec) return -ENOMEM;
    rc = write_all_fd(j->fd, rec, rn);
    free(rec);
    if (rc == 0 && fsync(j->fd) != 0) rc = -errno;
    if (rc == 0) j->len += rn;
    return rc;
}

/* The snapshot carries done/ranges/roots but not the job's terminal status, so the
 * Done body is written back after it (NULL when the job is unfinished) — a compaction
 * must never lose state (SPEC.md §14.2). */
int ava1_jnl_compact(ava1_jnl_t *j, const uint8_t *open_body, size_t open_len,
                     const uint8_t *snap_body, size_t snap_len,
                     const uint8_t *done_body, size_t done_len) {
    size_t an, bn, dn = 0;
    uint8_t *a = frame_rec(AVA1_JNL_OPEN, open_body, open_len, &an);
    uint8_t *b = frame_rec(AVA1_JNL_SNAPSHOT, snap_body, snap_len, &bn);
    uint8_t *d = done_body ? frame_rec(AVA1_JNL_DONE, done_body, done_len, &dn) : NULL;
    size_t total = sizeof MAGIC + an + bn + dn;
    uint8_t *all = (a && b && (d || !done_body)) ? malloc(total) : NULL;
    char p[600];
    int rc = -ENOMEM;
    if (all) {
        memcpy(all, MAGIC, sizeof MAGIC);
        memcpy(all + sizeof MAGIC, a, an);
        memcpy(all + sizeof MAGIC + an, b, bn);
        if (d) memcpy(all + sizeof MAGIC + an + bn, d, dn);
        rc = write_file_atomic(j->dir, "journal", all, total, NULL, 0);
    }
    if (rc == 0) {
        close(j->fd);
        path_in(j->dir, "journal", p, sizeof p);
        j->fd = open(p, O_WRONLY | O_APPEND);
        if (j->fd < 0) rc = -errno;
        else j->len = total;
    }
    free(a);
    free(b);
    free(d);
    free(all);
    return rc;
}

void ava1_jnl_close(ava1_jnl_t *j) {
    if (j->fd >= 0) close(j->fd);
    j->fd = -1;
}

int ava1_manifest_file_write(const char *dir, const uint8_t *blob, size_t len) {
    return write_file_atomic(dir, "manifest", blob, len, NULL, 0);
}

int ava1_manifest_file_read(const char *dir, uint8_t **blob, size_t *len) {
    char p[600];
    struct stat st;
    int fd;
    path_in(dir, "manifest", p, sizeof p);
    fd = open(p, O_RDONLY);
    if (fd < 0) return -errno;
    if (fstat(fd, &st) != 0) {
        int e = errno;
        close(fd);
        return -e;
    }
    *len = (size_t)st.st_size;
    *blob = malloc(*len ? *len : 1);
    if (!*blob || pread(fd, *blob, *len, 0) != (ssize_t)*len) {
        free(*blob);
        *blob = NULL;
        close(fd);
        return AVA1_E_IO;
    }
    close(fd);
    return 0;
}

void ava1_job_dir(const char *jobs_dir, const uint8_t job_id[16], char *out, size_t cap) {
    static const char H[] = "0123456789abcdef";
    char hex[33];
    int i;
    for (i = 0; i < 16; i++) {
        hex[2 * i] = H[job_id[i] >> 4];
        hex[2 * i + 1] = H[job_id[i] & 15];
    }
    hex[32] = 0;
    snprintf(out, cap, "%s/%s", jobs_dir, hex);
}

static int rm_tree(const char *p) {
    DIR *d;
    struct dirent *e;
    struct stat st;
    char q[1100];
    /* lstat, not stat: a symlink is removed, never followed — Rust's remove_dir_all has
     * the same rule, and following one here would delete a tree outside the job dir. */
    if (lstat(p, &st) != 0) return -errno;
    if (!S_ISDIR(st.st_mode)) return unlink(p) == 0 ? 0 : -errno;
    d = opendir(p);
    if (!d) return -errno;
    while ((e = readdir(d)) != NULL) {
        if (strcmp(e->d_name, ".") == 0 || strcmp(e->d_name, "..") == 0) continue;
        snprintf(q, sizeof q, "%s/%s", p, e->d_name);
        (void)rm_tree(q);
    }
    closedir(d);
    return rmdir(p) == 0 ? 0 : -errno;
}

int ava1_jobs_gc(const char *jobs_dir, int64_t now_unix, int64_t max_age_s) {
    DIR *d = opendir(jobs_dir);
    struct dirent *e;
    int n = 0;
    if (!d) return errno == ENOENT ? 0 : -errno;
    while ((e = readdir(d)) != NULL) {
        char p[700], jp[720];
        struct stat st, js;
        int64_t last;
        if (e->d_name[0] == '.') continue;
        snprintf(p, sizeof p, "%s/%s", jobs_dir, e->d_name);
        if (stat(p, &st) != 0 || !S_ISDIR(st.st_mode)) continue;
        last = (int64_t)st.st_mtime;
        snprintf(jp, sizeof jp, "%s/journal", p);
        if (stat(jp, &js) == 0 && (int64_t)js.st_mtime > last) last = (int64_t)js.st_mtime;
        if (now_unix - last > max_age_s && rm_tree(p) == 0) n++;
    }
    closedir(d);
    return n;
}
