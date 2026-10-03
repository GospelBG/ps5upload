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

/// `host:9120` — the AVA1 default port. The lab can override an address on
/// its own Pool; a process environment variable cannot redirect engine jobs.
pub fn ava1_addr(console: &str) -> String {
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
    /// Job directories (hex job ids) of the jobs running in this process, with a count each:
    /// the journal sweep never touches them (SPEC.md §14.3).
    live: Mutex<HashMap<String, usize>>,
}

/// A job running in this process (see `Pool::live_job`).
pub struct LiveJob<'a> {
    pool: &'a Pool,
    name: String,
}

impl Drop for LiveJob<'_> {
    fn drop(&mut self) {
        let mut l = self.pool.live.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = l.get_mut(&self.name) {
            *n -= 1;
            if *n == 0 {
                l.remove(&self.name);
            }
        }
    }
}

/// SPEC.md §14.3: a job directory idle for more than this is removed.
pub const JOURNAL_MAX_AGE_S: u64 = 7 * 24 * 3600;
/// How often the engine sweeps (and once at start).
pub const JOURNAL_GC_EVERY: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

impl Pool {
    pub(crate) fn unavailable() -> Pool {
        Pool {
            dir: PathBuf::new(),
            me: Err("no PS5Upload data directory; AVA1 identity unavailable".into()),
            peers: Arc::new(Mutex::new(PeerStore::in_memory())),
            sessions: tokio::sync::Mutex::default(),
            addr: None,
            attempts: AtomicUsize::new(0),
            live: Mutex::default(),
        }
    }

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
            live: Mutex::default(),
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

    /// Marks the job with this id as running here until the guard drops: its directories
    /// under `jobs/` and `send/` are not swept meanwhile.
    pub fn live_job(&self, id: &[u8; 16]) -> LiveJob<'_> {
        let name = ava1::hex::encode(id);
        *self
            .live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(name.clone())
            .or_insert(0) += 1;
        LiveJob { pool: self, name }
    }

    /// SPEC.md §14.3: removes job directories under `<ava dir>/jobs` (receiver journals) and
    /// `<ava dir>/send` (sender outboards) idle for more than `max_age_s` as of `now_unix`,
    /// except jobs running in this process. Returns how many were removed. Blocking file I/O:
    /// call it off the async runtime.
    pub fn gc_journals(&self, now_unix: u64, max_age_s: u64) -> usize {
        if self.dir.as_os_str().is_empty() {
            return 0; // an unavailable pool has no directory
        }
        let live = |name: &str| {
            self.live
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(name)
        };
        ["jobs", "send"]
            .iter()
            .map(|sub| {
                ava1::journal::gc_except(&self.dir.join(sub), now_unix, max_age_s, &live)
                    .unwrap_or(0)
            })
            .sum()
    }

    pub fn has_identity(&self) -> bool {
        self.me.is_ok()
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

/// `<data dir>/ava` — the same rules as the engine's `remote::store::data_dir()`.
/// Without a data directory no identity is created in the current directory.
fn data_dir() -> Option<PathBuf> {
    let data = std::env::var("PS5UPLOAD_DATA_DIR").ok();
    let home = std::env::var("HOME").ok();
    let profile = std::env::var("USERPROFILE").ok();
    data_dir_from(data.as_deref(), home.as_deref(), profile.as_deref())
}

fn data_dir_from(data: Option<&str>, home: Option<&str>, profile: Option<&str>) -> Option<PathBuf> {
    data.filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            home.filter(|v| !v.trim().is_empty())
                .or_else(|| profile.filter(|v| !v.trim().is_empty()))
                .map(|v| PathBuf::from(v).join(".ps5upload"))
        })
}

/// The process's pool (identity, peers and pins under `<data dir>/ava`).
pub fn pool() -> &'static Pool {
    static P: OnceLock<Pool> = OnceLock::new();
    P.get_or_init(|| match data_dir() {
        Some(dir) => Pool::new(dir.join("ava")),
        None => {
            // Once per process: this initialiser runs once. Never `eprintln!` (a
            // closed stderr panics it).
            use std::io::Write;
            let _ = writeln!(
                std::io::stderr(),
                "ava1: no PS5Upload data directory (set PS5UPLOAD_DATA_DIR or HOME); using FTX2"
            );
            Pool::unavailable()
        }
    })
}

#[cfg(test)]
mod data_dir_tests {
    use super::data_dir_from;

    #[test]
    fn no_data_directory_never_falls_back_to_the_current_directory() {
        assert_eq!(data_dir_from(None, None, None), None);
        assert_eq!(data_dir_from(Some(" "), Some(""), None), None);
    }
}

#[cfg(test)]
mod gc_tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ps5u-poolgc-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn age(dir: &Path, secs: u64) {
        let t = std::time::SystemTime::now() - std::time::Duration::from_secs(secs);
        for p in [dir.to_path_buf(), dir.join("journal")] {
            if let Ok(f) = std::fs::File::open(&p) {
                f.set_modified(t).unwrap();
            }
        }
    }

    #[test]
    fn gc_sweeps_jobs_and_send_after_seven_days_but_never_a_live_job() {
        let base = tmp("sweep");
        let pool = Pool::new(base.join("ava"));
        let d = pool.ava_dir().to_path_buf();
        let old_job = [1u8; 16];
        let live_job = [2u8; 16];
        let fresh_job = [3u8; 16];
        for sub in ["jobs", "send"] {
            for id in [&old_job, &live_job, &fresh_job] {
                let p = d.join(sub).join(ava1::hex::encode(id));
                std::fs::create_dir_all(&p).unwrap();
                std::fs::write(p.join("journal"), b"x").unwrap();
            }
        }
        let eight_days = 8 * 24 * 3600;
        for sub in ["jobs", "send"] {
            for id in [&old_job, &live_job] {
                age(&d.join(sub).join(ava1::hex::encode(id)), eight_days);
            }
        }
        let guard = pool.live_job(&live_job);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(
            pool.gc_journals(now, JOURNAL_MAX_AGE_S),
            2,
            "one per directory"
        );
        for sub in ["jobs", "send"] {
            assert!(!d.join(sub).join(ava1::hex::encode(&old_job)).exists());
            assert!(d.join(sub).join(ava1::hex::encode(&live_job)).exists());
            assert!(d.join(sub).join(ava1::hex::encode(&fresh_job)).exists());
        }
        drop(guard);
        assert_eq!(
            pool.gc_journals(now, JOURNAL_MAX_AGE_S),
            2,
            "the finished job expires too"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
