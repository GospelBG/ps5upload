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
/* Where a job's outgoing messages go: a session (network jobs) or a recorder (local, tests).
 * Called on job threads, workers and the data layer's own threads, never on a connection's
 * reader thread: it may wait for the socket (ava1_server_send, which serialises writers on
 * the connection's lock, so concurrent emitters are safe). Reader-thread hooks never emit;
 * what they send (Received, a refusal, Error) they post (ava1_server_post). */
typedef void (*ava1_emit_fn)(ava1_job_t *j, uint8_t type, uint8_t flags, const uint8_t *body, size_t len);

/* A frame waiting for a job (Task 14): a lane frame held until the job's map is out, or a
 * control frame for the job's feeder thread. `body` is malloc'd and owned. */
typedef struct ava1_inframe {
    struct ava1_inframe *next;
    uint8_t type;
    uint16_t lane;
    uint32_t seq;
    size_t len;
    uint8_t *body;
} ava1_inframe_t;

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
    /* ---- the wire (Task 14, ava1_data.c) ----
     * `cmu` is a leaf lock: held for a few field updates only, never across I/O and never
     * while taking another lock (j->mu, then cmu, is the only order). Reader threads take
     * cmu and never mu, so a job busy with its disk (mu is held across opens, fallocate and
     * journal compaction) cannot stall a session's reader. Under cmu: `attached` and `sid`
     * (also written under the table lock, so either lock reads them), the sender's credit
     * as this side counts it, the held lane frames and the control inbox. */
    pthread_mutex_t cmu;
    pthread_cond_t ccv;               /* the feeder waits here */
    int ready;                        /* the attached session has its map: lane frames go on */
    uint64_t w_avail;                 /* lane-frame bytes the sender may still send */
    uint64_t w_grant;                 /* the largest credit granted (admission's pre-read bound) */
    ava1_inframe_t *held_head, *held_tail; /* lane frames, in arrival order */
    ava1_inframe_t *in_head, *in_tail;     /* control frames, in arrival order */
    size_t in_bytes;
    int in_overflow;                  /* the inbox was full: the job ends with ERR_PROTOCOL */
    int feed_stop;
    pthread_t feeder;                 /* applies held frames and control frames (receiver jobs) */
    int feeder_started;
    /* Sender roles (Task 18): control frames from the receiving peer, and lane changes.
     * Both run on a reader thread (or under the table lock): they must not block. */
    int (*on_frame)(ava1_job_t *j, uint8_t type, const uint8_t *body, size_t len);
    void (*on_lane_change)(ava1_job_t *j, uint16_t lane, int up);
};

ava1_job_t *ava1_job_find(const uint8_t id[16]); /* referenced, or NULL */
/* Referenced; NULL when full. Created detached and stamped parked now, so a job nobody
 * attaches is collected like any parked one. */
ava1_job_t *ava1_job_create(const uint8_t id[16], const uint8_t owner[32]);
/* Like ava1_job_create, but attached to session `sid` before it is listed (NULL: detached). */
ava1_job_t *ava1_job_create_attached(const uint8_t id[16], const uint8_t owner[32], const uint8_t *sid);
/* Under the table lock: the listed job `id`, referenced and attached to `sid` (not parked);
 * NULL when it is not listed. Find and attach are one step, so the reaper cannot unlist a
 * job between them. */
ava1_job_t *ava1_job_find_attach(const uint8_t id[16], const uint8_t sid[16]);
/* The same for a job already in hand: 0, or -1 when it is no longer listed. */
int ava1_job_attach_sid(ava1_job_t *j, const uint8_t sid[16]);
void ava1_job_put(ava1_job_t *j);
/* For reader threads: drops a reference, never freeing the job on this thread (freeing
 * joins its threads); the last reference is dropped on a short-lived thread. */
void ava1_job_put_nowait(ava1_job_t *j);
/* fn(j, ctx) for every listed job, under the table lock: fn must not block or take it. */
void ava1_job_foreach(void (*fn)(ava1_job_t *j, void *ctx), void *ctx);
void ava1_job_park_session(const uint8_t sid[16]); /* every job attached to sid detaches */
/* Frees parked jobs older than the park age (data cfg park_ms; finished ones after 10 s). */
void ava1_job_reap(uint64_t now_ms);
void ava1_job_free_all(void);
void ava1_job_free_one(const uint8_t id[16]);      /* unlists it and drops the table's reference */
/* Takes the caller's reference. When only the table and the caller hold the job, unlists
 * it and frees it now (its threads joined, its files closed) and returns 0; otherwise drops
 * the caller's reference and returns -1 (someone else may still be running it). */
int ava1_job_retire(ava1_job_t *j);
void ava1_job_emit(ava1_job_t *j, uint8_t type, uint8_t flags, const uint8_t *body, size_t len);

#endif
