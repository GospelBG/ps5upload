//! The receiver on an engine (SPEC.md §12–§15): downloads, relays, the folder host.
//!
//! The Rust mirror of the payload's C receiver (ava1_recv.c, ava1_apply.c), simplified
//! by a normal OS underneath: blocking writes on `spawn_blocking`, one sync batch every
//! 250 ms run on a task of its own — the run loop keeps draining its inbox and answering
//! the peer while the batch's fsyncs run (ruling 11) — the same journal (SPEC.md §14)
//! and outboards, the same on-disk layout — so an engine can resume a job the console
//! started and vice versa.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::gen::{
    self, Bundle, Chunk, Credit, Durable, FileRange, FileRoot, JnlBatch, JnlOpen, JobCancel,
    JobDone, JobOpen, JobOpenAck, ManifestEnd, ManifestPage, Received, RootItem,
};
use crate::journal::{self, Journal, Record, State};
use crate::manifest::{self, Manifest};
use crate::ranges::{runs, Need, RangeSet};
use crate::router::{ConnTx, Inbound, JobLink};
use crate::send::{next_ctl, Progress, SendError};
use crate::verify::{self, Outboard, GROUP};
use crate::wire::FrameMessage;

/// Where a received job's bytes land. One implementation today (`LocalSink`); a relay
/// (Task 24) and the zip sink implement their own.
pub trait Sink: Send + Sync {
    /// Called once with the manifest, before any data (create directories, decide staging).
    fn prepare(&self, m: &Manifest) -> io::Result<()>;
    /// Bytes of a large file at `off` (group-aligned, whole groups unless it ends the file).
    fn write_at(&self, id: u32, off: u64, data: &[u8]) -> io::Result<()>;
    /// A whole small file (its root was checked when it arrived).
    fn write_whole(&self, id: u32, data: &[u8]) -> io::Result<()>;
    /// Durability: the bytes of every file in `ids` must reach the disk before the return.
    fn sync(&self, ids: &[u32]) -> io::Result<()>;
    /// The resume check's read (SPEC.md §13.4).
    fn read_at(&self, id: u32, off: u64, buf: &mut [u8]) -> io::Result<usize>;
    /// A relay keeps no durable bytes. Its need hint comes from the destination
    /// receiver, which verifies the complete file; this receiver only forwards.
    fn transient_relay(&self) -> bool {
        false
    }
    /// A complete file: part → final, same directory.
    fn commit(&self, id: u32) -> io::Result<()>;
    /// The whole job: staging → final.
    fn finish(&self) -> io::Result<()>;
    /// What the journal's Open records (SPEC.md §14): (destination root, staged). `None`
    /// for a sink with nothing resumable on disk. LocalSink returns the C receiver's rule
    /// (`staged = !single_file && !root.exists()`); see ruling Q1.
    fn resume_key(&self) -> Option<(String, bool)> {
        None
    }
}

/// Files under `root` on this computer: new folders staged in `<root>.ava-part`, large
/// files through `<name>.ava-part`, one rename each at the end.
pub struct LocalSink {
    root: PathBuf,
    single: bool,
    /// The staging decision, taken once at construction — the C receiver's rule
    /// (`staged = !single_file && !root.exists()`, ava1_recv.c:380-430). It is stable
    /// across a crash because the only transition is the atomic part→final rename, so
    /// `root absent ⟺ part present` on every run (ruling 10).
    staged: bool,
    st: Mutex<LocalState>,
}

#[derive(Default)]
struct LocalState {
    m: Option<Arc<Manifest>>,
    open: HashMap<u32, std::fs::File>,
}

impl LocalSink {
    pub fn new(root: PathBuf, single_file: bool) -> Self {
        let staged = !single_file && !root.exists();
        Self {
            root,
            single: single_file,
            staged,
            st: Mutex::default(),
        }
    }

    fn base(&self) -> PathBuf {
        if self.staged {
            PathBuf::from(format!("{}.ava-part", self.root.display()))
        } else {
            self.root.clone()
        }
    }

    /// The path for `id`, part or final. Every `.ava-part` path is derived from the final
    /// path's own parent, so the part→final renames are same-directory by construction and
    /// can never cross a device (Global Constraint 60; ruling Q3: the placement is the
    /// guard — the payload's C keeps the `st_dev` check, a host OS returns EXDEV).
    fn path(&self, st: &LocalState, id: u32, part: bool) -> PathBuf {
        if self.single {
            return if part {
                PathBuf::from(format!("{}.ava-part", self.root.display()))
            } else {
                self.root.clone()
            };
        }
        let rel = &st
            .m
            .as_ref()
            .expect("prepare runs before any data")
            .entry(id)
            .expect("an id the receiver validated against the manifest")
            .path;
        let p = self.base().join(rel);
        if part && !self.staged {
            PathBuf::from(format!("{}.ava-part", p.display()))
        } else {
            p
        }
    }

    fn file(&self, id: u32, part: bool, truncate: bool) -> io::Result<std::fs::File> {
        let mut st = self.st.lock().unwrap();
        if !truncate {
            if let Some(f) = st.open.get(&id) {
                return f.try_clone();
            }
        }
        let p = self.path(&st, id, part);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .read(true)
            .truncate(truncate)
            .open(&p)?;
        st.open.insert(id, f.try_clone()?);
        Ok(f)
    }
}

/// Plain `fsync(2)`: on macOS it hands the data to the drive without flushing the drive's
/// own cache (that is `flush_drive_cache`'s one call per batch); elsewhere it is the full
/// durable sync.
#[cfg(unix)]
fn sys_fsync(f: &std::fs::File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    extern "C" {
        fn fsync(fd: i32) -> i32;
    }
    loop {
        // SAFETY: fsync on a descriptor this File owns for the duration of the call.
        if unsafe { fsync(f.as_raw_fd()) } == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

#[cfg(not(unix))]
fn sys_fsync(f: &std::fs::File) -> io::Result<()> {
    f.sync_data()
}

/// macOS only: `F_FULLFSYNC` flushes the drive's write cache for everything fsync'd before
/// it, so one call per batch makes the whole batch durable. A no-op where fsync already is.
#[cfg(target_vendor = "apple")]
fn flush_drive_cache(f: &std::fs::File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    extern "C" {
        fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    }
    const F_FULLFSYNC: i32 = 51;
    // SAFETY: fcntl(F_FULLFSYNC) takes no argument and only reads the descriptor.
    if unsafe { fcntl(f.as_raw_fd(), F_FULLFSYNC) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_vendor = "apple"))]
fn flush_drive_cache(_f: &std::fs::File) -> io::Result<()> {
    Ok(())
}

impl Sink for LocalSink {
    fn prepare(&self, m: &Manifest) -> io::Result<()> {
        let mut st = self.st.lock().unwrap();
        st.m = Some(Arc::new(m.clone()));
        if !self.single {
            let base = self.base();
            std::fs::create_dir_all(&base)?;
            for e in m.entries.iter().filter(|e| e.kind == gen::ENTRY_DIR) {
                std::fs::create_dir_all(base.join(&e.path))?;
            }
        }
        Ok(())
    }

    fn write_at(&self, id: u32, off: u64, data: &[u8]) -> io::Result<()> {
        let f = self.file(id, true, false)?;
        verify::write_all_at(&f, data, off)
    }

    fn write_whole(&self, id: u32, data: &[u8]) -> io::Result<()> {
        let f = self.file(id, false, true)?;
        verify::write_all_at(&f, data, 0)?;
        f.set_len(data.len() as u64)
    }

    fn sync(&self, ids: &[u32]) -> io::Result<()> {
        let (files, dirs): (Vec<std::fs::File>, BTreeSet<PathBuf>) = {
            let st = self.st.lock().unwrap();
            let files = ids
                .iter()
                .filter_map(|i| st.open.get(i).and_then(|f| f.try_clone().ok()))
                .collect();
            let dirs = ids
                .iter()
                .filter(|i| st.open.contains_key(i))
                .filter_map(|i| self.path(&st, *i, true).parent().map(|p| p.to_path_buf()))
                .collect();
            (files, dirs)
        };
        // One sync per batch, not one drive flush per file (T28): every file gets the
        // cheap fsync, then ONE drive-cache flush covers them all, then the directories
        // (so the new names are durable too). std's `sync_data` is F_FULLFSYNC on macOS —
        // ~15 ms a file, which capped a 2,000-file download at 56 files/s.
        for f in &files {
            sys_fsync(f)?;
        }
        if let Some(f) = files.last() {
            flush_drive_cache(f)?;
        }
        #[cfg(unix)] // a directory cannot be opened for sync on Windows
        for d in &dirs {
            sys_fsync(&std::fs::File::open(d)?)?;
        }
        #[cfg(not(unix))]
        let _ = dirs;
        Ok(())
    }

    fn read_at(&self, id: u32, off: u64, buf: &mut [u8]) -> io::Result<usize> {
        let p = {
            let st = self.st.lock().unwrap();
            self.path(&st, id, true)
        };
        let f = std::fs::File::open(p)?;
        verify::read_exact_at(&f, buf, off).map(|_| buf.len())
    }

    fn commit(&self, id: u32) -> io::Result<()> {
        let mut st = self.st.lock().unwrap();
        let size =
            st.m.as_ref()
                .expect("prepare runs before any data")
                .entry(id)
                .expect("an id the receiver validated against the manifest")
                .size;
        if let Some(f) = st.open.remove(&id) {
            f.set_len(size)?;
            // Cheap fsync only: the batch's journal append (sync_all) flushes the drive cache
            // once for every file this batch committed.
            sys_fsync(&f)?;
        }
        let (part, fin) = (self.path(&st, id, true), self.path(&st, id, false));
        if part != fin {
            std::fs::rename(&part, &fin)?; // same directory by construction (ruling Q3)
        }
        Ok(())
    }

    fn finish(&self) -> io::Result<()> {
        let mut st = self.st.lock().unwrap();
        st.open.clear(); // small files were synced in their batches
        if self.staged {
            if self.root.exists() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "{} appeared during the download; the files are in .ava-part",
                        self.root.display()
                    ),
                ));
            }
            std::fs::rename(self.base(), &self.root)?; // same directory by construction
        }
        Ok(())
    }

    fn resume_key(&self) -> Option<(String, bool)> {
        Some((self.root.to_string_lossy().into_owned(), self.staged))
    }
}

pub struct RecvOptions {
    pub credit: u64,
    /// The job's flags; recorded in the journal's Open so a job reopened with different
    /// flags is refused/replaced exactly as the C receiver does (ruling Q4).
    pub flags: u32,
    pub jobs_dir: PathBuf,
    /// Must equal `flags & JF_ORDERED != 0`.
    pub ordered: bool,
    pub progress: Arc<Progress>,
    pub cancel: Arc<AtomicBool>,
}

#[derive(Debug, Clone)]
pub struct RecvReport {
    pub files: u32,
    pub bytes: u64,
    pub manifest: Arc<Manifest>,
}

fn proto(e: impl std::fmt::Display) -> SendError {
    SendError::Protocol(e.to_string())
}

/// `PathError` → wire mapping (ruling 13, documented in `manifest.rs`): a gap is a
/// malformed manifest (`ERR_PROTOCOL`); every other variant is an invalid path
/// (`ERR_PATH`). Used for manifest validation on the receiver path.
fn wire_path(e: manifest::PathError) -> SendError {
    let status = match e {
        manifest::PathError::Gap(_) => gen::ERR_PROTOCOL,
        _ => gen::ERR_PATH,
    };
    SendError::Refused {
        status,
        message: e.to_string(),
    }
}

/// The next inbox event: the session ending is an error (a job that stops reading is the
/// backpressure; an ended session ends the job).
async fn next(link: &mut JobLink) -> Result<Inbound, SendError> {
    match link.rx.recv().await {
        Some(Inbound::Closed(why)) => Err(SendError::Disconnected(why)),
        None => Err(SendError::Disconnected("the session ended".into())),
        Some(ev) => Ok(ev),
    }
}

/// Collects manifest pages until ManifestEnd; checks the end. The receiver learns the
/// manifest from the peer every time — the on-disk `manifest` file is written for the C
/// side's resume, never read back by Rust (ruling 20). The responder acks before its
/// pages, the opener's ack confirms the open, so a JobOpenAck may come first either way.
async fn read_manifest(link: &mut JobLink) -> Result<Manifest, SendError> {
    let mut pages = Vec::new();
    loop {
        let f = next_ctl(link).await?;
        match f.ty {
            ManifestPage::TYPE => pages.push(f.decode::<ManifestPage>().map_err(proto)?),
            ManifestEnd::TYPE => {
                let e: ManifestEnd = f.decode().map_err(proto)?;
                let m = Manifest::from_pages(pages).map_err(wire_path)?;
                if m.files() != e.files || m.bytes() != e.bytes || m.hash() != e.manifest_hash {
                    return Err(SendError::Protocol(
                        "the manifest does not match its end".into(),
                    ));
                }
                return Ok(m);
            }
            JobOpenAck::TYPE => {
                let a: JobOpenAck = f.decode().map_err(proto)?;
                if a.status != gen::STATUS_OK {
                    return Err(SendError::Refused {
                        status: a.status,
                        message: a.message.unwrap_or_default(),
                    });
                }
            }
            _ => {}
        }
    }
}

/// The opener side of a download (SPEC.md §11.5): JobOpen{JOB_DOWNLOAD, root = the source
/// on the peer, ext credit = the grant} → manifest. The lanes are joined first, so they
/// are already up when the responder adopts them.
pub async fn download_open(
    link: &mut JobLink,
    src_root: &str,
    flags: u32,
    credit: u64,
) -> Result<Arc<Manifest>, SendError> {
    if let Some(op) = link.opener().cloned() {
        while link.lanes().len() < crate::governor::START_LANES as usize {
            op.open()
                .await
                .map_err(|e| SendError::Disconnected(e.to_string()))?;
        }
    }
    link.control
        .send(&JobOpen {
            job_id: link.job_id,
            kind: gen::JOB_DOWNLOAD,
            policy: 0,
            flags,
            root: src_root.into(),
            src: None,
            credit: Some(credit),
        })
        .await
        .map_err(|e| SendError::Disconnected(e.to_string()))?;
    Ok(Arc::new(read_manifest(link).await?))
}

/// The opener side of a download, data phase: the map is answered, then `run`.
pub async fn download_run(
    link: &mut JobLink,
    manifest: Arc<Manifest>,
    need: Option<Need>,
    sink: Arc<dyn Sink>,
    o: RecvOptions,
) -> Result<RecvReport, SendError> {
    run(link, manifest, need, sink, o).await
}

/// One download from open to report: `download_open` then `download_run(None)`.
pub async fn download_job(
    link: &mut JobLink,
    src_root: &str,
    flags: u32,
    sink: Arc<dyn Sink>,
    mut o: RecvOptions,
) -> Result<RecvReport, SendError> {
    let credit = o.credit;
    let m = download_open(link, src_root, flags, credit).await?;
    o.flags = flags; // ruling 12: `run` reads only `o.flags`
    download_run(link, m, None, sink, o).await
}

/// The responder side of an upload (a host for uploads): the JobOpen already arrived.
/// The ack carries the job's absolute grant (the extra credit note: `Credit` frames are
/// incremental; only the ack sets the sender's window).
pub async fn receive_job(
    link: &mut JobLink,
    open: JobOpen,
    sink: Arc<dyn Sink>,
    mut o: RecvOptions,
) -> Result<RecvReport, SendError> {
    o.flags = open.flags; // ruling 12 / Q4: the journal's Open records the job's flags
    link.control
        .send(&JobOpenAck {
            job_id: link.job_id,
            status: gen::STATUS_OK,
            credit: o.credit,
            staged: 0,
            workers: 4,
            message: None,
        })
        .await
        .map_err(|e| SendError::Disconnected(e.to_string()))?;
    let m = match read_manifest(link).await {
        Ok(m) => m,
        Err(e) => {
            // The ack is already out; the sender learns about a bad manifest through the
            // JobDone (its open loop reads both), not a second ack.
            let status = match &e {
                SendError::Refused { status, .. } => *status,
                _ => gen::ERR_PROTOCOL,
            };
            let _ = link
                .control
                .send(&JobDone {
                    job_id: link.job_id,
                    status,
                    files: 0,
                    bytes: 0,
                    message: Some(e.to_string()),
                })
                .await;
            return Err(e);
        }
    };
    run(link, Arc::new(m), None, sink, o).await
}

/// The responder side of a `Resume` (SPEC.md §11.5): the stored manifest is already in hand, so
/// there is no ack or manifest exchange — the job's `JobMap` goes out and the job continues
/// exactly as a `JobOpen` resume would (journal replay, §13.4 re-check, map, data).
pub async fn resume_job(
    link: &mut JobLink,
    manifest: Manifest,
    sink: Arc<dyn Sink>,
    o: RecvOptions,
) -> Result<RecvReport, SendError> {
    // No ack carries the grant on this path, so it goes out as a `Credit` (SPEC.md §11.5).
    link.control
        .send(&Credit {
            job_id: link.job_id,
            bytes: o.credit,
        })
        .await
        .map_err(|e| SendError::Disconnected(e.to_string()))?;
    run(link, Arc::new(manifest), None, sink, o).await
}

struct Large {
    /// The group CVs, shared with the sync batch: the batch syncs the very instance the
    /// loop puts CVs into (the outboard's shadow-rename model forbids a second instance
    /// of the same path), so puts and the batch's sync serialize behind this lock.
    hasher_cvs: Option<Arc<Mutex<Outboard>>>,
    written: RangeSet,
    durable: RangeSet,
    root: Option<[u8; 32]>,
}

fn new_large(dir: &std::path::Path, m: &Manifest, id: u32) -> Large {
    let size = m
        .entry(id)
        .expect("an id the receiver validated against the manifest")
        .size;
    Large {
        hasher_cvs: (verify::groups(size) >= 2)
            .then(|| Outboard::open(&dir.join(format!("{id}.ob")), verify::groups(size)).ok())
            .flatten()
            .map(|ob| Arc::new(Mutex::new(ob))),
        written: RangeSet::new(),
        durable: RangeSet::new(),
        root: None,
    }
}

/// A chunk (SPEC.md §12.2, the C receiver's ava1_apply_chunk): inside the file, group
/// aligned, whole groups unless it ends the file. A wire-supplied id the manifest does
/// not carry is a protocol error — never a panic (ruling 3).
async fn apply_chunk(
    sink: &Arc<dyn Sink>,
    m: &Arc<Manifest>,
    dir: &std::path::Path,
    large: &mut HashMap<u32, Large>,
    id: u32,
    off: u64,
    data: Vec<u8>,
) -> Result<(), SendError> {
    let Some(e) = m.entry(id) else {
        return Err(SendError::Protocol(format!(
            "a chunk names file {id}, which this manifest has none of"
        )));
    };
    if e.kind != gen::ENTRY_FILE {
        return Err(SendError::Protocol(format!(
            "a chunk names {id}, which is not a file"
        )));
    }
    let size = e.size;
    let len = data.len() as u64;
    if !off.is_multiple_of(GROUP)
        || off > size
        || len > size - off
        || (!len.is_multiple_of(GROUP) && off + len != size)
    {
        return Err(SendError::Protocol("a chunk outside its file".into()));
    }
    let l = large.entry(id).or_insert_with(|| new_large(dir, m, id));
    let s2 = sink.clone();
    let data = Arc::new(data);
    let d2 = data.clone();
    tokio::task::spawn_blocking(move || s2.write_at(id, off, &d2))
        .await
        .map_err(proto)??;
    // The CVs are hashed before the outboard's lock is taken (BLAKE3 is the slow part);
    // the batch task's sync holds the lock only for the shadow rename, never for a hash.
    let mut cvs = Vec::with_capacity(data.len().div_ceil(GROUP as usize));
    for (k, g) in data.chunks(GROUP as usize).enumerate() {
        let gi = off / GROUP + k as u64;
        cvs.push((gi, verify::group_cv(g, gi)));
    }
    if let Some(ob) = l.hasher_cvs.as_ref() {
        let mut ob = ob.lock().unwrap();
        for (gi, cv) in cvs {
            ob.put(gi, &cv)?;
        }
    }
    l.written.insert(off, off + len);
    Ok(())
}

/// From here on both sides are identical: journal, map, apply, sync, commit, finish
/// (SPEC.md §12.6, §13, §14).
///
/// The loop runs in `run_loop`; this wrapper joins the sync batch on every exit path.
/// A dropped handle detaches the task, and a detached batch keeps appending to the
/// job's journal after the job is gone — two writers against the journal's
/// single-writer directory if the peer reopens the same job id (ruling 19). Joining
/// cannot deadlock: the batch awaits only its own `spawn_blocking` I/O and the
/// control outbox, whose sends fail as soon as the session's writer task ends
/// (bounded by `dead_after`), never anything the run loop holds.
async fn run(
    link: &mut JobLink,
    m: Arc<Manifest>,
    need_hint: Option<Need>,
    sink: Arc<dyn Sink>,
    o: RecvOptions,
) -> Result<RecvReport, SendError> {
    let mut batch_handle: Option<tokio::task::JoinHandle<Result<BatchDone, SendError>>> = None;
    let outcome = run_loop(link, m, need_hint, sink, o, &mut batch_handle).await;
    if let Some(h) = batch_handle.take() {
        let _ = h.await;
    }
    outcome
}

async fn run_loop(
    link: &mut JobLink,
    m: Arc<Manifest>,
    need_hint: Option<Need>,
    sink: Arc<dyn Sink>,
    o: RecvOptions,
    batch_handle: &mut Option<tokio::task::JoinHandle<Result<BatchDone, SendError>>>,
) -> Result<RecvReport, SendError> {
    let job_id = link.job_id;
    let dir = journal::job_dir(&o.jobs_dir, &job_id);
    std::fs::create_dir_all(&dir)?;
    // The destination root and the staging decision go into the journal's Open, exactly as
    // the C receiver records them (ava1_recv.c:380-430 decides, :1026-1040 writes), so
    // either side can resume a job the other started. The sink supplies both (ruling Q1).
    let (dest_root, staged) = sink.resume_key().unwrap_or((String::new(), false));
    let fresh = JnlOpen {
        job_id,
        manifest_hash: m.hash(),
        kind: gen::JOB_DOWNLOAD,
        flags: o.flags,
        staged: u8::from(staged),
        root: dest_root.clone(),
    };
    let mut st = State::default();
    let (mut jnl, open_rec) = match Journal::open(&dir) {
        Ok((j, recs)) => {
            for r in &recs {
                st.apply(r);
            }
            match st.open.clone().filter(|jo| {
                jo.kind == gen::JOB_DOWNLOAD
                    && jo.flags == o.flags
                    && jo.root == dest_root
                    && jo.manifest_hash == m.hash()
            }) {
                Some(jo) => (j, jo), // resume: replay + the recorded Open
                None => {
                    drop(j); // one writer per directory (ruling 19): a different job
                    st = State::default(); // start over
                    journal::write_manifest(&dir, &m)?;
                    let j = Journal::create(&dir, &fresh)?;
                    st.apply(&Record::Open(fresh.clone()));
                    (j, fresh.clone())
                }
            }
        }
        Err(_) => {
            journal::write_manifest(&dir, &m)?;
            let j = Journal::create(&dir, &fresh)?;
            st.apply(&Record::Open(fresh.clone()));
            (j, fresh.clone())
        }
    };
    let s2 = sink.clone();
    let m2 = m.clone();
    tokio::task::spawn_blocking(move || s2.prepare(&m2))
        .await
        .map_err(proto)??;

    if sink.transient_relay() {
        if let Some(hint) = &need_hint {
            st.done = hint.done.clone();
            st.ranges = hint.partial.clone();
        }
    }

    // Resume check (SPEC.md §13.4): every durable group of every partial file is re-hashed
    // against the sink and the outboard; a mismatch resets the file before the map.
    let mut large: HashMap<u32, Large> = HashMap::new();
    for (id, r) in st.ranges.clone() {
        let Some(e) = m.entry(id) else {
            // A journal id the manifest does not carry (corrupt state, or a hash
            // collision): an error ends the job, never a panic on the job task (ruling
            // 3 covers wire ids; this is the journal's).
            return Err(SendError::Protocol(format!(
                "the journal names file {id}, which this manifest has none of"
            )));
        };
        let size = e.size;
        let mut ob = Outboard::open(&dir.join(format!("{id}.ob")), verify::groups(size)).ok();
        let mut good = RangeSet::new();
        for (s, e) in r.iter() {
            if sink.transient_relay() {
                good.insert(s, e);
                continue;
            }
            let mut g = s / GROUP;
            while g * GROUP < e {
                let len = (size - g * GROUP).min(GROUP) as usize;
                let mut buf = vec![0u8; len];
                let ok = sink.read_at(id, g * GROUP, &mut buf).is_ok()
                    && (verify::groups(size) < 2
                        || ob.as_ref().and_then(|o| o.get(g)) == Some(verify::group_cv(&buf, g)));
                if ok {
                    good.insert(g * GROUP, g * GROUP + len as u64);
                }
                g += 1;
            }
        }
        if good.covered() != r.covered() {
            let rec = Record::Reset(id);
            st.apply(&rec);
            jnl.append(&rec)?;
            ob = Outboard::open(&dir.join(format!("{id}.ob")), verify::groups(size)).ok();
            good = RangeSet::new();
        }
        st.ranges.insert(id, good.clone());
        large.insert(
            id,
            Large {
                hasher_cvs: ob.map(|o| Arc::new(Mutex::new(o))),
                written: RangeSet::new(),
                durable: good,
                root: st.roots.get(&id).copied(),
            },
        );
    }
    // What the job still needs: the relay's hint when given (Task 24), else the replayed
    // state after the resume check (ruling 4: build it here; never change journal.rs).
    let ordered_skip = if sink.transient_relay() {
        need_hint.clone()
    } else {
        None
    };
    let need = match need_hint {
        Some(n) => n,
        None => Need {
            done: st.done.clone(),
            partial: st
                .ranges
                .iter()
                .filter(|(id, _)| !st.done.contains(id))
                .map(|(id, rs)| (*id, rs.clone()))
                .collect(),
        },
    };
    for p in need.to_pages(job_id, gen::STATUS_OK) {
        link.control
            .send(&p)
            .await
            .map_err(|e| SendError::Disconnected(e.to_string()))?;
    }
    // Progress: the totals, and the durable counters seeded from the replay so a resumed
    // run reports what the journal already knows (preflight row 21; Task 18's resume test
    // asserts bytes_durable on the resumed run).
    let pg = o.progress.clone();
    pg.bytes_total.store(m.bytes(), Ordering::Relaxed);
    pg.files_total.store(m.files() as u64, Ordering::Relaxed);
    pg.files_durable
        .store(st.done.len() as u64, Ordering::Relaxed);
    let mut durable_bytes = 0u64;
    for id in &st.done {
        let Some(e) = m.entry(*id) else {
            return Err(SendError::Protocol(format!(
                "the journal names file {id}, which this manifest has none of"
            )));
        };
        durable_bytes += e.size;
    }
    pg.bytes_durable.store(
        durable_bytes + st.ranges.values().map(|r| r.covered()).sum::<u64>(),
        Ordering::Relaxed,
    );

    let total_files = m.files() as usize;
    let mut done: BTreeSet<u32> = st.done.clone();
    let mut pending_small: Vec<u32> = Vec::new();
    // A zero-byte file is already complete: its zero range yields no chunks and, in an
    // ordered download, no bundle either (the ordered sender sends every file as chunks),
    // so no frame will ever arrive to mark it done — and the ordered cursor would stall
    // on it forever, holding every later file up. Create the empty file up front and let
    // the first batch journal it durable (SPEC.md §12.6: done always follows Durable), so
    // it completes on its own whether or not anything arrives for it.
    let zero: BTreeSet<u32> = m
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.kind == gen::ENTRY_FILE && e.size == 0)
        .map(|(i, _)| i as u32)
        .collect();
    for id in &zero {
        if done.contains(id) {
            continue; // a resume: the journal already made it durable
        }
        let (s2, id) = (sink.clone(), *id);
        tokio::task::spawn_blocking(move || s2.write_whole(id, &[]))
            .await
            .map_err(proto)??;
        pending_small.push(id);
    }
    let granted = o.credit;
    let mut credit_back = 0u64;
    // The credit still outstanding, exactly as the C receiver counts it (w_avail):
    // the grant, minus every frame received, plus every Credit frame sent back — the
    // sender's in-flight mirror. A job receiving more than `granted` in total is fine
    // (every returned Credit re-opens the window); only more than `granted` at once is
    // not (SPEC.md §12.4, ledger row 19).
    let mut outstanding = granted;
    let mut last_batch = Instant::now();
    // Out-of-order frames are bounded by the credit granted in JobOpen: the sender cannot
    // have more than one window in flight (SPEC.md §12.4). `whole` says the entry is a
    // root-checked bundle record rather than a chunk.
    let mut reorder: BTreeMap<(u32, u64), (bool, Vec<u8>)> = BTreeMap::new();
    let mut cursor: (u32, u64) = (0, 0);
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The journal and the applied state move into each sync batch and come back with it;
    // while a batch runs the loop never touches them, and the finish paths below only run
    // when no batch is in flight.
    let mut jnl = Some(jnl);
    let mut st = Some(st);
    loop {
        if o.cancel.load(Ordering::Relaxed) {
            let _ = link
                .control
                .send(&JobCancel {
                    job_id,
                    reason: gen::ERR_CANCELLED,
                })
                .await;
            return Err(SendError::Cancelled);
        }
        // A job task never stops reading (ruling 11): the inbox is drained continuously and
        // disk work runs on spawn_blocking — including the sync batch, which runs on a task
        // of its own (`batch_handle`) while this loop keeps routing frames and answering the
        // peer; awaiting room on the control outbox is the backpressure, so a peer that
        // stops reading slows this loop — never a queue here.
        let ev = tokio::select! {
            ev = next(link) => Some(ev?),
            _ = tick.tick() => None,
            joined = join_batch(batch_handle), if batch_handle.is_some() => {
                let b = joined.expect("guarded by `is_some`");
                *batch_handle = None;
                match b {
                    Ok(out) => {
                        fold_batch(&mut done, &mut large, &pg, &m, &out);
                        jnl = Some(out.jnl);
                        st = Some(out.st);
                    }
                    Err(e) => return Err(e),
                }
                continue;
            }
        };
        // Ruling 2: `ev` is matched below and read by `is_none` — bind first.
        let idle = ev.is_none();
        match ev {
            Some(Inbound::Lane { lane, frame }) => {
                let len = frame.body.len() as u64;
                // SPEC.md §12.4 (ledger row 19): a frame that exceeds the credit this job
                // still has outstanding is refused with ERR_CREDIT on that lane; nothing of
                // it is buffered or acknowledged. `outstanding` = granted − received +
                // returned, the sender's in-flight mirror (the C receiver's w_avail).
                if len > outstanding {
                    if let Some(l) = link.lane(lane) {
                        // The sealed Error on the offending lane (SPEC.md §12.4): the peer's
                        // link closes that lane when it reads it, which the sender observes
                        // exactly as the C receiver's explicit close: its lane dies.
                        let _ =
                            l.tx.send(&gen::Error {
                                code: gen::ERR_CREDIT,
                                message: "a frame larger than the credit granted".into(),
                            })
                            .await;
                    }
                    // Ruling: lane only. The sealed Error ends that lane when the peer reads
                    // it (SPEC.md §12.4); the session and the job stay, other lanes go on,
                    // and the refused frame is requeued by the sender as for any dead lane.
                    continue;
                }
                link.control
                    .send(&Received {
                        job_id,
                        lane,
                        seq: frame.channel,
                    })
                    .await
                    .map_err(|e| SendError::Disconnected(e.to_string()))?;
                outstanding -= len;

                match frame.ty {
                    Chunk::TYPE => {
                        let c: Chunk = frame.decode().map_err(proto)?;
                        if done.contains(&c.file_id) {
                            credit_back += len;
                            continue;
                        }
                        if o.ordered {
                            reorder.insert((c.file_id, c.offset), (false, c.data));
                        } else {
                            apply_chunk(&sink, &m, &dir, &mut large, c.file_id, c.offset, c.data)
                                .await?;
                        }
                    }
                    Bundle::TYPE => {
                        let b: Bundle = frame.decode().map_err(proto)?;
                        for r in b.records {
                            let Some(e) = m.entry(r.file_id) else {
                                return Err(SendError::Protocol(format!(
                                    "a bundle names file {}, which this manifest has none of",
                                    r.file_id
                                )));
                            };
                            if e.kind != gen::ENTRY_FILE {
                                return Err(SendError::Protocol(format!(
                                    "a bundle names {id}, which is not a file",
                                    id = r.file_id
                                )));
                            }
                            if done.contains(&r.file_id) {
                                continue;
                            }
                            // The C receiver's apply_record: a size mismatch is a file that
                            // changed under the sender (it re-reads), a root mismatch a bad
                            // transfer — FileRetry, never a silent skip that would stall the
                            // job with the file never marked done.
                            if r.data.len() as u64 != e.size {
                                link.control
                                    .send(&gen::FileRetry {
                                        job_id,
                                        file_id: r.file_id,
                                        reason: gen::RETRY_CHANGED,
                                    })
                                    .await
                                    .map_err(|e| SendError::Disconnected(e.to_string()))?;
                                continue;
                            }
                            if *blake3::hash(&r.data).as_bytes() != r.root {
                                link.control
                                    .send(&gen::FileRetry {
                                        job_id,
                                        file_id: r.file_id,
                                        reason: gen::RETRY_VERIFY,
                                    })
                                    .await
                                    .map_err(|e| SendError::Disconnected(e.to_string()))?;
                                continue;
                            }
                            if o.ordered {
                                reorder.insert((r.file_id, 0), (true, r.data));
                            } else {
                                let s2 = sink.clone();
                                tokio::task::spawn_blocking(move || {
                                    s2.write_whole(r.file_id, &r.data)
                                })
                                .await
                                .map_err(proto)??;
                                pending_small.push(r.file_id);
                            }
                        }
                    }
                    _ => {}
                }
                credit_back += len;
                if o.ordered {
                    // Feed the sink strictly in (file, offset) order; skip files already
                    // done — and zero-byte files, which no frame will ever describe: the
                    // cursor must not wait for a chunk that cannot arrive.
                    loop {
                        while (cursor.0 as usize) < m.entries.len()
                            && (m
                                .entry(cursor.0)
                                .expect("the cursor is inside the manifest")
                                .kind
                                != gen::ENTRY_FILE
                                || done.contains(&cursor.0)
                                || zero.contains(&cursor.0))
                        {
                            cursor = (cursor.0 + 1, 0);
                        }
                        // A relay's source sender omits groups B already has and
                        // whose CV the engine retained. Advance over those gaps;
                        // otherwise the ordered cursor waits forever at offset 0
                        // while later chunks accumulate in `reorder`.
                        if let Some(end) = ordered_skip
                            .as_ref()
                            .and_then(|skip| skip.partial.get(&cursor.0))
                            .and_then(|ranges| {
                                ranges
                                    .iter()
                                    .find(|(start, end)| *start <= cursor.1 && cursor.1 < *end)
                            })
                            .map(|(_, end)| end)
                        {
                            let size = m.entry(cursor.0).map_or(0, |e| e.size);
                            cursor = if end >= size {
                                (cursor.0 + 1, 0)
                            } else {
                                (cursor.0, end)
                            };
                            continue;
                        }
                        let Some((whole, data)) = reorder.remove(&cursor) else {
                            break;
                        };
                        let (fid, off, n) = (cursor.0, cursor.1, data.len() as u64);
                        if whole {
                            // A bundle record: its root was checked when it arrived, so the
                            // whole-file write is sound without the chunk machinery.
                            let s2 = sink.clone();
                            tokio::task::spawn_blocking(move || s2.write_whole(fid, &data))
                                .await
                                .map_err(proto)??;
                            pending_small.push(fid);
                        } else {
                            apply_chunk(&sink, &m, &dir, &mut large, fid, off, data).await?;
                        }
                        cursor = if off + n
                            >= m.entry(fid)
                                .expect("the cursor is inside the manifest")
                                .size
                        {
                            (fid + 1, 0)
                        } else {
                            (fid, off + n)
                        };
                    }
                }
            }
            Some(Inbound::Control(f)) => match f.ty {
                FileRoot::TYPE => {
                    let r: FileRoot = f.decode().map_err(proto)?;
                    let Some(e) = m.entry(r.file_id) else {
                        return Err(SendError::Protocol(format!(
                            "a root names file {}, which this manifest has none of",
                            r.file_id
                        )));
                    };
                    if e.kind != gen::ENTRY_FILE {
                        return Err(SendError::Protocol(format!(
                            "a root names {id}, which is not a file",
                            id = r.file_id
                        )));
                    }
                    large
                        .entry(r.file_id)
                        .or_insert_with(|| new_large(&dir, &m, r.file_id))
                        .root = Some(r.root);
                }
                JobCancel::TYPE => {
                    return Err(SendError::Refused {
                        status: gen::ERR_CANCELLED,
                        message: "the sender cancelled".into(),
                    });
                }
                _ => {}
            },
            Some(Inbound::LaneUp(_)) | Some(Inbound::LaneDown(_)) => {}
            // `next` maps Closed to an error; this arm documents the shape.
            Some(Inbound::Closed(why)) => return Err(SendError::Disconnected(why)),
            None => {}
        }
        if credit_back >= 4 << 20 || (credit_back > 0 && idle) {
            link.control
                .send(&Credit {
                    job_id,
                    bytes: credit_back,
                })
                .await
                .map_err(|e| SendError::Disconnected(e.to_string()))?;
            outstanding += credit_back;
            credit_back = 0;
        }
        if batch_handle.is_none() {
            let finished = done.len() >= total_files && pending_small.is_empty();
            if finished {
                let mut jnl = jnl.take().expect("a finished job has no batch in flight");
                let mut st = st.take().expect("a finished job has no batch in flight");
                let s2 = sink.clone();
                if let Err(e) = tokio::task::spawn_blocking(move || s2.finish())
                    .await
                    .map_err(proto)?
                {
                    // A failure after every byte is durable: report it, never ask for a resend.
                    let status = if e.kind() == io::ErrorKind::AlreadyExists {
                        gen::ERR_EXISTS
                    } else {
                        gen::ERR_IO
                    };
                    let rec = Record::Done(status);
                    st.apply(&rec);
                    jnl.append(&rec)?;
                    link.control
                        .send(&JobDone {
                            job_id,
                            status,
                            files: m.files(),
                            bytes: m.bytes(),
                            message: Some(e.to_string()),
                        })
                        .await
                        .map_err(|e| SendError::Disconnected(e.to_string()))?;
                    return Err(SendError::Refused {
                        status,
                        message: e.to_string(),
                    });
                }
                let rec = Record::Done(0);
                st.apply(&rec);
                jnl.append(&rec)?;
                link.control
                    .send(&JobDone {
                        job_id,
                        status: gen::STATUS_OK,
                        files: m.files(),
                        bytes: m.bytes(),
                        message: None,
                    })
                    .await
                    .map_err(|e| SendError::Disconnected(e.to_string()))?;
                return Ok(RecvReport {
                    files: m.files(),
                    bytes: m.bytes(),
                    manifest: m,
                });
            } else if last_batch.elapsed() >= Duration::from_millis(250) {
                last_batch = Instant::now();
                let snap = snapshot_batch(
                    job_id,
                    link.control.clone(),
                    &sink,
                    &m,
                    jnl.take().expect("no batch in flight"),
                    &open_rec,
                    st.take().expect("no batch in flight"),
                    &pg,
                    &mut pending_small,
                    &mut large,
                )?;
                *batch_handle = Some(tokio::task::spawn(batch_task(snap)));
            }
        }
    }
}

/// The work of one sync batch, owned: the snapshot is taken in the run loop (fast, no
/// I/O) so the batch's blocking I/O can run on its own task while the loop keeps draining
/// the inbox and answering control traffic (ruling 11). `jnl` and `st` move with the job
/// and come back in `BatchDone`, so the loop's journal and applied state are never shared.
struct BatchJob {
    job_id: [u8; 16],
    control: ConnTx,
    sink: Arc<dyn Sink>,
    jnl: Journal,
    open_rec: JnlOpen,
    st: State,
    pg: Arc<Progress>,
    /// The small files (whole bundle records) made durable by this batch.
    small: BTreeSet<u32>,
    /// The large files' ranges made durable by this batch.
    ranges: Vec<FileRange>,
    /// The roots to journal (every large file that has one).
    roots: Vec<RootItem>,
    /// One entry per large file with written data: its outboard — the same instance the
    /// loop keeps putting CVs into — and what was durable before this batch, so the commit
    /// check sees the merged coverage without touching the loop's map.
    large: Vec<BatchLarge>,
}

struct BatchLarge {
    id: u32,
    size: u64,
    prior_durable: RangeSet,
    root: Option<[u8; 32]>,
    ob: Option<Arc<Mutex<Outboard>>>,
}

/// What the batch hands back to the loop, which folds it into the live state.
struct BatchDone {
    jnl: Journal,
    st: State,
    /// Files made durable by the main record (the journal's files runs).
    small: BTreeSet<u32>,
    /// Large files committed (part → final): the fold removes them from `large`.
    committed: Vec<u32>,
    /// Large files whose root mismatched (Reset journaled, FileRetry sent): the fold
    /// removes them so the next chunk starts them fresh, exactly as before.
    reset: Vec<u32>,
    /// The large-file ranges this batch made durable: the fold subtracts each from
    /// `written` — the loop keeps writing while the batch runs, so a whole-file clear (the
    /// old behaviour) would drop the ranges written after the snapshot — and inserts it
    /// into `durable`.
    ranges: Vec<FileRange>,
}

/// Takes the snapshot of the pending work: the pending small files, the large files'
/// written ranges and roots, their outboards, the journal and the applied state. No I/O
/// (ruling 11: the loop must not block), so the loop keeps routing frames while
/// `batch_task` runs.
#[allow(clippy::too_many_arguments)]
fn snapshot_batch(
    job_id: [u8; 16],
    control: ConnTx,
    sink: &Arc<dyn Sink>,
    m: &Arc<Manifest>,
    jnl: Journal,
    open_rec: &JnlOpen,
    st: State,
    pg: &Arc<Progress>,
    small: &mut Vec<u32>,
    large: &mut HashMap<u32, Large>,
) -> Result<BatchJob, SendError> {
    let mut ranges = Vec::new();
    let mut roots = Vec::new();
    let mut entries = Vec::new();
    for (id, l) in large.iter() {
        if let Some(root) = l.root {
            roots.push(RootItem { file_id: *id, root });
        }
        let Some(e) = m.entry(*id) else {
            // `large` holds ids the journal replayed; a bad one is an error, not a panic
            // on the job task.
            return Err(SendError::Protocol(format!(
                "file {id} has written data but this manifest has none of it"
            )));
        };
        // A file whose bytes were all made durable by an earlier batch but whose root
        // arrived only afterwards (the root rides the control connection, the chunks the
        // lanes: either may win) still has to be committed: without this it was skipped
        // forever, the job never finished, and the receiver sat idle (T28: the "hang").
        let awaiting_commit = l.root.is_some() && l.durable.is_full(e.size);
        if l.written.covered() == 0 && !awaiting_commit {
            continue;
        }
        let size = e.size;
        for (s, e) in l.written.iter() {
            ranges.push(FileRange {
                file_id: *id,
                offset: s,
                len: e - s,
            });
        }
        entries.push(BatchLarge {
            id: *id,
            size,
            prior_durable: l.durable.clone(),
            root: l.root,
            ob: l.hasher_cvs.clone(),
        });
    }
    Ok(BatchJob {
        job_id,
        control,
        sink: sink.clone(),
        jnl,
        open_rec: open_rec.clone(),
        st,
        pg: pg.clone(),
        small: std::mem::take(small).into_iter().collect(),
        ranges,
        roots,
        large: entries,
    })
}

/// Sync → journal → Durable, then commit the complete large files (SPEC.md §12.6). The
/// durability ordering is unchanged — the sink's bytes, then the outboards, then the
/// journal record, then the Durable frames, then the commits — but it all runs off the
/// run-loop task, so the loop keeps draining its inbox and answering the peer while the
/// fsyncs run (a receiver that goes silent for seconds loses the session: the peer's
/// heartbeats go unanswered, and a sender's credit stalls too). Every appended record is
/// applied to `st` and, once the journal passes COMPACT_AT, it is compacted from exactly
/// the Open record that created it and the applied state (ruling 5).
async fn batch_task(job: BatchJob) -> Result<BatchDone, SendError> {
    let BatchJob {
        job_id,
        control,
        sink,
        jnl,
        open_rec,
        mut st,
        pg,
        small,
        ranges,
        roots,
        large,
    } = job;
    let mut jnl = jnl;
    // Nothing written since the last batch: nothing to sync, journal or send.
    if small.is_empty() && ranges.is_empty() && roots.is_empty() {
        return Ok(BatchDone {
            jnl,
            st,
            small,
            committed: Vec::new(),
            reset: Vec::new(),
            ranges,
        });
    }
    // The bytes first: the small files and every large file with a new range (the same
    // ids the old inline batch synced).
    let sync_ids: Vec<u32> = small
        .iter()
        .copied()
        .chain(large.iter().map(|l| l.id))
        .collect();
    if !sync_ids.is_empty() {
        let s2 = sink.clone();
        let ids = sync_ids;
        tokio::task::spawn_blocking(move || s2.sync(&ids))
            .await
            .map_err(proto)??;
    }
    // The outboards: sync the very instance the loop is still putting CVs into (a second
    // instance would rebuild its image from disk and drop the in-flight puts), so the
    // loop's puts serialize with the sync behind the same lock.
    let obs: Vec<Arc<Mutex<Outboard>>> = large.iter().filter_map(|l| l.ob.clone()).collect();
    if !obs.is_empty() {
        tokio::task::spawn_blocking(move || {
            for ob in &obs {
                ob.lock().unwrap().sync()?;
            }
            Ok::<(), io::Error>(())
        })
        .await
        .map_err(proto)??;
    }
    let rec = Record::Batch(JnlBatch {
        files: runs(&small),
        ranges: ranges.clone(),
        roots,
    });
    let (j, rec, r) = tokio::task::spawn_blocking(move || {
        let r = jnl.append(&rec);
        (jnl, rec, r)
    })
    .await
    .map_err(proto)?;
    jnl = j;
    r?;
    st.apply(&rec);
    for r in &ranges {
        pg.bytes_durable.fetch_add(r.len, Ordering::Relaxed);
    }
    for chunk in ranges.chunks(2000) {
        control
            .send(&Durable {
                job_id,
                files: Vec::new(),
                ranges: chunk.to_vec(),
            })
            .await
            .map_err(|e| SendError::Disconnected(e.to_string()))?;
    }
    for chunk in runs(&small).chunks(2000) {
        control
            .send(&Durable {
                job_id,
                files: chunk.to_vec(),
                ranges: Vec::new(),
            })
            .await
            .map_err(|e| SendError::Disconnected(e.to_string()))?;
    }
    // Commit the complete large files: the merged coverage is the prior durable state
    // (from the snapshot) plus this batch's ranges — the same view the old inline batch
    // had after applying the record.
    let mut committed = Vec::new();
    let mut reset = Vec::new();
    for l in &large {
        let Some(root) = l.root else {
            continue;
        };
        let mut durable = l.prior_durable.clone();
        for r in ranges.iter().filter(|r| r.file_id == l.id) {
            durable.insert(r.offset, r.offset + r.len);
        }
        if !durable.is_full(l.size) {
            continue;
        }
        let actual = if sink.transient_relay() {
            // B owns durability and checks the complete root. A's skipped bytes
            // exist only on B, so its transient sink cannot reread them here.
            Some(root)
        } else if verify::groups(l.size) >= 2 {
            let ob = l
                .ob
                .as_ref()
                .ok_or_else(|| io::Error::other("the outboard of a multi-group file is missing"))?;
            let cvs: Option<Vec<[u8; 32]>> = {
                let ob = ob.lock().unwrap();
                (0..verify::groups(l.size)).map(|g| ob.get(g)).collect()
            };
            cvs.map(|c| verify::root_from_cvs(&c))
        } else {
            let mut buf = vec![0u8; l.size as usize];
            let s2 = sink.clone();
            let id = l.id;
            let (buf, n) = tokio::task::spawn_blocking(move || {
                let n = s2.read_at(id, 0, &mut buf);
                (buf, n)
            })
            .await
            .map_err(proto)?;
            n.ok().map(|_| *blake3::hash(&buf).as_bytes())
        };
        if actual != Some(root) {
            let rec = Record::Reset(l.id);
            let (j, rec, r) = tokio::task::spawn_blocking(move || {
                let r = jnl.append(&rec);
                (jnl, rec, r)
            })
            .await
            .map_err(proto)?;
            jnl = j;
            r?;
            st.apply(&rec);
            control
                .send(&gen::FileRetry {
                    job_id,
                    file_id: l.id,
                    reason: gen::RETRY_VERIFY,
                })
                .await
                .map_err(|e| SendError::Disconnected(e.to_string()))?;
            reset.push(l.id);
            continue;
        }
        let s2 = sink.clone();
        let id = l.id;
        tokio::task::spawn_blocking(move || s2.commit(id))
            .await
            .map_err(proto)??;
        committed.push(id);
    }
    // ONE journal record, one drive flush and one Durable for every file this batch
    // committed (T28): per-file records meant a full-drive flush per file, so an ordered
    // download (every file goes through the large path) crawled and, under load, looked hung.
    if !committed.is_empty() {
        let set: BTreeSet<u32> = committed.iter().copied().collect();
        let rec = Record::Batch(JnlBatch {
            files: runs(&set),
            ranges: Vec::new(),
            roots: Vec::new(),
        });
        let (j, rec, r) = tokio::task::spawn_blocking(move || {
            let r = jnl.append(&rec);
            (jnl, rec, r)
        })
        .await
        .map_err(proto)?;
        jnl = j;
        r?;
        st.apply(&rec);
        pg.files_durable
            .fetch_add(committed.len() as u64, Ordering::Relaxed);
        for chunk in runs(&set).chunks(2000) {
            control
                .send(&Durable {
                    job_id,
                    files: chunk.to_vec(),
                    ranges: Vec::new(),
                })
                .await
                .map_err(|e| SendError::Disconnected(e.to_string()))?;
        }
    }
    // The journal is compacted once it passes COMPACT_AT (ruling 5): the engine-side
    // journal must not grow without bound, and the C side does the same.
    if jnl.len() > journal::COMPACT_AT {
        let (j, st2, r) = tokio::task::spawn_blocking(move || {
            let r = jnl.compact(&open_rec, &st);
            (jnl, st, r)
        })
        .await
        .map_err(proto)?;
        jnl = j;
        st = st2;
        r?;
    }
    Ok(BatchDone {
        jnl,
        st,
        small,
        committed,
        reset,
        ranges,
    })
}

/// Awaits the in-flight batch by reference — the handle stays in the slot, so if the
/// select resolves through another arm the borrow simply ends and the next iteration
/// keeps joining (the guard stays true the whole time); the run loop clears the slot
/// after a join completes. `None` when nothing is running.
async fn join_batch(
    h: &mut Option<tokio::task::JoinHandle<Result<BatchDone, SendError>>>,
) -> Option<Result<BatchDone, SendError>> {
    let t = h.as_mut()?;
    Some(match t.await {
        Ok(r) => r,
        Err(e) => Err(SendError::Disconnected(format!(
            "the sync batch task died: {e}"
        ))),
    })
}

/// Folds a finished batch into the loop's live state. The loop kept writing while the
/// batch ran, so `written` loses only the ranges the batch journaled — never a
/// whole-file clear, which would drop the ranges written after the snapshot.
fn fold_batch(
    done: &mut BTreeSet<u32>,
    large: &mut HashMap<u32, Large>,
    pg: &Progress,
    m: &Manifest,
    b: &BatchDone,
) {
    // The done-insert guard is the one the old inline batch used for the small files: a
    // bundle that re-arrived while the batch ran (the done fold was still pending) can be
    // batched twice, and the duplicate must not count twice.
    for id in &b.small {
        if done.insert(*id) {
            pg.files_durable.fetch_add(1, Ordering::Relaxed);
            pg.bytes_durable.fetch_add(
                m.entry(*id).expect("an id from the manifest").size,
                Ordering::Relaxed,
            );
        }
    }
    for id in &b.committed {
        done.insert(*id);
        large.remove(id);
    }
    for id in &b.reset {
        large.remove(id);
    }
    for r in &b.ranges {
        if let Some(l) = large.get_mut(&r.file_id) {
            l.written = subtract(&l.written, r.offset, r.offset + r.len);
            l.durable.insert(r.offset, r.offset + r.len);
        }
    }
}

/// `set` minus one half-open range (`RangeSet` has only insertion).
fn subtract(set: &RangeSet, from: u64, to: u64) -> RangeSet {
    let mut out = RangeSet::new();
    for (s, e) in set.iter() {
        if e <= from || s >= to {
            out.insert(s, e);
        } else {
            if s < from {
                out.insert(s, from);
            }
            if e > to {
                out.insert(to, e);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::{FrameReader, FrameWriter};
    use crate::manifest::Entry;
    use crate::router::{ConnTx, Router};
    use crate::session::Timing;
    use tokio::io::{duplex, split};
    use tokio::sync::mpsc;

    /// A `JobLink` over an in-memory pipe whose control frames a drain task consumes, so
    /// the batch's Durable frames never block on a full outbox. The `Link` comes back too:
    /// dropping it aborts the connection's tasks and the outbox's sends would fail.
    fn test_link(job: [u8; 16]) -> (JobLink, crate::link::Link) {
        let timing = Timing {
            ping_every: Duration::from_secs(3600),
            dead_after: Duration::from_secs(3600),
            handshake: Duration::from_secs(1),
            min_frame_rate: crate::link::MIN_FRAME_RATE,
        };
        let (a, b) = duplex(1 << 20);
        let (ar, aw) = split(a);
        let (tx, mut rx) = mpsc::channel(crate::link::DELIVER_DEPTH);
        let (link, outbox) =
            crate::link::drive(FrameReader::new(ar), FrameWriter::new(aw), timing, tx);
        tokio::spawn(async move {
            let _keep = b; // the peer half stays open: the writer must not see EOF
            while rx.recv().await.is_some() {}
        });
        (
            JobLink::new(job, Arc::new(Router::default()), ConnTx::new(outbox), None),
            link,
        )
    }

    struct NoopSink;
    impl Sink for NoopSink {
        fn prepare(&self, _m: &Manifest) -> io::Result<()> {
            Ok(())
        }
        fn write_at(&self, _id: u32, _off: u64, _data: &[u8]) -> io::Result<()> {
            Ok(())
        }
        fn write_whole(&self, _id: u32, _data: &[u8]) -> io::Result<()> {
            Ok(())
        }
        fn sync(&self, _ids: &[u32]) -> io::Result<()> {
            Ok(())
        }
        fn read_at(&self, _id: u32, _off: u64, _buf: &mut [u8]) -> io::Result<usize> {
            Ok(0)
        }
        fn commit(&self, _id: u32) -> io::Result<()> {
            Ok(())
        }
        fn finish(&self) -> io::Result<()> {
            Ok(())
        }
    }

    /// The receiver compacts its journal once it passes COMPACT_AT: the batch is the
    /// caller (ruling 5, preflight row 12), and the rewritten file (Open ‖ Snapshot)
    /// replays to exactly the applied state — the same property Task 9 pins for the
    /// journal itself.
    #[tokio::test]
    async fn batch_compacts_the_journal_once_it_passes_compact_at() {
        let dir = std::env::temp_dir().join(format!("ava1-recv-compact-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let job = [0x44; 16];
        let m = Arc::new(Manifest {
            entries: vec![Entry {
                kind: gen::ENTRY_FILE,
                mode: 0o644,
                size: 2 * GROUP + 7,
                mtime: 0,
                path: "big".into(),
                root: None,
            }],
        });
        let open = JnlOpen {
            job_id: job,
            manifest_hash: m.hash(),
            kind: gen::JOB_DOWNLOAD,
            flags: 0,
            staged: 0,
            root: "/dest".into(),
        };
        let mut jnl = Journal::create(&dir, &open).unwrap();
        let mut st = State::default();
        st.apply(&Record::Open(open.clone()));
        // Grow the journal past COMPACT_AT with the durable-file batches a long upload
        // appends (a 400-run batch is ~5.6 KiB on disk, so ~190 of them cross 1 MiB).
        let mut i = 0u32;
        while jnl.len() <= journal::COMPACT_AT {
            let rec = Record::Batch(JnlBatch {
                files: (0..400)
                    .map(|k| gen::FileRun {
                        first: i * 400 + 1 + k,
                        count: 1,
                    })
                    .collect(),
                ranges: Vec::new(),
                roots: Vec::new(),
            });
            st.apply(&rec);
            jnl.append(&rec).unwrap();
            i += 1;
            assert!(i < 10_000, "the journal never crossed COMPACT_AT");
        }
        let before = jnl.len();
        assert!(before > journal::COMPACT_AT);
        // One large file with a freshly written range: the batch syncs, journals the range
        // and — now past COMPACT_AT — compacts.
        let mut large = HashMap::new();
        large.insert(0u32, new_large(&dir, &m, 0));
        large.get_mut(&0).unwrap().written.insert(0, GROUP);
        let (link, _keep_link) = test_link(job);
        let sink: Arc<dyn Sink> = Arc::new(NoopSink);
        let mut done = BTreeSet::new();
        let pg = Arc::default();
        let snap = snapshot_batch(
            job,
            link.control.clone(),
            &sink,
            &m,
            jnl,
            &open,
            st,
            &pg,
            &mut Vec::new(),
            &mut large,
        )
        .unwrap();
        let out = batch_task(snap).await.unwrap();
        fold_batch(&mut done, &mut large, &pg, &m, &out);
        let (jnl, st) = (out.jnl, out.st);
        assert!(
            jnl.len() <= journal::COMPACT_AT,
            "a compaction brings the journal back under the threshold: {}",
            jnl.len()
        );
        assert!(
            jnl.len() < before,
            "the journal shrank: {} -> {}",
            before,
            jnl.len()
        );
        drop(jnl);
        let (_, recs) = Journal::open(&dir).unwrap();
        let mut st2 = State::default();
        for r in &recs {
            st2.apply(r);
        }
        assert_eq!(st2, st, "a compaction loses no state");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
