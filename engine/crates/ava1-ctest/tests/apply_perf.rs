//! perf-apply (review 003 §2.1, §3.3, §6): the receive/apply path's latency fixes, each proved
//! through the C engine's test hooks rather than by timing.
#![cfg(unix)]
use std::path::PathBuf;

use ava1::gen::{ENTRY_DIR, ENTRY_FILE};
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
