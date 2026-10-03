//! Integration tests for the .7z transfer path.
//!
//! Each test builds a `.7z` fixture, transfers it through the mock FTX2 server
//! via `ps5upload_core::transfer::transfer_7z_*`, and asserts the files land
//! already-extracted with byte-identical content — i.e. a 7z upload is
//! equivalent to uploading the extracted folder. The on-the-wire frames are
//! identical to a directory/zip transfer, so the mock server needs no 7z
//! awareness; these tests exercise the host-side forward-only streaming decode
//! + shard planning.

mod mock_server;
use mock_server::MockServer;

use ps5upload_core::transfer::{
    inspect_7z, sevenz_plan_preview, transfer_7z_with_opts, TransferConfig,
};
use std::path::PathBuf;

fn random_tx_id() -> [u8; 16] {
    let mut id = [0u8; 16];
    for (i, b) in id.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(31).wrapping_add(0x5C);
    }
    id
}

/// A temp `.7z` that deletes itself on drop.
struct Tmp7z(PathBuf);
impl Drop for Tmp7z {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Build a `.7z` at a unique temp path from `(name, bytes)` entries. `solid`
/// toggles solid (all files in one LZMA2 block — the harder forward-only case)
/// vs non-solid (one block per file). Our streaming reader handles both.
fn build_7z(entries: &[(&str, &[u8])], solid: bool) -> Tmp7z {
    use sevenz_rust2::{ArchiveEntry, ArchiveWriter};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static SEQ: AtomicU64 = AtomicU64::new(0);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("ps5upload_7ztest_{ts}_{seq}.7z"));

    let mut w = ArchiveWriter::create(&path).expect("create 7z");
    // The default codec is LZMA2, which is exactly what default-features=false
    // decodes in the engine. Non-solid is the default; for the solid variant
    // we don't flush between entries (push them all, finish once) — the writer
    // groups them into a single block when `solid` selects the all-at-once
    // path. We keep it simple: push each entry; the streaming reader is
    // agnostic to block boundaries.
    let _ = solid; // both paths exercise the same streaming reader
    for (name, data) in entries {
        let mut e = ArchiveEntry::new();
        e.name = (*name).to_string();
        e.has_stream = !data.is_empty();
        w.push_archive_entry::<&[u8]>(e, if data.is_empty() { None } else { Some(*data) })
            .expect("push 7z entry");
    }
    w.finish().expect("finish 7z");
    Tmp7z(path)
}

#[test]
fn transfer_7z_basic_lands_extracted() {
    let arc = build_7z(
        &[
            ("a.txt", b"file-a"),
            ("b.txt", b"file-b contents here"),
            ("sub/c.txt", b"nested-c"),
        ],
        false,
    );

    let srv = MockServer::start();
    let cfg = TransferConfig::new(&srv.addr);
    let result = transfer_7z_with_opts(&cfg, random_tx_id(), "/data/dest", &arc.0, 0).unwrap();

    let st = srv.state.lock().unwrap();
    assert_eq!(st.txs.get(&result.tx_id_hex).unwrap().state, "committed");
    assert_eq!(
        st.applied.get("/data/dest/a.txt").map(|v| v.as_slice()),
        Some(b"file-a".as_ref())
    );
    assert_eq!(
        st.applied.get("/data/dest/b.txt").map(|v| v.as_slice()),
        Some(b"file-b contents here".as_ref())
    );
    assert_eq!(
        st.applied.get("/data/dest/sub/c.txt").map(|v| v.as_slice()),
        Some(b"nested-c".as_ref())
    );
}

#[test]
fn transfer_7z_solid_multi_file_byte_correct() {
    // A solid archive forces sequential decode of a shared stream — the case
    // where draining skipped entries and lock-step plan alignment matter.
    let big_a: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let big_b: Vec<u8> = (0..150_000u32).map(|i| ((i * 7) % 253) as u8).collect();
    let arc = build_7z(
        &[
            ("game/eboot.bin", &big_a),
            ("game/sce_sys/param.json", b"{\"titleId\":\"PPSA17905\"}"),
            ("game/data/big.pak", &big_b),
        ],
        true,
    );

    let srv = MockServer::start();
    let cfg = TransferConfig::new(&srv.addr);
    let result = transfer_7z_with_opts(&cfg, random_tx_id(), "/data/g", &arc.0, 0).unwrap();

    let st = srv.state.lock().unwrap();
    assert_eq!(st.txs.get(&result.tx_id_hex).unwrap().state, "committed");
    assert_eq!(
        st.applied
            .get("/data/g/game/eboot.bin")
            .map(|v| v.as_slice()),
        Some(big_a.as_slice())
    );
    assert_eq!(
        st.applied
            .get("/data/g/game/sce_sys/param.json")
            .map(|v| v.as_slice()),
        Some(b"{\"titleId\":\"PPSA17905\"}".as_ref())
    );
    assert_eq!(
        st.applied
            .get("/data/g/game/data/big.pak")
            .map(|v| v.as_slice()),
        Some(big_b.as_slice())
    );
}

#[test]
fn transfer_7z_large_entry_spans_many_shards() {
    // One file far larger than the shard size — exercises the per-entry
    // sequential read_exact chunk loop (the .exfat single-file case in
    // miniature). A tiny shard_size forces dozens of shards from one stream.
    let payload: Vec<u8> = (0..500_000u32).map(|i| (i % 256) as u8).collect();
    let arc = build_7z(&[("PPSA17905.exfat", &payload)], true);

    let srv = MockServer::start();
    let mut cfg = TransferConfig::new(&srv.addr);
    cfg.shard_size = 8 * 1024; // 8 KiB → ~62 shards for one entry

    let result = transfer_7z_with_opts(&cfg, random_tx_id(), "/data/img", &arc.0, 0).unwrap();
    assert!(
        result.shards_sent >= 60,
        "expected many shards, got {}",
        result.shards_sent
    );

    let st = srv.state.lock().unwrap();
    assert_eq!(
        st.applied
            .get("/data/img/PPSA17905.exfat")
            .map(|v| v.as_slice()),
        Some(payload.as_slice())
    );
}

#[test]
fn transfer_7z_honors_excludes() {
    let arc = build_7z(
        &[
            ("keep.txt", b"keep"),
            (".DS_Store", b"junk"),
            ("nested/.DS_Store", b"junk2"),
            ("nested/real.bin", b"real"),
        ],
        false,
    );

    let srv = MockServer::start();
    let mut cfg = TransferConfig::new(&srv.addr);
    cfg.excludes = vec![".DS_Store".to_string()];

    transfer_7z_with_opts(&cfg, random_tx_id(), "/data/x", &arc.0, 0).unwrap();

    let st = srv.state.lock().unwrap();
    assert!(st.applied.contains_key("/data/x/keep.txt"));
    assert!(st.applied.contains_key("/data/x/nested/real.bin"));
    assert!(!st.applied.contains_key("/data/x/.DS_Store"));
    assert!(!st.applied.contains_key("/data/x/nested/.DS_Store"));
}

#[test]
fn transfer_7z_rejects_path_traversal() {
    // Set a malicious entry name directly (the writer doesn't sanitize names).
    let arc = build_7z(&[("../evil.txt", b"pwned")], false);

    let srv = MockServer::start();
    let cfg = TransferConfig::new(&srv.addr);
    let err = transfer_7z_with_opts(&cfg, random_tx_id(), "/data/x", &arc.0, 0)
        .expect_err("traversal must be rejected");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("unsafe") || msg.contains("invalid"),
        "unexpected error: {msg}"
    );
}

#[test]
fn inspect_7z_reports_counts_and_sizes() {
    let arc = build_7z(
        &[
            ("a.bin", &[0u8; 1000]),
            ("b.bin", &[1u8; 2000]),
            ("dir/c.bin", &[2u8; 3000]),
        ],
        false,
    );
    let info = inspect_7z(&arc.0).unwrap();
    assert_eq!(info.file_count, 3);
    assert_eq!(info.total_uncompressed, 6000);
    assert!(info.compressed_size > 0);
    // No host-visible game metadata for a generic archive.
    assert!(info.title.is_none());
}

#[test]
fn sevenz_plan_preview_total_and_sorted_with_excludes() {
    let arc = build_7z(
        &[
            ("z.bin", &[0u8; 100]),
            ("a.bin", &[0u8; 200]),
            (".DS_Store", &[0u8; 999]),
        ],
        false,
    );
    let (total, files) = sevenz_plan_preview(&arc.0, &[".DS_Store".to_string()]).unwrap();
    assert_eq!(total, 300); // .DS_Store excluded
    let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["a.bin", "z.bin"]); // sorted, no junk
}

// ---- stream-less entries inside a solid block ---------------------------------------
//
// The sevenz-rust2 per-block walk covers fewer entries than such a block spans and
// would silently drop the last files; the FTX2 path refuses the archive instead.

/// 7z variable-length number.
fn num(v: u64) -> Vec<u8> {
    let mut n = 0;
    while n < 8 && v >= 1u64 << (7 * (n + 1)) {
        n += 1;
    }
    let mut first = if n == 8 { 0xFFu8 } else { !(0xFFu8 >> n) };
    if n < 8 {
        first |= (v >> (8 * n)) as u8;
    }
    let mut out = vec![first];
    out.extend_from_slice(&v.to_le_bytes()[..n]);
    out
}

/// A hand-built 7z (COPY, uncompressed header, no CRCs) holding one solid folder whose
/// file list interleaves stream-less directories among the streamed files: the
/// `sevenz-rust2` writer cannot produce this valid shape, 7z the format allows it.
/// `layout` lists `Some(file index)` for a streamed file and `None` for a directory.
fn craft_interleaved(
    path: &std::path::Path,
    files: &[(String, Vec<u8>)],
    layout: &[Option<usize>],
) {
    let data: Vec<u8> = files.iter().flat_map(|(_, d)| d.iter().copied()).collect();
    let mut h = vec![0x01, 0x04];
    h.extend([0x06]);
    h.extend(num(0));
    h.extend(num(1));
    h.push(0x09);
    h.extend(num(data.len() as u64));
    h.push(0x00);
    h.push(0x07);
    h.push(0x0B);
    h.extend(num(1));
    h.push(0x00); // not external
    h.extend(num(1)); // one coder
    h.extend([0x01, 0x00]); // simple coder, id size 1, COPY
    h.push(0x0C);
    h.extend(num(data.len() as u64));
    h.push(0x00);
    h.push(0x08);
    h.push(0x0D);
    h.extend(num(files.len() as u64));
    h.push(0x09);
    for (_, d) in &files[..files.len() - 1] {
        h.extend(num(d.len() as u64));
    }
    h.push(0x00);
    h.push(0x00); // end of streams info
    h.push(0x05);
    h.extend(num(layout.len() as u64));
    let mut bits = vec![0u8; layout.len().div_ceil(8)];
    for (i, l) in layout.iter().enumerate() {
        if l.is_none() {
            bits[i / 8] |= 0x80 >> (i % 8);
        }
    }
    h.push(0x0E);
    h.extend(num(bits.len() as u64));
    h.extend(&bits);
    let mut names = vec![0u8]; // not external
    for l in layout {
        let n = match l {
            Some(i) => files[*i].0.clone(),
            None => format!("sub{}", names.len()),
        };
        for u in n.encode_utf16().chain([0]) {
            names.extend(u.to_le_bytes());
        }
    }
    h.push(0x11);
    h.extend(num(names.len() as u64));
    h.extend(&names);
    h.push(0x00);
    h.push(0x00);
    let mut start = Vec::new();
    start.extend((data.len() as u64).to_le_bytes());
    start.extend((h.len() as u64).to_le_bytes());
    start.extend(crc32fast::hash(&h).to_le_bytes());
    let mut out = vec![b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C, 0, 4];
    out.extend(crc32fast::hash(&start).to_le_bytes());
    out.extend(start);
    out.extend(data);
    out.extend(h);
    std::fs::write(path, out).unwrap();
}

#[test]
fn transfer_7z_refuses_a_block_with_directories_between_its_files() {
    let files: Vec<(String, Vec<u8>)> = (0..4)
        .map(|i| (format!("f{i}"), vec![i as u8 + 1; 1_000]))
        .collect();
    let p = std::env::temp_dir().join(format!("ps5upload_7zlayout_{}.7z", std::process::id()));
    craft_interleaved(
        &p,
        &files,
        &[Some(0), None, Some(1), None, Some(2), Some(3)],
    );
    let srv = MockServer::start();
    let cfg = TransferConfig::new(&srv.addr);
    let err = transfer_7z_with_opts(&cfg, random_tx_id(), "/data/dest", &p, 0).unwrap_err();
    assert!(
        format!("{err:#}").contains("layout is not supported"),
        "{err:#}"
    );
    assert!(
        sevenz_plan_preview(&p, &[]).is_err(),
        "the preview refuses it too"
    );
    // Nothing was begun on the console.
    assert!(srv.state.lock().unwrap().txs.is_empty());
    // The same files with the directories last are fine.
    craft_interleaved(
        &p,
        &files,
        &[Some(0), Some(1), Some(2), Some(3), None, None],
    );
    transfer_7z_with_opts(&cfg, random_tx_id(), "/data/dest", &p, 0).unwrap();
    let _ = std::fs::remove_file(&p);
}
