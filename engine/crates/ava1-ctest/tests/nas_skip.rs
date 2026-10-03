//! "Skip files the console already has" end to end: the adapter the engine calls
//! (`upload_dir_skip_existing_in`) over a real session to the C receiver on loopback,
//! for a local folder, a remote (NAS) source that reports mtimes, and one that does not
//! (SPEC.md §11.4). Each run is a new job (a new upload), as the engine's resume
//! strategy makes it.
#![cfg(unix)]
mod common;

use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ava1::keys::Identity;
use ava1::peers::PeerStore;
use ava1_ctest::CServer;
use common::{dir, SECRET};
use ps5upload_ava1::upload::{upload_dir_skip_existing_in, SkipMode};
use ps5upload_ava1::Pool;
use ps5upload_core::source_fs::{ReadSeek, SourceFs, SourceMeta};
use ps5upload_core::transfer::TransferConfig;

const MTIME: u64 = 1_650_000_000;

/// A paired engine pool and the C receiver it talks to.
struct Rig {
    pool: Pool,
    srv: CServer,
    t: PathBuf,
}

fn rig(tag: &str) -> Rig {
    let t = dir(tag);
    let ava = t.join("ava");
    std::fs::create_dir_all(&ava).unwrap();
    let srv_peers = t.join("srv-peers");
    // The pool's identity is trusted by the receiver, and it knows the receiver's key.
    let me = Identity::load_or_create(&ava.join("identity")).unwrap();
    PeerStore::load(&srv_peers)
        .unwrap()
        .add(me.public(), "engine")
        .unwrap();
    PeerStore::load(&ava.join("peers"))
        .unwrap()
        .add(Identity::from_secret(SECRET).public(), "C receiver")
        .unwrap();
    let srv = CServer::start_data(SECRET, &srv_peers, &t.join("jobs"), 200, 2000, 2000, 0);
    let pool = Pool::new(ava).with_addr(srv.addr());
    Rig { pool, srv, t }
}

struct Run {
    sent: u64,
    resent: u64,
}

fn cfg(fs: Option<Arc<dyn SourceFs>>) -> (TransferConfig, Arc<AtomicU64>) {
    let mut c = TransferConfig::new("127.0.0.1:9113");
    let sent = Arc::new(AtomicU64::new(0));
    c.progress_bytes = Some(sent.clone());
    c.progress_files = Some(Arc::new(AtomicU64::new(0)));
    c.progress_files_finalized = Some(Arc::new(AtomicU64::new(0)));
    c.progress_bytes_finalized = Some(Arc::new(AtomicU64::new(0)));
    c.cancel = Some(Arc::new(AtomicBool::new(false)));
    c.source_fs = fs;
    (c, sent)
}

static JOB: AtomicU64 = AtomicU64::new(1);

/// One upload of `src` to `dest` as a brand-new job.
fn run(r: &Rig, fs: Option<Arc<dyn SourceFs>>, src: &Path, dest: &Path, mode: SkipMode) -> Run {
    let (c, sent) = cfg(fs);
    let mut id = [0u8; 16];
    id[..8].copy_from_slice(&JOB.fetch_add(1, Ordering::SeqCst).to_le_bytes());
    id[8..12].copy_from_slice(&std::process::id().to_le_bytes());
    let res = upload_dir_skip_existing_in(&r.pool, &c, id, dest.to_str().unwrap(), src, mode)
        .unwrap_or_else(|e| panic!("upload failed: {e:#}"));
    let body: serde_json::Value = serde_json::from_str(&res.commit_ack_body).unwrap();
    Run {
        sent: sent.load(Ordering::Relaxed),
        resent: body["resent"].as_u64().unwrap(),
    }
}

fn write(p: &Path, b: &[u8]) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, b).unwrap();
}

fn big(n: usize, seed: u8) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(7).wrapping_add(seed))
        .collect()
}

#[test]
fn local_source_second_run_skips_everything_and_a_changed_file_is_resent() {
    let r = rig("nas-local");
    let src = r.t.join("src");
    let dest = r.t.join("dest");
    write(&src.join("a.bin"), &big(300_000, 1));
    write(&src.join("d/b.bin"), &big(200_000, 2));
    let first = run(&r, None, &src, &dest, SkipMode::Fast);
    assert!(first.sent >= 500_000, "first run sends the bytes");
    assert_eq!(
        std::fs::read(dest.join("d/b.bin")).unwrap(),
        big(200_000, 2)
    );

    let second = run(&r, None, &src, &dest, SkipMode::Fast);
    assert_eq!(second.sent, 0, "identical tree: nothing re-sent");
    assert_eq!(second.resent, 0);

    // A size change is re-sent, and only that file.
    write(&src.join("d/b.bin"), &big(210_000, 3));
    let third = run(&r, None, &src, &dest, SkipMode::Fast);
    assert!(
        third.sent >= 210_000 && third.sent < 300_000,
        "only the changed file: {}",
        third.sent
    );
    assert_eq!(
        std::fs::read(dest.join("d/b.bin")).unwrap(),
        big(210_000, 3)
    );
    assert_eq!(std::fs::read(dest.join("a.bin")).unwrap(), big(300_000, 1));
    drop(r.srv);
}

/// A one-level in-memory share; `with_mtime` decides whether the backend can report times.
#[derive(Debug)]
struct Nas {
    files: Mutex<BTreeMap<String, Vec<u8>>>,
    with_mtime: bool,
}

impl SourceFs for Nas {
    fn open(&self, p: &Path) -> std::io::Result<Box<dyn ReadSeek>> {
        let f = self.files.lock().unwrap();
        let b = f
            .get(p.to_str().unwrap())
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"))?;
        Ok(Box::new(Cursor::new(b.clone())))
    }
    fn metadata(&self, p: &Path) -> std::io::Result<SourceMeta> {
        match self.files.lock().unwrap().get(p.to_str().unwrap()) {
            Some(b) => Ok(SourceMeta {
                len: b.len() as u64,
                is_dir: false,
                is_file: true,
            }),
            None => Ok(SourceMeta {
                len: 0,
                is_dir: true,
                is_file: false,
            }),
        }
    }
    fn read_dir(&self, _p: &Path) -> std::io::Result<Vec<(PathBuf, bool)>> {
        Ok(self
            .files
            .lock()
            .unwrap()
            .keys()
            .map(|k| (PathBuf::from(k), false))
            .collect())
    }
    fn mtime(&self, p: &Path) -> Option<u64> {
        (self.with_mtime && self.files.lock().unwrap().contains_key(p.to_str().unwrap()))
            .then_some(MTIME)
    }
}

fn nas(with_mtime: bool) -> Arc<Nas> {
    let mut files = BTreeMap::new();
    files.insert("/share/a".to_string(), big(300_000, 5));
    files.insert("/share/b".to_string(), big(200_000, 6));
    Arc::new(Nas {
        files: Mutex::new(files),
        with_mtime,
    })
}

fn remote_flow(tag: &str, with_mtime: bool) {
    let r = rig(tag);
    let dest = r.t.join("dest");
    let n = nas(with_mtime);
    let fs: Arc<dyn SourceFs> = n.clone();
    let first = run(
        &r,
        Some(fs.clone()),
        Path::new("/share"),
        &dest,
        SkipMode::Fast,
    );
    assert!(first.sent >= 500_000);

    let second = run(
        &r,
        Some(fs.clone()),
        Path::new("/share"),
        &dest,
        SkipMode::Fast,
    );
    assert_eq!(second.sent, 0, "identical share: nothing re-sent");

    // Same size, other bytes. Mtime-less: the verify fallback sees it. Mtime-bearing: the
    // backend reports the same time, so skip-existing cannot (the documented weakness,
    // SPEC.md §11.4); Safe mode catches it either way.
    n.files
        .lock()
        .unwrap()
        .insert("/share/b".into(), big(200_000, 9));
    let third = run(
        &r,
        Some(fs.clone()),
        Path::new("/share"),
        &dest,
        SkipMode::Fast,
    );
    if with_mtime {
        assert_eq!(third.sent, 0, "size+mtime equal means skipped");
        assert_eq!(std::fs::read(dest.join("b")).unwrap(), big(200_000, 6));
    } else {
        assert!(
            third.sent >= 200_000 && third.sent < 300_000,
            "{}",
            third.sent
        );
        assert_eq!(std::fs::read(dest.join("b")).unwrap(), big(200_000, 9));
    }
    let safe = run(
        &r,
        Some(fs.clone()),
        Path::new("/share"),
        &dest,
        SkipMode::Safe,
    );
    assert_eq!(std::fs::read(dest.join("b")).unwrap(), big(200_000, 9));
    if with_mtime {
        assert!(safe.sent >= 200_000 && safe.sent < 300_000, "{}", safe.sent);
    } else {
        assert_eq!(safe.sent, 0, "already current");
    }

    // A size change is re-sent under both.
    n.files
        .lock()
        .unwrap()
        .insert("/share/a".into(), big(310_000, 4));
    let fifth = run(&r, Some(fs), Path::new("/share"), &dest, SkipMode::Fast);
    assert!(
        fifth.sent >= 310_000 && fifth.sent < 400_000,
        "{}",
        fifth.sent
    );
    assert_eq!(std::fs::read(dest.join("a")).unwrap(), big(310_000, 4));
    drop(r.srv);
}

#[test]
fn remote_source_with_mtimes_skips_by_size_and_time() {
    remote_flow("nas-mtime", true);
}

#[test]
fn remote_source_without_mtimes_verifies_and_resends_only_what_changed() {
    remote_flow("nas-nomtime", false);
}
