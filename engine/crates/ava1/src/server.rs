//! The server side: accept loop, control connections, pairing, RPC (SPEC.md §6–§8).
use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use crate::conn::{Frame, FrameReader, FrameWriter};
use crate::gen::{self, Hs1, Join, JoinAck, PairConfirm, PairResult, RpcRequest, RpcResponse};
use crate::handshake::{self, refuse, Admission};
use crate::keys::{self, Identity, SessionKeys};
use crate::link::{drive, Full, Outbox, DELIVER_DEPTH};
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
/// Connections one source address may hold (a session is 1 control + up to 8 lanes).
pub const MAX_CONNS_PER_IP: usize = 12;
/// Sessions that were welcomed during a pairing window but have not confirmed yet.
pub const MAX_UNPAIRED: usize = 2;
/// How long such a session may wait for its PairConfirm.
pub const PAIR_CONFIRM_DEADLINE: Duration = Duration::from_secs(60);
/// At most one pairing notification per this long, however many devices knock.
pub const NOTIFY_EVERY: Duration = Duration::from_secs(10);

/// The server's admission limits (SPEC.md §8). Tests lower or raise them.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub conns_per_ip: usize,
    pub unpaired: usize,
    pub pair_confirm: Duration,
    pub notify_every: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            conns_per_ip: MAX_CONNS_PER_IP,
            unpaired: MAX_UNPAIRED,
            pair_confirm: PAIR_CONFIRM_DEADLINE,
            notify_every: NOTIFY_EVERY,
        }
    }
}

pub struct PairRequest {
    pub peer_key: [u8; 32],
    pub peer_name: String,
    pub code: u32,
}

pub type RpcHandler = Box<dyn Fn(u16, &[u8]) -> RpcReply + Send + Sync>;
pub type PairHook = Box<dyn Fn(&PairRequest) -> bool + Send + Sync>;
pub type NotifyHook = Box<dyn Fn(&PairRequest) + Send + Sync>;
pub type LogHook = Box<dyn Fn(&str) + Send + Sync>;

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
    limits: Limits,
    peers: Mutex<PeerStore>,
    pairing_until: Mutex<Option<Instant>>,
    notify: NotifyHook,
    last_notify: Mutex<Option<Instant>>,
    log: LogHook,
    approve: PairHook,
    rpc: RpcHandler,
    pub(crate) sessions: Mutex<HashMap<[u8; 16], Arc<SessionEntry>>>,
    conns: AtomicUsize,
    per_ip: Mutex<HashMap<IpAddr, usize>>,
    session_slots: AtomicUsize,
    unpaired: AtomicUsize,
}

impl ServerCtx {
    pub fn new(identity: Identity, name: &str, peers: PeerStore, rpc: RpcHandler) -> Self {
        Self {
            identity,
            name: name.to_string(),
            timing: Timing::default(),
            limits: Limits::default(),
            peers: Mutex::new(peers),
            pairing_until: Mutex::new(None),
            notify: Box::new(|_| {}),
            last_notify: Mutex::new(None),
            log: Box::new(|_| {}),
            approve: Box::new(|_| true),
            rpc,
            sessions: Mutex::default(),
            conns: AtomicUsize::new(0),
            per_ip: Mutex::default(),
            session_slots: AtomicUsize::new(0),
            unpaired: AtomicUsize::new(0),
        }
    }

    pub fn with_limits(mut self, l: Limits) -> Self {
        self.limits = l;
        self
    }

    /// Where the server reports what an operator should know (a peers file it could not
    /// read, a pairing it could not store). Default: nowhere.
    pub fn with_log(mut self, f: LogHook) -> Self {
        self.log = f;
        self
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
    /// (SPEC.md §5 item 6). Returns whether it opened. A peers file that exists but could
    /// not be read is not "no paired peer": the window stays shut, and it is logged.
    pub fn open_pairing_if_unpaired(&self, d: Duration) -> bool {
        let unpaired = {
            let peers = self.peers.lock().unwrap();
            if let Some(why) = peers.unreadable() {
                (self.log)(&format!(
                    "ava1: the peers file could not be read ({why}); pairing stays closed and the file is left alone"
                ));
                return false;
            }
            peers.list().is_empty()
        };
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

    /// Shows a pairing request to the user, at most once per `notify_every`: a stranger
    /// reconnecting in a loop must not flood the screen.
    fn notify_limited(&self, req: &PairRequest) {
        {
            let mut last = self.last_notify.lock().unwrap();
            if last.is_some_and(|t| t.elapsed() < self.limits.notify_every) {
                return;
            }
            *last = Some(Instant::now());
        }
        (self.notify)(req);
    }
}

/// One connection's place in the global and per-address counts.
struct ConnSlot {
    ctx: Arc<ServerCtx>,
    ip: IpAddr,
}

impl ConnSlot {
    /// `Err` says which limit was hit.
    fn take(ctx: &Arc<ServerCtx>, ip: IpAddr) -> Result<Self, &'static str> {
        if ctx.conns.fetch_add(1, Ordering::SeqCst) >= MAX_CONNS {
            ctx.conns.fetch_sub(1, Ordering::SeqCst);
            return Err("too many connections");
        }
        let mut per_ip = ctx.per_ip.lock().unwrap();
        let n = per_ip.entry(ip).or_insert(0);
        if *n >= ctx.limits.conns_per_ip {
            if *n == 0 {
                per_ip.remove(&ip);
            }
            drop(per_ip);
            ctx.conns.fetch_sub(1, Ordering::SeqCst);
            return Err("too many connections from this address");
        }
        *n += 1;
        drop(per_ip);
        Ok(Self {
            ctx: ctx.clone(),
            ip,
        })
    }
}

impl Drop for ConnSlot {
    fn drop(&mut self) {
        let mut per_ip = self.ctx.per_ip.lock().unwrap();
        if let Some(n) = per_ip.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                per_ip.remove(&self.ip);
            }
        }
        drop(per_ip);
        self.ctx.conns.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A welcomed but not yet confirmed session's place among `Limits::unpaired`.
struct UnpairedSlot(Arc<ServerCtx>);

impl UnpairedSlot {
    fn take(ctx: &Arc<ServerCtx>) -> Option<Self> {
        if ctx.unpaired.fetch_add(1, Ordering::SeqCst) >= ctx.limits.unpaired {
            ctx.unpaired.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Self(ctx.clone()))
    }
}

impl Drop for UnpairedSlot {
    fn drop(&mut self) {
        self.0.unpaired.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Accepts forever. Never exits on an accept error (a transient errno must not take
/// the server down); refuses connections past `MAX_CONNS`, or past
/// `Limits::conns_per_ip` from one address, with `ERR_BUSY`.
pub async fn serve(listener: TcpListener, ctx: Arc<ServerCtx>) {
    loop {
        let (s, from) = match listener.accept().await {
            Ok(x) => x,
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let slot = match ConnSlot::take(&ctx, from.ip()) {
            Ok(slot) => slot,
            Err(why) => {
                tokio::spawn(async move {
                    let (_, wh) = s.into_split();
                    let mut w = FrameWriter::new(wh);
                    let deadline = tokio::time::Instant::now() + FAREWELL;
                    refuse_by(deadline, &mut w, gen::ERR_BUSY, why).await;
                });
                continue;
            }
        };
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let _ = handle(s, &ctx).await;
            drop(slot);
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
    // One deadline for the whole handshake, from accept to Welcome (or JoinAck): a peer
    // that trickles bytes cannot stretch it, and nothing before it can wait longer.
    let deadline = tokio::time::Instant::now() + ctx.timing.handshake;
    let first = tokio::time::timeout_at(deadline, r.recv())
        .await
        .map_err(|_| Ava1Error::Timeout)??;
    match first.ty {
        Hs1::TYPE => control(r, w, first, ctx, deadline).await,
        Join::TYPE => lane(r, w, first, ctx, deadline).await,
        t => {
            refuse_by(deadline, &mut w, gen::ERR_PROTOCOL, "expected Hs1 or Join").await;
            Err(Ava1Error::Unexpected(t))
        }
    }
}

async fn control(
    mut r: FrameReader<OwnedReadHalf>,
    mut w: FrameWriter<OwnedWriteHalf>,
    first: Frame,
    ctx: &Arc<ServerCtx>,
    deadline: tokio::time::Instant,
) -> Result<(), Ava1Error> {
    // Reserve atomically before the handshake; released on every exit path by the guard.
    let Some(_slot) = SessionSlot::take(ctx) else {
        refuse_by(deadline, &mut w, gen::ERR_BUSY, "too many sessions").await;
        return Err(Ava1Error::Refused {
            code: gen::ERR_BUSY,
            message: "too many sessions".into(),
        });
    };
    // Held while the session is welcomed but unconfirmed; dropped when it pairs or ends.
    let mut unpaired_slot: Option<UnpairedSlot> = None;
    let est = tokio::time::timeout_at(
        deadline,
        handshake::server(&mut r, &mut w, first, &ctx.identity, &ctx.name, |k| {
            if ctx.peers.lock().unwrap().contains(k) {
                Admission::Known
            } else if !ctx.pairing_open() {
                Admission::Refuse(
                    gen::ERR_PAIRING_CLOSED,
                    "this device is not paired and pairing is closed",
                )
            } else {
                match UnpairedSlot::take(ctx) {
                    Some(slot) => {
                        unpaired_slot = Some(slot);
                        Admission::Pairing
                    }
                    None => Admission::Refuse(gen::ERR_BUSY, "too many devices are pairing"),
                }
            }
        }),
    )
    .await
    .map_err(|_| Ava1Error::Timeout)??;
    let welcomed = Instant::now();
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
        ctx.notify_limited(&req);
    }
    let (tx, mut rx) = mpsc::channel(DELIVER_DEPTH);
    let (link, outbox) = drive(r, w, ctx.timing, tx);
    let rpc_slots = Arc::new(tokio::sync::Semaphore::new(RPC_WORKERS));
    // Replies from this loop are only ever queued, never awaited: a peer that sends
    // requests but stops reading fills the queue and is disconnected (`reply`), and the
    // writer task ends the link once it has taken nothing for `dead_after`.
    let reply = |channel: u32, status: u16| -> bool {
        let r = RpcResponse {
            status,
            body: Vec::new(),
        };
        outbox.try_send(channel, &r).is_ok()
    };
    let mut check = tokio::time::interval(ctx.timing.ping_every);
    loop {
        let f = tokio::select! {
            f = rx.recv() => match f {
                Some(f) => f,
                None => break,
            },
            _ = check.tick() => {
                // An unconfirmed session is only useful while it can still be confirmed:
                // it ends with the pairing window, or after the confirm deadline.
                if unpaired_slot.is_some()
                    && (!ctx.pairing_open() || welcomed.elapsed() > ctx.limits.pair_confirm)
                {
                    refuse_on(&outbox, gen::ERR_PAIRING_CLOSED, "pairing was not confirmed in time").await;
                    break;
                }
                continue;
            }
        };
        match f.ty {
            RpcRequest::TYPE => {
                let Ok(q) = f.decode::<RpcRequest>() else {
                    refuse_on(&outbox, gen::ERR_PROTOCOL, "bad RpcRequest").await;
                    break;
                };
                let channel = f.channel;
                if !entry.paired.load(Ordering::SeqCst) {
                    if !reply(channel, gen::ERR_NOT_PAIRED) {
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
                    if !reply(channel, status) {
                        break;
                    }
                    continue;
                }
                // Calls run on workers; the reader (and so liveness) never waits for one.
                let Ok(permit) = rpc_slots.clone().try_acquire_owned() else {
                    if !reply(channel, gen::ERR_BUSY) {
                        break;
                    }
                    continue;
                };
                let (ctx, outbox) = (ctx.clone(), outbox.clone());
                tokio::spawn(async move {
                    let reply = tokio::task::spawn_blocking(move || (ctx.rpc)(q.method, &q.body))
                        .await
                        .unwrap_or(RpcReply {
                            status: gen::ERR_INTERNAL,
                            body: Vec::new(),
                        });
                    // Waits for room (bounded: a stuck peer ends the link, which fails this).
                    let _ = outbox
                        .send(
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
                let already = entry.paired.load(Ordering::SeqCst);
                let accepted = already
                    || (ctx.pairing_open() && (ctx.approve)(&req) && {
                        let stored = ctx.peers.lock().unwrap().add(req.peer_key, &req.peer_name);
                        if let Err(e) = &stored {
                            (ctx.log)(&format!("ava1: pairing not stored: {e}"));
                        }
                        stored.is_ok()
                    });
                entry.paired.store(accepted, Ordering::SeqCst);
                if accepted && !already {
                    // One window, one pairing: whoever else is waiting must ask again.
                    ctx.close_pairing();
                    unpaired_slot = None;
                }
                let result = PairResult {
                    accepted: u8::from(accepted),
                };
                if outbox.try_send(f.channel, &result).is_err() || !accepted {
                    outbox.flush(FAREWELL).await;
                    break;
                }
            }
            _ if f.ignorable() => {}
            _ => {
                refuse_on(&outbox, gen::ERR_PROTOCOL, "unexpected frame").await;
                break;
            }
        }
    }
    ctx.sessions.lock().unwrap().remove(&est.session_id);
    drop(link);
    Ok(())
}

/// The longest a connection's last words (an Error, a PairResult) may take to leave.
const FAREWELL: Duration = Duration::from_secs(1);

/// Writes `Error{code, message}` on a connection not yet handed to a link, giving up at
/// the handshake deadline.
async fn refuse_by(
    deadline: tokio::time::Instant,
    w: &mut FrameWriter<OwnedWriteHalf>,
    code: u16,
    message: &str,
) {
    let _ = tokio::time::timeout_at(deadline, refuse(w, code, message)).await;
}

/// Queues `Error{code, message}` and gives it up to `FAREWELL` to go out.
async fn refuse_on(outbox: &Outbox, code: u16, message: &str) {
    let e = gen::Error {
        code,
        message: message.into(),
    };
    if outbox.try_send(0, &e) != Err(Full::Closed) {
        outbox.flush(FAREWELL).await;
    }
}

/// The nonces a session remembers, so a captured Join cannot be replayed.
const JOIN_NONCES: usize = 64;

async fn lane(
    mut r: FrameReader<OwnedReadHalf>,
    mut w: FrameWriter<OwnedWriteHalf>,
    first: Frame,
    ctx: &Arc<ServerCtx>,
    deadline: tokio::time::Instant,
) -> Result<(), Ava1Error> {
    let j: Join = first.decode()?;
    let entry = ctx.sessions.lock().unwrap().get(&j.session_id).cloned();
    let refused = |code: u16| Ava1Error::Refused {
        code,
        message: "join refused".into(),
    };
    let Some(entry) = entry else {
        refuse_by(deadline, &mut w, gen::ERR_BAD_JOIN, "unknown session").await;
        return Err(refused(gen::ERR_BAD_JOIN));
    };
    let want = keys::join_tag(&entry.keys.c2s, &j.session_id, j.lane_id, &j.client_nonce);
    let lane_ok = (1..=gen::MAX_LANES as u16).contains(&j.lane_id);
    if !lane_ok || !keys::ct_eq16(&want, &j.tag) {
        refuse_by(deadline, &mut w, gen::ERR_BAD_JOIN, "join refused").await;
        return Err(refused(gen::ERR_BAD_JOIN));
    }
    let fresh = {
        let mut n = entry.nonces.lock().unwrap();
        let fresh = !n.contains(&j.client_nonce);
        if fresh {
            if n.len() >= JOIN_NONCES {
                n.pop_front();
            }
            n.push_back(j.client_nonce);
        }
        fresh
    };
    if !fresh {
        refuse_by(deadline, &mut w, gen::ERR_BAD_JOIN, "join replayed").await;
        return Err(refused(gen::ERR_BAD_JOIN));
    }
    if !entry.paired.load(Ordering::SeqCst) {
        refuse_by(deadline, &mut w, gen::ERR_NOT_PAIRED, "pair first").await;
        return Err(Ava1Error::NotPaired);
    }
    let lane = j.lane_id as usize;
    let gen_no = {
        let mut g = entry.lane_gen.lock().unwrap();
        g[lane] += 1;
        g[lane]
    };
    // A fresh server nonce per join: even a replayed Join (one older than the nonce
    // window) gets keys never used before, so no (key, counter) pair repeats.
    let server_nonce: [u8; 16] = keys::random_bytes()?;
    let (cn, sn) = (j.client_nonce, server_nonce);
    let tag = keys::join_ack_tag(&entry.keys.s2c, &j.session_id, j.lane_id, &cn, &sn);
    let ack = JoinAck {
        lane_id: j.lane_id,
        server_nonce,
        tag,
    };
    tokio::time::timeout_at(deadline, w.send_msg(0, &ack))
        .await
        .map_err(|_| Ava1Error::Timeout)??;
    r.set_key(keys::lane_key(&entry.keys.c2s, j.lane_id, &cn, &sn));
    w.set_key(keys::lane_key(&entry.keys.s2c, j.lane_id, &cn, &sn));
    let (tx, mut rx) = mpsc::channel(DELIVER_DEPTH);
    let (link, outbox) = drive(r, w, ctx.timing, tx);
    loop {
        tokio::select! {
            // Project 1 lanes carry heartbeats only (handled by the link): anything else
            // that is not marked ignorable is a protocol error.
            f = rx.recv() => match f {
                None => break,
                Some(f) if f.ignorable() => {}
                Some(_) => {
                    refuse_on(&outbox, gen::ERR_PROTOCOL, "unexpected frame on a lane").await;
                    break;
                }
            },
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
