/* Threads and clocks (SPEC.md §15's 256 KiB rule, Global Constraint 65). */
#ifndef AVA1_THREAD_H
#define AVA1_THREAD_H

#include <pthread.h>
#include <stdint.h>

#define AVA1_THREAD_STACK (256u * 1024u)

/* Starts fn(arg) on a 256 KiB stack. out == NULL: detached. 0 or -1. */
int ava1_thread_start(void *(*fn)(void *), void *arg, pthread_t *out);
uint64_t ava1_mono_ms(void);
uint64_t ava1_mono_us(void);

#endif
