//! Copying AVA1's progress into the counters the engine's ticker reads.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use ava1::send::Progress;
use ps5upload_core::transfer::TransferConfig;

/// Copies AVA1 progress into `cfg`'s counters every 200 ms until dropped, and once
/// more on drop (the final values, so a caller asserting progress right after the
/// transfer returns sees what the ticker will report).
///
/// Mapping (C12/A3): `progress_bytes` ← `bytes_sent` (Received bytes),
/// `progress_files` and `progress_files_finalized` ← `files_durable`,
/// `progress_bytes_finalized` ← `bytes_durable`. AVA1 has no per-file "sent" counter;
/// durability is its analogue, so the UI's file counter moves in commit-sized steps —
/// slower but honest, and strictly better than showing 0 for the whole transfer.
/// The engine's ticker reads these counters absolutely, so the bridge `store`s, never
/// `fetch_add`s.
pub struct Bridge {
    handle: tokio::task::AbortHandle,
    counters: Arc<Counters>,
}

struct Counters {
    p: Arc<Progress>,
    bytes: Option<Arc<std::sync::atomic::AtomicU64>>,
    files: Option<Arc<std::sync::atomic::AtomicU64>>,
    files_finalized: Option<Arc<std::sync::atomic::AtomicU64>>,
    bytes_finalized: Option<Arc<std::sync::atomic::AtomicU64>>,
    settling: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl Bridge {
    pub fn start(p: Arc<Progress>, cfg: &TransferConfig) -> Bridge {
        let counters = Arc::new(Counters {
            p,
            bytes: cfg.progress_bytes.clone(),
            files: cfg.progress_files.clone(),
            files_finalized: cfg.progress_files_finalized.clone(),
            bytes_finalized: cfg.progress_bytes_finalized.clone(),
            settling: cfg.progress_settling.clone(),
        });
        let ticker = counters.clone();
        let handle = tokio::spawn(async move {
            loop {
                ticker.store();
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .abort_handle();
        Bridge { handle, counters }
    }
}

impl Counters {
    fn store(&self) {
        if let Some(x) = &self.bytes {
            // Skipped bytes count as done, so a resume's bar reaches 100%.
            x.store(
                self.p.bytes_sent.load(Ordering::Relaxed)
                    + self.p.skipped_bytes.load(Ordering::Relaxed),
                Ordering::Relaxed,
            );
        }
        if let Some(x) = &self.files {
            x.store(
                self.p.files_durable.load(Ordering::Relaxed),
                Ordering::Relaxed,
            );
        }
        if let Some(x) = &self.files_finalized {
            x.store(
                self.p.files_durable.load(Ordering::Relaxed),
                Ordering::Relaxed,
            );
        }
        if let Some(x) = &self.settling {
            x.store(self.p.settling.load(Ordering::Relaxed), Ordering::Relaxed);
        }
        if let Some(x) = &self.bytes_finalized {
            x.store(
                self.p.bytes_durable.load(Ordering::Relaxed),
                Ordering::Relaxed,
            );
        }
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.handle.abort();
        self.counters.store();
    }
}

/// The same words the benchmark reports (Tasks 26–28), so a report and a job's
/// `commit_ack` say the same thing.
pub fn bottleneck_name(b: u8) -> &'static str {
    match b {
        ava1::gen::BN_NETWORK => "network",
        ava1::gen::BN_SOURCE => "source",
        ava1::gen::BN_DISK => "console drive",
        ava1::gen::BN_WORKERS => "console workers",
        ava1::gen::BN_CREDIT => "console memory",
        _ => "none",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[tokio::test]
    async fn the_senders_settling_flag_reaches_the_transfer_config() {
        let p = Arc::new(Progress::default());
        let flag = Arc::new(AtomicBool::new(false));
        let mut cfg = TransferConfig::new("127.0.0.1:1");
        cfg.progress_settling = Some(flag.clone());
        let bridge = Bridge::start(p.clone(), &cfg);
        p.settling.store(true, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(450)).await;
        assert!(flag.load(Ordering::Relaxed));
        p.settling.store(false, Ordering::Relaxed);
        drop(bridge); // the final values are stored on drop
        assert!(!flag.load(Ordering::Relaxed));
    }
}
