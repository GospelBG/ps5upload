#![cfg(unix)]
use std::path::PathBuf;

use ava1::gen::{self, ENTRY_DIR, ENTRY_FILE};
use ava1::journal::{job_dir, Journal, Record};
use ava1::manifest::{Entry, Manifest};
use ava1::verify::GROUP;
use ava1_ctest::*;

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ava1-recv-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn f(path: &str, size: u64, mtime: u64) -> Entry {
    Entry {
        kind: ENTRY_FILE,
        mode: 0o644,
        size,
        mtime,
        path: path.into(),
        root: None,
    }
}

fn small_files(n: usize) -> Manifest {
    let mut entries = vec![Entry {
        kind: ENTRY_DIR,
        mode: 0o755,
        size: 0,
        mtime: 0,
        path: "d".into(),
        root: None,
    }];
    for i in 0..n {
        entries.push(f(&format!("d/{i}"), 4, 1_600_000_000));
    }
    Manifest { entries }
}

fn body(i: usize) -> Vec<u8> {
    format!("{i:04}").into_bytes()
}

fn send_all(r: &CRecv, n: usize) {
    for i in 0..n {
        r.record(i as u32 + 1, &body(i), *blake3::hash(&body(i)).as_bytes());
    }
}

fn journal(t: &std::path::Path) -> Vec<Record> {
    Journal::open(&job_dir(&t.join("jobs"), &[7; 16]))
        .unwrap()
        .1
}

#[test]
fn a_new_folder_opens_staged_with_an_empty_map() {
    let t = tmp("new");
    let m = small_files(3);
    let r = CRecv::open(&t.join("jobs"), &t.join("dest"), 0, gen::POLICY_REPLACE, 0);
    assert_eq!(r.ack_staged(), 1);
    r.manifest(&m);
    r.wait_event("map status=0 last=1 done= partial=0", 5000);
    send_all(&r, 3);
    assert_eq!(r.wait(10_000), 0);
    assert_eq!(std::fs::read(t.join("dest/d/2")).unwrap(), body(2));
    assert!(!t.join("dest.ava-part").exists());
}

#[test]
fn c_crash_between_sync_and_journal_resends_the_batch() {
    for (crash, expect_done) in [(1, false), (2, true)] {
        let t = tmp(&format!("crash{crash}"));
        let m = small_files(50);
        let r = CRecv::open(
            &t.join("jobs"),
            &t.join("dest"),
            0,
            gen::POLICY_REPLACE,
            crash,
        );
        r.manifest(&m);
        r.wait_event("map status=0", 5000);
        // all 50 in one batch, deterministically: no batch starts until they are pending
        r.hold_batches(true);
        send_all(&r, 50);
        r.wait_pending(50, 5000);
        r.hold_batches(false);
        r.wait_stopped(10_000); // the injected crash stopped the job thread
        let r = r.restart(0); // payload restart: memory gone, disk kept
        r.manifest(&m);
        let ev = r.wait_event("map status=0", 5000);
        assert_eq!(
            ev.contains("done=1+50"),
            expect_done,
            "crash point {crash}: {ev}"
        );
        if !expect_done {
            send_all(&r, 50);
        }
        // the destination this job took as its lock (an empty folder) is recognised as ours
        assert_eq!(r.wait(10_000), 0, "{}", r.events());
        assert_eq!(std::fs::read(t.join("dest/d/49")).unwrap(), body(49));
    }
}

#[test]
fn c_changed_file_loses_its_progress() {
    let t = tmp("changed");
    std::fs::create_dir_all(t.join("dest")).unwrap();
    let d: Vec<u8> = (0..3 * GROUP as usize).map(|i| i as u8).collect();
    let m = Manifest {
        entries: vec![f("big", d.len() as u64, 100)],
    };
    let r = CRecv::open(&t.join("jobs"), &t.join("dest"), 0, gen::POLICY_REPLACE, 0);
    r.manifest(&m);
    r.wait_event("map status=0", 5000);
    r.chunk(0, 0, &d[..GROUP as usize]);
    r.wait_event("durable", 5000);
    let r = r.restart(0);
    r.manifest(&m);
    assert!(r.wait_event("map status=0", 5000).contains("partial=1"));
    let r = r.restart(0);
    let mut m2 = m.clone();
    m2.entries[0].mtime = 101; // the source changed while we were away
    r.manifest(&m2);
    assert!(r.wait_event("map status=0", 5000).contains("partial=0"));
    // the old bytes are gone before any new range is asked for, and the reset is journaled
    assert!(!t.join("dest/big.ava-part").exists());
    let d2: Vec<u8> = d.iter().map(|b| b ^ 0x5a).collect();
    for o in (0..d2.len()).step_by(GROUP as usize) {
        r.chunk(0, o as u64, &d2[o..o + GROUP as usize]);
    }
    r.root(0, *blake3::hash(&d2).as_bytes());
    assert_eq!(r.wait(10_000), 0, "{}", r.events());
    assert_eq!(std::fs::read(t.join("dest/big")).unwrap(), d2);
    drop(r);
    assert!(journal(&t).iter().any(|x| matches!(x, Record::Reset(0))));
}

#[test]
fn a_changed_manifest_keeps_unchanged_files_by_path() {
    let t = tmp("remap");
    std::fs::create_dir_all(t.join("dest")).unwrap();
    let g = GROUP as usize;
    let d: Vec<u8> = (0..2 * g + 7).map(|i| (i * 3) as u8).collect();
    let m = Manifest {
        entries: vec![f("a", 4, 9), f("big", d.len() as u64, 9)],
    };
    let r = CRecv::open(&t.join("jobs"), &t.join("dest"), 0, gen::POLICY_REPLACE, 0);
    r.manifest(&m);
    r.wait_event("map status=0", 5000);
    r.chunk(1, 0, &d[..g]);
    r.wait_event("durable", 5000);
    let r = r.restart(0);
    // a new file sorts first: "big" moves from id 1 to id 2 and keeps its group
    let m2 = Manifest {
        entries: vec![f("0new", 4, 9), f("a", 4, 9), f("big", d.len() as u64, 9)],
    };
    r.manifest(&m2);
    let ev = r.wait_event("map status=0", 5000);
    assert!(ev.contains("partial=1"), "{ev}");
    r.chunk(2, g as u64, &d[g..]);
    r.root(2, *blake3::hash(&d).as_bytes());
    r.record(0, b"new!", *blake3::hash(b"new!").as_bytes());
    r.record(1, b"aaaa", *blake3::hash(b"aaaa").as_bytes());
    assert_eq!(r.wait(10_000), 0, "{}", r.events());
    assert_eq!(std::fs::read(t.join("dest/big")).unwrap(), d);
}

#[test]
fn skip_existing_and_verify_mark_matching_files_done() {
    let t = tmp("policy");
    let dest = t.join("dest");
    std::fs::create_dir_all(dest.join("d")).unwrap();
    std::fs::write(dest.join("d/0"), body(0)).unwrap();
    std::fs::write(dest.join("d/1"), b"XXXX").unwrap();
    for p in ["d/0", "d/1"] {
        let ft = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        std::fs::File::options()
            .write(true)
            .open(dest.join(p))
            .unwrap()
            .set_modified(ft)
            .unwrap();
    }
    let m = small_files(2);
    let r = CRecv::open(&t.join("jobs"), &dest, 0, gen::POLICY_SKIP_EXISTING, 0);
    r.manifest(&m);
    assert!(
        r.wait_event("map status=0", 5000).contains("done=1+2"),
        "both match by size+mtime"
    );
    drop(r);
    let t2 = tmp("policy2");
    let dest2 = t2.join("dest");
    std::fs::create_dir_all(dest2.join("d")).unwrap();
    std::fs::write(dest2.join("d/0"), body(0)).unwrap();
    std::fs::write(dest2.join("d/1"), b"XXXX").unwrap();
    let mut mv = small_files(2);
    mv.entries[1].root = Some(*blake3::hash(&body(0)).as_bytes());
    mv.entries[2].root = Some(*blake3::hash(&body(1)).as_bytes());
    let r = CRecv::open(&t2.join("jobs"), &dest2, 0, gen::POLICY_VERIFY, 0);
    r.manifest(&mv);
    let ev = r.wait_event("map status=0", 5000);
    assert!(
        ev.contains("done=1+1,") && !ev.contains("done=1+2"),
        "only d/0 hashes equal: {ev}"
    );
}

#[test]
fn a_torn_tail_group_fails_the_resume_check() {
    let t = tmp("verify");
    std::fs::create_dir_all(t.join("dest")).unwrap();
    let d: Vec<u8> = (0..3 * GROUP as usize).map(|i| (i * 7) as u8).collect();
    let m = Manifest {
        entries: vec![f("big", d.len() as u64, 5)],
    };
    let r = CRecv::open(&t.join("jobs"), &t.join("dest"), 0, gen::POLICY_REPLACE, 0);
    r.manifest(&m);
    r.wait_event("map status=0", 5000);
    r.chunk(0, 0, &d[..2 * GROUP as usize]);
    r.wait_event("durable", 5000);
    let r = r.restart(0);
    // Damage the second group on disk, as a crash mid-write could.
    let part = t.join("dest/big.ava-part");
    let mut b = std::fs::read(&part).unwrap();
    b[GROUP as usize + 10] ^= 0xff;
    std::fs::write(&part, &b).unwrap();
    r.manifest(&m);
    assert!(r.wait_event("map status=0", 5000).contains("partial=0"));
}

#[test]
fn an_intact_tail_passes_the_resume_check() {
    let t = tmp("intact");
    std::fs::create_dir_all(t.join("dest")).unwrap();
    let d: Vec<u8> = (0..3 * GROUP as usize).map(|i| (i * 5) as u8).collect();
    let m = Manifest {
        entries: vec![f("big", d.len() as u64, 5)],
    };
    let r = CRecv::open(&t.join("jobs"), &t.join("dest"), 0, gen::POLICY_REPLACE, 0);
    r.manifest(&m);
    r.wait_event("map status=0", 5000);
    r.chunk(0, 0, &d[..2 * GROUP as usize]);
    r.wait_event("durable", 5000);
    let r = r.restart(0);
    r.manifest(&m);
    assert!(r.wait_event("map status=0", 5000).contains("partial=1"));
    r.chunk(0, 2 * GROUP, &d[2 * GROUP as usize..]);
    r.root(0, *blake3::hash(&d).as_bytes());
    assert_eq!(r.wait(10_000), 0, "{}", r.events());
    assert_eq!(std::fs::read(t.join("dest/big")).unwrap(), d);
}

#[test]
fn a_manifest_that_does_not_match_its_end_is_refused() {
    let t = tmp("badend");
    let r = CRecv::open(&t.join("jobs"), &t.join("dest"), 0, gen::POLICY_REPLACE, 0);
    r.manifest_with_hash(&small_files(2), [9; 32]);
    r.wait_event(&format!("map status={}", gen::ERR_PROTOCOL), 5000);
}

#[test]
fn an_empty_folder_is_a_valid_job() {
    let t = tmp("empty");
    let r = CRecv::open(&t.join("jobs"), &t.join("dest"), 0, gen::POLICY_REPLACE, 0);
    r.manifest(&Manifest { entries: vec![] });
    r.wait_event("map status=0 last=1 done= partial=0", 5000);
    assert_eq!(r.wait(10_000), 0, "{}", r.events());
    assert!(t.join("dest").is_dir());
    assert!(!t.join("dest.ava-part").exists());
}

#[test]
fn an_entry_count_past_the_cap_is_refused_at_open() {
    let t = tmp("cap");
    assert_eq!(
        c_recv_open_status(&t.join("jobs"), &t.join("dest"), 4_000_001),
        gen::ERR_PROTOCOL as i32
    );
    assert_eq!(c_recv_open_status(&t.join("jobs"), &t.join("dest"), 10), 0);
}

#[test]
fn a_destination_that_appears_before_prepare_is_refused() {
    let t = tmp("taken");
    let r = CRecv::open(&t.join("jobs"), &t.join("dest"), 0, gen::POLICY_REPLACE, 0);
    assert_eq!(r.ack_staged(), 1);
    std::fs::create_dir_all(t.join("dest")).unwrap(); // someone else's, made after JobOpen
    r.manifest(&small_files(1));
    r.wait_event(&format!("map status={}", gen::ERR_EXISTS), 5000);
    assert_eq!(r.wait(10_000), gen::ERR_EXISTS as i32);
    // nothing was written into the folder that is not ours
    assert_eq!(std::fs::read_dir(t.join("dest")).unwrap().count(), 0);
}

#[test]
fn a_crash_after_taking_the_destination_resumes_cleanly() {
    let t = tmp("takecrash");
    let m = small_files(3);
    // stops right after mkdir(dest) and its directory sync, before the journal exists
    let r = CRecv::open(&t.join("jobs"), &t.join("dest"), 0, gen::POLICY_REPLACE, 3);
    r.manifest(&m);
    r.wait_stopped(10_000);
    assert!(t.join("dest").is_dir());
    let r = r.restart(0);
    r.manifest(&m);
    r.wait_event("map status=0", 5000);
    send_all(&r, 3);
    assert_eq!(r.wait(10_000), 0, "{}", r.events());
    assert_eq!(std::fs::read(t.join("dest/d/1")).unwrap(), body(1));
}

#[test]
fn a_finished_job_reopened_answers_all_done_and_its_status() {
    let t = tmp("finished");
    let m = small_files(3);
    let r = CRecv::open(&t.join("jobs"), &t.join("dest"), 0, gen::POLICY_REPLACE, 0);
    r.manifest(&m);
    r.wait_event("map status=0", 5000);
    send_all(&r, 3);
    assert_eq!(r.wait(10_000), 0);
    let r = r.restart(0);
    r.manifest(&m);
    let ev = r.wait_event("map status=0", 5000);
    assert!(ev.contains("done=1+3"), "{ev}");
    assert_eq!(r.wait(10_000), 0, "{}", r.events());
    assert_eq!(std::fs::read(t.join("dest/d/2")).unwrap(), body(2));
}

#[test]
fn resume_with_the_stored_manifest_answers_the_map() {
    let t = tmp("resume");
    std::fs::create_dir_all(t.join("dest")).unwrap();
    let m = small_files(4);
    let r = CRecv::open(&t.join("jobs"), &t.join("dest"), 0, gen::POLICY_REPLACE, 0);
    r.manifest(&m);
    r.wait_event("map status=0", 5000);
    r.hold_batches(true);
    send_all(&r, 2);
    r.wait_pending(2, 5000);
    r.hold_batches(false);
    r.wait_event("durable", 5000);
    let r = r.restart(0);
    r.resume([1; 32]); // not the manifest it holds
    r.wait_event(&format!("map status={}", gen::ERR_UNKNOWN_JOB), 5000);
    let r = r.restart(0);
    r.resume(m.hash());
    let ev = r.wait_event("map status=0", 5000);
    assert!(ev.contains("done=1+2"), "{ev}");
    r.record(3, &body(2), *blake3::hash(&body(2)).as_bytes());
    r.record(4, &body(3), *blake3::hash(&body(3)).as_bytes());
    assert_eq!(r.wait(10_000), 0, "{}", r.events());
}

#[test]
fn a_large_map_is_paged() {
    let t = tmp("paged");
    let dest = t.join("dest");
    std::fs::create_dir_all(dest.join("d")).unwrap();
    // every other file exists: 2,001 done runs, more than one JobMap page holds
    let n = 4002;
    let ft = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
    let m = small_files(n);
    for e in m.entries.iter().skip(1).step_by(2) {
        let p = dest.join(&e.path);
        std::fs::write(&p, b"zzzz").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(ft)
            .unwrap();
    }
    let r = CRecv::open(&t.join("jobs"), &dest, 0, gen::POLICY_SKIP_EXISTING, 0);
    r.manifest(&m);
    let ev = r.wait_event("map status=0 last=1", 10_000);
    let first = ev.find("map status=0 last=0").expect(&ev);
    assert!(first < ev.find("map status=0 last=1").unwrap());
}

#[test]
fn a_single_file_lands_at_its_root() {
    let t = tmp("single");
    let root = t.join("sub/file.bin");
    let m = Manifest {
        entries: vec![f("file.bin", 4, 7)],
    };
    let r = CRecv::open(
        &t.join("jobs"),
        &root,
        gen::JF_SINGLE_FILE,
        gen::POLICY_REPLACE,
        0,
    );
    assert_eq!(r.ack_staged(), 0);
    r.manifest(&m);
    r.wait_event("map status=0", 5000);
    r.record(0, b"wxyz", *blake3::hash(b"wxyz").as_bytes());
    assert_eq!(r.wait(10_000), 0, "{}", r.events());
    assert_eq!(std::fs::read(&root).unwrap(), b"wxyz");
}

#[test]
fn a_held_destination_that_gains_files_is_not_replaced() {
    let t = tmp("heldfull");
    let m = small_files(1);
    let r = CRecv::open(&t.join("jobs"), &t.join("dest"), 0, gen::POLICY_REPLACE, 0);
    r.manifest(&m);
    r.wait_event("map status=0", 5000);
    // our empty lock folder gains someone's file mid-upload
    std::fs::write(t.join("dest/theirs"), b"keep").unwrap();
    send_all(&r, 1);
    assert_eq!(r.wait(10_000), gen::ERR_EXISTS as i32);
    assert_eq!(std::fs::read(t.join("dest/theirs")).unwrap(), b"keep");
    assert!(t.join("dest.ava-part/d/0").exists());
    // the rename's own errno is in the message
    let ev = r.events();
    assert!(
        ev.contains("msg ") && {
            let l = ev.to_lowercase();
            l.contains("not empty") || l.contains("exists")
        },
        "{ev}"
    );
}
