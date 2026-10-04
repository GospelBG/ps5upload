#![cfg(unix)]
//! P3 Task 8 review: the payload's exit stops the AVA1 half too. The reply to `node.shutdown` is on
//! the wire before anything stops, the sessions leave, an in-flight Sony call is waited for (bounded),
//! and a running durable job is closed so it resumes after the next start with nothing lost.
mod common;

use std::time::{Duration, Instant};

use ava1::gen::{self, JobCopy, JobRef, Status};
use ava1::session::{connect, Session};
use ava1::wire::Message;
use ava1_ctest::CServer;
use common::*;

const STOP_CONNS_LEFT: i32 = 1;
const STOP_SONY_BUSY: i32 = 2;

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

async fn start(s: &Session, job: [u8; 16], src: &std::path::Path, dest: &std::path::Path) -> u16 {
    let body = JobCopy {
        job_id: job,
        src: src.to_str().unwrap().into(),
        dest: dest.to_str().unwrap().into(),
        flags: 0,
    };
    s.rpc(gen::METHOD_JOB_COPY, &body.to_bytes().unwrap())
        .await
        .unwrap()
        .status
}

#[tokio::test(flavor = "multi_thread")]
async fn node_shutdown_reply_arrives_before_the_server_stops() {
    let d = dir("t8-shutdown-reply");
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
    let r = s
        .rpc(gen::METHOD_NODE_SHUTDOWN, &[])
        .await
        .expect("the reply is received");
    assert_eq!(r.status, gen::STATUS_OK);
    assert_eq!(r.body, b"{}");
    // ...and the stop does follow: the listener goes away.
    let addr = srv.addr();
    let t0 = Instant::now();
    loop {
        if std::net::TcpStream::connect_timeout(&addr.parse().unwrap(), Duration::from_millis(200))
            .is_err()
        {
            break;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(6),
            "the server never stopped"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_running_copy_resumes_after_the_payload_stop_sequence() {
    let d = dir("t8-stop-resume");
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
    let job = [0x75; 16];
    let s = connect(&srv.addr(), me.clone(), mine.clone(), "rust", calm())
        .await
        .unwrap();
    assert_eq!(start(&s, job, &d.join("usb/g"), &d.join("data/g")).await, 0);
    for _ in 0..1500 {
        if status(&s, job).await.files_done >= 500 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let before = status(&s, job).await.files_done;
    assert!(before >= 500, "no progress before the stop");
    // The session is still open: the sequence must end it, not wait for the client.
    let t0 = Instant::now();
    assert_eq!(
        srv.payload_stop(3000, 1000),
        0,
        "sessions left, no Sony call"
    );
    assert!(t0.elapsed() < Duration::from_secs(5));
    srv.start_data_again();
    let s2 = connect(&srv.addr(), me, mine, "rust", calm())
        .await
        .unwrap();
    assert_eq!(
        start(&s2, job, &d.join("usb/g"), &d.join("data/g")).await,
        0
    );
    assert!(
        status(&s2, job).await.files_done >= before.min(500),
        "the journal kept what was durable"
    );
    for _ in 0..1200 {
        if status(&s2, job).await.state.unwrap_or(0) != 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(status(&s2, job).await.state, Some(1));
    assert!(
        same_tree(&d.join("usb/g"), &d.join("data/g")),
        "nothing lost"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_stop_does_not_return_while_a_sony_call_is_running() {
    let d = dir("t8-stop-sony");
    let mut srv = CServer::start_data(
        SECRET,
        &d.join("peers"),
        &d.join("jobs"),
        200,
        2000,
        2000,
        0,
    );
    // A worker inside a Sony call for 4 s; the soft wait is only 500 ms.
    let t0 = Instant::now();
    let holder = ava1_ctest::sony_hold(4000);
    let rc = srv.payload_stop(500, 500);
    let took = t0.elapsed();
    holder.join().unwrap();
    assert!(
        took >= Duration::from_millis(3900),
        "returned while the Sony call still held the lock: {took:?}"
    );
    assert_eq!(
        rc & STOP_SONY_BUSY,
        STOP_SONY_BUSY,
        "the slow call is reported"
    );
    assert_eq!(rc & STOP_CONNS_LEFT, 0);
    // with the lock free the stop is prompt and clean
    srv.start_data_again();
    let t1 = Instant::now();
    assert_eq!(srv.payload_stop(500, 500), 0);
    assert!(t1.elapsed() < Duration::from_secs(2));
}
