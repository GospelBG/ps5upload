/* AVA1 console-local copy and move (SPEC.md §13.5): a receiver job whose sender is the
 * in-process reader (Task 18). Only ava1_data_rpc opens one (it holds the open lock). */
#ifndef AVA1_COPY_H
#define AVA1_COPY_H

#include <stddef.h>
#include <stdint.h>

#include "ava1_gen.h"
#include "ava1_job.h"

/* Starts the copy, or returns the listed job (a re-issued job.copy resumes it). The
 * reference is the caller's to drop with ava1_job_put. NULL with *status set and `msg`
 * saying why on refusal. */
ava1_job_t *ava1_copy_open(const ava1_job_copy_t *c, const uint8_t owner[32],
                          ava1_mstore_t *prepared, uint16_t *status, char *msg, size_t cap);
/* Walks a new source before the data layer takes its global open lock. */
int ava1_copy_walk(const ava1_job_copy_t *c, ava1_mstore_t *out);
extern uint32_t ava1_copy_test_walk_delay_ms;
extern int ava1_copy_test_walk_active;
extern uint32_t ava1_copy_test_delete_delay_ms;
extern int ava1_copy_test_delete_active;

#endif
