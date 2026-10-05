#![cfg(unix)]
//! Review S2, round 3: a download never streams the AVA1 trust store, even through a symlink. The C sender
//! (ava1_send.c) walks with AVA1_WALK_FOLLOW (it must: SMP and symlinked game folders), so the walk and the
//! open both refuse what leads into the store (data cfg `refuse_link`, wired to path_tree_op_refused).
mod common;

use std::os::unix::fs::symlink;
use std::sync::Arc;

use ava1::recv::{download_job, LocalSink, RecvOptions};
use ava1::session::connect;
use ava1_ctest::CServer;
use common::*;

fn ro(jobs: &std::path::Path) -> RecvOptions {
    RecvOptions {
        credit: 64 << 20,
        flags: 0,
        jobs_dir: jobs.into(),
        ordered: false,
        progress: Arc::default(),
        cancel: Arc::default(),
        progress_deadline: None,
    }
}

fn all_files(root: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push((
                    p.strip_prefix(root).unwrap().display().to_string(),
                    std::fs::read(&p).unwrap(),
                ));
            }
        }
    }
    out
}

/// `<console>/game` holds an ordinary file, a link to a directory OUTSIDE the store (must still be
/// followed), and links into the store: to its directory, to a file in it, and to its ANCESTOR.
#[tokio::test(flavor = "multi_thread")]
async fn a_download_never_delivers_trust_store_bytes_through_a_symlink() {
    ava1_ctest::c_set_read_allowed(true);
    let d = dir("dl-trust");
    let store = d.join("console/ps5upload/ava");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(store.join("identity"), b"SECRET-KEY").unwrap();
    std::fs::write(store.join("peers"), b"TRUSTED-PEERS").unwrap();
    let store = store.canonicalize().unwrap();
    let game = d.join("console/game");
    std::fs::create_dir_all(game.join("real")).unwrap();
    std::fs::write(game.join("real/ok.bin"), b"fine").unwrap();
    std::fs::write(game.join("plain"), b"plain").unwrap();
    let other = d.join("console/shared");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("lib.so"), b"shared-lib").unwrap();
    symlink(&other, game.join("shared")).unwrap(); // a legitimate directory link: followed
    symlink(&store, game.join("x")).unwrap(); // directory link into the store
    symlink(store.join("identity"), game.join("id-file")).unwrap(); // file link into the store
    symlink(store.parent().unwrap(), game.join("up")).unwrap(); // link to an ancestor of the store
    ava1_ctest::c_set_protected(Some(&store));
    let (me, mine) = paired_client(&d.join("peers"));
    let srv = CServer::start_data(
        SECRET,
        &d.join("peers"),
        &d.join("jobs"),
        200,
        2000,
        2000,
        0,
    );
    let s = connect(&srv.addr(), me, mine, "rust", calm())
        .await
        .unwrap();
    let mut link = s.job([0x71; 16]);
    let sink = Arc::new(LocalSink::new(d.join("got"), false));
    let r = download_job(
        &mut link,
        game.to_str().unwrap(),
        0,
        sink,
        ro(&d.join("ejobs")),
    )
    .await;
    ava1_ctest::c_set_protected(None);
    r.unwrap();
    let got = all_files(&d.join("got"));
    for (name, bytes) in &got {
        let t = String::from_utf8_lossy(bytes);
        assert!(
            !t.contains("SECRET-KEY") && !t.contains("TRUSTED-PEERS"),
            "{name} carries trust-store bytes"
        );
    }
    let names: Vec<&str> = got.iter().map(|(n, _)| n.as_str()).collect();
    assert!(
        names.contains(&"real/ok.bin") && names.contains(&"plain"),
        "ordinary files arrive: {names:?}"
    );
    assert!(
        names.contains(&"shared/lib.so"),
        "a legitimate directory link is still followed: {names:?}"
    );
    drop(srv);
}
