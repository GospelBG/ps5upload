//! AVA1 or FTX2 for one transfer (FTX2 stays the default until the Task 23 cutover).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::pool::{pool, Pool};

/// Which transfer path a job takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Auto,
    Ava1,
    Ftx2,
}

/// From `PS5UPLOAD_TRANSFER=auto|ava1|ftx2` (case-insensitive; anything else → Auto).
pub fn mode() -> Mode {
    match std::env::var("PS5UPLOAD_TRANSFER")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "ava1" => Mode::Ava1,
        "ftx2" => Mode::Ftx2,
        _ => Mode::Auto,
    }
}

/// How long a failed probe is remembered for, and how long one probe may take
/// (nominal constants: 30 s of negative cache, 3 s for a session attempt that cannot
/// be interrupted).
const NEGATIVE_TTL: Duration = Duration::from_secs(30);
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// The negative cache: probe failures by console string. Only failures are cached — a
/// live session is already cached by the pool. The clock is injected (A4): tests
/// advance it to pin the expiry without sleeping.
struct Negative {
    at: Mutex<HashMap<String, Instant>>,
    now: Mutex<fn() -> Instant>,
}

fn negative() -> &'static Negative {
    static N: OnceLock<Negative> = OnceLock::new();
    N.get_or_init(|| Negative {
        at: Mutex::new(HashMap::new()),
        now: Mutex::new(Instant::now),
    })
}

fn now() -> Instant {
    negative().now.lock().unwrap()()
}

/// Test seam (A4): pins the negative cache's expiry with an injected clock, so no
/// test sleeps for the TTL.
#[cfg(test)]
fn set_clock_for_test(f: fn() -> Instant) {
    *negative().now.lock().unwrap() = f;
}

/// Whether a transfer to `console` goes over AVA1. Blocking: call from a blocking
/// thread. The probe *is* a session attempt — a successful one is reused by the
/// transfer that follows (C16).
///
/// `Ftx2`: never. `Ava1`: always — failures must surface, not silently fall back.
/// `Auto`: true only when a session can be established *and* the console advertises
/// the data plane (`peer_caps() & CAP_DATA_PLANE != 0`); a console that needs a user
/// code is `Err(NotPaired)` — a person must compare codes, not a transfer — and any
/// failure is cached for `NEGATIVE_TTL`.
pub fn use_ava1(console: &str) -> bool {
    use_ava1_in(pool(), console)
}

/// `use_ava1` against an explicit pool (the test seam, like the upload adapters'
/// `_in` variants — A1 keeps process env out of the tests).
pub fn use_ava1_in(pool: &Pool, console: &str) -> bool {
    match mode() {
        Mode::Ftx2 => false,
        Mode::Ava1 => true,
        Mode::Auto => {
            if negative()
                .at
                .lock()
                .unwrap()
                .get(console)
                .is_some_and(|t| now().saturating_duration_since(*t) < NEGATIVE_TTL)
            {
                return false;
            }
            let ok = crate::block_on(async {
                tokio::time::timeout(PROBE_TIMEOUT, pool.session(console))
                    .await
                    .ok()
                    .and_then(|r| r.ok())
                    .is_some_and(|s| s.peer_caps() & ava1::gen::CAP_DATA_PLANE != 0)
            });
            if !ok {
                negative()
                    .at
                    .lock()
                    .unwrap()
                    .insert(console.to_string(), now());
            }
            ok
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A fake clock: real time plus a test-controlled offset (A4 — the negative
    /// cache's expiry is pinned by advancing the clock, never by sleeping).
    static CLOCK_OFFSET_MS: AtomicU64 = AtomicU64::new(0);
    fn fake_now() -> Instant {
        Instant::now() + Duration::from_millis(CLOCK_OFFSET_MS.load(Ordering::Relaxed))
    }

    #[test]
    fn the_negative_cache_hits_until_the_injected_clock_expires_it() {
        CLOCK_OFFSET_MS.store(0, Ordering::Relaxed);
        set_clock_for_test(fake_now);
        let d = std::env::temp_dir().join(format!("p5a-route-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        // Port 1 is never a listener: every probe fails instantly (refused), so this
        // needs no server — the attempts counter is the observable.
        let p = Pool::new(d.join("ava")).with_addr("127.0.0.1:1");
        assert!(!use_ava1_in(&p, "c"), "a refused probe routes to FTX2");
        assert_eq!(p.attempts(), 1, "the first probe connected");
        assert!(!use_ava1_in(&p, "c"), "the failure is cached");
        assert_eq!(p.attempts(), 1, "the hit path probed again");
        CLOCK_OFFSET_MS.store(10_000, Ordering::Relaxed);
        assert!(!use_ava1_in(&p, "c"));
        assert_eq!(p.attempts(), 1, "10 s is inside the 30 s TTL");
        CLOCK_OFFSET_MS.store(31_000, Ordering::Relaxed);
        assert!(!use_ava1_in(&p, "c"));
        assert_eq!(p.attempts(), 2, "the entry expired and the probe ran again");
        let _ = std::fs::remove_dir_all(&d);
    }
}
