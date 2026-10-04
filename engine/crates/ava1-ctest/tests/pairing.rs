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

    pool.confirm_pairing("192.0.2.9", code).await.unwrap();
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
        pool.confirm_pairing("192.0.2.9", 0).await,
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

#[tokio::test(flavor = "multi_thread")]
async fn a_confirm_with_nothing_pending_re_reads_the_state() {
    let d = dir("pool-pair-late");
    let srv = CServer::start_with(SECRET, &d.join("srv-peers"), opts(60));
    let pool = Pool::new(d.join("ava")).with_addr(srv.addr());
    // Nothing pending and the window open: the confirm is answered with a fresh code, not "closed".
    let first = pool.confirm_or_status("192.0.2.9", 0).await.unwrap();
    assert!(matches!(first, Pairing::Code { .. }), "{first:?}");
    let shown = wait_for_console_code(&srv);
    // Now it is pending: a confirm pairs.
    assert_eq!(
        pool.confirm_or_status("192.0.2.9", shown).await.unwrap(),
        Pairing::Paired
    );
    // A second (late or concurrent) confirm finds it paired: accepted, never closed.
    assert_eq!(
        pool.confirm_or_status("192.0.2.9", shown).await.unwrap(),
        Pairing::Paired
    );
}

// ---- passkey entry through the engine's pool (SPEC.md §5.5) ----

use std::sync::Arc;

async fn start_pairing(pool: &Pool) -> String {
    match pool.pairing_status("192.0.2.9").await.unwrap() {
        Pairing::Code { peer_name, .. } => peer_name,
        other => panic!("expected a code, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_typo_keeps_the_console_code_on_screen_and_the_right_code_then_pairs() {
    let d = dir("pool-wrongcode");
    let srv = CServer::start_with(SECRET, &d.join("srv-peers"), opts(60));
    let pool = Pool::new(d.join("ava")).with_addr(srv.addr());
    start_pairing(&pool).await;
    let shown = wait_for_console_code(&srv);
    // Five typos in a row: the app catches each, the console is never asked, so its window
    // stays open and its code unchanged.
    for i in 1..=6 {
        let r = pool
            .confirm_or_status("192.0.2.9", (shown + i) % 1_000_000)
            .await
            .unwrap();
        assert!(matches!(r, Pairing::WrongCode { .. }), "{r:?}");
    }
    assert!(srv.pairing_open());
    assert_eq!(srv.pair_requests().0, 1, "no second pop-up was needed");
    assert_eq!(
        pool.confirm_or_status("192.0.2.9", shown).await.unwrap(),
        Pairing::Paired
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn dismissing_the_dialog_gives_the_unconfirmed_place_back() {
    let d = dir("pool-cancel");
    let srv = CServer::start_with(SECRET, &d.join("srv-peers"), opts(60));
    let pool = Pool::new(d.join("ava")).with_addr(srv.addr());
    start_pairing(&pool).await; // takes one of the console's two places
    let other = Pool::new(d.join("ava2")).with_addr(srv.addr());
    start_pairing(&other).await; // takes the other
    let third = Pool::new(d.join("ava3")).with_addr(srv.addr());
    let full = third.pairing_status("192.0.2.9").await;
    assert!(
        matches!(&full, Err(Ava1Error::Refused { code, .. }) if *code == ava1::gen::ERR_BUSY),
        "{full:?}"
    );
    assert!(pool.cancel_pairing("192.0.2.9").await);
    assert!(!pool.cancel_pairing("192.0.2.9").await, "nothing left");
    let t = Instant::now();
    loop {
        match third.pairing_status("192.0.2.9").await {
            Ok(Pairing::Code { .. }) => break,
            r => assert!(t.elapsed() < Duration::from_secs(5), "{r:?}"),
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // And the poll that follows a dismissal does not knock again.
    assert!(matches!(
        pool.confirm_pairing("192.0.2.9", 1).await,
        Err(Ava1Error::NotPaired)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_different_console_at_a_pinned_address_can_be_forgotten_and_paired() {
    let d = dir("pool-forget");
    let ava = d.join("ava");
    {
        let srv = CServer::start_with(SECRET, &d.join("srv-peers"), opts(60));
        let pool = Pool::new(ava.clone()).with_addr(srv.addr());
        start_pairing(&pool).await;
        let code = wait_for_console_code(&srv);
        pool.confirm_pairing("192.0.2.9", code).await.unwrap();
    }
    // A different console (another key) now answers at the same address.
    let srv2 = CServer::start_with([0x77; 32], &d.join("srv2-peers"), opts(60));
    let pool = Pool::new(ava).with_addr(srv2.addr());
    assert_eq!(
        pool.pairing_status("192.0.2.9").await.unwrap(),
        Pairing::WrongConsole
    );
    assert_eq!(
        pool.pairing_status("192.0.2.9").await.unwrap(),
        Pairing::WrongConsole,
        "no way out without forgetting"
    );
    assert!(pool.forget_console_key("192.0.2.9").await);
    assert!(!pool.forget_console_key("192.0.2.9").await, "already gone");
    start_pairing(&pool).await;
    let code = wait_for_console_code(&srv2);
    pool.confirm_pairing("192.0.2.9", code).await.unwrap();
    pool.session("192.0.2.9")
        .await
        .unwrap()
        .node_info()
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn polling_an_unpaired_console_for_a_minute_opens_at_most_two_handshakes() {
    use ps5upload_ava1::console::require_in;
    let d = dir("pool-poll");
    let srv = CServer::start_with(SECRET, &d.join("srv-peers"), opts(60));
    // The status poll is 10 s and the not-paired memory 30 s; scaled 1:100 here.
    let pool = Arc::new(
        Pool::new(d.join("ava"))
            .with_addr(srv.addr())
            .with_not_paired_ttl(Duration::from_millis(300)),
    );
    let p = pool.clone();
    tokio::task::spawn_blocking(move || {
        for _ in 0..11 {
            let _ = require_in(&p, "192.0.2.9", ava1::gen::CAP_DATA_PLANE);
            std::thread::sleep(Duration::from_millis(50));
        }
    })
    .await
    .unwrap();
    assert!(pool.attempts() <= 2, "{} handshakes", pool.attempts());
    assert!(pool.attempts() >= 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_poll_never_knocks_while_the_dialog_is_waiting_on_a_code() {
    use ps5upload_ava1::console::require_in;
    let d = dir("pool-poll-pending");
    let srv = CServer::start_with(SECRET, &d.join("srv-peers"), opts(60));
    let pool = Arc::new(
        Pool::new(d.join("ava"))
            .with_addr(srv.addr())
            .with_not_paired_ttl(Duration::from_millis(1)),
    );
    start_pairing(&pool).await;
    let before = pool.attempts();
    let p = pool.clone();
    tokio::task::spawn_blocking(move || {
        for _ in 0..5 {
            let _ = require_in(&p, "192.0.2.9", ava1::gen::CAP_DATA_PLANE);
            std::thread::sleep(Duration::from_millis(20));
        }
    })
    .await
    .unwrap();
    assert_eq!(
        pool.attempts(),
        before,
        "the pending handshake is the only one"
    );
}
