//! Data-plane routing (SPEC.md §11.1, §12.1). Every data-plane message starts with its
//! 16-byte job id, so a frame reaches its job without a full decode.
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::conn::Frame;
use crate::frame::FLAG_IGNORABLE;
use crate::link::Outbox;
use crate::wire::FrameMessage;
use crate::Ava1Error;

pub type JobId = [u8; 16];
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub fn is_data_type(ty: u8) -> bool {
    (0x20..=0x3F).contains(&ty)
}

pub fn job_of(f: &Frame) -> Option<JobId> {
    f.body.get(..16).and_then(|b| b.try_into().ok())
}

#[derive(Debug)]
pub enum Inbound {
    Control(Frame),
    Lane { lane: u16, frame: Frame },
    LaneUp(u16),
    LaneDown(u16),
    Closed(String),
}

/// Sends on one connection. Every send goes through the connection's writer queue,
/// whole or not at all, so a caller that drops the future (a timeout, a `select!`)
/// never leaves half a sealed frame on the wire, and a full queue (the peer not
/// taking bytes) is the backpressure.
#[derive(Clone)]
pub struct ConnTx {
    outbox: Outbox,
}

impl ConnTx {
    pub(crate) fn new(outbox: Outbox) -> Self {
        Self { outbox }
    }

    pub async fn send_raw(
        &self,
        ty: u8,
        flags: u8,
        channel: u32,
        body: Vec<u8>,
    ) -> Result<(), Ava1Error> {
        self.outbox.send_frame(ty, flags, channel, body).await
    }

    pub async fn send<M: FrameMessage>(&self, m: &M) -> Result<(), Ava1Error> {
        self.send_raw(M::TYPE, 0, 0, m.to_bytes()?).await
    }

    /// A receiver that does not know the type skips it (Status).
    pub async fn send_ignorable<M: FrameMessage>(&self, m: &M) -> Result<(), Ava1Error> {
        self.send_raw(M::TYPE, FLAG_IGNORABLE, 0, m.to_bytes()?)
            .await
    }
}

#[derive(Clone)]
pub struct LaneTx {
    pub id: u16,
    gen: u64,
    pub tx: ConnTx,
}

/// Opens and closes lanes for jobs. Only the dialling side has one.
pub trait LaneOpener: Send + Sync {
    fn open(&self) -> BoxFut<'_, Result<u16, Ava1Error>>;
    fn close(&self, id: u16);
}

/// Hands a job's frames to it; runs inside a server when a job arrives for nobody.
pub trait JobHost: Send + Sync {
    /// `first` is the JobOpen or Resume that created the job; `peer` is the static key
    /// of the device that sent it (a job is bound to it, SPEC.md §11.1).
    fn accept(&self, link: JobLink, first: Frame, peer: [u8; 32]);
}

#[derive(Default)]
pub struct Router {
    jobs: Mutex<HashMap<JobId, mpsc::UnboundedSender<Inbound>>>,
    lanes: Mutex<BTreeMap<u16, LaneTx>>,
    next_gen: AtomicU64,
    closed: Mutex<Option<String>>,
}

impl Router {
    /// A job's inbox. It first hears about lanes that are already up.
    pub fn register(&self, job: JobId) -> mpsc::UnboundedReceiver<Inbound> {
        let (tx, rx) = mpsc::unbounded_channel();
        if let Some(why) = self.closed.lock().unwrap().clone() {
            let _ = tx.send(Inbound::Closed(why));
        }
        for id in self.lanes.lock().unwrap().keys() {
            let _ = tx.send(Inbound::LaneUp(*id));
        }
        self.jobs.lock().unwrap().insert(job, tx);
        rx
    }

    pub fn unregister(&self, job: &JobId) {
        self.jobs.lock().unwrap().remove(job);
    }

    pub fn has_job(&self, job: &JobId) -> bool {
        self.jobs.lock().unwrap().contains_key(job)
    }

    /// Gives the frame back when no job is registered for it (the caller decides).
    pub fn route_control(&self, f: Frame) -> Option<Frame> {
        let job = job_of(&f)?;
        let jobs = self.jobs.lock().unwrap();
        match jobs.get(&job) {
            Some(tx) => {
                let _ = tx.send(Inbound::Control(f));
                None
            }
            None => Some(f),
        }
    }

    /// Lane frames for unknown jobs are dropped: credit only exists inside a job.
    pub fn route_lane(&self, lane: u16, f: Frame) {
        if let Some(job) = job_of(&f) {
            if let Some(tx) = self.jobs.lock().unwrap().get(&job) {
                let _ = tx.send(Inbound::Lane { lane, frame: f });
            }
        }
    }

    pub(crate) fn lane_up(&self, id: u16, outbox: Outbox) -> u64 {
        let gen = self.next_gen.fetch_add(1, Ordering::Relaxed) + 1;
        self.lanes.lock().unwrap().insert(
            id,
            LaneTx {
                id,
                gen,
                tx: ConnTx::new(outbox),
            },
        );
        self.broadcast(|| Inbound::LaneUp(id));
        gen
    }

    /// Only the generation that came up is taken down (a lane id can be re-joined).
    pub(crate) fn lane_down(&self, id: u16, gen: u64) {
        let removed = {
            let mut l = self.lanes.lock().unwrap();
            if l.get(&id).is_some_and(|t| t.gen == gen) {
                l.remove(&id);
                true
            } else {
                false
            }
        };
        if removed {
            self.broadcast(|| Inbound::LaneDown(id));
        }
    }

    pub fn lanes(&self) -> Vec<LaneTx> {
        self.lanes.lock().unwrap().values().cloned().collect()
    }

    pub fn lane(&self, id: u16) -> Option<LaneTx> {
        self.lanes.lock().unwrap().get(&id).cloned()
    }

    pub(crate) fn close(&self, why: &str) {
        {
            let mut c = self.closed.lock().unwrap();
            if c.is_some() {
                return;
            }
            *c = Some(why.to_string());
        }
        self.lanes.lock().unwrap().clear();
        self.broadcast(|| Inbound::Closed(why.to_string()));
    }

    fn broadcast(&self, ev: impl Fn() -> Inbound) {
        for tx in self.jobs.lock().unwrap().values() {
            let _ = tx.send(ev());
        }
    }
}

/// What a job task holds: its inbox, the control connection, and the session's lanes.
pub struct JobLink {
    pub job_id: JobId,
    pub rx: mpsc::UnboundedReceiver<Inbound>,
    pub control: ConnTx,
    router: Arc<Router>,
    opener: Option<Arc<dyn LaneOpener>>,
}

impl JobLink {
    pub(crate) fn new(
        job_id: JobId,
        router: Arc<Router>,
        control: ConnTx,
        opener: Option<Arc<dyn LaneOpener>>,
    ) -> Self {
        let rx = router.register(job_id);
        Self {
            job_id,
            rx,
            control,
            router,
            opener,
        }
    }

    pub fn lanes(&self) -> Vec<LaneTx> {
        self.router.lanes()
    }

    pub fn lane(&self, id: u16) -> Option<LaneTx> {
        self.router.lane(id)
    }

    pub fn opener(&self) -> Option<&Arc<dyn LaneOpener>> {
        self.opener.as_ref()
    }
}

impl Drop for JobLink {
    fn drop(&mut self) {
        self.router.unregister(&self.job_id);
    }
}
