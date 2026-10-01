mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ava1::gen;
use ava1::keys::Identity;
use ava1::peers::PeerStore;
use ava1::server::ServerCtx;
use ava1::session::connect;
use ava1::Ava1Error;
use common::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[tokio::test]
async fn paired_devices_connect_and_call_node_info() {
    let (addr, _ctx, me, peers) = paired().await;
    let s = connect(&addr.to_string(), me, peers, "laptop", fast())
        .await
        .unwrap();
    assert_eq!(s.pairing_code(), None);
    assert_eq!(s.peer_name(), "Rust test server");
    assert_eq!(s.node_info().await.unwrap().name, "Rust test server");
    let r = s.rpc(999, &[]).await.unwrap();
    assert_eq!(r.status, gen::ERR_UNKNOWN_METHOD);
}

#[tokio::test]
async fn heartbeats_keep_an_idle_session_alive_and_measure_rtt() {
    let (addr, _ctx, me, peers) = paired().await;
    let s = connect(&addr.to_string(), me, peers, "laptop", fast())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await; // 3× dead_after with no traffic of ours
    assert!(!s.is_closed());
    assert!(s.rtt().is_some());
    s.node_info().await.unwrap();
}

#[tokio::test]
async fn an_unknown_client_is_refused_when_pairing_is_closed() {
    let ctx = ServerCtx::new(
        Identity::generate().unwrap(),
        "s",
        PeerStore::in_memory(),
        node_info_rpc("s"),
    );
    let (addr, _ctx) = start(ctx.with_timing(fast())).await;
    let r = connect(
        &addr.to_string(),
        Arc::new(Identity::generate().unwrap()),
        Arc::new(Mutex::new(PeerStore::in_memory())),
        "c",
        fast(),
    )
    .await;
    assert!(matches!(r, Err(Ava1Error::Refused { code, .. }) if code == gen::ERR_PAIRING_CLOSED));
}

#[tokio::test]
async fn pairing_shows_the_same_code_and_persists_on_both_sides() {
    let dir = temp_dir("pair");
    let seen = Arc::new(Mutex::new(None));
    let seen2 = seen.clone();
    let ctx = ServerCtx::new(
        Identity::generate().unwrap(),
        "console",
        PeerStore::load(&dir.join("server-peers")).unwrap(),
        node_info_rpc("console"),
    )
    .with_timing(fast())
    .with_notify(Box::new(move |r| *seen2.lock().unwrap() = Some(r.code)));
    ctx.open_pairing(Duration::from_secs(60));
    let (addr, _ctx) = start(ctx).await;
    let me = Arc::new(Identity::generate().unwrap());
    let peers = Arc::new(Mutex::new(
        PeerStore::load(&dir.join("client-peers")).unwrap(),
    ));

    let mut s = connect(
        &addr.to_string(),
        me.clone(),
        peers.clone(),
        "laptop",
        fast(),
    )
    .await
    .unwrap();
    let code = s.pairing_code().expect("pairing needed");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        *seen.lock().unwrap(),
        Some(code),
        "the server shows the same code"
    );
    assert_eq!(
        s.rpc(gen::METHOD_NODE_INFO, &[]).await.unwrap().status,
        gen::ERR_NOT_PAIRED
    );
    s.confirm_pairing().await.unwrap();
    assert_eq!(s.node_info().await.unwrap().name, "console");

    let server_file = std::fs::read_to_string(dir.join("server-peers")).unwrap();
    assert!(
        server_file.contains(&ava1::hex::encode(&me.public())),
        "{server_file}"
    );
    assert!(server_file.trim_end().ends_with(" laptop"));
    // A new connection needs no pairing on either side.
    let again = connect(&addr.to_string(), me, peers, "laptop", fast())
        .await
        .unwrap();
    assert_eq!(again.pairing_code(), None);
}

#[tokio::test]
async fn a_server_that_declines_the_pairing_is_reported() {
    let ctx = ServerCtx::new(
        Identity::generate().unwrap(),
        "console",
        PeerStore::in_memory(),
        node_info_rpc("console"),
    )
    .with_timing(fast())
    .with_approve(Box::new(|_| false));
    ctx.open_pairing(Duration::from_secs(60));
    let (addr, _ctx) = start(ctx).await;
    let mut s = connect(
        &addr.to_string(),
        Arc::new(Identity::generate().unwrap()),
        Arc::new(Mutex::new(PeerStore::in_memory())),
        "c",
        fast(),
    )
    .await
    .unwrap();
    let r = s.confirm_pairing().await;
    assert!(matches!(r, Err(Ava1Error::Refused { .. })), "{r:?}");
}

#[tokio::test]
async fn a_silent_half_handshake_is_dropped() {
    // Review focus 1.
    let (addr, ctx, me, peers) = paired().await;
    let mut raw = TcpStream::connect(addr).await.unwrap();
    raw.write_all(b"A1\x01\x00\x00").await.unwrap(); // 5 of 16 header bytes, then silence
    let t = Instant::now();
    let mut buf = [0u8; 1];
    let n = tokio::time::timeout(Duration::from_secs(3), raw.read(&mut buf))
        .await
        .unwrap()
        .unwrap_or(0);
    assert_eq!(n, 0, "server closed the half-open handshake");
    assert!(t.elapsed() < Duration::from_millis(1500));
    // Everyone else was served meanwhile.
    connect(&addr.to_string(), me, peers, "c", fast())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(ctx.connections() <= 1);
}

#[tokio::test]
async fn a_connection_storm_is_refused_then_recovers() {
    // Review focus 2.
    let (addr, ctx, me, peers) = paired().await;
    let mut held = Vec::new();
    for _ in 0..ava1::server::MAX_CONNS {
        held.push(TcpStream::connect(addr).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let r = connect(&addr.to_string(), me.clone(), peers.clone(), "c", fast()).await;
    assert!(
        matches!(r, Err(Ava1Error::Refused { code, .. }) if code == gen::ERR_BUSY),
        "{r:?}"
    );
    drop(held);
    let t = Instant::now();
    while ctx.connections() > 0 && t.elapsed() < Duration::from_secs(3) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    connect(&addr.to_string(), me, peers, "c", fast())
        .await
        .unwrap();
}

#[tokio::test]
async fn closing_a_session_says_goodbye() {
    let (addr, ctx, me, peers) = paired().await;
    let s = connect(&addr.to_string(), me, peers, "c", fast())
        .await
        .unwrap();
    assert_eq!(ctx.sessions(), 1);
    s.close().await;
    let t = Instant::now();
    while ctx.sessions() > 0 && t.elapsed() < Duration::from_secs(2) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(ctx.sessions(), 0);
}

#[tokio::test]
async fn a_paired_device_opens_the_pairing_window_for_another() {
    let (addr, ctx, me, peers) = paired().await;
    assert!(
        !ctx.open_pairing_if_unpaired(Duration::from_secs(60)),
        "a paired node keeps its window shut"
    );
    let stranger = Arc::new(Identity::generate().unwrap());
    let none = Arc::new(Mutex::new(PeerStore::in_memory()));
    let r = connect(
        &addr.to_string(),
        stranger.clone(),
        none.clone(),
        "phone",
        fast(),
    )
    .await;
    assert!(matches!(r, Err(Ava1Error::Refused { code, .. }) if code == gen::ERR_PAIRING_CLOSED));
    let s = connect(&addr.to_string(), me, peers, "laptop", fast())
        .await
        .unwrap();
    s.open_pairing(60).await.unwrap();
    let p = connect(&addr.to_string(), stranger, none, "phone", fast())
        .await
        .unwrap();
    assert!(p.pairing_code().is_some());
}

#[tokio::test]
async fn an_unpaired_server_opens_its_own_window() {
    let ctx = ServerCtx::new(
        Identity::generate().unwrap(),
        "s",
        PeerStore::in_memory(),
        node_info_rpc("s"),
    );
    assert!(ctx.open_pairing_if_unpaired(Duration::from_secs(60)));
    assert!(ctx.pairing_open());
}

#[tokio::test]
async fn a_slow_call_does_not_stop_liveness_or_other_calls() {
    // Design-review flaw 4: a 1.5 s call on a 500 ms dead_after session.
    let (s_id, c_id) = (
        Identity::generate().unwrap(),
        Arc::new(Identity::generate().unwrap()),
    );
    let mut sp = PeerStore::in_memory();
    sp.add(c_id.public(), "c").unwrap();
    let mut cp = PeerStore::in_memory();
    cp.add(s_id.public(), "s").unwrap();
    let rpc: ava1::server::RpcHandler = Box::new(|method, _| {
        if method == 77 {
            std::thread::sleep(Duration::from_millis(1500));
        }
        ava1::session::RpcReply {
            status: gen::STATUS_OK,
            body: vec![method as u8],
        }
    });
    let (addr, _ctx) = start(ServerCtx::new(s_id, "s", sp, rpc).with_timing(fast())).await;
    let s = Arc::new(
        connect(
            &addr.to_string(),
            c_id,
            Arc::new(Mutex::new(cp)),
            "c",
            fast(),
        )
        .await
        .unwrap(),
    );
    let s2 = s.clone();
    let slow = tokio::spawn(async move { s2.rpc(77, &[]).await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        s.rpc(5, &[]).await.unwrap().body,
        vec![5],
        "a fast call is not stuck behind the slow one"
    );
    assert_eq!(slow.await.unwrap().unwrap().body, vec![77]);
    assert!(!s.is_closed(), "liveness held through the slow call");
}

#[test]
fn identity_file_of_wrong_length_is_an_error_not_a_new_key() {
    // Review focus 4 (covered in keys.rs too; pinned here at the integration boundary).
    let d = temp_dir("id");
    let p = d.join("identity");
    std::fs::write(&p, [1u8; 31]).unwrap();
    assert!(Identity::load_or_create(&p).is_err());
    assert_eq!(std::fs::read(&p).unwrap(), vec![1u8; 31]);
}

#[test]
fn peer_file_skips_garbage_lines() {
    // Review focus 4.
    let d = temp_dir("peers");
    let p = d.join("peers");
    let good = "ab".repeat(32);
    std::fs::write(
        &p,
        format!(
            "garbage\n{good} 1700000000 Pro\nzz{} 1 x\n\u{0}\u{ff}\n{good}1 2 short\n",
            "a".repeat(62)
        ),
    )
    .unwrap();
    let s = PeerStore::load(&p).unwrap();
    assert_eq!(s.list().len(), 1);
    assert_eq!(s.list()[0].name, "Pro");
    assert!(s.contains(&[0xab; 32]));
}

#[test]
fn peer_store_keeps_at_most_32_and_replaces_by_key() {
    let d = temp_dir("cap");
    let mut s = PeerStore::load(&d.join("peers")).unwrap();
    for i in 0..40u8 {
        s.add([i; 32], &format!("n{i}")).unwrap();
    }
    assert_eq!(s.list().len(), ava1::peers::MAX_PEERS);
    assert!(!s.contains(&[0; 32]) && s.contains(&[39; 32]));
    s.add([39; 32], "renamed\nline").unwrap();
    let again = PeerStore::load(&d.join("peers")).unwrap();
    assert_eq!(again.list().last().unwrap().name, "renamed line");
    assert_eq!(again.list().len(), ava1::peers::MAX_PEERS);
}
