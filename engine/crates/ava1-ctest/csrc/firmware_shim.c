/* The payload's firmware-string parser (payload/include/ps5_firmware.h), for host tests. */
#include <stddef.h>

#include "ps5_firmware.h"

void ava1_test_firmware(const char *kernel_version, char *out, size_t cap) {
    ps5_firmware_from_kernel(kernel_version, out, cap);
}
