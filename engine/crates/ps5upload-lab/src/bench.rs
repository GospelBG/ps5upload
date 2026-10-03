//! Benchmark corpora, corpus statistics and result records (Tasks 26–28).
//!
//! Every item here is `pub` (C14): Task 27's scenarios import the corpora and the
//! statistics, and compare AVA1 against FTX2 on identical inputs.

use std::collections::HashSet;
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};

/// The dedup class is `len < SMALL` — the corpus generators' `last_small` rule. One
/// boundary table shared with the histogram buckets (C4), so the two cannot drift.
pub const SMALL: u64 = 64 << 10;

/// Histogram buckets (C4's table). A size falls into the **first** bucket whose range
/// contains it, so exactly 16 MiB lands in bucket 3 even though bucket 4's lower bound
/// re-states 16 MiB.
const BUCKETS: [(u64, u64); 5] = [
    (0, 4096),
    (4097, SMALL - 1),
    (SMALL, 1 << 20),
    ((1 << 20) + 1, 16 << 20),
    (16 << 20, u64::MAX),
];

/// Compressible-fraction sample budget per file: 1 MiB (A4).
const SAMPLE: u64 = 1 << 20;

/// A4: files larger than the budget are sampled head+middle+tail — three chunks of
/// 340 KiB, not a 1 MiB prefix (game data often starts with incompressible tables and
/// goes repetitive later, or vice versa). 3 × 340 KiB = 1 020 KiB ≤ 1 MiB.
const SAMPLE_CHUNK: u64 = 340 << 10;

/// `bench-corpus ppsa01342`'s file count (C5: the command passes this; tests pass
/// hundreds, which at scale 1.0 keeps the corpus under ~100 MiB).
pub const PPSA_COUNT: u64 = 223_000;

/// ~40 files per synthetic-game directory (1–8 levels deep).
pub const PER_DIR: u64 = 40;

/// A1: `corpus_listing` drops this marker at the corpus root. A listing carries no
/// bytes, so a duplicate-by-content ratio is not measurable for such a corpus —
/// `stats()` then reports `duplicate_ratio` as `NaN` (which serde_json writes as
/// `null`), never as a false 0 that Tasks 27/28 would treat as data. The listing
/// format itself is not extended for v1 (known limitation, ledger).
pub const LISTING_MARKER: &str = ".ps5upload-lab-listing";

/// The frozen record schema version (A3). Every record carries `schema: 1` plus the
/// identity fields; changing the schema means bumping this and saying what changed,
/// never silently.
// FIXME-ish note, not a lint hack: SCHEMA is consumed by the record-writing commands
// (the follow-up calibrate arm and Task 27's runner) and by the tests; until then the
// bin build sees it unused.
#[allow(dead_code)]
pub const SCHEMA: u64 = 1;

/// One corpus's statistics. Serializes (T27/T28 record it); `duplicate_ratio` is `NaN`
/// — JSON `null` — for a corpus reproduced from a listing (A1).
#[derive(Debug, Clone, serde::Serialize)]
pub struct Stats {
    pub files: u64,
    pub bytes: u64,
    /// File counts per `BUCKETS` (labels: `histogram_labels()`).
    pub histogram: [u64; 5],
    /// Byte-weighted share of the sampled bytes whose deflate output is ≤ 90 % of the
    /// input (C10) — the number the deferred zstd work needs. `0.0` when nothing was
    /// sampled (an all-empty corpus; a zero-length file contributes nothing).
    pub compressible_fraction: f64,
    /// Files with `len < SMALL` whose whole-file BLAKE3 was seen before, over all such
    /// files (C3/C4). `NaN` (JSON `null`) for listing corpora (A1).
    pub duplicate_ratio: f64,
}

/// Human labels for the five histogram buckets, matching `BUCKETS`.
pub fn histogram_labels() -> [&'static str; 5] {
    [
        "≤ 4 KiB",
        "4–64 KiB",
        "64 KiB–1 MiB",
        "1–16 MiB",
        "> 16 MiB",
    ]
}

/// Deterministic incompressible bytes: a BLAKE3 XOF keyed by a spread seed.
///
/// The key spreads the seed explicitly — `seed.to_le_bytes()` cycled to 32 bytes, not
/// `[seed as u8; 32]` — so two different seeds can never collide even if a future
/// "optimisation" drops the extra `update` (C9). The seed travels in the file name
/// (`large-{gib}g.bin`), so a corpus file documents its own content.
pub fn write_random(p: &Path, len: u64, seed: u64) -> io::Result<()> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let mut key = [0u8; 32];
    for (k, b) in key.iter_mut().zip(seed.to_le_bytes().iter().cycle()) {
        *k = *b;
    }
    let mut x = blake3::Hasher::new_keyed(&key)
        .update(&seed.to_le_bytes())
        .finalize_xof();
    let mut f = io::BufWriter::with_capacity(1 << 20, std::fs::File::create(p)?);
    let mut buf = vec![0u8; 1 << 20];
    let mut left = len;
    while left > 0 {
        let n = left.min(buf.len() as u64) as usize;
        x.fill(&mut buf[..n]);
        f.write_all(&buf[..n])?;
        left -= n as u64;
    }
    f.flush()
}

/// Deterministic repetitive bytes: a 4 KiB pattern derived from the seed, so deflate
/// shrinks it hard — the corpus's compressible 40 %.
fn write_repetitive(p: &Path, len: u64, seed: u64) -> io::Result<()> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let pat: Vec<u8> = (0..4096u64).map(|i| ((i * 7 + seed) % 61) as u8).collect();
    let mut f = io::BufWriter::new(std::fs::File::create(p)?);
    let mut left = len;
    while left > 0 {
        let n = left.min(pat.len() as u64) as usize;
        f.write_all(&pat[..n])?;
        left -= n as u64;
    }
    f.flush()
}

/// SplitMix64 for shapes (sizes, paths, directory depths) — never content.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next() % (hi - lo + 1)
    }
}

/// 60 % incompressible, 40 % repetitive — content, not shape (the plan's mix).
fn write_one(p: &Path, len: u64, seed: u64, rng: &mut Rng) -> io::Result<()> {
    if rng.next() % 10 < 6 {
        write_random(p, len, seed)
    } else {
        write_repetitive(p, len, seed)
    }
}

/// A duplicate is a byte-for-byte copy of an earlier file. The copy path never
/// computes or receives a length (C6), so no refactor can turn a duplicate into an
/// empty file.
fn write_duplicate(p: &Path, src: &Path) -> io::Result<()> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::copy(src, p)?;
    Ok(())
}

/// One incompressible file, `large-{gib}g.bin`. The seed **is** the GiB count, so the
/// name documents the content (C9): the same command reproduces the same bytes, and
/// two sizes never collide.
pub fn corpus_large(dir: &Path, gib: u64) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    write_random(&dir.join(format!("large-{gib}g.bin")), gib << 30, gib)
}

/// N files of 1–64 KiB, 64 per directory (`d0000/f000000.dat` …). Every 50th file is a
/// byte-identical copy of the most recently written file (a ~2 % duplicate ratio, so
/// the ratio is measurable). `last_small` updates only for files actually written
/// (C6); a duplicate of a duplicate is still byte-identical by construction.
pub fn corpus_tiny(dir: &Path, n: u64) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut rng = Rng(11);
    let mut last_small: Option<PathBuf> = None;
    for i in 0..n {
        let p = dir.join(format!("d{:04}/f{i:06}.dat", i / 64));
        let src = if rng.next().is_multiple_of(50) {
            last_small.clone()
        } else {
            None
        };
        match src {
            Some(src) => write_duplicate(&p, &src)?,
            None => {
                write_one(&p, rng.range(1024, 64 * 1024), i, &mut rng)?;
                last_small = Some(p);
            }
        }
    }
    Ok(())
}

/// The synthetic PPSA01342 game-folder shape (C5: `count` parametrises the plan's
/// hard-coded 223 000 — the command passes `PPSA_COUNT`, tests pass a few hundred):
/// 70 % ≤ 4 KiB, 20 % 4–64 KiB, 9 % 64 KiB–1 MiB, 1 % 1–16 MiB, every size multiplied
/// by `scale` (1.0 ≈ 30 GB); directories 1–8 levels deep under `Image0`, at most
/// `PER_DIR` files per directory. Content: 60 % incompressible / 40 % repetitive,
/// and ~2 % of small files are byte-identical copies of an earlier small file.
pub fn corpus_ppsa01342(dir: &Path, scale: f64, count: u64) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut rng = Rng(1342);
    let mut last_small: Option<PathBuf> = None;
    let mut path_stack: Vec<String> = vec!["Image0".into()];
    for i in 0..count {
        if i % PER_DIR == 0 && i > 0 {
            // A fresh full path every PER_DIR files (C5's ≤ 40-files-per-directory
            // cadence): the plan's truncate-and-grow tree reuses shared parents
            // whenever the depth draw lands on an ancestor's level, so a directory
            // could accumulate several windows' worth of files. Fresh names at
            // every level keep the shape (Image0 root, 1–8 levels) and make every
            // directory hold at most PER_DIR files. A depth-1 draw after the first
            // window would reuse Image0 itself, so it goes one level deeper.
            let mut depth = rng.range(1, 8) as usize;
            if depth == 1 {
                depth = 2;
            }
            path_stack.clear();
            path_stack.push("Image0".into());
            for _ in 1..depth {
                path_stack.push(format!("dir{:05}", rng.next() % 100_000));
            }
        }
        let bucket = rng.next() % 100;
        let len = match bucket {
            0..=69 => rng.range(0, 4 << 10),
            70..=89 => rng.range(4 << 10, 64 << 10),
            90..=98 => rng.range(64 << 10, 1 << 20),
            _ => rng.range(1 << 20, 16 << 20),
        };
        let len = ((len as f64) * scale) as u64;
        let p = dir.join(path_stack.join("/")).join(format!("f{i:06}.bin"));
        let src = if len < SMALL && rng.next().is_multiple_of(50) {
            last_small.clone()
        } else {
            None
        };
        match src {
            Some(src) => write_duplicate(&p, &src)?,
            None => {
                write_one(&p, len, i, &mut rng)?;
                if len < SMALL {
                    last_small = Some(p);
                }
            }
        }
    }
    Ok(())
}

/// Reproduces a real game listing exactly: one `<size> <path>` line per file, sizes
/// multiplied by `scale`, synthetic bytes (the 60/40 mix). Rejects absolute paths and
/// `..`, a duplicate relative path, a path with a trailing `/`, and an empty path;
/// skips blank lines and `#` comments (C7). Fabricates no duplicates — a listing
/// carries no bytes, so the duplicate ratio is not measurable: the generator drops
/// `LISTING_MARKER` and `stats()` reports `duplicate_ratio` as `NaN`, never 0 (A1).
pub fn corpus_listing(dir: &Path, listing: &Path, scale: f64) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut rng = Rng(5);
    let mut seen: HashSet<String> = HashSet::new();
    let text = std::fs::read_to_string(listing)?;
    for (i, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue; // C7: blank lines and comments are skipped
        }
        let bad = |what: String| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("line {}: {what}", i + 1),
            )
        };
        let Some((size, path)) = trimmed.split_once(' ') else {
            return Err(bad(format!("expected `<size> <path>`, got {trimmed:?}")));
        };
        let size: u64 = size
            .trim()
            .parse()
            .map_err(|_| bad(format!("bad size {size:?}")))?;
        let path = path.trim();
        let rel = Path::new(path);
        if rel.as_os_str().is_empty() {
            return Err(bad("empty path".into()));
        }
        if rel.is_absolute()
            || rel
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(bad(format!("path must be relative without `..`: {path}")));
        }
        if path.ends_with('/') {
            return Err(bad(format!("trailing `/`: {path}")));
        }
        if !seen.insert(path.to_string()) {
            return Err(bad(format!("duplicate path: {path}")));
        }
        write_one(
            &dir.join(rel),
            ((size as f64) * scale) as u64,
            i as u64,
            &mut rng,
        )?;
    }
    std::fs::write(dir.join(LISTING_MARKER), listing.display().to_string())?;
    Ok(())
}

/// A2: the CLI's generator guard. Refuses to write into a non-empty directory unless
/// `force` — a generator pointed at a real folder is a data-loss bug waiting to
/// happen. With `--force`, logs what is being overwritten. Never deletes the target
/// directory itself; the generators write into it. A missing directory is created.
pub fn ensure_writable_target(dir: &Path, force: bool) -> io::Result<()> {
    match std::fs::read_dir(dir) {
        Ok(rd) => {
            let entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
            if entries.is_empty() {
                return Ok(());
            }
            if !force {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "{} is not empty ({} entries); pass --force to write anyway",
                        dir.display(),
                        entries.len()
                    ),
                ));
            }
            eprintln!(
                "overwriting {} existing entries in {} (e.g. {})",
                entries.len(),
                dir.display(),
                entries[0].path().display()
            );
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => std::fs::create_dir_all(dir),
        Err(e) => Err(e),
    }
}

/// Walks `dir` with an explicit stack and computes the statistics.
///
/// - C8: uses `symlink_metadata`, skips symlinks (counted, reported on stderr — a
///   real game folder may have them; the generators never create one), and reads
///   samples with `Read::take(len)` so a file that shrinks between `metadata` and
///   `open` cannot fail the whole run with `read_exact`'s error.
/// - C10: the compressible fraction is a **byte-weighted** sample, not a file count.
/// - A4: the sample is head+middle+tail (3 × 340 KiB) for files > 1 MiB, the whole
///   file otherwise.
/// - C3: the dedup digest reads the whole file — the 1 MiB budget applies only to the
///   compressible-fraction sample.
///
/// Single-threaded and that is fine (C13): a 223 000-file corpus reads ~15 GiB of
/// samples and deflates them — minutes, not seconds.
pub fn stats(dir: &Path) -> io::Result<Stats> {
    let mut st = Stats {
        files: 0,
        bytes: 0,
        histogram: [0; 5],
        compressible_fraction: 0.0,
        duplicate_ratio: 0.0,
    };
    let (mut sampled, mut compressible, mut small, mut dups, mut symlinks) =
        (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut seen: HashSet<[u8; 32]> = HashSet::new();
    let mut from_listing = false;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)? {
            let e = e?;
            let p = e.path();
            let m = std::fs::symlink_metadata(&p)?;
            let ft = m.file_type();
            if ft.is_dir() {
                stack.push(p);
                continue;
            }
            if ft.is_symlink() {
                symlinks += 1;
                continue;
            }
            if p == dir.join(LISTING_MARKER) {
                from_listing = true; // A1: the ratio is not measurable for this corpus
                continue;
            }
            let len = m.len();
            st.files += 1;
            st.bytes += len;
            let bucket = BUCKETS
                .iter()
                .position(|(lo, hi)| len >= *lo && len <= *hi)
                .expect("BUCKETS covers every u64");
            st.histogram[bucket] += 1;

            let mut f = std::fs::File::open(&p)?;

            // Compressible-fraction sample: head+middle+tail, or the whole file.
            let mut sample = Vec::with_capacity(len.min(SAMPLE) as usize);
            if len <= SAMPLE {
                io::Read::take(&mut f, len).read_to_end(&mut sample)?;
            } else {
                let mid = len / 2 - SAMPLE_CHUNK / 2;
                for at in [0u64, mid, len - SAMPLE_CHUNK] {
                    f.seek(io::SeekFrom::Start(at))?;
                    io::Read::take(&mut f, SAMPLE_CHUNK).read_to_end(&mut sample)?;
                }
            }
            let mut enc =
                flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
            enc.write_all(&sample)?;
            let out = enc.finish()?;
            sampled += sample.len() as u64;
            if !sample.is_empty() && (out.len() as f64) <= sample.len() as f64 * 0.9 {
                compressible += sample.len() as u64;
            }

            // Dedup digest: the whole file, not the sample (C3).
            if len < SMALL {
                small += 1;
                f.seek(io::SeekFrom::Start(0))?;
                let mut full = Vec::with_capacity(len as usize);
                io::Read::take(&mut f, len).read_to_end(&mut full)?;
                if !seen.insert(*blake3::hash(&full).as_bytes()) {
                    dups += 1;
                }
            }
        }
    }
    st.compressible_fraction = if sampled == 0 {
        0.0
    } else {
        compressible as f64 / sampled as f64
    };
    st.duplicate_ratio = if from_listing {
        f64::NAN
    } else if small == 0 {
        0.0
    } else {
        dups as f64 / small as f64
    };
    if symlinks > 0 {
        eprintln!("stats: skipped {symlinks} symlink(s) in {}", dir.display());
    }
    Ok(st)
}

/// The identity fields every record carries (A3): a machine tag (the host OS/arch, or
/// `PS5UPLOAD_BENCH_MACHINE` to disambiguate two identical hosts) and a start
/// timestamp (unix epoch seconds).
// Consumed via `record_envelope` (below); unused until the first record-writing
// command lands.
#[allow(dead_code)]
fn bench_machine() -> String {
    std::env::var("PS5UPLOAD_BENCH_MACHINE")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH))
}

#[allow(dead_code)] // see bench_machine
fn started_at() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The frozen record envelope (A3, coordinator ruling 2026-10-02): every record starts
/// with `schema: 1` and the identity fields `machine`, `started_at`, `corpus`, `seed`,
/// `protocol`; the caller adds its kind-specific measured fields, with units in the
/// field names (`bytes`, `ms`, `files_per_s`). Tasks 27/28 consume the same file, so a
/// schema change means bumping `SCHEMA` and saying what changed — never silently.
///
/// Kind-specific fields, frozen now so the follow-up arms only append:
/// - `"calibrate"` (added with Task 26b): `console`, `dir`, `files`,
///   `size_bytes`, and `points: [{workers, files_per_s, create_ms, fsync_ms}]`
///   — the wire reports µs; the record converts to `ms`.
/// - `"bench"` (Task 27): the scenario record, same envelope.
// Consumed by the same commands as `record` (below).
#[allow(dead_code)]
pub fn record_envelope(
    kind: &str,
    corpus: Option<&str>,
    seed: Option<u64>,
    protocol: &str,
) -> serde_json::Value {
    serde_json::json!({
        "schema": SCHEMA,
        "kind": kind,
        "machine": bench_machine(),
        "started_at": started_at(),
        "corpus": corpus,
        "seed": seed,
        "protocol": protocol,
    })
}

/// Appends one JSON record as a single line to `out` (C2/C15: the path is the
/// caller's — Task 27's runner exposes `--out FILE`, and the lab's default is the lab
/// data dir's `bench-results.jsonl`; two concurrent runs must not overwrite each
/// other). The whole line goes out in one `write_all` on an `O_APPEND` handle, so a
/// concurrent run can never interleave a partial line.
// Consumed by the first record-writing command (the follow-up calibrate arm) and
// Task 27's runner; the tests exercise it today.
#[allow(dead_code)]
pub fn record(out: &Path, line: &serde_json::Value) -> io::Result<()> {
    if let Some(d) = out.parent() {
        std::fs::create_dir_all(d)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(out)?;
    let mut bytes = line.to_string();
    bytes.push('\n');
    f.write_all(bytes.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// C12: every test builds its corpus under the system temp dir with a
    /// process-specific component — not the repo, not `$HOME` — and removes it on
    /// drop. Tests never call `corpus_large` or `corpus_ppsa01342(…, 223_000)`.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(name: &str) -> TempDir {
            let d =
                std::env::temp_dir().join(format!("ps5upload-lab-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            TempDir(d)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_tiny_corpus_has_the_requested_shape() {
        let d = TempDir::new("bench-tiny");
        corpus_tiny(d.path(), 500).unwrap();
        let st = stats(d.path()).unwrap();
        assert_eq!(st.files, 500);
        assert!(
            st.bytes >= 500 * 1024 && st.bytes <= 500 * 64 * 1024,
            "bytes {}",
            st.bytes
        );
        assert!(
            st.duplicate_ratio > 0.0 && st.duplicate_ratio < 0.1,
            "duplicate_ratio {}",
            st.duplicate_ratio
        );
    }

    #[test]
    fn a_listing_is_reproduced_exactly() {
        let d = TempDir::new("bench-list");
        let bad = |text: &str, dir: &Path| {
            let l = d.path().join("listing.txt");
            std::fs::write(&l, text).unwrap();
            let out = dir.join("out");
            corpus_listing(&out, &l, 1.0)
        };
        // C7: an absolute path, `..`, a trailing `/` and a duplicate path are all
        // rejected; a blank line and a `#` comment are skipped.
        assert!(bad("5 /etc/passwd\n", &d.path().join("b1")).is_err());
        assert!(bad("5 ../escape\n", &d.path().join("b2")).is_err());
        assert!(bad("5 a/dir/\n", &d.path().join("b3")).is_err());
        assert!(bad("5 a/once\n5 a/once\n", &d.path().join("b4")).is_err());
        assert!(bad("5 a/\n", &d.path().join("b5")).is_err());

        let out = d.path().join("out");
        let l = d.path().join("listing.txt");
        std::fs::write(
            &l,
            "10 a/b/c.bin\n0 a/empty\n70000 sce_sys/param.json\n\n# a comment\n",
        )
        .unwrap();
        corpus_listing(&out, &l, 1.0).unwrap();
        assert_eq!(std::fs::metadata(out.join("a/b/c.bin")).unwrap().len(), 10);
        assert_eq!(std::fs::metadata(out.join("a/empty")).unwrap().len(), 0);
        assert_eq!(
            std::fs::metadata(out.join("sce_sys/param.json"))
                .unwrap()
                .len(),
            70000
        );
        // A1: the marker makes the duplicate ratio undefined, never a false 0, and
        // the marker itself is not counted as a corpus file.
        let st = stats(&out).unwrap();
        assert_eq!(st.files, 3);
        assert!(st.duplicate_ratio.is_nan(), "{:?}", st.duplicate_ratio);
    }

    #[test]
    fn compressible_fraction_tells_random_from_repetitive() {
        let d = TempDir::new("bench-cmp");
        std::fs::write(d.path().join("z"), vec![0u8; 1 << 20]).unwrap();
        write_random(&d.path().join("r"), 1 << 20, 1).unwrap();
        let st = stats(d.path()).unwrap();
        assert!(
            (st.compressible_fraction - 0.5).abs() < 0.01,
            "{}",
            st.compressible_fraction
        );
        // C10's explicit empty rule: a zero-length file contributes nothing, and an
        // all-empty corpus has fraction 0.0 (the plan's `0 <= 0` was accidentally
        // true).
        let e = TempDir::new("bench-cmp-empty");
        std::fs::File::create(e.path().join("a")).unwrap();
        std::fs::File::create(e.path().join("b")).unwrap();
        let st = stats(e.path()).unwrap();
        assert_eq!(st.compressible_fraction, 0.0);
    }

    #[test]
    fn stats_counts_duplicates_by_content() {
        let d = TempDir::new("bench-dup");
        // Two byte-identical files and one that shares the pair's first 64 bytes but
        // differs after — a prefix-based digest would miscount it (C3: the digest
        // covers the whole file).
        let mut a = vec![0x5au8; 1000];
        a[..64].copy_from_slice(&[0x11u8; 64]);
        let mut b = a.clone();
        b[900] ^= 0xff;
        std::fs::write(d.path().join("a1"), &a).unwrap();
        std::fs::write(d.path().join("a2"), &a).unwrap();
        std::fs::write(d.path().join("b"), &b).unwrap();
        let st = stats(d.path()).unwrap();
        assert_eq!(st.files, 3);
        assert_eq!(st.duplicate_ratio, 1.0 / 3.0);
    }

    #[test]
    fn the_bucket_table_and_the_dedup_class_agree() {
        let d = TempDir::new("bench-buckets");
        // The five pinned boundary sizes (C4), plus a 65536-byte twin: if the dedup
        // class wrongly used `<= SMALL`, the twin pair would lift the ratio off 0.
        let twin = vec![0x42u8; SMALL as usize];
        let mut i = 0u64;
        let mut put = |len: u64| {
            let p = d.path().join(format!("f{i}.bin"));
            i += 1;
            write_random(&p, len, i).unwrap(); // distinct content per size
            if len == SMALL {
                std::fs::write(&p, &twin).unwrap(); // the twin pair shares bytes
            }
        };
        put(4096);
        put(4097);
        put(65535);
        put(65536);
        put(65536); // the twin
        put(65537);
        let st = stats(d.path()).unwrap();
        // C4: 4096 → bucket 0; 4097 and 65535 → bucket 1; 65536 (both) and 65537 →
        // bucket 2. And the dedup class (`len < SMALL`) contains only the first
        // three, all distinct → ratio exactly 0.
        assert_eq!(st.histogram, [1, 2, 3, 0, 0]);
        assert_eq!(st.duplicate_ratio, 0.0);
    }

    #[test]
    fn the_ppsa_shape_matches_its_histogram() {
        // C5, coordinator ruling 2026-10-02: scale 1.0 so the pinned proportions are
        // real, exercising the actual writer path; 700 files keeps the corpus at
        // ≈ 98.6 MiB expected (70 % × 2 KiB + 20 % × 34 KiB + 9 % × 544 KiB + 1 % ×
        // 8.5 MiB per file) — at or under ~100 MiB, cheap for CI. At 700 files the
        // ≤ 4 KiB share has σ ≈ 1.7 points and the > 1 MiB share σ ≈ 0.4 points, so
        // the ± 10-point tolerances are far from the flake edge.
        // The seed is fixed (Rng(1342)), so the corpus is identical every run:
        // measured 92.3 MiB, histogram [491, 139, 61, 9, 0] → shares 70.14 % /
        // 1.29 % (2026-10-02), both well inside the pinned tolerances.
        let d = TempDir::new("bench-ppsa");
        corpus_ppsa01342(d.path(), 1.0, 700).unwrap();
        let st = stats(d.path()).unwrap();
        let small_share = st.histogram[0] as f64 / st.files as f64;
        let large_share = (st.histogram[3] + st.histogram[4]) as f64 / st.files as f64;
        assert!(
            (small_share - 0.70).abs() < 0.10,
            "≤4 KiB share {small_share}, histogram {:?}",
            st.histogram
        );
        assert!(
            (large_share - 0.01).abs() < 0.10,
            ">1 MiB share {large_share}, histogram {:?}",
            st.histogram
        );
        // Directory cadence and depth (C5): ≤ 40 files per directory, at most 8 levels.
        let mut stack = vec![d.path().to_path_buf()];
        let mut max_depth = 0usize;
        while let Some(dir) = stack.pop() {
            max_depth = max_depth.max(
                dir.strip_prefix(d.path())
                    .map(|r| r.components().count())
                    .unwrap_or(0),
            );
            let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
            let files = entries
                .iter()
                .filter(|e| e.as_ref().unwrap().file_type().unwrap().is_file())
                .count();
            assert!(
                files <= PER_DIR as usize,
                "{} files in {}",
                files,
                dir.display()
            );
            for e in entries {
                let e = e.unwrap();
                if e.file_type().unwrap().is_dir() {
                    stack.push(e.path());
                }
            }
        }
        assert!(max_depth <= 8, "depth {max_depth}");
    }

    #[test]
    fn a_duplicate_in_the_tiny_corpus_is_byte_identical() {
        let d = TempDir::new("bench-tiny-dup");
        corpus_tiny(d.path(), 1000).unwrap();
        let st = stats(d.path()).unwrap();
        assert!(st.duplicate_ratio > 0.0, "the corpus must hold duplicates");
        // Verify the property the ratio claims: some reported-duplicate pair is
        // byte-identical on disk. Group the dedup class by whole-file digest (the
        // same rule stats() uses) and compare a colliding pair.
        let mut by_digest: std::collections::HashMap<[u8; 32], Vec<PathBuf>> =
            std::collections::HashMap::new();
        for e in walk_files(d.path()) {
            let len = std::fs::metadata(&e).unwrap().len();
            if len < SMALL {
                by_digest
                    .entry(*blake3::hash(&std::fs::read(&e).unwrap()).as_bytes())
                    .or_default()
                    .push(e);
            }
        }
        let (_, pair) = by_digest
            .iter()
            .find(|(_, v)| v.len() > 1)
            .expect("stats reported duplicates, so a pair exists");
        assert_eq!(
            std::fs::read(&pair[0]).unwrap(),
            std::fs::read(&pair[1]).unwrap()
        );
    }

    fn walk_files(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap() {
                let e = e.unwrap();
                if e.file_type().unwrap().is_dir() {
                    stack.push(e.path());
                } else {
                    out.push(e.path());
                }
            }
        }
        out
    }

    #[test]
    fn listing_scale_multiplies_sizes() {
        let d = TempDir::new("bench-scale");
        let l = d.path().join("listing.txt");
        std::fs::write(&l, "100 f.bin\n").unwrap();
        let half = d.path().join("half");
        corpus_listing(&half, &l, 0.5).unwrap();
        assert_eq!(std::fs::metadata(half.join("f.bin")).unwrap().len(), 50);
        let double = d.path().join("double");
        corpus_listing(&double, &l, 2.0).unwrap();
        assert_eq!(std::fs::metadata(double.join("f.bin")).unwrap().len(), 200);
    }

    #[test]
    fn record_appends_one_line_per_call() {
        let d = TempDir::new("bench-record");
        // The path is the caller's (C15): recording into a fresh path creates it, and
        // repeated calls append one JSON line each.
        let out = d.path().join("nested").join("results.jsonl");
        record(
            &out,
            &serde_json::json!({"schema": SCHEMA, "kind": "bench"}),
        )
        .unwrap();
        record(
            &out,
            &serde_json::json!({"schema": SCHEMA, "kind": "bench", "run": 2}),
        )
        .unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(v["schema"], SCHEMA);
        }
        assert_eq!(
            lines[1].trim_end(),
            r#"{"kind":"bench","run":2,"schema":1}"#
        );
    }

    #[test]
    fn record_envelope_freezes_the_schema() {
        let v = record_envelope("bench", Some("tiny"), Some(11), "ava1");
        assert_eq!(v["schema"], SCHEMA);
        assert_eq!(v["kind"], "bench");
        assert_eq!(v["corpus"], "tiny");
        assert_eq!(v["seed"], 11);
        assert_eq!(v["protocol"], "ava1");
        assert!(v["machine"].as_str().is_some_and(|m| !m.is_empty()));
        assert!(v["started_at"].as_u64().is_some());
        // A1: an undefined ratio serializes as JSON null, never 0.
        let s = Stats {
            files: 1,
            bytes: 1,
            histogram: [0; 5],
            compressible_fraction: 0.0,
            duplicate_ratio: f64::NAN,
        };
        assert!(serde_json::to_value(&s).unwrap()["duplicate_ratio"].is_null());
    }

    #[test]
    fn a_generator_refuses_a_nonempty_target_without_force() {
        let d = TempDir::new("bench-force");
        let target = d.path().join("corpus");
        // A missing directory is created.
        ensure_writable_target(&target, false).unwrap();
        assert!(target.is_dir());
        // Empty is fine; non-empty is refused without --force (A2) …
        ensure_writable_target(&target, false).unwrap();
        std::fs::write(target.join("keep.me"), b"precious").unwrap();
        assert!(ensure_writable_target(&target, false).is_err());
        // … and allowed with it, without deleting anything already there.
        ensure_writable_target(&target, true).unwrap();
        assert_eq!(std::fs::read(target.join("keep.me")).unwrap(), b"precious");
        assert!(target.is_dir());
    }

    #[test]
    fn stats_skips_symlinks() {
        // C8: a real game folder may hold symlinks; the walk must not follow them
        // (or fail) — they are skipped and reported.
        let d = TempDir::new("bench-symlink");
        std::fs::write(d.path().join("real"), vec![7u8; 100]).unwrap();
        std::os::unix::fs::symlink(d.path().join("real"), d.path().join("link")).unwrap();
        std::os::unix::fs::symlink("/nonexistent", d.path().join("dangling")).unwrap();
        let st = stats(d.path()).unwrap();
        assert_eq!(st.files, 1);
        assert_eq!(st.bytes, 100);
    }

    #[test]
    fn write_random_is_deterministic_per_seed() {
        // Benchmarks must be reproducible: the same seed yields the same bytes, a
        // different seed does not (C9's spread key makes that explicit).
        let d = TempDir::new("bench-seed");
        write_random(&d.path().join("a"), 100_000, 42).unwrap();
        write_random(&d.path().join("b"), 100_000, 42).unwrap();
        write_random(&d.path().join("c"), 100_000, 43).unwrap();
        assert_eq!(
            std::fs::read(d.path().join("a")).unwrap(),
            std::fs::read(d.path().join("b")).unwrap()
        );
        assert_ne!(
            std::fs::read(d.path().join("a")).unwrap(),
            std::fs::read(d.path().join("c")).unwrap()
        );
    }

    #[test]
    fn the_sample_takes_head_middle_and_tail() {
        // A4: for files > 1 MiB the compressible-fraction sample is head+middle+tail
        // (3 × 340 KiB), not a 1 MiB prefix. A 4 MiB file that is random for its
        // first MiB and zeros after: the head+middle+tail sample is 340 KiB random
        // (head) + 680 KiB zeros (middle and tail), which deflates to ~34 % of its
        // size, so the whole file counts compressible — a prefix sampler would read
        // only the random MiB and report 0. The scratch file lives in a second temp
        // dir so the walked corpus holds exactly one file.
        let d = TempDir::new("bench-hmt");
        let s = TempDir::new("bench-hmt-scratch");
        write_random(&s.path().join("head"), 1 << 20, 5).unwrap();
        let f = d.path().join("f.bin");
        let mut out = std::fs::File::create(&f).unwrap();
        out.write_all(&std::fs::read(s.path().join("head")).unwrap())
            .unwrap();
        out.write_all(&vec![0u8; 3 << 20]).unwrap();
        let st = stats(d.path()).unwrap();
        assert!(
            st.compressible_fraction > 0.9,
            "{}",
            st.compressible_fraction
        );
    }
}
