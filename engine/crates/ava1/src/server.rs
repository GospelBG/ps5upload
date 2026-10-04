//! The server side: accept loop, control connections, pairing, RPC (SPEC.md §6–§8).
use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};

use crate::conn::{Frame, FrameReader, FrameWriter};
use crate::gen::{
    self, Hs1, Join, JoinAck, PairConfirm, PairResult, Ping, Pong, RpcRequest, RpcResponse,
};
use crate::handshake::{self, refuse, Admission};
use crate::keys::{self, Identity, SessionKeys};
use crate::launch::LaunchSecret;
use crate::link::{drive, Full, Outbox, DELIVER_DEPTH};
use crate::peers::PeerStore;
use crate::router::{is_data_type, job_of, ConnTx, JobHost, JobLink, Router};
use crate::session::{RpcReply, Timing};
use crate::wire::{FrameMessage, Message};
use crate::Ava1Error;

pub const MAX_CONNS: usize = 64;
pub const MAX_SESSIONS: usize = 16;
/// Calls in flight per session; more are answered `ERR_BUSY`.
pub const RPC_WORKERS: usize = 8;
pub use crate::frame::{RPC_REPLY_MAX, RPC_REQUEST_MAX};
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
    peer_key: [u8; 32],
    pub(crate) paired: AtomicBool,
    pub(crate) router: Arc<Router>,
    /// Per lane id, how many connections have taken it over. A lane connection ends when
    /// its number is no longer the current one.
    lane_gen: watch::Sender<[u32; 9]>,
    pub(crate) nonces: Mutex<VecDeque<[u8; 16]>>,
    /// True once the session is over (its control connection ended, or the same device
    /// connected again): the control connection and every lane end at once.
    ended: watch::Sender<bool>,
    /// The session's connections, so their per-address counts can be given back the
    /// moment the session is superseded, before their tasks have wound down.
    conns: Mutex<Vec<Weak<ConnSlot>>>,
    /// Held while the session is welcomed but unconfirmed.
    unpaired: Mutex<Option<UnpairedSlot>>,
}

impl SessionEntry {
    fn adopt(&self, slot: &Arc<ConnSlot>) {
        let mut conns = self.conns.lock().unwrap();
        conns.retain(|c| c.strong_count() > 0);
        conns.push(Arc::downgrade(slot));
    }

    /// Ends the session now: wakes its connections' tasks and frees what it held.
    fn end(&self) {
        self.ended.send_replace(true);
        for c in self.conns.lock().unwrap().drain(..) {
            if let Some(c) = c.upgrade() {
                c.release_ip();
            }
        }
        self.unpaired.lock().unwrap().take();
    }
}

/// Resolves once the session is over.
async fn over(ended: &mut watch::Receiver<bool>) {
    let _ = ended.wait_for(|e| *e).await;
}

/// Resolves once `lane` has been taken over by a connection newer than `gen_no`.
async fn taken_over(gens: &mut watch::Receiver<[u32; 9]>, lane: usize, gen_no: u32) {
    let _ = gens.wait_for(|g| g[lane] != gen_no).await;
}

pub struct ServerCtx {
    identity: Identity,
    name: String,
    /// The key in this node's trust slot (SPEC.md §5.1): known without pairing.
    launcher: Option<[u8; 32]>,
    /// The slot's launch token, if it carried one (SPEC.md §5.2).
    launch: Option<LaunchSecret>,
    timing: Timing,
    limits: Limits,
    peers: Mutex<PeerStore>,
    pairing_until: Mutex<Option<Instant>>,
    notify: NotifyHook,
    last_notify: Mutex<Option<Instant>>,
    log: LogHook,
    approve: PairHook,
    rpc: RpcHandler,
    /// Hosts data-plane jobs (SPEC.md §11); its presence advertises CAP_DATA_PLANE.
    jobs: Option<Arc<dyn JobHost>>,
    /// Serves the management methods (SPEC.md §7.3); advertises CAP_MGMT.
    mgmt: bool,
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
            launcher: None,
            launch: None,
            timing: Timing::default(),
            limits: Limits::default(),
            peers: Mutex::new(peers),
            pairing_until: Mutex::new(None),
            notify: Box::new(|_| {}),
            last_notify: Mutex::new(None),
            log: Box::new(|_| {}),
            approve: Box::new(|_| true),
            rpc,
            jobs: None,
            mgmt: false,
            sessions: Mutex::default(),
            conns: AtomicUsize::new(0),
            per_ip: Mutex::default(),
            session_slots: AtomicUsize::new(0),
            unpaired: AtomicUsize::new(0),
        }
    }

    /// Trusts `key` without pairing, as a payload trusts the key stamped into its trust
    /// slot (SPEC.md §5.1).
    pub fn with_launcher(mut self, key: [u8; 32]) -> Self {
        self.launcher = Some(key);
        self
    }

    /// `with_launcher`, for a slot that also carried a launch `token`: the launcher's
    /// Welcome then proves the token (SPEC.md §5.2).
    pub fn with_launch(mut self, key: [u8; 32], token: [u8; 16]) -> Self {
        self.launcher = Some(key);
        self.launch = Some(LaunchSecret { key, token });
        self
    }

    /// The launcher (the key from the trust slot) is known without the peers file: this
    /// server was handed that key by the code that launched it. The C payload keeps the
    /// same trust in its peers file instead (SPEC.md §5.1), so a peers write that fails
    /// leaves its launcher refused while this one still admits it — they differ only in
    /// that case, and only this harness's favour.
    fn is_known(&self, key: &[u8; 32]) -> bool {
        self.launcher.as_ref() == Some(key) || self.peers.lock().unwrap().contains(key)
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

    /// Serves the management methods through the rpc handler and advertises CAP_MGMT.
    pub fn with_mgmt(mut self) -> Self {
        self.mgmt = true;
        self
    }

    /// Hosts data-plane jobs on this server and advertises CAP_DATA_PLANE.
    pub fn with_jobs(mut self, host: Arc<dyn JobHost>) -> Self {
        self.jobs = Some(host);
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

    /// One session per device (SPEC.md §8): a device that completes a new handshake
    /// replaces whatever session it had. A client reconnecting after its link died must
    /// not be refused because of its own dead connections, which linger until
    /// `dead_after`: they are ended here and their per-address counts given back at
    /// once.
    fn supersede(&self, peer_key: &[u8; 32]) {
        let old: Vec<Arc<SessionEntry>> = {
            let mut map = self.sessions.lock().unwrap();
            let ids: Vec<[u8; 16]> = map
                .iter()
                .filter(|(_, e)| &e.peer_key == peer_key)
                .map(|(id, _)| *id)
                .collect();
            ids.iter().filter_map(|id| map.remove(id)).collect()
        };
        for e in old {
            e.end();
        }
    }

    /// Decides a PairConfirm. One window, one pairing: the window check, the store and
    /// the closing of the window happen under one lock, so two devices confirming at the
    /// same moment cannot both get in. The owner's approval (which may wait on a person)
    /// is asked first, outside the lock, and the window checked again after it.
    fn accept_pairing(&self, req: &PairRequest) -> bool {
        if !self.pairing_open() || !(self.approve)(req) {
            return false;
        }
        let mut until = self.pairing_until.lock().unwrap();
        if !until.is_some_and(|t| Instant::now() < t) {
            return false;
        }
        let stored = self.peers.lock().unwrap().add(req.peer_key, &req.peer_name);
        match stored {
            Ok(()) => {
                // Whoever else is waiting must ask again.
                *until = None;
                true
            }
            Err(e) => {
                (self.log)(&format!("ava1: pairing not stored: {e}"));
                false
            }
        }
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
    /// Still counted against its address. Cleared early when its session is superseded.
    ip_held: AtomicBool,
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
            ip_held: AtomicBool::new(true),
        })
    }

    /// Gives back this connection's place in its address's count (once).
    fn release_ip(&self) {
        if !self.ip_held.swap(false, Ordering::SeqCst) {
            return;
        }
        let mut per_ip = self.ctx.per_ip.lock().unwrap();
        if let Some(n) = per_ip.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                per_ip.remove(&self.ip);
            }
        }
    }
}

impl Drop for ConnSlot {
    fn drop(&mut self) {
        self.release_ip();
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
        let (ctx, slot) = (ctx.clone(), Arc::new(slot));
        tokio::spawn(async move {
            let _ = handle(s, &ctx, &slot).await;
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

async fn handle(s: TcpStream, ctx: &Arc<ServerCtx>, slot: &Arc<ConnSlot>) -> Result<(), Ava1Error> {
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
        Hs1::TYPE => control(r, w, first, ctx, slot, deadline).await,
        Join::TYPE => lane(r, w, first, ctx, slot, deadline).await,
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
    slot: &Arc<ConnSlot>,
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
        handshake::server_launched(
            &mut r,
            &mut w,
            first,
            &ctx.identity,
            &ctx.name,
            ctx.launch.as_ref(),
            |k| {
                // Message 3 has just proved the client holds `k`. Any session that key still
                // has is replaced, before the limits below (and before Welcome, so the
                // client's lanes find the old connections' counts already given back).
                if ctx.is_known(k) {
                    ctx.supersede(k);
                    Admission::Known
                } else if !ctx.pairing_open() {
                    Admission::Refuse(
                        gen::ERR_PAIRING_CLOSED,
                        "this device is not paired and pairing is closed",
                    )
                } else {
                    ctx.supersede(k);
                    match UnpairedSlot::take(ctx) {
                        Some(slot) => {
                            unpaired_slot = Some(slot);
                            Admission::Pairing
                        }
                        None => Admission::Refuse(gen::ERR_BUSY, "too many devices are pairing"),
                    }
                }
            },
            (if ctx.jobs.is_some() {
                gen::CAP_DATA_PLANE
            } else {
                0
            }) | (if ctx.mgmt { gen::CAP_MGMT } else { 0 }),
        ),
    )
    .await
    .map_err(|_| Ava1Error::Timeout)??;
    let welcomed = Instant::now();
    let req = PairRequest {
        peer_key: est.peer_key,
        peer_name: est.peer_name.clone(),
        code: est.code,
    };
    let entry = Arc::new(SessionEntry {
        keys: est.keys.clone(),
        peer_key: est.peer_key,
        paired: AtomicBool::new(est.pairing.is_none()),
        router: Arc::new(Router::default()),
        lane_gen: watch::Sender::new([0; 9]),
        nonces: Mutex::new(VecDeque::new()),
        ended: watch::Sender::new(false),
        conns: Mutex::default(),
        unpaired: Mutex::new(unpaired_slot),
    });
    entry.adopt(slot);
    let mut ended = entry.ended.subscribe();
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
            // The same device connected again: this session is the old one.
            _ = over(&mut ended) => break,
            _ = check.tick() => {
                // An unconfirmed session is only useful while it can still be confirmed:
                // it ends with the pairing window, or after the confirm deadline.
                if !entry.paired.load(Ordering::SeqCst)
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
                if q.body.len() > RPC_REQUEST_MAX {
                    let r = RpcResponse {
                        status: gen::ERR_PROTOCOL,
                        body: b"request exceeds the 56 KiB RPC cap".to_vec(),
                    };
                    if outbox.try_send(channel, &r).is_err() {
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
                    let reply = if reply.body.len() > RPC_REPLY_MAX {
                        RpcReply {
                            status: gen::ERR_INTERNAL,
                            body: b"reply exceeds the 256 KiB RPC cap".to_vec(),
                        }
                    } else {
                        reply
                    };
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
                let accepted = already || ctx.accept_pairing(&req);
                entry.paired.store(accepted, Ordering::SeqCst);
                if accepted && !already {
                    entry.unpaired.lock().unwrap().take();
                }
                let result = PairResult {
                    accepted: u8::from(accepted),
                };
                if outbox.try_send(f.channel, &result).is_err() || !accepted {
                    outbox.flush(FAREWELL).await;
                    break;
                }
            }
            t if is_data_type(t) => {
                // A JobOpen for a job id that is still registered (the sender cancelled it a
                // moment ago and is resuming it on this session) must not be routed to the old
                // job: its receiver is draining what the old lanes buffered, ends on the
                // JobCancel queued before this frame, and drops whatever is queued behind it,
                // so the open would never be answered. BUSY now; the sender retries.
                if f.ty == gen::JobOpen::TYPE {
                    if let Some(job) = job_of(&f).filter(|j| entry.router.has_job(j)) {
                        let busy = gen::JobOpenAck {
                            job_id: job,
                            status: gen::ERR_BUSY,
                            credit: 0,
                            staged: 0,
                            workers: 0,
                            message: Some("the job's previous run is still closing".into()),
                        };
                        if outbox.try_send(f.channel, &busy).is_err() {
                            break;
                        }
                        continue;
                    }
                }
                // A known job's frames were delivered by the router; what comes back is
                // for no job at all.
                if let Some(f) = entry.router.route_control(f).await {
                    let opens = f.ty == gen::JobOpen::TYPE || f.ty == gen::Resume::TYPE;
                    if let (Some(host), true, Some(job), true) = (
                        &ctx.jobs,
                        opens,
                        job_of(&f),
                        entry.paired.load(Ordering::SeqCst),
                    ) {
                        let link = JobLink::new(
                            job,
                            entry.router.clone(),
                            ConnTx::new(outbox.clone()),
                            None,
                        );
                        host.accept(link, f, est.peer_key);
                    }
                    // Anything else for an unknown job is a late frame: dropped.
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
    // Jobs hear about the session's end; lanes end with the session, at once.
    entry.router.close("the session ended");
    entry.end();
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
    slot: &Arc<ConnSlot>,
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
    entry.adopt(slot);
    let lane = j.lane_id as usize;
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
    // A Join can be captured and sent again by someone who does not hold the session
    // keys. So this connection takes the lane over — ending an older connection of the
    // same lane id — only once its first sealed frame has opened under the new lane
    // key. Until then the older connection is left alone, and its bodies stay capped at
    // the control size: a forged Join must not buy a 16 MiB buffer per attempt.
    let proof = tokio::time::timeout_at(deadline, r.recv())
        .await
        .map_err(|_| Ava1Error::Timeout)??;
    // Only now, with the lane key proven, may frames be as large as the frame cap.
    r.set_max_body(crate::frame::MAX_BODY);
    let mut gen_no = 0;
    entry.lane_gen.send_modify(|g| {
        g[lane] += 1;
        gen_no = g[lane];
    });
    let (mut gens, mut ended) = (entry.lane_gen.subscribe(), entry.ended.subscribe());
    let (tx, mut rx) = mpsc::channel(DELIVER_DEPTH);
    let (link, outbox) = drive(r, w, ctx.timing, tx);
    let lane_gen = entry.router.lane_up(j.lane_id, outbox.clone());
    // The proving frame is a frame like any other (a client sends a Ping).
    let mut first = Some(proof);
    loop {
        let f = match first.take() {
            Some(f) => f,
            None => tokio::select! {
                f = rx.recv() => match f {
                    Some(f) => f,
                    None => break,
                },
                _ = taken_over(&mut gens, lane, gen_no) => break,
                _ = over(&mut ended) => break,
            },
        };
        match f.ty {
            Ping::TYPE => {
                let Ok(p) = f.decode::<Ping>() else {
                    refuse_on(&outbox, gen::ERR_PROTOCOL, "malformed Ping").await;
                    break;
                };
                let pong = Pong {
                    seq: p.seq,
                    t_us: p.t_us,
                };
                if outbox.try_send(0, &pong) == Err(Full::Closed) {
                    break;
                }
            }
            Pong::TYPE => {}
            gen::Bye::TYPE | gen::Error::TYPE => break,
            t if is_data_type(t) => entry.router.route_lane(j.lane_id, f).await,
            _ if f.ignorable() => {}
            _ => {
                refuse_on(&outbox, gen::ERR_PROTOCOL, "unexpected frame on a lane").await;
                break;
            }
        }
    }
    entry.router.lane_down(j.lane_id, lane_gen);
    drop(link);
    Ok(())
}
