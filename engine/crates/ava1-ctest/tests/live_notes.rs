//! A RUNNING upload's live notes (what limits it) reach the engine's counters, not only the
//! finished job's commit ack: a loopback upload to the C receiver with a slow disk, watched
//! while it runs.
#![cfg(unix)]
mod common;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use ava1::keys::Identity;
use ava1::peers::PeerStore;
use ava1_ctest::CServer;
use common::{dir, SECRET};
use ps5upload_ava1::upload::upload_dir_in;
use ps5upload_ava1::Pool;
use ps5upload_core::transfer::{LiveNotes, TransferConfig};

#[test]
fn a_running_upload_reports_its_bottleneck_before_it_finishes() {
    let t = dir("live-notes");
    let ava = t.join("ava");
    std::fs::create_dir_all(&ava).unwrap();
    let me = Identity::load_or_create(&ava.join("identity")).unwrap();
    PeerStore::load(&t.join("srv-peers"))
        .unwrap()
        .add(me.public(), "engine")
        .unwrap();
    PeerStore::load(&ava.join("peers"))
        .unwrap()
        .add(Identity::from_secret(SECRET).public(), "C receiver")
        .unwrap();
    // A slow disk (every data fsync waits) keeps the upload running long enough to watch.
    let srv = CServer::start_data(
        SECRET,
        &t.join("srv-peers"),
        &t.join("jobs"),
        200,
        5000,
        5000,
        20_000,
    );
    let pool = Pool::new(ava).with_addr(srv.addr());

    let src = t.join("src");
    std::fs::create_dir_all(&src).unwrap();
    for i in 0..24 {
        let b: Vec<u8> = (0..400_000)
            .map(|j| (j as u8).wrapping_add(i as u8))
            .collect();
        std::fs::write(src.join(format!("f{i:02}")), b).unwrap();
    }

    let live = Arc::new(LiveNotes::default());
    let mut c = TransferConfig::new("127.0.0.1:9113");
    c.progress_bytes = Some(Arc::new(AtomicU64::new(0)));
    c.progress_files = Some(Arc::new(AtomicU64::new(0)));
    c.progress_files_finalized = Some(Arc::new(AtomicU64::new(0)));
    c.progress_bytes_finalized = Some(Arc::new(AtomicU64::new(0)));
    c.cancel = Some(Arc::new(AtomicBool::new(false)));
    c.progress_live = Some(live.clone());

    let done = AtomicBool::new(false);
    let seen_while_running = AtomicBool::new(false);
    let res = std::thread::scope(|sc| {
        sc.spawn(|| {
            while !done.load(Ordering::Relaxed) {
                if live.bottleneck.load(Ordering::Relaxed) != 0 {
                    seen_while_running.store(true, Ordering::Relaxed);
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        });
        let r = upload_dir_in(
            &pool,
            &c,
            [0x77; 16],
            t.join("dest").to_str().unwrap(),
            &src,
        );
        done.store(true, Ordering::Relaxed);
        r
    })
    .unwrap();
    let ack: serde_json::Value = serde_json::from_str(&res.commit_ack_body).unwrap();
    assert!(
        seen_while_running.load(Ordering::Relaxed),
        "a bottleneck was reported while the job ran (commit ack says {})",
        ack["bottleneck"]
    );
    // Not skipping, not settling: those stay off for a plain upload.
    assert_eq!(live.phase.load(Ordering::Relaxed), 0);
    assert!(!live.settling.load(Ordering::Relaxed));
    drop(srv);
}
