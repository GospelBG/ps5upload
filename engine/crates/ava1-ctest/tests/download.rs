#![cfg(unix)]
//! Task 18: downloads, C sender (ava1_send.c) → Rust receiver (ava1::recv). The four
//! plan tests — a folder, a single ordered file, a resume from the engine's journal, a
//! refusal — plus the empty-folder test (ruling R3).
mod common;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use ava1::gen;
use ava1::recv::{download_job, LocalSink, RecvOptions};
use ava1::send::Progress;
use ava1::session::connect;
use ava1_ctest::CServer;
use common::*;

/// How long one download may take before the test fails it: a missing signal (a dead
/// job, a stalled sender) fails instead of hanging the round. Every real download here
/// finishes far below this.
const DL_DEADLINE: Duration = Duration::from_secs(300);

fn ro(jobs: &std::path::Path, ordered: bool) -> RecvOptions {
    RecvOptions {
        credit: 64 << 20,
        flags: 0, // `download_job` overwrites it with the flags argument
        jobs_dir: jobs.into(),
        ordered,
        progress: Arc::default(),
        cancel: Arc::default(),
    }
}

async fn download(
    link: &mut ava1::router::JobLink,
    src: &str,
    flags: u32,
    sink: Arc<LocalSink>,
    o: RecvOptions,
) -> Result<ava1::recv::RecvReport, ava1::send::SendError> {
    tokio::time::timeout(DL_DEADLINE, download_job(link, src, flags, sink, o))
        .await
        .expect("the download did not finish within DL_DEADLINE")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_folder_downloads_from_the_c_sender() {
    ava1_ctest::c_set_read_allowed(true);
    let d = dir("dl-folder");
    let src = d.join("console/game");
    write_tree(&src, 2000, |i| {
        if i % 400 == 0 {
            (6 << 20) + i
        } else {
            i % 3000
        }
    });
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
    let mut link = s.job([0x61; 16]);
    let sink = Arc::new(LocalSink::new(d.join("got"), false));
    let r = download(
        &mut link,
        src.to_str().unwrap(),
        0,
        sink,
        ro(&d.join("ejobs"), false),
    )
    .await
    .unwrap();
    assert_eq!(r.files, 2000);
    assert!(same_tree(&src, &d.join("got")));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_single_file_downloads_in_order() {
    ava1_ctest::c_set_read_allowed(true);
    let d = dir("dl-single");
    let f = d.join("console/a.pkg");
    std::fs::create_dir_all(f.parent().unwrap()).unwrap();
    std::fs::write(
        &f,
        (0..(40 << 20) + 9)
            .map(|i| (i * 5) as u8)
            .collect::<Vec<u8>>(),
    )
    .unwrap();
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
    let mut link = s.job([0x62; 16]);
    let sink = Arc::new(LocalSink::new(d.join("a.pkg"), true));
    download(
        &mut link,
        f.to_str().unwrap(),
        gen::JF_ORDERED,
        sink,
        ro(&d.join("ejobs"), true),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read(d.join("a.pkg")).unwrap(),
        std::fs::read(&f).unwrap()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_resumes_from_the_engine_journal() {
    ava1_ctest::c_set_read_allowed(true);
    let d = dir("dl-resume");
    let f = d.join("console/big.bin");
    std::fs::create_dir_all(f.parent().unwrap()).unwrap();
    std::fs::write(
        &f,
        (0..(128 << 20))
            .map(|i| (i * 13) as u8)
            .collect::<Vec<u8>>(),
    )
    .unwrap();
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
    let progress = Arc::new(Progress::default());
    {
        let s = connect(&srv.addr(), me.clone(), mine.clone(), "rust", calm())
            .await
            .unwrap();
        let mut link = s.job([0x63; 16]);
        let sink = Arc::new(LocalSink::new(d.join("big.bin"), true));
        let mut o = ro(&d.join("ejobs"), false);
        o.progress = progress.clone();
        let pg = progress.clone();
        let cancel = o.cancel.clone();
        let poll = tokio::spawn(async move {
            let t = std::time::Instant::now();
            while pg.bytes_durable.load(Ordering::Relaxed) < 48 << 20 {
                assert!(
                    t.elapsed() < DL_DEADLINE,
                    "the first download never made 48 MiB durable"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            cancel.store(true, Ordering::Relaxed);
        });
        assert!(download(&mut link, f.to_str().unwrap(), 0, sink, o)
            .await
            .is_err());
        poll.await.unwrap();
    }
    let s = connect(&srv.addr(), me, mine, "rust", calm())
        .await
        .unwrap();
    let mut link = s.job([0x63; 16]);
    let sink = Arc::new(LocalSink::new(d.join("big.bin"), true));
    let o2 = ro(&d.join("ejobs"), false);
    let pg2 = o2.progress.clone();
    download(&mut link, f.to_str().unwrap(), 0, sink, o2)
        .await
        .unwrap();
    assert!(pg2.bytes_durable.load(Ordering::Relaxed) >= 128 << 20);
    assert_eq!(
        std::fs::read(d.join("big.bin")).unwrap(),
        std::fs::read(&f).unwrap()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn reading_outside_the_allowed_roots_is_refused() {
    ava1_ctest::c_set_read_allowed(true);
    let d = dir("dl-refuse");
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
    ava1_ctest::c_set_read_allowed(false);
    let s = connect(&srv.addr(), me, mine, "rust", calm())
        .await
        .unwrap();
    let mut link = s.job([0x64; 16]);
    let sink = Arc::new(LocalSink::new(d.join("x"), false));
    let e = download(&mut link, "/etc", 0, sink, ro(&d.join("ejobs"), false))
        .await
        .unwrap_err();
    ava1_ctest::c_set_read_allowed(true);
    assert!(matches!(e, ava1::send::SendError::Refused { status, .. } if status == gen::ERR_PATH));
}

/// R3: an empty directory is a real tree shape — no manifest page, just ManifestEnd.
#[tokio::test(flavor = "multi_thread")]
async fn an_empty_folder_downloads() {
    ava1_ctest::c_set_read_allowed(true);
    let d = dir("dl-empty");
    let src = d.join("console/game");
    std::fs::create_dir_all(&src).unwrap();
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
    let mut link = s.job([0x65; 16]);
    let sink = Arc::new(LocalSink::new(d.join("got"), false));
    let r = download(
        &mut link,
        src.to_str().unwrap(),
        0,
        sink,
        ro(&d.join("ejobs"), false),
    )
    .await
    .unwrap();
    assert_eq!(r.files, 0);
    assert!(d.join("got").exists());
}
