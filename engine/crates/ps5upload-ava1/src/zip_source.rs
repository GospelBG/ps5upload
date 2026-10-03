//! A zip archive as an AVA1 source. Entry reads inflate on demand.
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use ava1::gen;
use ava1::manifest::{self, Entry, Manifest};
use ava1::source::{ReadAt, Source, SourceMeta};

pub struct ZipSource {
    path: PathBuf,
    files: BTreeMap<String, (usize, SourceMeta)>,
    dirs: BTreeSet<String>,
}

impl ZipSource {
    pub fn open(path: &Path, excludes: &[String]) -> io::Result<(Manifest, Self)> {
        let f = std::fs::File::open(path)?;
        let mut zip = zip::ZipArchive::new(f).map_err(invalid_zip)?;
        let mut files = BTreeMap::new();
        let mut dirs = BTreeSet::new();
        for i in 0..zip.len() {
            let entry = zip.by_index_raw(i).map_err(invalid_zip)?;
            let name = entry.name().trim_end_matches('/');
            if name.is_empty() {
                continue;
            }
            manifest::check_path(name)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{name}: {e}")))?;
            if ps5upload_core::excludes::is_excluded_strings(Path::new(name), excludes) {
                continue;
            }
            let parts: Vec<_> = name.split('/').collect();
            let mut parent = String::new();
            for component in &parts[..parts.len() - 1] {
                if !parent.is_empty() {
                    parent.push('/');
                }
                parent.push_str(component);
                dirs.insert(parent.clone());
            }
            if entry.is_dir() {
                dirs.insert(name.to_owned());
            } else {
                files.insert(
                    name.to_owned(),
                    (
                        i,
                        SourceMeta {
                            size: entry.size(),
                            mtime: 0,
                            mode: entry.unix_mode().unwrap_or(0o644) & 0o7777,
                            is_dir: false,
                        },
                    ),
                );
            }
        }
        if files.keys().any(|p| dirs.contains(p)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "zip path is both file and directory",
            ));
        }
        let mut entries = Vec::with_capacity(files.len() + dirs.len());
        for d in &dirs {
            entries.push(Entry {
                kind: gen::ENTRY_DIR,
                mode: 0o755,
                size: 0,
                mtime: 0,
                path: d.clone(),
                root: None,
            });
        }
        for (p, (_, m)) in &files {
            entries.push(Entry {
                kind: gen::ENTRY_FILE,
                mode: m.mode,
                size: m.size,
                mtime: 0,
                path: p.clone(),
                root: None,
            });
        }
        // Match manifest::walk's depth-first component ordering.
        entries.sort_by(|a, b| a.path.split('/').cmp(b.path.split('/')));
        Ok((
            Manifest { entries },
            Self {
                path: path.to_owned(),
                files,
                dirs,
            },
        ))
    }
}

fn invalid_zip(e: zip::result::ZipError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

struct ZipEntryReader {
    path: PathBuf,
    index: usize,
}

impl ReadAt for ZipEntryReader {
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let f = std::fs::File::open(&self.path)?;
        let mut zip = zip::ZipArchive::new(f).map_err(invalid_zip)?;
        let mut entry = zip.by_index(self.index).map_err(invalid_zip)?;
        io::copy(&mut entry.by_ref().take(off), &mut io::sink())?;
        entry.read(buf)
    }
}

impl Source for ZipSource {
    fn open(&self, rel: &str) -> io::Result<Box<dyn ReadAt>> {
        let (index, _) = self
            .files
            .get(rel)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, rel.to_owned()))?;
        Ok(Box::new(ZipEntryReader {
            path: self.path.clone(),
            index: *index,
        }))
    }

    fn list(&self, rel: &str) -> io::Result<Vec<(String, SourceMeta)>> {
        if !rel.is_empty() && !self.dirs.contains(rel) {
            return Err(io::Error::new(io::ErrorKind::NotFound, rel.to_owned()));
        }
        let prefix = if rel.is_empty() {
            String::new()
        } else {
            format!("{rel}/")
        };
        let mut out = BTreeMap::new();
        for d in &self.dirs {
            if let Some(rest) = d.strip_prefix(&prefix) {
                if !rest.contains('/') {
                    out.insert(
                        rest.to_owned(),
                        SourceMeta {
                            is_dir: true,
                            mode: 0o755,
                            ..SourceMeta::default()
                        },
                    );
                }
            }
        }
        for (p, (_, meta)) in &self.files {
            if let Some(rest) = p.strip_prefix(&prefix) {
                if !rest.contains('/') {
                    out.insert(rest.to_owned(), *meta);
                }
            }
        }
        Ok(out.into_iter().collect())
    }

    fn stat(&self, rel: &str) -> io::Result<SourceMeta> {
        if let Some((_, meta)) = self.files.get(rel) {
            return Ok(*meta);
        }
        if rel.is_empty() || self.dirs.contains(rel) {
            return Ok(SourceMeta {
                is_dir: true,
                mode: 0o755,
                ..SourceMeta::default()
            });
        }
        Err(io::Error::new(io::ErrorKind::NotFound, rel.to_owned()))
    }
}
