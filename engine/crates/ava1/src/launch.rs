//! Launch tokens (SPEC.md §5.2): whoever stamps the helper ELF puts a fresh random token
//! in its trust slot and keeps it here. The launched helper proves it holds the token —
//! bound to one handshake — in its Welcome, so the stamping side trusts that console
//! without a pairing code. The token itself never travels after the launch.
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use blake2::digest::consts::U32;
use blake2::digest::{KeyInit, Mac};
use blake2::Blake2bMac;

pub const TOKEN_LEN: usize = 16;
/// Tokens kept; issuing past this drops the oldest.
pub const MAX_TOKENS: usize = 32;
/// A token is accepted for this long after it was issued.
pub const TOKEN_TTL_S: u64 = 24 * 3600;
/// A token "issued" further in the future than this (a clock that moved back) is
/// treated as expired, not as live for longer.
const FUTURE_SLACK_S: u64 = 300;

/// The launch proof: the first 16 bytes of keyed BLAKE2b-256(key = token ‖ 16 zero
/// bytes, "AVA1 launch" ‖ h), h being the handshake hash. A fresh handshake has a fresh
/// h, so an old proof never verifies again.
pub fn proof(token: &[u8; TOKEN_LEN], h: &[u8; 64]) -> [u8; 16] {
    let mut key = [0u8; 32];
    key[..TOKEN_LEN].copy_from_slice(token);
    let mut m = <Blake2bMac<U32> as KeyInit>::new_from_slice(&key).expect("32-byte key");
    zeroize::Zeroize::zeroize(&mut key);
    Mac::update(&mut m, b"AVA1 launch");
    Mac::update(&mut m, h);
    let d = m.finalize().into_bytes();
    let mut o = [0u8; 16];
    o.copy_from_slice(&d[..16]);
    o
}

/// What a server stamped with a launch token proves to (SPEC.md §5.2): the client whose
/// static key is `key`, the trust slot's key.
#[derive(Clone)]
pub struct LaunchSecret {
    pub key: [u8; 32],
    pub token: [u8; TOKEN_LEN],
}

impl std::fmt::Debug for LaunchSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchSecret")
            .field("key", &crate::hex::encode(&self.key))
            .finish_non_exhaustive()
    }
}

impl Drop for LaunchSecret {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.token);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Issued {
    token: [u8; TOKEN_LEN],
    unix: u64,
}

/// The tokens this side issued, one per line: `<32 hex token> <unix seconds>`, file
/// mode 0600, written atomically. Read on every use, so a token another process issued
/// (the desktop app's send, through the engine) counts at once.
#[derive(Default)]
pub struct LaunchTokens {
    path: Option<PathBuf>,
    /// The tokens, when there is no file (tests); and the lock every change takes.
    mem: Mutex<Vec<Issued>>,
}

impl std::fmt::Debug for LaunchTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchTokens")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn live_at(i: &Issued, now: u64) -> bool {
    i.unix <= now.saturating_add(FUTURE_SLACK_S) && now < i.unix.saturating_add(TOKEN_TTL_S)
}

fn parse_line(l: &str) -> Option<Issued> {
    let mut it = l.split_whitespace();
    let token = crate::hex::decode(it.next()?)?.try_into().ok()?;
    let unix = it.next()?.parse().ok()?;
    Some(Issued { token, unix })
}

impl LaunchTokens {
    /// Kept in `path` (`<data dir>/ava/launch_tokens`).
    pub fn at(path: &Path) -> Self {
        Self {
            path: Some(path.to_path_buf()),
            mem: Mutex::default(),
        }
    }

    /// Not saved anywhere (tests, one-off tools).
    pub fn in_memory() -> Self {
        Self::default()
    }

    fn load(&self, mem: &[Issued]) -> io::Result<Vec<Issued>> {
        let Some(path) = &self.path else {
            return Ok(mem.to_vec());
        };
        match std::fs::read(path) {
            Ok(b) => Ok(String::from_utf8_lossy(&b)
                .lines()
                .filter_map(parse_line)
                .collect()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    fn save(&self, mem: &mut Vec<Issued>, all: Vec<Issued>) -> io::Result<()> {
        let Some(path) = &self.path else {
            *mem = all;
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text: String = all
            .iter()
            .map(|i| format!("{} {}\n", crate::hex::encode(&i.token), i.unix))
            .collect();
        let tmp = path.with_extension("tmp");
        {
            use std::io::Write;
            let mut o = std::fs::OpenOptions::new();
            o.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                o.mode(0o600);
            }
            let mut f = o.open(&tmp)?;
            // A file left by an older build may have looser bits: `mode` only applies
            // to a file it creates.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)
    }

    /// A fresh random token, recorded before it is returned: a token that could not be
    /// kept is never stamped.
    pub fn issue(&self) -> io::Result<[u8; TOKEN_LEN]> {
        let token = crate::keys::random_bytes()?;
        self.record(token, now_unix())?;
        Ok(token)
    }

    /// Records `token` as issued at `unix`; drops expired tokens and, past `MAX_TOKENS`,
    /// the oldest.
    pub fn record(&self, token: [u8; TOKEN_LEN], unix: u64) -> io::Result<()> {
        let mut mem = self.mem.lock().unwrap_or_else(|e| e.into_inner());
        let now = now_unix();
        let mut all: Vec<Issued> = self
            .load(&mem)?
            .into_iter()
            .filter(|i| live_at(i, now) && i.token != token)
            .collect();
        all.push(Issued { token, unix });
        all.sort_by_key(|i| i.unix);
        let excess = all.len().saturating_sub(MAX_TOKENS);
        all.drain(..excess);
        self.save(&mut mem, all)
    }

    /// How many unexpired tokens there are.
    pub fn live(&self) -> usize {
        let mem = self.mem.lock().unwrap_or_else(|e| e.into_inner());
        let now = now_unix();
        self.load(&mem)
            .map(|v| v.iter().filter(|i| live_at(i, now)).count())
            .unwrap_or(0)
    }

    /// Whether `proof` (from a Welcome on handshake `h`) was made with one of the
    /// unexpired tokens. An unreadable file recognises nothing: the session pairs the
    /// usual way.
    pub fn recognises(&self, h: &[u8; 64], proof_: &[u8; 16]) -> bool {
        let mem = self.mem.lock().unwrap_or_else(|e| e.into_inner());
        let now = now_unix();
        let Ok(all) = self.load(&mem) else {
            return false;
        };
        // Every live token is tried (no early exit), so the time taken says nothing
        // about which one matched.
        all.iter()
            .filter(|i| live_at(i, now))
            .fold(false, |hit, i| {
                crate::keys::ct_eq16(&proof(&i.token, h), proof_) | hit
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h16(s: &str) -> [u8; 16] {
        crate::hex::decode(s).unwrap().try_into().unwrap()
    }

    #[test]
    fn the_proof_vectors_reproduce() {
        let mut n = 0;
        for l in include_str!("../../../../protocol/ava1/vectors/launch.txt").lines() {
            if l.starts_with('#') || l.trim().is_empty() {
                continue;
            }
            let f: Vec<&str> = l.split_whitespace().collect();
            let h: [u8; 64] = crate::hex::decode(f[1]).unwrap().try_into().unwrap();
            assert_eq!(proof(&h16(f[0]), &h), h16(f[2]), "{l}");
            n += 1;
        }
        assert!(n >= 2);
    }

    #[test]
    fn a_proof_is_bound_to_the_token_and_the_handshake() {
        let (t, h) = ([1u8; 16], [2u8; 64]);
        let p = proof(&t, &h);
        let tokens = LaunchTokens::in_memory();
        assert!(!tokens.recognises(&h, &p), "no token issued yet");
        tokens.record(t, now_unix()).unwrap();
        assert!(tokens.recognises(&h, &p));
        assert!(!tokens.recognises(&[3; 64], &p), "another handshake");
        let mut tampered = p;
        tampered[0] ^= 1;
        assert!(!tokens.recognises(&h, &tampered));
        assert!(
            !tokens.recognises(&h, &proof(&[9; 16], &h)),
            "another token"
        );
    }

    #[test]
    fn tokens_expire_after_a_day_and_the_oldest_go_past_the_cap() {
        let tokens = LaunchTokens::in_memory();
        let h = [5u8; 64];
        let now = now_unix();
        tokens.record([1; 16], now - TOKEN_TTL_S - 1).unwrap();
        assert!(!tokens.recognises(&h, &proof(&[1; 16], &h)), "expired");
        tokens.record([2; 16], now + 10 * TOKEN_TTL_S).unwrap();
        assert!(
            !tokens.recognises(&h, &proof(&[2; 16], &h)),
            "from the future"
        );
        for i in 0..MAX_TOKENS as u8 + 3 {
            tokens
                .record([0x40 + i; 16], now - 100 + u64::from(i))
                .unwrap();
        }
        assert_eq!(tokens.live(), MAX_TOKENS);
        assert!(
            !tokens.recognises(&h, &proof(&[0x40; 16], &h)),
            "oldest dropped"
        );
        assert!(tokens.recognises(&h, &proof(&[0x40 + MAX_TOKENS as u8 + 2; 16], &h)));
    }

    #[test]
    fn issued_tokens_are_fresh_and_persist_in_a_private_file() {
        let d = std::env::temp_dir().join(format!("ava1-launch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let path = d.join("ava").join("launch_tokens");
        let a = LaunchTokens::at(&path);
        let (t1, t2) = (a.issue().unwrap(), a.issue().unwrap());
        assert_ne!(t1, t2);
        // Another process (a second store on the same file) sees both.
        let b = LaunchTokens::at(&path);
        let h = [7u8; 64];
        assert!(b.recognises(&h, &proof(&t1, &h)) && b.recognises(&h, &proof(&t2, &h)));
        assert_eq!(b.live(), 2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // A garbled line is skipped, not fatal.
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.insert_str(0, "not a token\n");
        std::fs::write(&path, text).unwrap();
        assert_eq!(b.live(), 2);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn debug_output_never_shows_a_token() {
        let s = LaunchSecret {
            key: [1; 32],
            token: [0xab; 16],
        };
        assert!(!format!("{s:?}").contains("abab"));
        let t = LaunchTokens::in_memory();
        t.record([0xab; 16], now_unix()).unwrap();
        assert!(!format!("{t:?}").contains("abab"));
    }
}
