#![allow(dead_code)]
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ava1::keys::Identity;
use ava1::peers::PeerStore;
use ava1::session::Timing;

pub const SECRET: [u8; 32] = [0x42; 32];

pub fn fast() -> Timing {
    Timing {
        ping_every: Duration::from_millis(100),
        dead_after: Duration::from_millis(500),
        handshake: Duration::from_millis(500),
        ..Timing::default()
    }
}

/// Liveness slow enough for disk-heavy tests on a loaded CI machine.
pub fn calm() -> Timing {
    Timing {
        ping_every: Duration::from_millis(200),
        dead_after: Duration::from_millis(2000),
        handshake: Duration::from_millis(2000),
        ..Timing::default()
    }
}

pub fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ava1-c-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A client the C server already knows, and that knows the C server.
pub fn paired_client(peers_file: &Path) -> (Arc<Identity>, Arc<Mutex<PeerStore>>) {
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
