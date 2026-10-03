//! Bounded engine relay: ordered download from A feeds an upload to B in RAM.
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use ava1::gen;
use ava1::manifest::Manifest;
use ava1::ranges::Need;
use ava1::recv::{download_open, download_run, RecvOptions, Sink};
use ava1::send::{open_upload, run_upload, Progress, SendError, SendOptions, SendReport};
use ava1::source::{ReadAt, Source, SourceMeta};

use crate::pool::{pool, Pool};

const RELAY_CAP: usize = 64 << 20;
// A blocked source or lane may leave both halves alive without progress.
const RELAY_WAIT: Duration = Duration::from_secs(120);
const STALL_LIMIT: Duration = Duration::from_secs(600);

struct State {
    chunks: BTreeMap<(u32, u64), Vec<u8>>,
    bytes: usize,
    failed: bool,
}

struct Relay {
    count: usize,
    expected: Need,
    state: Mutex<State>,
    changed: Condvar,
}

impl Relay {
    fn new(m: &Manifest, skip: &Need) -> Self {
        let mut expected = Need::default();
        for (i, e) in m.entries.iter().enumerate() {
            if e.kind != gen::ENTRY_FILE || skip.done.contains(&(i as u32)) {
                continue;
            }
            let id = i as u32;
            let skipped = skip.partial.get(&id).cloned().unwrap_or_default();
            let ranges = expected.partial.entry(id).or_default();
            for (start, end) in skipped.missing(e.size) {
                ranges.insert(start, end);
            }
        }
        Self {
            count: m.entries.len(),
            expected,
            state: Mutex::new(State {
                chunks: BTreeMap::new(),
                bytes: 0,
                failed: false,
            }),
            changed: Condvar::new(),
        }
    }

    fn fail(&self) {
        let mut s = self.state.lock().unwrap();
        s.failed = true;
        self.changed.notify_all();
    }

    fn permitted(&self, id: u32, off: u64) -> bool {
        self.expected
            .partial
            .get(&id)
            .is_some_and(|r| r.covers(off, off.saturating_add(1)))
    }

    fn put(&self, id: u32, off: u64, data: &[u8]) -> io::Result<()> {
        // Ordered downloads synthesize empty-file writes before any data frame;
        // B's sender opens those files but reads zero bytes from them.
        if data.is_empty() && off == 0 && (id as usize) < self.count {
            return Ok(());
        }
        if id as usize >= self.count || !self.permitted(id, off) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected relay data {id}@{off}"),
            ));
        }
        if data.len() > RELAY_CAP {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "relay frame exceeds capacity",
            ));
        }
        let deadline = Instant::now() + RELAY_WAIT;
        let mut s = self.state.lock().unwrap();
        loop {
            if s.failed {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "relay stopped"));
            }
            if s.bytes + data.len() <= RELAY_CAP {
                if let Some(old) = s.chunks.insert((id, off), data.to_vec()) {
                    s.bytes -= old.len();
                }
                s.bytes += data.len();
                self.changed.notify_all();
                return Ok(());
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("relay put timed out at {id}@{off}"),
                ));
            }
            let (next, _) = self.changed.wait_timeout(s, deadline - now).unwrap();
            s = next;
        }
    }

    fn take(&self, id: u32, off: u64) -> io::Result<Vec<u8>> {
        if !self.permitted(id, off) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("relay read outside expected set at {id}@{off}"),
            ));
        }
        let deadline = Instant::now() + RELAY_WAIT;
        let mut s = self.state.lock().unwrap();
        loop {
            if let Some((&(key_id, start), _)) = s.chunks.range(..=(id, off)).next_back() {
                if key_id == id && s.chunks[&(key_id, start)].len() as u64 > off - start {
                    let data = s.chunks.remove(&(key_id, start)).unwrap();
                    s.bytes -= data.len();
                    self.changed.notify_all();
                    return if off == start {
                        Ok(data)
                    } else {
                        Ok(data[(off - start) as usize..].to_vec())
                    };
                }
            }
            if s.failed {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    format!("relay ended before {id}@{off}"),
                ));
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("relay take timed out at {id}@{off}"),
                ));
            }
            let (next, _) = self.changed.wait_timeout(s, deadline - now).unwrap();
            s = next;
        }
    }
}

struct RelaySink(Arc<Relay>);
impl Sink for RelaySink {
    fn prepare(&self, _m: &Manifest) -> io::Result<()> {
        Ok(())
    }
    fn write_at(&self, id: u32, off: u64, data: &[u8]) -> io::Result<()> {
        self.0.put(id, off, data)
    }
    fn write_whole(&self, id: u32, data: &[u8]) -> io::Result<()> {
        self.0.put(id, 0, data)
    }
    // B's journal is the durability authority. Losing this buffer only costs a reread.
    fn sync(&self, _ids: &[u32]) -> io::Result<()> {
        Ok(())
    }
    fn read_at(&self, _id: u32, _off: u64, _buf: &mut [u8]) -> io::Result<usize> {
        Ok(0)
    }
    fn transient_relay(&self) -> bool {
        true
    }
    fn commit(&self, _id: u32) -> io::Result<()> {
        Ok(())
    }
    fn finish(&self) -> io::Result<()> {
        Ok(())
    }
}

struct RelayReader {
    relay: Arc<Relay>,
    id: u32,
    pending: Vec<u8>,
    pending_off: u64,
    turn: Arc<Turn>,
}
struct Turn {
    ids: Vec<u32>,
    next: Mutex<usize>,
    changed: Condvar,
}
impl Turn {
    fn enter(&self, id: u32, relay: &Relay) -> io::Result<()> {
        let deadline = Instant::now() + RELAY_WAIT;
        let mut next = self.next.lock().unwrap();
        while self.ids.get(*next) != Some(&id) {
            if relay.state.lock().unwrap().failed {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "relay stopped"));
            }
            if *next >= self.ids.len() || self.ids[*next] > id {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("relay read order passed file {id}"),
                ));
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("relay read order timed out at file {id}"),
                ));
            }
            let (n, _) = self
                .changed
                .wait_timeout(next, (deadline - now).min(Duration::from_millis(100)))
                .unwrap();
            next = n;
        }
        Ok(())
    }
    fn leave(&self, id: u32) {
        let mut next = self.next.lock().unwrap();
        if self.ids.get(*next) == Some(&id) {
            *next += 1;
            self.changed.notify_all();
        }
    }
}
impl Drop for RelayReader {
    fn drop(&mut self) {
        self.turn.leave(self.id);
    }
}
impl ReadAt for RelayReader {
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if off < self.pending_off || off >= self.pending_off + self.pending.len() as u64 {
            self.pending = self.relay.take(self.id, off)?;
            self.pending_off = off;
        }
        if self.pending.is_empty() {
            return Ok(0);
        }
        let start = (off - self.pending_off) as usize;
        let n = buf.len().min(self.pending.len() - start);
        buf[..n].copy_from_slice(&self.pending[start..start + n]);
        Ok(n)
    }
}

struct RelaySource {
    relay: Arc<Relay>,
    manifest: Arc<Manifest>,
    ids: HashMap<String, u32>,
    turn: Arc<Turn>,
}
impl RelaySource {
    fn new(relay: Arc<Relay>, manifest: Arc<Manifest>, b_need: &Need) -> Self {
        let ids = manifest
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.path.clone(), i as u32))
            .collect();
        let turns = manifest
            .entries
            .iter()
            .enumerate()
            .filter(|(i, e)| e.kind == gen::ENTRY_FILE && !b_need.done.contains(&(*i as u32)))
            .map(|(i, _)| i as u32)
            .collect();
        Self {
            relay,
            manifest,
            ids,
            turn: Arc::new(Turn {
                ids: turns,
                next: Mutex::new(0),
                changed: Condvar::new(),
            }),
        }
    }
}
impl Source for RelaySource {
    fn open(&self, rel: &str) -> io::Result<Box<dyn ReadAt>> {
        let id = *self
            .ids
            .get(rel)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, rel.to_owned()))?;
        self.turn.enter(id, &self.relay)?;
        Ok(Box::new(RelayReader {
            relay: self.relay.clone(),
            id,
            pending: Vec::new(),
            pending_off: 0,
            turn: self.turn.clone(),
        }))
    }
    fn list(&self, _rel: &str) -> io::Result<Vec<(String, SourceMeta)>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "relay manifest is already known",
        ))
    }
    fn stat(&self, rel: &str) -> io::Result<SourceMeta> {
        let id = *self
            .ids
            .get(rel)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, rel.to_owned()))?;
        let e = self
            .manifest
            .entry(id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, rel.to_owned()))?;
        Ok(SourceMeta {
            size: e.size,
            mtime: e.mtime,
            mode: e.mode,
            is_dir: e.kind == gen::ENTRY_DIR,
        })
    }
}

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn ps5_to_ps5(
    from: &str,
    src: &str,
    to: &str,
    dest: &str,
    job_id: [u8; 16],
    progress: Arc<Progress>,
    cancel: Arc<AtomicBool>,
) -> Result<SendReport> {
    ps5_to_ps5_between(
        pool(),
        from,
        src,
        pool(),
        to,
        dest,
        job_id,
        progress,
        cancel,
    )
}

/// Pool injection for local two-host tests and the same production relay body.
#[allow(clippy::too_many_arguments)]
pub fn ps5_to_ps5_between(
    from_pool: &Pool,
    from: &str,
    src: &str,
    to_pool: &Pool,
    to: &str,
    dest: &str,
    job_id: [u8; 16],
    progress: Arc<Progress>,
    cancel: Arc<AtomicBool>,
) -> Result<SendReport> {
    if std::ptr::eq(from_pool, to_pool) && from.split(':').next() == to.split(':').next() {
        return Err(anyhow!(
            "relay source and destination must be different consoles"
        ));
    }
    let hex = ava1::hex::encode(&job_id);
    let persist = to_pool.ava_dir().join("send").join(&hex);
    let scratch = from_pool.ava_dir().join("relay").join(&hex);
    let _scratch = Scratch(scratch.clone());
    crate::block_on(async {
        let mut backoff = Duration::from_millis(250);
        let (mut last_at, mut last_durable) = (Instant::now(), 0u64);
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(anyhow!("transfer_cancelled"));
            }
            // A's journal is never authoritative. Each attempt derives its map
            // anew from B's durable map and the sender's persisted outboards.
            match std::fs::remove_dir_all(&scratch) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            let durable = progress.bytes_durable.load(Ordering::Relaxed);
            if durable > last_durable {
                last_at = Instant::now();
                last_durable = durable;
            } else if last_at.elapsed() > STALL_LIMIT {
                return Err(anyhow!("no durable progress for {STALL_LIMIT:?}"));
            }
            let sa = match from_pool.session(from).await {
                Ok(s) => s,
                Err(e) => {
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(5));
                    eprintln!("ava1 relay: reconnecting source: {e}");
                    continue;
                }
            };
            let sb = match to_pool.session(to).await {
                Ok(s) => s,
                Err(e) => {
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(5));
                    eprintln!("ava1 relay: reconnecting destination: {e}");
                    continue;
                }
            };
            let (mut la, mut lb) = (sa.job(job_id), sb.job(job_id));
            let m = match download_open(&mut la, src, gen::JF_ORDERED, RELAY_CAP as u64).await {
                Ok(m) => m,
                Err(SendError::Disconnected(_)) => {
                    from_pool.forget(from).await;
                    tokio::time::sleep(backoff).await;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let mut o = SendOptions::upload(dest);
            o.progress = progress.clone();
            o.cancel = cancel.clone();
            o.persist = Some(persist.clone());
            // Must be one reader: parallel file reads can fill the cap with later files.
            o.readers = 1;
            let opened = match open_upload(&mut lb, &m, &o).await {
                Ok(opened) => opened,
                Err(SendError::Disconnected(_)) => {
                    to_pool.forget(to).await;
                    tokio::time::sleep(backoff).await;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let skip = ava1::send::skip_set(&m, &opened.1, Some(&persist));
            let relay = Arc::new(Relay::new(&m, &skip));
            let ro = RecvOptions {
                credit: RELAY_CAP as u64,
                flags: gen::JF_ORDERED,
                jobs_dir: scratch.clone(),
                ordered: true,
                progress: Arc::default(),
                cancel: cancel.clone(),
            };
            let a_relay = relay.clone();
            let a_m = m.clone();
            let mut a = tokio::spawn(async move {
                let result = download_run(
                    &mut la,
                    a_m,
                    Some(skip),
                    Arc::new(RelaySink(a_relay.clone())),
                    ro,
                )
                .await;
                a_relay.fail();
                result
            });
            let source = Arc::new(RelaySource::new(relay.clone(), m.clone(), &opened.1));
            let b = run_upload(&mut lb, m, source, o, opened).await;
            relay.fail();
            let a_result = match tokio::time::timeout(RELAY_WAIT, &mut a).await {
                Ok(r) => r.map_err(|e| anyhow!(e))?,
                Err(_) => {
                    a.abort();
                    return Err(anyhow!("source relay did not stop within {RELAY_WAIT:?}"));
                }
            };
            if matches!(a_result, Err(SendError::Disconnected(_))) {
                from_pool.forget(from).await;
                to_pool.forget(to).await;
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(5));
                continue;
            }
            match b {
                Ok(r) if r.status == gen::STATUS_OK => {
                    a_result?;
                    let _ = std::fs::remove_dir_all(&persist);
                    return Ok(r);
                }
                Ok(r) => {
                    return Err(anyhow!(
                        "console refused the relay ({}): {}",
                        r.status,
                        r.message.unwrap_or_default()
                    ))
                }
                Err(SendError::Disconnected(why)) => {
                    from_pool.forget(from).await;
                    to_pool.forget(to).await;
                    let _ = why;
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(5));
                }
                Err(SendError::Cancelled) => return Err(anyhow!("transfer_cancelled")),
                Err(e) => return Err(anyhow!(e)),
            }
        }
    })
}
