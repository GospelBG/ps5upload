/* What AVA1 needs from the OS (SPEC.md §4). PS5: platform_ps5.c; host: platform_posix.c. */
#ifndef AVA1_PLATFORM_H
#define AVA1_PLATFORM_H

#include <stddef.h>
#include <stdint.h>

/* Fill buf from the OS's secure random source. 0 on success, -1 on failure. */
int ava1_platform_random(uint8_t *buf, size_t n);
void ava1_platform_sleep_ms(unsigned ms);

#endif
