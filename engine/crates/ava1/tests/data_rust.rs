//! The Rust receiver end to end: a Rust sender against a Rust folder host, ordered
//! downloads, credit, resume from the engine's journal, and the host's path guard.
//! `OrderCheckSink` lives here, not in tests/common (the controller's file split for
//! this round: Task 16 owns the ava1-ctest tests/common).
mod common;

use std::sync::Arc;
use std::time::Duration;

use ava1::conn::Frame;
use ava1::gen::{
    self, Bundle, BundleRecord, Chunk, Credit, Durable, JobDone, JobMap, JobOpen, JobOpenAck,
    ManifestEnd, Received,
};
use ava1::host::FolderHost;
use ava1::journal::{self, Record};
use ava1::manifest::{self, Entry, Manifest};
use ava1::recv::{download_job, receive_job, LocalSink, RecvOptions, Sink};
use ava1::router::{Inbound, JobHost, JobLink};
use ava1::send::{send_job, SendOptions};
use ava1::session::connect;
use ava1::source::LocalSource;
use ava1::wire::{FrameMessage, Message};

/// Records every (file, offset) a download writes, checks the sequence is sorted, and
/// stores the bytes so the receiver's commit-time verification (which reads the groups
/// back) sees what was written — `read_at` returning zeros would fail the root check
/// for every one-group file and storm FileRetry.
#[derive(Default)]
struct OrderCheckSink {
    seen: std::sync::Mutex<Vec<(u32, u64)>>,
    bytes: std::sync::Mutex<std::collections::HashMap<u32, Vec<u8>>>,
}

impl OrderCheckSink {
    fn in_order(&self) -> bool {
        self.seen.lock().unwrap().windows(2).all(|w| w[0] < w[1])
    }
}

impl Sink for OrderCheckSink {
    fn prepare(&self, _m: &Manifest) -> std::io::Result<()> {
        Ok(())
    }
    fn write_at(&self, id: u32, off: u64, d: &[u8]) -> std::io::Result<()> {
        self.seen.lock().unwrap().push((id, off));
        let mut m = self.bytes.lock().unwrap();
        let b = m.entry(id).or_default();
        let end = off as usize + d.len();
        if b.len() < end {
            b.resize(end, 0);
        }
        b[off as usize..end].copy_from_slice(d);
        Ok(())
    }
    fn write_whole(&self, id: u32, d: &[u8]) -> std::io::Result<()> {
        self.seen.lock().unwrap().push((id, 0));
        self.bytes.lock().unwrap().insert(id, d.to_vec());
        Ok(())
    }
    fn sync(&self, _ids: &[u32]) -> std::io::Result<()> {
        Ok(())
    }
    fn read_at(&self, id: u32, off: u64, b: &mut [u8]) -> std::io::Result<usize> {
        let m = self.bytes.lock().unwrap();
        let Some(f) = m.get(&id) else { return Ok(0) };
        let start = off as usize;
        if start >= f.len() {
            return Ok(0);
        }
        let n = b.len().min(f.len() - start);
        b[..n].copy_from_slice(&f[start..start + n]);
        Ok(n)
    }
    fn commit(&self, _id: u32) -> std::io::Result<()> {
        Ok(())
    }
    fn finish(&self) -> std::io::Result<()> {
        Ok(())
    }
}

fn tree(d: &std::path::Path) {
    for i in 0..500 {
        let p = d.join(format!("a{}/f{i}", i % 9));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let n = if i % 100 == 0 { (3 << 20) + i } else { i * 7 };
        std::fs::write(p, (0..n).map(|k| (k + i) as u8).collect::<Vec<_>>()).unwrap();
    }
}

fn same(a: &std::path::Path, b: &std::path::Path) {
    let ma = manifest::walk(&LocalSource::new(a.into()), &|_: &str| false).unwrap();
    let mb = manifest::walk(&LocalSource::new(b.into()), &|_: &str| false).unwrap();
    assert_eq!(ma.entries.len(), mb.entries.len());
    for e in &ma.entries {
        if e.kind == gen::ENTRY_FILE {
            assert_eq!(
                std::fs::read(a.join(&e.path)).unwrap(),
                std::fs::read(b.join(&e.path)).unwrap(),
                "{}",
                e.path
            );
        }
    }
}

fn opts(jobs: &std::path::Path, ordered: bool) -> RecvOptions {
    RecvOptions {
        credit: 64 << 20,
        flags: if ordered { gen::JF_ORDERED } else { 0 },
        jobs_dir: jobs.into(),
        ordered,
        progress: Arc::default(),
        cancel: Arc::default(),
    }
}

/// The next control frame on the job's inbox. Every async wait in this file is bounded
/// (a missing signal must fail the test, not hang the round).
async fn next_control(link: &mut JobLink) -> Frame {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(20), link.rx.recv())
            .await
            .expect("no frame within 20 s")
            .expect("the job channel closed");
        if let Inbound::Control(f) = ev {
            return f;
        }
    }
}

/// The opener's upload handshake, hand-driven: JobOpen → ack → manifest pages → end →
/// map pages. The map accumulates into the returned `Need`-shaped pages.
async fn open_and_map(
    link: &mut JobLink,
    job: [u8; 16],
    root: &str,
    m: &Manifest,
) -> (JobOpenAck, ava1::ranges::Need) {
    link.control
        .send(&JobOpen {
            job_id: job,
            kind: gen::JOB_UPLOAD,
            policy: 0,
            flags: 0,
            root: root.into(),
            src: None,
            credit: None,
        })
        .await
        .unwrap();
    let ack: JobOpenAck = next_control(link).await.decode().unwrap();
    assert_eq!(ack.status, 0);
    for p in m.pages(job) {
        link.control.send(&p).await.unwrap();
    }
    link.control
        .send(&ManifestEnd {
            job_id: job,
            files: m.files(),
            bytes: m.bytes(),
            manifest_hash: m.hash(),
        })
        .await
        .unwrap();
    let mut need = ava1::ranges::Need::default();
    loop {
        let f = next_control(link).await;
        if f.ty != JobMap::TYPE {
            continue;
        }
        let p: JobMap = f.decode().unwrap();
        assert_eq!(p.status, 0);
        need.add_page(&p);
        if p.last == 1 {
            return (ack, need);
        }
    }
}

/// A host with a chosen credit, for the hand-driven receiver tests (the real host grants
/// 64 MiB; the credit tests need a small grant).
struct CreditHost {
    root: std::path::PathBuf,
    jobs: std::path::PathBuf,
    credit: u64,
}

impl JobHost for CreditHost {
    fn accept(&self, mut link: JobLink, first: Frame, _peer: [u8; 32]) {
        let (root, jobs, credit) = (self.root.clone(), self.jobs.clone(), self.credit);
        tokio::spawn(async move {
            let Ok(open) = first.decode::<JobOpen>() else {
                return;
            };
            let sink = Arc::new(LocalSink::new(
                root.join(&open.root),
                open.flags & gen::JF_SINGLE_FILE != 0,
            ));
            let o = RecvOptions {
                credit,
                flags: open.flags,
                jobs_dir: jobs,
                ordered: open.flags & gen::JF_ORDERED != 0,
                progress: Arc::default(),
                cancel: Arc::default(),
            };
            let _ = receive_job(&mut link, open, sink, o).await;
        });
    }
}

/// A sealed Bundle frame for one small file, with the root the receiver's check expects.
fn bundle_body(job: [u8; 16], file_id: u32, data: &[u8]) -> Vec<u8> {
    Bundle {
        job_id: job,
        records: vec![BundleRecord {
            file_id,
            root: *blake3::hash(data).as_bytes(),
            data: data.to_vec(),
        }],
    }
    .to_bytes()
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn rust_to_rust_upload_into_a_folder_host() {
    let d = common::temp_dir("rr-up");
    tree(&d.join("src"));
    let host = Arc::new(FolderHost {
        root: d.join("share"),
        jobs_dir: d.join("hjobs"),
    });
    let (addr, _ctx, id, peers) = common::paired_ctx(|c| c.with_jobs(host)).await;
    let s = connect(&addr.to_string(), id, peers, "client", common::fast())
        .await
        .unwrap();
    let src = LocalSource::new(d.join("src"));
    let m = Arc::new(manifest::walk(&src, &|_: &str| false).unwrap());
    let mut link = s.job([1; 16]);
    let r = send_job(&mut link, m, Arc::new(src), SendOptions::upload("in"))
        .await
        .unwrap();
    assert_eq!(r.status, 0);
    same(&d.join("src"), &d.join("share/in"));
}

#[tokio::test(flavor = "multi_thread")]
async fn rust_to_rust_download_from_a_folder_host() {
    let d = common::temp_dir("rr-down");
    tree(&d.join("share/out"));
    let host = Arc::new(FolderHost {
        root: d.join("share"),
        jobs_dir: d.join("hjobs"),
    });
    let (addr, _ctx, id, peers) = common::paired_ctx(|c| c.with_jobs(host)).await;
    let s = connect(&addr.to_string(), id, peers, "client", common::fast())
        .await
        .unwrap();
    let mut link = s.job([2; 16]);
    let sink = Arc::new(LocalSink::new(d.join("got"), false));
    let r = download_job(&mut link, "out", 0, sink, opts(&d.join("jobs"), false))
        .await
        .unwrap();
    assert_eq!(r.files, 500);
    same(&d.join("share/out"), &d.join("got"));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ordered_download_reaches_the_sink_in_file_order() {
    let d = common::temp_dir("rr-ordered");
    tree(&d.join("share/out"));
    let host = Arc::new(FolderHost {
        root: d.join("share"),
        jobs_dir: d.join("hjobs"),
    });
    let (addr, _ctx, id, peers) = common::paired_ctx(|c| c.with_jobs(host)).await;
    let s = connect(&addr.to_string(), id, peers, "client", common::fast())
        .await
        .unwrap();
    let mut link = s.job([3; 16]);
    let sink = Arc::new(OrderCheckSink::default());
    download_job(
        &mut link,
        "out",
        gen::JF_ORDERED,
        sink.clone(),
        opts(&d.join("jobs"), true),
    )
    .await
    .unwrap();
    assert!(
        sink.in_order(),
        "writes arrived out of (file, offset) order"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_refuses_paths_outside_its_share() {
    let d = common::temp_dir("rr-escape");
    let host = Arc::new(FolderHost {
        root: d.join("share"),
        jobs_dir: d.join("hjobs"),
    });
    let (addr, _ctx, id, peers) = common::paired_ctx(|c| c.with_jobs(host)).await;
    let s = connect(&addr.to_string(), id, peers, "client", common::fast())
        .await
        .unwrap();
    let src = LocalSource::new(d.clone());
    let m = Arc::new(Manifest::default());
    let mut link = s.job([4; 16]);
    let e = send_job(&mut link, m, Arc::new(src), SendOptions::upload("../x"))
        .await
        .unwrap_err();
    assert!(matches!(e, ava1::send::SendError::Refused { status, .. } if status == gen::ERR_PATH));
}

/// The §11.2 guard on the download direction: a peer-chosen root is refused before it
/// can reach `LocalSource` (ruling 14).
#[tokio::test(flavor = "multi_thread")]
async fn a_download_of_a_path_outside_the_share_is_refused_too() {
    let d = common::temp_dir("rr-descape");
    let host = Arc::new(FolderHost {
        root: d.join("share"),
        jobs_dir: d.join("hjobs"),
    });
    let (addr, _ctx, id, peers) = common::paired_ctx(|c| c.with_jobs(host)).await;
    let s = connect(&addr.to_string(), id, peers, "client", common::fast())
        .await
        .unwrap();
    let job = [0x33; 16];
    let mut link = s.job(job);
    link.control
        .send(&JobOpen {
            job_id: job,
            kind: gen::JOB_DOWNLOAD,
            policy: 0,
            flags: 0,
            root: "../x".into(),
            src: None,
            credit: Some(1 << 20),
        })
        .await
        .unwrap();
    let ack: JobOpenAck = next_control(&mut link).await.decode().unwrap();
    assert_eq!(
        ack.status,
        gen::ERR_PATH,
        "the host refused before building a source"
    );
}

/// SPEC.md §12.4 (ruling Q2): a frame larger than the credit still outstanding is refused
/// with the sealed Error(ERR_CREDIT) — on the offending lane, then on the control
/// connection, where the peer's link reader ends the whole session (a transport without a
/// server-side lane close ends the session instead of one lane; the sender observes the
/// same either way: its lane dies). Nothing of the frame is buffered or acknowledged.
#[tokio::test(flavor = "multi_thread")]
async fn an_over_credit_chunk_gets_the_sealed_error_and_the_session_ends() {
    let d = common::temp_dir("rr-credit");
    let host = Arc::new(CreditHost {
        root: d.join("share"),
        jobs: d.join("hjobs"),
        credit: 1 << 20,
    });
    let (addr, _ctx, id, peers) = common::paired_ctx(|c| c.with_jobs(host)).await;
    let s = connect(&addr.to_string(), id, peers, "client", common::fast())
        .await
        .unwrap();
    let job = [0x22; 16];
    let m = Manifest {
        entries: vec![Entry {
            kind: gen::ENTRY_FILE,
            mode: 0o644,
            size: 8 << 20,
            mtime: 0,
            path: "big".into(),
            root: None,
        }],
    };
    let mut link = s.job(job);
    let (ack, _map) = open_and_map(&mut link, job, "in", &m).await;
    assert_eq!(ack.credit, 1 << 20, "the grant is the job's credit");
    // The lane that carries the oversized chunk, opened here so the test can watch it die.
    let lane_conn = s.open_lane().await.unwrap();
    let lane = lane_conn.id;
    let body = Chunk {
        job_id: job,
        file_id: 0,
        offset: 0,
        data: vec![0x5a; 4 << 20],
    }
    .to_bytes()
    .unwrap();
    link.lane(lane)
        .unwrap()
        .tx
        .send_raw(Chunk::TYPE, 0, 1, body)
        .await
        .unwrap();
    // The receiver admits nothing: the frame is never acknowledged, so not even Received
    // crosses for it — asserted here, not just claimed. The session end (ruling Q2) rides
    // the control connection and the lane's death its own connection, so either may come
    // first; and when the session ends first the router's lane map is cleared with it, so
    // the inbox may never see a LaneDown at all — the lane's death is pinned below by the
    // lane connection's own close reason instead.
    let mut saw_received = false;
    let mut saw_closed = false;
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let ev = link.rx.recv().await.expect("the job channel closed");
            match ev {
                Inbound::Closed(_) => {
                    saw_closed = true;
                    break;
                }
                Inbound::Control(f) if f.ty == Received::TYPE => saw_received = true,
                _ => {}
            }
        }
    })
    .await
    .expect("the session end within 20 s");
    assert!(saw_closed, "the job heard the session end");
    assert!(
        !saw_received,
        "no Received crossed for a frame the receiver never admitted"
    );
    // The session ends with ERR_CREDIT: the control connection carried the sealed error
    // and the peer's link reader ended the session with it.
    let why = tokio::time::timeout(Duration::from_secs(20), s.closed())
        .await
        .expect("the session ended within 20 s");
    assert!(
        why.contains("17"),
        "the session ended with the ERR_CREDIT reason: {why}"
    );
    // The sender observes the sealed error: the lane connection's close reason is the
    // decoded Error — code 17 is ERR_CREDIT (SPEC.md §12.4, ruling Q2).
    let why = tokio::time::timeout(Duration::from_secs(20), lane_conn.closed())
        .await
        .expect("the lane closed within 20 s");
    assert!(
        why.contains("error 17"),
        "the sealed ERR_CREDIT crossed: {why}"
    );
}

/// SPEC.md §12.4, the grant direction (ledger row 19): the sender's window is the grant
/// plus every Credit frame sent back, so the receiver must admit frames up to that
/// running total — never only up to the grant minus the credit already returned. With
/// the wrong (subtractive) check the first frame after the receiver has returned credit
/// trips ERR_CREDIT: a 16 MiB upload against a 4 MiB grant dies on the second or third
/// piece, while the correct check mirrors the sender's window exactly and the whole
/// upload completes through several credit returns.
#[tokio::test(flavor = "multi_thread")]
async fn an_upload_larger_than_its_grant_completes_through_credit_returns() {
    let d = common::temp_dir("rr-grant");
    std::fs::create_dir_all(d.join("src")).unwrap();
    std::fs::write(d.join("src/big.bin"), vec![0x3c; 16 << 20]).unwrap();
    let host = Arc::new(CreditHost {
        root: d.join("share"),
        jobs: d.join("hjobs"),
        credit: 4 << 20,
    });
    let (addr, _ctx, id, peers) = common::paired_ctx(|c| c.with_jobs(host)).await;
    let s = connect(&addr.to_string(), id, peers, "client", common::fast())
        .await
        .unwrap();
    let src = LocalSource::new(d.join("src"));
    let m = Arc::new(manifest::walk(&src, &|_: &str| false).unwrap());
    let mut link = s.job([0x5a; 16]);
    let r = tokio::time::timeout(
        Duration::from_secs(30),
        send_job(&mut link, m, Arc::new(src), SendOptions::upload("in")),
    )
    .await
    .expect("the upload finished within 30 s")
    .unwrap();
    assert_eq!(r.status, 0);
    same(&d.join("src"), &d.join("share/in"));
}

/// An empty file is a real file: the download must complete with the empty file present.
/// The receiver completes it on its own — the sender may send an empty bundle record for
/// it, or nothing at all (and the sender's own bundle flush can drop the record). The
/// wait is bounded: a missing completion must fail the test, not hang it.
#[tokio::test(flavor = "multi_thread")]
async fn a_zero_byte_file_completes_in_an_ordinary_download() {
    let d = common::temp_dir("rr-zero");
    std::fs::create_dir_all(d.join("share/out")).unwrap();
    std::fs::write(d.join("share/out/empty.bin"), b"").unwrap();
    std::fs::write(d.join("share/out/real.bin"), vec![0xa7; 1 << 20]).unwrap();
    let host = Arc::new(FolderHost {
        root: d.join("share"),
        jobs_dir: d.join("hjobs"),
    });
    let (addr, _ctx, id, peers) = common::paired_ctx(|c| c.with_jobs(host)).await;
    let s = connect(&addr.to_string(), id, peers, "client", common::fast())
        .await
        .unwrap();
    let mut link = s.job([9; 16]);
    let sink = Arc::new(LocalSink::new(d.join("got"), false));
    let r = tokio::time::timeout(
        Duration::from_secs(30),
        download_job(&mut link, "out", 0, sink, opts(&d.join("jobs"), false)),
    )
    .await
    .expect("the download completed within 30 s")
    .unwrap();
    assert_eq!(r.files, 2);
    same(&d.join("share/out"), &d.join("got"));
}

/// An empty file ahead of a real file in an ordered download: the ordered sender sends
/// every file as chunks and a zero range has none, so no frame can ever arrive for the
/// empty file — the receiver's cursor waited at it forever and the real file behind it
/// never applied. The wait is bounded: a stall must fail the test, not hang it.
#[tokio::test(flavor = "multi_thread")]
async fn an_empty_file_never_stalls_the_ordered_files_behind_it() {
    let d = common::temp_dir("rr-zero-ord");
    std::fs::create_dir_all(d.join("share/out")).unwrap();
    std::fs::write(d.join("share/out/empty.bin"), b"").unwrap();
    std::fs::write(d.join("share/out/real.bin"), vec![0xb7; 1 << 20]).unwrap();
    let host = Arc::new(FolderHost {
        root: d.join("share"),
        jobs_dir: d.join("hjobs"),
    });
    let (addr, _ctx, id, peers) = common::paired_ctx(|c| c.with_jobs(host)).await;
    let s = connect(&addr.to_string(), id, peers, "client", common::fast())
        .await
        .unwrap();
    let mut link = s.job([10; 16]);
    let sink = Arc::new(LocalSink::new(d.join("got"), false));
    let r = tokio::time::timeout(
        Duration::from_secs(30),
        download_job(
            &mut link,
            "out",
            gen::JF_ORDERED,
            sink,
            opts(&d.join("jobs"), true),
        ),
    )
    .await
    .expect("the ordered download completed within 30 s")
    .unwrap();
    assert_eq!(r.files, 2);
    same(&d.join("share/out"), &d.join("got"));
}

/// The zero-frame half of the empty-file fix, pinned without the sender: only the real
/// file's frames arrive — nothing is ever sent for the zero-byte file — and the job
/// still ends with both files done (the empty one on disk too).
#[tokio::test(flavor = "multi_thread")]
async fn a_file_with_no_frames_still_completes_the_job() {
    let d = common::temp_dir("rr-zero-frames");
    let host = Arc::new(CreditHost {
        root: d.join("share"),
        jobs: d.join("hjobs"),
        credit: 1 << 20,
    });
    let (addr, _ctx, id, peers) = common::paired_ctx(|c| c.with_jobs(host)).await;
    let s = connect(&addr.to_string(), id, peers, "client", common::fast())
        .await
        .unwrap();
    let job = [0x77; 16];
    let m = Manifest {
        entries: vec![
            Entry {
                kind: gen::ENTRY_FILE,
                mode: 0o644,
                size: 0,
                mtime: 0,
                path: "empty.bin".into(),
                root: None,
            },
            Entry {
                kind: gen::ENTRY_FILE,
                mode: 0o644,
                size: 4,
                mtime: 0,
                path: "real.bin".into(),
                root: None,
            },
        ],
    };
    let mut link = s.job(job);
    let (ack, _map) = open_and_map(&mut link, job, "in", &m).await;
    assert_eq!(ack.credit, 1 << 20);
    let lane = link.opener().unwrap().open().await.unwrap();
    link.lane(lane)
        .unwrap()
        .tx
        .send_raw(Bundle::TYPE, 0, 1, bundle_body(job, 1, b"hey!"))
        .await
        .unwrap();
    let done: JobDone = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let f = next_control(&mut link).await;
            if f.ty == JobDone::TYPE {
                break f.decode().unwrap();
            }
        }
    })
    .await
    .expect("the job completed within 20 s");
    assert_eq!(done.status, 0);
    assert_eq!(done.files, 2);
    assert_eq!(std::fs::read(d.join("share/in/empty.bin")).unwrap(), b"");
    assert_eq!(std::fs::read(d.join("share/in/real.bin")).unwrap(), b"hey!");
}

/// The Windows half of the §11.2 guard: check_path splits on '/', so a backslash path
/// passes it on a Unix host — but Windows treats '\\' as a separator and
/// `PathBuf::join("a\\..\\..\\x")` escapes the share. The share-root check refuses
/// backslashes outright (a JobOpen.root never legitimately contains one).
#[tokio::test(flavor = "multi_thread")]
async fn a_host_refuses_backslash_paths() {
    let d = common::temp_dir("rr-bslash");
    let host = Arc::new(FolderHost {
        root: d.join("share"),
        jobs_dir: d.join("hjobs"),
    });
    let (addr, _ctx, id, peers) = common::paired_ctx(|c| c.with_jobs(host)).await;
    let s = connect(&addr.to_string(), id, peers, "client", common::fast())
        .await
        .unwrap();
    let src = LocalSource::new(d.clone());
    let m = Arc::new(Manifest::default());
    let mut link = s.job([0x55; 16]);
    let e = send_job(
        &mut link,
        m,
        Arc::new(src),
        SendOptions::upload("a\\..\\..\\x"),
    )
    .await
    .unwrap_err();
    assert!(matches!(e, ava1::send::SendError::Refused { status, .. } if status == gen::ERR_PATH));
}

/// A job reopened with the same manifest resumes from the engine's journal: the map
/// carries the durable file, the journal's Open records exactly the sink's resume key,
/// the ack carries the job's ABSOLUTE grant again, and every Credit frame is just the
/// delta it freed — never a grant (the extra credit note: only the ack sets the window).
#[tokio::test(flavor = "multi_thread")]
async fn a_reopened_job_resumes_from_the_engine_journal_and_credits_are_deltas() {
    let d = common::temp_dir("rr-resume");
    let host = Arc::new(CreditHost {
        root: d.join("share"),
        jobs: d.join("hjobs"),
        credit: 1 << 20,
    });
    let (addr, _ctx, id, peers) = common::paired_ctx(|c| c.with_jobs(host)).await;
    let job = [0x11; 16];
    let m = Manifest {
        entries: vec![
            Entry {
                kind: gen::ENTRY_FILE,
                mode: 0o644,
                size: 4,
                mtime: 0,
                path: "a".into(),
                root: None,
            },
            Entry {
                kind: gen::ENTRY_FILE,
                mode: 0o644,
                size: 4,
                mtime: 0,
                path: "b".into(),
                root: None,
            },
        ],
    };
    let bundle0 = bundle_body(job, 0, b"one!");
    let bundle1 = bundle_body(job, 1, b"two!");

    // First session: the ack is the absolute grant, one file arrives, its batch is
    // journaled (Durable), and the Credit frame is the delta the frame freed.
    {
        let s = connect(
            &addr.to_string(),
            id.clone(),
            peers.clone(),
            "client",
            common::fast(),
        )
        .await
        .unwrap();
        let mut link = s.job(job);
        let (ack, map) = open_and_map(&mut link, job, "in", &m).await;
        assert_eq!(ack.credit, 1 << 20, "the ack is the absolute grant");
        assert!(map.done.is_empty());
        let lane = link.opener().unwrap().open().await.unwrap();
        link.lane(lane)
            .unwrap()
            .tx
            .send_raw(Bundle::TYPE, 0, 1, bundle0.clone())
            .await
            .unwrap();
        let recv: ava1::gen::Received = next_control(&mut link).await.decode().unwrap();
        assert_eq!(recv.lane, lane);
        let credit: Credit = next_control(&mut link).await.decode().unwrap();
        assert_eq!(
            credit.bytes,
            bundle0.len() as u64,
            "a Credit frame is the delta it freed, not the grant"
        );
        let durable: Durable = next_control(&mut link).await.decode().unwrap();
        assert_eq!(durable.files, vec![gen::FileRun { first: 0, count: 1 }]);
        drop(s); // the session ends mid-job: file 1 never arrived
    }

    // The journal on disk is the C receiver's layout: the Open records the sink's resume
    // key — the destination root and the construction-time staged decision (ruling Q1).
    let dir = journal::job_dir(&d.join("hjobs"), &job);
    let (_, recs) = journal::Journal::open(&dir).unwrap();
    let open_rec: gen::JnlOpen = match &recs[0] {
        Record::Open(o) => o.clone(),
        r => panic!("expected the Open record, got {r:?}"),
    };
    assert_eq!(
        open_rec.root,
        d.join("share/in").to_str().unwrap(),
        "the journal's root is the sink's resume-key root"
    );
    assert_eq!(
        open_rec.staged, 1,
        "staged = !single_file && !root.exists()"
    );
    assert_eq!(open_rec.flags, 0);
    assert_eq!(open_rec.manifest_hash, m.hash());

    // Second session (a resume via JobOpen, SPEC.md §11.5): the map carries the durable
    // file, the ack again carries the ABSOLUTE grant — nothing outstanding carries across
    // a reconnect — and the remaining file's Credit is again just its delta.
    let s = connect(&addr.to_string(), id, peers, "client", common::fast())
        .await
        .unwrap();
    let mut link = s.job(job);
    let (ack, map) = open_and_map(&mut link, job, "in", &m).await;
    assert_eq!(
        ack.credit,
        1 << 20,
        "the resume ack is the absolute grant, not a remainder"
    );
    assert_eq!(
        map.done.iter().copied().collect::<Vec<_>>(),
        vec![0],
        "the journal's durable file is done on the wire"
    );
    assert!(map.partial.is_empty());
    let lane = link.opener().unwrap().open().await.unwrap();
    link.lane(lane)
        .unwrap()
        .tx
        .send_raw(Bundle::TYPE, 0, 1, bundle1.clone())
        .await
        .unwrap();
    let recv: ava1::gen::Received = next_control(&mut link).await.decode().unwrap();
    assert_eq!(recv.lane, lane);
    let credit: Credit = next_control(&mut link).await.decode().unwrap();
    assert_eq!(
        credit.bytes,
        bundle1.len() as u64,
        "the resumed job's Credit is again just the delta"
    );
    let done: JobDone = loop {
        let f = next_control(&mut link).await;
        if f.ty == JobDone::TYPE {
            break f.decode().unwrap();
        }
    };
    assert_eq!(done.status, 0);
    assert_eq!(std::fs::read(d.join("share/in/a")).unwrap(), b"one!");
    assert_eq!(std::fs::read(d.join("share/in/b")).unwrap(), b"two!");
}

/// Ruling Q3: the part file is staged next to the final path (same directory), so the
/// part→final rename can never cross a device — the placement is the guard.
#[test]
fn the_part_file_lives_in_the_final_files_parent_directory() {
    let d = common::temp_dir("rr-part");
    let root = d.join("root");
    std::fs::create_dir_all(&root).unwrap();
    let sink = LocalSink::new(root.clone(), false);
    let m = Manifest {
        entries: vec![
            Entry {
                kind: gen::ENTRY_DIR,
                mode: 0o755,
                size: 0,
                mtime: 0,
                path: "a".into(),
                root: None,
            },
            Entry {
                kind: gen::ENTRY_FILE,
                mode: 0o644,
                size: 8,
                mtime: 0,
                path: "a/b".into(),
                root: None,
            },
        ],
    };
    sink.prepare(&m).unwrap();
    sink.write_at(1, 0, b"hello!!!").unwrap();
    assert!(
        root.join("a/b.ava-part").exists(),
        "the part file is in the final file's parent directory"
    );
    assert!(!root.join("a/b").exists());
    sink.commit(1).unwrap();
    assert_eq!(std::fs::read(root.join("a/b")).unwrap(), b"hello!!!");
    assert!(!root.join("a/b.ava-part").exists());
}
