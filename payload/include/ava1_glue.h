#ifndef PS5UPLOAD_AVA1_GLUE_H
#define PS5UPLOAD_AVA1_GLUE_H

/* Starts the AVA1 server on 9120. 0 on success; FTX2 keeps running either way. */
int ava1_payload_start(void);

/* "starting", "up" or "failed": the AVA1 server's state, reported in the old Hello reply. */
const char *ava1_payload_state(void);

#endif
