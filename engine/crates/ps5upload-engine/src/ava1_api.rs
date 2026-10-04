//! This engine's AVA1 identity: one key pair in `<data dir>/ava/identity` (SPEC.md §5).
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::{ConnectInfo, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

pub fn identity() -> Option<Arc<ava1::keys::Identity>> {
    // Only a success is cached: a transient failure (data dir not there yet) retries next call.
    static ID: Mutex<Option<Arc<ava1::keys::Identity>>> = Mutex::new(None);
    let mut slot = ID.lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_none() {
        let path = crate::remote::store::data_dir()?
            .join("ava")
            .join("identity");
        match ava1::keys::Identity::load_or_create(&path) {
            Ok(i) => *slot = Some(Arc::new(i)),
            Err(e) => {
                crate::log_warn!("ava1: no identity at {}: {e}", path.display());
                return None;
            }
        }
    }
    slot.clone()
}

/// The launch tokens this engine issued (SPEC.md §5.2), in `<data dir>/ava/launch_tokens`.
/// Every stamp takes a fresh one, so a helper this engine launched pairs with no code.
pub fn launch_tokens() -> Option<ava1::launch::LaunchTokens> {
    let path = crate::remote::store::data_dir()?
        .join("ava")
        .join("launch_tokens");
    Some(ava1::launch::LaunchTokens::at(&path))
}

/// SPEC.md §14.3: removes the AVA1 job directories (`ava/jobs`, `ava/send`) idle for more than
/// seven days, except those of jobs running in this engine. Blocking file I/O.
pub fn sweep_journals() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let n = ps5upload_ava1::pool().gc_journals(now, ps5upload_ava1::JOURNAL_MAX_AGE_S);
    if n > 0 {
        crate::log_info!("ava1: removed {n} expired job director(ies)");
    }
}

/// Runs `run` once now and then every `period`, each time on a blocking thread so the sweep's
/// file I/O never occupies the async runtime. A panic in one run does not end the schedule.
pub(crate) fn spawn_periodic(
    period: std::time::Duration,
    run: impl Fn() + Send + Sync + 'static,
) -> tokio::task::JoinHandle<()> {
    let run = Arc::new(run);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(period);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await; // the first tick is immediate: the sweep at start
            let r = run.clone();
            let _ = tokio::task::spawn_blocking(move || r()).await;
        }
    })
}

/// The engine's journal sweep: at startup, then once a day (SPEC.md §14.3).
pub fn spawn_journal_gc() {
    spawn_periodic(ps5upload_ava1::JOURNAL_GC_EVERY, sweep_journals);
}

/// A token to stamp, or `None` (and a log line) when it could not be recorded — a token
/// this side did not keep would pair nothing, so the key alone is stamped instead.
fn fresh_token() -> Option<[u8; 16]> {
    match launch_tokens()?.issue() {
        Ok(t) => Some(t),
        Err(e) => {
            crate::log_warn!("ava1: launch token not recorded: {e}");
            None
        }
    }
}

/// The ps5upload helper with this engine's AVA1 key in its trust slot (SPEC.md §5.1), so
/// a console launched by this engine trusts it without pairing. Every engine-side send
/// of the helper goes through here. A helper that cannot be stamped (no identity, an
/// older build without a slot) is still returned and sent: the console then opens its
/// pairing window instead. The stamp also carries a fresh launch token (SPEC.md §5.2),
/// so the console proves to this engine that it is the helper it launched and no pairing
/// code is shown either way.
pub fn stamped_helper(elf: &[u8]) -> Vec<u8> {
    let key = identity().map(|i| i.public());
    let (bytes, outcome) = stamp_helper_with(elf, key.as_ref(), fresh_token);
    if let Err(why) = outcome {
        crate::log_warn!("ava1: {why}");
    }
    bytes
}

/// The identity response. The token is a secret — it is what proves to this engine that a
/// console is the helper this engine launched — so it goes only to a loopback caller, and
/// only then is one minted at all. The API is loopback-guarded anyway, but an operator can
/// allow extra peers, and a browser on the LAN — or the Docker engine's page — has no
/// business holding it; that engine stamps server-side and keeps its tokens to itself.
///
/// Residual: a forwarder on this host (a reverse proxy, an `ssh -L` tunnel) makes a remote
/// caller look local. Every route behind the guard has the same property, and a token
/// alone pairs nothing — it also needs the handshake hash of a live session with us.
fn identity_body(
    public_key: &str,
    peer: std::net::IpAddr,
    mint: impl FnOnce() -> Option<[u8; 16]>,
) -> serde_json::Value {
    let mut body = serde_json::json!({ "public_key": public_key });
    if peer.is_loopback() {
        if let Some(t) = mint() {
            body["launch_token"] = ava1::hex::encode(&t).into();
        }
    }
    body
}

fn stamp_helper_with(
    elf: &[u8],
    key: Option<&[u8; 32]>,
    token: impl FnOnce() -> Option<[u8; 16]>,
) -> (Vec<u8>, Result<(), String>) {
    let mut bytes = elf.to_vec();
    let outcome = ava1::trust::stamp_helper(&mut bytes, key, token);
    (bytes, outcome)
}

/// `GET /api/ava1/identity` — the public key an app stamps into the helper ELF, and a
/// fresh launch token to go with it (SPEC.md §5.2).
pub async fn identity_handler(ConnectInfo(peer): ConnectInfo<SocketAddr>) -> Response {
    match identity() {
        Some(i) => {
            let key = ava1::hex::encode(&i.public());
            Json(identity_body(&key, peer.ip(), fresh_token)).into_response()
        }
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no AVA1 identity (the engine has no data directory)" })),
        )
            .into_response(),
    }
}

/// The pairing dialog's view of a console, as the JSON the client keys on:
/// `state` is `none` (nothing in progress), `code` (compare `code` with the console's
/// screen; `console_name` is what the console calls itself), `accepted` (paired) or
/// `closed` (the console's pairing window is shut). A transport failure is a 502.
fn pairing_body(
    r: Result<ps5upload_ava1::Pairing, ava1::Ava1Error>,
) -> (StatusCode, serde_json::Value) {
    use ps5upload_ava1::Pairing;
    match r {
        Ok(Pairing::Paired) => (StatusCode::OK, serde_json::json!({ "state": "accepted" })),
        Ok(Pairing::Closed) => (StatusCode::OK, serde_json::json!({ "state": "closed" })),
        Ok(Pairing::Code { code, peer_name }) => (
            StatusCode::OK,
            // Zero-padded: the console shows six digits, and 012345 is not 12345.
            serde_json::json!({ "state": "code", "code": format!("{code:06}"), "console_name": peer_name }),
        ),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            serde_json::json!({ "state": "none", "error": e.to_string() }),
        ),
    }
}

#[derive(serde::Deserialize)]
pub struct PairingQuery {
    addr: Option<String>,
}

/// `GET /api/ava1/pairing?addr=` — starts (or re-reads) the pairing handshake with the
/// console and reports the code to compare. The handshake is held until confirmed.
pub async fn pairing_handler(
    State(state): State<crate::AppState>,
    Query(q): Query<PairingQuery>,
) -> Response {
    let addr = q.addr.unwrap_or_else(|| state.default_ps5_addr.clone());
    let (code, body) = pairing_body(ps5upload_ava1::pool().pairing_status(&addr).await);
    (code, Json(body)).into_response()
}

#[derive(serde::Deserialize)]
pub struct PairingConfirm {
    addr: Option<String>,
}

/// `POST /api/ava1/pairing/confirm` `{addr}` — the user saw matching codes. A refusal by the
/// console, or a handshake that is gone (the console's window timed out), is `closed`:
/// the dialog then explains how to reopen the window.
pub async fn pairing_confirm_handler(
    State(state): State<crate::AppState>,
    Json(req): Json<PairingConfirm>,
) -> Response {
    let addr = req.addr.unwrap_or_else(|| state.default_ps5_addr.clone());
    let r = ps5upload_ava1::pool()
        .confirm_pairing(&addr)
        .await
        .map(|()| ps5upload_ava1::Pairing::Paired);
    let r = match r {
        Err(ava1::Ava1Error::NotPaired) => Ok(ps5upload_ava1::Pairing::Closed),
        Err(ava1::Ava1Error::Refused { code, .. }) if code == ava1::gen::ERR_PAIRING_CLOSED => {
            Ok(ps5upload_ava1::Pairing::Closed)
        }
        other => other,
    };
    let (code, body) = pairing_body(r);
    (code, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use ps5upload_ava1::Pairing;

    #[test]
    fn the_pairing_body_names_the_state_and_pads_the_code() {
        let (st, b) = super::pairing_body(Ok(Pairing::Code {
            code: 4821,
            peer_name: "PS5-Pro".into(),
        }));
        assert_eq!(st, axum::http::StatusCode::OK);
        assert_eq!(b["state"], "code");
        assert_eq!(b["code"], "004821");
        assert_eq!(b["console_name"], "PS5-Pro");
        assert_eq!(
            super::pairing_body(Ok(Pairing::Paired)).1["state"],
            "accepted"
        );
        assert_eq!(
            super::pairing_body(Ok(Pairing::Closed)).1["state"],
            "closed"
        );
        let (st, b) = super::pairing_body(Err(ava1::Ava1Error::Timeout));
        assert_eq!(st, axum::http::StatusCode::BAD_GATEWAY);
        assert_eq!(b["state"], "none");
        assert!(b["error"].is_string());
    }
    #[test]
    fn the_helper_is_stamped_with_the_given_key_and_sent_regardless() {
        let mut elf = vec![0x11u8; 4096];
        elf[1000..1064].fill(0);
        elf[1000..1009].copy_from_slice(ava1::trust::MAGIC);
        let (stamped, outcome) = super::stamp_helper_with(&elf, Some(&[9; 32]), || None);
        assert!(outcome.is_ok());
        assert_eq!(ava1::trust::read(&stamped), Some([9; 32]));
        assert_eq!(
            ava1::trust::read(&elf),
            None,
            "the bundled image is not modified"
        );
        // No identity, or an image without a slot: unchanged bytes and a reason to log.
        let (same, outcome) = super::stamp_helper_with(&elf, None, || Some([4; 16]));
        assert!(outcome.is_err() && same == elf);
        let old_build = vec![0x22u8; 512];
        let (same, outcome) =
            super::stamp_helper_with(&old_build, Some(&[9; 32]), || Some([4; 16]));
        assert!(outcome.is_err() && same == old_build);
    }

    #[test]
    fn a_stamp_carries_the_token_that_was_minted_for_it() {
        let mut elf = vec![0x11u8; 4096];
        elf[1000..1064].fill(0);
        elf[1000..1009].copy_from_slice(ava1::trust::MAGIC);
        let (stamped, outcome) =
            super::stamp_helper_with(&elf, Some(&[9; 32]), || Some([0xa5; 16]));
        assert!(outcome.is_ok());
        assert_eq!(
            (
                ava1::trust::read(&stamped),
                ava1::trust::read_token(&stamped)
            ),
            (Some([9; 32]), Some([0xa5; 16]))
        );
        // A token that could not be minted stamps the key alone: the pairing window is
        // the fallback, never a broken slot.
        let (key_only, _) = super::stamp_helper_with(&elf, Some(&[9; 32]), || None);
        assert_eq!(ava1::trust::read_token(&key_only), None);
    }

    #[test]
    fn a_launch_token_is_issued_to_a_local_caller_and_to_nobody_else() {
        let minted = std::cell::Cell::new(0u32);
        let mint = || {
            minted.set(minted.get() + 1);
            Some([7u8; 16])
        };
        let local = super::identity_body(
            "aabb",
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            mint,
        );
        assert_eq!(local["public_key"], "aabb");
        assert_eq!(
            local["launch_token"].as_str(),
            Some(ava1::hex::encode(&[7u8; 16]).as_str())
        );
        assert_eq!(minted.get(), 1);
        let remote = super::identity_body("aabb", "192.168.1.5".parse().unwrap(), mint);
        assert_eq!(remote["public_key"], "aabb");
        assert!(
            remote.get("launch_token").is_none(),
            "a caller the engine does not count as local never sees the token: {remote}"
        );
        assert_eq!(minted.get(), 1, "and none is minted for it either");
    }

    #[test]
    fn the_identity_is_stable_and_lives_in_the_data_dir() {
        let a = super::identity();
        let b = super::identity();
        if let (Some(a), Some(b)) = (a, b) {
            assert_eq!(a.public(), b.public());
        }
    }

    #[tokio::test]
    async fn the_journal_sweep_runs_at_start_and_then_on_every_period() {
        let n = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let n2 = n.clone();
        let h = super::spawn_periodic(std::time::Duration::from_millis(20), move || {
            n2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while n.load(std::sync::atomic::Ordering::SeqCst) < 3 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the sweep ran at start and repeated");
        h.abort();
    }

    #[test]
    fn the_sweep_is_daily_and_expires_after_seven_days() {
        assert_eq!(ps5upload_ava1::JOURNAL_GC_EVERY.as_secs(), 86_400);
        assert_eq!(ps5upload_ava1::JOURNAL_MAX_AGE_S, 7 * 86_400);
    }
}
