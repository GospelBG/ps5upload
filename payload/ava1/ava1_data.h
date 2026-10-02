/* AVA1 data-layer config, credit budget and housekeeping (SPEC.md §13, §15). */
#ifndef AVA1_DATA_H
#define AVA1_DATA_H

#include <stdint.h>

typedef struct {
    char jobs_dir[256];
    uint64_t budget;                      /* credit across all jobs; 0 = 96 MiB */
    uint8_t workers_start, workers_min, workers_max; /* 0 = 4, 2, 16 */
    uint32_t cutoff;                      /* small/large; 0 = 256 KiB (informational: the sender decides) */
    int (*may_write)(const char *abs);    /* 1 = allowed */
    int (*may_read)(const char *abs, int unsafe_read);
    int (*same_device)(const char *a, const char *b); /* 1 same, 0 crosses, -1 unknown */
    uint32_t fsync_delay_us;              /* tests: a slow disk */
    int crash_at;                         /* tests: AVA1_CRASH_* (Task 13) */
} ava1_data_cfg_t;

int ava1_data_start(const ava1_data_cfg_t *cfg);  /* starts housekeeping; 0 or -errno */
void ava1_data_stop(void);                         /* stops and frees every job */
const ava1_data_cfg_t *ava1_data_cfg(void);
uint64_t ava1_budget_take(uint64_t want, uint64_t min); /* grants up to want, 0 if < min free */
void ava1_budget_give(uint64_t n);

#endif
