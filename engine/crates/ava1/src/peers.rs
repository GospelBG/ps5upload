//! Paired devices, one per line: `<64 hex key> <unix seconds> <name>` (SPEC.md §5).
use std::io;
use std::path::{Path, PathBuf};

pub const MAX_PEERS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub key: [u8; 32],
    pub added_unix: u64,
    pub name: String,
}

#[derive(Debug, Default)]
pub struct PeerStore {
    path: Option<PathBuf>,
    peers: Vec<Peer>,
}

impl PeerStore {
    /// Not saved anywhere (tests, one-off tools).
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// A missing file is an empty store; unreadable lines are skipped, never fatal.
    pub fn load(path: &Path) -> io::Result<Self> {
        let peers = match std::fs::read(path) {
            Ok(b) => String::from_utf8_lossy(&b)
                .lines()
                .filter_map(parse_line)
                .take(MAX_PEERS)
                .collect(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        Ok(Self {
            path: Some(path.to_path_buf()),
            peers,
        })
    }

    pub fn contains(&self, key: &[u8; 32]) -> bool {
        self.peers.iter().any(|p| &p.key == key)
    }

    pub fn list(&self) -> &[Peer] {
        &self.peers
    }

    /// Adds or replaces `key`; drops the oldest peer past `MAX_PEERS`; saves.
    pub fn add(&mut self, key: [u8; 32], name: &str) -> io::Result<()> {
        self.peers.retain(|p| p.key != key);
        if self.peers.len() >= MAX_PEERS {
            self.peers.remove(0);
        }
        let added_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.peers.push(Peer {
            key,
            added_unix,
            name: clean_name(name),
        });
        self.save()
    }

    pub fn remove(&mut self, key: &[u8; 32]) -> io::Result<bool> {
        let before = self.peers.len();
        self.peers.retain(|p| &p.key != key);
        if self.peers.len() == before {
            return Ok(false);
        }
        self.save()?;
        Ok(true)
    }

    fn save(&self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text: String = self
            .peers
            .iter()
            .map(|p| {
                format!(
                    "{} {} {}\n",
                    crate::hex::encode(&p.key),
                    p.added_unix,
                    p.name
                )
            })
            .collect();
        let tmp = path.with_extension("tmp");
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)
    }
}

/// One line, no control characters, at most 63 bytes on a char boundary.
fn clean_name(n: &str) -> String {
    let mut s = String::new();
    for c in n.chars().map(|c| if c.is_control() { ' ' } else { c }) {
        if s.len() + c.len_utf8() > 63 {
            break;
        }
        s.push(c);
    }
    s
}

fn parse_line(l: &str) -> Option<Peer> {
    let mut it = l.splitn(3, ' ');
    let key: [u8; 32] = crate::hex::decode(it.next()?)?.try_into().ok()?;
    let added_unix = it.next()?.parse().ok()?;
    let name = it.next().unwrap_or("").to_string();
    Some(Peer {
        key,
        added_unix,
        name,
    })
}
