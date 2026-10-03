//! The `ava1::source::Source` views this crate's uploads read through: the engine's
//! `SourceFs` (local disk, or a saved SMB/FTP/SFTP server) and the explicit file list.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use ava1::source::{ReadAt, SeekReader, Source, SourceMeta};
use ps5upload_core::source_fs::SourceFs;

/// A source through the engine's `SourceFs`: local disk by default, a saved remote
/// server when the config carries one.
pub struct FsSource {
    fs: Arc<dyn SourceFs>,
    root: PathBuf,
}

impl FsSource {
    pub fn new(fs: Arc<dyn SourceFs>, root: PathBuf) -> Self {
        Self { fs, root }
    }

    fn path(&self, rel: &str) -> PathBuf {
        if rel.is_empty() {
            self.root.clone()
        } else {
            self.root.join(rel)
        }
    }
}

impl Source for FsSource {
    fn open(&self, rel: &str) -> io::Result<Box<dyn ReadAt>> {
        Ok(Box::new(SeekReader::new(self.fs.open(&self.path(rel))?)))
    }

    fn list(&self, rel: &str) -> io::Result<Vec<(String, SourceMeta)>> {
        let mut out = Vec::new();
        for (p, is_dir) in self.fs.read_dir(&self.path(rel))? {
            let name = p
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "a name is not UTF-8"))?
                .to_string();
            let len = if is_dir { 0 } else { self.fs.metadata(&p)?.len };
            // `SourceFs::SourceMeta` has no mtime: report 0. That is legal — the
            // console applies mtime as information, and POLICY_REPLACE is the
            // engine's policy — but a remote source's mtimes are silently lost.
            out.push((
                name,
                SourceMeta {
                    size: len,
                    mtime: 0,
                    mode: if is_dir { 0o755 } else { 0o644 },
                    is_dir,
                },
            ));
        }
        Ok(out)
    }

    fn stat(&self, rel: &str) -> io::Result<SourceMeta> {
        let m = self.fs.metadata(&self.path(rel))?;
        Ok(SourceMeta {
            size: m.len,
            mtime: 0,
            mode: if m.is_dir { 0o755 } else { 0o644 },
            is_dir: m.is_dir,
        })
    }
}

/// Explicit (relative path → local file) pairs: the file-list upload.
pub struct ListSource {
    map: HashMap<String, PathBuf>,
}

impl ListSource {
    pub fn new(pairs: Vec<(String, PathBuf)>) -> Self {
        Self {
            map: pairs.into_iter().collect(),
        }
    }
}

impl Source for ListSource {
    fn open(&self, rel: &str) -> io::Result<Box<dyn ReadAt>> {
        let p = self
            .map
            .get(rel)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, rel.to_string()))?;
        Ok(Box::new(SeekReader::new(std::fs::File::open(p)?)))
    }

    fn list(&self, _rel: &str) -> io::Result<Vec<(String, SourceMeta)>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "a list source is never walked",
        ))
    }

    fn stat(&self, rel: &str) -> io::Result<SourceMeta> {
        let p = self
            .map
            .get(rel)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, rel.to_string()))?;
        let m = std::fs::metadata(p)?;
        // A real mtime: these are local files, and the manifest is built from here.
        let mtime = m
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        Ok(SourceMeta {
            size: m.len(),
            mtime,
            mode: 0o644,
            is_dir: false,
        })
    }
}
