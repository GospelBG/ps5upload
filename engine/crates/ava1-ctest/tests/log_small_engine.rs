#![cfg(unix)]
//! Durable-by-log on the engine's receiver (review 003 §3.2, SPEC.md §15.7): a download of many small
//! files from the C sender through `LocalSink` with the pack log on — the journal records pack batches and
//! sweeps, the job ends settled with no pack file left, and the per-file path (log off) still works.
mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use ava1::journal::{job_dir, Journal, Record, State};
use ava1::packlog::PackOpts;
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
    }
}

async fn run(
    tag: &str,
    id: u8,
    log: bool,
) -> (std::path::PathBuf, std::path::PathBuf, Vec<Record>) {
    ava1_ctest::c_set_read_allowed(true);
    let d = dir(tag);
    let src = d.join("console/game");
    write_tree(&src, 600, |i| 1 + i % 900);
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
    let mut link = s.job([id; 16]);
    let sink = Arc::new(LocalSink::new(d.join("got"), false).with_log(
        log,
        PackOpts {
            segment: 64 << 10, // segments roll several times
            max_unswept: 1 << 20,
            age: Duration::from_millis(20),
        },
    ));
    let t0 = Instant::now();
    let r = tokio::time::timeout(
        Duration::from_secs(120),
        download_job(
            &mut link,
            src.to_str().unwrap(),
            0,
            sink,
            ro(&d.join("ejobs")),
        ),
    )
    .await
    .expect("the download finished in time")
    .unwrap();
    assert_eq!(r.files, 600, "{:?}", t0.elapsed());
    assert!(same_tree(&src, &d.join("got")));
    let jd = job_dir(&d.join("ejobs"), &[id; 16]);
    let (_, recs) = Journal::open(&jd).unwrap();
    (d, jd, recs)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_through_the_pack_log_journals_batches_and_sweeps_and_ends_settled() {
    let (_d, jd, recs) = run("dl-logged", 0x6c, true).await;
    let packed = recs
        .iter()
        .filter(|r| matches!(r, Record::Batch(b) if b.pack_segment.is_some()))
        .count();
    let sweeps = recs
        .iter()
        .filter(|r| matches!(r, Record::Sweep(_)))
        .count();
    assert!(packed >= 1, "no batch carried the pack extension");
    assert!(sweeps >= 1, "no sweep was journaled");
    let mut st = State::default();
    for r in &recs {
        st.apply(r);
    }
    assert_eq!(st.done.len(), 600);
    assert!(st.unswept.is_empty() && st.packs.is_empty(), "{st:?}");
    assert_eq!(st.finished, Some(0));
    let left = std::fs::read_dir(&jd)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("pack."))
        .count();
    assert_eq!(left, 0, "pack files left behind");
}

#[tokio::test(flavor = "multi_thread")]
async fn with_the_log_off_the_per_file_path_is_unchanged() {
    let (_d, _jd, recs) = run("dl-unlogged", 0x6d, false).await;
    assert!(recs.iter().all(
        |r| !matches!(r, Record::Batch(b) if b.pack_segment.is_some())
            && !matches!(r, Record::Sweep(_))
    ));
}
