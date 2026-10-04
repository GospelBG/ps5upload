//! Frames on a byte stream, sealed after the handshake (SPEC.md §2, §4.4).
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use zeroize::Zeroizing;

use crate::frame::{
    Header, HeaderError, CONTROL_MAX_BODY, FLAG_IGNORABLE, FLAG_SEALED, HEADER_LEN, MAX_BODY,
};
use crate::keys::{open, seal_slice, MAC_LEN};
use crate::wire::FrameMessage;
use crate::Ava1Error;

/// A frame body on its way out: owned, or shared with the sender's bookkeeping. The data
/// plane keeps every in-flight frame (a lane death requeues it), so the writer borrows the
/// same bytes instead of taking a copy: the one copy per frame is the sealed output buffer
/// (review 003 §5 of 01).
#[derive(Debug, Clone)]
pub enum FrameBody {
    Owned(Vec<u8>),
    Shared(Arc<Vec<u8>>),
}

impl std::ops::Deref for FrameBody {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            FrameBody::Owned(v) => v,
            FrameBody::Shared(a) => a,
        }
    }
}

impl From<Vec<u8>> for FrameBody {
    fn from(v: Vec<u8>) -> Self {
        FrameBody::Owned(v)
    }
}

impl From<Arc<Vec<u8>>> for FrameBody {
    fn from(a: Arc<Vec<u8>>) -> Self {
        FrameBody::Shared(a)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub ty: u8,
    pub flags: u8,
    pub channel: u32,
    pub body: Vec<u8>,
}

impl Frame {
    pub fn decode<M: FrameMessage>(&self) -> Result<M, Ava1Error> {
        if self.ty != M::TYPE {
            return Err(Ava1Error::Unexpected(self.ty));
        }
        Ok(M::decode(&self.body)?)
    }

    /// A receiver that does not know this type may skip it.
    pub fn ignorable(&self) -> bool {
        self.flags & FLAG_IGNORABLE != 0
    }
}

/// Microseconds on a process-wide monotonic clock.
pub(crate) fn now_us() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u64
}

/// How slow a connection may be before the peer counts as gone (SPEC.md §6): no byte
/// moved for `idle`, or one frame slower than `min_rate` bytes/s after an `idle` grace.
#[derive(Debug, Clone, Copy)]
pub struct Pace {
    pub idle: Duration,
    pub min_rate: u32,
}

impl Pace {
    fn frame_budget(&self, len: usize) -> Duration {
        self.idle + Duration::from_millis(len as u64 * 1000 / u64::from(self.min_rate.max(1)))
    }
}

pub struct FrameWriter<W> {
    w: W,
    key: Option<Zeroizing<[u8; 32]>>,
    ctr: u64,
    pace: Option<Pace>,
    /// A write failed or stalled part-way: the stream is torn and nothing more is sent.
    broken: bool,
}

impl<W: AsyncWrite + Unpin> FrameWriter<W> {
    pub fn new(w: W) -> Self {
        Self {
            w,
            key: None,
            ctr: 0,
            pace: None,
            broken: false,
        }
    }

    /// Bounds every write: a peer that takes no bytes for `idle`, or takes one frame
    /// slower than the floor, fails the send and breaks the writer.
    pub fn set_pace(&mut self, pace: Pace) {
        self.pace = Some(pace);
    }

    /// Seal every following frame with `key`; the counter restarts at 0.
    pub fn set_key(&mut self, key: [u8; 32]) {
        self.key = Some(Zeroizing::new(key));
        self.ctr = 0;
    }

    pub async fn send(&mut self, ty: u8, channel: u32, body: &[u8]) -> Result<(), Ava1Error> {
        self.send_with_flags(ty, 0, channel, body).await
    }

    pub async fn send_ignorable(
        &mut self,
        ty: u8,
        channel: u32,
        body: &[u8],
    ) -> Result<(), Ava1Error> {
        self.send_with_flags(ty, FLAG_IGNORABLE, channel, body)
            .await
    }

    /// `send` with the exact header flags (e.g. `FLAG_IGNORABLE`) — the data plane
    /// sends every frame this way.
    pub async fn send_with_flags(
        &mut self,
        ty: u8,
        flags: u8,
        channel: u32,
        body: &[u8],
    ) -> Result<(), Ava1Error> {
        if self.broken {
            return Err(Ava1Error::Lost("an earlier write failed part-way".into()));
        }
        let mac = if self.key.is_some() { MAC_LEN } else { 0 };
        let total = body.len() + mac;
        let body_len =
            u32::try_from(total)
                .ok()
                .filter(|n| *n <= MAX_BODY)
                .ok_or(HeaderError::TooLong(
                    u32::try_from(total).unwrap_or(u32::MAX),
                ))?;
        let flags = flags | if mac > 0 { FLAG_SEALED } else { 0 };
        let h = Header {
            ty,
            flags,
            channel,
            body_len,
        }
        .encode();
        // One buffer: header, then the body sealed where it lies, then the MAC.
        let mut out = Vec::with_capacity(HEADER_LEN + total);
        out.extend_from_slice(&h);
        out.extend_from_slice(body);
        if let Some(k) = &self.key {
            let tag = seal_slice(k, self.ctr, &h[..12], &mut out[HEADER_LEN..]);
            out.extend_from_slice(&tag);
            self.ctr += 1;
        }
        // Broken until proven whole: a failure (or a dropped future) anywhere below leaves
        // a partial frame or a spent counter, after which nothing more may be sent.
        self.broken = true;
        match self.pace {
            None => {
                self.w.write_all(&out).await?;
                self.w.flush().await?;
            }
            Some(p) => {
                let deadline = tokio::time::Instant::now() + p.frame_budget(out.len());
                let mut at = 0;
                while at < out.len() {
                    // One write() call either moves bytes or none, so bounding each call
                    // is exact: a timeout means the peer took nothing for that long.
                    let limit = p
                        .idle
                        .min(deadline.saturating_duration_since(tokio::time::Instant::now()));
                    let n = tokio::time::timeout(limit, self.w.write(&out[at..]))
                        .await
                        .map_err(|_| Ava1Error::Timeout)??;
                    if n == 0 {
                        return Err(Ava1Error::Closed);
                    }
                    at += n;
                }
                tokio::time::timeout(p.idle, self.w.flush())
                    .await
                    .map_err(|_| Ava1Error::Timeout)??;
            }
        }
        self.broken = false;
        Ok(())
    }

    pub async fn send_msg<M: FrameMessage>(
        &mut self,
        channel: u32,
        m: &M,
    ) -> Result<(), Ava1Error> {
        let b = m.to_bytes()?;
        self.send(M::TYPE, channel, &b).await
    }

    pub async fn shutdown(&mut self) -> std::io::Result<()> {
        self.w.shutdown().await
    }

    /// The underlying stream (tests capture written frames this way).
    pub fn into_inner(self) -> W {
        self.w
    }
}

pub struct FrameReader<R> {
    r: R,
    key: Option<Zeroizing<[u8; 32]>>,
    ctr: u64,
    max_body: u32,
    /// Stamped (`now_us`) whenever bytes arrive, so liveness sees a large frame in progress.
    progress: Option<Arc<AtomicU64>>,
    pace: Option<Pace>,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    /// Accepts bodies up to `CONTROL_MAX_BODY` (MAC included) until `set_max_body` raises it.
    pub fn new(r: R) -> Self {
        Self {
            r,
            key: None,
            ctr: 0,
            max_body: CONTROL_MAX_BODY,
            progress: None,
            pace: None,
        }
    }

    /// Stamp `progress` with `now_us()` on every byte received, and give each frame body a
    /// deadline of `pace.idle + len / pace.min_rate` (a peer may be slow, but must not
    /// drip one frame forever). Silence between frames is the owner's to judge.
    pub fn set_pace(&mut self, progress: Arc<AtomicU64>, pace: Pace) {
        self.progress = Some(progress);
        self.pace = Some(pace);
    }

    async fn fill(&mut self, buf: &mut [u8]) -> Result<(), Ava1Error> {
        let mut at = 0;
        while at < buf.len() {
            let n = self.r.read(&mut buf[at..]).await?;
            if n == 0 {
                return Err(Ava1Error::Closed);
            }
            if let Some(p) = &self.progress {
                p.store(now_us(), Ordering::Relaxed);
            }
            at += n;
        }
        Ok(())
    }

    pub fn set_key(&mut self, key: [u8; 32]) {
        self.key = Some(Zeroizing::new(key));
        self.ctr = 0;
    }

    pub fn set_max_body(&mut self, n: u32) {
        self.max_body = n.min(MAX_BODY);
    }

    pub async fn recv(&mut self) -> Result<Frame, Ava1Error> {
        let mut hb = [0u8; HEADER_LEN];
        self.fill(&mut hb).await?;
        let h = Header::decode(&hb)?;
        if h.body_len > self.max_body {
            return Err(HeaderError::TooLong(h.body_len).into());
        }
        let mut body = vec![0u8; h.body_len as usize];
        match self.pace {
            None => self.fill(&mut body).await?,
            Some(p) => {
                let budget = p.frame_budget(body.len());
                tokio::time::timeout(budget, self.fill(&mut body))
                    .await
                    .map_err(|_| {
                        Ava1Error::Lost(format!(
                            "a {}-byte frame took over {} ms to arrive",
                            body.len(),
                            budget.as_millis()
                        ))
                    })??;
            }
        }
        let sealed = h.flags & FLAG_SEALED != 0;
        match &self.key {
            Some(k) => {
                if !sealed || !open(k, self.ctr, &hb[..12], &mut body) {
                    return Err(Ava1Error::BadTag);
                }
                self.ctr += 1;
            }
            None if sealed => return Err(Ava1Error::Unexpected(h.ty)),
            None => {}
        }
        Ok(Frame {
            ty: h.ty,
            flags: h.flags,
            channel: h.channel,
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shared_body_is_borrowed_not_copied() {
        let a = Arc::new(vec![7u8; 1 << 20]);
        let b: FrameBody = a.clone().into();
        assert_eq!(b.as_ptr(), a.as_ptr(), "same bytes, no memcpy");
        assert_eq!(b.len(), 1 << 20);
        let o: FrameBody = vec![1u8, 2, 3].into();
        assert_eq!(&*o, &[1, 2, 3]);
    }

    #[tokio::test]
    async fn a_shared_body_seals_to_the_same_wire_bytes_as_an_owned_one() {
        let key = [0x33u8; 32];
        let body: Vec<u8> = (0..40_000u32).map(|i| i as u8).collect();
        let mut w = FrameWriter::new(Vec::new());
        w.set_key(key);
        let shared: FrameBody = Arc::new(body.clone()).into();
        w.send_with_flags(0x20, 0, 5, &shared).await.unwrap();
        // The shared body is untouched (it may be resent), and the frame opens.
        assert_eq!(&*shared, &body[..]);
        let wire = w.into_inner();
        let mut r = FrameReader::new(&wire[..]);
        r.set_key(key);
        assert_eq!(r.recv().await.unwrap().body, body);
    }

    async fn sealed_frame(key: [u8; 32]) -> Vec<u8> {
        let mut w = FrameWriter::new(Vec::new());
        w.set_key(key);
        w.send(0x09, 7, b"heartbeat body").await.unwrap();
        w.into_inner()
    }

    #[tokio::test]
    async fn a_sealed_frame_whose_header_was_altered_does_not_open() {
        let key = [0x21u8; 32];
        let frame = sealed_frame(key).await;
        let mut r = FrameReader::new(&frame[..]);
        r.set_key(key);
        assert_eq!(r.recv().await.unwrap().body, b"heartbeat body");
        // Type, flags-free bytes of the channel: each change gets a valid CRC, so only the
        // AEAD's associated data (header bytes 0..12) can catch it.
        for at in [2usize, 4, 7] {
            let mut t = frame.clone();
            t[at] ^= 0x01;
            let crc = crate::crc32c::crc32c(&t[..12]);
            t[12..16].copy_from_slice(&crc.to_le_bytes());
            let mut r = FrameReader::new(&t[..]);
            r.set_key(key);
            assert!(
                matches!(r.recv().await, Err(Ava1Error::BadTag)),
                "header byte {at}"
            );
        }
    }
}
