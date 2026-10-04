#![cfg(unix)]
//! P3 Task 8: the payload's side of takeover. The old binary protocol lives only in
//! payload/src/legacy_takeover.c (a migration shim); between AVA1-era instances the new one
//! writes a flag file the old one polls (payload/src/takeover_flag.c).
use std::ffi::CString;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::raw::{c_char, c_int};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// Links against ava1c (build.rs), which compiles the two payload files.
use ava1_ctest as _;

extern "C" {
    fn legacy_takeover_frame(hdr: *mut u8);
    fn legacy_takeover(
        mgmt: c_int,
        xfer: c_int,
        ack_s: c_int,
        attempts: c_int,
        interval_us: c_int,
    ) -> c_int;
    fn takeover_flag_write(dir: *const c_char, id: u64) -> c_int;
    fn takeover_flag_read(dir: *const c_char, id: *mut u64) -> c_int;
    fn takeover_flag_newer(dir: *const c_char, my_id: u64) -> c_int;
    fn takeover_flag_request(
        dir: *const c_char,
        id: u64,
        ports: *const c_int,
        n: c_int,
        attempts: c_int,
        interval_us: c_int,
    ) -> c_int;
    fn takeover_flag_poll_start(
        dir: *const c_char,
        id: u64,
        period_ms: c_int,
        cb: extern "C" fn(),
    ) -> c_int;
}

const NONE: c_int = 0;
const FREED: c_int = 1;
const STUCK: c_int = -1;

fn dir() -> (tempdir::Dir, CString) {
    let d = tempdir::Dir::new();
    let c = CString::new(d.path().to_str().unwrap()).unwrap();
    (d, c)
}

/// A scratch directory that removes itself (no tempfile dependency in this crate).
mod tempdir {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};
    pub struct Dir(PathBuf);
    impl Dir {
        pub fn new() -> Dir {
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!(
                "ava1-t8-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&p).unwrap();
            Dir(p)
        }
        pub fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[test]
fn c_legacy_takeover_frame_bytes_match_the_ftx2_header() {
    // payload/src/takeover.c before the cutover: 28 bytes (the plan said 24; the code is the
    // authority): magic "FTX2" LE, version 1, frame type 18, flags 0, body_len 0, trace_id 0.
    let mut h = [0xEEu8; 28];
    unsafe { legacy_takeover_frame(h.as_mut_ptr()) };
    let mut want = [0u8; 28];
    want[0..4].copy_from_slice(&0x3258_5446u32.to_le_bytes());
    want[4..6].copy_from_slice(&1u16.to_le_bytes());
    want[6..8].copy_from_slice(&18u16.to_le_bytes());
    assert_eq!(h, want);
}

/// An "old helper": reads the request, checks it, answers with a 28-byte reply and exits (the
/// listener closes with the thread).
fn old_helper(l: TcpListener, got: Arc<AtomicBool>, answer: bool) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let mut b = [0u8; 28];
        s.read_exact(&mut b).unwrap();
        let mut want = [0u8; 28];
        unsafe { legacy_takeover_frame(want.as_mut_ptr()) };
        assert_eq!(b, want);
        got.store(true, Ordering::SeqCst);
        if answer {
            s.write_all(&[0u8; 28]).unwrap();
        }
        // dropping `l` and `s` frees the port
    })
}

#[test]
fn legacy_takeover_asks_the_old_helper_and_waits_for_its_ports() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let mgmt = l.local_addr().unwrap().port();
    let got = Arc::new(AtomicBool::new(false));
    let h = old_helper(l, got.clone(), true);
    let rc = unsafe { legacy_takeover(mgmt as c_int, free_port() as c_int, 2, 50, 20_000) };
    h.join().unwrap();
    assert!(got.load(Ordering::SeqCst));
    assert_eq!(rc, FREED);
}

#[test]
fn legacy_takeover_with_no_old_helper_is_none() {
    let rc = unsafe { legacy_takeover(free_port() as c_int, free_port() as c_int, 1, 5, 1000) };
    assert_eq!(rc, NONE);
}

#[test]
fn legacy_takeover_reports_a_helper_that_does_not_exit() {
    // Accepts and answers, but never lets go of its port.
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let mgmt = l.local_addr().unwrap().port();
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    let t = std::thread::spawn(move || {
        l.set_nonblocking(true).unwrap();
        while !s2.load(Ordering::SeqCst) {
            if let Ok((mut s, _)) = l.accept() {
                s.set_nonblocking(false).ok();
                let mut b = [0u8; 28];
                let _ = s.read(&mut b);
                let _ = s.write_all(&[0u8; 28]);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    let rc = unsafe { legacy_takeover(mgmt as c_int, free_port() as c_int, 1, 5, 10_000) };
    stop.store(true, Ordering::SeqCst);
    t.join().unwrap();
    assert_eq!(rc, STUCK);
}

#[test]
fn flag_file_roundtrip_and_newer_only() {
    let (_d, c) = dir();
    let mut id = 0u64;
    assert_eq!(
        unsafe { takeover_flag_read(c.as_ptr(), &mut id) },
        -1,
        "absent"
    );
    assert_eq!(unsafe { takeover_flag_write(c.as_ptr(), 500) }, 0);
    assert_eq!(unsafe { takeover_flag_read(c.as_ptr(), &mut id) }, 0);
    assert_eq!(id, 500);
    assert_eq!(unsafe { takeover_flag_newer(c.as_ptr(), 499) }, 1);
    // the new instance's own flag, and a leftover from before it, never ask it to exit
    assert_eq!(unsafe { takeover_flag_newer(c.as_ptr(), 500) }, 0);
    assert_eq!(unsafe { takeover_flag_newer(c.as_ptr(), 501) }, 0);
    // garbage is not a request
    std::fs::write(
        std::path::Path::new(c.to_str().unwrap()).join("takeover"),
        b"zzz",
    )
    .unwrap();
    assert_eq!(unsafe { takeover_flag_newer(c.as_ptr(), 1) }, 0);
    // no temp file left behind
    assert!(!std::path::Path::new(c.to_str().unwrap())
        .join("takeover.tmp")
        .exists());
}

static OLD_EXITED: AtomicBool = AtomicBool::new(false);
extern "C" fn old_exits() {
    OLD_EXITED.store(true, Ordering::SeqCst);
}
static STAYED: AtomicBool = AtomicBool::new(false);
extern "C" fn must_not_run() {
    STAYED.store(true, Ordering::SeqCst);
}

#[test]
fn flag_file_takeover_exits_the_old_instance() {
    let (_d, c) = dir();
    // The old instance (id 100) serves on `port` and polls the flag every 20 ms.
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port() as c_int;
    l.set_nonblocking(true).unwrap();
    assert_eq!(
        unsafe { takeover_flag_poll_start(c.as_ptr(), 100, 20, old_exits) },
        0
    );
    let old = std::thread::spawn(move || {
        // "serves" until its poll thread says exit, then releases the port
        let t0 = Instant::now();
        while !OLD_EXITED.load(Ordering::SeqCst) && t0.elapsed() < Duration::from_secs(10) {
            let _ = l.accept();
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(l);
    });
    assert!(TcpStream::connect(("127.0.0.1", port as u16)).is_ok());
    // The new instance (id 200) asks and waits for the port to free.
    let ports = [port];
    let rc = unsafe { takeover_flag_request(c.as_ptr(), 200, ports.as_ptr(), 1, 200, 20_000) };
    old.join().unwrap();
    assert_eq!(rc, 0, "the port freed");
    assert!(OLD_EXITED.load(Ordering::SeqCst), "the old instance exited");
}

#[test]
fn flag_file_takeover_times_out_on_an_instance_that_stays() {
    let (_d, c) = dir();
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port() as c_int;
    let ports = [port];
    let rc = unsafe { takeover_flag_request(c.as_ptr(), 200, ports.as_ptr(), 1, 5, 5_000) };
    assert_eq!(rc, -1);
    drop(l);
}

#[test]
fn an_older_or_equal_flag_never_stops_an_instance() {
    let (_d, c) = dir();
    unsafe { takeover_flag_write(c.as_ptr(), 100) };
    assert_eq!(
        unsafe { takeover_flag_poll_start(c.as_ptr(), 100, 10, must_not_run) },
        0
    );
    assert_eq!(
        unsafe { takeover_flag_poll_start(c.as_ptr(), 101, 10, must_not_run) },
        0
    );
    std::thread::sleep(Duration::from_millis(250));
    assert!(!STAYED.load(Ordering::SeqCst));
}
