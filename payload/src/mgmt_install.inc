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
#define MGMT_HS(m, f, a, fl, h, helper)                                                                    \
    static int mgmt_l_##m(void *st, int fd, uint64_t t, const char *b, uint64_t l) {                       \
        ftx2_header_t hdr;                                                                                  \
        char scratch[2048]; /* the old dispatcher's `body` buffer */                                        \
        (void)b;                                                                                           \
        (void)l;                                                                                           \
        memset(&hdr, 0, sizeof hdr);                                                                        \
        hdr.trace_id = t;                                                                                   \
        return h((runtime_state_t *)st, fd, hdr, scratch, sizeof scratch);                                  \
    }                                                                                                      \
    static int mgmt_r_##m(const uint8_t *q, uint32_t n, mgmt_ctx_t *cx) { return helper(q, n, cx, mgmt_l_##m); }
#define MGMT_N(m, f, a, fl, run) /* native: the runner is the table's own */
#include "mgmt_table.def"
#undef MGMT_H0
#undef MGMT_H1
#undef MGMT_HS
#undef MGMT_N

#define MGMT_H0(m, f, a, fl, h, helper) {(uint16_t)(m), (uint16_t)(f), (uint16_t)(a), (fl), mgmt_r_##m},
#define MGMT_H1(m, f, a, fl, h, helper) {(uint16_t)(m), (uint16_t)(f), (uint16_t)(a), (fl), mgmt_r_##m},
#define MGMT_HS(m, f, a, fl, h, helper) {(uint16_t)(m), (uint16_t)(f), (uint16_t)(a), (fl), mgmt_r_##m},
#define MGMT_N(m, f, a, fl, run) {(uint16_t)(m), (uint16_t)(f), (uint16_t)(a), (fl), run},
static const mgmt_entry_t k_mgmt_table[] = {
#include "mgmt_table.def"
};
#undef MGMT_H0
#undef MGMT_H1
#undef MGMT_HS
#undef MGMT_N

_Static_assert(FTX2_FRAME_ERROR == 3u, "mgmt_rpc.c's MGMT_FRAME_ERROR must match FTX2_FRAME_ERROR");

/* What the FTX2 management thread did around every frame (handle_binary_frame_impl):
 * the crash breadcrumb names the operation and the credentials are re-elevated (the first
 * attempt may predate kstuff). It does not move the command counter: the handlers bump it themselves
 * on success (as they did under FTX2), and a second count here would double it. */
static void mgmt_env_enter(uint16_t legacy_frame) {
    g_inflight_frame_type = (unsigned int)legacy_frame;
    runtime_apply_ucred_jailbreak();
}

static void mgmt_env_leave(void) { g_inflight_frame_type = 0; }

/* The native filesystem methods (mgmt_fs.c) get their policy from here: the same allowlist, the
 * same read carve-outs and the same command counter the FTX2 handlers use. */
/* (the policy callbacks, and the counter the fs runners call, are defined in runtime.c above this include) */
int runtime_mgmt_install(runtime_state_t *state) {
    mgmt_fs_policy_t pol;
    memset(&pol, 0, sizeof pol);
    pol.write_allowed = is_path_allowed;
    pol.read_allowed = mgmt_fs_read_allowed;
    pol.count = mgmt_fs_count;
    pol.dev_of = NULL; /* stat(2) */
    g_mgmt_fs_state = state;
    mgmt_fs_set_policy(&pol);
    return mgmt_rpc_install(k_mgmt_table, sizeof k_mgmt_table / sizeof k_mgmt_table[0], state, mgmt_env_enter,
                            mgmt_env_leave);
}
