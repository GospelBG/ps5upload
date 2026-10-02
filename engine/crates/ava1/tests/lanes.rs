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
        client_nonce: [1; 16],
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
            client_nonce: [i; 16],
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

/// Sends `j` on a fresh connection; returns the JoinAck's server nonce or the refusal code.
async fn join_ack(addr: std::net::SocketAddr, j: &Join) -> Result<[u8; 16], u16> {
    let (rh, wh) = TcpStream::connect(addr).await.unwrap().into_split();
    let (mut r, mut w) = (FrameReader::new(rh), FrameWriter::new(wh));
    w.send_msg(0, j).await.unwrap();
    let f = r.recv().await.unwrap();
    if f.ty == gen::Error::TYPE {
        return Err(f.decode::<gen::Error>().unwrap().code);
    }
    let ack: gen::JoinAck = f.decode().unwrap();
    assert_eq!(ack.lane_id, j.lane_id);
    Ok(ack.server_nonce)
}

#[tokio::test]
async fn two_joins_of_one_lane_id_never_share_a_key() {
    let (addr, _ctx, me, peers) = paired().await;
    let s = connect(&addr.to_string(), me, peers, "c", fast())
        .await
        .unwrap();
    let j = s.join_frame_for_test(2, [0x51; 16]);
    let sn1 = join_ack(addr, &j).await.unwrap();
    // Push the original Join out of the server's 64-entry replay window with valid joins.
    for i in 0..70u8 {
        join_ack(addr, &s.join_frame_for_test(2, [i; 16]))
            .await
            .unwrap();
    }
    // The replay is no longer remembered, so it is acked — but with a new server nonce,
    // so the lane keys differ from the original join's and no (key, counter) repeats.
    let sn2 = join_ack(addr, &j).await.unwrap();
    assert_ne!(sn1, sn2);
    let (a, b) = (
        s.lane_keys_for_test(2, &j.client_nonce, &sn1),
        s.lane_keys_for_test(2, &j.client_nonce, &sn2),
    );
    assert_ne!(a.0, b.0);
    assert_ne!(a.1, b.1);
}

#[tokio::test]
async fn an_unknown_frame_on_a_lane_is_a_protocol_error_unless_ignorable() {
    let (addr, _ctx, me, peers) = paired().await;
    let s = connect(&addr.to_string(), me, peers, "c", fast())
        .await
        .unwrap();
    for (ignorable, lane) in [(true, 5u16), (false, 6)] {
        let j = s.join_frame_for_test(lane, [lane as u8; 16]);
        let (rh, wh) = TcpStream::connect(addr).await.unwrap().into_split();
        let (mut r, mut w) = (FrameReader::new(rh), FrameWriter::new(wh));
        w.send_msg(0, &j).await.unwrap();
        let ack: gen::JoinAck = r.recv().await.unwrap().decode().unwrap();
        let (c2s, s2c) = s.lane_keys_for_test(lane, &j.client_nonce, &ack.server_nonce);
        w.set_key(c2s);
        r.set_key(s2c);
        if ignorable {
            w.send_ignorable(0x7e, 0, b"from the future").await.unwrap();
        } else {
            w.send(0x7e, 0, b"from the future").await.unwrap();
        }
        // The next non-heartbeat frame, if any, within a few heartbeats.
        let verdict = tokio::time::timeout(Duration::from_millis(700), async {
            loop {
                let f = r.recv().await?;
                if f.ty == gen::Error::TYPE {
                    return f.decode::<gen::Error>();
                }
                // Stay alive: answer the server's heartbeats.
                if let Ok(p) = f.decode::<gen::Ping>() {
                    let pong = gen::Pong {
                        seq: p.seq,
                        t_us: p.t_us,
                    };
                    w.send_msg(0, &pong).await?;
                }
            }
        })
        .await;
        match (ignorable, verdict) {
            (true, Err(_)) => {} // still open, only heartbeats
            (false, Ok(Ok(e))) => assert_eq!(e.code, gen::ERR_PROTOCOL),
            (i, v) => panic!("ignorable={i}: {v:?}"),
        }
    }
}
