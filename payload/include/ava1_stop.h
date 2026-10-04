#ifndef PS5UPLOAD2_AVA1_STOP_H
#define PS5UPLOAD2_AVA1_STOP_H

/* Result bits of ava1_payload_stop(). 0 = everything stopped cleanly. */
#define AVA1_STOP_CONNS_LEFT 1 /* a session was still open after the wait (its threads end with the process) */
#define AVA1_STOP_SONY_BUSY 2  /* a Sony call was still running after the wait */

/*
 * The AVA1 half of a payload exit (node.shutdown, the takeover flag, a signal-free main return):
 *   1. stop accepting and tell every session to end (ava1_server_stop);
 *   2. wait up to conn_wait_ms for the sessions to leave, so no handler starts a new call;
 *   3. wait up to sony_wait_ms for an in-flight Sony call to finish (sony_api_lock is free);
 *   4. stop the data layer: every job is stopped, its threads joined and its journal closed, so
 *      a durable job resumes after the next start with nothing lost.
 * Both waits use CLOCK_MONOTONIC and are bounded: the caller's exit watchdog is the last resort.
 */
int ava1_payload_stop(int conn_wait_ms, int sony_wait_ms);

/*
 * Runs fire(arg) on a detached thread after delay_ms (CLOCK_MONOTONIC-independent: a plain sleep).
 * node.shutdown uses it so the reply, which leaves only after the handler returns, is on the wire
 * before anything starts to stop. 0 on success; -1 when the thread could not start (the caller
 * then runs fire itself: a late reply is better than no shutdown).
 */
int ava1_shutdown_defer(int delay_ms, void (*fire)(void *), void *arg);

#endif
