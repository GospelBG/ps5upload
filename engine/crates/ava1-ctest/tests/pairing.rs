//! The engine's pairing seam (`Pool::pairing_status` / `Pool::confirm_pairing`) against the
//! real C server: the six-digit code the user compares, the confirm, and a closed window.
#![cfg(unix)]
use std::time::{Duration, Instant};

use ava1::Ava1Error;
use ava1_ctest::*;
use ps5upload_ava1::{Pairing, Pool};

mod common;
use common::{dir, SECRET};

fn opts(pairing_s: u32) -> ffi::TestOpts {
    ffi::TestOpts {
        pairing_s,
        ping_ms: 100,
        dead_ms: 5_000,
        handshake_ms: 2_000,
        ..Default::default()
    }
}

/// The server shows its code once it has checked our Auth; the connect can return first.
fn wait_for_console_code(srv: &CServer) -> u32 {
    let t = Instant::now();
    while srv.pair_requests().0 == 0 && t.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(20));
    }
    srv.pair_requests().1
}

#[tokio::test(flavor = "multi_thread")]
async fn the_code_is_the_one_the_console_shows_and_confirm_pairs() {
    let d = dir("pool-pair");
    let srv = CServer::start_with(SECRET, &d.join("srv-peers"), opts(60));
    let pool = Pool::new(d.join("ava")).with_addr(srv.addr());

    let first = pool.pairing_status("192.0.2.9").await.unwrap();
    let Pairing::Code { code, peer_name } = first else {
        panic!("expected a code, got {first:?}")
    };
    assert_eq!(
        code,
        wait_for_console_code(&srv),
        "same code on both screens"
    );
    assert_eq!(peer_name, "C test server");

    // Asking again while the dialog is open must not open a second handshake: the code on
    // the console would change under the user's eyes.
    let attempts = pool.attempts();
    let again = pool.pairing_status("192.0.2.9").await.unwrap();
    assert_eq!(
        again,
        Pairing::Code {
            code,
            peer_name: "C test server".into()
        }
    );
    assert_eq!(pool.attempts(), attempts, "the pending session is reused");

    pool.confirm_pairing("192.0.2.9").await.unwrap();
    assert!(
        !srv.pairing_open(),
        "a successful pairing closes the window"
    );
    assert_eq!(
        pool.pairing_status("192.0.2.9").await.unwrap(),
        Pairing::Paired
    );
    // And an ordinary session works now, with no code.
    pool.session("192.0.2.9")
        .await
        .unwrap()
        .node_info()
        .await
        .unwrap();
    let peers = std::fs::read_to_string(d.join("srv-peers")).unwrap();
    assert!(peers.lines().count() == 1, "{peers}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_closed_window_is_reported_as_closed_not_as_an_error() {
    let d = dir("pool-pair-closed");
    // pairing_s = 0: the window is shut, and the peers file already has a device in it.
    let (_other, _) = common::paired_client(&d.join("srv-peers"));
    let srv = CServer::start_with(SECRET, &d.join("srv-peers"), opts(0));
    let pool = Pool::new(d.join("ava")).with_addr(srv.addr());
    assert_eq!(
        pool.pairing_status("192.0.2.9").await.unwrap(),
        Pairing::Closed
    );
    // Nothing is pending, so a confirm has nothing to confirm.
    assert!(matches!(
        pool.confirm_pairing("192.0.2.9").await,
        Err(Ava1Error::NotPaired)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_console_that_already_trusts_us_needs_no_code() {
    let d = dir("pool-pair-known");
    // Teach each side the other's key, the way a launch from the app does.
    let ava = d.join("ava");
    std::fs::create_dir_all(&ava).unwrap();
    let me = ava1::keys::Identity::load_or_create(&ava.join("identity")).unwrap();
    ava1::peers::PeerStore::load(&d.join("srv-peers"))
        .unwrap()
        .add(me.public(), "engine")
        .unwrap();
    ava1::peers::PeerStore::load(&ava.join("peers"))
        .unwrap()
        .add(
            ava1::keys::Identity::from_secret(SECRET).public(),
            "C test server",
        )
        .unwrap();
    let srv = CServer::start_with(SECRET, &d.join("srv-peers"), opts(0));
    let pool = Pool::new(ava).with_addr(srv.addr());
    assert_eq!(
        pool.pairing_status("192.0.2.9").await.unwrap(),
        Pairing::Paired
    );
}
