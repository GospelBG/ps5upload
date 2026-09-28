/* Host-side test for how a single-file upload's `.ps5up2-tmp` is opened.
 *
 * The bug this guards: a fresh single-file upload pre-sizes its tmp file to
 * the full transfer size (posix_fallocate), and a resumed one reopened it
 * with O_APPEND. Appending to a pre-sized file writes past its end, so a
 * 1 GiB upload that dropped after 384 MiB and resumed grew to
 * 1 GiB + 640 MiB and COMMIT refused it with size_mismatch (seen on the
 * Android emulator). A resume must write where the acknowledged bytes end.
 *
 * The test drives the real open helper on a real temp file. */
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#include "../include/direct_open.h"

static int failures = 0;

#define CHECK(expr)                                                     \
    do {                                                                \
        if (!(expr)) {                                                  \
            fprintf(stderr, "FAIL line %d: %s\n", __LINE__, #expr);     \
            failures++;                                                 \
        }                                                               \
    } while (0)

static long long file_size(const char *p) {
    struct stat st;
    return stat(p, &st) == 0 ? (long long)st.st_size : -1;
}

static void write_bytes(int fd, size_t n, unsigned char v) {
    unsigned char *b = malloc(n);
    memset(b, v, n);
    CHECK(write(fd, b, n) == (ssize_t)n);
    free(b);
}

int main(void) {
    /* The plan: a first shard truncates; a resume never appends and seeks
     * to the acknowledged byte count. */
    direct_open_plan_t first = direct_open_plan(1, 0);
    CHECK(first.truncate == 1);
    CHECK(first.offset == 0);
    CHECK((first.flags & O_APPEND) == 0);

    direct_open_plan_t resume = direct_open_plan(0, 384);
    CHECK(resume.truncate == 0);
    CHECK((resume.flags & O_APPEND) == 0);
    CHECK((resume.flags & O_TRUNC) == 0);
    CHECK(resume.offset == 384);

    /* The scenario on a real file: pre-sized to 1000, 384 written, the
     * connection drops, the resume sends the remaining 616. */
    char path[] = "/tmp/ps5upload-direct-open-XXXXXX";
    int tfd = mkstemp(path);
    CHECK(tfd >= 0);
    close(tfd);

    int fd = direct_open_for_shard(path, first);
    CHECK(fd >= 0);
    CHECK(ftruncate(fd, 1000) == 0); /* the pre-size step */
    write_bytes(fd, 384, 0xAA);
    close(fd);

    fd = direct_open_for_shard(path, direct_open_plan(0, 384));
    CHECK(fd >= 0);
    write_bytes(fd, 616, 0xBB);
    close(fd);

    CHECK(file_size(path) == 1000);

    /* The bytes landed in order: 384 of 0xAA, then 616 of 0xBB. */
    {
        unsigned char buf[1000];
        int rfd = open(path, O_RDONLY);
        CHECK(read(rfd, buf, sizeof buf) == (ssize_t)sizeof buf);
        close(rfd);
        CHECK(buf[0] == 0xAA && buf[383] == 0xAA);
        CHECK(buf[384] == 0xBB && buf[999] == 0xBB);
    }

    /* A resume whose tmp file is gone starts it again (create). */
    unlink(path);
    fd = direct_open_for_shard(path, direct_open_plan(0, 0));
    CHECK(fd >= 0);
    close(fd);
    unlink(path);

    if (failures) {
        fprintf(stderr, "%d failure(s)\n", failures);
        return 1;
    }
    return 0;
}
