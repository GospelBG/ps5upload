#include "ava1_job.h"

#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include "ava1_data.h"
#include "ava1_thread.h"

static struct {
    pthread_mutex_t mu;
    ava1_job_t *jobs[AVA1_MAX_JOBS];
} T = { .mu = PTHREAD_MUTEX_INITIALIZER };

ava1_job_t *ava1_job_find(const uint8_t id[16]) {
    ava1_job_t *j = NULL;
    int i;
    pthread_mutex_lock(&T.mu);
    for (i = 0; i < AVA1_MAX_JOBS; i++)
        if (T.jobs[i] && memcmp(T.jobs[i]->id, id, 16) == 0) {
            j = T.jobs[i];
            j->refs++;
            break;
        }
    pthread_mutex_unlock(&T.mu);
    return j;
}

ava1_job_t *ava1_job_create(const uint8_t id[16], const uint8_t owner[32]) {
    ava1_job_t *j = calloc(1, sizeof *j);
    int i, slot = -1;
    if (!j) return NULL;
    memcpy(j->id, id, 16);
    memcpy(j->owner, owner, 32);
    j->refs = 2; /* the table's and the caller's */
    j->jnl.fd = -1;
    pthread_mutex_init(&j->mu, NULL);
    pthread_cond_init(&j->cv, NULL);
    pthread_mutex_lock(&T.mu);
    for (i = 0; i < AVA1_MAX_JOBS; i++) {
        if (T.jobs[i] && memcmp(T.jobs[i]->id, id, 16) == 0) {
            slot = -2; /* raced with another create */
            break;
        }
        if (!T.jobs[i] && slot == -1) slot = i;
    }
    if (slot >= 0) T.jobs[slot] = j;
    pthread_mutex_unlock(&T.mu);
    if (slot < 0) {
        pthread_mutex_destroy(&j->mu);
        pthread_cond_destroy(&j->cv);
        free(j);
        return NULL;
    }
    return j;
}

/* Stops threads, closes files, returns credit. Journal and job directory stay. */
static void job_destroy(ava1_job_t *j) {
    uint32_t i;
    ava1_work_t *w;
    pthread_mutex_lock(&j->mu);
    j->stopping = 1;
    pthread_cond_broadcast(&j->cv);
    pthread_mutex_unlock(&j->mu);
    if (j->thread_started) pthread_join(j->thread, NULL);
    for (i = 0; i < j->nworkers; i++) pthread_join(j->workers[i], NULL);
    while ((w = j->q_head) != NULL) {
        j->q_head = w->next;
        free(w->owned);
        free(w);
    }
    for (i = 0; i < j->pend_n; i++) close(j->pend_fd[i]);
    free(j->pend_small);
    free(j->pend_fd);
    free(j->last_ranges);
    ava1_mstore_free(&j->m_in);
    if (j->lf) {
        for (i = 0; i < j->m.n; i++)
            if (j->lf[i]) {
                if (j->lf[i]->fd >= 0) close(j->lf[i]->fd);
                if (j->lf[i]->ob_fd >= 0) close(j->lf[i]->ob_fd);
                ava1_rset_clear(&j->lf[i]->written);
                ava1_rset_clear(&j->lf[i]->durable);
                free(j->lf[i]);
            }
        free(j->lf);
    }
    ava1_jnl_close(&j->jnl);
    ava1_bits_free(&j->done);
    if (j->role_free) j->role_free(j);
    ava1_mstore_free(&j->m);
    if (j->credit) ava1_budget_give(j->credit);
    pthread_mutex_destroy(&j->mu);
    pthread_cond_destroy(&j->cv);
    free(j);
}

void ava1_job_put(ava1_job_t *j) {
    int last;
    pthread_mutex_lock(&T.mu);
    last = --j->refs == 0;
    pthread_mutex_unlock(&T.mu);
    if (last) job_destroy(j);
}

static void unlist(ava1_job_t *j) {
    int i;
    for (i = 0; i < AVA1_MAX_JOBS; i++)
        if (T.jobs[i] == j) T.jobs[i] = NULL;
}

void ava1_job_park_session(const uint8_t sid[16]) {
    int i;
    uint64_t now = ava1_mono_ms();
    pthread_mutex_lock(&T.mu);
    for (i = 0; i < AVA1_MAX_JOBS; i++) {
        ava1_job_t *j = T.jobs[i];
        if (j && j->attached && memcmp(j->sid, sid, 16) == 0) {
            j->attached = 0;
            j->parked_at_ms = now;
        }
    }
    pthread_mutex_unlock(&T.mu);
}

void ava1_job_reap(uint64_t now_ms) {
    ava1_job_t *gone[AVA1_MAX_JOBS];
    int i, n = 0;
    pthread_mutex_lock(&T.mu);
    for (i = 0; i < AVA1_MAX_JOBS; i++) {
        ava1_job_t *j = T.jobs[i];
        /* Only a job that was actually parked can be collected: `parked_at_ms == 0` means
         * the job is still being set up (between create and attach) or is driven by a test
         * with no session, and monotonic uptime must not age it. A local job (JOB_COPY) is
         * unlisted by Task 19, never here. */
        if (j && !j->attached && j->parked_at_ms != 0 &&
            (now_ms - j->parked_at_ms > AVA1_PARK_MS ||
             (j->finished && now_ms - j->parked_at_ms > 10000u))) {
            gone[n++] = j;
            unlist(j);
        }
    }
    pthread_mutex_unlock(&T.mu);
    for (i = 0; i < n; i++) ava1_job_put(gone[i]); /* the table's reference */
}

void ava1_job_free_all(void) {
    ava1_job_t *gone[AVA1_MAX_JOBS];
    int i, n = 0;
    pthread_mutex_lock(&T.mu);
    for (i = 0; i < AVA1_MAX_JOBS; i++)
        if (T.jobs[i]) {
            gone[n++] = T.jobs[i];
            T.jobs[i] = NULL;
        }
    pthread_mutex_unlock(&T.mu);
    for (i = 0; i < n; i++) ava1_job_put(gone[i]);
}

void ava1_job_free_one(const uint8_t id[16]) {
    ava1_job_t *j = NULL;
    int i;
    pthread_mutex_lock(&T.mu);
    for (i = 0; i < AVA1_MAX_JOBS; i++)
        if (T.jobs[i] && memcmp(T.jobs[i]->id, id, 16) == 0) {
            j = T.jobs[i];
            T.jobs[i] = NULL;
            break;
        }
    pthread_mutex_unlock(&T.mu);
    if (j) ava1_job_put(j);
}

void ava1_job_emit(ava1_job_t *j, uint8_t type, uint8_t flags, const uint8_t *body, size_t len) {
    if (j->emit) j->emit(j, type, flags, body, len);
}
