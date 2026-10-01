mod common;

use std::time::{Duration, Instant};

use ava1::session::connect;
use ava1_chaos::{ChaosConfig, ChaosProxy};
use common::*;

#[tokio::test]
async fn blackholed_session_is_declared_dead() {
    // Review focus 3: a half-open link (no FIN) on both sides.
    let (addr, ctx, me, peers) = paired().await;
    let proxy = ChaosProxy::start(addr, ChaosConfig::default())
        .await
        .unwrap();
    let s = connect(&proxy.addr.to_string(), me, peers, "c", fast())
        .await
        .unwrap();
    assert_eq!(ctx.sessions(), 1);
    proxy.blackhole(true);
    let t = Instant::now();
    let why = tokio::time::timeout(Duration::from_secs(3), s.closed())
        .await
        .expect("client noticed");
    let took = t.elapsed();
    assert!(why.contains("stopped answering"), "{why}");
    assert!(took < Duration::from_millis(1200), "{took:?}"); // dead_after 500 ms + one tick
                                                             // `s` is still alive here, so the server cannot have learned of death from a FIN.
    while ctx.sessions() > 0 && t.elapsed() < Duration::from_secs(3) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let server_took = t.elapsed();
    assert_eq!(ctx.sessions(), 0, "server noticed too");
    assert!(
        server_took < Duration::from_millis(1200),
        "server took {server_took:?}"
    );
    drop(s);
}

#[tokio::test]
async fn a_killed_lane_does_not_end_the_session() {
    let (addr, _ctx, me, peers) = paired().await;
    let proxy = ChaosProxy::start(addr, ChaosConfig::default())
        .await
        .unwrap();
    let s = connect(&proxy.addr.to_string(), me, peers, "c", fast())
        .await
        .unwrap();
    let keep = s.open_lane().await.unwrap();
    let doomed = s.open_lane().await.unwrap();
    proxy.kill_newest();
    tokio::time::timeout(Duration::from_secs(2), doomed.closed())
        .await
        .expect("killed lane noticed");
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(!keep.is_closed() && !s.is_closed());
    s.node_info().await.unwrap();
    drop(doomed);
    s.open_lane().await.unwrap();
}

#[tokio::test]
async fn latency_and_a_bandwidth_cap_do_not_kill_a_session() {
    let (addr, _ctx, me, peers) = paired().await;
    let cfg = ChaosConfig {
        delay: Duration::from_millis(60),
        bytes_per_sec: Some(32 * 1024),
        kill_every: None,
    };
    let proxy = ChaosProxy::start(addr, cfg).await.unwrap();
    let s = connect(&proxy.addr.to_string(), me, peers, "c", fast())
        .await
        .unwrap();
    for _ in 0..10 {
        s.node_info().await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(!s.is_closed());
    assert!(s.rtt().unwrap() >= Duration::from_millis(100));
}

#[tokio::test]
async fn a_killed_session_reports_why_and_a_new_one_connects() {
    let (addr, _ctx, me, peers) = paired().await;
    let proxy = ChaosProxy::start(addr, ChaosConfig::default())
        .await
        .unwrap();
    let s = connect(
        &proxy.addr.to_string(),
        me.clone(),
        peers.clone(),
        "c",
        fast(),
    )
    .await
    .unwrap();
    proxy.kill_all();
    let why = tokio::time::timeout(Duration::from_secs(2), s.closed())
        .await
        .unwrap();
    assert!(!why.is_empty());
    assert!(s.node_info().await.is_err());
    connect(&proxy.addr.to_string(), me, peers, "c", fast())
        .await
        .unwrap()
        .node_info()
        .await
        .unwrap();
}
