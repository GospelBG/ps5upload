//! The upload adapters end to end, against Task 17's `FolderHost` server on
//! 127.0.0.1. Built with public API only (an integration test of this crate cannot
//! see `ava1`'s own `tests/common`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ava1::gen;
use ava1::host::FolderHost;
use ava1::keys::Identity;
use ava1::manifest;
use ava1::peers::PeerStore;
use ava1::server::{self, ServerCtx};
use ava1::session::RpcReply;
use ava1::source::LocalSource;
use ava1::wire::{FrameMessage, Message};
use ava1_chaos::{ChaosConfig, ChaosProxy};
use ps5upload_ava1::route;
use ps5upload_ava1::upload;
use ps5upload_ava1::{block_on, Pool, PostCommitError, PostCommitKind};
use ps5upload_core::transfer::{FileListEntry, TransferConfig};

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("p5a-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Writes `files` files of deterministic content, `size_fn(i)` bytes each, dotted
/// through a few directories. Returns the total bytes written.
fn tree(dir: &Path, files: usize, size_fn: impl Fn(usize) -> usize) -> u64 {
    let mut total = 0u64;
    for i in 0..files {
        let p = dir.join(format!("x{}/f{i}", i % 4));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let n = size_fn(i);
        let mut data = Vec::with_capacity(n);
        let mut b = (i as u8).wrapping_mul(37).wrapping_add(11);
        while data.len() < n {
            data.push(b);
            b = b.wrapping_mul(37).wrapping_add(11);
        }
        std::fs::write(p, &data).unwrap();
        total += n as u64;
    }
    total
}

/// Both trees hold the same files, byte for byte.
fn same_tree(a: &Path, b: &Path) {
    let wa = manifest::walk(&LocalSource::new(a.to_path_buf()), &|_| false).unwrap();
    let wb = manifest::walk(&LocalSource::new(b.to_path_buf()), &|_| false).unwrap();
    let fa: Vec<_> = wa
        .entries
        .iter()
        .filter(|e| e.kind == gen::ENTRY_FILE)
        .collect();
    let fb: Vec<_> = wb
        .entries
        .iter()
        .filter(|e| e.kind == gen::ENTRY_FILE)
        .collect();
    assert_eq!(fa.len(), fb.len(), "file counts differ");
    for (ea, eb) in fa.iter().zip(&fb) {
        assert_eq!(ea.path, eb.path, "file sets differ");
        assert_eq!(ea.size, eb.size, "{}: size differs", ea.path);
        let ba = std::fs::read(a.join(&ea.path)).unwrap();
        let bb = std::fs::read(b.join(&eb.path)).unwrap();
        assert!(ba == bb, "{}: bytes differ", ea.path);
    }
}

/// Every wait in this file is bounded (project rule): a missing signal fails the
/// test, not the round.
async fn within<T>(secs: u64, f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(secs), f)
        .await
        .expect("timed out")
}

/// A folder host on 127.0.0.1. The server knows the engine's key (as a stamped
/// payload would); the pool does not know the server's — which is exactly the
/// `pairing_code().is_some() && !needs_user_pairing()` branch the pool walks (the
/// console already trusts us). Deliberate: it exercises the self-confirm path, and
/// the pool's peers file grows a line for it, so a second `session()` call for the
/// same console reuses the map entry instead of reconnecting.
async fn host(dir: &Path, host_jobs: bool) -> (String, Pool) {
    let ava = dir.join("ava");
    let me = Identity::load_or_create(&ava.join("identity")).unwrap();
    let mut peers = PeerStore::in_memory();
    peers.add(me.public(), "engine").unwrap();
    let ctx = ServerCtx::new(
        Identity::generate().unwrap(),
        "host",
        peers,
        node_info_rpc(),
    );
    let ctx = if host_jobs {
        ctx.with_jobs(Arc::new(FolderHost {
            root: dir.join("share"),
            jobs_dir: dir.join("hjobs"),
        }))
    } else {
        ctx
    };
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(server::serve(l, Arc::new(ctx)));
    (addr.clone(), Pool::new(ava).with_addr(addr))
}

/// Only METHOD_NODE_INFO gets a real answer; everything else is `ERR_UNKNOWN_METHOD`
/// (the named constant, not a literal — C8).
fn node_info_rpc() -> ava1::server::RpcHandler {
    Box::new(|method, _| {
        if method == gen::METHOD_NODE_INFO {
            let info = gen::NodeInfo {
                version: "test".into(),
                platform: "rust".into(),
                name: "host".into(),
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

fn cfg() -> TransferConfig {
    let mut c = TransferConfig::new("127.0.0.1:9113");
    c.progress_bytes = Some(Arc::new(AtomicU64::new(0)));
    c.progress_files = Some(Arc::new(AtomicU64::new(0)));
    c.progress_files_finalized = Some(Arc::new(AtomicU64::new(0)));
    c.progress_bytes_finalized = Some(Arc::new(AtomicU64::new(0)));
    c.cancel = Some(Arc::new(AtomicBool::new(false)));
    c
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_dir_lands_and_reports_progress() {
    let d = temp_dir("dir");
    std::fs::create_dir_all(&d).unwrap();
    let src = d.join("src");
    let total = tree(&src, 200, |i| i * 100);
    let (_addr, pool) = host(&d, true).await;
    let c = cfg();
    let (bytes, files_done) = (
        c.progress_bytes.clone().unwrap(),
        c.progress_files_finalized.clone().unwrap(),
    );
    let src2 = src.clone();
    let r = within(
        60,
        tokio::task::spawn_blocking(move || upload::upload_dir_in(&pool, &c, [1; 16], "in", &src2)),
    )
    .await
    .unwrap()
    .unwrap();
    let ack: serde_json::Value = serde_json::from_str(&r.commit_ack_body).unwrap();
    assert_eq!(ack["protocol"], "ava1");
    assert_eq!(ack["files"], 200);
    same_tree(&src, &d.join("share/in"));
    assert_eq!(
        bytes.load(Ordering::Relaxed),
        total,
        "progress_bytes reached the tree's total (the bridge ran, A3)"
    );
    assert_eq!(
        files_done.load(Ordering::Relaxed),
        200,
        "progress_files_finalized reached 200 (C12: durable files)"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_list_maps_relative_destinations_under_the_root() {
    let d = temp_dir("list");
    std::fs::create_dir_all(d.join("a")).unwrap();
    std::fs::write(d.join("a/1"), b"one").unwrap();
    std::fs::write(d.join("a/2"), b"two").unwrap();
    let (_addr, pool) = host(&d, true).await;
    let entries = vec![
        FileListEntry {
            src: d.join("a/1").to_string_lossy().into_owned(),
            dest: "x/1".into(),
        },
        FileListEntry {
            src: d.join("a/2").to_string_lossy().into_owned(),
            dest: "y/2".into(),
        },
    ];
    let c = cfg();
    let r = within(
        60,
        tokio::task::spawn_blocking(move || {
            upload::upload_list_in(&pool, &c, [2; 16], "list", &entries)
        }),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(r.shards_sent, 2);
    assert_eq!(std::fs::read(d.join("share/list/x/1")).unwrap(), b"one");
    assert_eq!(std::fs::read(d.join("share/list/y/2")).unwrap(), b"two");
    // The rest of the tree is untouched: only the two mapped files exist.
    let files: Vec<String> = manifest::walk(&LocalSource::new(d.join("share").clone()), &|_| false)
        .unwrap()
        .entries
        .into_iter()
        .filter(|e| e.kind == gen::ENTRY_FILE)
        .map(|e| e.path)
        .collect();
    assert_eq!(files, vec!["list/x/1".to_string(), "list/y/2".to_string()]);

    // A destination outside the root fails locally: anyhow, no console round trip
    // (a fresh pool never connects).
    let bad = vec![FileListEntry {
        src: d.join("a/1").to_string_lossy().into_owned(),
        dest: "/elsewhere/3".into(),
    }];
    let c2 = cfg();
    let p2 = Pool::new(d.join("ava")).with_addr("127.0.0.1:1");
    let err = upload::upload_list_in(&p2, &c2, [3; 16], "list", &bad).unwrap_err();
    assert!(err.to_string().contains("is not under list"), "{err:#}");
    assert_eq!(p2.attempts(), 0, "the refusal is local: nothing connected");
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_file_writes_the_destination_path_itself() {
    let d = temp_dir("single");
    std::fs::create_dir_all(d.join("src")).unwrap();
    std::fs::write(d.join("src/single.bin"), vec![0x5a; 500_000]).unwrap();
    let (_addr, pool) = host(&d, true).await;
    let c = cfg();
    let src = d.join("src/single.bin");
    let r = within(
        60,
        tokio::task::spawn_blocking(move || {
            upload::upload_file_in(&pool, &c, [4; 16], "single.bin", &src)
        }),
    )
    .await
    .unwrap()
    .unwrap();
    // C11: `dest` is the full destination path — the file lands AT the path, not in a
    // directory named after it.
    let landed = d.join("share/single.bin");
    assert!(landed.is_file(), "the destination path itself is the file");
    assert!(
        !landed.join("single.bin").exists(),
        "the destination was treated as a directory"
    );
    assert_eq!(std::fs::read(&landed).unwrap(), vec![0x5a; 500_000]);
    assert_eq!(r.shards_sent, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn post_commit_failure_is_not_a_resend() {
    let d = temp_dir("post");
    let src = d.join("src");
    let total = tree(&src, 200, |_| 128 * 1024);
    let (_addr, pool) = host(&d, true).await;
    let c = cfg();
    let sent = c.progress_bytes.clone().unwrap();
    let share = d.join("share/out");
    // Create the destination (non-empty, so even a race with the receiver's
    // exists-check cannot silently rename over it) once data is flowing: the receiver
    // staged the landing (share/out did not exist at open), so the final move
    // refuses with ERR_EXISTS — a post-commit failure, not a transport one.
    let progress = c.progress_bytes.clone().unwrap();
    let maker = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while progress.load(Ordering::Relaxed) == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the transfer never started"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        std::fs::create_dir_all(&share).unwrap();
        std::fs::write(share.join("taken"), b"x").unwrap();
    });
    let e = within(
        60,
        tokio::task::spawn_blocking(move || upload::upload_dir_in(&pool, &c, [5; 16], "out", &src)),
    )
    .await
    .unwrap()
    .unwrap_err();
    maker.join().unwrap();
    let pe = e
        .downcast_ref::<PostCommitError>()
        .expect("the refusal is a PostCommitError");
    assert_eq!(pe.kind, PostCommitKind::Exists);
    assert_eq!(pe.kind.as_str(), "ava1_commit_exists");
    assert!(
        !ps5upload_core::transfer::is_retryable_transfer_error(&e),
        "a post-commit failure must not be retried"
    );
    // No byte went twice: the loop did not resume (a post-commit failure is final),
    // so Received bytes never exceed the source by more than one bundle (64 KiB).
    assert!(
        sent.load(Ordering::Relaxed) <= total + 64 * 1024,
        "{} > {}: bytes were sent more than once",
        sent.load(Ordering::Relaxed),
        total + 64 * 1024
    );
}

/// This test owns PS5UPLOAD_TRANSFER — the only test in this file that mutates the
/// environment (the brief's carve-out; noted so nobody adds a second). Every test
/// that *reads* the routing mode takes `ENV_LOCK`, so the parallel run cannot see the
/// variable mid-mutation.
static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn routing_mode_comes_from_the_environment() {
    let _g = ENV_LOCK.lock().unwrap();
    std::env::set_var("PS5UPLOAD_TRANSFER", "ftx2");
    assert!(matches!(route::mode(), route::Mode::Ftx2));
    assert!(!route::use_ava1("192.0.2.1"));
    std::env::set_var("PS5UPLOAD_TRANSFER", "AVA1");
    assert!(matches!(route::mode(), route::Mode::Ava1));
    // Ava1: true unconditionally, without any probe (the pool never connects).
    let d = temp_dir("route");
    let p = Pool::new(d.join("ava")).with_addr("127.0.0.1:1");
    assert!(route::use_ava1_in(&p, "console"));
    assert_eq!(p.attempts(), 0, "Ava1 mode must not probe");
    std::env::remove_var("PS5UPLOAD_TRANSFER");
    assert!(matches!(route::mode(), route::Mode::Auto));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_console_that_wants_a_user_code_is_not_routed_to() {
    let d = temp_dir("pair");
    std::fs::create_dir_all(&d).unwrap();
    // A second server whose peer store does NOT know the engine's key and whose
    // pairing window is closed (the default: it only opens after open_pairing).
    let ctx = ServerCtx::new(
        Identity::generate().unwrap(),
        "stranger",
        PeerStore::in_memory(),
        node_info_rpc(),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(server::serve(l, Arc::new(ctx)));
    let pool = Pool::new(d.join("ava")).with_addr(addr.clone());
    assert!(
        pool.session(&addr).await.is_err(),
        "the console refused the stranger"
    );
    // Auto: the probe fails, the failure is cached, and the second call makes no
    // further connection attempt (A4: the hit path is pinned by counting, not by
    // sleeping). `use_ava1` is blocking (C15), so it runs on a blocking thread; the
    // lock keeps test 5's env mutation out of these calls.
    let (pool, addr) = (Arc::new(pool), addr.clone());
    let (a1, a2, attempts) = tokio::task::spawn_blocking(move || {
        let _g = ENV_LOCK.lock().unwrap();
        let a1 = route::use_ava1_in(&pool, &addr);
        let a2 = route::use_ava1_in(&pool, &addr);
        (a1, a2, pool.attempts())
    })
    .await
    .unwrap();
    assert!(!a1, "Auto does not route to it");
    assert!(!a2, "the second call is cached");
    assert_eq!(
        attempts, 2,
        "the cached failure probed again: the 30 s negative cache did not hit"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_killed_session_resumes_the_same_job() {
    let d = temp_dir("chaos");
    let src = d.join("src");
    let total = tree(&src, 16, |_| 1 << 20); // 16 MiB: long enough to kill mid-transfer
    let (addr, pool) = host(&d, true).await;
    // One deterministic kill instead of the brief's periodic killer (measured: a
    // periodic kill plus the backoff ladder lost every cycle to the 5 s wait, and a
    // tree small enough to finish between kills flaked). The cap keeps the transfer
    // slow enough that the 200 ms progress tick observes durable bytes mid-transfer;
    // the killer then drops every connection once, so the resume loop must reconnect
    // with the same job id and the console's journal must resume a partially-durable
    // job.
    let proxy = ChaosProxy::start(
        addr.parse().unwrap(),
        ChaosConfig {
            bytes_per_sec: Some(2 << 20),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let proxy = Arc::new(proxy);
    let pool = Arc::new(pool.with_addr(proxy.addr.to_string()));
    let c = cfg();
    let progress = c.progress_bytes.clone().unwrap();
    let finalized = c.progress_bytes_finalized.clone().unwrap();
    let proxy2 = proxy.clone();
    let finalized2 = finalized.clone();
    let killer = std::thread::spawn(move || {
        // Wait for durable progress (and not completion): the kill lands on a job
        // whose journal already holds bytes.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while finalized2.load(Ordering::Relaxed) == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the transfer never made anything durable"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            progress.load(Ordering::Relaxed) > 0,
            "no bytes were acked either"
        );
        proxy2.kill_all();
    });
    let (pool2, src2) = (pool.clone(), src.clone());
    let r = within(
        60,
        tokio::task::spawn_blocking(move || {
            upload::upload_dir_in(&pool2, &c, [6; 16], "in", &src2)
        }),
    )
    .await
    .unwrap()
    .unwrap();
    killer.join().unwrap();
    same_tree(&src, &d.join("share/in"));
    assert_eq!(
        finalized.load(Ordering::Relaxed),
        total,
        "the whole tree is durable after the resume"
    );
    let attempts = pool.attempts();
    let ack: serde_json::Value = serde_json::from_str(&r.commit_ack_body).unwrap();
    let resent = ack["resent"].as_u64().unwrap_or(0);
    // The observed numbers, for the flake record (--nocapture).
    println!("killed-session resume: attempts = {attempts}, resent = {resent}");
    // A whole-session death never sets `resent`: the new session sends the missing
    // ranges as fresh frames (resent counts in-session requeues, not cross-session
    // ones), so the reconnect — attempts >= 2 with durable bytes surviving in the
    // log — is the evidence the brief asks for.
    assert!(
        attempts >= 2,
        "the session was killed but the pool never reconnected (attempts = {attempts})"
    );
    assert!(
        resent > 0 || attempts >= 2,
        "a reconnect is otherwise evidenced (resent = {resent}, attempts = {attempts})"
    );
    drop(proxy);
}

#[test]
fn block_on_works_outside_any_runtime() {
    // The lab's CLI path: a plain thread with no runtime — the private fallback
    // runtime runs the future.
    let out = block_on(async {
        tokio::time::sleep(Duration::from_millis(10)).await;
        41
    });
    assert_eq!(out, 41);
}

#[tokio::test(flavor = "multi_thread")]
async fn block_on_works_from_a_blocking_thread_and_a_multithread_worker() {
    // From a spawn_blocking thread (the engine's pattern, C15): the handle's block_on.
    let out = within(
        30,
        tokio::task::spawn_blocking(|| {
            block_on(async {
                tokio::time::sleep(Duration::from_millis(10)).await;
                42
            })
        }),
    )
    .await
    .expect("block_on on a blocking thread");
    assert_eq!(out, 42);
    // From a multi-thread worker in sync context (block_in_place).
    let out = tokio::task::block_in_place(|| block_on(async { 43 }));
    assert_eq!(out, 43);
}

async fn failure_of(pool: Pool, src: PathBuf) -> (String, Duration) {
    let started = std::time::Instant::now();
    let e = within(
        60,
        tokio::task::spawn_blocking(move || {
            upload::upload_dir_in(&pool, &cfg(), [21; 16], "out", &src)
        }),
    )
    .await
    .unwrap()
    .unwrap_err();
    let f = e
        .downcast_ref::<upload::UploadFailure>()
        .unwrap_or_else(|| panic!("not an UploadFailure: {e:#}"));
    (f.reason.clone(), started.elapsed())
}

#[tokio::test(flavor = "multi_thread")]
async fn three_refused_connections_end_an_upload_as_unreachable() {
    let d = temp_dir("refused");
    let src = d.join("src");
    tree(&src, 2, |_| 1000);
    let pool = Pool::new(d.join("ava")).with_addr("127.0.0.1:1");
    let (reason, took) = failure_of(pool, src).await;
    assert_eq!(reason, "ava1_unreachable");
    assert!(took < Duration::from_secs(20), "{took:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn no_identity_ends_an_upload_without_a_connection_attempt() {
    let d = temp_dir("noid");
    let src = d.join("src");
    tree(&src, 2, |_| 1000);
    std::fs::create_dir_all(d.join("ava/identity")).unwrap();
    let pool = Pool::new(d.join("ava")).with_addr("127.0.0.1:1");
    assert!(!pool.has_identity());
    let (reason, took) = failure_of(pool, src).await;
    assert_eq!(reason, "ava1_no_identity");
    assert!(took < Duration::from_secs(3), "{took:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_console_that_wants_a_user_code_ends_an_upload_as_not_paired() {
    let d = temp_dir("notpaired");
    let src = d.join("src");
    tree(&src, 2, |_| 1000);
    let ctx = ServerCtx::new(
        Identity::generate().unwrap(),
        "stranger",
        PeerStore::in_memory(),
        node_info_rpc(),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(server::serve(l, Arc::new(ctx)));
    let pool = Pool::new(d.join("ava")).with_addr(addr);
    let (reason, took) = failure_of(pool, src).await;
    assert_eq!(reason, "ava1_not_paired");
    assert!(took < Duration::from_secs(20), "{took:?}");
}

/// A host that answers the first `busy` JobOpens `ERR_BUSY` (a console whose recovery pass holds the job id)
/// and serves the rest like `FolderHost`.
struct BusyHost {
    inner: FolderHost,
    busy: std::sync::atomic::AtomicU32,
    seen: std::sync::atomic::AtomicU32,
}

impl ava1::router::JobHost for BusyHost {
    fn accept(&self, link: ava1::router::JobLink, first: ava1::conn::Frame, peer: [u8; 32]) {
        use std::sync::atomic::Ordering::SeqCst;
        if first.ty == gen::JobOpen::TYPE {
            self.seen.fetch_add(1, SeqCst);
            let left = self.busy.load(SeqCst);
            if left > 0 {
                self.busy.store(left - 1, SeqCst);
                if let Ok(open) = first.decode::<gen::JobOpen>() {
                    tokio::spawn(async move {
                        let _ = link
                            .control
                            .send(&gen::JobOpenAck {
                                job_id: open.job_id,
                                status: gen::ERR_BUSY,
                                credit: 0,
                                staged: 0,
                                workers: 0,
                                message: Some(
                                    "the console is finishing this job's files; try again".into(),
                                ),
                            })
                            .await;
                    });
                    return;
                }
            }
        }
        self.inner.accept(link, first, peer)
    }
}

async fn busy_host(dir: &Path, busy: u32) -> (Arc<BusyHost>, Pool) {
    let ava = dir.join("ava");
    let me = Identity::load_or_create(&ava.join("identity")).unwrap();
    let mut peers = PeerStore::in_memory();
    peers.add(me.public(), "engine").unwrap();
    let h = Arc::new(BusyHost {
        inner: FolderHost {
            root: dir.join("share"),
            jobs_dir: dir.join("hjobs"),
        },
        busy: busy.into(),
        seen: 0.into(),
    });
    let ctx = ServerCtx::new(
        Identity::generate().unwrap(),
        "host",
        peers,
        node_info_rpc(),
    )
    .with_jobs(h.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(server::serve(l, Arc::new(ctx)));
    (h, Pool::new(ava).with_addr(addr))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_busy_job_open_is_retried_until_the_console_accepts() {
    // the console answers BUSY twice (recovery holds the job), then OK: the upload completes
    let d = temp_dir("busy-then-ok");
    let src = d.join("src");
    tree(&src, 40, |_| 4096);
    let (h, pool) = busy_host(&d, 2).await;
    let c = cfg();
    let r = within(
        60,
        tokio::task::spawn_blocking(move || {
            upload::upload_dir_in(&pool, &c, [0x21; 16], "out", &src)
        }),
    )
    .await
    .unwrap();
    r.expect("the upload completes after the BUSY answers");
    assert_eq!(
        h.seen.load(Ordering::SeqCst),
        3,
        "two BUSY answers, then the real open"
    );
    same_tree(&d.join("src"), &d.join("share/out"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_console_that_stays_busy_fails_the_upload_after_the_bound_with_a_clear_reason() {
    let d = temp_dir("busy-forever");
    let src = d.join("src");
    tree(&src, 4, |_| 1024);
    let (h, pool) = busy_host(&d, u32::MAX).await;
    let pool = pool.with_busy_tries(3);
    let c = cfg();
    let e = within(
        60,
        tokio::task::spawn_blocking(move || {
            upload::upload_dir_in(&pool, &c, [0x22; 16], "out", &src)
        }),
    )
    .await
    .unwrap()
    .unwrap_err();
    let f = e
        .downcast_ref::<upload::UploadFailure>()
        .unwrap_or_else(|| panic!("not a classified failure: {e:#}"));
    assert_eq!(f.reason, "ava1_busy", "{f:?}");
    assert!(f.detail.contains("busy"), "{}", f.detail);
    assert_eq!(
        h.seen.load(Ordering::SeqCst),
        4,
        "the first try and three retries, then it gave up"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_ends_the_busy_wait() {
    let d = temp_dir("busy-cancel");
    let src = d.join("src");
    tree(&src, 4, |_| 1024);
    let (_h, pool) = busy_host(&d, u32::MAX).await;
    let c = cfg();
    let cancel = c.cancel.clone().unwrap();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(600));
        cancel.store(true, Ordering::Relaxed);
    });
    let t = std::time::Instant::now();
    let e = within(
        60,
        tokio::task::spawn_blocking(move || {
            upload::upload_dir_in(&pool, &c, [0x23; 16], "out", &src)
        }),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(e.to_string().contains("cancel"), "{e:#}");
    assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
}
