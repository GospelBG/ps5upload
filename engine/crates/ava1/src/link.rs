//! Runs one connection after its handshake: answers Ping, records RTT from Pong,
//! sends heartbeats, declares the peer dead after `dead_after` of silence, and hands
//! every other frame to the owner (SPEC.md §6).
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, watch};
use tokio::task::AbortHandle;

use crate::conn::{Frame, FrameReader, FrameWriter};
use crate::gen::{self, Bye, Ping, Pong};
use crate::session::Timing;
use crate::wire::FrameMessage;

pub(crate) type SharedWriter = Arc<tokio::sync::Mutex<FrameWriter<OwnedWriteHalf>>>;

pub(crate) fn now_us() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u64
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
    writer: SharedWriter,
    timing: Timing,
    deliver: mpsc::UnboundedSender<Frame>,
) -> Link {
    let (tx, rx) = watch::channel(None::<String>);
    let tasks: Arc<Mutex<Vec<AbortHandle>>> = Arc::default();
    let last_rx = Arc::new(Mutex::new(Instant::now()));
    let rtt = Arc::new(AtomicU64::new(0));

    let close: Close = {
        let (tasks, writer) = (tasks.clone(), writer.clone());
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
            let w = writer.clone();
            tokio::spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(1), async {
                    w.lock().await.shutdown().await
                })
                .await;
            });
            for t in tasks.lock().unwrap().drain(..) {
                t.abort();
            }
        })
    };

    let reader_task = {
        let (writer, close, last_rx, rtt) =
            (writer.clone(), close.clone(), last_rx.clone(), rtt.clone());
        tokio::spawn(async move {
            loop {
                let f = match reader.recv().await {
                    Ok(f) => f,
                    Err(e) => return close(e.to_string()),
                };
                *last_rx.lock().unwrap() = Instant::now();
                match f.ty {
                    Ping::TYPE => {
                        let Ok(p) = f.decode::<Ping>() else {
                            return close("malformed Ping".into());
                        };
                        // Answered off the reader, so a busy writer never stalls reading.
                        let w = writer.clone();
                        tokio::spawn(async move {
                            let _ = w
                                .lock()
                                .await
                                .send_msg(
                                    0,
                                    &Pong {
                                        seq: p.seq,
                                        t_us: p.t_us,
                                    },
                                )
                                .await;
                        });
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
                        if deliver.send(f).is_err() {
                            return close("session ended".into());
                        }
                    }
                }
            }
        })
    };

    let heartbeat_task = {
        let (writer, close, last_rx) = (writer.clone(), close.clone(), last_rx.clone());
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(timing.ping_every);
            iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut seq = 0u32;
            loop {
                iv.tick().await;
                let quiet = last_rx.lock().unwrap().elapsed();
                if quiet > timing.dead_after {
                    return close(format!(
                        "the other device stopped answering ({} ms without a frame)",
                        quiet.as_millis()
                    ));
                }
                seq = seq.wrapping_add(1);
                // A busy writer means data is flowing, which is the proof of life: skip the
                // Ping. A slow write never counts as death (the peer may just read slowly);
                // only silence on the read side does. The write is never cancelled (a
                // cancelled write would leave a partial sealed frame and desync the stream),
                // so it runs in its own task; a failure surfaces through the reader side.
                if let Ok(mut guard) = writer.clone().try_lock_owned() {
                    tokio::spawn(async move {
                        let _ = guard
                            .send_msg(
                                0,
                                &Ping {
                                    seq,
                                    t_us: now_us(),
                                },
                            )
                            .await;
                    });
                }
            }
        })
    };

    tasks
        .lock()
        .unwrap()
        .extend([reader_task.abort_handle(), heartbeat_task.abort_handle()]);
    Link {
        closed: rx,
        rtt_us: rtt,
        tasks,
    }
}
