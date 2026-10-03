//! 7z uploads over AVA1 (Task 11): a forward-only `SevenzSource` feeding the
//! sender's decode thread, against the Rust folder host on 127.0.0.1.
//! Archives are written by the `sevenz-rust2` writer; every wait is bounded.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ava1::gen;
use ava1::host::FolderHost;
use ava1::keys::Identity;
use ava1::manifest::Manifest;
use ava1::peers::PeerStore;
use ava1::seq::{EntrySink, Keep, Restart, SeqSource};
use ava1::server::{self, ServerCtx};
use ava1::session::RpcReply;
use ava1::wire::Message;
use ava1_chaos::{ChaosConfig, ChaosProxy};
use ps5upload_ava1::seq::SevenzSource;
use ps5upload_ava1::upload::{self, UploadFailure};
use ps5upload_ava1::Pool;
use ps5upload_core::transfer::TransferConfig;
use sevenz_rust2::{
    ArchiveEntry, ArchiveWriter, EncoderConfiguration, EncoderMethod, SourceReader,
};

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("p5z-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Deterministic incompressible bytes.
fn noise(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut v = Vec::with_capacity(n);
    while v.len() < n {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.truncate(n);
    v
}

/// A folder is one solid block; `copy` stores without compression (a flipped byte then
/// reaches the CRC check instead of the LZMA decoder).
struct Spec {
    folders: Vec<Vec<(String, Vec<u8>)>>,
    dirs: Vec<String>,
    empties: Vec<String>,
    copy: bool,
}

fn build(path: &Path, spec: &Spec) {
    let mut w = ArchiveWriter::create(path).unwrap();
    if spec.copy {
        w.set_content_methods(vec![EncoderConfiguration::from(EncoderMethod::COPY)]);
    }
    for d in &spec.dirs {
        w.push_archive_entry::<&[u8]>(ArchiveEntry::new_directory(d), None)
            .unwrap();
    }
    for f in &spec.folders {
        let entries: Vec<ArchiveEntry> = f.iter().map(|(n, _)| ArchiveEntry::new_file(n)).collect();
        let readers: Vec<SourceReader<&[u8]>> =
            f.iter().map(|(_, d)| SourceReader::new(&d[..])).collect();
        w.push_archive_entries(entries, readers).unwrap();
    }
    for e in &spec.empties {
        let mut en = ArchiveEntry::new_file(e);
        en.has_stream = false;
        w.push_archive_entry::<&[u8]>(en, None).unwrap();
    }
    w.finish().unwrap();
}

fn expected(spec: &Spec) -> BTreeMap<String, Vec<u8>> {
    let mut m = BTreeMap::new();
    for f in &spec.folders {
        for (n, d) in f {
            m.insert(n.clone(), d.clone());
        }
    }
    for e in &spec.empties {
        m.insert(e.clone(), Vec::new());
    }
    m
}

/// `n` folders of `files` files of `size` bytes each, named `d<k>/f<i>`.
fn folders(n: usize, files: usize, size: usize) -> Spec {
    Spec {
        folders: (0..n)
            .map(|k| {
                (0..files)
                    .map(|i| (format!("d{k}/f{i}"), noise((k * 1000 + i) as u64, size + i)))
                    .collect()
            })
            .collect(),
        dirs: vec![],
        empties: vec![],
        copy: false,
    }
}

fn read_tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                let rel = p
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.insert(rel, std::fs::read(&p).unwrap());
            }
        }
    }
    let mut m = BTreeMap::new();
    walk(root, root, &mut m);
    m
}

fn same(want: &BTreeMap<String, Vec<u8>>, got: &BTreeMap<String, Vec<u8>>) {
    assert_eq!(
        want.keys().collect::<Vec<_>>(),
        got.keys().collect::<Vec<_>>(),
        "file sets differ"
    );
    for (k, v) in want {
        assert!(&got[k] == v, "{k}: bytes differ");
    }
}

async fn within<T>(secs: u64, f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(secs), f)
        .await
        .expect("timed out")
}

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

/// A folder host on 127.0.0.1 that already trusts the engine's key; returns its
/// address and the identity directory pools are built from.
async fn host(dir: &Path) -> (String, PathBuf) {
    let ava = dir.join("ava");
    let me = Identity::load_or_create(&ava.join("identity")).unwrap();
    let mut peers = PeerStore::in_memory();
    peers.add(me.public(), "engine").unwrap();
    let ctx = ServerCtx::new(
        // Stable across calls: a second `host(dir)` is "the same console" again.
        Identity::load_or_create(&dir.join("host-id")).unwrap(),
        "host",
        peers,
        node_info_rpc(),
    )
    .with_jobs(Arc::new(FolderHost {
        root: dir.join("share"),
        jobs_dir: dir.join("hjobs"),
    }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(server::serve(l, Arc::new(ctx)));
    (addr, ava)
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

// ---- pass-level tests (no network) ------------------------------------------------

#[derive(Default)]
struct Collect {
    files: BTreeMap<String, Vec<u8>>,
    cur: Option<String>,
}
impl EntrySink for Collect {
    fn begin(&mut self, path: &str) -> io::Result<()> {
        self.cur = Some(path.into());
        self.files.insert(path.into(), Vec::new());
        Ok(())
    }
    fn data(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.files
            .get_mut(self.cur.as_ref().unwrap())
            .unwrap()
            .extend_from_slice(bytes);
        Ok(())
    }
    fn end(&mut self) -> io::Result<()> {
        self.cur = None;
        Ok(())
    }
}

/// What a resume does: restart at the earliest unfinished file's folder, `Skip` the
/// files in `done`.
fn resume_pass(
    src: &SevenzSource,
    m: &Manifest,
    done: &[&str],
) -> io::Result<BTreeMap<String, Vec<u8>>> {
    let restart = m
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.kind == gen::ENTRY_FILE && !done.contains(&e.path.as_str()))
        .map(|(i, _)| src.restart_for(i as u32))
        .min()
        .unwrap_or(Restart::START);
    let mut sink = Collect::default();
    let cancel = AtomicBool::new(false);
    src.pass(
        restart,
        &mut |p, _| {
            if done.contains(&p) {
                Keep::Skip
            } else {
                Keep::All
            }
        },
        &mut sink,
        &cancel,
    )?;
    Ok(sink.files)
}

#[test]
fn nonsolid_resume_skips_whole_folders() {
    let d = temp_dir("nonsolid");
    // One folder per file: push each file on its own.
    let mut w = ArchiveWriter::create(d.join("a.7z")).unwrap();
    let datas: Vec<Vec<u8>> = (0..10).map(|i| noise(i, 20_000 + i as usize)).collect();
    for (i, data) in datas.iter().enumerate() {
        w.push_archive_entry(ArchiveEntry::new_file(&format!("f{i}")), Some(&data[..]))
            .unwrap();
    }
    w.finish().unwrap();
    let (m, src) = SevenzSource::open(&d.join("a.7z"), &[]).unwrap();
    assert_eq!(src.folder_count(), 10);
    let done: Vec<String> = (0..8).map(|i| format!("f{i}")).collect();
    let done_refs: Vec<&str> = done.iter().map(String::as_str).collect();
    let got = resume_pass(&src, &m, &done_refs).unwrap();
    assert_eq!(got.keys().collect::<Vec<_>>(), vec!["f8", "f9"]);
    assert_eq!(got["f8"], datas[8]);
    assert_eq!(
        src.folders_opened(),
        2,
        "only the unfinished folders opened"
    );
    assert_eq!(
        src.bytes_skipped(),
        0,
        "nothing was decoded to be discarded"
    );
}

#[test]
fn solid_resume_decodes_from_the_folder_start_and_stops_after_the_last_wanted_file() {
    let d = temp_dir("solid");
    let spec = folders(2, 4, 30_000);
    build(&d.join("a.7z"), &spec);
    let (m, src) = SevenzSource::open(&d.join("a.7z"), &[]).unwrap();
    assert_eq!(src.folder_count(), 2);
    // Folder 0 done; folder 1: f0,f1 done, f2 wanted, f3 done.
    let done = [
        "d0/f0", "d0/f1", "d0/f2", "d0/f3", "d1/f0", "d1/f1", "d1/f3",
    ];
    let got = resume_pass(&src, &m, &done).unwrap();
    assert_eq!(got.keys().collect::<Vec<_>>(), vec!["d1/f2"]);
    assert_eq!(got["d1/f2"], spec.folders[1][2].1);
    assert_eq!(
        src.folders_opened(),
        1,
        "the finished folder is never opened"
    );
    let sizes: Vec<u64> = spec.folders[1]
        .iter()
        .map(|(_, d)| d.len() as u64)
        .collect();
    assert_eq!(
        src.bytes_skipped(),
        sizes[0] + sizes[1],
        "the folder prefix is decoded and dropped"
    );
    assert_eq!(
        src.bytes_decoded(),
        sizes[0] + sizes[1] + sizes[2],
        "decoding stops after the last wanted file"
    );
}

#[test]
fn cancel_during_a_long_skip_ends_promptly() {
    let d = temp_dir("cancel");
    // 24 MiB of a repeating pattern then a small file, one solid folder.
    let pat: Vec<u8> = (0..4096u32).map(|i| (i * 7 % 251) as u8).collect();
    let big: Vec<u8> = pat.iter().copied().cycle().take(24 << 20).collect();
    let spec = Spec {
        folders: vec![vec![("big".into(), big), ("tail".into(), vec![9; 100])]],
        dirs: vec![],
        empties: vec![],
        copy: false,
    };
    build(&d.join("a.7z"), &spec);
    let (_m, src) = SevenzSource::open(&d.join("a.7z"), &[]).unwrap();
    let src = Arc::new(src);
    let cancel = Arc::new(AtomicBool::new(false));
    let trigger = {
        let (src, cancel) = (src.clone(), cancel.clone());
        std::thread::spawn(move || {
            let t0 = Instant::now();
            while src.bytes_skipped() < (1 << 20) {
                assert!(
                    t0.elapsed() < Duration::from_secs(60),
                    "the skip never began"
                );
                std::thread::sleep(Duration::from_micros(200));
            }
            cancel.store(true, Ordering::Relaxed);
        })
    };
    let t0 = Instant::now();
    let r = src.pass(
        Restart::START,
        &mut |p, _| if p == "tail" { Keep::All } else { Keep::Skip },
        &mut Collect::default(),
        &cancel,
    );
    let took = t0.elapsed();
    trigger.join().unwrap();
    let e = r.expect_err("the pass was cancelled");
    assert_eq!(e.kind(), io::ErrorKind::Interrupted);
    assert!(
        src.bytes_skipped() < (16 << 20),
        "stopped within a few buffers, not at the end ({} skipped)",
        src.bytes_skipped()
    );
    assert!(took < Duration::from_secs(5), "{took:?}");
}

#[test]
fn a_sink_error_comes_back_from_pass() {
    struct Failing;
    impl EntrySink for Failing {
        fn begin(&mut self, _: &str) -> io::Result<()> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "lane gone"))
        }
        fn data(&mut self, _: &[u8]) -> io::Result<()> {
            Ok(())
        }
        fn end(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let d = temp_dir("sinkerr");
    build(&d.join("a.7z"), &folders(1, 2, 1000));
    let (_m, src) = SevenzSource::open(&d.join("a.7z"), &[]).unwrap();
    let e = src
        .pass(
            Restart::START,
            &mut |_, _| Keep::All,
            &mut Failing,
            &AtomicBool::new(false),
        )
        .unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::BrokenPipe);
}

#[test]
fn the_manifest_has_dirs_empty_files_and_honours_excludes() {
    let d = temp_dir("manifest");
    let mut spec = folders(1, 3, 500);
    spec.dirs = vec!["emptydir".into()];
    spec.empties = vec!["zero.bin".into(), "d0/zero2".into()];
    build(&d.join("a.7z"), &spec);
    let (m, _src) =
        SevenzSource::open(&d.join("a.7z"), &["f1".to_string()]).unwrap_or_else(|e| panic!("{e}"));
    let kinds: Vec<(&str, u8)> = m
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e.kind))
        .collect();
    assert!(kinds.contains(&("emptydir", gen::ENTRY_DIR)));
    assert!(kinds.contains(&("zero.bin", gen::ENTRY_FILE)));
    assert!(kinds.contains(&("d0", gen::ENTRY_DIR)));
    // `f1` is excluded by name wherever it sits.
    let (m2, _) = SevenzSource::open(&d.join("a.7z"), &["f1".to_string()]).unwrap();
    assert!(m2.entries.iter().all(|e| e.path != "d0/f1"), "{kinds:?}");
}

// ---- upload tests -------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn upload_verifies_a_multi_folder_solid_archive() {
    let d = temp_dir("upload");
    // 3 solid folders, 200 files in all, one above the large-file cutoff, plus empties.
    let mut spec = folders(3, 66, 3000);
    spec.folders[1].push(("d1/big.bin".into(), noise(77, 600_000)));
    spec.dirs = vec!["emptydir".into()];
    spec.empties = vec!["zero.bin".into()];
    build(&d.join("a.7z"), &spec);
    let (addr, ava) = host(&d).await;
    let pool = Pool::new(ava).with_addr(addr);
    let c = cfg();
    let (fin, files) = (
        c.progress_bytes_finalized.clone().unwrap(),
        c.progress_files_finalized.clone().unwrap(),
    );
    let arc = d.join("a.7z");
    let r = within(
        120,
        tokio::task::spawn_blocking(move || upload::upload_7z_in(&pool, &c, [7; 16], "out", &arc)),
    )
    .await
    .unwrap()
    .unwrap();
    let ack: serde_json::Value = serde_json::from_str(&r.commit_ack_body).unwrap();
    assert_eq!(ack["protocol"], "ava1");
    let want = expected(&spec);
    assert_eq!(ack["files"], want.len());
    let total: usize = want.values().map(Vec::len).sum();
    same(&want, &read_tree(&d.join("share/out")));
    assert!(d.join("share/out/emptydir").is_dir(), "empty dir created");
    assert_eq!(fin.load(Ordering::Relaxed), total as u64);
    assert_eq!(files.load(Ordering::Relaxed), want.len() as u64);
}

/// Uploads `arc` through a throttled proxy and cancels once `fin_bytes` are durable;
/// returns the durable byte count at the cancel.
async fn partial_upload(d: &Path, arc: &Path, job: [u8; 16], fin_bytes: u64) -> u64 {
    let (addr, ava) = host(d).await;
    let proxy = Arc::new(
        ChaosProxy::start(
            addr.parse().unwrap(),
            ChaosConfig {
                bytes_per_sec: Some(2 << 20),
                ..Default::default()
            },
        )
        .await
        .unwrap(),
    );
    let pool = Pool::new(ava).with_addr(proxy.addr.to_string());
    let c = cfg();
    let fin = c.progress_bytes_finalized.clone().unwrap();
    let cancel = c.cancel.clone().unwrap();
    let watcher = {
        let fin = fin.clone();
        std::thread::spawn(move || {
            let t0 = Instant::now();
            while fin.load(Ordering::Relaxed) < fin_bytes {
                assert!(
                    t0.elapsed() < Duration::from_secs(60),
                    "never reached the cut"
                );
                std::thread::sleep(Duration::from_millis(2));
            }
            cancel.store(true, Ordering::Relaxed);
        })
    };
    let arc = arc.to_owned();
    let e = within(
        90,
        tokio::task::spawn_blocking(move || upload::upload_7z_in(&pool, &c, job, "out", &arc)),
    )
    .await
    .unwrap()
    .unwrap_err();
    watcher.join().unwrap();
    assert!(e.to_string().contains("transfer_cancelled"), "{e:#}");
    drop(proxy);
    fin.load(Ordering::Relaxed)
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_restarts_the_folder_and_sends_only_missing_files() {
    let d = temp_dir("resume");
    let spec = folders(3, 20, 64 * 1024 - 20);
    build(&d.join("a.7z"), &spec);
    let total: u64 = expected(&spec).values().map(|v| v.len() as u64).sum();
    let fin1 = partial_upload(&d, &d.join("a.7z"), [3; 16], 2 << 20).await;
    assert!(fin1 >= 2 << 20 && fin1 < total, "{fin1} of {total}");

    // Second attempt: a fresh source object, the same job id, straight to the host.
    let (addr, ava) = host(&d).await;
    let pool = Pool::new(ava).with_addr(addr);
    let c = cfg();
    let sent = c.progress_bytes.clone().unwrap();
    let (m, src) = SevenzSource::open(&d.join("a.7z"), &[]).unwrap();
    let src = Arc::new(src);
    let src2 = src.clone();
    within(
        120,
        tokio::task::spawn_blocking(move || {
            upload::upload_7z_source_in(&pool, &c, [3; 16], "out", m, src2)
        }),
    )
    .await
    .unwrap()
    .unwrap();
    same(&expected(&spec), &read_tree(&d.join("share/out")));
    assert!(
        sent.load(Ordering::Relaxed) <= total - fin1,
        "sent {} but only {} were missing",
        sent.load(Ordering::Relaxed),
        total - fin1
    );
    assert!(
        src.folders_opened() <= 2,
        "the finished folder was not decoded again ({} opened)",
        src.folders_opened()
    );
    assert!(
        src.bytes_skipped() > 0,
        "the restart folder's done prefix was discarded"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn changed_archive_restarts_not_splices() {
    let d = temp_dir("changed");
    let a = folders(3, 20, 64 * 1024 - 20);
    build(&d.join("a.7z"), &a);
    let fin1 = partial_upload(&d, &d.join("a.7z"), [4; 16], 2 << 20).await;
    assert!(fin1 >= 2 << 20);
    // Same names and sizes, different bytes.
    let mut b = folders(3, 20, 64 * 1024 - 20);
    for f in &mut b.folders {
        for (n, data) in f.iter_mut() {
            *data = noise(n.len() as u64 * 31 + data.len() as u64 + 99, data.len());
        }
    }
    build(&d.join("a.7z"), &b);
    let (addr, ava) = host(&d).await;
    let pool = Pool::new(ava).with_addr(addr);
    let arc = d.join("a.7z");
    within(
        120,
        tokio::task::spawn_blocking(move || {
            upload::upload_7z_in(&pool, &cfg(), [4; 16], "out", &arc)
        }),
    )
    .await
    .unwrap()
    .unwrap();
    same(&expected(&b), &read_tree(&d.join("share/out")));
}

fn failure(e: &anyhow::Error) -> &UploadFailure {
    e.downcast_ref::<UploadFailure>()
        .unwrap_or_else(|| panic!("not a typed failure: {e:#}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_corrupt_archive_fails_with_a_typed_reason() {
    let d = temp_dir("corrupt");
    // (1) not a 7z at all, (2) a truncated archive.
    std::fs::write(d.join("junk.7z"), vec![0x42u8; 5000]).unwrap();
    let spec = folders(1, 3, 10_000);
    build(&d.join("good.7z"), &spec);
    let good = std::fs::read(d.join("good.7z")).unwrap();
    std::fs::write(d.join("cut.7z"), &good[..good.len() / 2]).unwrap();
    let p = Pool::new(d.join("ava")).with_addr("127.0.0.1:1");
    for name in ["junk.7z", "cut.7z"] {
        let e = upload::upload_7z_in(&p, &cfg(), [5; 16], "out", &d.join(name)).unwrap_err();
        assert_eq!(failure(&e).reason, "ava1_7z_corrupt", "{name}: {e:#}");
    }
    assert_eq!(p.attempts(), 0, "refused locally, no connection");

    // (3) a flipped data byte: the header is intact, the entry fails its CRC mid-upload.
    let mut stored = folders(1, 3, 10_000);
    stored.copy = true;
    build(&d.join("flip.7z"), &stored);
    let mut bytes = std::fs::read(d.join("flip.7z")).unwrap();
    bytes[32 + 5_000] ^= 0xff;
    std::fs::write(d.join("flip.7z"), &bytes).unwrap();
    let (addr, ava) = host(&d).await;
    let pool = Pool::new(ava).with_addr(addr);
    let arc = d.join("flip.7z");
    let e = within(
        60,
        tokio::task::spawn_blocking(move || {
            upload::upload_7z_in(&pool, &cfg(), [6; 16], "out", &arc)
        }),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(failure(&e).reason, "ava1_7z_corrupt", "{e:#}");
    assert!(!d.join("share/out").exists(), "nothing was committed");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unsafe_entry_path_is_refused() {
    let d = temp_dir("unsafe");
    let spec = Spec {
        folders: vec![vec![("../escape.txt".into(), vec![1; 10])]],
        dirs: vec![],
        empties: vec![],
        copy: false,
    };
    build(&d.join("evil.7z"), &spec);
    let p = Pool::new(d.join("ava")).with_addr("127.0.0.1:1");
    let e = upload::upload_7z_in(&p, &cfg(), [8; 16], "out", &d.join("evil.7z")).unwrap_err();
    assert_eq!(failure(&e).reason, "ava1_7z_unsafe_path", "{e:#}");
    assert_eq!(p.attempts(), 0);
}
