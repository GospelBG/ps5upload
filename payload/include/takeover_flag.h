#ifndef PS5UPLOAD2_TAKEOVER_FLAG_H
#define PS5UPLOAD2_TAKEOVER_FLAG_H

#include <stdint.h>

/*
 * Takeover between two AVA1-era instances (and the loopback helpers both takeover paths share).
 *
 * The new instance writes <dir>/takeover naming its own instance id (instance ids start with the
 * start time in seconds, so a newer instance has a larger id). The old instance polls the file
 * once a second and shuts itself down when it names a LARGER id than its own. A flag that names
 * the same or an older instance (a leftover from before a reboot, or the new instance's own)
 * changes nothing. The file is local: nothing on the network can write it.
 */

/* Writes the flag (a temporary file in <dir>, then an in-directory rename). 0 on success. */
int takeover_flag_write(const char *dir, uint64_t instance_id);

/* Reads the instance id the flag names. 0 on success; -1 when absent or unreadable. */
int takeover_flag_read(const char *dir, uint64_t *instance_id);

/* 1 when the flag names an instance newer than `my_id`. */
int takeover_flag_newer(const char *dir, uint64_t my_id);

/* 1 when something accepts a TCP connection on 127.0.0.1:port. */
int takeover_port_responding(int port);

/* Writes the flag, then waits until none of `ports` answers on loopback: `attempts` checks,
 * `interval_us` apart (CLOCK_MONOTONIC is not needed: the loop counts checks, not time).
 * 0 when every port is free, -1 when one still answers after the last check. */
int takeover_flag_request(const char *dir, uint64_t my_id, const int *ports, int nports,
                          int attempts, int interval_us);

/* Starts a detached thread that checks the flag every `period_ms` and calls `on_newer` once when a
 * newer instance asked this one to exit, then ends. 0 on success. */
int takeover_flag_poll_start(const char *dir, uint64_t my_id, int period_ms, void (*on_newer)(void));

#endif
