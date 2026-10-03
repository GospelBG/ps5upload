//! The upload adapters — the shapes Task 23 swaps in for the FTX2 calls. Blocking:
//! call them from `spawn_blocking` or a non-async thread (C15), exactly like the
//! FTX2 functions they replace; calling them from inside an async task panics.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use ava1::gen;
use ava1::manifest::{self, Entry, Manifest};
use ava1::send::{send_job, Progress, SendError, SendOptions};
use ava1::source::{LocalSource, Source};
use ava1::Ava1Error;
use ps5upload_core::transfer::{FileListEntry, TransferConfig, TransferResult};

use crate::pool::{pool, Pool};
use crate::progress::{bottleneck_name, Bridge};
use crate::source::{FsSource, ListSource};
use crate::zip_source::ZipSource;

/// `ZipEntryReader::read_at` inflates from the beginning for every read. Above
/// this size its repeated work grows quadratically; FTX2 streams those entries.
#[derive(Debug, thiserror::Error)]
#[error("zip entry {0} is larger than 256 MiB: AVA1 sends this archive with FTX2")]
pub struct ZipTooLarge(pub String);

/// The archive cannot be an AVA1 source (a path the manifest refuses, an unsupported
/// method, encryption, a damaged directory): FTX2 reads zips its own way.
#[derive(Debug, thiserror::Error)]
#[error("zip is not usable as an AVA1 source: {0}")]
pub struct ZipUnsupported(pub String);

pub const ZIP_MAX_ENTRY: u64 = 256 << 20;

pub fn zip_too_large(m: &Manifest) -> Option<&str> {
    m.entries
        .iter()
        .find(|e| e.kind == gen::ENTRY_FILE && e.size > ZIP_MAX_ENTRY)
        .map(|e| e.path.as_str())
}

pub fn upload_zip_in(
    pool: &Pool,
    cfg: &TransferConfig,
    job_id: [u8; 16],
    dest_root: &str,
    zip_path: &Path,
) -> Result<TransferResult> {
    let (manifest, source) =
        ZipSource::open(zip_path, &cfg.excludes).map_err(|e| ZipUnsupported(e.to_string()))?;
    if let Some(path) = zip_too_large(&manifest) {
        return Err(ZipTooLarge(path.to_owned()).into());
    }
    upload_with_in(
        pool,
        &cfg.addr,
        job_id,
        manifest,
        Arc::new(source),
        SendOptions::upload(dest_root),
        cfg,
    )
}

pub fn upload_zip(
    cfg: &TransferConfig,
    job_id: [u8; 16],
    dest_root: &str,
    zip_path: &Path,
) -> Result<TransferResult> {
    upload_zip_in(pool(), cfg, job_id, dest_root, zip_path)
}

/// Why the console refused a transfer whose data it had already received (a
/// post-commit failure). The upload must never be retried: the destination is taken
/// and resuming would re-send every byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostCommitKind {
    /// The destination already exists (`ERR_EXISTS`).
    Exists,
    /// The destination is on a different storage device than the staged files
    /// (`ERR_CROSS_DEVICE`).
    CrossDevice,
}

impl PostCommitKind {
    /// The machine-readable name Task 23's handler uses to build `error_reason`
    /// (`{"error": …, "detail": …}`). The `Display`s stay human sentences (A2).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exists => "ava1_commit_exists",
            Self::CrossDevice => "ava1_commit_cross_device",
        }
    }
}

impl std::fmt::Display for PostCommitKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Exists => "the destination already exists",
            Self::CrossDevice => "the destination is on another storage device",
        })
    }
}

/// The console accepted and applied the transfer but refused to commit it (C13/A2).
/// The reason travels as the typed [`PostCommitKind`], never as a machine-shaped
/// Display — any wrapper or log formatter that rewrites the message must not destroy
/// the engine's `error_reason`. Not retryable: the destination is taken.
#[derive(Debug, thiserror::Error)]
#[error("the console refused to commit the transfer: {kind} ({detail})")]
pub struct PostCommitError {
    pub kind: PostCommitKind,
    /// The console's own message, when it sent one.
    pub detail: String,
}

/// A refusal or terminal connection failure with a stable reason for the UI.
#[derive(Debug, thiserror::Error)]
#[error("{detail}")]
pub struct UploadFailure {
    pub reason: String,
    pub detail: String,
}

pub(crate) fn refusal_reason(status: u16) -> String {
    match status {
        gen::ERR_NO_SPACE => "ava1_no_space".into(),
        gen::ERR_PATH => "ava1_not_allowed".into(),
        gen::ERR_EXISTS => "ava1_exists".into(),
        gen::ERR_CROSS_DEVICE => "ava1_cross_device".into(),
        _ => format!("ava1_refused_{status}"),
    }
}

pub(crate) fn terminal_connection_reason(error: &Ava1Error) -> Option<&'static str> {
    match error {
        Ava1Error::Io(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            Some("ava1_unreachable")
        }
        Ava1Error::NotPaired => Some("ava1_not_paired"),
        // The console's own refusal of an unpaired peer (pairing window closed).
        Ava1Error::Refused { code, .. }
            if *code == gen::ERR_NOT_PAIRED || *code == gen::ERR_PAIRING_CLOSED =>
        {
            Some("ava1_not_paired")
        }
        Ava1Error::WrongPeer => Some("ava1_wrong_console"),
        _ => None,
    }
}

/// How many consecutive terminal connection failures end a job.
pub(crate) const TERMINAL_ATTEMPTS: u32 = 3;

/// The session-level retry policy every AVA1 job shares (uploads and the relay): a
/// console that refuses us, is not paired, has another key or has no listener is
/// given three tries and then reported with a stable reason; anything else is
/// transient and resets the count.
#[derive(Default)]
pub(crate) struct SessionGate {
    terminal: u32,
}

impl SessionGate {
    /// No identity can never recover by retrying.
    pub(crate) fn identity(pool: &Pool) -> Result<(), UploadFailure> {
        if pool.has_identity() {
            return Ok(());
        }
        Err(UploadFailure {
            reason: "ava1_no_identity".into(),
            detail: "no AVA1 identity is available".into(),
        })
    }

    /// A session was opened.
    pub(crate) fn connected(&mut self) {
        self.terminal = 0;
    }

    /// A session attempt failed: `Some` when the job must end now.
    pub(crate) fn failed(&mut self, e: &Ava1Error) -> Option<UploadFailure> {
        let Some(reason) = terminal_connection_reason(e) else {
            self.terminal = 0;
            return None;
        };
        self.terminal += 1;
        (self.terminal >= TERMINAL_ATTEMPTS).then(|| UploadFailure {
            reason: reason.into(),
            detail: e.to_string(),
        })
    }
}

pub(crate) fn refusal(status: u16, message: String) -> UploadFailure {
    UploadFailure {
        reason: refusal_reason(status),
        detail: format!("console refused the transfer ({status}): {message}"),
    }
}

impl PostCommitError {
    fn new(kind: PostCommitKind, message: Option<String>) -> Self {
        let detail = message
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| kind.as_str().to_string());
        Self { kind, detail }
    }
}

/// Readers for a remote source (SMB/FTP/SFTP): per-file round-trip latency through a
/// saved server means 8 sequential readers starve the lanes.
const REMOTE_SOURCE_READERS: usize = 16;

/// No durable progress for this long ends the job: the elapsed clock resets only when
/// `bytes_durable` grows, so a link that keeps reconnecting but never lands a byte
/// gives up.
pub(crate) const STALL_LIMIT: Duration = Duration::from_secs(600);

pub(crate) fn hex(b: &[u8; 16]) -> String {
    ava1::hex::encode(b)
}

fn source_for(cfg: &TransferConfig, root: &Path) -> Arc<dyn Source> {
    match &cfg.source_fs {
        Some(fs) => Arc::new(FsSource::new(fs.clone(), root.to_path_buf())),
        None => Arc::new(LocalSource::new(root.to_path_buf())),
    }
}

/// One upload, retried across connection loss with the same `job_id`: the console's
/// journal resumes the job, and durable progress bounds the retries.
pub fn upload_with_in(
    pool: &Pool,
    console: &str,
    job_id: [u8; 16],
    manifest: Manifest,
    source: Arc<dyn Source>,
    opts: SendOptions,
    cfg: &TransferConfig,
) -> Result<TransferResult> {
    let manifest = Arc::new(manifest);
    let progress = Arc::new(Progress::default());
    let cancel = cfg
        .cancel
        .clone()
        .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
    // Sender outboards (engine restart without re-reading); removed on success.
    let persist = pool.ava_dir().join("send").join(hex(&job_id));
    let dest = opts.root.clone();
    crate::block_on(async {
        SessionGate::identity(pool)?;
        let _bridge = Bridge::start(progress.clone(), cfg);
        let mut backoff = Duration::from_millis(250);
        let mut gate = SessionGate::default();
        let (mut last_at, mut last_durable) = (Instant::now(), 0u64);
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(anyhow!("transfer_cancelled"));
            }
            let durable = progress.bytes_durable.load(Ordering::Relaxed);
            if durable > last_durable {
                (last_at, last_durable) = (Instant::now(), durable);
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
            let mut link = session.job(job_id);
            let o = SendOptions {
                kind: opts.kind,
                policy: opts.policy,
                flags: opts.flags,
                root: opts.root.clone(),
                // Must stay gen::LARGE_CUTOFF (only tests may change it, C18).
                cutoff: opts.cutoff,
                readers: if cfg.source_fs.is_some() {
                    REMOTE_SOURCE_READERS
                } else {
                    opts.readers
                },
                persist: Some(persist.clone()),
                progress: progress.clone(),
                // The shared flag, not a copy (C18): flipping cfg.cancel ends the job.
                cancel: cancel.clone(),
                bandwidth_cap: cfg.bandwidth_cap_bps,
            };
            match send_job(&mut link, manifest.clone(), source.clone(), o).await {
                Ok(r) if r.status == gen::STATUS_OK => {
                    let _ = std::fs::remove_dir_all(&persist);
                    let body = serde_json::json!({
                        "protocol": "ava1",
                        "files": r.files,
                        "bytes": r.bytes,
                        "resent": r.resent,
                        "max_lanes": r.max_lanes,
                        "bottleneck": bottleneck_name(r.bottleneck),
                        "sequential": r.sequential,
                    });
                    return Ok(TransferResult {
                        tx_id_hex: hex(&job_id),
                        // The field name is FTX2's; for AVA1 it is files (C19).
                        shards_sent: u64::from(r.files),
                        bytes_sent: progress.bytes_sent.load(Ordering::Relaxed),
                        dest,
                        commit_ack_body: body.to_string(),
                    });
                }
                Ok(r) if r.status == gen::ERR_EXISTS => {
                    return Err(PostCommitError::new(PostCommitKind::Exists, r.message).into());
                }
                Ok(r) if r.status == gen::ERR_CROSS_DEVICE => {
                    return Err(PostCommitError::new(PostCommitKind::CrossDevice, r.message).into());
                }
                Ok(r) => return Err(refusal(r.status, r.message.unwrap_or_default()).into()),
                Err(SendError::Disconnected(why)) => {
                    let durable = progress.bytes_durable.load(Ordering::Relaxed);
                    pool.forget(console).await;
                    wait(&mut backoff, &format!("{why} ({durable} bytes durable)")).await;
                }
                Err(SendError::Refused { status, message }) if status == gen::ERR_EXISTS => {
                    return Err(PostCommitError::new(PostCommitKind::Exists, Some(message)).into());
                }
                Err(SendError::Refused { status, message }) if status == gen::ERR_CROSS_DEVICE => {
                    return Err(
                        PostCommitError::new(PostCommitKind::CrossDevice, Some(message)).into(),
                    );
                }
                Err(SendError::Refused { status, message }) => {
                    return Err(refusal(status, message).into());
                }
                Err(SendError::Cancelled) => return Err(anyhow!("transfer_cancelled")),
                Err(e) => return Err(anyhow!(e)),
            }
        }
    })
}

/// Jittered doubling backoff, 250 ms → 5 s. Logs without ever panicking on a closed
/// stderr (an engine under a dead parent).
pub(crate) async fn wait(backoff: &mut Duration, why: &str) {
    let jitter = Duration::from_millis(
        u64::from(std::process::id() % 97) * backoff.as_millis() as u64 / 400,
    );
    let sleep = *backoff + jitter;
    let _ = writeln!(std::io::stderr(), "ava1: reconnecting in {sleep:?}: {why}");
    tokio::time::sleep(sleep).await;
    *backoff = (*backoff * 2).min(Duration::from_secs(5));
}

pub fn upload_with(
    console: &str,
    job_id: [u8; 16],
    manifest: Manifest,
    source: Arc<dyn Source>,
    opts: SendOptions,
    cfg: &TransferConfig,
) -> Result<TransferResult> {
    upload_with_in(pool(), console, job_id, manifest, source, opts, cfg)
}

pub fn upload_file_in(
    pool: &Pool,
    cfg: &TransferConfig,
    job_id: [u8; 16],
    dest: &str,
    src: &Path,
) -> Result<TransferResult> {
    // C11: `dest` is the full destination path (parent directory + file name) —
    // `JF_SINGLE_FILE` writes `<dest>.ava-part` and renames it to `<dest>` on the
    // console, exactly the FTX2 contract at the call sites. Do not "fix" it into a
    // root/name split.
    let parent = src
        .parent()
        .ok_or_else(|| anyhow!("source has no parent directory"))?;
    let name = src
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("source name is not UTF-8"))?;
    let source = source_for(cfg, parent);
    let manifest = manifest::single(source.as_ref(), name)?;
    let mut opts = SendOptions::upload(dest);
    opts.flags = gen::JF_SINGLE_FILE;
    upload_with_in(pool, &cfg.addr, job_id, manifest, source, opts, cfg)
}

pub fn upload_dir_in(
    pool: &Pool,
    cfg: &TransferConfig,
    job_id: [u8; 16],
    dest_root: &str,
    src_dir: &Path,
) -> Result<TransferResult> {
    let source = source_for(cfg, src_dir);
    let excludes = cfg.excludes.clone();
    // The same matcher FTX2 uses, so excludes behave identically.
    let manifest = manifest::walk(source.as_ref(), &|p: &str| {
        ps5upload_core::excludes::is_excluded_strings(Path::new(p), &excludes)
    })?;
    upload_with_in(
        pool,
        &cfg.addr,
        job_id,
        manifest,
        source,
        SendOptions::upload(dest_root),
        cfg,
    )
}

/// The path within an AVA1 job's destination root. FTX2 treats relative list
/// destinations as relative to that root; absolute destinations must really be
/// below it, with a path-component boundary.
fn relative_list_path(dest_root: &str, dest: &str) -> Result<String> {
    let root = if dest_root == "/" {
        "/"
    } else {
        dest_root.trim_end_matches('/')
    };
    let rel = if dest.starts_with('/') {
        Path::new(dest)
            .strip_prefix(Path::new(root))
            .map_err(|_| anyhow!("{dest} is not under {root}"))?
    } else {
        Path::new(dest)
    };
    let rel = rel
        .to_str()
        .ok_or_else(|| anyhow!("destination is not UTF-8"))?;
    manifest::check_path(rel)?;
    Ok(rel.to_owned())
}

/// A mixed file list may contain absolute paths outside the AVA1 job root.
/// The engine routes that entire job through FTX2, which supports them.
pub fn upload_list_supported(dest_root: &str, entries: &[FileListEntry]) -> bool {
    entries
        .iter()
        .all(|e| relative_list_path(dest_root, &e.dest).is_ok())
}

pub fn upload_list_in(
    pool: &Pool,
    cfg: &TransferConfig,
    job_id: [u8; 16],
    dest_root: &str,
    entries: &[FileListEntry],
) -> Result<TransferResult> {
    let root = if dest_root == "/" {
        "/"
    } else {
        dest_root.trim_end_matches('/')
    };
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for e in entries {
        files.push((relative_list_path(root, &e.dest)?, e.src.clone().into()));
    }
    // Exactly manifest::walk's order (depth-first preorder: component comparison).
    files.sort_by(|a, b| a.0.split('/').cmp(b.0.split('/')));
    let mut all: Vec<Entry> = Vec::new();
    let mut dirs = std::collections::BTreeSet::new();
    for (rel, _) in &files {
        let mut acc = String::new();
        let parts: Vec<&str> = rel.split('/').collect();
        for c in parts[..parts.len() - 1].iter() {
            acc = if acc.is_empty() {
                (*c).to_string()
            } else {
                format!("{acc}/{c}")
            };
            dirs.insert(acc.clone());
        }
    }
    for d in dirs {
        all.push(Entry {
            kind: gen::ENTRY_DIR,
            mode: 0o755,
            size: 0,
            mtime: 0,
            path: d,
            root: None,
        });
    }
    let source: Arc<dyn Source> = Arc::new(ListSource::new(files.clone()));
    for (rel, _) in files {
        let st = source.stat(&rel)?;
        all.push(Entry {
            kind: gen::ENTRY_FILE,
            mode: st.mode,
            size: st.size,
            mtime: st.mtime,
            path: rel,
            root: None,
        });
    }
    all.sort_by(|a, b| a.path.split('/').cmp(b.path.split('/')));
    for e in &all {
        manifest::check_path(&e.path)?;
    }
    upload_with_in(
        pool,
        &cfg.addr,
        job_id,
        Manifest { entries: all },
        source,
        SendOptions::upload(root),
        cfg,
    )
}

pub fn upload_file(
    cfg: &TransferConfig,
    job_id: [u8; 16],
    dest: &str,
    src: &Path,
) -> Result<TransferResult> {
    upload_file_in(pool(), cfg, job_id, dest, src)
}

pub fn upload_dir(
    cfg: &TransferConfig,
    job_id: [u8; 16],
    dest_root: &str,
    src_dir: &Path,
) -> Result<TransferResult> {
    upload_dir_in(pool(), cfg, job_id, dest_root, src_dir)
}

pub fn upload_list(
    cfg: &TransferConfig,
    job_id: [u8; 16],
    dest_root: &str,
    entries: &[FileListEntry],
) -> Result<TransferResult> {
    upload_list_in(pool(), cfg, job_id, dest_root, entries)
}

#[cfg(test)]
mod list_destination_tests {
    use super::{relative_list_path, upload_list_supported};
    use ps5upload_core::transfer::FileListEntry;

    #[test]
    fn relative_destination_stays_under_the_requested_root() {
        assert_eq!(
            relative_list_path("/data/games", "Title/file.bin").unwrap(),
            "Title/file.bin"
        );
    }

    #[test]
    fn an_absolute_destination_uses_path_components() {
        assert_eq!(
            relative_list_path("/data/games", "/data/games/Title/file.bin").unwrap(),
            "Title/file.bin"
        );
        assert!(relative_list_path("/data/games", "/data/gamesX/file.bin").is_err());
    }

    #[test]
    fn a_destination_outside_the_root_is_rejected() {
        assert!(relative_list_path("/data/games", "/data/other/file.bin").is_err());
        assert!(relative_list_path("/data/games", "../other/file.bin").is_err());
    }

    #[test]
    fn a_mixed_list_with_one_outside_path_uses_the_ftx2_route() {
        let entries = [
            FileListEntry {
                src: "a".into(),
                dest: "Title/a".into(),
            },
            FileListEntry {
                src: "b".into(),
                dest: "/data/other/b".into(),
            },
        ];
        assert!(!upload_list_supported("/data/games", &entries));
    }
}

#[cfg(test)]
mod failure_reason_tests {
    use super::{refusal_reason, terminal_connection_reason};
    use ava1::{gen, Ava1Error};
    use std::io;

    #[test]
    fn connection_refusal_and_pairing_errors_have_distinct_terminal_reasons() {
        assert_eq!(
            terminal_connection_reason(&Ava1Error::Io(io::Error::from(
                io::ErrorKind::ConnectionRefused
            ))),
            Some("ava1_unreachable")
        );
        assert_eq!(
            terminal_connection_reason(&Ava1Error::NotPaired),
            Some("ava1_not_paired")
        );
        assert_eq!(
            terminal_connection_reason(&Ava1Error::WrongPeer),
            Some("ava1_wrong_console")
        );
        assert_eq!(terminal_connection_reason(&Ava1Error::Timeout), None);
    }

    #[test]
    fn refusals_keep_their_machine_reason() {
        assert_eq!(refusal_reason(gen::ERR_NO_SPACE), "ava1_no_space");
        assert_eq!(refusal_reason(gen::ERR_PATH), "ava1_not_allowed");
        assert_eq!(refusal_reason(gen::ERR_EXISTS), "ava1_exists");
        assert_eq!(refusal_reason(gen::ERR_CROSS_DEVICE), "ava1_cross_device");
        assert_eq!(refusal_reason(65535), "ava1_refused_65535");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused() -> Ava1Error {
        Ava1Error::Io(std::io::Error::from(std::io::ErrorKind::ConnectionRefused))
    }
    fn other() -> Ava1Error {
        Ava1Error::Io(std::io::Error::other("handshake reset"))
    }

    #[test]
    fn the_third_consecutive_refusal_ends_the_job() {
        let mut g = SessionGate::default();
        assert!(g.failed(&refused()).is_none());
        assert!(g.failed(&refused()).is_none());
        let f = g.failed(&refused()).expect("third attempt is terminal");
        assert_eq!(f.reason, "ava1_unreachable");
    }

    #[test]
    fn a_transient_error_resets_the_count() {
        let mut g = SessionGate::default();
        for _ in 0..2 {
            assert!(g.failed(&refused()).is_none());
        }
        assert!(g.failed(&other()).is_none());
        for _ in 0..2 {
            assert!(g.failed(&refused()).is_none(), "the count restarted");
        }
        assert!(g.failed(&refused()).is_some());
    }

    #[test]
    fn a_connected_session_resets_the_count() {
        let mut g = SessionGate::default();
        for _ in 0..2 {
            assert!(g.failed(&refused()).is_none());
        }
        g.connected();
        assert!(g.failed(&refused()).is_none());
    }

    #[test]
    fn pairing_and_identity_failures_have_their_own_reasons() {
        for (e, reason) in [
            (Ava1Error::NotPaired, "ava1_not_paired"),
            (Ava1Error::WrongPeer, "ava1_wrong_console"),
            (
                Ava1Error::Refused {
                    code: gen::ERR_PAIRING_CLOSED,
                    message: "closed".into(),
                },
                "ava1_not_paired",
            ),
        ] {
            let mut g = SessionGate::default();
            assert!(g.failed(&e).is_none());
            assert!(g.failed(&e).is_none());
            assert_eq!(g.failed(&e).unwrap().reason, reason);
        }
    }

    #[test]
    fn no_identity_is_terminal_immediately() {
        let d = std::env::temp_dir().join(format!("gate-noid-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("identity")).unwrap();
        let pool = Pool::new(d.clone());
        assert_eq!(
            SessionGate::identity(&pool).unwrap_err().reason,
            "ava1_no_identity"
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
