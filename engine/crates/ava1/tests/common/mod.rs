#![allow(dead_code)]
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ava1::gen;
use ava1::keys::Identity;
use ava1::peers::PeerStore;
use ava1::server::{self, RpcHandler, ServerCtx};
use ava1::session::{RpcReply, Timing};
use ava1::wire::Message;
use tokio::net::TcpListener;

pub fn fast() -> Timing {
    Timing {
        ping_every: Duration::from_millis(100),
        dead_after: Duration::from_millis(500),
        handshake: Duration::from_millis(500),
    }
}

pub fn node_info_rpc(name: &'static str) -> RpcHandler {
    Box::new(move |method, _| {
        if method == gen::METHOD_NODE_INFO {
            let info = gen::NodeInfo {
                version: "test".into(),
                platform: "rust".into(),
                name: name.into(),
                firmware: None,
            };
            RpcReply {
                status: gen::STATUS_OK,
                body: info.to_bytes().unwrap(),
            }
        } else {
            RpcReply {
                status: gen::ERR_UNKNOWN_METHOD,
                body: Vec::new(),
            }
        }
    })
}

pub async fn start(ctx: ServerCtx) -> (SocketAddr, Arc<ServerCtx>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let ctx = Arc::new(ctx);
    tokio::spawn(server::serve(l, ctx.clone()));
    (addr, ctx)
}

/// A server and a client identity that already know each other.
pub async fn paired() -> (
    SocketAddr,
    Arc<ServerCtx>,
    Arc<Identity>,
    Arc<Mutex<PeerStore>>,
) {
    let (s_id, c_id) = (
        Identity::generate().unwrap(),
        Arc::new(Identity::generate().unwrap()),
    );
    let mut s_peers = PeerStore::in_memory();
    s_peers.add(c_id.public(), "client").unwrap();
    let mut c_peers = PeerStore::in_memory();
    c_peers.add(s_id.public(), "server").unwrap();
    let ctx = ServerCtx::new(
        s_id,
        "Rust test server",
        s_peers,
        node_info_rpc("Rust test server"),
    )
    .with_timing(fast());
    let (addr, ctx) = start(ctx).await;
    (addr, ctx, c_id, Arc::new(Mutex::new(c_peers)))
}

pub fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "ava1-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}
