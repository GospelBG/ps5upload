//! The payload's AVA1 C, built for the host (see build.rs). Test-only.
#![cfg(unix)]

use ava1::frame::Header;
use std::ffi::CString;
use std::os::raw::{c_char, c_int};

pub mod ffi {
    use super::*;

    #[repr(C)]
    pub struct CHeader {
        pub ty: u8,
        pub flags: u8,
        pub channel: u32,
        pub body_len: u32,
    }

    extern "C" {
        pub fn ava1_roundtrip(
            name: *const c_char,
            inp: *const u8,
            in_len: usize,
            out: *mut u8,
            cap: usize,
            out_len: *mut usize,
        ) -> c_int;
        pub fn ava1_utf8_valid(s: *const u8, n: usize) -> c_int;
        pub fn ava1_crc32c(p: *const u8, n: usize) -> u32;
        pub fn ava1_header_encode(h: *const CHeader, out: *mut u8);
        pub fn ava1_header_decode(inp: *const u8, h: *mut CHeader) -> c_int;
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct CIdentity {
        pub secret: [u8; 32],
        pub public: [u8; 32],
    }

    /// Mirrors ava1_noise_t (checked by `ava1_test_sizeof_noise`).
    #[repr(C)]
    pub struct CNoise {
        pub ck: [u8; 64],
        pub h: [u8; 64],
        pub k: [u8; 32],
        pub has_k: c_int,
        pub n: u64,
        pub s: CIdentity,
        pub e: CIdentity,
        pub rs: [u8; 32],
        pub re: [u8; 32],
        pub initiator: c_int,
        pub step: c_int,
        pub failed: c_int,
    }

    extern "C" {
        pub static mut ava1_trust_slot: [u8; 64];
        pub fn ava1_trust_slot_key(out: *mut u8) -> c_int;
        pub fn ava1_trust_slot_token(out: *mut u8) -> c_int;
        pub fn ava1_launch_proof(token: *const u8, h: *const u8, out: *mut u8);
    }

    extern "C" {
        pub fn ava1_test_sizeof_noise() -> usize;
        pub fn ava1_identity_from_secret(id: *mut CIdentity, secret: *const u8);
        pub fn ava1_lane_key(dir: *const u8, lane: u16, cn: *const u8, sn: *const u8, out: *mut u8);
        pub fn ava1_control_key(dir: *const u8, out: *mut u8);
        pub fn ava1_join_tag(
            dir: *const u8,
            sid: *const u8,
            lane: u16,
            cn: *const u8,
            out: *mut u8,
        );
        pub fn ava1_join_ack_tag(
            dir: *const u8,
            sid: *const u8,
            lane: u16,
            cn: *const u8,
            sn: *const u8,
            out: *mut u8,
        );
        /// Feeds `frame` to the C reader of a connection keyed with `key` (counter 0).
        /// 0 = opened; AVA1_E_* otherwise.
        pub fn ava1_test_conn_open_frame(key: *const u8, frame: *const u8, len: usize) -> c_int;
        pub fn ava1_pairing_code(hash: *const u8) -> u32;
        pub fn ava1_noise_init(
            ns: *mut CNoise,
            initiator: c_int,
            s: *const CIdentity,
            e: *const CIdentity,
            prologue: *const u8,
            plen: usize,
        );
        pub fn ava1_noise_write(
            ns: *mut CNoise,
            payload: *const u8,
            plen: usize,
            out: *mut u8,
            cap: usize,
            out_len: *mut usize,
        ) -> c_int;
        pub fn ava1_noise_read(
            ns: *mut CNoise,
            msg: *const u8,
            len: usize,
            payload: *mut u8,
            cap: usize,
            plen: *mut usize,
        ) -> c_int;
        pub fn ava1_noise_split(ns: *const CNoise, k_i2r: *mut u8, k_r2i: *mut u8) -> c_int;
        pub fn ava1_seal(
            key: *const u8,
            n: u64,
            ad: *const u8,
            ad_len: usize,
            buf: *mut u8,
            len: usize,
            mac: *mut u8,
        );
        pub fn ava1_open(
            key: *const u8,
            n: u64,
            ad: *const u8,
            ad_len: usize,
            buf: *mut u8,
            len: usize,
            mac: *const u8,
        ) -> c_int;
        /// "avx2" or "portable" (NUL-terminated).
        pub fn ava1_aead_backend() -> *const c_char;
        /// 0 forces the portable ChaCha20, 1 restores CPUID selection.
        pub fn ava1_aead_allow_simd(allow: c_int);
    }

    extern "C" {
        pub fn ava1_b3_group_cv(data: *const u8, len: usize, index: u64, cv: *mut u8);
        pub fn ava1_b3_root_from_cvs(cvs: *const [u8; 32], n: u64, root: *mut u8);
        pub fn ava1_b3_hash(data: *const u8, len: usize, out: *mut u8);
    }

    #[repr(C)]
    pub struct CPeer {
        pub key: [u8; 32],
        pub added_unix: u64,
        pub name: [c_char; 64],
    }

    #[repr(C)]
    pub struct CPeers {
        pub p: [CPeer; 32],
        pub n: c_int,
    }

    /// Mirrors ava1_test_opts_t in csrc/test_shim.c.
    #[repr(C)]
    #[derive(Debug, Clone, Copy, Default)]
    pub struct TestOpts {
        pub pairing_s: u32,
        pub ping_ms: u32,
        pub dead_ms: u32,
        pub handshake_ms: u32,
        /// 0 = the server's default, here and below.
        pub min_frame_rate: u32,
        pub max_conns_per_ip: u32,
        pub max_unpaired: u32,
        pub pair_confirm_ms: u32,
        pub notify_every_ms: u32,
        /// 1: the trust slot's key is `launch_key` and, with 2, its token `launch_token`
        /// (what ava1_glue.c passes the server from the slot).
        pub launch: u32,
        pub launch_key: [u8; 32],
        pub launch_token: [u8; 16],
    }

    extern "C" {
        pub fn ava1_test_server_start(
            secret: *const u8,
            peers_path: *const c_char,
            opts: *const TestOpts,
        ) -> c_int;
        /// The C server with the echo data hooks (test_shim.c).
        pub fn ava1_test_server_start_echo(
            secret: *const u8,
            peers_path: *const c_char,
            ping_ms: u32,
            dead_ms: u32,
            handshake_ms: u32,
        ) -> c_int;
        /// ava1_conn_post's bounded queue (test_shim.c): fill it while the writer
        /// cannot drain, read every frame back whole and in order, then check the
        /// bound refuses with AVA1_E_BUSY and breaks the connection. 0 = ok.
        pub fn ava1_test_post_queue(key: *const u8) -> c_int;
        pub fn ava1_test_sizeof_opts() -> usize;
        pub fn ava1_test_pair_requests() -> u32;
        pub fn ava1_test_last_pair_code() -> u32;
        pub fn ava1_test_logs() -> u32;
        pub fn ava1_server_open_pairing(seconds: u32);
        pub fn ava1_server_pairing_open() -> c_int;
        pub fn ava1_server_stop();
        pub fn ava1_server_conns() -> c_int;
        pub fn ava1_identity_load_or_create(path: *const c_char, id: *mut CIdentity) -> c_int;
        pub fn ava1_peers_load(ps: *mut CPeers, path: *const c_char) -> c_int;
        pub fn ava1_peers_contains(ps: *const CPeers, key: *const u8) -> c_int;
        pub fn ava1_test_records_helpers(
            blob: *const u8,
            len: u32,
            out: *mut u8,
            cap: usize,
            out_len: *mut usize,
            count: *mut u32,
        ) -> c_int;
        pub fn ava1_test_rset_after(
            ops: *const u64,
            nops: usize,
            out: *mut u64,
            cap: usize,
        ) -> usize;
        pub fn ava1_test_journal_dump(dir: *const c_char, out: *mut u8, cap: usize) -> usize;
        pub fn ava1_test_journal_write_sample(dir: *const c_char) -> c_int;
        pub fn ava1_test_bits_runs(
            n: u32,
            set: *const u32,
            nset: usize,
            out: *mut u32,
            cap: usize,
        ) -> usize;
    }
}

/// The generated C per-struct records helpers (SPEC.md §3) over one blob of items: how
/// many items the blob holds, and the blob C rebuilds by re-appending each of them.
pub fn c_records_helpers(blob: &[u8]) -> Result<(Vec<u8>, u32), i32> {
    let mut out = vec![0u8; blob.len() + 64];
    let mut out_len = 0usize;
    let mut count = 0u32;
    let rc = unsafe {
        ffi::ava1_test_records_helpers(
            blob.as_ptr(),
            blob.len() as u32,
            out.as_mut_ptr(),
            out.len(),
            &mut out_len,
            &mut count,
        )
    };
    if rc != 0 {
        return Err(rc);
    }
    out.truncate(out_len);
    Ok((out, count))
}

pub fn c_roundtrip(name: &str, input: &[u8]) -> Result<Vec<u8>, i32> {
    let n = CString::new(name).unwrap();
    let mut out = vec![0u8; input.len() + 64];
    let mut len = 0usize;
    let rc = unsafe {
        ffi::ava1_roundtrip(
            n.as_ptr(),
            input.as_ptr(),
            input.len(),
            out.as_mut_ptr(),
            out.len(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(rc);
    }
    out.truncate(len);
    Ok(out)
}

pub fn c_crc32c(b: &[u8]) -> u32 {
    unsafe { ffi::ava1_crc32c(b.as_ptr(), b.len()) }
}

pub fn c_header_encode(h: Header) -> [u8; 16] {
    let ch = ffi::CHeader {
        ty: h.ty,
        flags: h.flags,
        channel: h.channel,
        body_len: h.body_len,
    };
    let mut out = [0u8; 16];
    unsafe { ffi::ava1_header_encode(&ch, out.as_mut_ptr()) };
    out
}

pub fn c_header_decode(b: &[u8; 16]) -> Result<Header, i32> {
    let mut ch = ffi::CHeader {
        ty: 0,
        flags: 0,
        channel: 0,
        body_len: 0,
    };
    match unsafe { ffi::ava1_header_decode(b.as_ptr(), &mut ch) } {
        0 => Ok(Header {
            ty: ch.ty,
            flags: ch.flags,
            channel: ch.channel,
            body_len: ch.body_len,
        }),
        e => Err(e),
    }
}

pub fn c_utf8_valid(b: &[u8]) -> bool {
    unsafe { ffi::ava1_utf8_valid(b.as_ptr(), b.len()) != 0 }
}

pub fn c_b3_group_cv(d: &[u8], index: u64) -> [u8; 32] {
    let mut cv = [0u8; 32];
    unsafe { ffi::ava1_b3_group_cv(d.as_ptr(), d.len(), index, cv.as_mut_ptr()) };
    cv
}

pub fn c_b3_root(cvs: &[[u8; 32]]) -> [u8; 32] {
    let mut r = [0u8; 32];
    unsafe { ffi::ava1_b3_root_from_cvs(cvs.as_ptr(), cvs.len() as u64, r.as_mut_ptr()) };
    r
}

pub fn c_b3_hash(d: &[u8]) -> [u8; 32] {
    let mut r = [0u8; 32];
    unsafe { ffi::ava1_b3_hash(d.as_ptr(), d.len(), r.as_mut_ptr()) };
    r
}

pub fn c_identity(secret: [u8; 32]) -> ffi::CIdentity {
    let mut id = ffi::CIdentity {
        secret: [0; 32],
        public: [0; 32],
    };
    unsafe { ffi::ava1_identity_from_secret(&mut id, secret.as_ptr()) };
    id
}

/// One side of a C Noise handshake.
pub struct CHandshake(Box<ffi::CNoise>);

impl CHandshake {
    pub fn new(initiator: bool, s: [u8; 32], e: [u8; 32], prologue: &[u8]) -> Self {
        let (s, e) = (c_identity(s), c_identity(e));
        let mut ns: Box<ffi::CNoise> = Box::new(unsafe { std::mem::zeroed() });
        unsafe {
            ffi::ava1_noise_init(
                &mut *ns,
                initiator as c_int,
                &s,
                &e,
                prologue.as_ptr(),
                prologue.len(),
            )
        };
        CHandshake(ns)
    }
    pub fn write(&mut self, payload: &[u8]) -> Result<Vec<u8>, i32> {
        let mut out = vec![0u8; payload.len() + 128];
        let mut n = 0usize;
        match unsafe {
            ffi::ava1_noise_write(
                &mut *self.0,
                payload.as_ptr(),
                payload.len(),
                out.as_mut_ptr(),
                out.len(),
                &mut n,
            )
        } {
            0 => {
                out.truncate(n);
                Ok(out)
            }
            e => Err(e),
        }
    }
    pub fn read(&mut self, msg: &[u8]) -> Result<Vec<u8>, i32> {
        let mut out = vec![0u8; msg.len()];
        let mut n = 0usize;
        match unsafe {
            ffi::ava1_noise_read(
                &mut *self.0,
                msg.as_ptr(),
                msg.len(),
                out.as_mut_ptr(),
                out.len(),
                &mut n,
            )
        } {
            0 => {
                out.truncate(n);
                Ok(out)
            }
            e => Err(e),
        }
    }
    pub fn hash(&self) -> [u8; 64] {
        self.0.h
    }
    pub fn remote_static(&self) -> [u8; 32] {
        self.0.rs
    }
    /// Panics unless the handshake completed.
    pub fn split(&self) -> ([u8; 32], [u8; 32]) {
        self.try_split().expect("handshake complete")
    }

    pub fn try_split(&self) -> Result<([u8; 32], [u8; 32]), i32> {
        let (mut a, mut b) = ([0xffu8; 32], [0xffu8; 32]);
        match unsafe { ffi::ava1_noise_split(&*self.0, a.as_mut_ptr(), b.as_mut_ptr()) } {
            0 => Ok((a, b)),
            e => {
                assert_eq!((a, b), ([0u8; 32], [0u8; 32]), "no key material on failure");
                Err(e)
            }
        }
    }

    /// Replaces the static public key this side will send (to play a hostile peer).
    pub fn set_static_public(&mut self, p: [u8; 32]) {
        self.0.s.public = p;
    }
}

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

static C_SERVER: Mutex<()> = Mutex::new(());

/// The payload's server, running on 127.0.0.1. One at a time per process.
pub struct CServer {
    pub port: u16,
    _lock: MutexGuard<'static, ()>,
}

impl CServer {
    pub fn start(
        secret: [u8; 32],
        peers_path: &Path,
        pairing_s: u32,
        ping_ms: u32,
        dead_ms: u32,
        handshake_ms: u32,
    ) -> Self {
        Self::start_with(
            secret,
            peers_path,
            ffi::TestOpts {
                pairing_s,
                ping_ms,
                dead_ms,
                handshake_ms,
                ..Default::default()
            },
        )
    }

    pub fn start_with(secret: [u8; 32], peers_path: &Path, opts: ffi::TestOpts) -> Self {
        let lock = C_SERVER.lock().unwrap_or_else(|e| e.into_inner());
        let p = CString::new(peers_path.to_str().unwrap()).unwrap();
        let rc = unsafe { ffi::ava1_test_server_start(secret.as_ptr(), p.as_ptr(), &opts) };
        assert!(rc > 0, "C server failed to start: {rc}");
        CServer {
            port: rc as u16,
            _lock: lock,
        }
    }

    /// The C server with the echo data hooks (test_shim.c).
    pub fn start_echo(
        secret: [u8; 32],
        peers_path: &Path,
        ping_ms: u32,
        dead_ms: u32,
        hs_ms: u32,
    ) -> Self {
        let lock = C_SERVER.lock().unwrap_or_else(|e| e.into_inner());
        let p = CString::new(peers_path.to_str().unwrap()).unwrap();
        let rc = unsafe {
            ffi::ava1_test_server_start_echo(secret.as_ptr(), p.as_ptr(), ping_ms, dead_ms, hs_ms)
        };
        assert!(rc > 0, "C server failed to start: {rc}");
        CServer {
            port: rc as u16,
            _lock: lock,
        }
    }

    pub fn addr(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    pub fn conns(&self) -> i32 {
        unsafe { ffi::ava1_server_conns() }
    }

    /// Lines the server has logged since it started.
    pub fn logs(&self) -> u32 {
        unsafe { ffi::ava1_test_logs() }
    }

    pub fn open_pairing(&self, seconds: u32) {
        unsafe { ffi::ava1_server_open_pairing(seconds) }
    }

    pub fn pairing_open(&self) -> bool {
        unsafe { ffi::ava1_server_pairing_open() != 0 }
    }

    pub fn pair_requests(&self) -> (u32, u32) {
        unsafe {
            (
                ffi::ava1_test_pair_requests(),
                ffi::ava1_test_last_pair_code(),
            )
        }
    }
}

impl Drop for CServer {
    fn drop(&mut self) {
        unsafe { ffi::ava1_server_stop() };
    }
}

extern "C" {
    fn ava1_test_firmware(kernel_version: *const c_char, out: *mut c_char, cap: usize);
}

/// The payload's node.info firmware string for kernel build string `kv`, written into a
/// `cap`-byte buffer (payload/include/ps5_firmware.h).
pub fn c_firmware_from_kernel(kv: &str, cap: usize) -> String {
    let kv = CString::new(kv).unwrap();
    let mut out = vec![0x55 as c_char; cap];
    unsafe { ava1_test_firmware(kv.as_ptr(), out.as_mut_ptr(), cap) };
    unsafe { std::ffi::CStr::from_ptr(out.as_ptr()) }
        .to_str()
        .unwrap()
        .to_string()
}

pub fn c_identity_load_or_create(path: &Path) -> Result<[u8; 32], i32> {
    let p = CString::new(path.to_str().unwrap()).unwrap();
    let mut id = ffi::CIdentity {
        secret: [0; 32],
        public: [0; 32],
    };
    match unsafe { ffi::ava1_identity_load_or_create(p.as_ptr(), &mut id) } {
        0 => Ok(id.public),
        e => Err(e),
    }
}

/// (number of peers loaded, whether `key` is among them)
pub fn c_peers_load(path: &Path, key: &[u8; 32]) -> (i32, bool) {
    let p = CString::new(path.to_str().unwrap()).unwrap();
    let mut ps: Box<std::mem::MaybeUninit<ffi::CPeers>> = Box::new(std::mem::MaybeUninit::uninit());
    unsafe {
        assert_eq!(ffi::ava1_peers_load(ps.as_mut_ptr(), p.as_ptr()), 0);
        let ps = ps.assume_init_ref();
        (ps.n, ffi::ava1_peers_contains(ps, key.as_ptr()) != 0)
    }
}

/// ava1_conn_post's bounded queue (test_shim.c): fills it while the writer cannot
/// drain, reads every frame back whole and in order, then checks the bound refuses
/// with AVA1_E_BUSY and breaks the connection. 0 = ok, negative = which check failed.
pub fn c_post_queue(key: [u8; 32]) -> i32 {
    unsafe { ffi::ava1_test_post_queue(key.as_ptr()) }
}

pub fn c_rset_after(ops: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let flat: Vec<u64> = ops.iter().flat_map(|(s, e)| [*s, *e]).collect();
    let mut out = vec![0u64; 2 * ops.len() + 2];
    let n = unsafe {
        ffi::ava1_test_rset_after(flat.as_ptr(), ops.len(), out.as_mut_ptr(), ops.len() + 1)
    };
    out.chunks(2).take(n).map(|p| (p[0], p[1])).collect()
}

pub fn c_bits_runs(n: u32, set: &[u32]) -> Vec<(u32, u32)> {
    let mut out = vec![0u32; 2 * (n as usize + 1)];
    let k = unsafe {
        ffi::ava1_test_bits_runs(n, set.as_ptr(), set.len(), out.as_mut_ptr(), n as usize + 1)
    };
    out.chunks(2).take(k).map(|p| (p[0], p[1])).collect()
}

/// The C replay of the journal at `dir`, as the text `c_style_dump` builds for the
/// same state.
pub fn c_journal_dump(dir: &Path) -> String {
    let d = CString::new(dir.to_str().unwrap()).unwrap();
    let mut out = vec![0u8; 1 << 20];
    let n = unsafe { ffi::ava1_test_journal_dump(d.as_ptr(), out.as_mut_ptr(), out.len()) };
    String::from_utf8(out[..n].to_vec()).unwrap()
}

/// Writes a sample journal from C; 0 or a negative error.
pub fn c_journal_write_sample(dir: &Path) -> i32 {
    let d = CString::new(dir.to_str().unwrap()).unwrap();
    unsafe { ffi::ava1_test_journal_write_sample(d.as_ptr()) }
}
