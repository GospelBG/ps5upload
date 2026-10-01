//! Frames on a byte stream, sealed after the handshake (SPEC.md §2, §4.4).
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::frame::{
    Header, HeaderError, CONTROL_MAX_BODY, FLAG_IGNORABLE, FLAG_SEALED, HEADER_LEN, MAX_BODY,
};
use crate::keys::{open, seal, MAC_LEN};
use crate::wire::FrameMessage;
use crate::Ava1Error;

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

pub struct FrameWriter<W> {
    w: W,
    key: Option<[u8; 32]>,
    ctr: u64,
}

impl<W: AsyncWrite + Unpin> FrameWriter<W> {
    pub fn new(w: W) -> Self {
        Self {
            w,
            key: None,
            ctr: 0,
        }
    }

    /// Seal every following frame with `key`; the counter restarts at 0.
    pub fn set_key(&mut self, key: [u8; 32]) {
        self.key = Some(key);
        self.ctr = 0;
    }

    pub async fn send(&mut self, ty: u8, channel: u32, body: &[u8]) -> Result<(), Ava1Error> {
        self.send_flags(ty, 0, channel, body).await
    }

    pub async fn send_ignorable(
        &mut self,
        ty: u8,
        channel: u32,
        body: &[u8],
    ) -> Result<(), Ava1Error> {
        self.send_flags(ty, FLAG_IGNORABLE, channel, body).await
    }

    async fn send_flags(
        &mut self,
        ty: u8,
        flags: u8,
        channel: u32,
        body: &[u8],
    ) -> Result<(), Ava1Error> {
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
        let mut out = Vec::with_capacity(HEADER_LEN + total);
        out.extend_from_slice(&h);
        let mut sealed = body.to_vec();
        if let Some(k) = &self.key {
            seal(k, self.ctr, &h[..12], &mut sealed);
            self.ctr += 1;
        }
        out.extend_from_slice(&sealed);
        self.w.write_all(&out).await?;
        self.w.flush().await?;
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
}

pub struct FrameReader<R> {
    r: R,
    key: Option<[u8; 32]>,
    ctr: u64,
    max_body: u32,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    /// Accepts bodies up to `CONTROL_MAX_BODY` (MAC included) until `set_max_body` raises it.
    pub fn new(r: R) -> Self {
        Self {
            r,
            key: None,
            ctr: 0,
            max_body: CONTROL_MAX_BODY,
        }
    }

    pub fn set_key(&mut self, key: [u8; 32]) {
        self.key = Some(key);
        self.ctr = 0;
    }

    pub fn set_max_body(&mut self, n: u32) {
        self.max_body = n.min(MAX_BODY);
    }

    pub async fn recv(&mut self) -> Result<Frame, Ava1Error> {
        let mut hb = [0u8; HEADER_LEN];
        self.r.read_exact(&mut hb).await.map_err(eof_is_closed)?;
        let h = Header::decode(&hb)?;
        if h.body_len > self.max_body {
            return Err(HeaderError::TooLong(h.body_len).into());
        }
        let mut body = vec![0u8; h.body_len as usize];
        self.r.read_exact(&mut body).await.map_err(eof_is_closed)?;
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

fn eof_is_closed(e: std::io::Error) -> Ava1Error {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        Ava1Error::Closed
    } else {
        Ava1Error::Io(e)
    }
}
