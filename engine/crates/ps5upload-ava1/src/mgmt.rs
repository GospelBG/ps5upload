//! The AVA1 implementation of `ps5upload_core::mgmt::MgmtTransport`.
//!
//! One pooled session per console (`pool().session`), shared with every transfer: a
//! second session for the same identity would evict the first (SPEC.md section 8), so this
//! module never connects on its own. Calls take a permit from a per-console gate that
//! holds back two of the eight in-flight slots for `node.status`, `job.status`,
//! `job.cancel` and `job.list`, so a flood of slow calls can never hide a cancel or
//! liveness (SPEC.md section 7.4).
//!
//! Callers keep the legacy bodies; `mgmt_convert` maps them to the typed ones, large reads
//! loop on `eof` and large writes go in `FsWrite` chunks.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use ava1::gen::{self, MgmtText};
use ava1::wire::Message;
use ava1::Ava1Error;
use ps5upload_core::mgmt::{self, Method, MgmtError, MgmtTransport};
use tokio::sync::{Semaphore, SemaphorePermit};

use crate::mgmt_convert as conv;
use crate::pool::{host_of, pool, Pool};
use crate::route::{mode, use_ava1_in, Mode};

/// In-flight calls the payload allows per session (`RPC_WORKERS`, SPEC.md section 7.4).
pub const IN_FLIGHT: usize = ava1::server::RPC_WORKERS;
/// Slots only `node.status`, `job.status`, `job.cancel` and `job.list` may use.
pub const RESERVED: usize = 2;
/// Slots every other method shares.
pub const GENERAL: usize = IN_FLIGHT - RESERVED;
/// `ERR_BUSY` is retried this many times, with these pauses, and never surfaced as a
/// payload failure while a retry is left.
pub const BUSY_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(100),
    Duration::from_millis(300),
    Duration::from_millis(900),
];
/// How long a console that answered `ERR_UNKNOWN_METHOD` (an older AVA1 helper without the
/// management methods) is sent to FTX2 without asking again.
const NO_MGMT_TTL: Duration = Duration::from_secs(30);

/// True for the methods the reserved slots exist for.
pub fn is_priority(method: u16) -> bool {
    matches!(
        method,
        gen::METHOD_NODE_STATUS
            | gen::METHOD_JOB_STATUS
            | gen::METHOD_JOB_CANCEL
            | gen::METHOD_JOB_LIST
    )
}

/// Methods that change nothing, so a call lost with its session may be sent again on the
/// next session. Everything else is never repeated: it may already have run.
fn is_read_only(method: u16) -> bool {
    matches!(
        method,
        gen::METHOD_NODE_STATUS
            | gen::METHOD_JOB_STATUS
            | gen::METHOD_JOB_LIST
            | gen::METHOD_FS_VOLUMES
            | gen::METHOD_FS_LIST
            | gen::METHOD_FS_STAT
            | gen::METHOD_FS_READ
            | gen::METHOD_LOG_KLOG
            | gen::METHOD_LOG_SYSLOG
            | gen::METHOD_HW_INFO
            | gen::METHOD_HW_TEMPS
            | gen::METHOD_HW_POWER
            | gen::METHOD_HW_STORAGE
    )
}

/// The per-console in-flight gate: [`GENERAL`] slots for everyone, [`RESERVED`] more for
/// the priority methods. The total never exceeds the payload's eight.
pub struct MgmtGate {
    general: Semaphore,
    reserved: Semaphore,
}

impl Default for MgmtGate {
    fn default() -> Self {
        Self {
            general: Semaphore::new(GENERAL),
            reserved: Semaphore::new(RESERVED),
        }
    }
}

impl MgmtGate {
    pub async fn acquire(&self, priority: bool) -> SemaphorePermit<'_> {
        if !priority {
            return self.general.acquire().await.expect("gate is never closed");
        }
        if let Ok(p) = self.general.try_acquire() {
            return p;
        }
        tokio::select! {
            biased;
            p = self.general.acquire() => p.expect("gate is never closed"),
            p = self.reserved.acquire() => p.expect("gate is never closed"),
        }
    }

    /// Free general / reserved slots (test seam).
    pub fn available(&self) -> (usize, usize) {
        (
            self.general.available_permits(),
            self.reserved.available_permits(),
        )
    }
}

enum PoolRef {
    Global,
    Fixed(&'static Pool),
}

/// First line of a `log.klog` / `log.syslog` reply the console had to clamp (SPEC.md section 7.3).
pub const TAIL_CLIPPED: &str =
    "[earlier log text omitted: the console returned only the newest part of this log]\n";

pub struct AvaTransport {
    pool: PoolRef,
    gates: Mutex<HashMap<String, Arc<MgmtGate>>>,
    no_mgmt: Mutex<HashMap<String, Instant>>,
    busy_delays: [Duration; 3],
}

/// Registers the AVA1 transport for this process (the engine calls it once at start).
pub fn install() {
    mgmt::set_transport(Arc::new(AvaTransport::global()));
}

impl AvaTransport {
    /// Over the process's pool, the one every transfer shares.
    pub fn global() -> Self {
        Self::with(PoolRef::Global)
    }

    /// Over an explicit pool (tests). The pool must outlive every call; leak it.
    pub fn with_pool(pool: &'static Pool) -> Self {
        Self::with(PoolRef::Fixed(pool))
    }

    fn with(pool: PoolRef) -> Self {
        Self {
            pool,
            gates: Mutex::default(),
            no_mgmt: Mutex::default(),
            busy_delays: BUSY_RETRY_DELAYS,
        }
    }

    /// Test seam: shorter `ERR_BUSY` pauses.
    pub fn with_busy_delays(mut self, d: [Duration; 3]) -> Self {
        self.busy_delays = d;
        self
    }

    fn pool(&self) -> &Pool {
        match &self.pool {
            PoolRef::Global => pool(),
            PoolRef::Fixed(p) => p,
        }
    }

    /// The gate of a console, keyed by host like the pool's sessions: two ports of one console
    /// share one gate of eight.
    pub fn gate(&self, console: &str) -> Arc<MgmtGate> {
        self.gates
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(host_of(console))
            .or_default()
            .clone()
    }

    /// Whether this console is served over AVA1 for management: the same `use_ava1`
    /// decision uploads make, minus a console that just said it has no management methods.
    fn serves(&self, console: &str) -> bool {
        if mode() == Mode::Auto {
            let mut n = self.no_mgmt.lock().unwrap_or_else(|e| e.into_inner());
            match n.get(&host_of(console)) {
                Some(t) if t.elapsed() < NO_MGMT_TTL => return false,
                Some(_) => {
                    n.remove(&host_of(console));
                }
                None => {}
            }
        }
        use_ava1_in(self.pool(), console)
    }

    fn mark_no_mgmt(&self, console: &str) {
        self.no_mgmt
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(host_of(console), Instant::now());
    }

    /// One RPC: gate permit, `ERR_BUSY` retries, one resend on a lost session for
    /// read-only methods. `Ok` is the body of a status-0 reply; a non-zero status is a
    /// [`MgmtError`] carrying the legacy token.
    async fn rpc(
        &self,
        console: &str,
        method: u16,
        label: &str,
        body: &[u8],
        timeout: Duration,
    ) -> Result<Vec<u8>> {
        let gate = self.gate(console);
        let mut busy = 0usize;
        let mut resent = false;
        loop {
            let attempt = tokio::time::timeout(timeout, async {
                let session = self.pool().session(console).await?;
                let _permit = gate.acquire(is_priority(method)).await;
                session.rpc(method, body).await
            })
            .await;
            let reply = match attempt {
                Err(_) => return Err(anyhow::anyhow!("{label}: timed out after {timeout:?}")),
                Ok(Err(e)) => {
                    let lost =
                        matches!(e, Ava1Error::Lost(_) | Ava1Error::Closed | Ava1Error::Io(_));
                    if lost && is_read_only(method) && !resent {
                        resent = true;
                        continue;
                    }
                    return Err(map_transport_error(label, e));
                }
                Ok(Ok(r)) => r,
            };
            if reply.status == gen::STATUS_OK {
                return Ok(reply.body);
            }
            if reply.status == gen::ERR_BUSY && busy < self.busy_delays.len() {
                tokio::time::sleep(self.busy_delays[busy]).await;
                busy += 1;
                continue;
            }
            return Err(refusal(label, reply.status, &reply.body).into());
        }
    }

    async fn text(
        &self,
        console: &str,
        method: Method,
        label: &str,
        body: &[u8],
        timeout: Duration,
    ) -> Result<Vec<u8>> {
        let req = MgmtText {
            body: body.to_vec(),
            more: None,
        }
        .to_bytes()?;
        let reply = self.rpc(console, method.id, label, &req, timeout).await?;
        let t = MgmtText::decode(&reply)?;
        // A paged reply is not the whole answer. The two log tails are clamped reads whose
        // `more` only says older text exists, which the legacy handlers never reported.
        let tail = matches!(method.id, gen::METHOD_LOG_KLOG | gen::METHOD_LOG_SYSLOG);
        if t.more.unwrap_or(0) != 0 && !tail {
            return Err(MgmtError {
                label: label.to_string(),
                status: gen::ERR_INTERNAL,
                cause: "reply_paged".into(),
            }
            .into());
        }
        if tail && t.more.unwrap_or(0) != 0 {
            // Say so in the text itself: a bug report that quietly starts mid-log reads as if
            // the console had nothing older (the legacy FTX2 reply had no such limit).
            let mut v = TAIL_CLIPPED.as_bytes().to_vec();
            v.extend_from_slice(&t.body);
            return Ok(v);
        }
        Ok(t.body)
    }

    /// The whole call. `Ok(None)` sends the caller to FTX2.
    async fn run(
        &self,
        console: &str,
        method: Method,
        label: &str,
        body: &[u8],
        timeout: Duration,
    ) -> Result<Option<Vec<u8>>> {
        let r = self.dispatch(console, method, label, body, timeout).await;
        if let Err(e) = &r {
            let unknown = e
                .downcast_ref::<MgmtError>()
                .is_some_and(|m| m.status == gen::ERR_UNKNOWN_METHOD);
            if unknown && mode() == Mode::Auto {
                // An older AVA1 helper: it speaks transfers but has no management methods.
                self.mark_no_mgmt(console);
                return Ok(None);
            }
        }
        r
    }

    async fn dispatch(
        &self,
        console: &str,
        method: Method,
        label: &str,
        body: &[u8],
        timeout: Duration,
    ) -> Result<Option<Vec<u8>>> {
        let id = method.id;
        let json =
            |v: serde_json::Value| -> Result<Option<Vec<u8>>> { Ok(Some(serde_json::to_vec(&v)?)) };
        match id {
            gen::METHOD_NODE_STATUS => {
                let r = self.rpc(console, id, label, &[], timeout).await?;
                json(conv::node_status_json(&gen::NodeStatus::decode(&r)?))
            }
            gen::METHOD_FS_LIST => {
                let req = conv::fs_list_request(body, label)?;
                let path = req.path.clone();
                let r = self
                    .rpc(console, id, label, &req.to_bytes()?, timeout)
                    .await?;
                json(conv::fs_list_reply(&path, &gen::FsListResult::decode(&r)?))
            }
            gen::METHOD_FS_STAT => {
                let req = conv::fs_path_request(body, label)?;
                let r = self
                    .rpc(console, id, label, &req.to_bytes()?, timeout)
                    .await?;
                json(conv::fs_stat_reply(&gen::FsStat::decode(&r)?))
            }
            gen::METHOD_FS_MKDIR => {
                let req = conv::fs_mkdir_request(body, label)?;
                self.rpc(console, id, label, &req.to_bytes()?, timeout)
                    .await?;
                Ok(Some(Vec::new()))
            }
            gen::METHOD_FS_RENAME => {
                let req = conv::fs_rename_request(body, label)?;
                self.rpc(console, id, label, &req.to_bytes()?, timeout)
                    .await?;
                Ok(Some(Vec::new()))
            }
            gen::METHOD_FS_CHMOD => match conv::fs_chmod_request(body, label)? {
                // Recursive chmod is a job.run op (Task 5); FTX2 still serves it until then.
                None => Ok(None),
                Some(req) => {
                    self.rpc(console, id, label, &req.to_bytes()?, timeout)
                        .await?;
                    Ok(Some(Vec::new()))
                }
            },
            gen::METHOD_FS_READ => {
                let ask = conv::fs_read_request(body, label)?;
                self.read_loop(console, label, &ask, timeout)
                    .await
                    .map(Some)
            }
            gen::METHOD_FS_WRITE => {
                let ask = conv::fs_write_request(body, label)?;
                let size = self.write_chunks(console, label, &ask, timeout).await?;
                json(serde_json::json!({ "ok": true, "size": size }))
            }
            _ => self
                .text(console, method, label, body, timeout)
                .await
                .map(Some),
        }
    }

    /// Reads up to `ask.limit` bytes: `fs.read` calls of at most `FS_READ_MAX` each, until the
    /// bytes asked for arrived, `eof` was set, or the node had nothing more to give.
    async fn read_loop(
        &self,
        console: &str,
        label: &str,
        ask: &conv::ReadAsk,
        timeout: Duration,
    ) -> Result<Vec<u8>> {
        let mut out: Vec<u8> = Vec::new();
        let want = ask.limit;
        loop {
            let left = want - out.len() as u64;
            // A zero-length ask is still one call: it answers path errors and existence.
            if left == 0 && !(want == 0 && out.is_empty()) {
                break;
            }
            let req = gen::FsRead {
                path: ask.path.clone(),
                offset: ask.offset + out.len() as u64,
                len: left.min(gen::FS_READ_MAX as u64) as u32,
                flags: ask.flags,
            };
            let r = self
                .rpc(
                    console,
                    gen::METHOD_FS_READ,
                    label,
                    &req.to_bytes()?,
                    timeout,
                )
                .await?;
            let r = gen::FsReadResult::decode(&r)?;
            let got = r.data.len();
            out.extend_from_slice(&r.data);
            if r.eof != 0 || got == 0 || want == 0 {
                break;
            }
        }
        Ok(out)
    }

    /// Writes `ask.data` as one atomic call, or as `FSW_CHUNK_MAX` chunks at their offsets
    /// with `COMMIT` on the last. A failed chunk leaves the `.ps5upload.tmp` file for the
    /// next write at offset 0 to truncate (SPEC.md section 7.5).
    ///
    /// TODO(Task 5): remove `<path>.ps5upload.tmp` best-effort on failure once `job.run`
    /// DELETE is available here; no removal method exists before it. Tracked in CUTOVER.md.
    async fn write_chunks(
        &self,
        console: &str,
        label: &str,
        ask: &conv::WriteAsk,
        timeout: Duration,
    ) -> Result<usize> {
        let chunk = gen::FSW_CHUNK_MAX as usize;
        let total = ask.data.len().div_ceil(chunk).max(1);
        for i in 0..total {
            let lo = i * chunk;
            let hi = (lo + chunk).min(ask.data.len());
            let req = gen::FsWrite {
                path: ask.path.clone(),
                offset: lo as u64,
                flags: conv::write_chunk_flags(ask.create_only, i, total),
                data: ask.data[lo..hi].to_vec(),
                mode: None,
            };
            self.rpc(
                console,
                gen::METHOD_FS_WRITE,
                label,
                &req.to_bytes()?,
                timeout,
            )
            .await?;
        }
        Ok(ask.data.len())
    }
}

impl MgmtTransport for AvaTransport {
    fn call(
        &self,
        addr: &str,
        method: Method,
        label: &str,
        body: &[u8],
        timeout: Duration,
    ) -> Result<Option<Vec<u8>>> {
        run_blocking(|| {
            if !self.serves(addr) {
                return Ok(None);
            }
            crate::block_on(self.run(addr, method, label, body, timeout))
        })
    }
}

/// Runs blocking work (which itself `block_on`s) without panicking when the caller sits on an
/// async worker: a multi-thread runtime worker hands its role over (`block_in_place`); a
/// current-thread runtime cannot, so the work runs on a helper thread while this one waits.
/// Threads outside any runtime, and `spawn_blocking` threads (the engine's normal case), run it
/// directly.
fn run_blocking<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    use tokio::runtime::{Handle, RuntimeFlavor};
    match Handle::try_current().map(|h| h.runtime_flavor()) {
        Ok(RuntimeFlavor::MultiThread) => tokio::task::block_in_place(f),
        Ok(_) => std::thread::scope(|s| {
            s.spawn(f)
                .join()
                .unwrap_or_else(|p| std::panic::resume_unwind(p))
        }),
        Err(_) => f(),
    }
}

/// A non-zero reply status as the error callers know: the body is the legacy token.
fn refusal(label: &str, status: u16, body: &[u8]) -> MgmtError {
    let cause = conv::cause_text(body);
    MgmtError {
        label: label.to_string(),
        status,
        cause: if cause.is_empty() {
            conv::default_cause(status).to_string()
        } else {
            cause
        },
    }
}

/// A session or network failure. Not being paired is its own refusal: the UI keys on the
/// `not_paired` cause to show the pairing prompt.
fn map_transport_error(label: &str, e: Ava1Error) -> anyhow::Error {
    match e {
        Ava1Error::NotPaired => MgmtError {
            label: label.to_string(),
            status: gen::ERR_NOT_PAIRED,
            cause: "not_paired".into(),
        }
        .into(),
        Ava1Error::Refused { code, message } => MgmtError {
            label: label.to_string(),
            status: code,
            cause: message,
        }
        .into(),
        e => anyhow::Error::new(e).context(format!("management call {label} failed")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn being_unpaired_is_the_pairing_error_the_ui_keys_on() {
        let e = map_transport_error("HW_INFO", Ava1Error::NotPaired);
        let m = e.downcast_ref::<MgmtError>().unwrap();
        assert_eq!(m.status, gen::ERR_NOT_PAIRED);
        assert_eq!(m.cause, "not_paired");
        assert_eq!(e.to_string(), "payload rejected HW_INFO: not_paired");
    }

    #[test]
    fn a_network_failure_is_not_a_payload_refusal() {
        let e = map_transport_error("HW_INFO", Ava1Error::Lost("reset".into()));
        assert!(e.downcast_ref::<MgmtError>().is_none());
        assert!(e.to_string().contains("HW_INFO"), "{e}");
    }

    #[test]
    fn the_gate_is_eight_slots_with_two_held_back() {
        assert_eq!((GENERAL, RESERVED, IN_FLIGHT), (6, 2, 8));
        for id in [
            gen::METHOD_NODE_STATUS,
            gen::METHOD_JOB_STATUS,
            gen::METHOD_JOB_CANCEL,
            gen::METHOD_JOB_LIST,
        ] {
            assert!(is_priority(id));
        }
        assert!(!is_priority(gen::METHOD_HW_INFO));
    }

    #[tokio::test]
    async fn priority_calls_use_the_reserve_after_the_general_slots_are_taken() {
        let g = MgmtGate::default();
        let mut held = Vec::new();
        for _ in 0..GENERAL {
            held.push(g.acquire(false).await);
        }
        assert_eq!(g.available(), (0, RESERVED));
        // A seventh ordinary call waits.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), g.acquire(false))
                .await
                .is_err()
        );
        // Two priority calls pass, a third waits.
        held.push(g.acquire(true).await);
        held.push(g.acquire(true).await);
        assert_eq!(g.available(), (0, 0));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), g.acquire(true))
                .await
                .is_err()
        );
        drop(held);
        assert_eq!(g.available(), (GENERAL, RESERVED));
    }

    #[tokio::test]
    async fn a_priority_call_prefers_a_general_slot_so_the_reserve_stays_free() {
        let g = MgmtGate::default();
        let _p = g.acquire(true).await;
        assert_eq!(g.available(), (GENERAL - 1, RESERVED));
    }
}
