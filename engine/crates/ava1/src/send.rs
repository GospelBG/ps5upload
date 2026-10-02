//! The sender (SPEC.md §11–§12, §16): manifest, map, readers, bundles, chunks, lanes,
//! credit, requeues and the governor.
//!
//! Memory (correction 1): every byte between the blocking readers and a lane carries a
//! read-ahead permit. The permit is acquired before the read and released only when the
//! frame that holds the bytes is finally dropped — acknowledged, discarded, or lost with
//! the job — so a fast source can never buffer more than the read-ahead budget in any
//! queue, however slowly the receiver acknowledges.
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch, OwnedSemaphorePermit, Semaphore};

use crate::conn::Frame;
use crate::gen::{
    self, Bundle, BundleRecord, Chunk, Credit, Durable, FileRetry, FileRoot, JobDone, JobMap,
    JobOpen, JobOpenAck, ManifestEnd, Received, Status,
};
use crate::governor::{self, Class, Governor, Mode, Sample};
use crate::manifest::Manifest;
use crate::ranges::{from_runs, Need, RangeSet};
use crate::router::{Inbound, JobLink, LaneTx};
use crate::source::{read_full_at, Source};
use crate::verify::{self, FileHasher, Outboard, GROUP};
use crate::wire::{FrameMessage, Message};

pub struct SendOptions {
    pub kind: u8,
    pub policy: u8,
    pub flags: u32,
    pub root: String,
    /// Small/large. The protocol constant `gen::LARGE_CUTOFF` (§12.2): receivers enforce
    /// it (a Chunk for a file below it, or a BundleRecord for one at or above it, ends the
    /// job with ERR_PROTOCOL), so only tests may set another value — against a receiver
    /// configured the same way.
    pub cutoff: u64,
    pub readers: usize,
    /// Sender outboards (engine restart without re-reading).
    pub persist: Option<PathBuf>,
    pub progress: Arc<Progress>,
    pub cancel: Arc<AtomicBool>,
    /// Bytes/s the sender paces its lanes to, when the link itself is not the limit.
    pub bandwidth_cap: Option<u64>,
}

impl SendOptions {
    pub fn upload(root: &str) -> Self {
        Self {
            kind: gen::JOB_UPLOAD,
            policy: gen::POLICY_REPLACE,
            flags: 0,
            root: root.into(),
            cutoff: gen::LARGE_CUTOFF as u64,
            readers: 8,
            persist: None,
            progress: Arc::default(),
            cancel: Arc::default(),
            bandwidth_cap: None,
        }
    }
}

#[derive(Debug, Default)]
pub struct Progress {
    pub bytes_total: AtomicU64,
    pub files_total: AtomicU64,
    /// Received-acknowledged payload bytes.
    pub bytes_sent: AtomicU64,
    pub bytes_durable: AtomicU64,
    pub files_durable: AtomicU64,
    /// Payload bytes sent more than once.
    pub resent_bytes: AtomicU64,
    pub lanes: AtomicU8,
    pub bottleneck: AtomicU8,
    pub sequential: AtomicBool,
}

#[derive(Debug, Clone)]
pub struct SendReport {
    pub status: u16,
    pub message: Option<String>,
    pub files: u32,
    pub bytes: u64,
    pub resent: u64,
    pub max_lanes: u8,
    pub bottleneck: u8,
    pub sequential: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("connection lost: {0}")]
    Disconnected(String),
    #[error("refused ({status}): {message}")]
    Refused { status: u16, message: String },
    #[error("cancelled")]
    Cancelled,
    #[error("reading the source: {0}")]
    Source(#[from] std::io::Error),
    #[error("protocol: {0}")]
    Protocol(String),
}

impl From<crate::Ava1Error> for SendError {
    fn from(e: crate::Ava1Error) -> Self {
        SendError::Disconnected(e.to_string())
    }
}

// ---- pure helpers ----------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Piece {
    pub offset: u64,
    pub len: u64,
    /// false: read only to hash it (durable at the receiver, CV unknown here).
    pub send: bool,
}

/// What to read of a large file, in offset order: groups the receiver lacks (sent, in
/// runs of at most `chunk`) and durable groups whose CV this side does not know (hashed).
///
/// Pieces with `send = false` never reach a lane: their bytes are read only to finish the
/// file's root, so a consumer must never wait for them — a relay feeds only `send` pieces
/// into the pipe (Task 24).
pub fn pieces(
    size: u64,
    durable: &RangeSet,
    have_cv: &dyn Fn(u64) -> bool,
    chunk: u64,
) -> Vec<Piece> {
    let mut out: Vec<Piece> = Vec::new();
    let chunk = chunk.max(GROUP) / GROUP * GROUP;
    for g in 0..verify::groups(size) {
        let off = g * GROUP;
        let len = (size - off).min(GROUP);
        let send = !durable.covers(off, off + len);
        if !send && have_cv(g) {
            continue;
        }
        match out.last_mut() {
            Some(p)
                if p.send == send
                    && p.offset + p.len == off
                    && p.len + len <= chunk
                    && (send || p.len < chunk) =>
            {
                p.len += len
            }
            _ => out.push(Piece {
                offset: off,
                len,
                send,
            }),
        }
    }
    out
}

type LaneFrames = HashMap<u16, (u64, BTreeMap<u32, u64>)>;

/// Credit and per-lane in-flight accounting (SPEC.md §12.3–§12.5). All arithmetic is
/// checked: `sent` refuses a frame larger than the credit instead of underflowing.
pub struct Window {
    credit: u64,
    /// lane -> (unreceived bytes, seq -> frame length)
    lanes: LaneFrames,
    /// seq -> len, frames of dead lanes, refunded until a late Received charges them.
    refunded: HashMap<u32, u64>,
}

impl Window {
    pub fn new(credit: u64) -> Self {
        Self {
            credit,
            lanes: HashMap::new(),
            refunded: HashMap::new(),
        }
    }
    pub fn can_send(&self, lane: u16, len: u64, cap: u64) -> bool {
        let inflight = self.lanes.get(&lane).map_or(0, |l| l.0);
        len <= self.credit && (inflight == 0 || inflight + len <= cap)
    }
    /// Charges the frame, or refuses it when it does not fit the credit.
    pub fn sent(&mut self, lane: u16, seq: u32, len: u64) -> bool {
        if len > self.credit {
            return false;
        }
        self.credit -= len;
        let l = self.lanes.entry(lane).or_default();
        l.0 += len;
        l.1.insert(seq, len);
        true
    }
    /// Returns the frame's length when it was in flight here.
    pub fn received(&mut self, seq: u32) -> Option<u64> {
        for l in self.lanes.values_mut() {
            if let Some(len) = l.1.remove(&seq) {
                l.0 -= len;
                return Some(len);
            }
        }
        if let Some(len) = self.refunded.remove(&seq) {
            self.credit = self.credit.saturating_sub(len); // the receiver did charge it
        }
        None
    }
    pub fn credit(&mut self, n: u64) {
        self.credit += n;
    }
    /// Unreceived frames of a dead lane: refunded now, charged again if a Received arrives.
    pub fn lane_down(&mut self, lane: u16) -> Vec<u32> {
        let Some((_, frames)) = self.lanes.remove(&lane) else {
            return Vec::new();
        };
        let mut seqs = Vec::new();
        for (seq, len) in frames {
            self.credit += len;
            self.refunded.insert(seq, len);
            seqs.push(seq);
        }
        seqs
    }
    pub fn available(&self) -> u64 {
        self.credit
    }
}

// ---- the job ---------------------------------------------------------------------------

/// A frame waiting for a lane. Its read-ahead permits (correction 1) are released when
/// the frame is dropped — acknowledged, requeued and lost, or discarded with the job — so
/// every queue between the readers and the lanes stays under the read-ahead budget.
struct OutFrame {
    ty: u8,
    body: Arc<Vec<u8>>,
    class: Class,
    /// Payload bytes (without frame and message headers), for progress.
    payload: u64,
    resend: bool,
    /// Read-ahead permits (correction 1). Never read: the drop is the accounting — the
    /// permits are released when the frame is finally disposed of.
    _budget: Vec<OwnedSemaphorePermit>,
}

#[derive(Default)]
struct Sched {
    bundles: VecDeque<OutFrame>,
    chunks: VecDeque<OutFrame>,
    requeue: VecDeque<OutFrame>,
    inflight: HashMap<u32, (u16, OutFrame)>,
    bundles_inflight: usize,
    next_seq: u32,
    decision: Option<governor::Decision>,
    floor: usize,
    /// Smoothed bytes/s per lane (EWMA over ticks): lanes size their in-flight cap by it.
    lane_rate: HashMap<u16, f64>,
    /// Raw bytes acked this tick per lane, normalised into `lane_rate` at the tick.
    lane_bytes: HashMap<u16, u64>,
    credit_starved: bool,
    source_starved: bool,
    acked_tick: u64,
    small_durable_tick: u64,
    large_durable_tick: u64,
    stalls: u32,
}

struct Shared {
    sched: Mutex<Sched>,
    window: Mutex<Window>,
    /// The lanes' wake signal (correction 4): a versioned channel. A waiter marks the
    /// current version seen *before* checking its state, then awaits `changed()` — a wake
    /// between the check and the wait is a version bump and is never lost.
    wake_tx: watch::Sender<u64>,
    chunk: AtomicU32,
    bundle: AtomicU32,
    /// Read-ahead, in KiB permits: readers acquire before reading, frames carry the
    /// permit until they are finally dropped.
    bytes_budget: Arc<Semaphore>,
}

impl Shared {
    fn wake(&self) {
        let v = *self.wake_tx.borrow();
        self.wake_tx.send_replace(v.wrapping_add(1));
    }
}

enum Read {
    Record {
        file_id: u32,
        root: [u8; 32],
        data: Vec<u8>,
        budget: OwnedSemaphorePermit,
    },
    Chunk {
        file_id: u32,
        offset: u64,
        data: Vec<u8>,
        budget: OwnedSemaphorePermit,
    },
    Root {
        file_id: u32,
        root: [u8; 32],
    },
    Failed(std::io::Error),
}

const READ_AHEAD_KIB: u32 = 96 * 1024;

async fn next_ctl(link: &mut JobLink) -> Result<Frame, SendError> {
    loop {
        match link.rx.recv().await {
            Some(Inbound::Control(f)) if f.ty == Status::TYPE => continue,
            Some(Inbound::Control(f)) => return Ok(f),
            Some(Inbound::Closed(why)) => return Err(SendError::Disconnected(why)),
            None => return Err(SendError::Disconnected("the session ended".into())),
            Some(_) => {} // lane events before data starts: lanes are read from the router
        }
    }
}

/// JobOpen → ack → manifest pages → ManifestEnd → map pages. Returns (credit, need).
pub async fn open_upload(
    link: &mut JobLink,
    m: &Manifest,
    o: &SendOptions,
) -> Result<(u64, Need), SendError> {
    let job_id = link.job_id;
    link.control
        .send(&JobOpen {
            job_id,
            kind: o.kind,
            policy: o.policy,
            flags: o.flags,
            root: o.root.clone(),
            src: None,
            credit: None,
        })
        .await?;
    let ack: JobOpenAck = loop {
        let f = next_ctl(link).await?;
        if f.ty == JobOpenAck::TYPE {
            break f.decode().map_err(|e| SendError::Protocol(e.to_string()))?;
        }
    };
    if ack.status != gen::STATUS_OK {
        return Err(SendError::Refused {
            status: ack.status,
            message: ack.message.unwrap_or_default(),
        });
    }
    for p in m.pages(job_id) {
        link.control.send(&p).await?;
    }
    link.control
        .send(&ManifestEnd {
            job_id,
            files: m.files(),
            bytes: m.bytes(),
            manifest_hash: m.hash(),
        })
        .await?;
    let mut need = Need::default();
    loop {
        let f = next_ctl(link).await?;
        if f.ty == JobDone::TYPE {
            // A job that failed in prepare: the map carried the status already, or it is this.
            let d: JobDone = f.decode().map_err(|e| SendError::Protocol(e.to_string()))?;
            return Err(SendError::Refused {
                status: d.status,
                message: d.message.unwrap_or_default(),
            });
        }
        if f.ty != JobMap::TYPE {
            continue;
        }
        let map: JobMap = f.decode().map_err(|e| SendError::Protocol(e.to_string()))?;
        if map.status != gen::STATUS_OK {
            return Err(SendError::Refused {
                status: map.status,
                message: map.message.unwrap_or_default(),
            });
        }
        need.add_page(&map);
        if map.last == 1 {
            return Ok((ack.credit, need));
        }
    }
}

/// The blocking readers. Small files: `readers` threads over a shared queue. Large files:
/// one thread, file by file, piece by piece, hashing every group it reads.
fn spawn_readers(
    m: Arc<Manifest>,
    src: Arc<dyn Source>,
    small: Arc<Mutex<VecDeque<u32>>>,
    large: Arc<Mutex<VecDeque<(u32, RangeSet)>>>,
    sh: Arc<Shared>,
    o: &SendOptions,
    tx: mpsc::UnboundedSender<Read>,
) {
    let rt = tokio::runtime::Handle::current();
    for _ in 0..o.readers.max(1) {
        let (m, src, small, sh, tx, rt) = (
            m.clone(),
            src.clone(),
            small.clone(),
            sh.clone(),
            tx.clone(),
            rt.clone(),
        );
        tokio::task::spawn_blocking(move || loop {
            let Some(id) = small.lock().unwrap().pop_front() else {
                return;
            };
            let Some(e) = m.entry(id) else {
                return;
            };
            let kib = (e.size / 1024 + 1).min(READ_AHEAD_KIB as u64) as u32;
            let budget = rt
                .block_on(sh.bytes_budget.clone().acquire_many_owned(kib))
                .expect("the read-ahead semaphore is never closed");
            let mut data = vec![0u8; e.size as usize];
            let r = src
                .open(&e.path)
                .and_then(|mut f| read_full_at(f.as_mut(), 0, &mut data));
            match r {
                Ok(n) if n as u64 == e.size => {
                    let root = *blake3::hash(&data).as_bytes();
                    let _ = tx.send(Read::Record {
                        file_id: id,
                        root,
                        data,
                        budget,
                    });
                }
                Ok(_) => {
                    let _ = tx.send(Read::Failed(std::io::Error::other(format!(
                        "{} changed while it was being sent",
                        e.path
                    ))));
                    return;
                }
                Err(err) => {
                    let _ = tx.send(Read::Failed(std::io::Error::new(
                        err.kind(),
                        format!("{}: {err}", e.path),
                    )));
                    return;
                }
            }
        });
    }
    let persist = o.persist.clone();
    tokio::task::spawn_blocking(move || loop {
        let Some((id, durable)) = large.lock().unwrap().pop_front() else {
            return;
        };
        let Some(e) = m.entry(id).cloned() else {
            return;
        };
        let mut hasher = FileHasher::new(e.size);
        let mut ob = persist.as_ref().and_then(|d| {
            std::fs::create_dir_all(d).ok()?;
            Outboard::open(&d.join(format!("{id}.ob")), verify::groups(e.size)).ok()
        });
        if let Some(ob) = &ob {
            for g in 0..verify::groups(e.size) {
                if let Some(cv) = ob.get(g) {
                    hasher.set_cv(g, cv);
                }
            }
        }
        let mut f = match src.open(&e.path) {
            Ok(f) => f,
            Err(err) => {
                let _ = tx.send(Read::Failed(err));
                return;
            }
        };
        let chunk = sh.chunk.load(Ordering::Relaxed) as u64;
        let plan = pieces(e.size, &durable, &|g| hasher.cv(g).is_some(), chunk);
        for p in plan {
            // The permit is acquired before the read and rides with the frame: only the
            // frame's final drop releases it (correction 1). Hash-only pieces are read
            // into one transient buffer and never enter a queue, so they hold none.
            let budget = p
                .send
                .then(|| {
                    rt.block_on(
                        sh.bytes_budget
                            .clone()
                            .acquire_many_owned((p.len / 1024 + 1) as u32),
                    )
                })
                .transpose()
                .expect("the read-ahead semaphore is never closed");
            let mut data = vec![0u8; p.len as usize];
            match read_full_at(f.as_mut(), p.offset, &mut data) {
                Ok(n) if n as u64 == p.len => {}
                Ok(_) => {
                    let _ = tx.send(Read::Failed(std::io::Error::other(format!(
                        "{} changed while it was being sent",
                        e.path
                    ))));
                    return;
                }
                Err(err) => {
                    let _ = tx.send(Read::Failed(err));
                    return;
                }
            }
            for (k, g) in data.chunks(GROUP as usize).enumerate() {
                let gi = p.offset / GROUP + k as u64;
                hasher.add_group(gi, g);
                if let (Some(ob), Some(cv)) = (ob.as_mut(), hasher.cv(gi)) {
                    let _ = ob.put(gi, &cv);
                }
            }
            if let Some(ob) = ob.as_mut() {
                let _ = ob.sync();
            }
            if p.send {
                let budget = budget.expect("send pieces hold a read-ahead permit");
                let _ = tx.send(Read::Chunk {
                    file_id: id,
                    offset: p.offset,
                    data,
                    budget,
                });
            }
        }
        if let Some(root) = hasher.root() {
            let _ = tx.send(Read::Root { file_id: id, root });
        }
    });
}

/// Packs records into bundles; flushes a partial bundle when no record is waiting.
fn bundle_frame(job_id: [u8; 16], recs: Vec<(BundleRecord, OwnedSemaphorePermit)>) -> OutFrame {
    let payload = recs.iter().map(|r| r.0.data.len() as u64).sum();
    let body = Bundle {
        job_id,
        records: recs.iter().map(|r| r.0.clone()).collect(),
    }
    .to_bytes()
    .expect("bundle encodes");
    let budget = recs.into_iter().map(|r| r.1).collect();
    OutFrame {
        ty: Bundle::TYPE,
        body: Arc::new(body),
        class: Class::Bundle,
        payload,
        resend: false,
        _budget: budget,
    }
}

fn chunk_frame(
    job_id: [u8; 16],
    file_id: u32,
    offset: u64,
    data: Vec<u8>,
    budget: OwnedSemaphorePermit,
) -> OutFrame {
    let payload = data.len() as u64;
    let body = Chunk {
        job_id,
        file_id,
        offset,
        data,
    }
    .to_bytes()
    .expect("chunk encodes");
    OutFrame {
        ty: Chunk::TYPE,
        body: Arc::new(body),
        class: Class::Stream,
        payload,
        resend: false,
        _budget: vec![budget],
    }
}

/// Which queue the next frame sits in (the order `pick` takes them).
enum Slot {
    Requeue,
    Bundles,
    Chunks,
}

fn slot(s: &Sched) -> Option<Slot> {
    if !s.requeue.is_empty() {
        return Some(Slot::Requeue);
    }
    let d = s.decision?;
    let bundle_first = match d.mode {
        Mode::BundleOnly => true,
        Mode::StreamOnly => false,
        Mode::Mixed => s.bundles_inflight < s.floor || d.prefer == Class::Bundle,
    };
    if bundle_first {
        if !s.bundles.is_empty() {
            Some(Slot::Bundles)
        } else if !s.chunks.is_empty() {
            Some(Slot::Chunks)
        } else {
            None
        }
    } else if !s.chunks.is_empty() {
        Some(Slot::Chunks)
    } else if !s.bundles.is_empty() {
        Some(Slot::Bundles)
    } else {
        None
    }
}

/// Takes the next frame out of its queue. Correction 2: `can_send` judges exactly this
/// frame — when it must wait, `put_back` returns it to the front of the same queue.
fn pick(s: &mut Sched) -> Option<(Slot, OutFrame)> {
    let sl = slot(s)?;
    let f = match sl {
        Slot::Requeue => s.requeue.pop_front().unwrap(),
        Slot::Bundles => s.bundles.pop_front().unwrap(),
        Slot::Chunks => s.chunks.pop_front().unwrap(),
    };
    Some((sl, f))
}

fn put_back(s: &mut Sched, sl: Slot, f: OutFrame) {
    match sl {
        Slot::Requeue => s.requeue.push_front(f),
        Slot::Bundles => s.bundles.push_front(f),
        Slot::Chunks => s.chunks.push_front(f),
    }
}

/// One tick's lane-rate update: EWMA-smoothed bytes/s per lane, from the raw bytes acked
/// this tick. Correction 3: the smoothed rate persists across ticks (a lane with no bytes
/// this tick keeps its rate), and lanes size their in-flight cap by it.
fn smooth_rates(lane_bytes: &mut HashMap<u16, u64>, lane_rate: &mut HashMap<u16, f64>, secs: f64) {
    let secs = secs.max(0.001);
    for (lane, bytes) in lane_bytes.drain() {
        let rate = bytes as f64 / secs;
        let slot = lane_rate.entry(lane).or_default();
        *slot += (rate - *slot) * 0.5;
    }
}

/// One per lane: take the next frame the window allows, send it, repeat. Exits when the
/// lane's send fails (the control loop sees LaneDown and requeues) or `stop` is set.
async fn lane_task(lane: LaneTx, sh: Arc<Shared>, cap_bps: Option<u64>, stop: Arc<AtomicBool>) {
    let started = Instant::now();
    let mut sent_bytes = 0u64;
    let mut wake = sh.wake_tx.subscribe();
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        // The version is marked seen before the state check: a wake between the check and
        // the wait below lands as a version bump and is not lost (correction 4).
        let _ = *wake.borrow_and_update();
        let next = {
            let mut s = sh.sched.lock().unwrap();
            let mut w = sh.window.lock().unwrap();
            let chunk = sh.chunk.load(Ordering::Relaxed);
            let rate = s.lane_rate.get(&lane.id).copied().unwrap_or(0.0);
            let cap = governor::inflight_cap(chunk, rate);
            match pick(&mut s) {
                Some((sl, f)) => {
                    let len = f.body.len() as u64;
                    if !w.can_send(lane.id, len, cap) {
                        if len > w.available() {
                            s.credit_starved = true;
                        }
                        put_back(&mut s, sl, f);
                        None
                    } else {
                        s.next_seq += 1;
                        let seq = s.next_seq;
                        let ty = f.ty;
                        let body = (*f.body).clone();
                        if f.class == Class::Bundle {
                            s.bundles_inflight += 1;
                        }
                        assert!(w.sent(lane.id, seq, len), "can_send passed");
                        s.inflight.insert(seq, (lane.id, f));
                        Some((seq, ty, body))
                    }
                }
                None => {
                    if s.requeue.is_empty() && s.bundles.is_empty() && s.chunks.is_empty() {
                        s.source_starved = true;
                    }
                    None
                }
            }
        };
        let Some((seq, ty, body)) = next else {
            if wake.changed().await.is_err() {
                return;
            }
            continue;
        };
        if let Some(bps) = cap_bps {
            sent_bytes += body.len() as u64;
            let due = Duration::from_secs_f64(sent_bytes as f64 / bps as f64);
            if let Some(wait) = due.checked_sub(started.elapsed()) {
                tokio::time::sleep(wait).await;
            }
        }
        // Queued whole or not at all (the outbox), so this task may be cancelled here
        // without leaving half a sealed frame on the lane.
        if lane.tx.send_raw(ty, 0, seq, body).await.is_err() {
            return;
        }
    }
}

/// The data phase of an upload: what `open_upload` returned (credit, need) in, a report
/// (or the error that ended the job) out. Every lane task is cancelled and joined before
/// this returns, on every exit (correction 5).
pub async fn run_upload(
    link: &mut JobLink,
    manifest: Arc<Manifest>,
    source: Arc<dyn Source>,
    opts: SendOptions,
    opened: (u64, Need),
) -> Result<SendReport, SendError> {
    let (credit, need) = opened;
    let job_id = link.job_id;
    let pg = opts.progress.clone();
    pg.bytes_total.store(manifest.bytes(), Ordering::Relaxed);
    pg.files_total
        .store(manifest.files() as u64, Ordering::Relaxed);

    // What is left to send.
    let (mut small, mut large) = (VecDeque::new(), VecDeque::new());
    let (mut small_left, mut large_left) = (0u64, 0u64);
    let mut durable_files: HashSet<u32> = need.done.iter().copied().collect();
    for (i, e) in manifest.entries.iter().enumerate() {
        let id = i as u32;
        if e.kind != gen::ENTRY_FILE || durable_files.contains(&id) {
            continue;
        }
        if e.size < opts.cutoff {
            small.push_back(id);
            small_left += e.size;
        } else {
            let d = need.partial.get(&id).cloned().unwrap_or_default();
            large_left += e.size - d.covered();
            large.push_back((id, d));
        }
    }
    let done_bytes: u64 = durable_files
        .iter()
        .map(|i| manifest.entry(*i).map_or(0, |e| e.size))
        .sum::<u64>()
        + need.partial.values().map(|r| r.covered()).sum::<u64>();
    pg.bytes_durable.store(done_bytes, Ordering::Relaxed);
    pg.files_durable
        .store(durable_files.len() as u64, Ordering::Relaxed);

    let sh = Arc::new(Shared {
        sched: Mutex::new(Sched {
            floor: 4,
            ..Default::default()
        }),
        window: Mutex::new(Window::new(credit)),
        wake_tx: watch::channel(0).0,
        chunk: AtomicU32::new(governor::START_CHUNK),
        bundle: AtomicU32::new(governor::START_BUNDLE),
        bytes_budget: Arc::new(Semaphore::new(READ_AHEAD_KIB as usize)),
    });
    let mut gov = Governor::new();
    let first = gov.tick(&Sample::default());
    sh.sched.lock().unwrap().decision = Some(first);
    let small_q = Arc::new(Mutex::new(small));
    let large_q = Arc::new(Mutex::new(large));
    let (rtx, mut rrx) = mpsc::unbounded_channel();
    spawn_readers(
        manifest.clone(),
        source.clone(),
        small_q.clone(),
        large_q.clone(),
        sh.clone(),
        &opts,
        rtx.clone(),
    );

    // Lanes: open the governor's starting count (client side); adopt any already up.
    let stop = Arc::new(AtomicBool::new(false));
    let mut lane_tasks: HashMap<u16, tokio::task::JoinHandle<()>> = HashMap::new();
    if let Some(op) = link.opener().cloned() {
        while link.lanes().len() < first.lanes as usize {
            op.open()
                .await
                .map_err(|e| SendError::Disconnected(e.to_string()))?;
        }
    }
    let spawn_lane =
        |id: u16, tasks: &mut HashMap<u16, tokio::task::JoinHandle<()>>, link: &JobLink| {
            if tasks.contains_key(&id) {
                return;
            }
            if let Some(l) = link.lane(id) {
                let h = tokio::spawn(lane_task(l, sh.clone(), opts.bandwidth_cap, stop.clone()));
                tasks.insert(id, h);
            }
        };
    for l in link.lanes() {
        spawn_lane(l.id, &mut lane_tasks, link);
    }

    let mut pending: Vec<(BundleRecord, OwnedSemaphorePermit)> = Vec::new();
    let mut pending_bytes = 0usize;
    let mut retries: HashMap<u32, u32> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_tick = Instant::now();
    let (mut max_lanes, mut receiver_bn, mut sequential) = (0u8, gen::BN_NONE, false);
    let mut last_bn = gen::BN_NONE;
    let result = loop {
        if opts.cancel.load(Ordering::Relaxed) {
            let _ = link
                .control
                .send(&gen::JobCancel {
                    job_id,
                    reason: gen::ERR_CANCELLED,
                })
                .await;
            break Err(SendError::Cancelled);
        }
        tokio::select! {
            r = rrx.recv() => match r {
                Some(Read::Record { file_id, root, data, budget }) => {
                    pending_bytes += data.len() + 48;
                    pending.push((BundleRecord { file_id, root, data }, budget));
                    // A slow source never holds a half-full bundle back: flush when the
                    // record channel is momentarily empty; a fast source keeps it full and
                    // the bundles reach the governor's size.
                    let flush = pending_bytes >= sh.bundle.load(Ordering::Relaxed) as usize || rrx.is_empty();
                    if flush {
                        let f = bundle_frame(job_id, std::mem::take(&mut pending));
                        pending_bytes = 0;
                        sh.sched.lock().unwrap().bundles.push_back(f);
                        sh.wake();
                    }
                }
                Some(Read::Chunk { file_id, offset, data, budget }) => {
                    let f = chunk_frame(job_id, file_id, offset, data, budget);
                    sh.sched.lock().unwrap().chunks.push_back(f);
                    sh.wake();
                }
                Some(Read::Root { file_id, root }) => {
                    if let Err(e) = link.control.send(&FileRoot { job_id, file_id, root }).await {
                        break Err(SendError::Disconnected(e.to_string()));
                    }
                }
                Some(Read::Failed(e)) => {
                    let _ = link.control.send(&gen::JobCancel { job_id, reason: gen::ERR_IO }).await;
                    break Err(SendError::Source(e));
                }
                // The retry path holds `rtx` alive, so the channel never closes mid-job.
                None => {}
            },
            ev = link.rx.recv() => match ev {
                None => break Err(SendError::Disconnected("the session ended".into())),
                Some(Inbound::Closed(why)) => break Err(SendError::Disconnected(why)),
                Some(Inbound::LaneUp(id)) => spawn_lane(id, &mut lane_tasks, link),
                Some(Inbound::LaneDown(id)) => {
                    if let Some(h) = lane_tasks.remove(&id) {
                        h.abort();
                    }
                    let seqs = sh.window.lock().unwrap().lane_down(id);
                    let mut s = sh.sched.lock().unwrap();
                    for seq in seqs {
                        if let Some((_, mut f)) = s.inflight.remove(&seq) {
                            if f.class == Class::Bundle {
                                s.bundles_inflight -= 1;
                            }
                            f.resend = true;
                            s.requeue.push_back(f);
                        }
                    }
                    s.stalls += 1;
                    drop(s);
                    sh.wake();
                }
                Some(Inbound::Lane { .. }) => {} // an uploader receives nothing on lanes
                Some(Inbound::Control(f)) => match f.ty {
                    Received::TYPE => match f.decode::<Received>() {
                        Ok(r) => {
                            let got = sh.window.lock().unwrap().received(r.seq);
                            let mut s = sh.sched.lock().unwrap();
                            if let (Some(len), Some((lane, fr))) = (got, s.inflight.remove(&r.seq)) {
                                if fr.class == Class::Bundle {
                                    s.bundles_inflight -= 1;
                                }
                                s.acked_tick += len;
                                *s.lane_bytes.entry(lane).or_default() += len;
                                pg.bytes_sent.fetch_add(fr.payload, Ordering::Relaxed);
                                if fr.resend {
                                    pg.resent_bytes.fetch_add(fr.payload, Ordering::Relaxed);
                                }
                            }
                            drop(s);
                            sh.wake();
                        }
                        Err(e) => break Err(SendError::Protocol(e.to_string())),
                    },
                    Credit::TYPE => match f.decode::<Credit>() {
                        Ok(c) => {
                            sh.window.lock().unwrap().credit(c.bytes);
                            sh.wake();
                        }
                        Err(e) => break Err(SendError::Protocol(e.to_string())),
                    },
                    Durable::TYPE => match f.decode::<Durable>() {
                        Ok(d) => {
                            let mut s = sh.sched.lock().unwrap();
                            for id in from_runs(&d.files) {
                                if durable_files.insert(id) {
                                    let size = manifest.entry(id).map_or(0, |e| e.size);
                                    pg.files_durable.fetch_add(1, Ordering::Relaxed);
                                    if size < opts.cutoff {
                                        pg.bytes_durable.fetch_add(size, Ordering::Relaxed);
                                        s.small_durable_tick += size;
                                        small_left = small_left.saturating_sub(size);
                                    }
                                }
                            }
                            for r in &d.ranges {
                                pg.bytes_durable.fetch_add(r.len, Ordering::Relaxed);
                                s.large_durable_tick += r.len;
                                large_left = large_left.saturating_sub(r.len);
                            }
                        }
                        Err(e) => break Err(SendError::Protocol(e.to_string())),
                    },
                    FileRetry::TYPE => match f.decode::<FileRetry>() {
                        Ok(r) => {
                            let n = retries.entry(r.file_id).or_default();
                            *n += 1;
                            if *n > 3 {
                                let _ = link.control.send(&gen::JobCancel { job_id, reason: gen::ERR_VERIFY }).await;
                                break Err(SendError::Protocol(format!(
                                    "{} failed verification 3 times",
                                    manifest.entry(r.file_id).map_or_else(|| "?".into(), |e| e.path.clone())
                                )));
                            }
                            if let Some(d) = &opts.persist {
                                let _ = std::fs::remove_file(d.join(format!("{}.ob", r.file_id)));
                            }
                            let small_file = manifest.entry(r.file_id).is_some_and(|e| e.size < opts.cutoff);
                            if small_file {
                                small_q.lock().unwrap().push_back(r.file_id);
                            } else {
                                large_q.lock().unwrap().push_back((r.file_id, RangeSet::new()));
                            }
                            // The reader threads may have exited; start a fresh set for the retried file.
                            spawn_readers(manifest.clone(), source.clone(), small_q.clone(), large_q.clone(), sh.clone(), &opts, rtx.clone());
                        }
                        Err(e) => break Err(SendError::Protocol(e.to_string())),
                    },
                    Status::TYPE => {
                        if let Ok(st) = f.decode::<Status>() {
                            receiver_bn = st.bottleneck;
                            sh.sched.lock().unwrap().floor = st.workers.max(1) as usize;
                        }
                    }
                    JobDone::TYPE => match f.decode::<JobDone>() {
                        Ok(d) => break Ok(SendReport {
                            status: d.status,
                            message: d.message,
                            files: d.files,
                            bytes: d.bytes,
                            resent: pg.resent_bytes.load(Ordering::Relaxed),
                            max_lanes,
                            bottleneck: last_bn,
                            sequential,
                        }),
                        Err(e) => break Err(SendError::Protocol(e.to_string())),
                    },
                    _ => {}
                },
            },
            _ = tick.tick() => {
                let secs = last_tick.elapsed().as_secs_f64();
                last_tick = Instant::now();
                let lanes_now = link.lanes().len() as u8;
                let sample = {
                    let mut s = sh.sched.lock().unwrap();
                    let mut lane_bytes = std::mem::take(&mut s.lane_bytes);
                    smooth_rates(&mut lane_bytes, &mut s.lane_rate, secs);
                    Sample {
                        secs,
                        bytes_acked: std::mem::take(&mut s.acked_tick),
                        lanes: lanes_now,
                        stalls: std::mem::take(&mut s.stalls),
                        credit_starved: std::mem::take(&mut s.credit_starved),
                        source_starved: std::mem::take(&mut s.source_starved) && s.requeue.is_empty(),
                        receiver_bottleneck: receiver_bn,
                        small_durable: std::mem::take(&mut s.small_durable_tick),
                        large_durable: std::mem::take(&mut s.large_durable_tick),
                        small_left,
                        large_left,
                    }
                };
                let d = gov.tick(&sample);
                sh.chunk.store(d.chunk, Ordering::Relaxed);
                sh.bundle.store(d.bundle, Ordering::Relaxed);
                sh.sched.lock().unwrap().decision = Some(d);
                sequential = d.sequential;
                last_bn = d.bottleneck;
                pg.bottleneck.store(d.bottleneck, Ordering::Relaxed);
                pg.sequential.store(d.sequential, Ordering::Relaxed);
                pg.lanes.store(lanes_now, Ordering::Relaxed);
                max_lanes = max_lanes.max(lanes_now);
                if let Some(op) = link.opener().cloned() {
                    if lanes_now < d.lanes {
                        let _ = op.open().await; // LaneUp follows
                    } else if lanes_now > d.lanes {
                        if let Some(l) = link.lanes().last() {
                            op.close(l.id); // LaneDown follows and requeues
                        }
                    }
                }
                sh.wake();
            }
        }
    };
    // Every exit: stop the lanes, wake the sleepers, cancel and join every lane task
    // (correction 5) — nothing of this job keeps running after the return.
    stop.store(true, Ordering::Relaxed);
    sh.wake();
    for (_, h) in lane_tasks.drain() {
        h.abort();
        let _ = h.await;
    }
    result
}

/// One upload from open to report: `open_upload` then `run_upload`.
pub async fn send_job(
    link: &mut JobLink,
    manifest: Arc<Manifest>,
    source: Arc<dyn Source>,
    opts: SendOptions,
) -> Result<SendReport, SendError> {
    let opened = open_upload(link, &manifest, &opts).await?;
    run_upload(link, manifest, source, opts, opened).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::{FrameReader, FrameWriter};
    use crate::manifest::Entry;
    use crate::router::{is_data_type, ConnTx, Router};
    use crate::session::Timing;
    use crate::source::SourceMeta;
    use crate::wire::SplitMix;
    use std::io;
    use tokio::io::{duplex, split};

    const G: u64 = crate::verify::GROUP;

    #[test]
    fn pieces_send_the_missing_groups_and_hash_the_unknown_ones() {
        let mut durable = RangeSet::new();
        durable.insert(0, 2 * G);
        durable.insert(4 * G, 5 * G);
        let size = 6 * G + 7;
        // CVs known for group 0 only: group 1 and 4 are durable but must be read for the root.
        let p = pieces(size, &durable, &|g| g == 0, 2 * G);
        let got: Vec<(u64, u64, bool)> = p.iter().map(|x| (x.offset / G, x.len, x.send)).collect();
        assert_eq!(
            got,
            vec![
                (1, G, false),
                (2, 2 * G, true),
                (4, G, false),
                (5, G + 7, true)
            ]
        );
    }

    #[test]
    fn pieces_never_cross_a_chunk_limit_or_a_group_boundary() {
        let p = pieces(10 * G + 1, &RangeSet::new(), &|_| false, 3 * G);
        assert!(p
            .iter()
            .all(|x| x.offset % G == 0 && x.len <= 3 * G && x.send));
        assert_eq!(p.iter().map(|x| x.len).sum::<u64>(), 10 * G + 1);
    }

    #[test]
    fn every_chunk_but_a_files_last_is_whole_groups() {
        // Correction 6, the wire contract: the receiver answers ERR_PROTOCOL on a mid-file
        // chunk that is not a whole number of 1 MiB groups.
        let mut rng = SplitMix(11);
        for _ in 0..300 {
            let size = rng.below(16 * G) + 1;
            let mut durable = RangeSet::new();
            let mut left = rng.below(6);
            while left > 0 {
                let s = rng.below(size);
                durable.insert(s, (s + rng.below(size - s) + 1).min(size));
                left -= 1;
            }
            let chunk = (rng.below(5) + 1) * G;
            let cv_at = rng.below(16);
            let p = pieces(size, &durable, &|g| g == cv_at, chunk);
            for x in &p {
                assert!(x.len <= chunk && x.offset % G == 0, "{x:?} of {size}");
                if x.offset + x.len < size {
                    assert_eq!(x.len % G, 0, "a mid-file piece is not whole groups: {x:?}");
                }
            }
        }
    }

    #[test]
    fn the_window_refunds_lost_frames_and_charges_late_receipts() {
        let mut w = Window::new(100);
        assert!(w.can_send(1, 60, 1000));
        w.sent(1, 7, 60);
        assert!(!w.can_send(2, 60, 1000), "credit");
        w.lane_down(1); // frame 7 never acknowledged: refunded
        assert!(w.can_send(2, 60, 1000));
        w.received(7); // ...but it had arrived after all
        assert!(!w.can_send(2, 60, 1000));
        w.credit(60);
        assert!(w.can_send(2, 60, 1000));
        w.sent(2, 8, 60);
        assert!(!w.can_send(2, 30, 70), "the lane's in-flight cap");
        assert_eq!(w.received(8), Some(60));
    }

    #[test]
    fn sent_never_takes_more_than_the_credit() {
        let mut w = Window::new(50);
        assert!(
            !w.sent(1, 1, 60),
            "a frame larger than the credit is refused"
        );
        assert_eq!(w.available(), 50);
        assert!(w.sent(1, 2, 50));
        assert_eq!(w.available(), 0);
    }

    #[test]
    fn the_window_never_underflows_under_mixed_sizes() {
        // Correction 2: checked arithmetic; mixed sizes under low credit never panic and
        // the credit stays within its initial grant plus what was credited.
        let mut rng = SplitMix(3);
        let mut w = Window::new(500);
        let mut credited = 0u64;
        for _ in 0..20_000 {
            match rng.below(6) {
                0 => {
                    let lane = (rng.below(3) + 1) as u16;
                    let len = rng.below(600) + 1;
                    if w.can_send(lane, len, 1000) {
                        assert!(w.sent(lane, rng.below(500) as u32, len));
                    }
                }
                1 => {
                    let _ = w.received(rng.below(500) as u32);
                }
                2 => {
                    let lane = (rng.below(3) + 1) as u16;
                    let _ = w.lane_down(lane);
                }
                3 => {
                    let n = rng.below(400);
                    w.credit(n);
                    credited += n;
                }
                _ => {
                    let _ = w.available();
                }
            }
            assert!(w.available() <= 500 + credited, "credit over-accounted");
        }
    }

    #[test]
    fn lane_rates_are_smoothed_and_survive_idle_ticks() {
        // Correction 3: lanes size their in-flight cap by a real, persistent rate — not a
        // per-tick byte count cleared before the lanes read it.
        let mut bytes: HashMap<u16, u64> = HashMap::new();
        let mut rate: HashMap<u16, f64> = HashMap::new();
        for _ in 0..8 {
            bytes.insert(1, 10_000_000);
            smooth_rates(&mut bytes, &mut rate, 1.0);
        }
        assert!(
            rate[&1] > 9.9e6,
            "the EWMA converges to the true rate: {}",
            rate[&1]
        );
        let before = rate[&1];
        smooth_rates(&mut bytes, &mut rate, 1.0);
        assert_eq!(rate[&1], before, "an idle tick must not reset the rate");
        assert_eq!(
            governor::inflight_cap(4 << 20, rate[&1]),
            (rate[&1] * 2.0) as u64,
            "the cap is two seconds of the lane's rate"
        );
    }

    fn test_shared(chunk: u32, budget_kib: usize) -> Arc<Shared> {
        Arc::new(Shared {
            sched: Mutex::new(Sched {
                floor: 4,
                ..Default::default()
            }),
            window: Mutex::new(Window::new(1 << 20)),
            wake_tx: watch::channel(0).0,
            chunk: AtomicU32::new(chunk),
            bundle: AtomicU32::new(governor::START_BUNDLE),
            bytes_budget: Arc::new(Semaphore::new(budget_kib)),
        })
    }

    fn test_frame(ty: u8, payload: u64) -> OutFrame {
        OutFrame {
            ty,
            body: Arc::new(
                Chunk {
                    job_id: [0x78; 16],
                    file_id: 0,
                    offset: 0,
                    data: vec![0x22; payload as usize],
                }
                .to_bytes()
                .unwrap(),
            ),
            class: Class::Stream,
            payload,
            resend: false,
            _budget: Vec::new(),
        }
    }

    #[tokio::test]
    async fn a_wake_between_the_check_and_the_wait_is_not_lost() {
        // Correction 4: the exact interleaving a lost wake needs — the waiter has checked
        // (nothing to do) and is about to wait when the producer queues work and wakes.
        // The versioned channel sees the bump; a bare Notify would leave the waiter asleep.
        let sh = test_shared(governor::START_CHUNK, READ_AHEAD_KIB as usize);
        let (armed, fire) = tokio::sync::oneshot::channel();
        let waiter = {
            let sh = sh.clone();
            tokio::spawn(async move {
                let mut rx = sh.wake_tx.subscribe();
                let mut armed = Some(armed);
                loop {
                    let _ = *rx.borrow_and_update(); // the version is seen first
                    let work = {
                        let s = sh.sched.lock().unwrap();
                        !s.chunks.is_empty() || !s.bundles.is_empty() || !s.requeue.is_empty()
                    };
                    if work {
                        return;
                    }
                    if let Some(a) = armed.take() {
                        if a.send(()).is_err() {
                            return;
                        }
                    }
                    if rx.changed().await.is_err() {
                        return;
                    }
                }
            })
        };
        fire.await.unwrap(); // the waiter checked, found nothing, and is (about to be) waiting
        sh.sched
            .lock()
            .unwrap()
            .chunks
            .push_back(test_frame(Chunk::TYPE, 64));
        sh.wake();
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("the wake between the check and the wait was lost")
            .unwrap();
    }

    /// A `ReadAt` over memory, and a `Source` that serves it as one file.
    struct MemRead {
        data: Vec<u8>,
    }
    impl crate::source::ReadAt for MemRead {
        fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<usize> {
            let n = buf.len().min(self.data.len().saturating_sub(off as usize));
            if n > 0 {
                buf[..n].copy_from_slice(&self.data[off as usize..off as usize + n]);
            }
            Ok(n)
        }
    }
    struct MemSource(Vec<u8>);
    impl Source for MemSource {
        fn open(&self, _rel: &str) -> io::Result<Box<dyn crate::source::ReadAt>> {
            Ok(Box::new(MemRead {
                data: self.0.clone(),
            }))
        }
        fn list(&self, _rel: &str) -> io::Result<Vec<(String, SourceMeta)>> {
            Ok(Vec::new())
        }
        fn stat(&self, _rel: &str) -> io::Result<SourceMeta> {
            Ok(SourceMeta {
                size: self.0.len() as u64,
                mtime: 0,
                mode: 0o644,
                is_dir: false,
            })
        }
    }

    #[tokio::test]
    async fn reader_bytes_stay_under_the_read_ahead_budget() {
        // Correction 1: the permit is acquired before the read and rides with the frame,
        // so a fast source can never buffer more than the budget in the queues.
        let g = G as usize;
        let size = 16 * g;
        let sh = test_shared(g as u32, g / 1024 + 8); // one piece plus the per-piece slack
        let m = Arc::new(Manifest {
            entries: vec![Entry {
                kind: gen::ENTRY_FILE,
                mode: 0o644,
                size: size as u64,
                mtime: 1,
                path: "f".into(),
                root: None,
            }],
        });
        let src: Arc<dyn Source> = Arc::new(MemSource(vec![0x5a; size]));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let small = Arc::new(Mutex::new(VecDeque::new()));
        let large = Arc::new(Mutex::new(VecDeque::from([(0u32, RangeSet::new())])));
        spawn_readers(
            m,
            src,
            small,
            large,
            sh.clone(),
            &SendOptions::upload(""),
            tx,
        );
        let budget = sh.bytes_budget.clone();
        let first = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap();
        match &first {
            Read::Chunk { data, .. } => assert!(data.len() <= g),
            _ => panic!("expected a chunk"),
        }
        // The read bytes still hold the budget: less than one piece's worth is left.
        assert!(
            budget.available_permits() < g / 1024 + 1,
            "{}",
            budget.available_permits()
        );
        // ...and nothing more can be read while the piece waits in the queue.
        assert!(tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .is_err());
        // The permit travels with the frame: dropped, the budget returns and the next
        // piece is read.
        drop(first);
        let second = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(second, Read::Chunk { .. }));
    }

    #[tokio::test]
    async fn a_lane_task_stops_promptly_when_the_job_ends() {
        // The teardown's other half (correction 5): a lane task sleeping on the wake
        // signal sees `stop` and exits, so the join never hangs.
        let timing = Timing {
            ping_every: Duration::from_secs(3600),
            dead_after: Duration::from_secs(3600),
            handshake: Duration::from_secs(1),
            min_frame_rate: crate::link::MIN_FRAME_RATE,
        };
        let (a, b) = duplex(1 << 20);
        let (ar, aw) = split(a);
        let (tx, _rx) = mpsc::channel(crate::link::DELIVER_DEPTH);
        let (_link, outbox) =
            crate::link::drive(FrameReader::new(ar), FrameWriter::new(aw), timing, tx);
        let mut peer = FrameReader::new(b);
        peer.set_max_body(crate::frame::MAX_BODY);
        // The lane's frame must be taken or the send would block.
        let drain = tokio::spawn(async move {
            let _ = peer.recv().await;
        });
        let router = Arc::new(Router::default());
        router.lane_up(1, outbox);
        let lane = router.lane(1).unwrap();
        let sh = test_shared(governor::START_CHUNK, READ_AHEAD_KIB as usize);
        sh.sched
            .lock()
            .unwrap()
            .chunks
            .push_back(test_frame(Chunk::TYPE, 1 << 20));
        let stop = Arc::new(AtomicBool::new(false));
        let h = tokio::spawn(lane_task(lane, sh.clone(), None, stop.clone()));
        tokio::time::timeout(Duration::from_secs(5), drain)
            .await
            .unwrap()
            .unwrap();
        stop.store(true, Ordering::Relaxed);
        sh.wake();
        tokio::time::timeout(Duration::from_secs(1), h)
            .await
            .expect("the lane task did not stop")
            .unwrap();
    }

    #[tokio::test]
    async fn an_injected_receiver_error_ends_the_job_promptly_with_no_lane_left_sending() {
        // Correction 5: no early return from a select arm — on any error every lane task
        // is cancelled and joined before `send_job` reports it. The fake receiver answers
        // the open, answers the map, acknowledges the first chunk, then sends a malformed
        // Received; the job must end with Protocol and nothing may keep sending on the lane.
        let timing = Timing {
            ping_every: Duration::from_secs(3600),
            dead_after: Duration::from_secs(3600),
            handshake: Duration::from_secs(5),
            min_frame_rate: crate::link::MIN_FRAME_RATE,
        };
        let job = [0x79u8; 16];
        // The control connection.
        let (ca, cb) = duplex(1 << 20);
        let (car, caw) = split(ca);
        let (cbr, cbw) = split(cb);
        let (ctx, mut crx) = mpsc::channel(crate::link::DELIVER_DEPTH);
        let (_clink, coutbox) =
            crate::link::drive(FrameReader::new(car), FrameWriter::new(caw), timing, ctx);
        // One lane.
        let (la, lb) = duplex(1 << 20);
        let (lar, law) = split(la);
        let (lbr, _lbw) = split(lb);
        let (ltx, _lrx) = mpsc::channel(crate::link::DELIVER_DEPTH);
        let (_llink, loutbox) =
            crate::link::drive(FrameReader::new(lar), FrameWriter::new(law), timing, ltx);
        let router = Arc::new(Router::default());
        router.lane_up(1, loutbox);
        let mut link = JobLink::new(job, router.clone(), ConnTx::new(coutbox), None);
        // Route the control connection's frames into the job, like the session dispatcher.
        let r2 = router.clone();
        tokio::spawn(async move {
            while let Some(f) = crx.recv().await {
                if is_data_type(f.ty) {
                    let _ = r2.route_control(f).await;
                }
            }
            r2.close("the session ended");
        });
        // The fake receiver: answers the open and the map, acknowledges lane frames, and
        // after the first acknowledgement injects the malformed Received.
        let lane_seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let ls = lane_seen.clone();
        tokio::spawn(async move {
            let mut peer = FrameReader::new(cbr);
            peer.set_max_body(crate::frame::MAX_BODY);
            let mut lane_peer = FrameReader::new(lbr);
            lane_peer.set_max_body(crate::frame::MAX_BODY);
            let mut peer_w = FrameWriter::new(cbw);
            loop {
                tokio::select! {
                    f = peer.recv() => match f {
                        Ok(f) if f.ty == JobOpen::TYPE => {
                            let open: JobOpen = f.decode().unwrap();
                            peer_w.send_msg(0, &JobOpenAck {
                                job_id: open.job_id,
                                status: 0,
                                credit: 64 << 20,
                                staged: 1,
                                workers: 4,
                                message: None,
                            }).await.unwrap();
                        }
                        Ok(f) if f.ty == ManifestEnd::TYPE => {
                            peer_w.send_msg(0, &JobMap {
                                job_id: job,
                                status: 0,
                                last: 1,
                                done: vec![],
                                partial: vec![],
                                message: None,
                            }).await.unwrap();
                        }
                        Ok(_) => {}
                        Err(_) => return,
                    },
                    f = lane_peer.recv() => match f {
                        Ok(f) if is_data_type(f.ty) => {
                            let n = ls.fetch_add(1, Ordering::Relaxed);
                            let _ = peer_w.send_msg(0, &Received { job_id: job, lane: 1, seq: f.channel }).await;
                            if n == 0 {
                                // The first chunk is acknowledged; now the injected error: a
                                // Received whose body is just the job id (its decode fails).
                                peer_w.send(Received::TYPE, 0, &job).await.unwrap();
                            }
                        }
                        Ok(_) => {}
                        Err(_) => return,
                    },
                }
            }
        });

        let dir = std::env::temp_dir().join(format!("ava1-send-err-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f"), vec![0x33u8; 64 << 20]).unwrap();
        let m = Arc::new(Manifest {
            entries: vec![Entry {
                kind: gen::ENTRY_FILE,
                mode: 0o644,
                size: 64 << 20,
                mtime: 1,
                path: "f".into(),
                root: None,
            }],
        });
        let result = tokio::time::timeout(
            Duration::from_secs(15),
            send_job(
                &mut link,
                m,
                Arc::new(crate::source::LocalSource::new(dir.clone())),
                SendOptions::upload("dest"),
            ),
        )
        .await;
        let err = result
            .expect("the job ended promptly")
            .expect_err("the injected error surfaces");
        assert!(matches!(err, SendError::Protocol(_)), "{err:?}");
        // Nothing was left running: no frame arrives on the lane after the error.
        let n = lane_seen.load(Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            lane_seen.load(Ordering::Relaxed),
            n,
            "a lane task kept sending after the error"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
