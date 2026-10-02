//! Manifests (SPEC.md §11): what a job moves, in file_id order.
use std::io;

use crate::gen::{ManifestEntry, ManifestPage, ENTRY_DIR, ENTRY_FILE};
use crate::source::Source;
use crate::wire::Message;

pub const MAX_PATH: usize = 1024;
/// A page's encoded size limit: under the 64 KiB control cap with room for the MAC.
pub const PAGE_BYTES: usize = 60 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    #[error("path is empty")]
    Empty,
    #[error("path is longer than {MAX_PATH} bytes")]
    TooLong,
    #[error("path is absolute")]
    Absolute,
    #[error("path has an empty, '.' or '..' component")]
    BadComponent,
    #[error("path contains NUL")]
    Nul,
    #[error("file ids are not consecutive at {0}")]
    Gap(u32),
}

pub fn check_path(p: &str) -> Result<(), PathError> {
    if p.is_empty() {
        return Err(PathError::Empty);
    }
    if p.len() > MAX_PATH {
        return Err(PathError::TooLong);
    }
    if p.starts_with('/') {
        return Err(PathError::Absolute);
    }
    if p.contains('\0') {
        return Err(PathError::Nul);
    }
    if p.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
        return Err(PathError::BadComponent);
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub kind: u8,
    pub mode: u32,
    pub size: u64,
    pub mtime: u64,
    pub path: String,
    /// Set only under the verify policy.
    pub root: Option<[u8; 32]>,
}

impl Entry {
    fn wire(&self, file_id: u32, with_root: bool) -> ManifestEntry {
        ManifestEntry {
            file_id,
            kind: self.kind,
            mode: self.mode,
            size: self.size,
            mtime: self.mtime,
            path: self.path.clone(),
            root: if with_root { self.root } else { None },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Manifest {
    pub entries: Vec<Entry>,
}

impl Manifest {
    pub fn entry(&self, id: u32) -> &Entry {
        &self.entries[id as usize]
    }

    pub fn is_file(&self, id: u32) -> bool {
        self.entries[id as usize].kind == ENTRY_FILE
    }

    pub fn files(&self) -> u32 {
        self.entries.iter().filter(|e| e.kind == ENTRY_FILE).count() as u32
    }

    pub fn bytes(&self) -> u64 {
        self.entries
            .iter()
            .filter(|e| e.kind == ENTRY_FILE)
            .map(|e| e.size)
            .sum()
    }

    /// BLAKE3 over `u32le(len) ‖ entry` for every entry, without ext (SPEC.md §11.3).
    pub fn hash(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        for (i, e) in self.entries.iter().enumerate() {
            let b = e
                .wire(i as u32, false)
                .to_bytes()
                .expect("checked paths encode");
            h.update(&(b.len() as u32).to_le_bytes());
            h.update(&b);
        }
        *h.finalize().as_bytes()
    }

    pub fn pages(&self, job_id: [u8; 16]) -> Vec<ManifestPage> {
        let mut pages = Vec::new();
        let mut cur: Vec<ManifestEntry> = Vec::new();
        // job_id + records length + ext count
        let base = 16 + 4 + 2;
        let mut size = base;
        for (i, e) in self.entries.iter().enumerate() {
            let w = e.wire(i as u32, true);
            let n = 4 + w.to_bytes().expect("checked paths encode").len();
            if size + n > PAGE_BYTES && !cur.is_empty() {
                pages.push(ManifestPage {
                    job_id,
                    entries: std::mem::take(&mut cur),
                });
                size = base;
            }
            size += n;
            cur.push(w);
        }
        if !cur.is_empty() || pages.is_empty() {
            pages.push(ManifestPage {
                job_id,
                entries: cur,
            });
        }
        pages
    }

    /// Reassembles pages, checking ids are consecutive from 0 and every path is valid.
    pub fn from_pages(pages: impl IntoIterator<Item = ManifestPage>) -> Result<Self, PathError> {
        let mut entries = Vec::new();
        for p in pages {
            for w in p.entries {
                if w.file_id as usize != entries.len() {
                    return Err(PathError::Gap(w.file_id));
                }
                check_path(&w.path)?;
                entries.push(Entry {
                    kind: w.kind,
                    mode: w.mode,
                    size: w.size,
                    mtime: w.mtime,
                    path: w.path,
                    root: w.root,
                });
            }
        }
        Ok(Self { entries })
    }
}

/// Every directory and file under the source root, depth first, children sorted by name.
/// `exclude` sees each relative path; true leaves it (and a directory's subtree) out.
pub fn walk(src: &dyn Source, exclude: &dyn Fn(&str) -> bool) -> io::Result<Manifest> {
    let mut entries = Vec::new();
    let mut dirs = vec![String::new()];
    while let Some(dir) = dirs.pop() {
        for (name, m) in src.list(&dir)? {
            let rel = if dir.is_empty() {
                name
            } else {
                format!("{dir}/{name}")
            };
            if exclude(&rel) {
                continue;
            }
            check_path(&rel)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{rel}: {e}")))?;
            if m.is_dir {
                dirs.push(rel.clone());
            }
            entries.push(Entry {
                kind: if m.is_dir { ENTRY_DIR } else { ENTRY_FILE },
                mode: m.mode,
                size: m.size,
                mtime: m.mtime,
                path: rel,
                root: None,
            });
        }
    }
    // Depth-first preorder with siblings sorted by name is exactly the order of the
    // paths' component lists (["a"] < ["a", "x"] < ["a2"]), whatever order we visited in.
    entries.sort_by(|a, b| a.path.split('/').cmp(b.path.split('/')));
    Ok(Manifest { entries })
}

/// A one-file manifest for `rel` (JF_SINGLE_FILE). Its path is the file name.
pub fn single(src: &dyn Source, rel: &str) -> io::Result<Manifest> {
    let m = src.stat(rel)?;
    if m.is_dir {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a file"));
    }
    let name = rel.rsplit('/').next().unwrap_or(rel).to_string();
    check_path(&name).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
    Ok(Manifest {
        entries: vec![Entry {
            kind: ENTRY_FILE,
            mode: m.mode,
            size: m.size,
            mtime: m.mtime,
            path: name,
            root: None,
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::LocalSource;

    fn e(path: &str, size: u64) -> Entry {
        Entry {
            kind: ENTRY_FILE,
            mode: 0o644,
            size,
            mtime: 1_700_000_000,
            path: path.into(),
            root: None,
        }
    }

    #[test]
    fn path_rules() {
        for ok in ["a", "a/b", "My..Game/x", "..rc1", "ü/日本", "a b/c"] {
            assert_eq!(check_path(ok), Ok(()), "{ok}");
        }
        let long = "x".repeat(MAX_PATH + 1);
        for bad in [
            "",
            "/a",
            "a//b",
            "a/",
            ".",
            "..",
            "a/../b",
            "a/./b",
            "a\0b",
            long.as_str(),
        ] {
            assert!(check_path(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn path_at_exactly_max_len_is_ok() {
        assert_eq!(check_path(&"x".repeat(MAX_PATH)), Ok(()));
        assert_eq!(
            check_path(&format!("{}/y", "x".repeat(MAX_PATH - 2))),
            Ok(())
        );
    }

    #[test]
    fn the_hash_covers_every_field_but_not_the_root() {
        let m = Manifest {
            entries: vec![e("a", 1), e("b", 2)],
        };
        let h = m.hash();
        let mut m2 = m.clone();
        m2.entries[1].root = Some([9; 32]);
        assert_eq!(m2.hash(), h);
        for f in [
            |x: &mut Entry| x.size += 1,
            |x: &mut Entry| x.mtime += 1,
            |x: &mut Entry| x.mode ^= 1,
            |x: &mut Entry| x.kind = ENTRY_DIR,
            |x: &mut Entry| x.path.push('z'),
        ] {
            let mut m3 = m.clone();
            f(&mut m3.entries[0]);
            assert_ne!(m3.hash(), h);
        }
    }

    #[test]
    fn pages_hold_every_entry_and_fit_a_control_frame() {
        let entries: Vec<Entry> = (0..5000)
            .map(|i| e(&format!("dir/{i:05}/{}", "n".repeat(i % 300 + 1)), i as u64))
            .collect();
        let m = Manifest { entries };
        let pages = m.pages([1; 16]);
        assert!(pages.len() > 1);
        let mut next = 0u32;
        for p in &pages {
            let len = p.to_bytes().unwrap().len();
            assert!(len <= PAGE_BYTES, "{len}");
            for x in &p.entries {
                assert_eq!(x.file_id, next);
                next += 1;
            }
        }
        assert_eq!(Manifest::from_pages(pages).unwrap(), m);
    }

    #[test]
    fn from_pages_refuses_gaps_and_bad_paths() {
        let m = Manifest {
            entries: vec![e("a", 1), e("b", 2)],
        };
        let mut p = m.pages([0; 16]);
        p[0].entries[1].file_id = 5;
        assert!(Manifest::from_pages(p).is_err());
        let mut p = m.pages([0; 16]);
        p[0].entries[0].path = "../x".into();
        assert!(Manifest::from_pages(p).is_err());
    }

    #[test]
    fn empty_manifest_pages_to_one_empty_page_and_round_trips() {
        let m = Manifest::default();
        let pages = m.pages([7; 16]);
        assert_eq!(pages.len(), 1);
        assert!(pages[0].entries.is_empty());
        assert_eq!(pages[0].job_id, [7; 16]);
        assert_eq!(Manifest::from_pages(pages).unwrap(), m);
        assert_eq!((m.files(), m.bytes()), (0, 0));
        // An empty manifest's hash is BLAKE3 of nothing.
        assert_eq!(m.hash(), *blake3::hash(b"").as_bytes());
    }

    #[test]
    fn walk_is_depth_first_sorted_and_honours_excludes() {
        let d = std::env::temp_dir().join(format!("ava1-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("b/c")).unwrap();
        std::fs::create_dir_all(d.join("a")).unwrap();
        std::fs::write(d.join("z.txt"), b"z").unwrap();
        std::fs::write(d.join("b/c/f"), b"ff").unwrap();
        std::fs::write(d.join("b/.DS_Store"), b"x").unwrap();
        let src = LocalSource::new(d.clone());
        let m = walk(&src, &|p: &str| p.ends_with(".DS_Store")).unwrap();
        let got: Vec<(&str, u8, u64)> = m
            .entries
            .iter()
            .map(|x| (x.path.as_str(), x.kind, x.size))
            .collect();
        assert_eq!(
            got,
            vec![
                ("a", ENTRY_DIR, 0),
                ("b", ENTRY_DIR, 0),
                ("b/c", ENTRY_DIR, 0),
                ("b/c/f", ENTRY_FILE, 2),
                ("z.txt", ENTRY_FILE, 1)
            ]
        );
        assert_eq!((m.files(), m.bytes()), (2, 3));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn single_uses_the_basename_and_refuses_directories() {
        let d = std::env::temp_dir().join(format!("ava1-single-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("sub/f.txt"), b"hello").unwrap();
        let src = LocalSource::new(d.clone());
        let m = single(&src, "sub/f.txt").unwrap();
        assert_eq!(m.entries.len(), 1);
        let x = &m.entries[0];
        assert_eq!((x.kind, x.path.as_str(), x.size), (ENTRY_FILE, "f.txt", 5));
        assert!(single(&src, "sub").is_err());
        assert!(single(&src, "missing").is_err());
        std::fs::remove_dir_all(&d).unwrap();
    }
}
