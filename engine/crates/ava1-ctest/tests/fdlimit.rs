#![cfg(unix)]
//! The open-file budget: tiny-file uploads and disk.calibrate stay within it, and a failure
//! tells the engine which step failed.
mod common;

use ava1::gen;
use ava1::session::connect;
use ava1_ctest::CServer;
use common::*;

#[tokio::test(flavor = "multi_thread")]
async fn two_thousand_tiny_files_never_hold_more_than_the_budget() {
    let d = dir("fd-upload");
    let src = d.join("src");
    write_tree(&src, 2000, |i| 1 + i % 200);
    let peers = d.join("peers");
    let (me, mine) = paired_client(&peers);
    let srv = CServer::start_data(SECRET, &peers, &d.join("jobs"), 200, 4000, 4000, 0);
    srv.knob("fd_budget", 64);
    srv.knob("fd_peak_reset", 0);
    let root = d.join("dest");
    let (r, _) = upload(
        &srv.addr(),
        me,
        mine,
        &src,
        root.to_str().unwrap(),
        [7; 16],
        |_| {},
    )
    .await;
    assert_eq!(r.status, 0);
    assert!(same_tree(&src, &root));
    let peak = srv.fd_peak(0);
    assert!(peak > 0 && peak <= 32, "pending fds peaked at {peak}");
}

#[tokio::test(flavor = "multi_thread")]
async fn calibrate_with_many_files_fits_a_small_budget() {
    let d = dir("fd-cal");
    let peers = d.join("peers");
    let (me, mine) = paired_client(&peers);
    let srv = CServer::start_data(SECRET, &peers, &d.join("jobs"), 200, 4000, 4000, 0);
    srv.knob("fd_budget", 64);
    srv.knob("fd_peak_reset", 0);
    let s = connect(&srv.addr(), me, mine, "rust", calm())
        .await
        .unwrap();
    let cal = d.join("cal");
    std::fs::create_dir_all(&cal).unwrap();
    let pts = s
        .calibrate(cal.to_str().unwrap(), 2000, 4096)
        .await
        .unwrap();
    assert_eq!(pts.len(), 5);
    assert!(pts.iter().all(|p| p.files_per_s > 0), "{pts:?}");
    let peak = srv.fd_peak(1);
    assert!(peak > 0 && peak <= 64, "calibrate held {peak} fds");
    assert_eq!(std::fs::read_dir(&cal).unwrap().count(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_calibrate_names_the_step() {
    let d = dir("fd-cal-err");
    let peers = d.join("peers");
    let (me, mine) = paired_client(&peers);
    let srv = CServer::start_data(SECRET, &peers, &d.join("jobs"), 200, 4000, 4000, 0);
    let s = connect(&srv.addr(), me, mine, "rust", calm())
        .await
        .unwrap();
    let cal = d.join("cal");
    std::fs::create_dir_all(cal.join(".ava-cal-1")).unwrap();
    let err = s
        .calibrate(cal.to_str().unwrap(), 8, 4096)
        .await
        .unwrap_err();
    match err {
        ava1::Ava1Error::Refused { code, message } => {
            assert_eq!(code, gen::ERR_EXISTS);
            assert!(
                message.contains("disk.calibrate") && message.contains("mkdir"),
                "{message}"
            );
            assert!(message.contains("exist"), "{message}");
        }
        e => panic!("{e:?}"),
    }
}
