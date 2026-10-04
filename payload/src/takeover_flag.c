/* Flag-file takeover between AVA1-era instances: see include/takeover_flag.h. */
#include "takeover_flag.h"

#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

#define FLAG_NAME "takeover"
#define FLAG_TMP "takeover.tmp"

static int flag_path(char *out, size_t cap, const char *dir, const char *name) {
    int n = snprintf(out, cap, "%s/%s", dir, name);
    return (n < 0 || (size_t)n >= cap) ? -1 : 0;
}

int takeover_flag_write(const char *dir, uint64_t instance_id) {
    char tmp[300], dst[300], body[32];
    if (!dir || flag_path(tmp, sizeof tmp, dir, FLAG_TMP) || flag_path(dst, sizeof dst, dir, FLAG_NAME))
        return -1;
    int n = snprintf(body, sizeof body, "%llu\n", (unsigned long long)instance_id);
    int fd = open(tmp, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) return -1;
    int ok = write(fd, body, (size_t)n) == n;
    ok = (close(fd) == 0) && ok;
    /* Same directory, so never a cross-mount rename. */
    if (!ok || rename(tmp, dst) != 0) {
        (void)unlink(tmp);
        return -1;
    }
    return 0;
}

int takeover_flag_read(const char *dir, uint64_t *instance_id) {
    char path[300], body[32];
    if (!dir || !instance_id || flag_path(path, sizeof path, dir, FLAG_NAME)) return -1;
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -1;
    ssize_t n = read(fd, body, sizeof body - 1);
    close(fd);
    if (n <= 0) return -1;
    body[n] = '\0';
    char *end = NULL;
    errno = 0;
    unsigned long long v = strtoull(body, &end, 10);
    if (errno != 0 || end == body || v == 0) return -1;
    *instance_id = (uint64_t)v;
    return 0;
}

int takeover_flag_newer(const char *dir, uint64_t my_id) {
    uint64_t id = 0;
    return takeover_flag_read(dir, &id) == 0 && id > my_id;
}

int takeover_port_responding(int port) {
    struct sockaddr_in a;
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) return 0;
    memset(&a, 0, sizeof a);
    a.sin_family = AF_INET;
    a.sin_port = htons((uint16_t)port);
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    int up = connect(fd, (struct sockaddr *)&a, sizeof a) == 0;
    close(fd);
    return up;
}

int takeover_flag_request(const char *dir, uint64_t my_id, const int *ports, int nports,
                          int attempts, int interval_us) {
    if (takeover_flag_write(dir, my_id) != 0) return -1;
    for (int i = 0; i < attempts; i++) {
        int busy = 0;
        for (int p = 0; p < nports; p++) busy |= takeover_port_responding(ports[p]);
        if (!busy) return 0;
        usleep((useconds_t)interval_us);
    }
    return -1;
}

typedef struct {
    char dir[240];
    uint64_t my_id;
    int period_ms;
    void (*on_newer)(void);
} poll_ctx_t;

static void *poll_thread(void *arg) {
    poll_ctx_t c = *(poll_ctx_t *)arg;
    free(arg);
    for (;;) {
        struct timespec ts = {c.period_ms / 1000, (long)(c.period_ms % 1000) * 1000000L};
        nanosleep(&ts, NULL);
        if (takeover_flag_newer(c.dir, c.my_id)) {
            c.on_newer();
            return NULL;
        }
    }
}

int takeover_flag_poll_start(const char *dir, uint64_t my_id, int period_ms, void (*on_newer)(void)) {
    if (!dir || !on_newer || period_ms <= 0 || strlen(dir) >= sizeof(((poll_ctx_t *)0)->dir)) return -1;
    poll_ctx_t *c = calloc(1, sizeof *c);
    if (!c) return -1;
    memcpy(c->dir, dir, strlen(dir) + 1);
    c->my_id = my_id;
    c->period_ms = period_ms;
    c->on_newer = on_newer;
    pthread_t t;
    pthread_attr_t a;
    if (pthread_attr_init(&a) != 0) {
        free(c);
        return -1;
    }
    (void)pthread_attr_setdetachstate(&a, PTHREAD_CREATE_DETACHED);
    int rc = pthread_create(&t, &a, poll_thread, c);
    pthread_attr_destroy(&a);
    if (rc != 0) {
        free(c);
        return -1;
    }
    return 0;
}
