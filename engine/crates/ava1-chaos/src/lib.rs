//! A TCP proxy that misbehaves on purpose: latency, bandwidth caps, blackholes
//! (half-open links) and killed connections. For AVA1 tests and the lab tool.
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::AbortHandle;

#[derive(Debug, Clone, Default)]
pub struct ChaosConfig {
    /// Added to every chunk, each direction.
    pub delay: Duration,
    /// Per-direction cap.
    pub bytes_per_sec: Option<u64>,
    /// Kill every connection this often.
    pub kill_every: Option<Duration>,
}

struct Ctl {
    blackhole: AtomicBool,
    conns: Mutex<Vec<[AbortHandle; 2]>>,
}

pub struct ChaosProxy {
    pub addr: SocketAddr,
    ctl: Arc<Ctl>,
    tasks: Vec<AbortHandle>,
}

impl Drop for ChaosProxy {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
        self.kill_all();
    }
}

impl ChaosProxy {
    pub async fn start(upstream: SocketAddr, cfg: ChaosConfig) -> std::io::Result<Self> {
        Self::start_on("127.0.0.1:0", upstream, cfg).await
    }

    pub async fn start_on(
        listen: &str,
        upstream: SocketAddr,
        cfg: ChaosConfig,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(listen).await?;
        let addr = listener.local_addr()?;
        let ctl = Arc::new(Ctl {
            blackhole: AtomicBool::new(false),
            conns: Mutex::default(),
        });
        let mut tasks = Vec::new();
        {
            let (ctl, cfg) = (ctl.clone(), cfg.clone());
            tasks.push(
                tokio::spawn(async move {
                    loop {
                        let Ok((down, _)) = listener.accept().await else {
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            continue;
                        };
                        let (ctl, cfg) = (ctl.clone(), cfg.clone());
                        tokio::spawn(async move {
                            let Ok(up) = TcpStream::connect(upstream).await else {
                                return;
                            };
                            let _ = (down.set_nodelay(true), up.set_nodelay(true));
                            let (dr, dw) = down.into_split();
                            let (ur, uw) = up.into_split();
                            let a =
                                tokio::spawn(pump(dr, uw, cfg.clone(), ctl.clone())).abort_handle();
                            let b =
                                tokio::spawn(pump(ur, dw, cfg.clone(), ctl.clone())).abort_handle();
                            let mut conns = ctl.conns.lock().unwrap();
                            conns.retain(|hs| !hs.iter().all(|h| h.is_finished()));
                            conns.push([a, b]);
                        });
                    }
                })
                .abort_handle(),
            );
        }
        if let Some(every) = cfg.kill_every {
            let ctl = ctl.clone();
            tasks.push(
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(every).await;
                        kill_all(&ctl);
                    }
                })
                .abort_handle(),
            );
        }
        Ok(Self { addr, ctl, tasks })
    }

    /// Stop forwarding in both directions without closing anything: a half-open link.
    pub fn blackhole(&self, on: bool) {
        self.ctl.blackhole.store(on, Ordering::SeqCst);
    }

    pub fn kill_all(&self) {
        kill_all(&self.ctl);
    }

    /// Kill only the most recently opened connection (e.g. the lane just opened).
    pub fn kill_newest(&self) {
        let mut conns = self.ctl.conns.lock().unwrap();
        while let Some(hs) = conns.pop() {
            if hs.iter().all(|h| h.is_finished()) {
                continue;
            }
            for h in hs {
                h.abort();
            }
            break;
        }
    }

    pub fn connections(&self) -> usize {
        self.ctl
            .conns
            .lock()
            .unwrap()
            .iter()
            .filter(|hs| hs.iter().all(|h| !h.is_finished()))
            .count()
    }
}

fn kill_all(ctl: &Ctl) {
    for hs in ctl.conns.lock().unwrap().drain(..) {
        for h in hs {
            h.abort();
        }
    }
}

async fn pump(mut from: OwnedReadHalf, mut to: OwnedWriteHalf, cfg: ChaosConfig, ctl: Arc<Ctl>) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = match from.read(&mut buf).await {
            Ok(0) | Err(_) => {
                // A blackhole is half-open: do not forward the FIN either.
                wait_blackhole(&ctl).await;
                break;
            }
            Ok(n) => n,
        };
        wait_blackhole(&ctl).await;
        if !cfg.delay.is_zero() {
            tokio::time::sleep(cfg.delay).await;
        }
        if let Some(bps) = cfg.bytes_per_sec {
            tokio::time::sleep(Duration::from_secs_f64(n as f64 / bps.max(1) as f64)).await;
        }
        if to.write_all(&buf[..n]).await.is_err() {
            break;
        }
    }
    let _ = to.shutdown().await;
}

async fn wait_blackhole(ctl: &Ctl) {
    while ctl.blackhole.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
