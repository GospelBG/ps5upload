#include "ava1_data.h"

#include <errno.h>
#include <pthread.h>
#include <string.h>

#include "ava1_job.h"
#include "ava1_platform.h"
#include "ava1_thread.h"

static struct {
    ava1_data_cfg_t cfg;
    pthread_mutex_t mu;
    uint64_t budget_free;
    volatile int running;
    pthread_t house;
} D = { .mu = PTHREAD_MUTEX_INITIALIZER };

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
    pthread_join(D.house, NULL);
    ava1_job_free_all();
}
