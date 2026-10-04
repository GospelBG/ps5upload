//! Tests of the migration shim (`legacy_helper.rs`); deleted with it in the release after the cutover.
use super::*;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A fake old helper on a loopback management port: answers `Hello` with a header, records
/// every `Shutdown` frame's bytes, acknowledges it, and (when `exits`) stops listening.
struct Fake {
    ports: Ports,
    seen: Arc<Mutex<Vec<[u8; HEADER_LEN]>>>,
    closed: Arc<AtomicBool>,
}

fn fake_helper(exits: bool) -> Fake {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let ports = Ports {
        mgmt: l.local_addr().unwrap().port(),
        transfer: free_port(),
        ava1: free_port(),
    };
    let seen = Arc::new(Mutex::new(Vec::new()));
    let closed = Arc::new(AtomicBool::new(false));
    let (s2, c2) = (seen.clone(), closed.clone());
    std::thread::spawn(move || {
        for conn in l.incoming() {
            let Ok(mut s) = conn else { continue };
            let mut h = [0u8; HEADER_LEN];
            if s.read_exact(&mut h).is_err() {
                continue; // a bare connect (the port check)
            }
            let ty = u16::from_le_bytes([h[6], h[7]]);
            s2.lock().unwrap().push(h);
            let reply = if ty == SHUTDOWN { SHUTDOWN_ACK } else { 2 };
            let _ = s.write_all(&frame(reply));
            if ty == SHUTDOWN && exits {
                c2.store(true, Ordering::SeqCst);
                return; // drops the listener: the port closes
            }
        }
    });
    Fake {
        ports,
        seen,
        closed,
    }
}

#[test]
fn frame_bytes_are_the_old_header() {
    let mut want = [0u8; HEADER_LEN];
    want[..4].copy_from_slice(b"FTX2");
    want[4] = 1;
    want[6] = 22;
    assert_eq!(frame(SHUTDOWN), want);
    assert_eq!(frame(HELLO)[6], 1);
}

#[test]
fn probe_recognises_an_old_helper_and_nothing_else() {
    let fake = fake_helper(false);
    assert!(probe("127.0.0.1", fake.ports));
    // a listener that is not an old helper never answers with the magic
    let other = TcpListener::bind("127.0.0.1:0").unwrap();
    let p = Ports {
        mgmt: other.local_addr().unwrap().port(),
        transfer: free_port(),
        ava1: 0,
    };
    std::thread::spawn(move || {
        if let Ok((mut s, _)) = other.accept() {
            let _ = s.write_all(&[0u8; HEADER_LEN]);
        }
    });
    assert!(!probe("127.0.0.1", p));
    // nothing at all
    let none = Ports {
        mgmt: free_port(),
        transfer: free_port(),
        ava1: 0,
    };
    assert!(!probe("127.0.0.1", none));
}

#[test]
fn state_tells_the_three_situations_apart() {
    let fake = fake_helper(false);
    assert_eq!(state("127.0.0.1", fake.ports), HELPER_OLD);
    let none = Ports {
        mgmt: free_port(),
        transfer: free_port(),
        ava1: free_port(),
    };
    assert_eq!(state("127.0.0.1", none), NOT_RUNNING);
    let up = TcpListener::bind("127.0.0.1:0").unwrap();
    let p = Ports {
        ava1: up.local_addr().unwrap().port(),
        ..fake.ports
    };
    assert_eq!(
        state("127.0.0.1", p),
        AVA1,
        "AVA1 wins over a leftover old listener"
    );
}

#[test]
fn legacy_helper_is_shut_down_then_replaced() {
    let fake = fake_helper(true);
    let ava1 = fake.ports.ava1;
    let closed = fake.closed.clone();
    let sent = Arc::new(Mutex::new(None::<bool>));
    let sent2 = sent.clone();
    let new_helper = Arc::new(Mutex::new(None::<TcpListener>));
    let nh = new_helper.clone();
    let r = replace(
        "127.0.0.1",
        fake.ports,
        Duration::from_secs(5),
        Duration::from_secs(5),
        move || {
            // the send step: the old helper must be gone by now
            *sent2.lock().unwrap() = Some(closed.load(Ordering::SeqCst));
            *nh.lock().unwrap() = Some(TcpListener::bind(("127.0.0.1", ava1)).unwrap());
            Ok(())
        },
    )
    .expect("replaced");
    assert_eq!(r, Replaced { ava1_up: true });
    assert_eq!(
        *sent.lock().unwrap(),
        Some(true),
        "sent after the old helper exited"
    );
    let seen = fake.seen.lock().unwrap();
    assert_eq!(
        seen.last().copied(),
        Some(frame(SHUTDOWN)),
        "the shutdown frame bytes"
    );
}

#[test]
fn legacy_helper_wedged_is_reported() {
    let fake = fake_helper(false); // acknowledges, never lets go of its port
    let called = Arc::new(AtomicBool::new(false));
    let c2 = called.clone();
    let r = replace(
        "127.0.0.1",
        fake.ports,
        Duration::from_millis(400),
        Duration::from_millis(100),
        move || {
            c2.store(true, Ordering::SeqCst);
            Ok(())
        },
    );
    assert_eq!(r, Err(ReplaceError::Wedged));
    assert!(
        !called.load(Ordering::SeqCst),
        "the new helper is not sent over a live one"
    );
    assert!(r.unwrap_err().to_string().starts_with(LEGACY_HELPER_WEDGED));
}

#[test]
fn a_failed_send_is_not_called_wedged() {
    let fake = fake_helper(true);
    let r = replace(
        "127.0.0.1",
        fake.ports,
        Duration::from_secs(5),
        Duration::from_millis(100),
        || Err("connect 10.0.0.2:9021: refused".into()),
    );
    assert_eq!(
        r,
        Err(ReplaceError::Send("connect 10.0.0.2:9021: refused".into()))
    );
}

#[test]
fn a_new_helper_that_is_slow_to_listen_is_not_an_error() {
    let fake = fake_helper(true);
    let r = replace(
        "127.0.0.1",
        fake.ports,
        Duration::from_secs(5),
        Duration::from_millis(200),
        || Ok(()),
    );
    assert_eq!(r, Ok(Replaced { ava1_up: false }));
}
