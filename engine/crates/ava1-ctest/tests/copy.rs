#![cfg(unix)]
mod common;

use std::time::Duration;

use ava1::gen::{self, JobCopy, JobRef, Status};
use ava1::session::{connect, Session};
use ava1::wire::Message;
use ava1_ctest::CServer;
use common::*;

// The C server is a process-wide singleton: run this binary with `--test-threads=1`.
// Every `start_data` blocks on the same lock until the previous test's server drops.

async fn status(s: &Session, job: [u8; 16]) -> Status {
    let r = s
        .rpc(
            gen::METHOD_JOB_STATUS,
            &JobRef { job_id: job }.to_bytes().unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status, gen::STATUS_OK);
    Status::decode(&r.body).unwrap()
}

async fn wait_finished(s: &Session, job: [u8; 16]) -> Status {
    for _ in 0..1200 {
        let st = status(s, job).await;
        if st.state.unwrap_or(0) != 0 {
            return st;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("copy did not finish");
}

async fn start(
    s: &Session,
    job: [u8; 16],
    src: &std::path::Path,
    dest: &std::path::Path,
    flags: u32,
) -> u16 {
    let body = JobCopy {
        job_id: job,
        src: src.to_str().unwrap().into(),
        dest: dest.to_str().unwrap().into(),
        flags,
    };
    s.rpc(gen::METHOD_JOB_COPY, &body.to_bytes().unwrap())
        .await
        .unwrap()
        .status
}

/// A move's delete follows the journaled Done by a moment (the journal fsync sits between
/// the terminal state and the unlinks): poll, bound so a missing delete fails the test
/// instead of hanging it.
async fn wait_gone(p: &std::path::Path) {
    for _ in 0..200 {
        if !p.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("{} was never deleted", p.display());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_folder_copies_on_the_console() {
    let d = dir("copy");
    write_tree(&d.join("usb/game"), 1500, |i| {
        if i % 300 == 0 {
            (3 << 20) + i
        } else {
            i % 2000
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
    let job = [0x71; 16];
    assert_eq!(
        start(&s, job, &d.join("usb/game"), &d.join("data/game"), 0).await,
        gen::STATUS_OK
    );
    let st = wait_finished(&s, job).await;
    assert_eq!(st.state, Some(1), "{:?}", st.current);
    assert_eq!(st.files_done, 1500);
    assert_eq!(st.files_total, 1500);
    assert!(same_tree(&d.join("usb/game"), &d.join("data/game")));
    // A finished copy stays listed so status keeps answering (10-minute window).
    assert_eq!(status(&s, job).await.state, Some(1));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_move_deletes_the_source_only_after_a_verified_copy() {
    let d = dir("move");
    write_tree(&d.join("usb/g"), 300, |i| i * 11);
    let keep = d.join("keep");
    copy_dir(&d.join("usb/g"), &keep);
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
    let job = [0x72; 16];
    assert_eq!(
        start(&s, job, &d.join("usb/g"), &d.join("data/g"), gen::JF_MOVE).await,
        0
    );
    assert_eq!(wait_finished(&s, job).await.state, Some(1));
    wait_gone(&d.join("usb/g")).await;
    assert!(same_tree(&keep, &d.join("data/g")));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_single_file_copies_and_moves() {
    let d = dir("copy-file");
    std::fs::create_dir_all(d.join("usb")).unwrap();
    std::fs::write(d.join("usb/one.bin"), vec![7u8; 700_000]).unwrap();
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
    let job = [0x75; 16];
    assert_eq!(
        start(
            &s,
            job,
            &d.join("usb/one.bin"),
            &d.join("data/one.bin"),
            gen::JF_MOVE
        )
        .await,
        0
    );
    let st = wait_finished(&s, job).await;
    assert_eq!(st.state, Some(1), "{:?}", st.current);
    assert_eq!(st.files_done, 1);
    wait_gone(&d.join("usb/one.bin")).await;
    assert_eq!(
        std::fs::read(d.join("data/one.bin")).unwrap(),
        vec![7u8; 700_000]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_folder_copies_on_the_console() {
    let d = dir("copy-empty");
    std::fs::create_dir_all(d.join("usb/empty")).unwrap();
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
    let job = [0x76; 16];
    assert_eq!(
        start(&s, job, &d.join("usb/empty"), &d.join("data/empty"), 0).await,
        0
    );
    let st = wait_finished(&s, job).await;
    assert_eq!(st.state, Some(1), "{:?}", st.current);
    assert_eq!(st.files_total, 0);
    assert!(d.join("data/empty").is_dir());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_copy_into_its_own_source_is_refused() {
    let d = dir("copy-self");
    std::fs::create_dir_all(d.join("a")).unwrap();
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
    assert_eq!(
        start(&s, [0x73; 16], &d.join("a"), &d.join("a/b"), 0).await,
        gen::ERR_PATH
    );
    assert_eq!(
        start(&s, [0x73; 16], &d.join("a"), &d.join("a"), 0).await,
        gen::ERR_PATH
    );
    // The other direction too: the written namespace would reach into the read tree.
    std::fs::create_dir_all(d.join("a/b")).unwrap();
    assert_eq!(
        start(&s, [0x7a; 16], &d.join("a/b"), &d.join("a"), 0).await,
        gen::ERR_PATH
    );
    // A destination that simply shares a prefix is not "inside": /ab is not under /a. The
    // flag is JF_OVERWRITE because the folder `ab` exists (C14 refuses it otherwise).
    std::fs::create_dir_all(d.join("ab")).unwrap();
    assert_eq!(
        start(
            &s,
            [0x7b; 16],
            &d.join("a/b"),
            &d.join("ab"),
            gen::JF_OVERWRITE
        )
        .await,
        0
    );
    assert_eq!(wait_finished(&s, [0x7b; 16]).await.state, Some(1));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_copy_to_the_same_destination_is_refused() {
    let d = dir("copy-busy");
    write_tree(&d.join("usb/g"), 3000, |_| 2048);
    // 3 ms per fsync keeps the first copy running while the second one asks.
    let (me, mine) = paired_client(&d.join("peers"));
    let srv = CServer::start_data(
        SECRET,
        &d.join("peers"),
        &d.join("jobs"),
        200,
        2000,
        2000,
        3000,
    );
    let s = connect(&srv.addr(), me, mine, "rust", calm())
        .await
        .unwrap();
    let a = [0x77; 16];
    assert_eq!(
        start(&s, a, &d.join("usb/g"), &d.join("data/g"), 0).await,
        0
    );
    assert!(
        status(&s, a).await.files_done < 3000,
        "the first copy is still running"
    );
    assert_eq!(
        start(&s, [0x78; 16], &d.join("usb/g"), &d.join("data/g"), 0).await,
        gen::ERR_BUSY
    );
    // The same job id is not a second writer: it resumes.
    assert_eq!(
        start(&s, a, &d.join("usb/g"), &d.join("data/g"), 0).await,
        0
    );
    let c = s
        .rpc(
            gen::METHOD_JOB_CANCEL,
            &JobRef { job_id: a }.to_bytes().unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(c.status, gen::STATUS_OK);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dangling_symlink_fails_the_copy() {
    let d = dir("copy-link");
    write_tree(&d.join("usb/g"), 4, |_| 64);
    std::os::unix::fs::symlink(d.join("usb/gone"), d.join("usb/g/d00/link")).unwrap();
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
    // Rust's walk fails the whole tree on a dangling symlink (source.rs): the C walk must
    // not silently drop the entry and copy a tree that is missing a file.
    assert_eq!(
        start(&s, [0x79; 16], &d.join("usb/g"), &d.join("data/g"), 0).await,
        gen::ERR_IO
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_copy_resumes_after_a_payload_restart() {
    let d = dir("copy-resume");
    write_tree(&d.join("usb/g"), 3000, |_| 2048);
    let (me, mine) = paired_client(&d.join("peers"));
    let mut srv = CServer::start_data(
        SECRET,
        &d.join("peers"),
        &d.join("jobs"),
        200,
        2000,
        2000,
        3000,
    );
    let job = [0x74; 16];
    {
        let s = connect(&srv.addr(), me.clone(), mine.clone(), "rust", calm())
            .await
            .unwrap();
        assert_eq!(
            start(&s, job, &d.join("usb/g"), &d.join("data/g"), 0).await,
            0
        );
        for _ in 0..1500 {
            // 30 s bound: the first copy must reach 500 durable files.
            if status(&s, job).await.files_done >= 500 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            status(&s, job).await.files_done >= 500,
            "the first copy made no progress"
        );
    }
    srv.restart_data();
    let s = connect(&srv.addr(), me, mine, "rust", calm())
        .await
        .unwrap();
    assert_eq!(
        start(&s, job, &d.join("usb/g"), &d.join("data/g"), 0).await,
        0
    );
    let first = status(&s, job).await;
    assert!(
        first.files_done >= 500,
        "the journal's progress survived the restart"
    );
    let st = wait_finished(&s, job).await;
    assert_eq!(st.state, Some(1), "{:?}", st.current);
    assert!(same_tree(&d.join("usb/g"), &d.join("data/g")));
}

/* ---- C14: JF_OVERWRITE ------------------------------------------------------------- */

#[tokio::test(flavor = "multi_thread")]
async fn an_existing_destination_is_refused_without_overwrite() {
    let d = dir("copy-exists");
    write_tree(&d.join("usb/g"), 12, |i| 100 + i);
    write_tree(&d.join("data/g"), 5, |i| 300 + i);
    let keep = d.join("keep");
    copy_dir(&d.join("data/g"), &keep);
    std::fs::create_dir_all(d.join("usb")).unwrap();
    std::fs::write(d.join("usb/one.bin"), b"source bytes").unwrap();
    std::fs::create_dir_all(d.join("data")).unwrap();
    std::fs::write(d.join("data/one.bin"), b"already here").unwrap();
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
    // A tree destination that already holds files: refused, and left byte-for-byte alone.
    assert_eq!(
        start(&s, [0x81; 16], &d.join("usb/g"), &d.join("data/g"), 0).await,
        gen::ERR_EXISTS
    );
    assert!(
        same_tree(&keep, &d.join("data/g")),
        "the destination was not left untouched"
    );
    // A single file whose destination exists: the same refusal.
    assert_eq!(
        start(
            &s,
            [0x82; 16],
            &d.join("usb/one.bin"),
            &d.join("data/one.bin"),
            0
        )
        .await,
        gen::ERR_EXISTS
    );
    assert_eq!(
        std::fs::read(d.join("data/one.bin")).unwrap(),
        b"already here"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn overwrite_replaces_colliding_files_and_keeps_destination_only_files() {
    let d = dir("copy-overwrite");
    write_tree(&d.join("usb/g"), 8, |i| 1000 + i * 3);
    // The same layout at the destination: every file collides (different bytes), plus one
    // destination-only file that must survive.
    for i in 0..8 {
        let p = d.join("data/g").join(format!("d{:02}/f{i:05}", i % 37));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        if i == 3 {
            std::fs::write(&p, vec![0xEEu8; 200]).unwrap();
        } else {
            std::fs::write(&p, vec![0x11u8; 40]).unwrap();
        }
    }
    let extra = d.join("data/g/d00/extra");
    std::fs::write(&extra, b"dest only").unwrap();
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
    let job = [0x83; 16];
    assert_eq!(
        start(
            &s,
            job,
            &d.join("usb/g"),
            &d.join("data/g"),
            gen::JF_OVERWRITE
        )
        .await,
        0
    );
    let st = wait_finished(&s, job).await;
    assert_eq!(st.state, Some(1), "{:?}", st.current);
    assert_eq!(st.files_done, 8);
    for i in 0..8 {
        let rel = format!("d{:02}/f{i:05}", i % 37);
        assert_eq!(
            std::fs::read(d.join("data/g").join(&rel)).unwrap(),
            std::fs::read(d.join("usb/g").join(&rel)).unwrap(),
            "{rel} was not replaced with the source bytes"
        );
    }
    assert_eq!(
        std::fs::read(&extra).unwrap(),
        b"dest only",
        "a destination-only file must survive"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_single_file_with_overwrite_replaces_the_file() {
    let d = dir("copy-file-overwrite");
    std::fs::create_dir_all(d.join("usb")).unwrap();
    std::fs::write(d.join("usb/one.bin"), vec![7u8; 700_000]).unwrap();
    std::fs::create_dir_all(d.join("data")).unwrap();
    std::fs::write(d.join("data/one.bin"), vec![9u8; 123]).unwrap();
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
    let job = [0x84; 16];
    assert_eq!(
        start(
            &s,
            job,
            &d.join("usb/one.bin"),
            &d.join("data/one.bin"),
            gen::JF_SINGLE_FILE | gen::JF_OVERWRITE
        )
        .await,
        0
    );
    let st = wait_finished(&s, job).await;
    assert_eq!(st.state, Some(1), "{:?}", st.current);
    assert_eq!(st.files_done, 1);
    assert_eq!(
        std::fs::read(d.join("data/one.bin")).unwrap(),
        vec![7u8; 700_000]
    );
    assert!(d.join("usb/one.bin").exists(), "an overwrite is not a move");
}
