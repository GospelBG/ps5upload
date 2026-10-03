//! Console → computer downloads over AVA1: a folder or file landing on disk
//! (`to_local`) and a folder or file streamed straight into a `.zip` (`to_zip`).
//! Blocking, like the upload adapters: call from `spawn_blocking` or a plain thread.
//!
//! Route selection (`route::use_ava1`), the terminal-versus-retryable split, the
//! `error_reason` words and the retry/backoff loop are the upload adapters' own
//! (`upload.rs`): a download differs only in which side holds the sink.

use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use ava1::gen;
use ava1::journal;
use ava1::manifest::{self, Manifest};
use ava1::recv::{download_job, LocalSink, RecvOptions, Sink};
use ava1::send::{Progress, SendError};
use ps5upload_core::download::DownloadKind;
use zip::write::SimpleFileOptions;

use crate::pool::{pool, Pool};
use crate::upload::{refusal, wait, SessionGate, UploadFailure, STALL_LIMIT};

/// The grant a download extends to the console (SPEC.md §12.4).
const CREDIT: u64 = 64 << 20;

/// The counters the engine's 200 ms ticker reads. All are stored absolutely (never
/// `fetch_add`), and only ever raised: a counter that steps backwards reads as a bug
/// to the person watching it. `total` is the ticker's dynamic total
/// (`TickerContext::dynamic_total_bytes`): the AVA1 path only learns the size when the
/// console's manifest arrives, so it is filled from the manifest, not from a Status.
#[derive(Clone, Default)]
pub struct Counters {
    pub bytes: Arc<AtomicU64>,
    pub files: Arc<AtomicU64>,
    pub files_finalized: Arc<AtomicU64>,
    pub bytes_finalized: Arc<AtomicU64>,
    pub total: Option<Arc<AtomicU64>>,
}

/// Copies AVA1's progress into the counters every 200 ms until dropped, and once more on
/// drop. Mapping (the upload bridge's rule): AVA1 only knows a file is done when it is
/// durable, so `bytes` and `bytes_finalized` ← `bytes_durable`, `files` and
/// `files_finalized` ← `files_durable`. `base_*` carry the work of earlier attempts
/// (zip only): the counter is monotonic "work done" and, by design, may end above the
/// archive's final byte count after a restart.
struct Ticker {
    handle: tokio::task::AbortHandle,
    state: Arc<TickState>,
}

struct TickState {
    p: Arc<Progress>,
    c: Counters,
    base_bytes: u64,
    base_files: u64,
}

impl TickState {
    fn store(&self) {
        let bytes = self.base_bytes + self.p.bytes_durable.load(Ordering::Relaxed);
        let files = self.base_files + self.p.files_durable.load(Ordering::Relaxed);
        self.c.bytes.fetch_max(bytes, Ordering::Relaxed);
        self.c.bytes_finalized.fetch_max(bytes, Ordering::Relaxed);
        self.c.files.fetch_max(files, Ordering::Relaxed);
        self.c.files_finalized.fetch_max(files, Ordering::Relaxed);
        if let Some(t) = &self.c.total {
            let total = self.p.bytes_total.load(Ordering::Relaxed);
            if total > 0 {
                t.store(total, Ordering::Release);
            }
        }
    }
}

impl Ticker {
    fn start(p: Arc<Progress>, c: &Counters, base_bytes: u64, base_files: u64) -> Ticker {
        let state = Arc::new(TickState {
            p,
            c: c.clone(),
            base_bytes,
            base_files,
        });
        let s = state.clone();
        let handle = tokio::spawn(async move {
            loop {
                s.store();
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .abort_handle();
        Ticker { handle, state }
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        self.handle.abort();
        self.state.store();
    }
}

fn basename(src: &str) -> Result<&str> {
    let name = src.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    if name.is_empty() || name == "." || name == ".." || name.contains('\\') {
        return Err(anyhow!("{src:?} has no usable file name"));
    }
    Ok(name)
}

const WINDOWS_RESERVED: [&str; 6] = ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"];

/// Why a path component cannot be written safely on every host, if it cannot.
fn host_unsafe_component(comp: &str) -> Option<&'static str> {
    // CONIN$ / CONOUT$ are device names too; a '$' alone is fine ("price$").
    let device_stem = comp.split('.').next().unwrap_or(comp).trim_end_matches(' ');
    if WINDOWS_RESERVED.contains(&device_stem.to_ascii_uppercase().as_str()) {
        return Some("a reserved device name");
    }
    if comp.contains(':') {
        return Some("a name with ':' (a drive or stream on Windows)");
    }
    if comp.ends_with('.') || comp.ends_with(' ') {
        return Some("a name ending in '.' or a space");
    }
    // The device name is the part before the first dot: `CON.txt` is still CON.
    let stem = comp.split('.').next().unwrap_or(comp).trim_end_matches(' ');
    let up = stem.to_ascii_uppercase();
    let numbered = |p: &str| {
        up.strip_prefix(p).is_some_and(|n| {
            let mut c = n.chars();
            matches!(
                (c.next(), c.next()),
                (Some('1'..='9' | '\u{b9}' | '\u{b2}' | '\u{b3}'), None)
            )
        })
    };
    if WINDOWS_RESERVED.contains(&up.as_str()) || numbered("COM") || numbered("LPT") {
        return Some("a reserved device name");
    }
    None
}

/// What the manifest of a download may look like (peer-supplied data: `Manifest::
/// from_pages` already ran `check_path` on every path; this adds the shape the request
/// promised and the one path rule `check_path` leaves to the host OS).
fn check_shape(m: &Manifest, single: bool) -> io::Result<()> {
    let bad = |why: String| io::Error::new(io::ErrorKind::InvalidData, why);
    for e in &m.entries {
        manifest::check_path(&e.path).map_err(|e| bad(e.to_string()))?;
        // A backslash is a separator on Windows: `a\..\b` would climb out of the root.
        if e.path.contains('\\') {
            return Err(bad(format!("{:?} contains a backslash", e.path)));
        }
        // Peer-supplied names are written on THIS computer, whatever it is. A ':' makes
        // `C:/x` a drive path and `C:evil` a drive-relative one on Windows (and an
        // alternate data stream on NTFS); the reserved device names and a trailing dot
        // or space name something other than the file asked for. Refused everywhere so
        // the console's data lands the same way on every host.
        for comp in e.path.split('/') {
            if let Some(why) = host_unsafe_component(comp) {
                return Err(bad(format!("{:?}: {why}", e.path)));
            }
        }
    }
    if single {
        let files = m
            .entries
            .iter()
            .filter(|e| e.kind == gen::ENTRY_FILE)
            .count();
        if m.entries.len() != 1 || files != 1 {
            return Err(bad(format!(
                "a single-file download received a manifest of {} entries",
                m.entries.len()
            )));
        }
    }
    Ok(())
}

/// `LocalSink` plus the checks above. The landing root is `dest_dir/<basename>` for both
/// kinds (the manifest's paths are root-relative), and every byte goes through a
/// `.ava-part` sibling in the destination's own directory, renamed once at the end.
struct CheckedSink {
    inner: LocalSink,
    root: PathBuf,
    single: bool,
}

impl Sink for CheckedSink {
    fn prepare(&self, m: &Manifest) -> io::Result<()> {
        check_shape(m, self.single)?;
        if self.single && self.root.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "{} is a folder; a file cannot replace it",
                    self.root.display()
                ),
            ));
        }
        self.inner.prepare(m)
    }
    fn write_at(&self, id: u32, off: u64, data: &[u8]) -> io::Result<()> {
        self.inner.write_at(id, off, data)
    }
    fn write_whole(&self, id: u32, data: &[u8]) -> io::Result<()> {
        self.inner.write_whole(id, data)
    }
    fn sync(&self, ids: &[u32]) -> io::Result<()> {
        self.inner.sync(ids)
    }
    fn read_at(&self, id: u32, off: u64, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read_at(id, off, buf)
    }
    fn commit(&self, id: u32) -> io::Result<()> {
        self.inner.commit(id)
    }
    fn finish(&self) -> io::Result<()> {
        self.inner.finish()
    }
    fn resume_key(&self) -> Option<(String, bool)> {
        self.inner.resume_key()
    }
}

/// The zip entry's name. A folder's entries sit under `<basename>/`; a single file's
/// entry is exactly `<basename>` (FTX2's `enumerate_download_set` puts the basename in
/// `rel_path` for a file and `download_to_zip_ex` writes it verbatim, so prefixing here
/// would produce `foo.pkg/foo.pkg`).
pub fn zip_entry_name(single: bool, basename: &str, rel: &str) -> String {
    if single {
        basename.to_owned()
    } else {
        format!("{basename}/{rel}")
    }
}

struct ZipState {
    m: Option<Arc<Manifest>>,
    zip: Option<zip::ZipWriter<BufWriter<std::fs::File>>>,
    /// The entry being written, and how much of it is in the archive.
    current: Option<u32>,
    written: u64,
    /// Highest id started: ordered delivery never goes back.
    last: Option<u32>,
    started: usize,
    finished: bool,
    /// Bytes of single-group files between their write and their commit. The receiver
    /// verifies such a file by reading it back (`read_at`) before it commits; a deflate
    /// stream cannot be read back, so the sink keeps the (at most one group) bytes
    /// itself. Larger files are verified from their outboards and need nothing.
    kept: std::collections::HashMap<u32, Vec<u8>>,
}

/// An ordered download appended into a `.zip`. Deflate entries cannot be seeked into, so
/// the receiver must deliver each file's bytes contiguously and in file order (the
/// ordered flag); anything else is a protocol violation, never garbage in the archive.
/// Writes `<dest>.ava-part` and renames over `dest` in `finish`, so the final path never
/// holds a half-written archive; an abandoned sink removes its part file.
/// Empty directories are not entries (FTX2's zip manifest holds files only). Empty
/// files are appended at `finish`: the receiver creates them up front, out of order,
/// and a zip can only have one entry open at a time.
pub struct ZipSink {
    dest: PathBuf,
    part: PathBuf,
    single: bool,
    base: String,
    st: Mutex<ZipState>,
}

fn zip_err(e: zip::result::ZipError) -> io::Error {
    io::Error::other(e)
}

/// A file arrived a second time (the receiver re-requested it after a verify mismatch).
/// A deflate stream cannot rewrite what it already holds, so the archive attempt is
/// abandoned and restarted — like a dropped connection — instead of being reported as
/// a bad manifest.
#[derive(Debug)]
struct ZipRestart(String);

impl std::fmt::Display for ZipRestart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ZipRestart {}

fn zip_restart(why: impl Into<String>) -> io::Error {
    io::Error::other(ZipRestart(why.into()))
}

/// True for the error a sink returns when a file it already wrote is sent again.
#[doc(hidden)]
pub fn is_zip_restart(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|inner| inner.is::<ZipRestart>())
}

/// A file that keeps failing verification must end the job, not restart forever.
const MAX_RETRY_RESTARTS: u32 = 3;

fn invalid(why: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why.into())
}

impl ZipSink {
    /// A folder download: entries are `<prefix>/<root-relative path>`.
    pub fn new(path: PathBuf, prefix: &str) -> Self {
        Self::build(path, prefix, false)
    }

    /// A single-file download: the one entry is exactly `name`.
    pub fn single(path: PathBuf, name: &str) -> Self {
        Self::build(path, name, true)
    }

    fn build(dest: PathBuf, base: &str, single: bool) -> Self {
        let mut part = dest.clone().into_os_string();
        part.push(".ava-part");
        Self {
            dest,
            part: PathBuf::from(part),
            single,
            base: base.to_owned(),
            st: Mutex::new(ZipState {
                m: None,
                zip: None,
                current: None,
                written: 0,
                last: None,
                started: 0,
                finished: false,
                kept: Default::default(),
            }),
        }
    }

    fn opts() -> SimpleFileOptions {
        // zip64 so a single >4 GiB game file is encoded correctly (the FTX2 zip's rule).
        SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .large_file(true)
    }

    fn append(&self, id: u32, off: u64, data: &[u8]) -> io::Result<()> {
        let mut st = self.st.lock().unwrap();
        let m =
            st.m.clone()
                .ok_or_else(|| invalid("data before the manifest"))?;
        let e = m
            .entry(id)
            .filter(|e| e.kind == gen::ENTRY_FILE)
            .ok_or_else(|| invalid(format!("data for {id}, which is not a file")))?;
        if e.size == 0 {
            // Written at `finish`; the receiver's up-front empty write lands here.
            return if data.is_empty() {
                Ok(())
            } else {
                Err(invalid(format!("data for empty file {id}")))
            };
        }
        if st.current != Some(id) {
            if st.last.is_some_and(|l| id <= l) {
                // From the start of a file already written: a retry, not a violation.
                return Err(if off == 0 {
                    zip_restart(format!("file {id} was sent again"))
                } else {
                    invalid(format!("file {id} arrived out of order"))
                });
            }
            if let Some(cur) = st.current {
                let want = m.entry(cur).map(|c| c.size).unwrap_or(0);
                if st.written != want {
                    return Err(invalid(format!(
                        "file {cur} ended at {} of {want} bytes",
                        st.written
                    )));
                }
            }
            if off != 0 {
                return Err(invalid(format!(
                    "file {id} starts at offset {off}: a gap in the archive stream"
                )));
            }
            let name = zip_entry_name(self.single, &self.base, &e.path);
            let zip = st.zip.as_mut().ok_or_else(|| invalid("archive not open"))?;
            zip.start_file(name, Self::opts()).map_err(zip_err)?;
            st.current = Some(id);
            st.last = Some(id);
            st.written = 0;
            st.started += 1;
        } else if off == 0 && st.written > 0 {
            // The current file again from its first byte (a retry after a mismatch).
            return Err(zip_restart(format!("file {id} was sent again")));
        } else if off != st.written {
            // A gap or an overlap (a duplicate range): never append it.
            return Err(invalid(format!(
                "file {id} wrote at offset {off} but the archive is at {}",
                st.written
            )));
        }
        if st.written + data.len() as u64 > e.size {
            return Err(invalid(format!("file {id} wrote past its size")));
        }
        let zip = st.zip.as_mut().ok_or_else(|| invalid("archive not open"))?;
        zip.write_all(data)?;
        st.written += data.len() as u64;
        if ava1::verify::groups(e.size) < 2 {
            st.kept.entry(id).or_default().extend_from_slice(data);
        }
        Ok(())
    }
}

impl Drop for ZipSink {
    fn drop(&mut self) {
        let finished = self.st.lock().map(|s| s.finished).unwrap_or(false);
        if !finished {
            let _ = std::fs::remove_file(&self.part);
        }
    }
}

impl Sink for ZipSink {
    /// Truncates: `File::create` discards whatever an earlier attempt left in the part
    /// file, so two archives' bytes are never mixed.
    fn prepare(&self, m: &Manifest) -> io::Result<()> {
        check_shape(m, self.single)?;
        let f = std::fs::File::create(&self.part)?;
        let mut st = self.st.lock().unwrap();
        st.m = Some(Arc::new(m.clone()));
        st.zip = Some(zip::ZipWriter::new(BufWriter::new(f)));
        st.current = None;
        st.written = 0;
        st.last = None;
        st.started = 0;
        st.kept.clear();
        Ok(())
    }
    fn write_at(&self, id: u32, off: u64, data: &[u8]) -> io::Result<()> {
        self.append(id, off, data)
    }
    fn write_whole(&self, id: u32, data: &[u8]) -> io::Result<()> {
        self.append(id, 0, data)
    }
    fn sync(&self, _ids: &[u32]) -> io::Result<()> {
        Ok(())
    }
    fn read_at(&self, id: u32, off: u64, buf: &mut [u8]) -> io::Result<usize> {
        let st = self.st.lock().unwrap();
        let kept = st
            .kept
            .get(&id)
            .ok_or_else(|| io::Error::from(io::ErrorKind::Unsupported))?;
        let end = off as usize + buf.len();
        if end > kept.len() {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        buf.copy_from_slice(&kept[off as usize..end]);
        Ok(buf.len())
    }
    fn commit(&self, id: u32) -> io::Result<()> {
        self.st.lock().unwrap().kept.remove(&id);
        Ok(())
    }
    fn finish(&self) -> io::Result<()> {
        let mut st = self.st.lock().unwrap();
        let m =
            st.m.clone()
                .ok_or_else(|| invalid("finished before the manifest"))?;
        let nonempty = m
            .entries
            .iter()
            .filter(|e| e.kind == gen::ENTRY_FILE && e.size > 0)
            .count();
        if let Some(cur) = st.current {
            let want = m.entry(cur).map(|c| c.size).unwrap_or(0);
            if st.written != want {
                return Err(invalid(format!(
                    "file {cur} ended at {} of {want}",
                    st.written
                )));
            }
        }
        if st.started != nonempty {
            return Err(invalid(format!(
                "the archive holds {} of {nonempty} files",
                st.started
            )));
        }
        let mut zip = st.zip.take().ok_or_else(|| invalid("archive not open"))?;
        for e in m
            .entries
            .iter()
            .filter(|e| e.kind == gen::ENTRY_FILE && e.size == 0)
        {
            zip.start_file(
                zip_entry_name(self.single, &self.base, &e.path),
                Self::opts(),
            )
            .map_err(zip_err)?;
        }
        let mut out = zip.finish().map_err(zip_err)?;
        out.flush()?;
        let f = out.into_inner().map_err(|e| e.into_error())?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&self.part, &self.dest)?; // same directory by construction
        st.finished = true;
        Ok(())
    }
}

/// Maps the receiver's terminal errors to what the engine's `job_failed_from_err` already
/// understands (`UploadFailure` with a stable `error_reason`). Nothing here is retried:
/// by the time a local write or the final rename fails, the bytes are already down, and
/// asking again would pull gigabytes for the same failure.
fn terminal(e: SendError) -> anyhow::Error {
    match e {
        SendError::Cancelled => anyhow!("transfer_cancelled"),
        SendError::Refused { status, message } => refusal(status, message).into(),
        SendError::Source(e) if e.kind() == io::ErrorKind::InvalidData => UploadFailure {
            reason: "ava1_bad_manifest".into(),
            detail: format!("the console sent a download this computer will not write: {e}"),
        }
        .into(),
        SendError::Source(e) => UploadFailure {
            reason: "ava1_local_io".into(),
            detail: format!("writing the download on this computer failed: {e}"),
        }
        .into(),
        other => anyhow!(other),
    }
}

/// Job id of attempt `n`: attempt 0 is the job's own id (so the journal directory, the
/// `JobOpen` and the job record agree); later attempts of a zip hash it with the attempt.
fn attempt_id(job_id: [u8; 16], attempt: u32, fresh_per_attempt: bool) -> [u8; 16] {
    if attempt == 0 || !fresh_per_attempt {
        return job_id;
    }
    let mut h = blake3::Hasher::new();
    h.update(&job_id);
    h.update(&attempt.to_le_bytes());
    let mut id = [0u8; 16];
    id.copy_from_slice(&h.finalize().as_bytes()[..16]);
    id
}

/// One download, retried across connection loss. `make_sink` builds the sink for an
/// attempt.
///
/// `fresh_per_attempt` (zip): every attempt is a NEW job with a new journal. FTX2
/// resumes a zip inside one run (it keeps the `ZipWriter` and re-requests from the
/// offset), which AVA1's ordered receiver cannot: a deflate stream cannot be seeked
/// into, and the journal's ranges cannot reconstruct compressed bytes. So a dropped zip
/// download restarts the archive from byte zero — a 40 GiB zip that drops at 90% pays
/// for it again, which FTX2 does not. The previous attempt's archive is discarded
/// (`ZipSink::prepare` truncates, and an abandoned sink deletes its part file) so two
/// archives are never mixed, and the restart is logged with the attempt number and the
/// bytes spent.
#[allow(clippy::too_many_arguments)]
fn run(
    pool: &Pool,
    console: &str,
    src: &str,
    flags: u32,
    job_id: [u8; 16],
    fresh_per_attempt: bool,
    make_sink: &dyn Fn() -> Arc<dyn Sink>,
    counters: &Counters,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<u64> {
    let cancel = cancel.unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
    let jobs_dir = pool.ava_dir().join("jobs");
    crate::block_on(async {
        SessionGate::identity(pool)?;
        let mut sink = make_sink();
        let mut backoff = Duration::from_millis(250);
        let mut gate = SessionGate::default();
        let (mut base_bytes, mut base_files) = (0u64, 0u64);
        let mut attempt = 0u32;
        let mut retry_restarts = 0u32;
        let (mut last_at, mut last_work) = (Instant::now(), 0u64);
        let mut progress = Arc::new(Progress::default());
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(anyhow!("transfer_cancelled"));
            }
            let work = base_bytes + progress.bytes_durable.load(Ordering::Relaxed);
            if work > last_work {
                (last_at, last_work) = (Instant::now(), work);
            } else if last_at.elapsed() > STALL_LIMIT {
                return Err(anyhow!(
                    "no durable progress for {STALL_LIMIT:?}; giving up"
                ));
            }
            let session = match pool.session(console).await {
                Ok(s) => s,
                Err(e) => {
                    if let Some(failure) = gate.failed(&e) {
                        return Err(failure.into());
                    }
                    wait(&mut backoff, &e.to_string()).await;
                    continue;
                }
            };
            gate.connected();
            let id = attempt_id(job_id, attempt, fresh_per_attempt);
            let mut link = session.job(id);
            let _ticker = Ticker::start(progress.clone(), counters, base_bytes, base_files);
            let o = RecvOptions {
                credit: CREDIT,
                flags,
                jobs_dir: jobs_dir.clone(),
                // `ordered` must agree with the flags (download_job rewrites `flags`).
                ordered: flags & gen::JF_ORDERED != 0,
                progress: progress.clone(),
                cancel: cancel.clone(),
            };
            let (why, dropped) = match download_job(&mut link, src, flags, sink.clone(), o).await {
                Ok(r) => {
                    let _ = std::fs::remove_dir_all(journal::job_dir(&jobs_dir, &id));
                    return Ok(r.bytes);
                }
                Err(SendError::Disconnected(why)) => (why, true),
                Err(SendError::Source(e))
                    if fresh_per_attempt
                        && is_zip_restart(&e)
                        && retry_restarts < MAX_RETRY_RESTARTS =>
                {
                    retry_restarts += 1;
                    (e.to_string(), false)
                }
                Err(e) => return Err(terminal(e)),
            };
            let durable = progress.bytes_durable.load(Ordering::Relaxed);
            if dropped {
                pool.forget(console).await;
            }
            if fresh_per_attempt {
                let _ = std::fs::remove_dir_all(journal::job_dir(&jobs_dir, &id));
                base_bytes += durable;
                base_files += progress.files_durable.load(Ordering::Relaxed);
                attempt += 1;
                progress = Arc::new(Progress::default());
                drop(std::mem::replace(&mut sink, make_sink()));
                let _ = writeln!(
                    std::io::stderr(),
                    "ava1: zip download restarts from the beginning (attempt {attempt}, \
                     {base_bytes} bytes already spent): {why}"
                );
                wait(&mut backoff, "restarting the archive").await;
            } else {
                wait(&mut backoff, &format!("{why} ({durable} bytes durable)")).await;
            }
        }
    })
}

#[allow(clippy::too_many_arguments)]
pub fn to_local_in(
    pool: &Pool,
    console: &str,
    src: &str,
    kind: DownloadKind,
    dest_dir: &Path,
    unsafe_read: bool,
    job_id: [u8; 16],
    counters: &Counters,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<u64> {
    // The manifest's paths are root-relative, so the landing root is
    // `dest_dir/<basename>` for both kinds; for a file that root is the file's own path.
    // Not `dest_dir` (loses the basename) and not a stripped component (double-nests).
    let target = dest_dir.join(basename(src)?);
    // The request kind decides single-file, never the manifest's shape: a folder holding
    // exactly one file must stay a folder.
    let single = kind == DownloadKind::File;
    let mut flags = if single { gen::JF_SINGLE_FILE } else { 0 };
    if unsafe_read {
        flags |= gen::JF_UNSAFE_READ;
    }
    // The staging path every kind of landing uses (`<root>.ava-part`: the staging
    // folder of a new folder, the part file of a single file).
    let part = {
        let mut p = target.clone().into_os_string();
        p.push(".ava-part");
        PathBuf::from(p)
    };
    let job_dir = journal::job_dir(&pool.ava_dir().join("jobs"), &job_id);
    // A fresh job (no journal records) cannot own what is already in the staging path:
    // it is what an earlier, failed download left behind, and renaming it into place
    // would put that manifest's files into this one's folder. A job with a journal is
    // a resume: the staging path is its work.
    if journal_is_empty(&job_dir) {
        remove_path(&part);
    }
    let sink: Arc<dyn Sink> = Arc::new(CheckedSink {
        inner: LocalSink::new(target.clone(), single),
        root: target,
        single,
    });
    let r = run(
        pool,
        console,
        src,
        flags,
        job_id,
        false,
        &move || sink.clone(),
        counters,
        cancel,
    );
    if r.is_err() {
        // `run` only returns a terminal error (a cancel, a refusal, a local write
        // failure, the stall limit): nothing will resume this job, so neither the
        // journal nor the staged bytes may outlive it.
        let _ = std::fs::remove_dir_all(&job_dir);
        remove_path(&part);
    }
    r
}

fn journal_is_empty(dir: &Path) -> bool {
    match std::fs::read_dir(dir) {
        Ok(mut it) => it.next().is_none(),
        Err(_) => true,
    }
}

fn remove_path(p: &Path) {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => {
            let _ = std::fs::remove_dir_all(p);
        }
        Ok(_) => {
            let _ = std::fs::remove_file(p);
        }
        Err(_) => {}
    }
}

#[allow(clippy::too_many_arguments)]
pub fn to_local(
    console: &str,
    src: &str,
    kind: DownloadKind,
    dest_dir: &Path,
    unsafe_read: bool,
    job_id: [u8; 16],
    counters: &Counters,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<u64> {
    to_local_in(
        pool(),
        console,
        src,
        kind,
        dest_dir,
        unsafe_read,
        job_id,
        counters,
        cancel,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn to_zip_in(
    pool: &Pool,
    console: &str,
    src: &str,
    kind: DownloadKind,
    dest_zip: &Path,
    unsafe_read: bool,
    job_id: [u8; 16],
    counters: &Counters,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<u64> {
    let name = basename(src)?.to_owned();
    let single = kind == DownloadKind::File;
    // A zip needs in-order bytes.
    let mut flags = gen::JF_ORDERED;
    if single {
        flags |= gen::JF_SINGLE_FILE;
    }
    if unsafe_read {
        flags |= gen::JF_UNSAFE_READ;
    }
    let dest = dest_zip.to_path_buf();
    run(
        pool,
        console,
        src,
        flags,
        job_id,
        true,
        &move || -> Arc<dyn Sink> {
            Arc::new(if single {
                ZipSink::single(dest.clone(), &name)
            } else {
                ZipSink::new(dest.clone(), &name)
            })
        },
        counters,
        cancel,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn to_zip(
    console: &str,
    src: &str,
    kind: DownloadKind,
    dest_zip: &Path,
    unsafe_read: bool,
    job_id: [u8; 16],
    counters: &Counters,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<u64> {
    to_zip_in(
        pool(),
        console,
        src,
        kind,
        dest_zip,
        unsafe_read,
        job_id,
        counters,
        cancel,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zip_entry_names_follow_one_rule() {
        assert_eq!(zip_entry_name(false, "Game", "a/b.bin"), "Game/a/b.bin");
        assert_eq!(zip_entry_name(true, "foo.pkg", "ignored"), "foo.pkg");
    }

    #[test]
    fn attempt_ids_differ_only_for_a_fresh_per_attempt_job() {
        let id = [7u8; 16];
        assert_eq!(attempt_id(id, 0, true), id);
        assert_eq!(attempt_id(id, 3, false), id);
        assert_ne!(attempt_id(id, 1, true), id);
        assert_ne!(attempt_id(id, 1, true), attempt_id(id, 2, true));
    }

    #[test]
    fn basename_refuses_names_that_climb() {
        assert_eq!(basename("/data/foo/").unwrap(), "foo");
        assert!(basename("/").is_err());
        assert!(basename("/a/..").is_err());
    }

    fn entry(kind: u8, path: &str) -> manifest::Entry {
        manifest::Entry {
            kind,
            mode: 0o644,
            size: 0,
            mtime: 0,
            path: path.into(),
            root: None,
        }
    }

    fn files(paths: &[&str]) -> Manifest {
        Manifest {
            entries: paths.iter().map(|p| entry(gen::ENTRY_FILE, p)).collect(),
        }
    }

    #[test]
    fn check_shape_refuses_paths_a_host_would_misplace() {
        for bad in [
            "C:/x",
            "C:evil",
            "a/C:/x",
            "a\\b",
            "CON",
            "con",
            "a/NUL.txt",
            "Aux.tar.gz",
            "COM1",
            "lpt9.log",
            "CONIN$",
            "conout$",
            "CONIN$.txt",
            "COM\u{b9}",
            "com\u{b2}.txt",
            "LPT\u{b3}",
            "x.",
            "a/x ",
            "dir./f",
            "s:stream",
        ] {
            let e = check_shape(&files(&[bad]), false);
            assert!(e.is_err(), "{bad:?} must be refused");
            assert_eq!(e.unwrap_err().kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn check_shape_accepts_ordinary_and_lookalike_names() {
        for good in [
            "a/b.txt",
            "COM",
            "COM0",
            "COM10",
            "CONSOLE",
            "console.txt",
            "NULL",
            "LPT",
            ".hidden",
            "a b/c d",
            "x.y",
            "price$",
            "CONIN",
            "CON$X",
        ] {
            check_shape(&files(&[good]), false)
                .unwrap_or_else(|e| panic!("{good:?} must be accepted: {e}"));
        }
    }

    #[test]
    fn a_single_file_manifest_must_be_exactly_one_file() {
        check_shape(&files(&["one"]), true).unwrap();
        assert!(check_shape(&files(&["one", "two"]), true).is_err());
        let dir = Manifest {
            entries: vec![entry(gen::ENTRY_DIR, "d")],
        };
        assert!(
            check_shape(&dir, true).is_err(),
            "a directory is not a file"
        );
        let dir_and_file = Manifest {
            entries: vec![entry(gen::ENTRY_DIR, "d"), entry(gen::ENTRY_FILE, "d/f")],
        };
        assert!(check_shape(&dir_and_file, true).is_err());
        // A folder download may hold one file.
        check_shape(&dir_and_file, false).unwrap();
    }

    /// A sink that wraps `ZipSink` and, on its first write, reports that a file was
    /// sent again (what the receiver's FileRetry causes), once per test.
    struct RetrySink {
        inner: ZipSink,
        fail_first_write: bool,
        failed: AtomicBool,
    }

    impl Sink for RetrySink {
        fn prepare(&self, m: &Manifest) -> io::Result<()> {
            self.inner.prepare(m)
        }
        fn write_at(&self, id: u32, off: u64, data: &[u8]) -> io::Result<()> {
            if self.fail_first_write && !self.failed.swap(true, Ordering::Relaxed) {
                return Err(zip_restart("injected: file sent again"));
            }
            self.inner.write_at(id, off, data)
        }
        fn write_whole(&self, id: u32, data: &[u8]) -> io::Result<()> {
            if self.fail_first_write && !self.failed.swap(true, Ordering::Relaxed) {
                return Err(zip_restart("injected: file sent again"));
            }
            self.inner.write_whole(id, data)
        }
        fn sync(&self, ids: &[u32]) -> io::Result<()> {
            self.inner.sync(ids)
        }
        fn read_at(&self, id: u32, off: u64, buf: &mut [u8]) -> io::Result<usize> {
            self.inner.read_at(id, off, buf)
        }
        fn commit(&self, id: u32) -> io::Result<()> {
            self.inner.commit(id)
        }
        fn finish(&self) -> io::Result<()> {
            self.inner.finish()
        }
    }

    async fn folder_host(dir: &Path, engine_key: [u8; 32]) -> String {
        use ava1::host::FolderHost;
        use ava1::peers::PeerStore;
        use ava1::server::{self, ServerCtx};
        let mut peers = PeerStore::in_memory();
        peers.add(engine_key, "engine").unwrap();
        let rpc: server::RpcHandler = Box::new(|_, _| ava1::session::RpcReply {
            status: gen::ERR_UNKNOWN_METHOD,
            body: Vec::new(),
        });
        let ctx = ServerCtx::new(
            ava1::keys::Identity::generate().unwrap(),
            "host",
            peers,
            rpc,
        )
        .with_jobs(Arc::new(FolderHost {
            root: dir.join("share"),
            jobs_dir: dir.join("hjobs"),
        }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        tokio::spawn(server::serve(l, Arc::new(ctx)));
        addr
    }

    // A re-sent file restarts the archive attempt (like a drop) instead of failing the
    // download as a bad manifest.
    #[test]
    fn a_zip_download_restarts_when_a_file_is_sent_again() {
        let d = std::env::temp_dir().join(format!("p5a-zip-retry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("share/G")).unwrap();
        for i in 0..3u8 {
            std::fs::write(d.join(format!("share/G/f{i}")), vec![i + 1; 5000]).unwrap();
        }
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let ava = d.join("ava");
        let key = ava1::keys::Identity::load_or_create(&ava.join("identity"))
            .unwrap()
            .public();
        let addr = rt.block_on(folder_host(&d, key));
        let pool = Pool::new(ava).with_addr(addr);
        let dest = d.join("g.zip");
        let made = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let (made2, dest2) = (made.clone(), dest.clone());
        let make = move || -> Arc<dyn Sink> {
            let n = made2.fetch_add(1, Ordering::Relaxed);
            Arc::new(RetrySink {
                inner: ZipSink::new(dest2.clone(), "G"),
                fail_first_write: n == 0,
                failed: AtomicBool::new(false),
            })
        };
        let flags = gen::JF_ORDERED;
        // The test thread owns no runtime context: `run` blocks on its own.
        let bytes = run(
            &pool,
            "c",
            "G",
            flags,
            [9; 16],
            true,
            &make,
            &Counters::default(),
            None,
        )
        .expect("a re-sent file must restart the archive, not fail it");
        assert_eq!(bytes, 15000);
        // Two sinks were built: the original and the restart.
        assert_eq!(made.load(Ordering::Relaxed), 2);
        let mut z = zip::ZipArchive::new(std::fs::File::open(&dest).unwrap()).unwrap();
        assert_eq!(z.len(), 3);
        for i in 0..3u8 {
            let mut e = z.by_name(&format!("G/f{i}")).unwrap();
            let mut got = Vec::new();
            io::Read::read_to_end(&mut e, &mut got).unwrap();
            assert_eq!(got, vec![i + 1; 5000]);
        }
        assert!(!d.join("g.zip.ava-part").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_rewrite_from_offset_zero_is_a_restart_and_a_gap_is_not() {
        let d = std::env::temp_dir().join(format!("p5a-zip-sink-rw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let mut m = files(&["a", "b"]);
        m.entries[0].size = 100;
        m.entries[1].size = 50;
        let s = ZipSink::new(d.join("o.zip"), "P");
        s.prepare(&m).unwrap();
        s.write_at(0, 0, &[1; 100]).unwrap();
        s.write_at(1, 0, &[2; 10]).unwrap();
        // An earlier, finished file sent again, and the current one again.
        assert!(is_zip_restart(&s.write_at(0, 0, &[1; 100]).unwrap_err()));
        assert!(is_zip_restart(&s.write_at(1, 0, &[2; 10]).unwrap_err()));
        // A gap is a protocol violation, not a retry.
        let gap = s.write_at(1, 30, &[2; 10]).unwrap_err();
        assert!(!is_zip_restart(&gap));
        assert_eq!(gap.kind(), io::ErrorKind::InvalidData);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn terminal_errors_keep_their_reasons() {
        let reason = |e: SendError| {
            let e = terminal(e);
            match e.downcast_ref::<UploadFailure>() {
                Some(f) => f.reason.clone(),
                None => e.to_string(),
            }
        };
        assert_eq!(reason(SendError::Cancelled), "transfer_cancelled");
        assert_eq!(
            reason(SendError::Source(io::Error::from(
                io::ErrorKind::PermissionDenied
            ))),
            "ava1_local_io"
        );
        assert_eq!(
            reason(SendError::Source(io::Error::other("disk full"))),
            "ava1_local_io"
        );
        assert_eq!(
            reason(SendError::Source(invalid("bad name"))),
            "ava1_bad_manifest"
        );
        assert_eq!(
            reason(SendError::Refused {
                status: gen::ERR_PATH,
                message: "no".into()
            }),
            "ava1_not_allowed"
        );
    }

    #[test]
    fn a_stale_staging_path_is_cleared_only_for_a_fresh_journal() {
        let d = std::env::temp_dir().join(format!("p5a-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("j")).unwrap();
        assert!(journal_is_empty(&d.join("missing")));
        assert!(journal_is_empty(&d.join("j")));
        std::fs::write(d.join("j/rec"), b"x").unwrap();
        assert!(!journal_is_empty(&d.join("j")));
        std::fs::create_dir_all(d.join("p.ava-part/sub")).unwrap();
        remove_path(&d.join("p.ava-part"));
        assert!(!d.join("p.ava-part").exists());
        std::fs::write(d.join("f.ava-part"), b"x").unwrap();
        remove_path(&d.join("f.ava-part"));
        assert!(!d.join("f.ava-part").exists());
        let _ = std::fs::remove_dir_all(&d);
    }
}
