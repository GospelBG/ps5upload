//! perf-apply (review 003 §2.1, §3.3, §6): the receive/apply path's latency fixes, each proved
//! through the C engine's test hooks rather than by timing.
#![cfg(unix)]
use std::path::PathBuf;

use ava1::gen::{self, ENTRY_DIR, ENTRY_FILE};
use ava1::manifest::{Entry, Manifest};
use ava1::verify::GROUP;
use ava1_ctest::*;

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ava1-perf-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn file(path: &str, size: u64) -> Entry {
    Entry {
        kind: ENTRY_FILE,
        mode: 0o640,
        size,
        mtime: 1_600_000_000,
        path: path.into(),
        root: None,
    }
}

fn dir(path: &str) -> Entry {
    Entry {
        kind: ENTRY_DIR,
        mode: 0o755,
        size: 0,
        mtime: 0,
        path: path.into(),
        root: None,
    }
}

fn data(n: usize, seed: u8) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(13).wrapping_add(seed))
        .collect()
}

fn send_large(job: &CApplyJob, id: u32, d: &[u8]) {
    let g = GROUP as usize;
    for o in (0..d.len()).step_by(g).rev() {
        job.chunk(id, o as u64, &d[o..(o + g).min(d.len())]);
    }
    job.root(id, *blake3::hash(d).as_bytes());
}

#[test]
fn preallocation_happens_outside_the_job_mutex() {
    // Review 003 §2.1: a 4 GiB preallocation under j->mu stalled the feeder and every other
    // worker for minutes on a slow drive. The hook fires right before each preallocation and
    // the shim checks, from that very thread, whether j->mu is held.
    let t = tmp("prealloc");
    let root = t.join("dest");
    std::fs::create_dir_all(&root).unwrap();
    let a = data(4 * GROUP as usize + 3, 1);
    let b = data(3 * GROUP as usize + 1, 2);
    let m = Manifest {
        entries: vec![file("a.bin", a.len() as u64), file("b.bin", b.len() as u64)],
    };
    let job = CApplyJob::begin(&t.join("jobs"), &root, 0, &m, 0);
    send_large(&job, 0, &a);
    send_large(&job, 1, &b);
    assert_eq!(job.wait(10_000), 0, "{}", job.events());
    assert_eq!(std::fs::read(root.join("a.bin")).unwrap(), a);
    assert_eq!(std::fs::read(root.join("b.bin")).unwrap(), b);
    let p = job.probe();
    assert_eq!(p.prealloc_calls, 2, "once per file, however many chunks");
    assert_eq!(
        p.prealloc_with_job_mutex_held, 0,
        "preallocated under j->mu"
    );
    drop(dir("unused"));
}

#[test]
fn commits_run_on_the_workers_not_the_job_thread() {
    // Review 003 §3.3 item 2: commit_large (four fsyncs) ran inline on the job thread, so a
    // batch's commits stopped every other batch. They now run on the worker pool.
    let t = tmp("commits");
    let root = t.join("dest");
    std::fs::create_dir_all(&root).unwrap();
    let n = 8usize;
    let files: Vec<Vec<u8>> = (0..n)
        .map(|i| data(2 * GROUP as usize + 5 + i, i as u8))
        .collect();
    let m = Manifest {
        entries: files
            .iter()
            .enumerate()
            .map(|(i, d)| file(&format!("f{i}.bin"), d.len() as u64))
            .collect(),
    };
    let job = CApplyJob::begin(&t.join("jobs"), &root, 0, &m, 0);
    for (i, d) in files.iter().enumerate() {
        send_large(&job, i as u32, d);
    }
    assert_eq!(job.wait(15_000), 0, "{}", job.events());
    for (i, d) in files.iter().enumerate() {
        assert_eq!(&std::fs::read(root.join(format!("f{i}.bin"))).unwrap(), d);
    }
    let p = job.probe();
    assert_eq!(p.commits, n as u64);
    assert_eq!(p.commits_on_job_thread, 0, "a commit ran on the job thread");
}

/// The ids a map event line reports as done ("map status=0 last=1 done=0+2,5+1, partial=N").
fn done_ids(ev: &str) -> Vec<u32> {
    let line = ev.lines().find(|l| l.starts_with("map status=0")).unwrap();
    let runs = line
        .split("done=")
        .nth(1)
        .unwrap()
        .split(' ')
        .next()
        .unwrap();
    let mut out = vec![];
    for r in runs.split(',').filter(|r| !r.is_empty()) {
        let (a, b) = r.split_once('+').unwrap();
        let (a, b): (u32, u32) = (a.parse().unwrap(), b.parse().unwrap());
        out.extend(a..a + b);
    }
    out
}

/// Several large files commit on the workers at once and the process dies at `crash_at`; the
/// resumed job must finish every file with the right bytes and lose none, whichever commit was
/// cut and wherever (renamed, not journaled; or not started).
fn worker_commit_crash(tag: &str, crash_at: i32) {
    let t = tmp(tag);
    std::fs::create_dir_all(t.join("dest")).unwrap(); // merge mode: the commit really renames
    let n = 6usize;
    let files: Vec<Vec<u8>> = (0..n)
        .map(|i| data(2 * GROUP as usize + 11 + i, 40 + i as u8))
        .collect();
    let m = Manifest {
        entries: files
            .iter()
            .enumerate()
            .map(|(i, d)| file(&format!("big{i}"), d.len() as u64))
            .collect(),
    };
    let r = CRecv::open(
        &t.join("jobs"),
        &t.join("dest"),
        0,
        gen::POLICY_REPLACE,
        crash_at,
    );
    r.manifest(&m);
    r.wait_event("map status=0", 5000);
    for (i, d) in files.iter().enumerate() {
        send_large(&r, i as u32, d);
    }
    r.wait_stopped(10_000); // the first commit to reach the crash point stopped the job
    let r = r.restart(0);
    r.manifest(&m);
    let ev = r.wait_event("map status=0", 5000);
    let done = done_ids(&ev);
    for (i, d) in files.iter().enumerate() {
        if !done.contains(&(i as u32)) {
            // what the sender does for a file the map does not list as done: send it again
            send_large(&r, i as u32, d);
        }
    }
    assert_eq!(r.wait(15_000), 0, "{}", r.events());
    for (i, d) in files.iter().enumerate() {
        assert_eq!(
            &std::fs::read(t.join(format!("dest/big{i}"))).unwrap(),
            d,
            "file {i} after the crash at {crash_at}"
        );
        assert!(!t.join(format!("dest/big{i}.ava-part")).exists());
    }
}

#[test]
fn a_worker_commit_cut_between_rename_and_journal_loses_no_file() {
    worker_commit_crash("wcommit-renamed", 4); // AVA1_CRASH_COMMIT_RENAMED
}

#[test]
fn a_worker_commit_cut_before_it_starts_loses_no_file() {
    worker_commit_crash("wcommit-before", 5); // AVA1_CRASH_BEFORE_COMMIT
}
