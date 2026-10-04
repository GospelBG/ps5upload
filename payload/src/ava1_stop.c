/* The AVA1 half of a payload exit: see include/ava1_stop.h. */
#include "ava1_stop.h"

#include <pthread.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>

#include "ava1_data.h"
#include "ava1_server.h"
#include "sony_api_lock.h"

static long long mono_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (long long)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

int ava1_payload_stop(int conn_wait_ms, int sony_wait_ms) {
    int rc = 0;
    long long end;

    ava1_server_stop();
    end = mono_ms() + conn_wait_ms;
    while (ava1_server_conns() > 0 && mono_ms() < end) usleep(10000);
    if (ava1_server_conns() > 0) rc |= AVA1_STOP_CONNS_LEFT;

    /* No new SESSION can start a call now (accept is closed and the sessions were told to end), but
     * the legacy ports stay open until main.c closes them after this returns, so this check is a
     * point in time, not a barrier. Taking the lock proves the last Sony call returned.
     *
     * A call that outlives `sony_wait_ms` is reported (AVA1_STOP_SONY_BUSY) but NOT abandoned: the
     * caller must not return, and the process must not exit, while a worker is inside a Sony call
     * (a cut call can wedge the console). So keep polling until the lock frees. The only other way
     * out is the exit watchdog main.c armed (runtime_arm_shutdown_watchdog, 8 s), which ends the
     * process itself. */
    end = mono_ms() + sony_wait_ms;
    for (;;) {
        if (pthread_mutex_trylock(&sony_api_lock) == 0) {
            pthread_mutex_unlock(&sony_api_lock);
            break;
        }
        if (mono_ms() >= end) rc |= AVA1_STOP_SONY_BUSY;
        usleep(10000);
    }

    ava1_data_stop();
    return rc;
}

typedef struct {
    int delay_ms;
    void (*fire)(void *);
    void *arg;
} defer_t;

static void *defer_main(void *p) {
    defer_t d = *(defer_t *)p;
    free(p);
    usleep((useconds_t)d.delay_ms * 1000u);
    d.fire(d.arg);
    return NULL;
}

int ava1_shutdown_defer(int delay_ms, void (*fire)(void *), void *arg) {
    pthread_t t;
    pthread_attr_t a;
    int rc;
    defer_t *d = malloc(sizeof *d);
    if (!d || !fire) {
        free(d);
        return -1;
    }
    d->delay_ms = delay_ms;
    d->fire = fire;
    d->arg = arg;
    if (pthread_attr_init(&a) != 0) {
        free(d);
        return -1;
    }
    (void)pthread_attr_setdetachstate(&a, PTHREAD_CREATE_DETACHED);
    rc = pthread_create(&t, &a, defer_main, d);
    pthread_attr_destroy(&a);
    if (rc != 0) {
        free(d);
        return -1;
    }
    return 0;
}
