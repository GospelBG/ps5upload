/* The 64-byte trust slot an engine stamps its key into when it sends this ELF (SPEC.md §5.1). */
#ifndef AVA1_TRUST_H
#define AVA1_TRUST_H

#include <stdint.h>

/* 0 and the stamped key when an engine stamped this ELF; -1 otherwise. */
int ava1_trust_slot_key(uint8_t out[32]);

#endif
