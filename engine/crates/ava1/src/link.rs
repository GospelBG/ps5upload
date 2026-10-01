//! Runs one connection after its handshake: answers Ping, records RTT from Pong,
//! sends heartbeats, declares the peer dead after `dead_after` without a byte, and
//! hands every other frame to the owner (SPEC.md §6).
//!
//! Every frame goes out through one writer task fed by a bounded queue (`Outbox`), so
//! no reader ever waits on a socket write, and enqueueing is cancel-safe: a frame is
//! either queued whole or not at all, and only the writer task touches the stream.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::AbortHandle;

use crate::conn::{now_us, Frame, FrameReader, FrameWriter, Pace};
use crate::gen::{self, Bye, Ping, Pong};
use crate::session::Timing;
use crate::wire::FrameMessage;
use crate::Ava1Error;

/// Frames waiting for the writer, per connection. A full queue means the peer is not
/// taking our bytes; the writer's own stall limit then ends the connection.
pub(crate) const OUTBOX_DEPTH: usize = 64;
/// Frames waiting for the owner, per connection. A full queue stops the reader, which
/// pushes back on the peer through TCP.
pub(crate) const DELIVER_DEPTH: usize = 64;
/// The default slowest one frame may arrive or leave (after a `dead_after` grace) before
/// the peer counts as gone: slow links are fine, a peer dripping a frame forever is not.
pub(crate) const MIN_FRAME_RATE: u32 = 8 * 1024;

enum Out {
    Frame { ty: u8, channel: u32, body: Vec<u8> },
    Flush(oneshot::Sender<()>),
}

/// The sending side of a connection.
#[derive(Clone)]
pub(crate) struct Outbox {
    tx: mpsc::Sender<Out>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Full {
    /// The queue is full: the peer is not reading.
    Full,
    /// The connection has ended.
    Closed,
}

impl Outbox {
    /// Queues a frame, waiting for room. Cancel-safe: dropped before it completes, the
    /// frame was not queued.
    pub(crate) async fn send<M: FrameMessage>(&self, channel: u32, m: &M) -> Result<(), Ava1Error> {
        let body = m.to_bytes()?;
        self.tx
            .send(Out::Frame {
                ty: M::TYPE,
                channel,
                body,
            })
            .await
            .map_err(|_| Ava1Error::Lost("the connection has ended".into()))
    }

    /// Queues a frame only if there is room right now.
    pub(crate) fn try_send<M: FrameMessage>(&self, channel: u32, m: &M) -> Result<(), Full> {
        let body = m.to_bytes().map_err(|_| Full::Closed)?;
        self.tx
            .try_send(Out::Frame {
                ty: M::TYPE,
                channel,
                body,
            })
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => Full::Full,
                mpsc::error::TrySendError::Closed(_) => Full::Closed,
            })
    }

    /// Nothing is waiting to be written.
    pub(crate) fn is_idle(&self) -> bool {
        self.tx.capacity() == self.tx.max_capacity()
    }

    /// Waits (at most `limit`) until everything queued before this call is on the wire.
    /// Never hangs on a stuck peer: past `limit` it gives up and returns false.
    pub(crate) async fn flush(&self, limit: Duration) -> bool {
        tokio::time::timeout(limit, async {
            let (tx, rx) = oneshot::channel();
            self.tx.send(Out::Flush(tx)).await.ok()?;
            rx.await.ok()
        })
        .await
        .ok()
        .flatten()
        .is_some()
    }
}

type Close = Arc<dyn Fn(String) + Send + Sync>;

pub(crate) struct Link {
    closed: watch::Receiver<Option<String>>,
    rtt_us: Arc<AtomicU64>,
    tasks: Arc<Mutex<Vec<AbortHandle>>>,
}

impl Drop for Link {
    fn drop(&mut self) {
        for t in self.tasks.lock().unwrap().drain(..) {
            t.abort();
        }
    }
}

impl Link {
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.borrow().is_some()
    }

    pub(crate) fn reason(&self) -> String {
        self.closed
            .borrow()
            .clone()
            .unwrap_or_else(|| "closed".into())
    }

    /// Waits until the connection ends; returns why.
    pub(crate) async fn closed(&self) -> String {
        let mut rx = self.closed.clone();
        loop {
            if let Some(r) = rx.borrow_and_update().clone() {
                return r;
            }
            if rx.changed().await.is_err() {
                return "closed".into();
            }
        }
    }

    pub(crate) fn rtt(&self) -> Option<Duration> {
        match self.rtt_us.load(Ordering::Relaxed) {
            0 => None,
            us => Some(Duration::from_micros(us)),
        }
    }
}

pub(crate) fn drive(
    mut reader: FrameReader<OwnedReadHalf>,
    mut writer: FrameWriter<OwnedWriteHalf>,
    timing: Timing,
    deliver: mpsc::Sender<Frame>,
) -> (Link, Outbox) {
    let (tx, rx) = watch::channel(None::<String>);
    let tasks: Arc<Mutex<Vec<AbortHandle>>> = Arc::default();
    let last_rx = Arc::new(AtomicU64::new(now_us()));
    let rtt = Arc::new(AtomicU64::new(0));
    let pace = Pace {
        idle: timing.dead_after,
        min_rate: timing.min_frame_rate,
    };
    reader.set_pace(last_rx.clone(), pace);
    writer.set_pace(pace);
    let (out_tx, mut out_rx) = mpsc::channel::<Out>(OUTBOX_DEPTH);
    let outbox = Outbox { tx: out_tx };

    // Ends the connection: every task is aborted, which drops both socket halves.
    let close: Close = {
        let tasks = tasks.clone();
        Arc::new(move |why: String| {
            let first = tx.send_if_modified(|v| {
                if v.is_some() {
                    return false;
                }
                *v = Some(why.clone());
                true
            });
            if !first {
                return;
            }
            for t in tasks.lock().unwrap().drain(..) {
                t.abort();
            }
        })
    };

    let writer_task = {
        let close = close.clone();
        tokio::spawn(async move {
            while let Some(o) = out_rx.recv().await {
                match o {
                    Out::Frame { ty, channel, body } => {
                        if let Err(e) = writer.send(ty, channel, &body).await {
                            let why = match e {
                                Ava1Error::Timeout => format!(
                                    "the other device stopped taking data ({} ms without progress)",
                                    timing.dead_after.as_millis()
                                ),
                                e => format!("write failed: {e}"),
                            };
                            return close(why);
                        }
                    }
                    Out::Flush(ack) => {
                        let _ = ack.send(());
                    }
                }
            }
        })
    };

    let reader_task = {
        let (outbox, close, rtt) = (outbox.clone(), close.clone(), rtt.clone());
        tokio::spawn(async move {
            loop {
                let f = match reader.recv().await {
                    Ok(f) => f,
                    Err(e) => return close(e.to_string()),
                };
                match f.ty {
                    Ping::TYPE => {
                        let Ok(p) = f.decode::<Ping>() else {
                            return close("malformed Ping".into());
                        };
                        // A full queue means data is on its way out, which proves we are
                        // alive just as well; a stuck writer ends the link on its own.
                        let pong = Pong {
                            seq: p.seq,
                            t_us: p.t_us,
                        };
                        if outbox.try_send(0, &pong) == Err(Full::Closed) {
                            return close("session ended".into());
                        }
                    }
                    Pong::TYPE => {
                        if let Ok(p) = f.decode::<Pong>() {
                            rtt.store(now_us().saturating_sub(p.t_us).max(1), Ordering::Relaxed);
                        }
                    }
                    Bye::TYPE => return close("the other device closed the session".into()),
                    gen::Error::TYPE => {
                        let why = match f.decode::<gen::Error>() {
                            Ok(e) => {
                                format!("the other device reported error {}: {}", e.code, e.message)
                            }
                            Err(_) => "the other device reported an error".into(),
                        };
                        return close(why);
                    }
                    _ => {
                        if deliver.send(f).await.is_err() {
                            return close("session ended".into());
                        }
                    }
                }
            }
        })
    };

    let heartbeat_task = {
        let (outbox, close, last_rx) = (outbox.clone(), close.clone(), last_rx.clone());
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(timing.ping_every);
            iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut seq = 0u32;
            loop {
                iv.tick().await;
                // Any byte counts: a large frame still arriving is proof of life.
                let quiet =
                    Duration::from_micros(now_us().saturating_sub(last_rx.load(Ordering::Relaxed)));
                if quiet > timing.dead_after {
                    return close(format!(
                        "the other device stopped answering ({} ms without a byte)",
                        quiet.as_millis()
                    ));
                }
                seq = seq.wrapping_add(1);
                // Frames already queued are proof of life for the peer: skip the Ping.
                if outbox.is_idle() {
                    let ping = Ping {
                        seq,
                        t_us: now_us(),
                    };
                    if outbox.try_send(0, &ping) == Err(Full::Closed) {
                        return close("session ended".into());
                    }
                }
            }
        })
    };

    tasks.lock().unwrap().extend([
        writer_task.abort_handle(),
        reader_task.abort_handle(),
        heartbeat_task.abort_handle(),
    ]);
    // The connection may already have ended (a task can finish before the handles are
    // registered): make sure nothing is left running.
    if rx.borrow().is_some() {
        for t in tasks.lock().unwrap().drain(..) {
            t.abort();
        }
    }
    (
        Link {
            closed: rx,
            rtt_us: rtt,
            tasks,
        },
        outbox,
    )
}
