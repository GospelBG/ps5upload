//! MIGRATION SHIM: deleted with `legacy_helper.rs` in the release after the AVA1 cutover.
//!
//! Keeps `POST /api/ps5/helper/replace` from restarting a console's helper twice at once or twice
//! within 60 s (a console that is restarted faster than that has gone down before). Per host: one
//! replace in flight, and a cooldown after the last one STARTED (a failed attempt counts: it
//! may have restarted the helper). The clock is injected so tests do not sleep.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Error token: a replace for this host is already running.
pub const REPLACE_IN_PROGRESS: &str = "replace_in_progress";
/// Error token: this host was replaced less than [`COOLDOWN`] ago.
pub const REPLACE_COOLDOWN: &str = "replace_cooldown";
/// The minimum spacing between helper restarts on one console.
pub const COOLDOWN: Duration = Duration::from_secs(60);

#[derive(Default)]
pub struct Guard {
    hosts: Mutex<HashMap<String, Entry>>,
}

#[derive(Default)]
struct Entry {
    in_flight: bool,
    last_start: Option<Instant>,
}

/// Held while a replace runs; releases the in-flight mark on drop.
pub struct Permit<'a> {
    guard: &'a Guard,
    host: String,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut m = self.guard.hosts.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = m.get_mut(&self.host) {
            e.in_flight = false;
        }
    }
}

impl Guard {
    /// Claims `host` at time `now`: `Err(token)` when one is running or the cooldown has not passed.
    pub fn begin(&self, host: &str, now: Instant) -> Result<Permit<'_>, &'static str> {
        let mut m = self.hosts.lock().unwrap_or_else(|e| e.into_inner());
        let e = m.entry(host.to_string()).or_default();
        if e.in_flight {
            return Err(REPLACE_IN_PROGRESS);
        }
        if let Some(t) = e.last_start {
            if now.saturating_duration_since(t) < COOLDOWN {
                return Err(REPLACE_COOLDOWN);
            }
        }
        e.in_flight = true;
        e.last_start = Some(now);
        drop(m);
        Ok(Permit {
            guard: self,
            host: host.to_string(),
        })
    }
}

/// The text of a refused claim (the token first: the client matches on it).
pub fn message(token: &str) -> String {
    format!("{token}: a helper replace for this console is running or was just done (60 s apart)")
}

/// Why a console in `state` (a `legacy_helper::state` token) must not be replaced: only a genuinely
/// older helper is. The token comes first.
pub fn not_replaceable(state: &str) -> String {
    match state {
        "starting" => {
            "helper_starting: the new helper is still starting; wait, do not replace".into()
        }
        "ava1_failed" => {
            "ava1_failed: the helper is new but its AVA1 server did not start; restart the console"
                .into()
        }
        _ => "helper_not_running: nothing answers on the console".into(),
    }
}

/// The process-wide guard the route uses.
pub fn global() -> &'static Guard {
    static G: std::sync::OnceLock<Guard> = std::sync::OnceLock::new();
    G.get_or_init(Guard::default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_old_helper_is_offered_a_replace() {
        assert!(not_replaceable("starting").starts_with("helper_starting"));
        assert!(not_replaceable("ava1_failed").starts_with("ava1_failed"));
        assert!(not_replaceable("not_running").starts_with("helper_not_running"));
        assert!(message(REPLACE_COOLDOWN).starts_with("replace_cooldown"));
    }

    #[test]
    fn a_second_replace_while_one_runs_is_refused() {
        let g = Guard::default();
        let t0 = Instant::now();
        let p = g.begin("10.0.0.2", t0).expect("first");
        assert_eq!(g.begin("10.0.0.2", t0).err(), Some(REPLACE_IN_PROGRESS));
        // another console is independent
        assert!(g.begin("10.0.0.3", t0).is_ok());
        drop(p);
        // finished, but the cooldown now applies
        assert_eq!(
            g.begin("10.0.0.2", t0 + Duration::from_secs(1)).err(),
            Some(REPLACE_COOLDOWN)
        );
    }

    #[test]
    fn a_replace_within_60_s_of_the_last_is_refused_and_one_after_is_allowed() {
        let g = Guard::default();
        let t0 = Instant::now();
        drop(g.begin("h", t0).unwrap());
        assert_eq!(
            g.begin("h", t0 + Duration::from_secs(59)).err(),
            Some(REPLACE_COOLDOWN)
        );
        assert!(g.begin("h", t0 + Duration::from_secs(60)).is_ok());
    }

    #[test]
    fn concurrent_callers_get_exactly_one_permit() {
        let g = std::sync::Arc::new(Guard::default());
        let t0 = Instant::now();
        let won = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hold = std::sync::Arc::new(std::sync::Barrier::new(8));
        let hs: Vec<_> = (0..8)
            .map(|_| {
                let (g, won, hold) = (g.clone(), won.clone(), hold.clone());
                std::thread::spawn(move || {
                    let r = g.begin("h", t0);
                    if r.is_ok() {
                        won.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                    hold.wait(); // every caller has tried while the winner still holds its permit
                    drop(r);
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        assert_eq!(won.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
