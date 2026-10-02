//! This engine's AVA1 identity: one key pair in `<data dir>/ava/identity` (SPEC.md §5).
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::ConnectInfo;
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
///
/// The token is a secret: it is what proves to this engine that a console is the helper
/// this engine launched. Only a loopback caller gets one. The API is loopback-guarded
/// anyway, but an operator can allow extra peers, and a browser on the LAN — or the
/// Docker engine's page — has no business holding it; that engine stamps server-side and
/// keeps its tokens to itself.
pub async fn identity_handler(ConnectInfo(peer): ConnectInfo<SocketAddr>) -> Response {
    match identity() {
        Some(i) => {
            let mut body = serde_json::json!({ "public_key": ava1::hex::encode(&i.public()) });
            if peer.ip().is_loopback() {
                if let Some(t) = fresh_token() {
                    body["launch_token"] = ava1::hex::encode(&t).into();
                }
            }
            Json(body).into_response()
        }
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no AVA1 identity (the engine has no data directory)" })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
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
    fn the_identity_is_stable_and_lives_in_the_data_dir() {
        let a = super::identity();
        let b = super::identity();
        if let (Some(a), Some(b)) = (a, b) {
            assert_eq!(a.public(), b.public());
        }
    }
}
