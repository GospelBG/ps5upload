#![cfg(unix)]
use std::path::{Path, PathBuf};

use ava1::gen::{ENTRY_DIR, ENTRY_FILE};
use ava1::manifest::{Entry, Manifest};
use ava1::verify::GROUP;
use ava1_ctest::*;

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ava1-apply-{tag}-{}", std::process::id()));
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

fn send_large(job: &CApplyJob, id: u32, d: &[u8], reverse: bool) {
    let g = GROUP as usize;
    let mut offs: Vec<usize> = (0..d.len()).step_by(g).collect();
    if reverse {
        offs.reverse();
    }
    for o in offs {
        job.chunk(id, o as u64, &d[o..(o + g).min(d.len())]);
    }
    job.root(id, *blake3::hash(d).as_bytes());
}

#[test]
fn a_large_file_assembles_out_of_order_and_commits() {
    let t = tmp("large");
    let root = t.join("dest");
    std::fs::create_dir_all(&root).unwrap(); // existing root: merge mode
    let d = data(5 * GROUP as usize + 17, 1);
    let m = Manifest {
        entries: vec![dir("sub"), file("sub/big.bin", d.len() as u64)],
    };
    let job = CApplyJob::begin(&t.join("jobs"), &root, 0, &m, 0);
    send_large(&job, 1, &d, true);
    assert_eq!(job.wait(10_000), 0);
    let out = root.join("sub/big.bin");
    assert_eq!(std::fs::read(&out).unwrap(), d);
    assert!(!root.join("sub/big.bin.ava-part").exists());
    let md = std::fs::metadata(&out).unwrap();
    assert_eq!(std::os::unix::fs::MetadataExt::mtime(&md), 1_600_000_000);
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&md.permissions()) & 0o777,
        0o640
    );
    let ev = job.events();
    assert!(ev.contains("durable files=1+1"), "{ev}");
    assert!(ev.ends_with("done 0\n"), "{ev}");
}

#[test]
fn tiny_files_apply_in_parallel_and_become_durable_in_batches() {
    let t = tmp("tiny");
    let root = t.join("dest");
    let n = 2000;
    let mut entries = vec![dir("a")];
    for i in 0..n {
        entries.push(file(&format!("a/f{i:04}"), (i % 300) as u64));
    }
    let m = Manifest { entries };
    let job = CApplyJob::begin(&t.join("jobs"), &root, 0, &m, 0);
    for i in 0..n {
        let d = data(i % 300, i as u8);
        job.record(i as u32 + 1, &d, *blake3::hash(&d).as_bytes());
    }
    assert_eq!(job.wait(30_000), 0);
    // staged: the tree appears at once, complete
    assert!(!t.join("dest.ava-part").exists());
    for i in (0..n).step_by(97) {
        assert_eq!(
            std::fs::read(root.join(format!("a/f{i:04}"))).unwrap(),
            data(i % 300, i as u8)
        );
    }
    let ev = job.events();
    assert!(
        ev.matches("durable").count() >= 2,
        "expected several batches: {ev}"
    );
}

#[test]
fn a_wrong_root_resets_the_file_and_asks_for_it_again() {
    let t = tmp("badroot");
    let root = t.join("dest");
    std::fs::create_dir_all(&root).unwrap();
    let d = data(3 * GROUP as usize, 9);
    let m = Manifest {
        entries: vec![file("x", d.len() as u64)],
    };
    let job = CApplyJob::begin(&t.join("jobs"), &root, 0, &m, 0);
    for o in (0..d.len()).step_by(GROUP as usize) {
        job.chunk(0, o as u64, &d[o..o + GROUP as usize]);
    }
    job.root(0, [0xee; 32]);
    job.wait_event("retry 0 1", 10_000);
    assert!(!root.join("x").exists());
    send_large(&job, 0, &d, false);
    assert_eq!(job.wait(10_000), 0);
    assert_eq!(std::fs::read(root.join("x")).unwrap(), d);
}

#[test]
fn a_bad_record_root_is_refused() {
    let t = tmp("badrec");
    let root = t.join("dest");
    std::fs::create_dir_all(&root).unwrap();
    let m = Manifest {
        entries: vec![file("s", 3)],
    };
    let job = CApplyJob::begin(&t.join("jobs"), &root, 0, &m, 0);
    job.record(0, b"abc", [0; 32]);
    job.wait_event("retry 0 1", 10_000);
    assert!(!root.join("s").exists());
}

#[test]
fn c_commit_refuses_cross_device_rename() {
    let t = tmp("xdev");
    let root = t.join("dest");
    std::fs::create_dir_all(&root).unwrap();
    let d = data(2 * GROUP as usize + 1, 3);
    let m = Manifest {
        entries: vec![file("big", d.len() as u64)],
    };
    c_set_same_device(0); // every st_dev comparison reports "crosses"
    let job = CApplyJob::begin(&t.join("jobs"), &root, 0, &m, 0);
    send_large(&job, 0, &d, false);
    assert_eq!(job.wait(10_000), ava1::gen::ERR_CROSS_DEVICE as i32);
    c_set_same_device(1);
    assert!(root.join("big.ava-part").exists());
    assert!(!root.join("big").exists());
}

#[test]
fn a_staged_tree_is_not_moved_over_a_root_that_appeared() {
    let t = tmp("exists");
    let root = t.join("dest");
    let m = Manifest {
        entries: vec![file("a", 1), file("b", 1)],
    };
    let job = CApplyJob::begin(&t.join("jobs"), &root, 0, &m, 0);
    job.record(0, b"1", *blake3::hash(b"1").as_bytes());
    std::fs::create_dir_all(&root).unwrap(); // someone made it meanwhile
    job.record(1, b"2", *blake3::hash(b"2").as_bytes());
    assert_eq!(job.wait(10_000), ava1::gen::ERR_EXISTS as i32);
    assert!(Path::new(&format!("{}.ava-part", root.display()))
        .join("a")
        .exists());
}

#[test]
fn a_single_file_lands_through_its_part_file() {
    let t = tmp("single");
    let dest = t.join("one.pkg");
    let d = data(GROUP as usize * 2 + 5, 4);
    let m = Manifest {
        entries: vec![file("one.pkg", d.len() as u64)],
    };
    let job = CApplyJob::begin(&t.join("jobs"), &dest, ava1::gen::JF_SINGLE_FILE, &m, 0);
    send_large(&job, 0, &d, true);
    assert_eq!(job.wait(10_000), 0);
    assert_eq!(std::fs::read(&dest).unwrap(), d);
    assert!(!t.join("one.pkg.ava-part").exists());
}
