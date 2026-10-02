#![cfg(unix)]
use ava1::manifest::{walk, Entry, Manifest};
use ava1::source::LocalSource;
use ava1::wire::Message;
use ava1_ctest::*;

fn e(path: &str, kind: u8, size: u64) -> Entry {
    Entry {
        kind,
        mode: 0o644,
        size,
        mtime: 1_700_000_000,
        path: path.into(),
        root: None,
    }
}

#[test]
fn c_rebuilds_rust_pages_with_the_same_hash() {
    let entries: Vec<Entry> = (0..3000)
        .map(|i| {
            e(
                &format!("d{}/f{i:05}{}", i % 7, "x".repeat(i % 200)),
                ava1::gen::ENTRY_FILE,
                i as u64 * 3,
            )
        })
        .collect();
    let m = Manifest { entries };
    let pages: Vec<Vec<u8>> = m
        .pages([5; 16])
        .iter()
        .map(|p| p.to_bytes().unwrap())
        .collect();
    let (rc, hash, n, bytes) = c_mstore_from_pages(&pages);
    assert_eq!(rc, 0);
    assert_eq!(
        (hash, n, bytes),
        (m.hash(), m.entries.len() as u32, m.bytes())
    );
}

#[test]
fn c_manifest_refuses_escaping_paths() {
    for bad in ["../x", "/abs", "a/../b", "a//b", "a/./b", "a/", ""] {
        let m = Manifest {
            entries: vec![e("ok", 0, 1), e(bad, 0, 1)],
        };
        // Encode by hand: Manifest::pages would not refuse, the receiver must.
        let pages: Vec<Vec<u8>> = m
            .pages([1; 16])
            .iter()
            .map(|p| p.to_bytes().unwrap())
            .collect();
        let (rc, ..) = c_mstore_from_pages(&pages);
        assert_eq!(rc, -20, "{bad:?} must be refused with AVA1_E_BADPATH");
    }
}

#[test]
fn c_and_rust_walk_a_tree_identically() {
    let d = std::env::temp_dir().join(format!("ava1-cwalk-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    for p in ["a/b/c", "a2", "a/b2", "z"] {
        std::fs::create_dir_all(d.join(p)).unwrap();
    }
    for (p, n) in [
        ("a/b/c/f1", 10),
        ("a/x", 0),
        ("a2/y", 3),
        ("top", 7),
        ("a/b2/é", 1),
    ] {
        std::fs::write(d.join(p), vec![1u8; n]).unwrap();
    }
    let m = walk(&LocalSource::new(d.clone()), &|_: &str| false).unwrap();
    let (rc, hash, n) = c_mstore_walk(&d);
    assert_eq!(rc, 0);
    assert_eq!((hash, n), (m.hash(), m.entries.len() as u32));
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn c_threads_start_with_their_own_stack() {
    // Not a smoke test: the C side reports the stack it actually ran on and fails unless
    // it is within [200 KiB, 256 KiB + 4 KiB] (64 KiB of ceiling on macOS, whose pthreads
    // pad the reported allocation) — the default pthread stack (512 KiB on macOS and
    // Linux) must not pass. See ruling 4.
    assert_eq!(c_thread_smoke().0, 0);
}
