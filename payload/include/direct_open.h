/* How a single-file upload opens its `.ps5up2-tmp`.
 *
 * A fresh upload truncates the tmp file (the caller then pre-sizes it to the
 * full transfer with posix_fallocate). A resumed upload must write where the
 * acknowledged bytes end — NOT at end-of-file: the pre-sized file's end is
 * the full transfer size, so O_APPEND wrote past it (a 1 GiB upload that
 * resumed after 384 MiB grew to 1 GiB + 640 MiB and COMMIT refused it with
 * size_mismatch). Positional writes are also idempotent: a shard re-sent
 * after a drop overwrites the same bytes instead of duplicating them.
 *
 * Header-only so the host self-test drives exactly this code. */
#ifndef PS5UPLOAD_DIRECT_OPEN_H
#define PS5UPLOAD_DIRECT_OPEN_H

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <sys/types.h>
#include <unistd.h>

typedef struct {
    int flags;
    /* Fresh file: the caller may pre-size it after opening. */
    int truncate;
    /* Where the next shard's bytes go. */
    uint64_t offset;
} direct_open_plan_t;

/* `bytes_acked` is the tx's acknowledged byte count (entry->bytes_received,
 * persisted in the journal), i.e. the byte offset of the next shard. */
static inline direct_open_plan_t direct_open_plan(int is_first_shard, uint64_t bytes_acked) {
    direct_open_plan_t p;
    if (is_first_shard) {
        p.flags = O_WRONLY | O_CREAT | O_TRUNC;
        p.truncate = 1;
        p.offset = 0;
    } else {
        p.flags = O_WRONLY | O_CREAT;
        p.truncate = 0;
        p.offset = bytes_acked;
    }
    return p;
}

/* Open per `p` and position the fd at `p.offset`. Returns the fd, or -1 with
 * errno set (the fd is closed if the seek fails). */
static inline int direct_open_for_shard(const char *path, direct_open_plan_t p) {
    int fd = open(path, p.flags, 0777);
    if (fd < 0) return -1;
    if (p.offset > 0 && lseek(fd, (off_t)p.offset, SEEK_SET) < 0) {
        int e = errno;
        close(fd);
        errno = e;
        return -1;
    }
    return fd;
}

#endif
