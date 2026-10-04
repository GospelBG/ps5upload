//! "No behaviour change": a transport that does not serve the console (`Ok(None)`) leaves the
//! call on the real FTX2 path. A minimal FTX2 server records the exact request frame and
//! answers with a chosen frame; the test asserts the bytes on the wire and the error text.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use ftx2_proto::{FrameHeader, FrameType, FRAME_HEADER_LEN};
use ps5upload_core::mgmt::{self, m, Method, MgmtError, MgmtTransport};

/// A transport that never serves a console, and counts how often it was asked.
struct NotServed(Mutex<usize>);

impl MgmtTransport for NotServed {
    fn call(&self, _: &str, _: Method, _: &str, _: &[u8], _: Duration) -> Result<Option<Vec<u8>>> {
        *self.0.lock().unwrap() += 1;
        Ok(None)
    }
}

/// One-shot server: reads one request frame, answers `(frame, body)`, returns what it read.
fn serve_once(
    reply: (FrameType, &'static [u8]),
) -> (String, thread::JoinHandle<(FrameHeader, Vec<u8>)>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let h = thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut hb = [0u8; FRAME_HEADER_LEN];
        s.read_exact(&mut hb).unwrap();
        let hdr = FrameHeader::decode(&hb).unwrap();
        let mut body = vec![0u8; hdr.body_len as usize];
        s.read_exact(&mut body).unwrap();
        let rh = FrameHeader::new(reply.0, 0, reply.1.len() as u64, hdr.trace_id).encode();
        s.write_all(&rh).unwrap();
        s.write_all(reply.1).unwrap();
        (hdr, body)
    });
    (addr, h)
}

fn not_served() -> (Arc<NotServed>, mgmt::ScopedTransport) {
    let t = Arc::new(NotServed(Mutex::new(0)));
    (t.clone(), mgmt::scoped_transport(t))
}

#[test]
fn an_unserved_call_sends_the_legacy_request_frame_and_returns_the_ack_body() {
    let (t, _g) = not_served();
    let (addr, srv) = serve_once((FrameType::FsMkdirAck, b""));
    let r = mgmt::call(&addr, m::FS_MKDIR, br#"{"path":"/data/x"}"#).unwrap();
    assert!(r.is_empty());
    let (hdr, body) = srv.join().unwrap();
    assert_eq!(hdr.frame_type, FrameType::FsMkdir as u16, "request frame");
    assert_eq!(body, br#"{"path":"/data/x"}"#, "request body, unchanged");
    assert_eq!(*t.0.lock().unwrap(), 1, "the transport was asked first");
}

#[test]
fn an_unserved_call_returns_the_ack_body_bytes() {
    let (_t, _g) = not_served();
    let (addr, srv) = serve_once((FrameType::HwInfoAck, b"model=PS5\n"));
    assert_eq!(mgmt::call(&addr, m::HW_INFO, b"").unwrap(), b"model=PS5\n");
    assert_eq!(srv.join().unwrap().0.frame_type, FrameType::HwInfo as u16);
}

#[test]
fn an_error_frame_reads_payload_rejected_label_cause() {
    let (_t, _g) = not_served();
    let (addr, srv) = serve_once((FrameType::Error, b"fs_move_cross_mount"));
    let e = mgmt::call_as(
        &addr,
        m::FS_RENAME,
        "FS_MOVE",
        br#"{"from":"/a","to":"/b"}"#,
    )
    .unwrap_err();
    assert_eq!(
        e.to_string(),
        "payload rejected FS_MOVE: fs_move_cross_mount"
    );
    let me = e.downcast_ref::<MgmtError>().unwrap();
    assert_eq!((me.status, me.cause.as_str()), (0, "fs_move_cross_mount"));
    srv.join().unwrap();
}

#[test]
fn a_wrong_ack_frame_is_an_error_naming_the_expected_one() {
    let (_t, _g) = not_served();
    let (addr, srv) = serve_once((FrameType::HwPowerAck, b""));
    let e = mgmt::call(&addr, m::HW_INFO, b"").unwrap_err();
    assert_eq!(e.to_string(), "expected HwInfoAck, got HwPowerAck");
    srv.join().unwrap();
}

#[test]
fn a_method_ftx2_never_had_fails_clearly_instead_of_sending_garbage() {
    let (_t, _g) = not_served();
    let e = mgmt::call("127.0.0.1:1", m::FS_STAT, br#"{"path":"/a"}"#).unwrap_err();
    assert!(e.to_string().contains("older one"), "{e}");
}

// ---- the Task 4 call sites keep working against an FTX2-only helper ----

#[test]
fn fs_stat_on_an_ftx2_helper_is_the_one_byte_read_it_replaced() {
    let (_t, _g) = not_served();
    let (addr, srv) = serve_once((FrameType::FsReadAck, b"x"));
    let s = ps5upload_core::fs_ops::fs_stat(&addr, "/data/f").unwrap();
    assert_eq!((s.kind.as_str(), s.size), ("file", 1));
    let (hdr, body) = srv.join().unwrap();
    assert_eq!(hdr.frame_type, FrameType::FsRead as u16);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        (v["path"].as_str(), v["limit"].as_u64()),
        (Some("/data/f"), Some(1))
    );
    // and an absent path is still "not found", not an error the caller must guess about
    let (addr, srv) = serve_once((FrameType::Error, b"fs_read_stat_failed"));
    assert!(!ps5upload_core::fs_ops::fs_exists(&addr, "/data/nope").unwrap());
    srv.join().unwrap();
}

#[test]
fn shutdown_list_volumes_and_cleanup_use_their_legacy_frames_over_ftx2() {
    let (_t, _g) = not_served();
    let (addr, srv) = serve_once((FrameType::ShutdownAck, b"{}"));
    assert!(ps5upload_core::payload_lifecycle::shutdown_running_payload(&addr).unwrap());
    assert_eq!(srv.join().unwrap().0.frame_type, FrameType::Shutdown as u16);
    // a process that answers with another frame is not a payload that acknowledged
    let (addr, srv) = serve_once((FrameType::HwInfoAck, b""));
    assert!(!ps5upload_core::payload_lifecycle::shutdown_running_payload(&addr).unwrap());
    srv.join().unwrap();

    let (addr, srv) = serve_once((FrameType::FsListVolumesAck, br#"{"volumes":[]}"#));
    assert!(ps5upload_core::volumes::list_volumes(&addr)
        .unwrap()
        .volumes
        .is_empty());
    assert_eq!(
        srv.join().unwrap().0.frame_type,
        FrameType::FsListVolumes as u16
    );

    let (addr, srv) = serve_once((
        FrameType::CleanupAck,
        br#"{"ok":true,"path":"/data/x","removed_files":3,"removed_dirs":1}"#,
    ));
    let c = ps5upload_core::cleanup::cleanup_path(&addr, "/data/x").unwrap();
    assert_eq!(c.removed_files, 3);
    srv.join().unwrap();
    let (addr, srv) = serve_once((FrameType::Error, b"cleanup_path_denied"));
    let e = ps5upload_core::cleanup::cleanup_path(&addr, "/system").unwrap_err();
    assert_eq!(
        e.to_string(),
        "payload rejected CLEANUP: cleanup_path_denied"
    );
    srv.join().unwrap();
}

#[test]
fn net_reach_and_mounts_read_their_ok_false_bodies_over_ftx2() {
    let (_t, _g) = not_served();
    // FTX2 answered an unreachable host as a SUCCESS frame with ok:false; it still parses.
    let (addr, srv) = serve_once((
        FrameType::NetReachAck,
        br#"{"ok":false,"timed_out":true,"errno":0,"err":"timed out","ms":3000}"#,
    ));
    let r = ps5upload_core::diagnostics::net_reach(&addr, "10.0.0.9", 9, 100).unwrap();
    assert!(!r.ok && r.timed_out && r.ms == 3000);
    srv.join().unwrap();
    let (addr, srv) = serve_once((
        FrameType::PkgDirectMountAck,
        br#"{"ok":false,"code":-5,"mount_point":"/mnt/ps5upload/x"}"#,
    ));
    let e =
        ps5upload_core::diagnostics::pkg_direct_mount(&addr, "/mnt/ext0/x.pkg", None).unwrap_err();
    assert!(e.to_string().contains("PKG_DIRECT_MOUNT failed"), "{e}");
    srv.join().unwrap();
}
