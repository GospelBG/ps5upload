/* PS5 (FreeBSD 11 based): kern.arandom is the kernel CSPRNG, the source FreeBSD 11's
 * own arc4random uses. getentropy() arrived in FreeBSD 12. */
#include "ava1_platform.h"

#include <sys/types.h>
#include <sys/sysctl.h>
#include <unistd.h>

#ifndef KERN_ARND
#define KERN_ARND 37
#endif

int ava1_platform_random(uint8_t *buf, size_t n) {
    int mib[2] = { CTL_KERN, KERN_ARND };
    while (n > 0) {
        size_t want = n > 256 ? 256 : n, got = want;
        if (sysctl(mib, 2, buf, &got, NULL, 0) != 0 || got == 0) return -1;
        buf += got;
        n -= got;
    }
    return 0;
}

void ava1_platform_sleep_ms(unsigned ms) { usleep((useconds_t)ms * 1000u); }
