//! The server side: accept loop, control connections, pairing, RPC (SPEC.md §6–§8).
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use crate::conn::{Frame, FrameReader, FrameWriter};
use crate::gen::{self, Hs1, Join, JoinAck, PairConfirm, PairResult, RpcRequest, RpcResponse};
use crate::handshake::{self, refuse};
use crate::keys::{self, Identity, SessionKeys};
use crate::link::{drive, SharedWriter};
use crate::peers::PeerStore;
use crate::session::{RpcReply, Timing};
use crate::wire::{FrameMessage, Message};
use crate::Ava1Error;

pub const MAX_CONNS: usize = 64;
pub const MAX_SESSIONS: usize = 16;
/// Calls in flight per session; more are answered `ERR_BUSY`.
pub const RPC_WORKERS: usize = 4;
/// The longest window `pairing.open` may ask for.
pub const MAX_PAIRING_WINDOW_S: u16 = 600;

pub struct PairRequest {
    pub peer_key: [u8; 32],
    pub peer_name: String,
    pub code: u32,
}

pub type RpcHandler = Box<dyn Fn(u16, &[u8]) -> RpcReply + Send + Sync>;
pub type PairHook = Box<dyn Fn(&PairRequest) -> bool + Send + Sync>;
pub type NotifyHook = Box<dyn Fn(&PairRequest) + Send + Sync>;

pub(crate) struct SessionEntry {
    pub(crate) keys: SessionKeys,
    pub(crate) paired: AtomicBool,
    pub(crate) lane_gen: Mutex<[u32; 9]>,
    pub(crate) nonces: Mutex<VecDeque<[u8; 16]>>,
}

pub struct ServerCtx {
    identity: Identity,
    name: String,
    timing: Timing,
    peers: Mutex<PeerStore>,
    pairing_until: Mutex<Option<Instant>>,
    notify: NotifyHook,
    approve: PairHook,
    rpc: RpcHandler,
    pub(crate) sessions: Mutex<HashMap<[u8; 16], Arc<SessionEntry>>>,
    conns: AtomicUsize,
    session_slots: AtomicUsize,
}

impl ServerCtx {
    pub fn new(identity: Identity, name: &str, peers: PeerStore, rpc: RpcHandler) -> Self {
        Self {
            identity,
            name: name.to_string(),
            timing: Timing::default(),
            peers: Mutex::new(peers),
            pairing_until: Mutex::new(None),
            notify: Box::new(|_| {}),
            approve: Box::new(|_| true),
            rpc,
            sessions: Mutex::default(),
            conns: AtomicUsize::new(0),
            session_slots: AtomicUsize::new(0),
        }
    }

    pub fn with_timing(mut self, t: Timing) -> Self {
        self.timing = t;
        self
    }

    /// Called when an unknown device starts pairing (show `code` to the user).
    pub fn with_notify(mut self, f: NotifyHook) -> Self {
        self.notify = f;
        self
    }

    /// Decides a PairConfirm (default: accept while the window is open).
    pub fn with_approve(mut self, f: PairHook) -> Self {
        self.approve = f;
        self
    }

    pub fn open_pairing(&self, d: Duration) {
        *self.pairing_until.lock().unwrap() = Some(Instant::now() + d);
    }

    /// The automatic window: only a node with no paired peer opens one by itself
    /// (SPEC.md §5 item 6). Returns whether it opened.
    pub fn open_pairing_if_unpaired(&self, d: Duration) -> bool {
        let unpaired = self.peers.lock().unwrap().list().is_empty();
        if unpaired {
            self.open_pairing(d);
        }
        unpaired
    }

    pub fn close_pairing(&self) {
        *self.pairing_until.lock().unwrap() = None;
    }

    pub fn pairing_open(&self) -> bool {
        self.pairing_until
            .lock()
            .unwrap()
            .is_some_and(|t| Instant::now() < t)
    }

    pub fn connections(&self) -> usize {
        self.conns.load(Ordering::SeqCst)
    }

    pub fn sessions(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }
}

/// Accepts forever. Never exits on an accept error (a transient errno must not take
/// the server down); refuses connections past `MAX_CONNS` with `ERR_BUSY`.
pub async fn serve(listener: TcpListener, ctx: Arc<ServerCtx>) {
    loop {
        let (s, _) = match listener.accept().await {
            Ok(x) => x,
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        if ctx.conns.fetch_add(1, Ordering::SeqCst) >= MAX_CONNS {
            ctx.conns.fetch_sub(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let (_, wh) = s.into_split();
                refuse(
                    &mut FrameWriter::new(wh),
                    gen::ERR_BUSY,
                    "too many connections",
                )
                .await;
            });
            continue;
        }
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let _ = handle(s, &ctx).await;
            ctx.conns.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

struct SessionSlot(Arc<ServerCtx>);

impl SessionSlot {
    fn take(ctx: &Arc<ServerCtx>) -> Option<Self> {
        if ctx.session_slots.fetch_add(1, Ordering::SeqCst) >= MAX_SESSIONS {
            ctx.session_slots.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Self(ctx.clone()))
    }
}

impl Drop for SessionSlot {
    fn drop(&mut self) {
        self.0.session_slots.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn handle(s: TcpStream, ctx: &Arc<ServerCtx>) -> Result<(), Ava1Error> {
    s.set_nodelay(true)?;
    let (rh, wh) = s.into_split();
    let (mut r, mut w) = (FrameReader::new(rh), FrameWriter::new(wh));
    let first = tokio::time::timeout(ctx.timing.handshake, r.recv())
        .await
        .map_err(|_| Ava1Error::Timeout)??;
    match first.ty {
        Hs1::TYPE => control(r, w, first, ctx).await,
        Join::TYPE => lane(r, w, first, ctx).await,
        t => {
            refuse(&mut w, gen::ERR_PROTOCOL, "expected Hs1 or Join").await;
            Err(Ava1Error::Unexpected(t))
        }
    }
}

async fn control(
    mut r: FrameReader<OwnedReadHalf>,
    mut w: FrameWriter<OwnedWriteHalf>,
    first: Frame,
    ctx: &Arc<ServerCtx>,
) -> Result<(), Ava1Error> {
    // Reserve atomically before the handshake; released on every exit path by the guard.
    let Some(_slot) = SessionSlot::take(ctx) else {
        refuse(&mut w, gen::ERR_BUSY, "too many sessions").await;
        return Err(Ava1Error::Refused {
            code: gen::ERR_BUSY,
            message: "too many sessions".into(),
        });
    };
    let est = tokio::time::timeout(
        ctx.timing.handshake,
        handshake::server(
            &mut r,
            &mut w,
            first,
            &ctx.identity,
            &ctx.name,
            |k| ctx.peers.lock().unwrap().contains(k),
            ctx.pairing_open(),
        ),
    )
    .await
    .map_err(|_| Ava1Error::Timeout)??;
    let req = PairRequest {
        peer_key: est.peer_key,
        peer_name: est.peer_name.clone(),
        code: keys::pairing_code(&est.keys.hash),
    };
    let entry = Arc::new(SessionEntry {
        keys: est.keys.clone(),
        paired: AtomicBool::new(est.pairing.is_none()),
        lane_gen: Mutex::new([0; 9]),
        nonces: Mutex::new(VecDeque::new()),
    });
    ctx.sessions
        .lock()
        .unwrap()
        .insert(est.session_id, entry.clone());
    if est.pairing.is_some() {
        (ctx.notify)(&req);
    }
    let writer: SharedWriter = Arc::new(tokio::sync::Mutex::new(w));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let link = drive(r, writer.clone(), ctx.timing, tx);
    let rpc_slots = Arc::new(tokio::sync::Semaphore::new(RPC_WORKERS));
    while let Some(f) = rx.recv().await {
        match f.ty {
            RpcRequest::TYPE => {
                let Ok(q) = f.decode::<RpcRequest>() else {
                    break;
                };
                let channel = f.channel;
                if !entry.paired.load(Ordering::SeqCst) {
                    let resp = RpcResponse {
                        status: gen::ERR_NOT_PAIRED,
                        body: Vec::new(),
                    };
                    if writer.lock().await.send_msg(channel, &resp).await.is_err() {
                        break;
                    }
                    continue;
                }
                if q.method == gen::METHOD_PAIRING_OPEN {
                    let status = match gen::PairingOpen::decode(&q.body) {
                        Ok(o) => {
                            ctx.open_pairing(Duration::from_secs(u64::from(
                                o.seconds.min(MAX_PAIRING_WINDOW_S),
                            )));
                            gen::STATUS_OK
                        }
                        Err(_) => gen::ERR_PROTOCOL,
                    };
                    let _ = writer
                        .lock()
                        .await
                        .send_msg(
                            channel,
                            &RpcResponse {
                                status,
                                body: Vec::new(),
                            },
                        )
                        .await;
                    continue;
                }
                // Calls run on workers; the reader (and so liveness) never waits for one.
                let Ok(permit) = rpc_slots.clone().try_acquire_owned() else {
                    let _ = writer
                        .lock()
                        .await
                        .send_msg(
                            channel,
                            &RpcResponse {
                                status: gen::ERR_BUSY,
                                body: Vec::new(),
                            },
                        )
                        .await;
                    continue;
                };
                let (ctx, writer) = (ctx.clone(), writer.clone());
                tokio::spawn(async move {
                    let reply = tokio::task::spawn_blocking(move || (ctx.rpc)(q.method, &q.body))
                        .await
                        .unwrap_or(RpcReply {
                            status: gen::ERR_INTERNAL,
                            body: Vec::new(),
                        });
                    let _ = writer
                        .lock()
                        .await
                        .send_msg(
                            channel,
                            &RpcResponse {
                                status: reply.status,
                                body: reply.body,
                            },
                        )
                        .await;
                    drop(permit);
                });
            }
            PairConfirm::TYPE => {
                let accepted = entry.paired.load(Ordering::SeqCst)
                    || (ctx.pairing_open()
                        && (ctx.approve)(&req)
                        && ctx
                            .peers
                            .lock()
                            .unwrap()
                            .add(req.peer_key, &req.peer_name)
                            .is_ok());
                entry.paired.store(accepted, Ordering::SeqCst);
                let _ = writer
                    .lock()
                    .await
                    .send_msg(
                        f.channel,
                        &PairResult {
                            accepted: u8::from(accepted),
                        },
                    )
                    .await;
                if !accepted {
                    break;
                }
            }
            _ if f.ignorable() => {}
            _ => {
                refuse(
                    &mut *writer.lock().await,
                    gen::ERR_PROTOCOL,
                    "unexpected frame",
                )
                .await;
                break;
            }
        }
    }
    ctx.sessions.lock().unwrap().remove(&est.session_id);
    drop(link);
    Ok(())
}

/// The nonces a session remembers, so a captured Join cannot be replayed.
const JOIN_NONCES: usize = 64;

async fn lane(
    mut r: FrameReader<OwnedReadHalf>,
    mut w: FrameWriter<OwnedWriteHalf>,
    first: Frame,
    ctx: &Arc<ServerCtx>,
) -> Result<(), Ava1Error> {
    let j: Join = first.decode()?;
    let entry = ctx.sessions.lock().unwrap().get(&j.session_id).cloned();
    let refused = |code: u16| Ava1Error::Refused {
        code,
        message: "join refused".into(),
    };
    let Some(entry) = entry else {
        refuse(&mut w, gen::ERR_BAD_JOIN, "unknown session").await;
        return Err(refused(gen::ERR_BAD_JOIN));
    };
    let want = keys::join_tag(&entry.keys.c2s, b"join", &j.session_id, j.lane_id, &j.nonce);
    let lane_ok = (1..=gen::MAX_LANES as u16).contains(&j.lane_id);
    if !lane_ok || !keys::ct_eq16(&want, &j.tag) {
        refuse(&mut w, gen::ERR_BAD_JOIN, "join refused").await;
        return Err(refused(gen::ERR_BAD_JOIN));
    }
    let fresh = {
        let mut n = entry.nonces.lock().unwrap();
        let fresh = !n.contains(&j.nonce);
        if fresh {
            if n.len() >= JOIN_NONCES {
                n.pop_front();
            }
            n.push_back(j.nonce);
        }
        fresh
    };
    if !fresh {
        refuse(&mut w, gen::ERR_BAD_JOIN, "join replayed").await;
        return Err(refused(gen::ERR_BAD_JOIN));
    }
    if !entry.paired.load(Ordering::SeqCst) {
        refuse(&mut w, gen::ERR_NOT_PAIRED, "pair first").await;
        return Err(Ava1Error::NotPaired);
    }
    let lane = j.lane_id as usize;
    let gen_no = {
        let mut g = entry.lane_gen.lock().unwrap();
        g[lane] += 1;
        g[lane]
    };
    let tag = keys::join_tag(
        &entry.keys.s2c,
        b"join-ack",
        &j.session_id,
        j.lane_id,
        &j.nonce,
    );
    w.send_msg(
        0,
        &JoinAck {
            lane_id: j.lane_id,
            tag,
        },
    )
    .await?;
    r.set_key(keys::lane_key(&entry.keys.c2s, j.lane_id));
    w.set_key(keys::lane_key(&entry.keys.s2c, j.lane_id));
    let writer: SharedWriter = Arc::new(tokio::sync::Mutex::new(w));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let link = drive(r, writer, ctx.timing, tx);
    loop {
        tokio::select! {
            f = rx.recv() => if f.is_none() { break },
            _ = tokio::time::sleep(ctx.timing.ping_every) => {
                let superseded = entry.lane_gen.lock().unwrap()[lane] != gen_no;
                let session_gone = !ctx.sessions.lock().unwrap().contains_key(&j.session_id);
                if superseded || session_gone {
                    break;
                }
            }
        }
    }
    drop(link);
    Ok(())
}
