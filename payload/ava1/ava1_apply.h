/* AVA1 apply engine (SPEC.md §12.5, §12.6, §13, §14): the receiver's work after the map.
 * Chunks and bundles go to a worker pool; the job thread batches fsyncs, journals them,
 * acknowledges Durable, commits large files whose root matches, and finishes the job. */
#ifndef AVA1_APPLY_H
#define AVA1_APPLY_H

#include <stddef.h>
#include <stdint.h>

#include "ava1_gen.h"
#include "ava1_job.h"

#define AVA1_W_CHUNK 1
#define AVA1_W_BUNDLE 2
#define AVA1_W_CALL 3
#define AVA1_PEND_MAX 512u          /* small-file fds held open until their batch */
#define AVA1_CRASH_AFTER_SYNC 1     /* tests: die after fsync, before the journal */
#define AVA1_CRASH_AFTER_JOURNAL 2  /* tests: die after the journal, before Durable */

/* `owned` buffers are freed by the apply engine (always, including on error); their
 * length returns to the sender as credit. */
int ava1_apply_start(ava1_job_t *j);                    /* job thread + workers_start workers */
int ava1_apply_reserve(ava1_job_t *j, size_t n);       /* credit: 0, or -1 when it would exceed */
void ava1_apply_unreserve(ava1_job_t *j, size_t n);
/* AVA1_E_PROTO unless the chunk is inside a file and group-aligned: every chunk but a
 * file's final one is a whole number of verification groups. */
int ava1_apply_chunk(ava1_job_t *j, uint8_t *owned, size_t owned_len, uint32_t file_id, uint64_t off,
                     const uint8_t *data, size_t len);
int ava1_apply_bundle(ava1_job_t *j, uint8_t *owned, size_t owned_len, const ava1_bundle_t *b);
int ava1_apply_root(ava1_job_t *j, uint32_t file_id, const uint8_t root[32]);
/* Runs fn(j, arg, 0..n-1) on the workers and waits for all of them. */
int ava1_apply_parallel(ava1_job_t *j, void (*fn)(ava1_job_t *, void *, uint32_t), void *arg, uint32_t n);
void ava1_apply_path(const ava1_job_t *j, uint32_t id, int part, char *out, size_t cap);
/* Ends the job once (also the success path, status AVA1_STATUS_OK): journal Done when
 * asked, then JobDone. */
void ava1_apply_fail(ava1_job_t *j, uint16_t status, const char *what, int err, int journal_done);
void ava1_apply_status(ava1_job_t *j);                  /* emit one Status now */
/* Forget a large file's progress (journal Reset, clear its ranges and root, blank its
 * outboard). Requires j->lf[id] != NULL. `reason == 0` resets silently (no FileRetry). */
void ava1_apply_reset(ava1_job_t *j, uint32_t id, uint16_t reason);
/* Rewrite the journal as Open + Snapshot of the job's state (mid-job: no Done). */
void ava1_apply_compact(ava1_job_t *j);

/* Tests only (NULL in the payload): called at these points of a commit, in this order.
 * `id` is the file, or UINT32_MAX for the staged tree's rename. */
#define AVA1_HOOK_COMMIT_VERIFIED 1 /* root matched; the commit is about to proceed */
#define AVA1_HOOK_RENAMED 2         /* the rename into place */
#define AVA1_HOOK_DIR_SYNCED 3      /* its directory fsynced */
#define AVA1_HOOK_JOURNALED 4       /* the file's done Batch appended */
#define AVA1_HOOK_OB_UNLINKED 5     /* its outboard removed */
extern void (*ava1_apply_hook)(ava1_job_t *j, int point, uint32_t id);

#endif
