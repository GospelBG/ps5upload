/* AVA1 jobs: the in-memory table every data-plane task drives (SPEC.md §13).
 * Task 11 wrote the struct; later tasks add their own fields (T13: replay_done,
 * replay_status, dest_held, ava1_lfile_t.dir_synced; T14 adds more). */
#ifndef AVA1_JOB_H
#define AVA1_JOB_H

#include <pthread.h>
#include <stddef.h>
#include <stdint.h>

#include "ava1_gen.h"
#include "ava1_journal.h"
#include "ava1_manifest.h"
#include "ava1_ranges.h"
#include "ava1_tune.h"

#define AVA1_MAX_JOBS 32
#define AVA1_PARK_MS (10u * 60u * 1000u)
/* JnlOpen.staged: bit 0 = staged under <root>.ava-part; bit 1 = the receiver created the
 * empty <root> as its lock (SPEC.md §11.6), so an empty <root> on resume is its own. */
#define AVA1_STAGED_HELD 2

typedef struct ava1_job ava1_job_t;
/* Where a job's outgoing messages go: a session (network jobs) or a recorder (local, tests). */
typedef void (*ava1_emit_fn)(ava1_job_t *j, uint8_t type, uint8_t flags, const uint8_t *body, size_t len);

typedef struct {        /* one large file being assembled */
    int fd, ob_fd;
    ava1_rset_t written;   /* pwritten, not yet synced */
    ava1_rset_t durable;   /* synced and journaled */
    int has_root, root_journaled;
    uint8_t root[32];
    int committed;
    int dir_synced;        /* its part file's directory entry is durable (Task 13) */
} ava1_lfile_t;

typedef struct ava1_work { /* a unit for the worker pool */
    struct ava1_work *next;
    uint8_t kind;          /* AVA1_W_CHUNK, AVA1_W_BUNDLE, AVA1_W_CALL */
    uint8_t *owned;        /* freed after apply; its size is returned as credit */
    size_t owned_len;
    uint32_t file_id;
    uint64_t offset;
    const uint8_t *data;
    size_t len;
    void (*fn)(ava1_job_t *j, void *arg, uint32_t i); /* CALL */
    void *arg;
    uint32_t i;
} ava1_work_t;

struct ava1_job {
    uint8_t id[16], owner[32], sid[16];
    int attached, refs, stopping, finished, prepared;
    uint64_t parked_at_ms;
    uint8_t kind, policy;
    uint32_t flags;
    int staged;
    int dest_held;                  /* staged and <root> is our empty lock folder (Task 13) */
    char root[AVA1_MAX_PATH + 1];   /* the job root as requested */
    char base[AVA1_MAX_PATH + 16];  /* where entries land: root, or root.ava-part */
    char dir[512];                  /* job directory (journal, manifest, outboards) */
    char src[AVA1_MAX_PATH + 1];    /* JOB_COPY source, JOB_DOWNLOAD root */
    ava1_mstore_t m;                /* the manifest; file ids index everything below */
    ava1_mstore_t m_in;             /* pages of a JobOpen still arriving (Task 13) */
    int have_manifest;
    int replay_done;                /* the journal ended with JnlDone (Task 13) */
    uint16_t replay_status;         /* ... and this status */
    uint8_t manifest_hash[32];
    ava1_jnl_t jnl;
    ava1_bits_t done;               /* committed (small: synced; large: renamed) */
    ava1_lfile_t **lf;              /* per file_id; NULL for small files and directories */
    ava1_file_range_t *last_ranges; /* the last journal batch's ranges (resume check, Task 13) */
    uint32_t last_ranges_n;
    uint64_t credit, outstanding;   /* granted; lane-frame bytes held */
    uint64_t credit_back;           /* freed, not yet returned to the sender */
    uint64_t bytes_received, bytes_durable;
    uint32_t files_done;
    pthread_mutex_t mu;
    pthread_cond_t cv;              /* workers wait here for work */
    ava1_work_t *q_head, *q_tail;   /* worker queue */
    uint32_t q_len, q_busy_ticks, ticks;
    uint32_t *pend_small;           /* small files written, waiting for a sync batch */
    int *pend_fd;
    uint32_t pend_n, pend_cap;
    uint32_t batch_max;             /* small files per sync batch (tuned, Task 12) */
    uint64_t last_batch_ms, unsynced_bytes;
    uint32_t roots_new;             /* FileRoots not yet journaled */
    uint8_t lanes;                  /* live lanes of the attached session (Task 14) */
    uint32_t tune_ticks, tune_busy; /* queue occupancy since the last tuning step */
    uint32_t calls_left;            /* outstanding AVA1_W_CALL items (ava1_apply_parallel) */
    int ev_end, ev_resume;          /* control events for the job thread (Task 13) */
    uint32_t end_files;
    uint64_t end_bytes;
    uint8_t end_hash[32];
    void (*on_events)(ava1_job_t *j);  /* set by the receiver (Task 13) */
    void (*on_tick)(ava1_job_t *j);    /* set by the sender roles (Tasks 18-19) */
    void *role;                     /* the sender state of JOB_DOWNLOAD / JOB_COPY */
    void (*role_free)(ava1_job_t *j);
    pthread_t thread;               /* the job thread: events, sync batches, commits, status */
    int thread_started;
    pthread_t workers[16];
    uint8_t nworkers, want_workers;
    uint32_t busy;                  /* workers applying right now */
    ava1_wtune_t tune;
    uint64_t tune_ms, status_ms;
    uint32_t applied_since_tune;
    ava1_emit_fn emit;
    void *emit_ctx;
    uint16_t final_status;
    char message[128];
};

ava1_job_t *ava1_job_find(const uint8_t id[16]); /* referenced, or NULL */
ava1_job_t *ava1_job_create(const uint8_t id[16], const uint8_t owner[32]); /* referenced; NULL when full */
void ava1_job_put(ava1_job_t *j);
void ava1_job_park_session(const uint8_t sid[16]); /* every job attached to sid detaches */
void ava1_job_reap(uint64_t now_ms);               /* frees parked jobs older than AVA1_PARK_MS */
void ava1_job_free_all(void);
void ava1_job_free_one(const uint8_t id[16]);      /* unlists it and drops the table's reference */
void ava1_job_emit(ava1_job_t *j, uint8_t type, uint8_t flags, const uint8_t *body, size_t len);

#endif
