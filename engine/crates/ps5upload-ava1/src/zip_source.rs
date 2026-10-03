//! A zip archive as an AVA1 source. Entry reads inflate on demand.
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use ava1::gen;
use ava1::manifest::{self, Entry, Manifest};
use ava1::source::{ReadAt, Source, SourceMeta};

/// Where one entry's bytes live in the archive file, found once at open.
#[derive(Clone, Copy)]
struct Located {
    stored: bool,
    data_start: u64,
    compressed: u64,
}

pub struct ZipSource {
    path: PathBuf,
    files: BTreeMap<String, (Located, SourceMeta)>,
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
                let stored = match entry.compression() {
                    zip::CompressionMethod::Stored => true,
                    zip::CompressionMethod::Deflated => false,
                    other => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("{name}: unsupported zip compression {other:?}"),
                        ))
                    }
                };
                if entry.encrypted() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{name}: encrypted zip entries are not supported"),
                    ));
                }
                let data_start = entry.data_start().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{name}: no data offset"),
                    )
                })?;
                files.insert(
                    name.to_owned(),
                    (
                        Located {
                            stored,
                            data_start,
                            compressed: entry.compressed_size(),
                        },
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

/// One open entry. Reads are sequential in practice (`run_upload` walks a file front
/// to back), so the inflater is kept between calls and only restarted when a read
/// goes backwards: total work is O(entry), not O(entry x reads).
pub struct ZipEntryReader {
    file: BufReader<std::fs::File>,
    at: Located,
    size: u64,
    inflater: Option<flate2::read::DeflateDecoder<io::Take<BufReader<std::fs::File>>>>,
    /// Uncompressed offset the inflater is at.
    pos: u64,
    restarts: u32,
}

impl ZipEntryReader {
    /// How many times the inflater was started (1 for a front-to-back read).
    pub fn restarts(&self) -> u32 {
        self.restarts
    }

    fn start(&mut self) -> io::Result<()> {
        let mut f = BufReader::with_capacity(256 << 10, self.file.get_ref().try_clone()?);
        f.seek(SeekFrom::Start(self.at.data_start))?;
        self.inflater = Some(flate2::read::DeflateDecoder::new(
            f.take(self.at.compressed),
        ));
        self.pos = 0;
        self.restarts += 1;
        Ok(())
    }
}

impl ReadAt for ZipEntryReader {
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<usize> {
        let r = self.read_at_inner(off, buf);
        if r.is_err() {
            // A failed read leaves the decoder mid-stream at an unknown spot: a retry
            // must start over, never continue from there.
            self.inflater = None;
        }
        r
    }
}

impl ZipEntryReader {
    fn read_at_inner(&mut self, off: u64, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || off >= self.size {
            return Ok(0);
        }
        if self.at.stored {
            self.file.seek(SeekFrom::Start(self.at.data_start + off))?;
            let want = (buf.len() as u64).min(self.size - off) as usize;
            return self.file.read(&mut buf[..want]);
        }
        if self.inflater.is_none() || off < self.pos {
            self.start()?;
        }
        let dec = self.inflater.as_mut().expect("started above");
        let mut scratch = [0u8; 16 << 10];
        while self.pos < off {
            let n = ((off - self.pos) as usize).min(scratch.len());
            let got = dec.read(&mut scratch[..n])?;
            if got == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            self.pos += got as u64;
        }
        // Fill the buffer: a deflate stream yields short reads, and callers treat a
        // short read as the end of the file.
        let want = (buf.len() as u64).min(self.size - off) as usize;
        let mut filled = 0;
        while filled < want {
            let got = dec.read(&mut buf[filled..want])?;
            if got == 0 {
                break;
            }
            filled += got;
        }
        self.pos += filled as u64;
        Ok(filled)
    }
}

impl ZipSource {
    /// `Source::open` with the concrete reader (its `restarts` is a test seam).
    pub fn open_entry(&self, rel: &str) -> io::Result<ZipEntryReader> {
        let (at, meta) = self
            .files
            .get(rel)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, rel.to_owned()))?;
        Ok(ZipEntryReader {
            file: BufReader::with_capacity(256 << 10, std::fs::File::open(&self.path)?),
            at: *at,
            size: meta.size,
            inflater: None,
            pos: 0,
            restarts: 0,
        })
    }
}

impl Source for ZipSource {
    fn open(&self, rel: &str) -> io::Result<Box<dyn ReadAt>> {
        Ok(Box::new(self.open_entry(rel)?))
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
