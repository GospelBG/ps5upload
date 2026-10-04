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
    p.confirm_pairing(p.pairing_code().unwrap()).await.unwrap();
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
    first
        .confirm_pairing(first.pairing_code().unwrap())
        .await
        .unwrap();
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
    second
        .confirm_pairing(second.pairing_code().unwrap())
        .await
        .unwrap();
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
    let r = s.confirm_pairing(s.pairing_code().unwrap()).await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_devices_confirming_at_the_same_moment_get_one_pairing() {
    // The owner's approval takes a while (a prompt, a slow disk): long enough for a
    // second PairConfirm to arrive while the first is still being decided.
    let ctx = ServerCtx::new(
        Identity::generate().unwrap(),
        "console",
        PeerStore::in_memory(),
        node_info_rpc("console"),
    )
    .with_timing(fast())
    .with_approve(Box::new(|_| {
        std::thread::sleep(Duration::from_millis(150));
        true
    }));
    assert!(ctx.open_pairing_if_unpaired(Duration::from_secs(60)));
    let (addr, ctx) = start(ctx).await;
    let (mut a, mut b) = (stranger(addr).await.unwrap(), stranger(addr).await.unwrap());
    let (ra, rb) = tokio::join!(
        a.confirm_pairing(a.pairing_code().unwrap()),
        b.confirm_pairing(b.pairing_code().unwrap())
    );
    assert_eq!(
        u8::from(ra.is_ok()) + u8::from(rb.is_ok()),
        1,
        "one window, one pairing: {ra:?} {rb:?}"
    );
    assert!(!ctx.pairing_open());
}

// ---- passkey entry (SPEC.md §4.6, §5.5): the console checks the code the user typed ----

use ava1::gen::{PairConfirm, PairResult};
use ava1::wire::{FrameMessage, Message};
use common::{RawReader, RawWriter};

struct Rogue {
    r: RawReader,
    w: RawWriter,
    code: u32,
    key: [u8; 32],
}

/// A LAN host with a throwaway key: completes Noise and is welcomed with knows_you = 0.
/// `code` is what its own transcript derived; the user's code comes off the console's
/// screen, which a host on the wire cannot read.
async fn rogue(addr: std::net::SocketAddr) -> Rogue {
    let me = Identity::generate().unwrap();
    let sock = TcpStream::connect(addr).await.unwrap();
    let (rh, wh) = sock.into_split();
    let (mut r, mut w) = (
        ava1::conn::FrameReader::new(rh),
        ava1::conn::FrameWriter::new(wh),
    );
    let est = ava1::handshake::client(&mut r, &mut w, &me, "rogue", |_| false, 0)
        .await
        .unwrap();
    assert!(est.pairing.is_some_and(|p| p.server_must_confirm));
    Rogue {
        r,
        w,
        code: est.code,
        key: me.public(),
    }
}

impl Rogue {
    /// Sends `body` as a PairConfirm; true when the console accepted.
    async fn confirm_raw(&mut self, body: &[u8]) -> bool {
        self.w.send(PairConfirm::TYPE, 1, body).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match self.r.recv().await {
                    Ok(f) if f.ty == PairResult::TYPE => {
                        return f.decode::<PairResult>().unwrap().accepted != 0
                    }
                    Ok(_) => {} // pings, notices
                    Err(_) => return false,
                }
            }
        })
        .await
        .unwrap_or(false)
    }
    async fn confirm(&mut self, code: u32) -> bool {
        self.confirm_raw(&PairConfirm { code }.to_bytes().unwrap())
            .await
    }
}

fn wrong(code: u32) -> u32 {
    (code + 1) % 1_000_000
}

#[tokio::test]
async fn a_pairconfirm_without_the_code_is_refused_and_not_stored() {
    let (addr, ctx, _) = open_server(Limits::default()).await;
    let mut g = rogue(addr).await;
    // The old, empty PairConfirm: it carries no code field.
    assert!(!g.confirm_raw(&[0, 0]).await);
    assert!(
        !ctx.knows(&g.key),
        "a stranger without the code is not stored"
    );
}

#[tokio::test]
async fn a_pairconfirm_with_a_wrong_code_is_refused_and_not_stored() {
    let (addr, ctx, _) = open_server(Limits::default()).await;
    let mut g = rogue(addr).await;
    let bad = wrong(g.code);
    assert!(!g.confirm(bad).await);
    assert!(!ctx.knows(&g.key));
    assert!(ctx.pairing_open(), "one failure does not close the window");
}

#[tokio::test]
async fn the_right_code_pairs() {
    let (addr, ctx, _) = open_server(Limits::default()).await;
    let mut g = rogue(addr).await;
    let ok = g.code;
    assert!(g.confirm(ok).await);
    assert!(ctx.knows(&g.key));
    assert!(!ctx.pairing_open());
}

#[tokio::test]
async fn five_wrong_codes_close_the_window() {
    let (addr, ctx, _) = open_server(Limits::default()).await;
    for i in 0..ava1::server::MAX_PAIR_FAILURES {
        assert!(ctx.pairing_open(), "still open before failure {i}");
        let mut g = rogue(addr).await;
        let bad = wrong(g.code);
        assert!(!g.confirm(bad).await);
        drop(g);
        wait_for(Duration::from_secs(5), || ctx.sessions() == 0)
            .await
            .expect("the refused session is gone");
    }
    assert!(!ctx.pairing_open(), "the window closed");
    let r = stranger(addr).await;
    assert!(is_pairing_closed(&r), "{r:?}");
    // Reopened (by a paired device or a restart), it pairs again.
    ctx.open_pairing(Duration::from_secs(60));
    let mut g = rogue(addr).await;
    let ok = g.code;
    assert!(g.confirm(ok).await, "reopened: pairs");
}

#[tokio::test]
async fn a_session_gets_one_attempt() {
    let (addr, ctx, _) = open_server(Limits::default()).await;
    let mut g = rogue(addr).await;
    let bad = wrong(g.code);
    assert!(!g.confirm(bad).await);
    // The same session, now with the right code: closed already.
    let ok = PairConfirm { code: g.code }.to_bytes().unwrap();
    let _ = g.w.send(PairConfirm::TYPE, 2, &ok).await;
    let got = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match g.r.recv().await {
                Ok(f) if f.ty == PairResult::TYPE => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
    })
    .await;
    assert!(!matches!(got, Ok(true)), "closed after one attempt");
    assert!(!ctx.knows(&g.key));
}

#[tokio::test]
async fn a_mismatched_code_is_refused_before_it_is_sent_and_stores_nothing() {
    // The man in the middle: the app's own code differs from the one on the console's
    // screen (the user types what the console shows).
    let (addr, ctx, _) = open_server(Limits::default()).await;
    let mut s = stranger(addr).await.unwrap();
    let mine = s.pairing_code().unwrap();
    let r = s.confirm_pairing(wrong(mine)).await;
    assert!(
        matches!(&r, Err(Ava1Error::Refused { code, .. }) if *code == gen::ERR_PAIRING_CODE),
        "{r:?}"
    );
    assert_eq!(ctx.pair_failures(), 0, "it never reached the console");
    s.confirm_pairing(mine).await.unwrap();
    s.node_info().await.unwrap();
}
