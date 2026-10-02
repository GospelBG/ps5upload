//! Pairing policy (SPEC.md §5, §8): who may hold an unconfirmed session and for how
//! long, how many connections one address gets, and what a server does about a peers
//! file it cannot read.
mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ava1::gen;
use ava1::keys::Identity;
use ava1::peers::PeerStore;
use ava1::server::{Limits, ServerCtx};
use ava1::session::{connect, Session};
use ava1::Ava1Error;
use common::*;
use tokio::net::TcpStream;

/// An unpaired server with its window open for a minute; counts pairing notifications.
async fn open_server(limits: Limits) -> (std::net::SocketAddr, Arc<ServerCtx>, Arc<AtomicUsize>) {
    let shown = Arc::new(AtomicUsize::new(0));
    let n = shown.clone();
    let ctx = ServerCtx::new(
        Identity::generate().unwrap(),
        "console",
        PeerStore::in_memory(),
        node_info_rpc("console"),
    )
    .with_timing(fast())
    .with_limits(limits)
    .with_notify(Box::new(move |_| {
        n.fetch_add(1, Ordering::SeqCst);
    }));
    assert!(ctx.open_pairing_if_unpaired(Duration::from_secs(60)));
    let (addr, ctx) = start(ctx).await;
    (addr, ctx, shown)
}

async fn stranger(addr: std::net::SocketAddr) -> Result<Session, Ava1Error> {
    connect(
        &addr.to_string(),
        Arc::new(Identity::generate().unwrap()),
        Arc::new(Mutex::new(PeerStore::in_memory())),
        "phone",
        fast(),
    )
    .await
}

fn is_busy<T: std::fmt::Debug>(r: &Result<T, Ava1Error>) -> bool {
    matches!(r, Err(Ava1Error::Refused { code, .. }) if *code == gen::ERR_BUSY)
}

fn is_pairing_closed<T: std::fmt::Debug>(r: &Result<T, Ava1Error>) -> bool {
    matches!(r, Err(Ava1Error::Refused { code, .. }) if *code == gen::ERR_PAIRING_CLOSED)
}

#[tokio::test]
async fn an_unconfirmed_session_ends_when_the_window_closes() {
    let (addr, ctx, _) = open_server(Limits::default()).await;
    let s = stranger(addr).await.unwrap();
    assert!(s.pairing_code().is_some());
    assert_eq!(ctx.sessions(), 1);
    ctx.close_pairing();
    let why = tokio::time::timeout(Duration::from_secs(5), s.closed())
        .await
        .expect("the unconfirmed session was closed");
    assert!(why.contains("not confirmed"), "{why}");
    wait_for(Duration::from_secs(5), || ctx.sessions() == 0)
        .await
        .expect("its slot is free");
}

#[tokio::test]
async fn an_unconfirmed_session_ends_at_the_confirm_deadline() {
    let limits = Limits {
        pair_confirm: Duration::from_millis(400),
        ..Limits::default()
    };
    let (addr, ctx, _) = open_server(limits).await;
    let s = stranger(addr).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), s.closed())
        .await
        .expect("closed at the deadline");
    assert!(ctx.pairing_open(), "the window itself is still open");
    // A paired session is not subject to the deadline.
    let mut p = stranger(addr).await.unwrap();
    p.confirm_pairing().await.unwrap();
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(!p.is_closed());
    p.node_info().await.unwrap();
}

#[tokio::test]
async fn at_most_two_devices_wait_to_be_confirmed() {
    let (addr, ctx, _) = open_server(Limits::default()).await;
    let a = stranger(addr).await.unwrap();
    let _b = stranger(addr).await.unwrap();
    let c = stranger(addr).await;
    assert!(is_busy(&c), "{c:?}");
    // A place frees when one of them leaves.
    a.close().await;
    wait_for(Duration::from_secs(5), || ctx.sessions() == 1)
        .await
        .unwrap();
    let t = std::time::Instant::now();
    loop {
        match stranger(addr).await {
            Ok(_) => break,
            r => assert!(is_busy(&r) && t.elapsed() < Duration::from_secs(5), "{r:?}"),
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn one_address_holds_at_most_twelve_connections() {
    let (addr, ctx, me, peers) = paired().await;
    let mut held = Vec::new();
    for _ in 0..ava1::server::MAX_CONNS_PER_IP {
        held.push(TcpStream::connect(addr).await.unwrap());
    }
    wait_for(Duration::from_secs(5), || {
        ctx.connections() == ava1::server::MAX_CONNS_PER_IP
    })
    .await
    .expect("twelve accepted");
    let r = connect(&addr.to_string(), me.clone(), peers.clone(), "c", fast()).await;
    assert!(is_busy(&r), "{r:?}");
    held.pop();
    wait_for(Duration::from_secs(5), || {
        ctx.connections() < ava1::server::MAX_CONNS_PER_IP
    })
    .await
    .unwrap();
    connect(&addr.to_string(), me, peers, "c", fast())
        .await
        .unwrap();
}

#[tokio::test]
async fn pairing_notifications_are_rate_limited() {
    let (addr, _ctx, shown) = open_server(Limits::default()).await;
    let _a = stranger(addr).await.unwrap();
    let _b = stranger(addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(shown.load(Ordering::SeqCst), 1, "one notification per 10 s");

    let limits = Limits {
        notify_every: Duration::from_millis(100),
        ..Limits::default()
    };
    let (addr, _ctx, shown) = open_server(limits).await;
    let _a = stranger(addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    let _b = stranger(addr).await.unwrap();
    wait_for(Duration::from_secs(5), || shown.load(Ordering::SeqCst) == 2)
        .await
        .expect("a later request is shown again");
}

#[tokio::test]
async fn a_successful_pairing_closes_the_window() {
    // The automatic window.
    let (addr, ctx, _) = open_server(Limits::default()).await;
    let mut first = stranger(addr).await.unwrap();
    let waiting = stranger(addr).await.unwrap();
    first.confirm_pairing().await.unwrap();
    assert!(!ctx.pairing_open(), "one window, one pairing");
    let r = stranger(addr).await;
    assert!(is_pairing_closed(&r), "{r:?}");
    // The other device that was waiting is let go too.
    tokio::time::timeout(Duration::from_secs(5), waiting.closed())
        .await
        .expect("the other unconfirmed session ends with the window");
    // A window opened by pairing.open closes the same way.
    first.open_pairing(60).await.unwrap();
    assert!(ctx.pairing_open());
    let mut second = stranger(addr).await.unwrap();
    second.confirm_pairing().await.unwrap();
    assert!(!ctx.pairing_open());
    assert!(!first.is_closed());
    first.node_info().await.unwrap();
}

#[tokio::test]
async fn an_unreadable_peers_file_keeps_pairing_closed_and_is_never_overwritten() {
    // A directory where the file should be: it exists, and reading it fails.
    let d = temp_dir("unreadable");
    let path = d.join("peers");
    std::fs::create_dir(&path).unwrap();
    assert!(PeerStore::load(&path).is_err());
    let mut store = PeerStore::load_or_unreadable(&path);
    assert!(store.unreadable().is_some());
    assert!(store.add([1; 32], "x").is_err(), "never written");
    assert!(!store.contains(&[1; 32]));
    assert!(path.is_dir(), "left alone");
    assert!(!d.join("peers.tmp").exists());

    let logged = Arc::new(Mutex::new(Vec::<String>::new()));
    let l2 = logged.clone();
    let ctx = ServerCtx::new(
        Identity::generate().unwrap(),
        "console",
        store,
        node_info_rpc("console"),
    )
    .with_timing(fast())
    .with_log(Box::new(move |m| l2.lock().unwrap().push(m.to_string())));
    assert!(
        !ctx.open_pairing_if_unpaired(Duration::from_secs(60)),
        "unknown peers is not no peers"
    );
    assert!(logged.lock().unwrap()[0].contains("could not be read"));
    let (addr, ctx) = start(ctx).await;
    let r = stranger(addr).await;
    assert!(is_pairing_closed(&r), "{r:?}");
    // Even with the window forced open, a pairing that cannot be stored is refused.
    ctx.open_pairing(Duration::from_secs(60));
    let mut s = stranger(addr).await.unwrap();
    let r = s.confirm_pairing().await;
    assert!(matches!(r, Err(Ava1Error::Refused { .. })), "{r:?}");
    assert!(path.is_dir());
    assert!(logged
        .lock()
        .unwrap()
        .iter()
        .any(|m| m.contains("not stored")));
}

#[test]
fn a_missing_peers_file_is_an_empty_store_not_an_unreadable_one() {
    let d = temp_dir("missing");
    let mut store = PeerStore::load_or_unreadable(&d.join("peers"));
    assert!(store.unreadable().is_none());
    store.add([2; 32], "x").unwrap();
    assert!(PeerStore::load(&d.join("peers"))
        .unwrap()
        .contains(&[2; 32]));
}
