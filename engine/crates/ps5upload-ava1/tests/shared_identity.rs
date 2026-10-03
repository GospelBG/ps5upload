//! SPEC.md §8: a console keeps one session per identity, so two engines sharing one evict
//! each other. The pool notices repeated evictions and names the cause.
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ava1::keys::Identity;
use ava1::peers::PeerStore;
use ava1::server::{self, ServerCtx};
use ps5upload_ava1::Pool;

fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("p5a-shared-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

async fn until(what: &str, f: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !f() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

/// A loopback console that trusts `me`, and `n` pools that all load the same identity.
async fn console_and_pools(tag: &str, n: usize, deaths: usize) -> Vec<Pool> {
    let base = temp(tag);
    let first = base.join("e0");
    std::fs::create_dir_all(&first).unwrap();
    let me = Identity::load_or_create(&first.join("identity")).unwrap();
    let mut peers = PeerStore::in_memory();
    peers.add(me.public(), "engine").unwrap();
    let ctx = ServerCtx::new(
        Identity::generate().unwrap(),
        "host",
        peers,
        Box::new(|_, _| ava1::session::RpcReply {
            status: ava1::gen::ERR_UNKNOWN_METHOD,
            body: Vec::new(),
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(server::serve(l, Arc::new(ctx)));
    (0..n)
        .map(|i| {
            let d = base.join(format!("e{i}"));
            std::fs::create_dir_all(&d).unwrap();
            if i > 0 {
                std::fs::copy(first.join("identity"), d.join("identity")).unwrap();
            }
            Pool::new(d)
                .with_addr(addr.clone())
                .with_churn(deaths, Duration::from_secs(60))
        })
        .collect()
}

#[tokio::test]
async fn two_engines_sharing_an_identity_evict_each_other_and_the_pool_says_so() {
    let pools = console_and_pools("two", 2, 2).await;
    let (a, b) = (&pools[0], &pools[1]);
    for round in 0..4 {
        let (mine, theirs) = if round % 2 == 0 { (a, b) } else { (b, a) };
        let s = mine.session("console").await.unwrap();
        // The other engine's session is ended by the console the moment ours is proved.
        let other = theirs.session("console").await.unwrap();
        until("the older session to be evicted", || s.is_closed()).await;
        assert!(!other.is_closed());
        // Its watcher records the end asynchronously; give it the next round to do so.
    }
    // A lost its session in rounds 0 and 2 (two evictions inside the window); B's second
    // loss is only noticed by its watcher, so B may or may not have warned yet.
    until("the warning", || a.superseded_warnings() > 0).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        a.superseded_warnings(),
        1,
        "once per window, not once per eviction"
    );
}

#[tokio::test]
async fn one_engine_reconnecting_is_not_a_second_engine() {
    let pools = console_and_pools("one", 1, 2).await;
    let a = &pools[0];
    for _ in 0..4 {
        let s = a.session("console").await.unwrap();
        a.forget("console").await;
        // Reconnecting makes the console end the forgotten session: ours, not an eviction.
        let _fresh = a.session("console").await.unwrap();
        until("the session to end", || s.is_closed()).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        a.superseded_warnings(),
        0,
        "our own closes are not evictions"
    );
}
