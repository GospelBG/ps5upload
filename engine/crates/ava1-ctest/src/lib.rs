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
    }

    extern "C" {
        pub fn ava1_test_sizeof_noise() -> usize;
        pub fn ava1_identity_from_secret(id: *mut CIdentity, secret: *const u8);
        pub fn ava1_lane_key(dir: *const u8, lane: u16, out: *mut u8);
        pub fn ava1_join_tag(
            dir: *const u8,
            label: *const c_char,
            sid: *const u8,
            lane: u16,
            nonce: *const u8,
            out: *mut u8,
        );
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
        pub fn ava1_noise_split(ns: *const CNoise, k_i2r: *mut u8, k_r2i: *mut u8);
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
    }
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
    pub fn split(&self) -> ([u8; 32], [u8; 32]) {
        let (mut a, mut b) = ([0u8; 32], [0u8; 32]);
        unsafe { ffi::ava1_noise_split(&*self.0, a.as_mut_ptr(), b.as_mut_ptr()) };
        (a, b)
    }
}
