#![cfg(unix)]
use ava1::gen::{FileRange, FileRun, JnlBatch, JnlOpen, RootItem};
use ava1::journal::{Journal, Record, State};
use ava1_ctest::*;
use std::path::PathBuf;

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ava1-cjnl-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The exact text `ava1_test_journal_dump` produces for the same state.
fn c_style_dump(st: &State) -> String {
    let mut s = String::new();
    for f in &st.done {
        s.push_str(&format!("done {f}\n"));
    }
    for (f, r) in &st.ranges {
        for (a, b) in r.iter() {
            s.push_str(&format!("range {f} {a} {b}\n"));
        }
    }
    for (f, r) in &st.roots {
        s.push_str(&format!("root {f} {:02x}\n", r[0]));
    }
    match st.finished {
        Some(x) => s.push_str(&format!("finished={x}")),
        None => s.push_str("finished=none"),
    }
    s
}

#[test]
fn c_replays_a_rust_journal() {
    let d = tmp("r2c");
    let o = JnlOpen {
        job_id: [3; 16],
        manifest_hash: [4; 32],
        kind: 1,
        flags: 0,
        staged: 0,
        root: "/data/t".into(),
    };
    let mut j = Journal::create(&d, &o).unwrap();
    let mut st = State::default();
    st.apply(&Record::Open(o.clone()));
    for i in 0..50u32 {
        let r = Record::Batch(JnlBatch {
            files: vec![FileRun {
                first: i * 2,
                count: 1,
            }],
            ranges: vec![FileRange {
                file_id: 1000,
                offset: (i as u64) << 20,
                len: 1 << 20,
            }],
            roots: vec![RootItem {
                file_id: 1000 + i,
                root: [i as u8; 32],
            }],
        });
        st.apply(&r);
        j.append(&r).unwrap();
    }
    j.append(&Record::Reset(4)).unwrap();
    st.apply(&Record::Reset(4));
    drop(j);
    assert_eq!(c_journal_dump(&d), c_style_dump(&st));
}

#[test]
fn c_journal_torn_tail_is_ignored() {
    let d = tmp("c2r");
    assert_eq!(c_journal_write_sample(&d), 0); // open + files 0..9 + reset 3 + done 0
    let (_, recs) = Journal::open(&d).unwrap();
    let mut st = State::default();
    for r in &recs {
        st.apply(r);
    }
    assert_eq!(
        st.done.iter().copied().collect::<Vec<_>>(),
        vec![0, 1, 2, 4, 5, 6, 7, 8, 9]
    );
    assert_eq!(st.finished, Some(0));
    let p = d.join("journal");
    let mut b = std::fs::read(&p).unwrap();
    b.truncate(b.len() - 2);
    std::fs::write(&p, &b).unwrap();
    assert!(c_journal_dump(&d).ends_with("finished=none"));
}
