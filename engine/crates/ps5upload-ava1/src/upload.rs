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
use ps5upload_core::transfer::{FileListEntry, TransferConfig, TransferResult};

use crate::pool::{pool, Pool};
use crate::progress::{bottleneck_name, Bridge};
use crate::source::{FsSource, ListSource};

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
const STALL_LIMIT: Duration = Duration::from_secs(600);

fn hex(b: &[u8; 16]) -> String {
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
        let _bridge = Bridge::start(progress.clone(), cfg);
        let mut backoff = Duration::from_millis(250);
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
                    wait(&mut backoff, &e.to_string()).await;
                    continue;
                }
            };
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
                Ok(r) => {
                    return Err(anyhow!(
                        "console refused the transfer ({}): {}",
                        r.status,
                        r.message.unwrap_or_default()
                    ))
                }
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
                Err(SendError::Cancelled) => return Err(anyhow!("transfer_cancelled")),
                Err(e) => return Err(anyhow!(e)),
            }
        }
    })
}

/// Jittered doubling backoff, 250 ms → 5 s. Logs without ever panicking on a closed
/// stderr (an engine under a dead parent).
async fn wait(backoff: &mut Duration, why: &str) {
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

pub fn upload_list_in(
    pool: &Pool,
    cfg: &TransferConfig,
    job_id: [u8; 16],
    dest_root: &str,
    entries: &[FileListEntry],
) -> Result<TransferResult> {
    let root = dest_root.trim_end_matches('/');
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for e in entries {
        let rel = e
            .dest
            .strip_prefix(root)
            .map(|r| r.trim_start_matches('/'))
            .filter(|r| !r.is_empty())
            .ok_or_else(|| anyhow!("{} is not under {root}", e.dest))?;
        files.push((rel.to_string(), e.src.clone().into()));
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
