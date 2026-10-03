use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use ava1::gen;
use ava1::host::FolderHost;
use ava1::keys::Identity;
use ava1::manifest::{Entry, Manifest};
use ava1::peers::PeerStore;
use ava1::send::Progress;
use ava1::server::{self, ServerCtx};
use ava1::session::RpcReply;
use ava1::wire::Message;
use ava1_chaos::{ChaosConfig, ChaosProxy};
use ps5upload_ava1::relay::ps5_to_ps5_between;
use ps5upload_ava1::upload::{self, ZipTooLarge, ZIP_MAX_ENTRY};
use ps5upload_ava1::zip_source::ZipSource;
use ps5upload_ava1::Pool;
use ps5upload_core::transfer::TransferConfig;

fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ava1-relay-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn rpc() -> ava1::server::RpcHandler {
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

async fn host(root: &Path, engine_key: [u8; 32]) -> String {
    let mut peers = PeerStore::in_memory();
    peers.add(engine_key, "engine").unwrap();
    let ctx = ServerCtx::new(Identity::generate().unwrap(), "host", peers, rpc()).with_jobs(
        Arc::new(FolderHost {
            root: root.join("share"),
            jobs_dir: root.join("jobs"),
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(server::serve(l, Arc::new(ctx)));
    addr
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tree_relays_between_two_hosts() {
    let d = temp("tree");
    let ava = d.join("engine");
    let key = Identity::load_or_create(&ava.join("identity"))
        .unwrap()
        .public();
    let a = d.join("a");
    let b = d.join("b");
    std::fs::create_dir_all(a.join("share/src/nested")).unwrap();
    std::fs::create_dir_all(b.join("share")).unwrap();
    for i in 0..40 {
        let n = if i < 2 { 40 << 20 } else { 1000 + i };
        std::fs::write(a.join(format!("share/src/nested/f{i}")), vec![i as u8; n]).unwrap();
    }
    std::fs::write(a.join("share/src/nested/empty"), []).unwrap();
    let (addr_a, addr_b) = (host(&a, key).await, host(&b, key).await);
    let (pool_a, pool_b) = (
        Pool::new(ava.clone()).with_addr(addr_a),
        Pool::new(ava).with_addr(addr_b),
    );
    let report = tokio::time::timeout(
        Duration::from_secs(90),
        tokio::task::spawn_blocking(move || {
            ps5_to_ps5_between(
                &pool_a,
                "a",
                "src",
                &pool_b,
                "b",
                "dst",
                [11; 16],
                Arc::new(Progress::default()),
                Arc::new(AtomicBool::new(false)),
            )
        }),
    )
    .await
    .expect("relay timed out")
    .unwrap()
    .unwrap();
    assert_eq!(report.status, gen::STATUS_OK);
    assert_eq!(report.files, 41);
    for i in 0..40 {
        assert_eq!(
            std::fs::read(a.join(format!("share/src/nested/f{i}"))).unwrap(),
            std::fs::read(b.join(format!("share/dst/nested/f{i}"))).unwrap()
        );
    }
    assert!(std::fs::read(b.join("share/dst/nested/empty"))
        .unwrap()
        .is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_partly_durable_file_resumes_without_hanging() {
    let d = temp("resume");
    let ava = d.join("engine");
    let key = Identity::load_or_create(&ava.join("identity"))
        .unwrap()
        .public();
    let a = d.join("a");
    let b = d.join("b");
    std::fs::create_dir_all(a.join("share/src")).unwrap();
    std::fs::create_dir_all(b.join("share")).unwrap();
    const SIZE: u64 = 160 << 20;
    std::fs::File::create(a.join("share/src/big"))
        .unwrap()
        .set_len(SIZE)
        .unwrap();
    let (addr_a, addr_b) = (host(&a, key).await, host(&b, key).await);
    let progress = Arc::new(Progress::default());
    let cancel = Arc::new(AtomicBool::new(false));
    let first_progress = progress.clone();
    let first_cancel = cancel.clone();
    let first_ava = ava.clone();
    let first_a = addr_a.clone();
    let first_b = addr_b.clone();
    let first = tokio::task::spawn_blocking(move || {
        let pa = Pool::new(first_ava.clone()).with_addr(first_a);
        let pb = Pool::new(first_ava).with_addr(first_b);
        ps5_to_ps5_between(
            &pa,
            "a",
            "src",
            &pb,
            "b",
            "dst",
            [15; 16],
            first_progress,
            first_cancel,
        )
    });
    tokio::time::timeout(Duration::from_secs(40), async {
        loop {
            let durable = progress.bytes_durable.load(Ordering::Relaxed);
            if durable > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("first attempt made no durable progress");
    assert!(
        progress.bytes_durable.load(Ordering::Relaxed) < SIZE,
        "first attempt finished before it could be interrupted"
    );
    cancel.store(true, Ordering::Relaxed);
    let _ = tokio::time::timeout(Duration::from_secs(40), first)
        .await
        .expect("cancelled attempt hung")
        .unwrap();
    let second = tokio::time::timeout(
        Duration::from_secs(90),
        tokio::task::spawn_blocking(move || {
            let pa = Pool::new(ava.clone()).with_addr(addr_a);
            let pb = Pool::new(ava).with_addr(addr_b);
            ps5_to_ps5_between(
                &pa,
                "a",
                "src",
                &pb,
                "b",
                "dst",
                [15; 16],
                Arc::new(Progress::default()),
                Arc::new(AtomicBool::new(false)),
            )
        }),
    )
    .await
    .expect("resumed attempt hung")
    .unwrap()
    .unwrap();
    assert_eq!(second.status, gen::STATUS_OK);
    assert_eq!(second.bytes, SIZE);
    let dest = b.join("share/dst/big");
    assert_eq!(std::fs::metadata(&dest).unwrap().len(), SIZE);
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(dest).unwrap();
    let mut sample = [1u8; 4096];
    f.read_exact(&mut sample).unwrap();
    assert_eq!(sample, [0; 4096]);
    f.seek(SeekFrom::End(-4096)).unwrap();
    f.read_exact(&mut sample).unwrap();
    assert_eq!(sample, [0; 4096]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_destination_connection_killed_midway_resumes() {
    let d = temp("kill-destination");
    let ava = d.join("engine");
    let key = Identity::load_or_create(&ava.join("identity"))
        .unwrap()
        .public();
    let a = d.join("a");
    let b = d.join("b");
    std::fs::create_dir_all(a.join("share/src")).unwrap();
    std::fs::create_dir_all(b.join("share")).unwrap();
    const SIZE: u64 = 48 << 20;
    std::fs::File::create(a.join("share/src/big"))
        .unwrap()
        .set_len(SIZE)
        .unwrap();
    let (addr_a, addr_b) = (host(&a, key).await, host(&b, key).await);
    let proxy = ChaosProxy::start(
        addr_b.parse().unwrap(),
        ChaosConfig {
            bytes_per_sec: Some(4 << 20),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let pa = Arc::new(Pool::new(ava.clone()).with_addr(addr_a));
    let pb = Arc::new(Pool::new(ava).with_addr(proxy.addr.to_string()));
    let progress = Arc::new(Progress::default());
    let (pa2, pb2, progress2) = (pa.clone(), pb.clone(), progress.clone());
    let run = tokio::task::spawn_blocking(move || {
        ps5_to_ps5_between(
            &pa2,
            "a",
            "src",
            &pb2,
            "b",
            "dst",
            [16; 16],
            progress2,
            Arc::new(AtomicBool::new(false)),
        )
    });
    tokio::time::timeout(Duration::from_secs(40), async {
        while progress.bytes_durable.load(Ordering::Relaxed) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("relay made no durable progress");
    assert!(progress.bytes_durable.load(Ordering::Relaxed) < SIZE);
    proxy.kill_all();
    let report = tokio::time::timeout(Duration::from_secs(90), run)
        .await
        .expect("killed destination did not resume")
        .unwrap()
        .unwrap();
    assert_eq!(report.status, gen::STATUS_OK);
    assert_eq!(report.bytes, SIZE);
    assert!(pb.attempts() >= 2, "destination was never reconnected");
    assert_eq!(
        std::fs::metadata(b.join("share/dst/big")).unwrap().len(),
        SIZE
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_zip_uploads_and_matches_its_contents() {
    let d = temp("upload-zip");
    let ava = d.join("engine");
    let key = Identity::load_or_create(&ava.join("identity"))
        .unwrap()
        .public();
    let host_root = d.join("host");
    std::fs::create_dir_all(host_root.join("share")).unwrap();
    let addr = host(&host_root, key).await;
    let path = d.join("input.zip");
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
    for i in 0..200 {
        let method = if i == 199 {
            zip::CompressionMethod::Stored
        } else {
            zip::CompressionMethod::Deflated
        };
        let name = format!("nested/f{i}");
        zip.start_file(
            &name,
            zip::write::SimpleFileOptions::default().compression_method(method),
        )
        .unwrap();
        let n = if i < 2 { 3 << 20 } else { i + 100 };
        zip.write_all(&vec![i as u8; n]).unwrap();
    }
    zip.finish().unwrap();
    let pool = Pool::new(ava).with_addr(addr);
    let result = tokio::time::timeout(
        Duration::from_secs(90),
        tokio::task::spawn_blocking(move || {
            upload::upload_zip_in(
                &pool,
                &TransferConfig::new("127.0.0.1:9113"),
                [14; 16],
                "dst",
                &path,
            )
        }),
    )
    .await
    .expect("zip upload timed out")
    .unwrap()
    .unwrap();
    assert_eq!(result.shards_sent, 200);
    for i in 0..200 {
        let n = if i < 2 { 3 << 20 } else { i + 100 };
        assert_eq!(
            std::fs::read(host_root.join(format!("share/dst/nested/f{i}"))).unwrap(),
            vec![i as u8; n]
        );
    }
}

#[test]
fn traversal_zip_fails_before_connecting() {
    let d = temp("traversal");
    let path = d.join("bad.zip");
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
    zip.start_file("../evil", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(b"bad").unwrap();
    zip.finish().unwrap();
    let pool = Pool::new(d.join("ava")).with_addr("127.0.0.1:1");
    let err = upload::upload_zip_in(
        &pool,
        &TransferConfig::new("127.0.0.1:1"),
        [12; 16],
        "dst",
        &path,
    )
    .unwrap_err();
    assert!(err.to_string().contains("../evil"), "{err:#}");
}

#[test]
fn large_zip_entry_has_a_typed_error() {
    let m = Manifest {
        entries: vec![Entry {
            kind: gen::ENTRY_FILE,
            size: ZIP_MAX_ENTRY + 1,
            path: "huge".into(),
            mode: 0o644,
            mtime: 0,
            root: None,
        }],
    };
    let name = upload::zip_too_large(&m).unwrap();
    let e: anyhow::Error = ZipTooLarge(name.to_owned()).into();
    assert!(e.downcast_ref::<ZipTooLarge>().is_some());
}

#[test]
fn zip_source_reads_nested_and_stored_entries() {
    let d = temp("zip");
    let path = d.join("input.zip");
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
    zip.start_file(
        "nested/one",
        zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated),
    )
    .unwrap();
    zip.write_all(b"hello world").unwrap();
    zip.start_file(
        "two",
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
    )
    .unwrap();
    zip.write_all(b"stored").unwrap();
    zip.finish().unwrap();
    let (m, source) = ZipSource::open(&path, &[]).unwrap();
    assert_eq!(m.files(), 2);
    let mut r = ava1::source::Source::open(&source, "nested/one").unwrap();
    let mut buf = [0u8; 5];
    assert_eq!(r.read_at(6, &mut buf).unwrap(), 5);
    assert_eq!(&buf, b"world");
}
