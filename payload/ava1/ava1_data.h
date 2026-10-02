/* AVA1 data-layer config, credit budget and housekeeping (SPEC.md §13, §15). */
#ifndef AVA1_DATA_H
#define AVA1_DATA_H

#include <stdint.h>

#include "ava1_job.h"
#include "ava1_server.h"

typedef struct {
    char jobs_dir[256];
    uint64_t budget;                      /* credit across all jobs; 0 = 96 MiB */
    uint8_t workers_start, workers_min, workers_max; /* 0 = 4, 2, 16 */
    /* small/large (SPEC.md §12.2); 0 = 256 KiB. JobOpen carries no cutoff, so the sender
     * must use this one: a Chunk for a smaller file, or a BundleRecord for a file this
     * size or larger, ends the job with ERR_PROTOCOL. */
    uint32_t cutoff;
    int (*may_write)(const char *abs);    /* 1 = allowed */
    int (*may_read)(const char *abs, int unsafe_read);
    int (*same_device)(const char *a, const char *b); /* 1 same, 0 crosses, -1 unknown */
    uint32_t fsync_delay_us;              /* tests: a slow disk */
    int crash_at;                         /* tests: AVA1_CRASH_* (Task 13) */
    uint32_t park_ms;                     /* a parked job is freed after this; 0 = AVA1_PARK_MS */
} ava1_data_cfg_t;

int ava1_data_start(const ava1_data_cfg_t *cfg);  /* starts housekeeping; 0 or -errno */
void ava1_data_stop(void);                         /* stops and frees every job */
const ava1_data_cfg_t *ava1_data_cfg(void);
uint64_t ava1_budget_take(uint64_t want, uint64_t min); /* grants up to want, 0 if < min free */
void ava1_budget_give(uint64_t n);
/* Runs fn(arg) on a short-lived detached thread that ava1_data_stop waits for. 0 or -1. */
int ava1_data_spawn(void *(*fn)(void *), void *arg);

/* The server's data hooks (SPEC.md §11-§12): JobOpen and the job conversation on the
 * control connection, lane frames admitted against credit, Received before any disk work,
 * a closed session parking its jobs. Every hook only decodes, routes and queues: JobOpen's
 * work runs on a short-lived thread, a receiver job's frames on its feeder thread. */
const ava1_data_hooks_t *ava1_data_hooks(void);
/* Attaches a job to session `sid`: its messages go there (waiting sends), its lanes are
 * counted, frames held for an earlier session are dropped and lane frames wait for the
 * new session's map. 0, or -1 when the job is no longer listed. */
int ava1_job_attach(ava1_job_t *j, const uint8_t sid[16]);

/* Tests only (0 in the payload): JobOpen's work waits this long before it starts, and an
 * OK map this long before it is sent. */
extern uint32_t ava1_data_test_open_delay_ms;
extern uint32_t ava1_data_test_map_delay_ms;

#endif
