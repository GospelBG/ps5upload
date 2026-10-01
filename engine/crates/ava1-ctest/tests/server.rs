#![cfg(unix)]
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ava1::conn::{FrameReader, FrameWriter};
use ava1::gen;
use ava1::keys::Identity;
use ava1::peers::PeerStore;
use ava1::session::{connect, Timing};
use ava1::Ava1Error;
use ava1_chaos::{ChaosConfig, ChaosProxy};
use ava1_ctest::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const SECRET: [u8; 32] = [0x42; 32];

fn fast() -> Timing {
    Timing {
        ping_every: Duration::from_millis(100),
        dead_after: Duration::from_millis(500),
        handshake: Duration::from_millis(500),
        ..Timing::default()
    }
}

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ava1-c-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A client the C server already knows, and that knows the C server.
fn paired_client(peers_file: &std::path::Path) -> (Arc<Identity>, Arc<Mutex<PeerStore>>) {
    let me = Arc::new(Identity::generate().unwrap());
    PeerStore::load(peers_file)
        .unwrap()
        .add(me.public(), "rust client")
        .unwrap();
    let mut mine = PeerStore::in_memory();
    mine.add(Identity::from_secret(SECRET).public(), "C test server")
        .unwrap();
    (me, Arc::new(Mutex::new(mine)))
}

async fn wait_conns(s: &CServer, n: i32) {
    let t = Instant::now();
    while s.conns() > n && t.elapsed() < Duration::from_secs(3) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        s.conns() <= n,
        "C server still has {} connections",
        s.conns()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rust_client_talks_to_the_c_server() {
    let d = dir("basic");
    let (me, peers) = paired_client(&d.join("peers"));
    let srv = CServer::start(SECRET, &d.join("peers"), 0, 100, 500, 500);
    let s = connect(&srv.addr(), me, peers, "laptop", fast())
        .await
        .unwrap();
    assert_eq!(s.pairing_code(), None);
    assert_eq!(s.peer_name(), "C test server");
    assert_eq!(s.node_info().await.unwrap().name, "C test server");
    assert_eq!(
        s.rpc(999, &[]).await.unwrap().status,
        gen::ERR_UNKNOWN_METHOD
    );
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(!s.is_closed() && s.rtt().is_some(), "heartbeats both ways");
}

#[tokio::test(flavor = "multi_thread")]
async fn pairing_with_the_c_server() {
    let d = dir("pair");
    let srv = CServer::start(SECRET, &d.join("peers"), 60, 100, 500, 500);
    let me = Arc::new(Identity::generate().unwrap());
    let peers = Arc::new(Mutex::new(PeerStore::in_memory()));
    let mut s = connect(&srv.addr(), me.clone(), peers.clone(), "laptop", fast())
        .await
        .unwrap();
    let code = s.pairing_code().unwrap();
    // The server shows the code once it has checked our Auth; connect() can return first.
    let t = Instant::now();
    while srv.pair_requests().0 == 0 && t.elapsed() < Duration::from_secs(1) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        srv.pair_requests(),
        (1, code),
        "the console shows the same code"
    );
    assert_eq!(
        s.rpc(gen::METHOD_NODE_INFO, &[]).await.unwrap().status,
        gen::ERR_NOT_PAIRED
    );
    assert!(matches!(s.open_lane().await, Err(Ava1Error::NotPaired)));
    s.confirm_pairing().await.unwrap();
    s.node_info().await.unwrap();
    let file = std::fs::read_to_string(d.join("peers")).unwrap();
    assert!(
        file.contains(&ava1::hex::encode(&me.public())) && file.contains(" laptop"),
        "{file}"
    );
    // Rust reads what C wrote.
    assert!(PeerStore::load(&d.join("peers"))
        .unwrap()
        .contains(&me.public()));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_c_server_refuses_strangers_when_pairing_is_closed() {
    let d = dir("closed");
    let srv = CServer::start(SECRET, &d.join("peers"), 0, 100, 500, 500);
    let r = connect(
        &srv.addr(),
        Arc::new(Identity::generate().unwrap()),
        Arc::new(Mutex::new(PeerStore::in_memory())),
        "x",
        fast(),
    )
    .await;
    assert!(matches!(r, Err(Ava1Error::Refused { code, .. }) if code == gen::ERR_PAIRING_CLOSED));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_c_server_window_stays_shut_once_paired_until_a_peer_opens_it() {
    let d = dir("window");
    let (me, peers) = paired_client(&d.join("peers"));
    // pairing_s = 60, but the peers file is not empty: no automatic window (design review, flaw 2).
    let srv = CServer::start(SECRET, &d.join("peers"), 60, 100, 500, 500);
    let stranger = Arc::new(Identity::generate().unwrap());
    let none = Arc::new(Mutex::new(PeerStore::in_memory()));
    let r = connect(&srv.addr(), stranger.clone(), none.clone(), "phone", fast()).await;
    assert!(
        matches!(r, Err(Ava1Error::Refused { code, .. }) if code == gen::ERR_PAIRING_CLOSED),
        "{r:?}"
    );
    let s = connect(&srv.addr(), me, peers, "laptop", fast())
        .await
        .unwrap();
    s.open_pairing(60).await.unwrap();
    let p = connect(&srv.addr(), stranger, none, "phone", fast())
        .await
        .unwrap();
    assert!(p.pairing_code().is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn lanes_on_the_c_server() {
    let d = dir("lanes");
    let (me, peers) = paired_client(&d.join("peers"));
    let srv = CServer::start(SECRET, &d.join("peers"), 0, 100, 500, 500);
    let s = connect(&srv.addr(), me, peers, "laptop", fast())
        .await
        .unwrap();
    let mut lanes = Vec::new();
    for _ in 0..gen::MAX_LANES {
        lanes.push(s.open_lane().await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(lanes.iter().all(|l| !l.is_closed() && l.rtt().is_some()));
    // Superseding: the same id again replaces the old connection.
    let old = lanes.remove(2);
    let _new = s.reopen_lane_for_test(old.id).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), old.closed())
        .await
        .expect("old lane closed by the C server");
    // Lanes end with their session.
    s.close().await;
    for l in &lanes {
        tokio::time::timeout(Duration::from_secs(2), l.closed())
            .await
            .expect("lane ended with session");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_c_server_rejects_forged_and_replayed_frames() {
    let d = dir("forged");
    let (me, peers) = paired_client(&d.join("peers"));
    let srv = CServer::start(SECRET, &d.join("peers"), 0, 100, 500, 500);
    let _s = connect(&srv.addr(), me, peers, "laptop", fast())
        .await
        .unwrap();
    // A forged Join.
    let (rh, wh) = TcpStream::connect(srv.addr()).await.unwrap().into_split();
    let (mut r, mut w) = (FrameReader::new(rh), FrameWriter::new(wh));
    w.send_msg(
        0,
        &gen::Join {
            session_id: [1; 16],
            lane_id: 1,
            client_nonce: [2; 16],
            tag: [3; 16],
        },
    )
    .await
    .unwrap();
    assert_eq!(
        r.recv().await.unwrap().decode::<gen::Error>().unwrap().code,
        gen::ERR_BAD_JOIN
    );
    // A tagged frame with the wrong key on a fresh control connection: the C server just closes.
    let (rh, wh) = TcpStream::connect(srv.addr()).await.unwrap().into_split();
    let (mut r, mut w) = (FrameReader::new(rh), FrameWriter::new(wh));
    w.set_key([9; 32]);
    w.send_msg(0, &gen::Ping { seq: 1, t_us: 1 }).await.unwrap();
    assert!(r.recv().await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn c_server_drops_a_silent_half_handshake() {
    // Review focus 1.
    let d = dir("slow");
    let (me, peers) = paired_client(&d.join("peers"));
    let srv = CServer::start(SECRET, &d.join("peers"), 0, 100, 500, 300);
    let mut raw = TcpStream::connect(srv.addr()).await.unwrap();
    raw.write_all(b"A1\x01\x00\x00").await.unwrap();
    let t = Instant::now();
    let mut b = [0u8; 1];
    let n = tokio::time::timeout(Duration::from_secs(3), raw.read(&mut b))
        .await
        .unwrap()
        .unwrap_or(0);
    assert_eq!(n, 0);
    assert!(
        t.elapsed() < Duration::from_millis(1200),
        "{:?}",
        t.elapsed()
    );
    connect(&srv.addr(), me, peers, "laptop", fast())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn c_server_refuses_a_storm_then_recovers() {
    // Review focus 2.
    let d = dir("storm");
    let (me, peers) = paired_client(&d.join("peers"));
    let srv = CServer::start(SECRET, &d.join("peers"), 0, 100, 500, 2000);
    let mut held = Vec::new();
    for _ in 0..64 {
        held.push(TcpStream::connect(srv.addr()).await.unwrap());
    }
    let t = Instant::now();
    while srv.conns() < 64 && t.elapsed() < Duration::from_secs(2) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let r = connect(&srv.addr(), me.clone(), peers.clone(), "laptop", fast()).await;
    assert!(
        matches!(r, Err(Ava1Error::Refused { code, .. }) if code == gen::ERR_BUSY),
        "{r:?}"
    );
    drop(held);
    wait_conns(&srv, 0).await;
    connect(&srv.addr(), me, peers, "laptop", fast())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn c_server_drops_a_blackholed_session() {
    // Review focus 3.
    let d = dir("blackhole");
    let (me, peers) = paired_client(&d.join("peers"));
    let srv = CServer::start(SECRET, &d.join("peers"), 0, 100, 500, 500);
    let proxy = ChaosProxy::start(srv.addr().parse().unwrap(), ChaosConfig::default())
        .await
        .unwrap();
    let s = connect(&proxy.addr.to_string(), me, peers, "laptop", fast())
        .await
        .unwrap();
    let _lane = s.open_lane().await.unwrap();
    assert_eq!(srv.conns(), 2);
    proxy.blackhole(true);
    let t = Instant::now();
    wait_conns(&srv, 0).await;
    assert!(
        t.elapsed() < Duration::from_millis(1500),
        "{:?}",
        t.elapsed()
    );
    let why = tokio::time::timeout(Duration::from_secs(2), s.closed())
        .await
        .unwrap();
    assert!(why.contains("stopped answering"), "{why}");
}

#[test]
fn c_store_rejects_bad_identity_and_skips_bad_peers() {
    // Review focus 4.
    let d = dir("store");
    let id = d.join("identity");
    let a = c_identity_load_or_create(&id).unwrap();
    assert_eq!(c_identity_load_or_create(&id).unwrap(), a);
    assert_eq!(
        Identity::load_or_create(&id).unwrap().public(),
        a,
        "Rust reads C's identity file"
    );
    std::fs::write(&id, [7u8; 33]).unwrap();
    assert!(c_identity_load_or_create(&id).is_err());
    assert_eq!(std::fs::read(&id).unwrap(), vec![7u8; 33], "never replaced");

    let peers = d.join("peers");
    let good = "cd".repeat(32);
    std::fs::write(
        &peers,
        format!("junk\n{good} 1700000000 Phat\nxx\n{good}9 1 y\n\n"),
    )
    .unwrap();
    assert_eq!(c_peers_load(&peers, &[0xcd; 32]), (1, true));
    assert_eq!(c_peers_load(&d.join("missing"), &[0; 32]), (0, false));
}

#[tokio::test(flavor = "multi_thread")]
async fn c_server_drops_a_trickling_handshake() {
    // The handshake deadline is absolute: a client that keeps feeding bytes is still cut.
    let d = dir("trickle");
    let (me, peers) = paired_client(&d.join("peers"));
    let srv = CServer::start(SECRET, &d.join("peers"), 0, 100, 500, 500);
    let mut raw = TcpStream::connect(srv.addr()).await.unwrap();
    let t = Instant::now();
    let mut header = [0u8; 16];
    header[..3].copy_from_slice(b"A1\x01");
    let mut closed = false;
    for b in header {
        if raw.write_all(&[b]).await.is_err() {
            closed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        if t.elapsed() > Duration::from_millis(1200) {
            break;
        }
    }
    let mut buf = [0u8; 1];
    let n = tokio::time::timeout(Duration::from_secs(2), raw.read(&mut buf))
        .await
        .unwrap()
        .unwrap_or(0);
    assert!(closed || n == 0 || n > 0, "unreachable");
    assert_eq!(n, 0, "the server closed the trickling connection");
    assert!(
        t.elapsed() < Duration::from_millis(1500),
        "{:?}",
        t.elapsed()
    );
    connect(&srv.addr(), me, peers, "laptop", fast())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn c_server_refuses_a_17th_session_before_the_handshake() {
    let d = dir("sess17");
    let mut mine = PeerStore::in_memory();
    mine.add(Identity::from_secret(SECRET).public(), "C test server")
        .unwrap();
    let mine = Arc::new(Mutex::new(mine));
    let mut store = PeerStore::load(&d.join("peers")).unwrap();
    let ids: Vec<Arc<Identity>> = (0..17)
        .map(|_| Arc::new(Identity::generate().unwrap()))
        .collect();
    for (i, id) in ids.iter().enumerate() {
        store.add(id.public(), &format!("c{i}")).unwrap();
    }
    let srv = CServer::start(SECRET, &d.join("peers"), 0, 100, 500, 2000);
    let mut sessions = Vec::new();
    for id in &ids[..16] {
        sessions.push(
            connect(&srv.addr(), id.clone(), mine.clone(), "c", fast())
                .await
                .unwrap(),
        );
    }
    // connect() reads the first frame expecting Hs2: Refused means the server sent the
    // error instead of Hs2, i.e. it refused before any Noise work.
    let r = connect(&srv.addr(), ids[16].clone(), mine.clone(), "c", fast()).await;
    assert!(
        matches!(r, Err(Ava1Error::Refused { code, .. }) if code == gen::ERR_BUSY),
        "{r:?}"
    );
    // A freed slot is usable again.
    sessions.pop().unwrap().close().await;
    let t = Instant::now();
    loop {
        match connect(&srv.addr(), ids[16].clone(), mine.clone(), "c", fast()).await {
            Ok(_) => break,
            Err(e) => assert!(t.elapsed() < Duration::from_secs(3), "{e:?}"),
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_c_server_never_reuses_a_lane_key() {
    let d = dir("rejoin");
    let (me, peers) = paired_client(&d.join("peers"));
    let srv = CServer::start(SECRET, &d.join("peers"), 0, 100, 500, 500);
    let s = connect(&srv.addr(), me, peers, "laptop", fast())
        .await
        .unwrap();
    async fn ack(addr: &str, j: &gen::Join) -> [u8; 16] {
        let (rh, wh) = TcpStream::connect(addr).await.unwrap().into_split();
        let (mut r, mut w) = (FrameReader::new(rh), FrameWriter::new(wh));
        w.send_msg(0, j).await.unwrap();
        r.recv()
            .await
            .unwrap()
            .decode::<gen::JoinAck>()
            .unwrap()
            .server_nonce
    }
    let j = s.join_frame_for_test(4, [0x61; 16]);
    let sn1 = ack(&srv.addr(), &j).await;
    for i in 0..70u8 {
        ack(&srv.addr(), &s.join_frame_for_test(4, [i; 16])).await;
        // One lane connection at a time, so the per-address limit never applies.
        wait_conns(&srv, 1).await;
    }
    // Past the 64-entry window the replay is acked again, but with fresh keys.
    let sn2 = ack(&srv.addr(), &j).await;
    assert_ne!(sn1, sn2);
    assert_ne!(
        s.lane_keys_for_test(4, &j.client_nonce, &sn1),
        s.lane_keys_for_test(4, &j.client_nonce, &sn2)
    );
}

type RawW = FrameWriter<tokio::net::tcp::OwnedWriteHalf>;
type RawR = FrameReader<tokio::net::tcp::OwnedReadHalf>;

/// A paired client driven by hand: handshake only, small receive buffer, no heartbeats.
async fn raw_session(addr: &str, me: &Identity) -> (RawR, RawW) {
    let sock = tokio::net::TcpSocket::new_v4().unwrap();
    sock.set_recv_buffer_size(4096).unwrap();
    let stream = sock.connect(addr.parse().unwrap()).await.unwrap();
    let (rh, wh) = stream.into_split();
    let (mut r, mut w) = (FrameReader::new(rh), FrameWriter::new(wh));
    let est = ava1::handshake::client(&mut r, &mut w, me, "raw", |_| true)
        .await
        .unwrap();
    assert!(est.pairing.is_none());
    (r, w)
}

fn half_frame_header() -> Vec<u8> {
    ava1::frame::Header {
        ty: gen::RpcRequest::TYPE,
        flags: ava1::frame::FLAG_SEALED,
        channel: 2,
        body_len: 60_000,
    }
    .encode()
    .to_vec()
}

use ava1::wire::FrameMessage;

#[tokio::test(flavor = "multi_thread")]
async fn a_max_size_frame_slower_than_dead_after_keeps_the_c_session() {
    let d = dir("bigframe");
    let (me, peers) = paired_client(&d.join("peers"));
    let srv = CServer::start(SECRET, &d.join("peers"), 0, 200, 1000, 3000);
    let cfg = ChaosConfig {
        bytes_per_sec: Some(16 * 1024),
        ..ChaosConfig::default()
    };
    let proxy = ChaosProxy::start(srv.addr().parse().unwrap(), cfg)
        .await
        .unwrap();
    let t = Timing {
        ping_every: Duration::from_millis(200),
        dead_after: Duration::from_millis(1000),
        handshake: Duration::from_secs(3),
        ..Timing::default()
    };
    let s = connect(&proxy.addr.to_string(), me, peers, "laptop", t)
        .await
        .unwrap();
    let start = Instant::now();
    let r = s
        .rpc(gen::METHOD_NODE_INFO, &vec![0x5a; 65_000])
        .await
        .unwrap();
    assert_eq!(r.status, gen::STATUS_OK);
    assert!(start.elapsed() > t.dead_after * 3, "{:?}", start.elapsed());
    // The C server kept pinging while it read the frame, so the client stayed alive too.
    assert!(!s.is_closed(), "client side");
    assert_eq!(srv.conns(), 1, "server side");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_c_server_drops_a_peer_that_stops_mid_frame() {
    let d = dir("midframe");
    let (me, _peers) = paired_client(&d.join("peers"));
    let srv = CServer::start(SECRET, &d.join("peers"), 0, 100, 500, 500);
    let (_r, w) = raw_session(&srv.addr(), &me).await;
    let mut stream = w.into_inner();
    let mut raw = half_frame_header();
    raw.extend_from_slice(&[0u8; 1000]);
    stream.write_all(&raw).await.unwrap();
    let t = Instant::now();
    wait_conns(&srv, 0).await;
    assert!(
        t.elapsed() < Duration::from_millis(2000),
        "{:?}",
        t.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_c_server_drops_a_dripping_frame_by_the_rate_floor() {
    let d = dir("drip");
    let (me, _peers) = paired_client(&d.join("peers"));
    let srv = CServer::start_with(
        SECRET,
        &d.join("peers"),
        ffi::TestOpts {
            ping_ms: 100,
            dead_ms: 500,
            handshake_ms: 500,
            min_frame_rate: 64 * 1024,
            ..Default::default()
        },
    );
    let (_r, w) = raw_session(&srv.addr(), &me).await;
    let mut stream = w.into_inner();
    stream.write_all(&half_frame_header()).await.unwrap();
    let drip = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if stream.write_all(&[0]).await.is_err() {
                return;
            }
        }
    });
    let t = Instant::now();
    while srv.conns() > 0 && t.elapsed() < Duration::from_secs(8) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(srv.conns(), 0, "the rate floor ends a dripping frame");
    assert!(
        t.elapsed() > Duration::from_millis(900),
        "{:?}",
        t.elapsed()
    );
    drip.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_c_server_drops_a_peer_that_never_reads_its_replies() {
    let d = dir("noread");
    let (me, peers) = paired_client(&d.join("peers"));
    let srv = CServer::start(SECRET, &d.join("peers"), 0, 100, 500, 500);
    let (_r, mut w) = raw_session(&srv.addr(), &me).await; // _r is never read
    let flood = tokio::spawn(async move {
        let q = gen::RpcRequest {
            method: gen::METHOD_NODE_INFO,
            body: Vec::new(),
        };
        let mut id = 1u32;
        while w.send_msg(id, &q).await.is_ok() {
            id = id.wrapping_add(1);
        }
    });
    let t = Instant::now();
    while srv.conns() > 0 && t.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        srv.conns(),
        0,
        "the C server drops a peer that stopped reading"
    );
    flood.abort();
    connect(&srv.addr(), me, peers, "laptop", fast())
        .await
        .unwrap()
        .node_info()
        .await
        .unwrap();
}
