//! The client side of a session (SPEC.md §6–§8).
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;

use crate::conn::{Frame, FrameReader, FrameWriter};
use crate::gen::{self, Bye, PairConfirm, PairResult, RpcRequest, RpcResponse};
use crate::handshake::{self, Established};
use crate::keys::Identity;
use crate::link::{drive, Link, SharedWriter};
use crate::peers::PeerStore;
use crate::wire::{FrameMessage, Message};
use crate::Ava1Error;

#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub ping_every: Duration,
    pub dead_after: Duration,
    pub handshake: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            ping_every: Duration::from_secs(2),
            dead_after: Duration::from_secs(6),
            handshake: Duration::from_secs(10),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcReply {
    pub status: u16,
    pub body: Vec<u8>,
}

type Pending = Arc<Mutex<HashMap<u32, oneshot::Sender<Frame>>>>;

// Task 7 (lanes) reads addr, timing and lanes_live; it removes this allow.
#[allow(dead_code)]
pub struct Session {
    pub(crate) addr: SocketAddr,
    pub(crate) timing: Timing,
    pub(crate) est: Established,
    peers: Arc<Mutex<PeerStore>>,
    writer: SharedWriter,
    link: Link,
    pending: Pending,
    next_req: AtomicU32,
    pub(crate) lanes_live: Arc<Mutex<[bool; 9]>>,
    dispatcher: AbortHandle,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("addr", &self.addr)
            .field("peer_name", &self.est.peer_name)
            .field("closed", &self.link.is_closed())
            .finish_non_exhaustive()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.dispatcher.abort();
    }
}

async fn within<T>(
    limit: Duration,
    f: impl std::future::Future<Output = Result<T, Ava1Error>>,
) -> Result<T, Ava1Error> {
    tokio::time::timeout(limit, f)
        .await
        .map_err(|_| Ava1Error::Timeout)?
}

/// Sends one frame from a task of its own: a caller that drops the future (a timeout,
/// a `select!`) must never cancel a write midway, which would leave a partial sealed
/// frame on the wire and desynchronise the stream. Dropping the join handle does not
/// cancel the task.
struct PendingEntry<'a> {
    pending: &'a Pending,
    id: u32,
}

impl Drop for PendingEntry<'_> {
    fn drop(&mut self) {
        self.pending.lock().unwrap().remove(&self.id);
    }
}

async fn send_owned<M: FrameMessage + Send + Sync + 'static>(
    writer: &SharedWriter,
    channel: u32,
    m: M,
) -> Result<(), Ava1Error> {
    let mut guard = writer.clone().lock_owned().await;
    tokio::spawn(async move { guard.send_msg(channel, &m).await })
        .await
        .map_err(|e| Ava1Error::Lost(format!("send task failed: {e}")))?
}

pub async fn connect(
    addr: &str,
    me: Arc<Identity>,
    peers: Arc<Mutex<PeerStore>>,
    my_name: &str,
    timing: Timing,
) -> Result<Session, Ava1Error> {
    let stream = within(timing.handshake, async {
        Ok(TcpStream::connect(addr).await?)
    })
    .await?;
    stream.set_nodelay(true)?;
    let peer_addr = stream.peer_addr()?;
    let (rh, wh) = stream.into_split();
    let (mut r, mut w) = (FrameReader::new(rh), FrameWriter::new(wh));
    let est = within(
        timing.handshake,
        handshake::client(&mut r, &mut w, &me, my_name, |k| {
            peers.lock().unwrap().contains(k)
        }),
    )
    .await?;
    let writer: SharedWriter = Arc::new(tokio::sync::Mutex::new(w));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let link = drive(r, writer.clone(), timing, tx);
    let pending: Pending = Arc::default();
    let p2 = pending.clone();
    let dispatcher = tokio::spawn(async move {
        while let Some(f) = rx.recv().await {
            if f.ty == RpcResponse::TYPE || f.ty == PairResult::TYPE {
                let waiter = p2.lock().unwrap().remove(&f.channel);
                if let Some(tx) = waiter {
                    let _ = tx.send(f);
                }
            }
        }
        p2.lock().unwrap().clear();
    })
    .abort_handle();
    Ok(Session {
        addr: peer_addr,
        timing,
        est,
        peers,
        writer,
        link,
        pending,
        next_req: AtomicU32::new(1),
        lanes_live: Arc::default(),
        dispatcher,
    })
}

impl Session {
    pub fn peer_key(&self) -> [u8; 32] {
        self.est.peer_key
    }

    pub fn peer_name(&self) -> &str {
        &self.est.peer_name
    }

    /// The code to show the user while the devices are not yet paired.
    pub fn pairing_code(&self) -> Option<u32> {
        self.est.pairing.map(|p| p.code)
    }

    pub fn rtt(&self) -> Option<Duration> {
        self.link.rtt()
    }

    pub fn is_closed(&self) -> bool {
        self.link.is_closed()
    }

    /// Waits until the session ends; returns why.
    pub async fn closed(&self) -> String {
        self.link.closed().await
    }

    async fn request<M: FrameMessage + Send + Sync + 'static>(
        &self,
        m: M,
    ) -> Result<Frame, Ava1Error> {
        let id = self.next_req.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        // Removes the entry on every exit, including the caller dropping this future.
        let _entry = PendingEntry {
            pending: &self.pending,
            id,
        };
        send_owned(&self.writer, id, m).await?;
        let mut rx = rx;
        tokio::select! {
            f = &mut rx => f.map_err(|_| Ava1Error::Lost(self.link.reason())),
            why = self.link.closed() => {
                // The reply may have arrived in the same instant as the close (a server
                // that answers and hangs up): the dispatcher drains the frames already
                // delivered, then drops our sender. Give it that moment before giving up.
                let late = tokio::time::timeout(Duration::from_millis(250), rx).await;
                match late {
                    Ok(Ok(f)) => Ok(f),
                    _ => Err(Ava1Error::Lost(why)),
                }
            }
        }
    }

    pub async fn rpc(&self, method: u16, body: &[u8]) -> Result<RpcReply, Ava1Error> {
        let f = self
            .request(RpcRequest {
                method,
                body: body.to_vec(),
            })
            .await?;
        let r: RpcResponse = f.decode()?;
        Ok(RpcReply {
            status: r.status,
            body: r.body,
        })
    }

    pub async fn node_info(&self) -> Result<gen::NodeInfo, Ava1Error> {
        let r = self.rpc(gen::METHOD_NODE_INFO, &[]).await?;
        if r.status != gen::STATUS_OK {
            return Err(Ava1Error::Refused {
                code: r.status,
                message: "node.info failed".into(),
            });
        }
        Ok(gen::NodeInfo::decode(&r.body)?)
    }

    /// Asks the other device (which must already trust us) to accept new pairings for
    /// `seconds` — the app's "Pair another device".
    pub async fn open_pairing(&self, seconds: u16) -> Result<(), Ava1Error> {
        let body = gen::PairingOpen { seconds }.to_bytes()?;
        let r = self.rpc(gen::METHOD_PAIRING_OPEN, &body).await?;
        if r.status != gen::STATUS_OK {
            return Err(Ava1Error::Refused {
                code: r.status,
                message: "pairing.open refused".into(),
            });
        }
        Ok(())
    }

    /// Call after the user confirmed the codes match. Stores the other device's key.
    pub async fn confirm_pairing(&mut self) -> Result<(), Ava1Error> {
        let Some(p) = self.est.pairing else {
            return Ok(());
        };
        if p.server_must_confirm {
            let r: PairResult = self.request(PairConfirm {}).await?.decode()?;
            if r.accepted == 0 {
                return Err(Ava1Error::Refused {
                    code: gen::ERR_PAIRING_CLOSED,
                    message: "the other device did not accept the pairing".into(),
                });
            }
        }
        self.peers
            .lock()
            .unwrap()
            .add(self.est.peer_key, &self.est.peer_name)?;
        self.est.pairing = None;
        Ok(())
    }

    pub async fn close(self) {
        let _ = send_owned(&self.writer, 0, Bye { reason: 0 }).await;
    }
}
