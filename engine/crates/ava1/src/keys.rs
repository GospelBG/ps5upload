//! Identities, the Noise XX handshake, and sealed frames (SPEC.md §4).

use std::io;
use std::path::Path;

use blake2::digest::consts::{U16, U32};
use blake2::digest::{KeyInit, Mac};
use blake2::{Blake2b, Blake2bMac, Digest};
use chacha20poly1305::aead::AeadInPlace;
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce, Tag};
use x25519_dalek::{PublicKey, StaticSecret};

pub const NOISE: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2b";
pub const PROLOGUE: &[u8] = b"AVA1 v1";
pub const MAC_LEN: usize = 16;

/// A node's long-lived X25519 key pair (project 2 adds an Ed25519 key for tickets).
pub struct Identity {
    secret: [u8; 32],
    public: [u8; 32],
}

impl Drop for Identity {
    fn drop(&mut self) {
        self.secret = [0; 32];
    }
}

impl Identity {
    pub fn from_secret(secret: [u8; 32]) -> Self {
        let public = PublicKey::from(&StaticSecret::from(secret)).to_bytes();
        Self { secret, public }
    }

    pub fn generate() -> io::Result<Self> {
        Ok(Self::from_secret(random_bytes::<32>()?))
    }

    pub fn public(&self) -> [u8; 32] {
        self.public
    }

    /// Reads a 32-byte secret, creating it (mode 0600) when the file is missing. A file of
    /// the wrong size is an error: replacing it would silently unpair every device.
    pub fn load_or_create(path: &Path) -> io::Result<Self> {
        match std::fs::read(path) {
            Ok(b) => {
                let a: [u8; 32] = b.as_slice().try_into().map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{} holds {} bytes, expected 32", path.display(), b.len()),
                    )
                })?;
                return Ok(Self::from_secret(a));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let bytes = random_bytes::<32>()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        write_private(&tmp, &bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok(Self::from_secret(bytes))
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let mut f = o.open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

pub fn random_bytes<const N: usize>() -> io::Result<[u8; N]> {
    let mut b = [0u8; N];
    getrandom::fill(&mut b)
        .map_err(|e| io::Error::other(format!("no secure random source: {e}")))?;
    Ok(b)
}

/// Keys of an established session. The client is always the Noise initiator.
#[derive(Clone)]
pub struct SessionKeys {
    pub c2s: [u8; 32],
    pub s2c: [u8; 32],
    /// Noise handshake hash: binds the session (pairing code, joins).
    pub hash: [u8; 64],
}

/// One side of a `Noise_XX_25519_ChaChaPoly_BLAKE2b` handshake.
pub struct Handshake {
    hs: snow::HandshakeState,
}

impl Handshake {
    pub fn initiator(me: &Identity) -> Result<Self, snow::Error> {
        Self::build(me, true, None, PROLOGUE)
    }

    pub fn responder(me: &Identity) -> Result<Self, snow::Error> {
        Self::build(me, false, None, PROLOGUE)
    }

    /// `eph` fixes the ephemeral key: for the published test vector only.
    #[doc(hidden)]
    pub fn build(
        me: &Identity,
        initiator: bool,
        eph: Option<&[u8; 32]>,
        prologue: &[u8],
    ) -> Result<Self, snow::Error> {
        let mut b = snow::Builder::new(NOISE.parse()?)
            .local_private_key(&me.secret)?
            .prologue(prologue)?;
        if let Some(e) = eph {
            b = b.fixed_ephemeral_key_for_testing_only(e);
        }
        let hs = if initiator {
            b.build_initiator()?
        } else {
            b.build_responder()?
        };
        Ok(Self { hs })
    }

    pub fn write(&mut self, payload: &[u8]) -> Result<Vec<u8>, snow::Error> {
        let mut out = vec![0u8; payload.len() + 128];
        let n = self.hs.write_message(payload, &mut out)?;
        out.truncate(n);
        Ok(out)
    }

    pub fn read(&mut self, msg: &[u8]) -> Result<Vec<u8>, snow::Error> {
        let mut out = vec![0u8; msg.len()];
        let n = self.hs.read_message(msg, &mut out)?;
        out.truncate(n);
        Ok(out)
    }

    pub fn remote_static(&self) -> Option<[u8; 32]> {
        self.hs.get_remote_static()?.try_into().ok()
    }

    /// After message 3: the split keys (initiator → responder first) and the handshake hash.
    pub fn finish(mut self) -> SessionKeys {
        let mut hash = [0u8; 64];
        hash.copy_from_slice(self.hs.get_handshake_hash());
        let (c2s, s2c) = self.hs.dangerously_get_raw_split();
        SessionKeys { c2s, s2c, hash }
    }
}

fn mac32(key: &[u8; 32], parts: &[&[u8]]) -> [u8; 32] {
    let mut m = <Blake2bMac<U32> as KeyInit>::new_from_slice(key).expect("32-byte key");
    for p in parts {
        Mac::update(&mut m, p);
    }
    let mut o = [0u8; 32];
    o.copy_from_slice(&m.finalize().into_bytes());
    o
}

fn mac16(key: &[u8; 32], parts: &[&[u8]]) -> [u8; 16] {
    let mut m = <Blake2bMac<U16> as KeyInit>::new_from_slice(key).expect("32-byte key");
    for p in parts {
        Mac::update(&mut m, p);
    }
    let mut o = [0u8; 16];
    o.copy_from_slice(&m.finalize().into_bytes());
    o
}

/// The key a lane uses in one direction: keyed BLAKE2b-256(dir, "AVA1 lane" ‖ u16le(lane)).
pub fn lane_key(dir: &[u8; 32], lane: u16) -> [u8; 32] {
    mac32(dir, &[b"AVA1 lane", &lane.to_le_bytes()])
}

/// Join proofs: keyed BLAKE2b-128(BLAKE2b-256(dir, "AVA1 join"), label ‖ sid ‖ u16le(lane) ‖ nonce).
pub fn join_tag(
    dir: &[u8; 32],
    label: &[u8],
    sid: &[u8; 16],
    lane: u16,
    nonce: &[u8; 16],
) -> [u8; 16] {
    let jk = mac32(dir, &[b"AVA1 join"]);
    mac16(&jk, &[label, sid, &lane.to_le_bytes(), nonce])
}

/// The six-digit code both devices show while pairing, bound to this handshake.
pub fn pairing_code(hash: &[u8; 64]) -> u32 {
    let d = Blake2b::<U32>::new()
        .chain_update(b"AVA1 pairing")
        .chain_update(hash)
        .finalize();
    u32::from_le_bytes([d[0], d[1], d[2], d[3]]) % 1_000_000
}

fn nonce(n: u64) -> [u8; 12] {
    let mut b = [0u8; 12];
    b[4..].copy_from_slice(&n.to_le_bytes());
    b
}

/// ChaCha20-Poly1305 in place; appends the 16-byte MAC.
pub fn seal(key: &[u8; 32], n: u64, ad: &[u8], buf: &mut Vec<u8>) {
    let tag = ChaCha20Poly1305::new(Key::from_slice(key))
        .encrypt_in_place_detached(Nonce::from_slice(&nonce(n)), ad, buf)
        .expect("frames are far below ChaCha20's length limit");
    buf.extend_from_slice(&tag);
}

/// Verifies and decrypts in place, stripping the MAC. `false` = forged, reordered or damaged.
pub fn open(key: &[u8; 32], n: u64, ad: &[u8], buf: &mut Vec<u8>) -> bool {
    if buf.len() < MAC_LEN {
        return false;
    }
    let at = buf.len() - MAC_LEN;
    let tag = Tag::clone_from_slice(&buf[at..]);
    buf.truncate(at);
    ChaCha20Poly1305::new(Key::from_slice(key))
        .decrypt_in_place_detached(Nonce::from_slice(&nonce(n)), ad, buf, &tag)
        .is_ok()
}

pub fn ct_eq16(a: &[u8; 16], b: &[u8; 16]) -> bool {
    a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hex;

    fn v() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../../protocol/ava1/vectors/noise_xx.json"
        ))
        .unwrap()
    }
    fn b(v: &serde_json::Value, k: &str) -> Vec<u8> {
        hex::decode(v[k].as_str().unwrap()).unwrap()
    }
    fn k32(x: Vec<u8>) -> [u8; 32] {
        x.try_into().unwrap()
    }

    #[test]
    fn the_published_noise_vector_reproduces() {
        let v = v();
        let pro = b(&v, "init_prologue");
        let (is, ie) = (
            Identity::from_secret(k32(b(&v, "init_static"))),
            k32(b(&v, "init_ephemeral")),
        );
        let (rs, re) = (
            Identity::from_secret(k32(b(&v, "resp_static"))),
            k32(b(&v, "resp_ephemeral")),
        );
        let mut i = Handshake::build(&is, true, Some(&ie), &pro).unwrap();
        let mut r = Handshake::build(&rs, false, Some(&re), &pro).unwrap();
        let msgs = v["messages"].as_array().unwrap();
        for (n, m) in msgs.iter().take(3).enumerate() {
            let payload = hex::decode(m["payload"].as_str().unwrap()).unwrap();
            let (w, rd) = if n % 2 == 0 {
                (&mut i, &mut r)
            } else {
                (&mut r, &mut i)
            };
            let msg = w.write(&payload).unwrap();
            assert_eq!(
                hex::encode(&msg),
                m["ciphertext"].as_str().unwrap(),
                "message {n}"
            );
            assert_eq!(rd.read(&msg).unwrap(), payload);
        }
        assert_eq!(i.remote_static(), Some(rs.public()));
        assert_eq!(r.remote_static(), Some(is.public()));
        let (ki, kr) = (i.finish(), r.finish());
        assert_eq!(hex::encode(&ki.hash), v["handshake_hash"].as_str().unwrap());
        assert_eq!((ki.c2s, ki.s2c), (kr.c2s, kr.s2c));
        // Transport messages 3..5 alternate responder, initiator, responder, each direction from n = 0.
        let (mut n_i, mut n_r) = (0u64, 0u64);
        for (n, m) in msgs.iter().enumerate().skip(3) {
            let mut buf = hex::decode(m["payload"].as_str().unwrap()).unwrap();
            let from_initiator = n % 2 == 0;
            let (key, ctr) = if from_initiator {
                (&ki.c2s, &mut n_i)
            } else {
                (&ki.s2c, &mut n_r)
            };
            seal(key, *ctr, &[], &mut buf);
            *ctr += 1;
            assert_eq!(
                hex::encode(&buf),
                m["ciphertext"].as_str().unwrap(),
                "transport {n}"
            );
        }
    }

    #[test]
    fn a_fresh_handshake_agrees_on_keys_and_peers() {
        let (c, s) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (mut i, mut r) = (
            Handshake::initiator(&c).unwrap(),
            Handshake::responder(&s).unwrap(),
        );
        r.read(&i.write(b"hello").unwrap()).unwrap();
        assert_eq!(i.read(&r.write(b"server").unwrap()).unwrap(), b"server");
        assert_eq!(r.read(&i.write(b"client").unwrap()).unwrap(), b"client");
        assert_eq!(i.remote_static(), Some(s.public()));
        assert_eq!(r.remote_static(), Some(c.public()));
        let (a, b) = (i.finish(), r.finish());
        assert_eq!((a.c2s, a.s2c, a.hash), (b.c2s, b.s2c, b.hash));
        assert_eq!(pairing_code(&a.hash), pairing_code(&b.hash));
        assert!(pairing_code(&a.hash) < 1_000_000);
    }

    #[test]
    fn a_tampered_handshake_message_is_refused() {
        let (c, s) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (mut i, mut r) = (
            Handshake::initiator(&c).unwrap(),
            Handshake::responder(&s).unwrap(),
        );
        r.read(&i.write(&[]).unwrap()).unwrap();
        let mut m2 = r.write(&[]).unwrap();
        m2[40] ^= 1; // inside the encrypted static key
        assert!(i.read(&m2).is_err());
    }

    #[test]
    fn sealing_binds_key_counter_and_header() {
        let k = [3u8; 32];
        let mut a = b"frame body".to_vec();
        seal(&k, 5, b"hdr", &mut a);
        assert_eq!(a.len(), 10 + MAC_LEN);
        for (key, n, ad) in [
            ([4u8; 32], 5u64, &b"hdr"[..]),
            (k, 6, b"hdr"),
            (k, 5, b"hdx"),
        ] {
            let mut x = a.clone();
            assert!(!open(&key, n, ad, &mut x));
        }
        let mut ok = a.clone();
        assert!(open(&k, 5, b"hdr", &mut ok));
        assert_eq!(ok, b"frame body");
        let mut short = vec![0u8; 3];
        assert!(!open(&k, 0, b"", &mut short));
    }

    #[test]
    fn derived_keys_differ_by_lane_and_label() {
        let d = [9u8; 32];
        assert_ne!(lane_key(&d, 0), lane_key(&d, 1));
        let (sid, nonce) = ([1u8; 16], [2u8; 16]);
        assert_ne!(
            join_tag(&d, b"join", &sid, 1, &nonce),
            join_tag(&d, b"join-ack", &sid, 1, &nonce)
        );
        assert!(ct_eq16(&[7; 16], &[7; 16]) && !ct_eq16(&[7; 16], &[8; 16]));
    }

    #[test]
    fn identity_persists_and_a_bad_file_is_an_error() {
        let dir = std::env::temp_dir().join(format!("ava1-id-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = dir.join("ava").join("identity");
        let a = Identity::load_or_create(&p).unwrap();
        assert_eq!(Identity::load_or_create(&p).unwrap().public(), a.public());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::write(&p, b"short").unwrap();
        assert_eq!(
            Identity::load_or_create(&p).err().unwrap().kind(),
            std::io::ErrorKind::InvalidData
        );
        assert_eq!(
            std::fs::read(&p).unwrap(),
            b"short",
            "a bad identity is never replaced"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
