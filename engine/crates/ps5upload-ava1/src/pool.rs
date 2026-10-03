//! One AVA1 session per console, shared by every job (SPEC.md §6 limits a peer to 12
//! connections per IP; one control connection plus at most 8 lanes is one session).

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use ava1::keys::Identity;
use ava1::peers::PeerStore;
use ava1::session::{connect_expecting, Session, Timing};
use ava1::Ava1Error;

/// `host:9120` — the AVA1 default port — or the whole address from `AVA1_ADDR` when
/// it is set. That override is for the lab/chaos runs only: a single-console,
/// whole-process override. The engine never honours it (A1) — a process-global
/// address would silently redirect every console's transfer the moment two consoles
/// are used.
pub fn ava1_addr(console: &str) -> String {
    if let Ok(a) = std::env::var("AVA1_ADDR") {
        if !a.trim().is_empty() {
            return a;
        }
    }
    format!("{}:{}", host_of(console), ava1::gen::DEFAULT_PORT)
}

/// The console string without its port (the pool's key).
fn host_of(console: &str) -> String {
    console
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(console)
        .to_string()
}

pub struct Pool {
    dir: PathBuf,
    /// No identity is an error, not a panic (C4): `session()` turns the reason into an
    /// `Ava1Error::Io` — "not paired" would misdescribe a missing identity file, and
    /// both fall back to FTX2 under Auto, so the honest error wins.
    me: Result<Arc<Identity>, String>,
    peers: Arc<Mutex<PeerStore>>,
    sessions: tokio::sync::Mutex<HashMap<String, Arc<Session>>>,
    /// Test/lab override: every console resolves to this address (A1). Never set in
    /// the engine.
    addr: Option<String>,
    /// Connection attempts so far. A test seam (A4): the negative cache's hit path is
    /// pinned by counting, never by sleeping.
    attempts: AtomicUsize,
}

impl Pool {
    pub fn new(dir: PathBuf) -> Pool {
        let _ = std::fs::create_dir_all(&dir);
        let me = Identity::load_or_create(&dir.join("identity"))
            .map(Arc::new)
            .map_err(|e| {
                format!(
                    "no AVA1 identity at {}: {e}",
                    dir.join("identity").display()
                )
            });
        // C3: an unreadable peers file is kept as the fact (every write refuses and
        // `unreadable()` says why), never silently replaced by an empty store.
        let peers = PeerStore::load_or_unreadable(&dir.join("peers"));
        Pool {
            dir,
            me,
            peers: Arc::new(Mutex::new(peers)),
            sessions: tokio::sync::Mutex::default(),
            addr: None,
            attempts: AtomicUsize::new(0),
        }
    }

    /// Test/lab override: every console resolves to this address. Never set in the
    /// engine (A1).
    pub fn with_addr(mut self, addr: impl Into<String>) -> Pool {
        self.addr = Some(addr.into());
        self
    }

    pub fn ava_dir(&self) -> &Path {
        &self.dir
    }

    /// The address `session()` connects to: the per-pool override first (A1: the
    /// engine's pools never carry one), then the console's own address.
    fn addr_for(&self, console: &str) -> String {
        match &self.addr {
            Some(a) => a.clone(),
            None => ava1_addr(console),
        }
    }

    /// The key pinned for this console's address, if one was recorded. A *changed* key
    /// at a known address is refused by `connect_expecting` (`WrongPeer`), never
    /// re-pinned silently — the identity pinning's whole point.
    fn pinned(&self, host: &str) -> Option<[u8; 32]> {
        let text = std::fs::read_to_string(self.dir.join("consoles")).ok()?;
        for line in text.lines() {
            // Tolerant parser: unattributable lines are skipped.
            let Some((h, k)) = line.split_once(' ') else {
                continue;
            };
            if h != host {
                continue;
            }
            if let Some(k) = ava1::hex::decode(k.trim()) {
                if let Ok(k) = k.try_into() {
                    return Some(k);
                }
            }
        }
        None
    }

    /// Records `host → key`, first pin only: a known host's line is replaced with the
    /// same key, never re-pinned to a different one (a changed key there is refused by
    /// `connect_expecting`, and this only runs after that passed). Written through a
    /// temp file + rename, the same shape the peer store uses.
    fn pin(&self, host: &str, key: [u8; 32]) {
        let p = self.dir.join("consoles");
        let mut lines: Vec<String> = std::fs::read_to_string(&p)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.split_once(' ').map(|(h, _)| h) != Some(host))
            .map(String::from)
            .collect();
        lines.push(format!("{host} {}", ava1::hex::encode(&key)));
        let tmp = self.dir.join("consoles.tmp");
        if std::fs::write(&tmp, lines.join("\n") + "\n").is_ok() {
            let _ = std::fs::rename(tmp, p);
        }
    }

    /// One live session for the console, connecting when there is none. C17: the lock
    /// is never held across the handshake (up to `Timing::handshake`) — one
    /// unreachable console must not stall every other console's transfer. If another
    /// caller won the race, theirs is kept and the loser closed: SPEC §6 caps a peer
    /// at 12 connections per IP, so a leaked session is not free.
    pub async fn session(&self, console: &str) -> Result<Arc<Session>, Ava1Error> {
        let host = host_of(console);
        {
            let mut map = self.sessions.lock().await;
            if let Some(s) = map.get(&host) {
                if s.is_closed() {
                    map.remove(&host);
                } else {
                    return Ok(s.clone());
                }
            }
        }
        // C4: no identity is an `Io` error, not `NotPaired` — the latter's message
        // ("the devices are not paired yet") would misdescribe a missing identity
        // file; both fall back to FTX2 under Auto, so the honest error wins.
        let me = self
            .me
            .clone()
            .map_err(|why| Ava1Error::Io(io::Error::other(why)))?;
        let pin = self.pinned(&host);
        self.attempts.fetch_add(1, Ordering::Relaxed);
        let mut s = connect_expecting(
            &self.addr_for(console),
            pin,
            me,
            self.peers.clone(),
            "ps5upload",
            Timing::default(),
        )
        .await?;
        if s.needs_user_pairing() {
            // A person must compare codes (SPEC.md §5), not a transfer.
            return Err(Ava1Error::NotPaired);
        }
        if s.pairing_code().is_some() {
            // The console already trusts us (it was launched by us): record its key.
            s.confirm_pairing().await?;
        }
        if pin.is_none() {
            self.pin(&host, s.peer_key());
        }
        let s = Arc::new(s);
        let mut map = self.sessions.lock().await;
        let kept = match map.get(&host) {
            Some(kept) if !kept.is_closed() => Some(kept.clone()),
            _ => None,
        };
        if let Some(kept) = kept {
            drop(map);
            if let Some(loser) = Arc::into_inner(s) {
                loser.close().await;
            }
            Ok(kept)
        } else {
            map.insert(host, s.clone());
            Ok(s)
        }
    }

    /// Drops the cached session (async: it takes the sessions lock — C2). Whether to
    /// `close()` the session is the caller's decision.
    pub async fn forget(&self, console: &str) {
        self.sessions.lock().await.remove(&host_of(console));
    }

    /// Connection attempts so far (test seam, A4).
    pub fn attempts(&self) -> usize {
        self.attempts.load(Ordering::Relaxed)
    }
}

/// `<data dir>/ava` — the same rules as the engine's `remote::store::data_dir()`
/// (which returns `Option` and has no CWD fallback; this crate needs a directory, so
/// it keeps the plan's last resort).
fn data_dir() -> PathBuf {
    if let Ok(v) = std::env::var("PS5UPLOAD_DATA_DIR") {
        if !v.trim().is_empty() {
            return PathBuf::from(v);
        }
    }
    if let Ok(h) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        if !h.trim().is_empty() {
            return PathBuf::from(h).join(".ps5upload");
        }
    }
    PathBuf::from(".ps5upload")
}

/// The process's pool (identity, peers and pins under `<data dir>/ava`).
pub fn pool() -> &'static Pool {
    static P: OnceLock<Pool> = OnceLock::new();
    P.get_or_init(|| Pool::new(data_dir().join("ava")))
}
