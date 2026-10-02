//! The receiver's journal (SPEC.md §14): data sync → journal append → Durable. One format,
//! written by both the engine and the console, so either can resume a job the other started.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, Write};
use std::path::{Path, PathBuf};

use crate::gen::{JnlBatch, JnlDone, JnlOpen, JnlReset, JnlSnapshot, ManifestEntry, RootItem};
use crate::manifest::{Entry, Manifest};
use crate::ranges::{from_runs, runs, Need, RangeSet};
use crate::wire::{Message, Reader, Writer};

pub const MAGIC: &[u8; 8] = b"AVA1JNL1";
pub const K_OPEN: u8 = 1;
pub const K_BATCH: u8 = 2;
pub const K_RESET: u8 = 3;
pub const K_SNAPSHOT: u8 = 4;
pub const K_DONE: u8 = 5;
/// The journal is compacted once it passes this size (SPEC.md §14).
pub const COMPACT_AT: u64 = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    Open(JnlOpen),
    Batch(JnlBatch),
    Reset(u32),
    Snapshot(JnlSnapshot),
    Done(u16),
}

impl Record {
    fn encode(&self) -> io::Result<(u8, Vec<u8>)> {
        let (k, b) = match self {
            Record::Open(o) => (K_OPEN, o.to_bytes()),
            Record::Batch(b) => (K_BATCH, b.to_bytes()),
            Record::Reset(f) => (K_RESET, JnlReset { file_id: *f }.to_bytes()),
            Record::Snapshot(s) => (K_SNAPSHOT, s.to_bytes()),
            Record::Done(s) => (K_DONE, JnlDone { status: *s }.to_bytes()),
        };
        let body = b.map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        Ok((k, body))
    }

    fn decode(kind: u8, b: &[u8]) -> Option<Record> {
        Some(match kind {
            K_OPEN => Record::Open(JnlOpen::decode(b).ok()?),
            K_BATCH => Record::Batch(JnlBatch::decode(b).ok()?),
            K_RESET => Record::Reset(JnlReset::decode(b).ok()?.file_id),
            K_SNAPSHOT => Record::Snapshot(JnlSnapshot::decode(b).ok()?),
            K_DONE => Record::Done(JnlDone::decode(b).ok()?.status),
            _ => return None,
        })
    }
}

fn frame(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(body.len() + 9);
    v.extend_from_slice(&((body.len() + 1) as u32).to_le_bytes());
    v.push(kind);
    v.extend_from_slice(body);
    let mut c = vec![kind];
    c.extend_from_slice(body);
    v.extend_from_slice(&crate::crc32c::crc32c(&c).to_le_bytes());
    v
}

/// A single writer per directory: `create`, `open`, `append` and `compact` all assume no other
/// process holds this job's journal.
pub struct Journal {
    f: File,
    dir: PathBuf,
    len: u64,
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

fn open_for_append(p: &Path) -> io::Result<File> {
    let mut f = OpenOptions::new().read(true).write(true).open(p)?;
    f.seek(io::SeekFrom::End(0))?;
    Ok(f)
}

impl Journal {
    /// A brand-new job's journal. Clobbers any existing one in `dir`: callers resume with
    /// `open`, never with `create`.
    pub fn create(dir: &Path, open: &JnlOpen) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let (k, b) = Record::Open(open.clone()).encode()?;
        let mut all = MAGIC.to_vec();
        all.extend(frame(k, &b));
        let tmp = dir.join("journal.tmp");
        {
            let mut f = File::create(&tmp)?;
            f.write_all(&all)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, dir.join("journal"))?; // same directory: never cross a device
        sync_dir(dir)?;
        let f = open_for_append(&dir.join("journal"))?;
        Ok(Self {
            f,
            dir: dir.to_path_buf(),
            len: all.len() as u64,
        })
    }

    /// Replays every intact record and truncates a torn tail so appends continue cleanly.
    pub fn open(dir: &Path) -> io::Result<(Self, Vec<Record>)> {
        let p = dir.join("journal");
        let b = fs::read(&p)?;
        if b.len() < 8 || &b[..8] != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not an AVA1 journal",
            ));
        }
        let mut at = 8usize;
        let mut recs = Vec::new();
        while b.len() - at >= 4 {
            let len = u32::from_le_bytes(b[at..at + 4].try_into().unwrap()) as usize;
            if len == 0 {
                break;
            }
            let Some(end) = at.checked_add(8).and_then(|x| x.checked_add(len)) else {
                break;
            };
            if end > b.len() {
                break;
            }
            let body = &b[at + 4..at + 4 + len];
            let crc = u32::from_le_bytes(b[at + 4 + len..end].try_into().unwrap());
            if crate::crc32c::crc32c(body) != crc {
                break;
            }
            let Some(r) = Record::decode(body[0], &body[1..]) else {
                break;
            };
            recs.push(r);
            at = end;
        }
        let f = OpenOptions::new().read(true).write(true).open(&p)?;
        f.set_len(at as u64)?;
        f.sync_all()?;
        let mut j = Self {
            f,
            dir: dir.to_path_buf(),
            len: at as u64,
        };
        j.seek_end()?;
        Ok((j, recs))
    }

    fn seek_end(&mut self) -> io::Result<()> {
        self.f.seek(io::SeekFrom::Start(self.len))?;
        Ok(())
    }

    pub fn append(&mut self, r: &Record) -> io::Result<()> {
        let (k, b) = r.encode()?;
        let fr = frame(k, &b);
        self.f.write_all(&fr)?;
        self.f.sync_data()?;
        self.len += fr.len() as u64;
        Ok(())
    }

    pub fn compact(&mut self, open: &JnlOpen, snap: &JnlSnapshot) -> io::Result<()> {
        let mut all = MAGIC.to_vec();
        let (k, b) = Record::Open(open.clone()).encode()?;
        all.extend(frame(k, &b));
        let (k, b) = Record::Snapshot(snap.clone()).encode()?;
        all.extend(frame(k, &b));
        let tmp = self.dir.join("journal.tmp");
        {
            let mut f = File::create(&tmp)?;
            f.write_all(&all)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, self.dir.join("journal"))?;
        sync_dir(&self.dir)?;
        self.f = open_for_append(&self.dir.join("journal"))?;
        self.len = all.len() as u64;
        Ok(())
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len <= 8
    }
}

/// What a replay knows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    pub open: Option<JnlOpen>,
    pub done: BTreeSet<u32>,
    pub ranges: BTreeMap<u32, RangeSet>,
    pub roots: BTreeMap<u32, [u8; 32]>,
    pub finished: Option<u16>,
}

impl State {
    pub fn apply(&mut self, r: &Record) {
        match r {
            Record::Open(o) => self.open = Some(o.clone()),
            Record::Batch(b) => {
                // A file marked done drops its ranges (SPEC.md §14.2): the bytes are complete.
                self.done.extend(from_runs(&b.files));
                for x in &b.ranges {
                    self.ranges
                        .entry(x.file_id)
                        .or_default()
                        .insert(x.offset, x.offset + x.len);
                }
                for x in &b.roots {
                    self.roots.insert(x.file_id, x.root);
                }
                for f in from_runs(&b.files) {
                    self.ranges.remove(&f);
                }
            }
            Record::Reset(f) => {
                self.done.remove(f);
                self.ranges.remove(f);
                self.roots.remove(f);
            }
            Record::Snapshot(s) => {
                self.done = from_runs(&s.done);
                self.ranges.clear();
                for x in &s.ranges {
                    self.ranges
                        .entry(x.file_id)
                        .or_default()
                        .insert(x.offset, x.offset + x.len);
                }
                self.roots = s.roots.iter().map(|x| (x.file_id, x.root)).collect();
            }
            Record::Done(s) => self.finished = Some(*s),
        }
    }

    pub fn snapshot(&self) -> JnlSnapshot {
        JnlSnapshot {
            done: runs(&self.done),
            ranges: self
                .ranges
                .iter()
                .flat_map(|(f, r)| {
                    r.iter().map(move |(s, e)| crate::gen::FileRange {
                        file_id: *f,
                        offset: s,
                        len: e - s,
                    })
                })
                .collect(),
            roots: self
                .roots
                .iter()
                .map(|(f, r)| RootItem {
                    file_id: *f,
                    root: *r,
                })
                .collect(),
        }
    }

    /// The receiver's answer for a resume: done files, plus the durable ranges of the rest.
    pub fn need(&self) -> Need {
        Need {
            done: self.done.clone(),
            partial: self.ranges.clone().into_iter().collect(),
        }
    }
}

/// The job directory name is the lowercase hex of the job id (the same bytes C's
/// `ava1_job_dir` produces).
pub fn job_dir(jobs_dir: &Path, job: &[u8; 16]) -> PathBuf {
    jobs_dir.join(crate::hex::encode(job))
}

pub fn write_manifest(dir: &Path, m: &Manifest) -> io::Result<()> {
    let mut w = Writer::new();
    let entries: Vec<ManifestEntry> = m
        .entries
        .iter()
        .enumerate()
        .map(|(i, e)| ManifestEntry {
            file_id: i as u32,
            kind: e.kind,
            mode: e.mode,
            size: e.size,
            mtime: e.mtime,
            path: e.path.clone(),
            root: e.root,
        })
        .collect();
    w.records(&entries)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let tmp = dir.join("manifest.tmp");
    {
        let mut f = File::create(&tmp)?;
        f.write_all(&w.buf[4..])?; // the item stream, without the records length prefix
        f.sync_all()?;
    }
    fs::rename(&tmp, dir.join("manifest"))?;
    sync_dir(dir)
}

pub fn read_manifest(dir: &Path) -> io::Result<Manifest> {
    let b = fs::read(dir.join("manifest"))?;
    let mut framed = (b.len() as u32).to_le_bytes().to_vec();
    framed.extend_from_slice(&b);
    let entries: Vec<ManifestEntry> = Reader::new(&framed)
        .records()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(Manifest {
        entries: entries
            .into_iter()
            .map(|w| Entry {
                kind: w.kind,
                mode: w.mode,
                size: w.size,
                mtime: w.mtime,
                path: w.path,
                root: w.root,
            })
            .collect(),
    })
}

/// Removes job directories idle for more than `max_age_s` as seen from `now_unix`; returns how
/// many were removed. A directory whose mtime cannot be read is left alone — never deleted on a
/// guess.
pub fn gc(jobs_dir: &Path, now_unix: u64, max_age_s: u64) -> io::Result<usize> {
    let mut n = 0;
    let Ok(rd) = fs::read_dir(jobs_dir) else {
        return Ok(0);
    };
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        let Some(last) = last_write(&p) else { continue };
        if now_unix.saturating_sub(last) > max_age_s {
            fs::remove_dir_all(&p)?;
            n += 1;
        }
    }
    Ok(n)
}

/// The newest mtime of the directory itself and its journal, or `None` if neither is readable.
fn last_write(dir: &Path) -> Option<u64> {
    let mut newest: Option<u64> = None;
    for q in [dir.to_path_buf(), dir.join("journal")] {
        if let Ok(t) = fs::metadata(&q).and_then(|m| m.modified()) {
            if let Ok(d) = t.duration_since(std::time::UNIX_EPOCH) {
                let s = d.as_secs();
                newest = Some(newest.map_or(s, |n| n.max(s)));
            }
        }
    }
    newest
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gen::{FileRange, FileRun, RootItem};
    use std::path::PathBuf;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ava1-jnl-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn open_rec() -> JnlOpen {
        JnlOpen {
            job_id: [1; 16],
            manifest_hash: [2; 32],
            kind: 1,
            flags: 0,
            staged: 1,
            root: "/data/x".into(),
        }
    }

    fn batch(f: u32, off: u64) -> Record {
        Record::Batch(JnlBatch {
            files: vec![FileRun { first: f, count: 1 }],
            ranges: vec![FileRange {
                file_id: 9,
                offset: off,
                len: 1 << 20,
            }],
            roots: vec![RootItem {
                file_id: 9,
                root: [off as u8; 32],
            }],
        })
    }

    #[test]
    fn records_replay_in_order() {
        let d = tmp("order");
        let mut j = Journal::create(&d, &open_rec()).unwrap();
        j.append(&batch(0, 0)).unwrap();
        j.append(&Record::Reset(9)).unwrap(); // drops file 9's range and root
        j.append(&batch(1, 1 << 20)).unwrap();
        j.append(&Record::Done(0)).unwrap();
        drop(j);
        let (_, recs) = Journal::open(&d).unwrap();
        let mut st = State::default();
        for r in &recs {
            st.apply(r);
        }
        assert_eq!(st.open, Some(open_rec()));
        assert_eq!(st.done.iter().copied().collect::<Vec<_>>(), vec![0, 1]);
        assert_eq!(
            st.ranges[&9].iter().collect::<Vec<_>>(),
            vec![(1 << 20, 2 << 20)]
        );
        assert_eq!(st.finished, Some(0));
    }

    #[test]
    fn journal_torn_tail_is_ignored() {
        let d = tmp("torn");
        let mut j = Journal::create(&d, &open_rec()).unwrap();
        j.append(&batch(0, 0)).unwrap();
        j.append(&batch(1, 1 << 20)).unwrap();
        let good = j.len();
        j.append(&batch(2, 2 << 20)).unwrap();
        drop(j);
        let p = d.join("journal");
        let mut b = std::fs::read(&p).unwrap();
        b.truncate(b.len() - 3); // cuts into the third record
        b.extend_from_slice(&[0xff; 40]);
        std::fs::write(&p, &b).unwrap();
        let (mut j, recs) = Journal::open(&d).unwrap();
        assert_eq!(recs.len(), 3);
        assert_eq!(j.len(), good); // the torn tail was truncated away
        j.append(&batch(3, 3 << 20)).unwrap();
        drop(j);
        let (_, recs) = Journal::open(&d).unwrap();
        assert_eq!(recs.len(), 4);
        // One flipped CRC byte in the middle of the file stops replay at that record.
        let mut b = std::fs::read(&p).unwrap();
        let at = good as usize - 1;
        b[at] ^= 0x01;
        std::fs::write(&p, &b).unwrap();
        let (_, recs) = Journal::open(&d).unwrap();
        assert_eq!(recs.len(), 2);
    }

    #[test]
    fn compaction_keeps_the_state_and_shrinks_the_file() {
        let d = tmp("compact");
        let mut j = Journal::create(&d, &open_rec()).unwrap();
        let mut st = State::default();
        st.apply(&Record::Open(open_rec()));
        // A 400-run batch is ~5.6 KB on disk, so this genuinely crosses 1 MiB. (The plan's
        // 20,000 one-run batches only reach ~700 KB and never trigger compaction.)
        let mut i = 0u32;
        while j.len() <= COMPACT_AT {
            let files: Vec<FileRun> = (0..400)
                .map(|k| FileRun {
                    first: i * 400 + k,
                    count: 1,
                })
                .collect();
            let r = Record::Batch(JnlBatch {
                files,
                ..Default::default()
            });
            st.apply(&r);
            j.append(&r).unwrap();
            i += 1;
            assert!(i < 10_000, "the journal never crossed COMPACT_AT");
        }
        j.compact(&open_rec(), &st.snapshot()).unwrap();
        assert!(j.len() < 1024);
        drop(j);
        let (_, recs) = Journal::open(&d).unwrap();
        assert_eq!(recs.len(), 2); // open + snapshot
        let mut st2 = State::default();
        for r in &recs {
            st2.apply(r);
        }
        assert_eq!(st2.done, st.done);
    }

    #[test]
    fn gc_removes_only_job_dirs_idle_for_seven_days() {
        let d = tmp("gc");
        let old = job_dir(&d, &[1; 16]);
        let new = job_dir(&d, &[2; 16]);
        Journal::create(&old, &open_rec()).unwrap();
        // mtimes have 1 s granularity: a 2.1 s gap makes `old` >= 2 s idle and `new` <= 1 s,
        // so a 1 s threshold separates them whatever the clock's fractional phase.
        std::thread::sleep(std::time::Duration::from_millis(2100));
        Journal::create(&new, &open_rec()).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(gc(&d, now, 7 * 86_400).unwrap(), 0); // both fresh
        assert_eq!(gc(&d, now, 1).unwrap(), 1); // only `old` is > 1 s idle
        assert!(!old.exists() && new.exists());
        assert_eq!(gc(&d, now + 8 * 86_400, 7 * 86_400).unwrap(), 1); // the 7-day path
        assert!(!new.exists());
    }
}
