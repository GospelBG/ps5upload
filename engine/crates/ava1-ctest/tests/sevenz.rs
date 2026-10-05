#![cfg(unix)]
//! Task 11 against the payload's real C receiver: a 7z uploaded through the sequential
//! source (decode-order records and chunks, a root after each large file, empty files
//! last) lands byte for byte, and survives a payload restart mid-upload.
mod common;

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use ava1::gen;
use ava1::send::{send_job, Progress, SendError, SendOptions};
use ava1::session::connect;
use ava1_chaos::{ChaosConfig, ChaosProxy};
use ava1_ctest::CServer;
use common::*;
use ps5upload_ava1::seq::{NoSource, SevenzSource};
use sevenz_rust2::{ArchiveEntry, ArchiveWriter, SourceReader};

fn noise(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut v = Vec::with_capacity(n + 8);
    while v.len() < n {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.truncate(n);
    v
}

fn read_tree(root: &std::path::Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(base: &std::path::Path, dir: &std::path::Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                let rel = p.strip_prefix(base).unwrap().to_string_lossy().into_owned();
                out.insert(rel, std::fs::read(&p).unwrap());
            }
        }
    }
    let mut m = BTreeMap::new();
    walk(root, root, &mut m);
    m
}

#[tokio::test(flavor = "multi_thread")]
async fn sevenz_upload_to_a_c_server_verifies_every_file() {
    let d = dir("sevenz-c");
    // 2 solid folders, 200 files (one above the large-file cutoff, so a chunk stream and
    // a root), 3 empty files, a few directories. ~5 MiB in all.
    let mut want: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut w = ArchiveWriter::create(d.join("a.7z")).unwrap();
    for k in 0..2 {
        let mut files: Vec<(String, Vec<u8>)> = (0..100)
            .map(|i| {
                (
                    format!("g{k}/s{}/f{i}", i % 5),
                    noise((k * 1000 + i) as u64, 24_000 + i),
                )
            })
            .collect();
        if k == 1 {
            files[7] = ("g1/big.bin".into(), noise(4242, 700_000));
        }
        let entries: Vec<ArchiveEntry> = files
            .iter()
            .map(|(n, _)| ArchiveEntry::new_file(n))
            .collect();
        let readers: Vec<SourceReader<&[u8]>> = files
            .iter()
            .map(|(_, d)| SourceReader::new(&d[..]))
            .collect();
        w.push_archive_entries(entries, readers).unwrap();
        want.extend(files);
    }
    w.push_archive_entry::<&[u8]>(ArchiveEntry::new_directory("emptydir/inner"), None)
        .unwrap();
    for e in ["zero0", "g0/zero1", "g1/s2/zero2"] {
        let mut en = ArchiveEntry::new_file(e);
        en.has_stream = false;
        w.push_archive_entry::<&[u8]>(en, None).unwrap();
        want.insert(e.into(), Vec::new());
    }
    w.finish().unwrap();
    assert!(want.len() >= 200);
    let total: u64 = want.values().map(|v| v.len() as u64).sum();

    let (m, src) = SevenzSource::open(&d.join("a.7z"), &[]).unwrap();
    let src = Arc::new(src);
    let peers = d.join("peers");
    let (me, mine) = paired_client(&peers);
    let mut srv = CServer::start_data(SECRET, &peers, &d.join("jobs"), 200, 2000, 2000, 0);
    // Throttled so the payload restart lands mid-upload.
    let px = Arc::new(
        ChaosProxy::start(
            srv.addr().parse().unwrap(),
            ChaosConfig {
                bytes_per_sec: Some(2 << 20),
                ..Default::default()
            },
        )
        .await
        .unwrap(),
    );
    let addr = px.addr.to_string();
    let progress = Arc::new(Progress::default());
    let pg = progress.clone();
    let root = d.join("dest");
    let root_s = root.to_str().unwrap().to_string();
    let src2 = src.clone();
    let up = tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(240);
        let mut sessions = 0u32;
        loop {
            assert!(tokio::time::Instant::now() < deadline, "no finished job");
            let Ok(s) = connect(&addr, me.clone(), mine.clone(), "rust", calm()).await else {
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            };
            sessions += 1;
            let mut link = s.job([11; 16]);
            let mut o = SendOptions::upload(&root_s);
            o.progress = pg.clone();
            o.seq = Some(src2.clone());
            match send_job(&mut link, Arc::new(m.clone()), Arc::new(NoSource), o).await {
                Ok(r) => return (r, sessions),
                Err(SendError::Disconnected(_)) => {
                    tokio::time::sleep(Duration::from_millis(200)).await
                }
                Err(e) => panic!("upload failed: {e}"),
            }
        }
    });
    // Restart the payload once about half the bytes are durable: the journal survives,
    // memory does not.
    let t0 = std::time::Instant::now();
    while progress.bytes_durable.load(Ordering::Relaxed) < total / 2 {
        assert!(t0.elapsed() < Duration::from_secs(60), "never got halfway");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let durable_at_kill = progress.bytes_durable.load(Ordering::Relaxed);
    srv.restart_data();
    let (r, sessions) = up.await.unwrap();
    assert_eq!(r.status, gen::STATUS_OK);
    assert!(sessions >= 2, "the restart cost a session");
    assert!(durable_at_kill < total, "the kill landed mid-upload");
    let got = read_tree(&root);
    assert_eq!(
        got.keys().collect::<Vec<_>>(),
        want.keys().collect::<Vec<_>>()
    );
    for (k, v) in &want {
        assert!(&got[k] == v, "{k}: bytes differ");
    }
    assert!(root.join("emptydir/inner").is_dir());
    assert!(
        src.folders_opened() >= 2,
        "both folders were decoded at least once"
    );
}
