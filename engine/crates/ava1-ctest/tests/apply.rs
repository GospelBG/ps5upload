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

// ---- fix round 1 ------------------------------------------------------------------

/// AVA1_E_PROTO, what the apply entry points answer for a frame that breaks the rules.
const E_PROTO: i32 = -11;

#[test]
fn a_stop_mid_batch_journals_and_acknowledges_nothing() {
    let t = tmp("stopbatch");
    let root = t.join("dest");
    std::fs::create_dir_all(&root).unwrap();
    let n = 20;
    let m = Manifest {
        entries: (0..n).map(|i| file(&format!("f{i:02}"), 3)).collect(),
    };
    // every fsync takes 5 s: the first batch is still syncing when the job is stopped
    let job = CApplyJob::begin(&t.join("jobs"), &root, 0, &m, 5_000_000);
    for i in 0..n {
        job.record(i, b"abc", *blake3::hash(b"abc").as_bytes());
    }
    std::thread::sleep(std::time::Duration::from_millis(1000));
    let t0 = std::time::Instant::now();
    let ev = job.end();
    assert!(t0.elapsed().as_secs() < 5, "the stop waited out the fsyncs");
    assert!(!ev.contains("durable"), "{ev}");
    let (_, recs) =
        ava1::journal::Journal::open(&ava1::journal::job_dir(&t.join("jobs"), &[7; 16])).unwrap();
    assert!(
        !recs
            .iter()
            .any(|r| matches!(r, ava1::journal::Record::Batch(_))),
        "{recs:?}"
    );
}

#[test]
fn a_duplicate_chunk_racing_the_commit_does_not_fail_the_job() {
    let t = tmp("dupcommit");
    let root = t.join("dest");
    std::fs::create_dir_all(&root).unwrap();
    let g = GROUP as usize;
    let (d0, d1) = (data(2 * g, 5), data(2 * g, 6));
    let m = Manifest {
        entries: vec![file("a", d0.len() as u64), file("b", d1.len() as u64)],
    };
    let job = CApplyJob::begin(&t.join("jobs"), &root, 0, &m, 0);
    // a duplicate of file 0's first group lands after its root is verified, before it is
    // marked committed
    job.dup_on_commit(0, 0, &d0[..g]);
    send_large(&job, 0, &d0, false);
    job.chunk(1, 0, &d1[..g]);
    job.chunk(1, g as u64, &d1[g..]);
    job.wait_event("durable files=0+1", 10_000);
    std::thread::sleep(std::time::Duration::from_millis(400)); // a few more batches
    job.root(1, *blake3::hash(&d1).as_bytes());
    assert_eq!(job.wait(10_000), 0, "{}", job.events());
    assert_eq!(std::fs::read(root.join("a")).unwrap(), d0);
    assert_eq!(std::fs::read(root.join("b")).unwrap(), d1);
}

#[test]
fn a_bundle_record_naming_a_directory_is_refused() {
    let t = tmp("recdir");
    let root = t.join("dest");
    std::fs::create_dir_all(&root).unwrap();
    let m = Manifest {
        entries: vec![dir("d"), file("f", 1)],
    };
    let job = CApplyJob::begin(&t.join("jobs"), &root, 0, &m, 0);
    assert_eq!(
        job.try_record(0, b"x", *blake3::hash(b"x").as_bytes()),
        E_PROTO
    );
    assert_eq!(
        job.try_record(9, b"x", *blake3::hash(b"x").as_bytes()),
        E_PROTO
    );
    job.record(1, b"1", *blake3::hash(b"1").as_bytes());
    assert_eq!(job.wait(10_000), 0, "{}", job.events());
}

#[test]
fn a_truncated_bundle_is_refused() {
    let t = tmp("rectrunc");
    let root = t.join("dest");
    std::fs::create_dir_all(&root).unwrap();
    let m = Manifest {
        entries: vec![file("f", 4)],
    };
    let job = CApplyJob::begin(&t.join("jobs"), &root, 0, &m, 0);
    let mut rec = Vec::new();
    rec.extend_from_slice(&40u32.to_le_bytes()); // claims 40 bytes, carries 6
    rec.extend_from_slice(&[0, 0, 0, 0, 1, 2]);
    assert_eq!(job.raw_bundle(&rec, 1), E_PROTO);
    assert_eq!(job.raw_bundle(&[], 1), E_PROTO); // a count the records do not match
    job.record(0, b"abcd", *blake3::hash(b"abcd").as_bytes());
    assert_eq!(job.wait(10_000), 0, "{}", job.events());
}

fn lines_in_order(ev: &str, want: &[&str]) {
    let mut at = 0;
    for w in want {
        match ev[at..].find(w) {
            Some(i) => at += i + w.len(),
            None => panic!("{w:?} missing or out of order in {ev}"),
        }
    }
}

#[test]
fn a_rename_is_synced_in_its_directory_before_it_is_journaled() {
    // a large file in merge mode: verify, rename, sync the directory, journal, drop the outboard
    let t = tmp("dirsync");
    let root = t.join("dest");
    std::fs::create_dir_all(&root).unwrap();
    let d = data(2 * GROUP as usize + 3, 8);
    let m = Manifest {
        entries: vec![file("big", d.len() as u64)],
    };
    let job = CApplyJob::begin(&t.join("jobs"), &root, 0, &m, 0);
    job.trace(true);
    send_large(&job, 0, &d, false);
    assert_eq!(job.wait(10_000), 0);
    lines_in_order(
        &job.events(),
        &[
            "hook 1 0\n",
            "hook 2 0\n",
            "hook 3 0\n",
            "hook 4 0\n",
            "hook 5 0\n",
            "done 0\n",
        ],
    );
    drop(job);
    // a staged tree: the staging rename, its directory sync, then JobDone
    let root2 = t.join("staged");
    let m = Manifest {
        entries: vec![file("s", 1)],
    };
    let job = CApplyJob::begin(&t.join("jobs2"), &root2, 0, &m, 0);
    job.trace(true);
    job.record(0, b"s", *blake3::hash(b"s").as_bytes());
    assert_eq!(job.wait(10_000), 0);
    lines_in_order(
        &job.events(),
        &["hook 2 4294967295\n", "hook 3 4294967295\n", "done 0\n"],
    );
}

#[test]
fn a_final_rename_onto_a_directory_reports_exists() {
    let t = tmp("renexists");
    let root = t.join("dest");
    std::fs::create_dir_all(root.join("x")).unwrap();
    std::fs::write(root.join("x/keep"), b"k").unwrap();
    let d = data(2 * GROUP as usize + 1, 2);
    let m = Manifest {
        entries: vec![file("x", d.len() as u64)],
    };
    let job = CApplyJob::begin(&t.join("jobs"), &root, 0, &m, 0);
    send_large(&job, 0, &d, false);
    assert_eq!(job.wait(10_000), ava1::gen::ERR_EXISTS as i32);
    assert!(root.join("x/keep").exists());
}

#[test]
fn chunks_outside_the_group_rules_are_refused() {
    let t = tmp("chunkrules");
    let root = t.join("dest");
    std::fs::create_dir_all(&root).unwrap();
    let g = GROUP;
    let m = Manifest {
        entries: vec![dir("d"), file("f", 3 * g + 5)],
    };
    let job = CApplyJob::begin(&t.join("jobs"), &root, 0, &m, 0);
    let wraps = u64::MAX - g + 1; // group-aligned; off + len wraps past zero
    assert_eq!(job.try_chunk(1, wraps, &[0; 16]), E_PROTO);
    assert_eq!(job.try_chunk(1, 0, &vec![0; (g / 2) as usize]), E_PROTO); // short mid-file
    assert_eq!(job.try_chunk(1, 4 * g, &[0; 1]), E_PROTO); // past the size
    assert_eq!(job.try_chunk(1, 3 * g, &[0; 6]), E_PROTO); // longer than the tail
    assert_eq!(job.try_chunk(1, 5, &[0; 1]), E_PROTO); // unaligned
    assert_eq!(job.try_chunk(0, 0, &[0; 1]), E_PROTO); // a directory
    assert_eq!(job.try_chunk(7, 0, &[0; 1]), E_PROTO); // no such file
    assert_eq!(job.try_chunk(1, 3 * g, &[0; 5]), 0); // the short final chunk is fine
}
