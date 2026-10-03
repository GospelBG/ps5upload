/* Included by runtime.c after the management handlers: wrappers and the table built from
 * mgmt_table.def, the per-call environment, and runtime_mgmt_install(). */

#define MGMT_H0(m, f, a, fl, h, helper)                                                                    \
    static int mgmt_l_##m(void *st, int fd, uint64_t t, const char *b, uint64_t l) {                       \
        (void)b;                                                                                           \
        (void)l;                                                                                           \
        return h((runtime_state_t *)st, fd, t);                                                            \
    }                                                                                                      \
    static int mgmt_r_##m(const uint8_t *q, uint32_t n, mgmt_ctx_t *cx) { return helper(q, n, cx, mgmt_l_##m); }
#define MGMT_H1(m, f, a, fl, h, helper)                                                                    \
    static int mgmt_l_##m(void *st, int fd, uint64_t t, const char *b, uint64_t l) {                       \
        return h((runtime_state_t *)st, fd, t, b, l);                                                      \
    }                                                                                                      \
    static int mgmt_r_##m(const uint8_t *q, uint32_t n, mgmt_ctx_t *cx) { return helper(q, n, cx, mgmt_l_##m); }
#include "mgmt_table.def"
#undef MGMT_H0
#undef MGMT_H1

#define MGMT_H0(m, f, a, fl, h, helper) {(uint16_t)(m), (uint16_t)(f), (uint16_t)(a), (fl), mgmt_r_##m},
#define MGMT_H1(m, f, a, fl, h, helper) {(uint16_t)(m), (uint16_t)(f), (uint16_t)(a), (fl), mgmt_r_##m},
static const mgmt_entry_t k_mgmt_table[] = {
#include "mgmt_table.def"
};
#undef MGMT_H0
#undef MGMT_H1

_Static_assert(FTX2_FRAME_ERROR == 3u, "mgmt_rpc.c's MGMT_FRAME_ERROR must match FTX2_FRAME_ERROR");

static runtime_state_t *g_mgmt_state;

/* What the FTX2 management thread did around every frame (handle_binary_frame_impl):
 * the crash breadcrumb names the operation, the credentials are re-elevated (the first
 * attempt may predate kstuff), and the command counter moves. */
static void mgmt_env_enter(uint16_t legacy_frame) {
    g_inflight_frame_type = (unsigned int)legacy_frame;
    runtime_apply_ucred_jailbreak();
    if (g_mgmt_state) {
        pthread_mutex_lock(&g_mgmt_state->state_mtx);
        g_mgmt_state->command_count += 1;
        pthread_mutex_unlock(&g_mgmt_state->state_mtx);
    }
}

static void mgmt_env_leave(void) { g_inflight_frame_type = 0; }

int runtime_mgmt_install(runtime_state_t *state) {
    g_mgmt_state = state;
    return mgmt_rpc_install(k_mgmt_table, sizeof k_mgmt_table / sizeof k_mgmt_table[0], state, mgmt_env_enter,
                            mgmt_env_leave);
}
