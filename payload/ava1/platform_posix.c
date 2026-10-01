#include "ava1_platform.h"

#include <unistd.h>
#if defined(__APPLE__)
#include <sys/random.h>
#endif

int ava1_platform_random(uint8_t *buf, size_t n) {
    while (n > 0) {
        size_t k = n > 256 ? 256 : n;
        if (getentropy(buf, k) != 0) return -1;
        buf += k;
        n -= k;
    }
    return 0;
}

void ava1_platform_sleep_ms(unsigned ms) { usleep((useconds_t)ms * 1000u); }
