mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ava1::conn::{FrameReader, FrameWriter};
use ava1::gen::{self, Join};
use ava1::keys::Identity;
use ava1::peers::PeerStore;
use ava1::server::ServerCtx;
use ava1::session::connect;
use ava1::wire::FrameMessage;
use ava1::Ava1Error;
use common::*;
use tokio::net::TcpStream;

#[tokio::test]
async fn eight_lanes_open_and_stay_alive_on_heartbeats() {
    let (addr, _ctx, me, peers) = paired().await;
    let s = connect(&addr.to_string(), me, peers, "c", fast())
        .await
        .unwrap();
    let mut lanes = Vec::new();
    for _ in 0..gen::MAX_LANES {
        lanes.push(s.open_lane().await.unwrap());
    }
    let ids: Vec<u16> = lanes.iter().map(|l| l.id).collect();
    assert_eq!(ids, (1..=gen::MAX_LANES as u16).collect::<Vec<_>>());
    assert!(
        matches!(s.open_lane().await, Err(Ava1Error::Refused { code, .. }) if code == gen::ERR_BUSY)
    );
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(lanes.iter().all(|l| !l.is_closed() && l.rtt().is_some()));
    drop(lanes.remove(0));
    assert_eq!(
        s.open_lane().await.unwrap().id,
        1,
        "a dropped lane's id is reused"
    );
}

#[tokio::test]
async fn lanes_end_when_their_session_ends() {
    let (addr, _ctx, me, peers) = paired().await;
    let s = connect(&addr.to_string(), me, peers, "c", fast())
        .await
        .unwrap();
    let lane = s.open_lane().await.unwrap();
    s.close().await;
    let why = tokio::time::timeout(Duration::from_secs(2), lane.closed())
        .await
        .expect("lane ended");
    assert!(!why.is_empty());
}

#[tokio::test]
async fn a_lane_cannot_be_opened_before_pairing() {
    let ctx = ServerCtx::new(
        Identity::generate().unwrap(),
        "s",
        PeerStore::in_memory(),
        node_info_rpc("s"),
    )
    .with_timing(fast());
    ctx.open_pairing(Duration::from_secs(60));
    let (addr, _ctx) = start(ctx).await;
    let s = connect(
        &addr.to_string(),
        Arc::new(Identity::generate().unwrap()),
        Arc::new(Mutex::new(PeerStore::in_memory())),
        "c",
        fast(),
    )
    .await
    .unwrap();
    assert!(matches!(s.open_lane().await, Err(Ava1Error::NotPaired)));
}

async fn raw_join(addr: std::net::SocketAddr, j: &Join) -> gen::Error {
    let (rh, wh) = TcpStream::connect(addr).await.unwrap().into_split();
    let (mut r, mut w) = (FrameReader::new(rh), FrameWriter::new(wh));
    w.send_msg(0, j).await.unwrap();
    r.recv().await.unwrap().decode::<gen::Error>().unwrap()
}

#[tokio::test]
async fn forged_and_unknown_joins_are_refused() {
    let (addr, _ctx, me, peers) = paired().await;
    let _s = connect(&addr.to_string(), me, peers, "c", fast())
        .await
        .unwrap();
    let forged = Join {
        session_id: [7; 16],
        lane_id: 1,
        nonce: [1; 16],
        tag: [2; 16],
    };
    assert_eq!(raw_join(addr, &forged).await.code, gen::ERR_BAD_JOIN);
    let bad_lane = Join {
        lane_id: 0,
        ..forged
    };
    assert_eq!(raw_join(addr, &bad_lane).await.code, gen::ERR_BAD_JOIN);
}

#[tokio::test]
async fn reopening_a_live_lane_id_supersedes_the_old_connection() {
    // A Wi-Fi blip on one lane: the client reconnects with the same id before the
    // server has noticed the old one died.
    let (addr, _ctx, me, peers) = paired().await;
    let s = connect(&addr.to_string(), me, peers, "c", fast())
        .await
        .unwrap();
    let old = s.open_lane().await.unwrap();
    let old_id = old.id;
    let new = s.reopen_lane_for_test(old_id).await.unwrap();
    let t = Instant::now();
    let why = tokio::time::timeout(Duration::from_secs(2), old.closed())
        .await
        .expect("old lane ended");
    assert!(t.elapsed() < Duration::from_secs(2), "{why}");
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(!new.is_closed());
}

#[tokio::test]
async fn a_cancelled_open_lane_frees_its_id() {
    let (addr, _ctx, me, peers) = paired().await;
    let s = connect(&addr.to_string(), me, peers, "c", fast())
        .await
        .unwrap();
    // Cancel open_lane at several points, from before the connect to mid-join.
    for i in 0..12u64 {
        let _ = tokio::time::timeout(Duration::from_micros(i * 150), s.open_lane()).await;
    }
    let first = s.open_lane().await.unwrap();
    assert_eq!(first.id, 1, "cancelled opens must not leak ids");
}

#[tokio::test]
async fn a_cancelled_open_lane_against_a_silent_server_frees_its_id() {
    let (addr, _ctx, me, peers) = paired().await;
    let s = connect(&addr.to_string(), me, peers, "c", fast())
        .await
        .unwrap();
    // Cancel while waiting for an ack that never comes: swap in a silent listener.
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let silent_addr = silent.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((c, _)) = silent.accept().await {
            held.push(c);
        }
    });
    for _ in 0..10 {
        assert!(
            tokio::time::timeout(Duration::from_millis(30), s.open_lane_at(silent_addr))
                .await
                .is_err()
        );
    }
    assert_eq!(s.open_lane().await.unwrap().id, 1);
}

#[tokio::test]
async fn a_replayed_join_is_refused() {
    let (addr, _ctx, me, peers) = paired().await;
    let s = connect(&addr.to_string(), me, peers, "c", fast())
        .await
        .unwrap();
    let good = s.join_frame_for_test(3, [9; 16]);
    async fn first_reply(
        addr: std::net::SocketAddr,
        j: &Join,
    ) -> (
        u16,
        (
            FrameReader<tokio::net::tcp::OwnedReadHalf>,
            FrameWriter<tokio::net::tcp::OwnedWriteHalf>,
        ),
    ) {
        let (rh, wh) = TcpStream::connect(addr).await.unwrap().into_split();
        let (mut r, mut w) = (FrameReader::new(rh), FrameWriter::new(wh));
        w.send_msg(0, j).await.unwrap();
        let f = r.recv().await.unwrap();
        let ty = f.ty;
        let code = if ty == gen::Error::TYPE {
            f.decode::<gen::Error>().unwrap().code
        } else {
            0
        };
        (code, (r, w))
    }
    let (code, _keep) = first_reply(addr, &good).await;
    assert_eq!(code, 0, "first join is acked");
    for i in 0..100u8 {
        let junk = Join {
            tag: [i; 16],
            nonce: [i; 16],
            ..good
        };
        assert_eq!(raw_join(addr, &junk).await.code, gen::ERR_BAD_JOIN);
    }
    let (code, _) = first_reply(addr, &good).await;
    assert_eq!(
        code,
        gen::ERR_BAD_JOIN,
        "replay refused even after junk joins"
    );
}
