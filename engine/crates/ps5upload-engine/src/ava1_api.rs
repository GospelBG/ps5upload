//! This engine's AVA1 identity: one key pair in `<data dir>/ava/identity` (SPEC.md §5).
use std::sync::{Arc, Mutex};

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

/// The ps5upload helper with this engine's AVA1 key in its trust slot (SPEC.md §5.1), so
/// a console launched by this engine trusts it without pairing. Every engine-side send
/// of the helper goes through here. A helper that cannot be stamped (no identity, an
/// older build without a slot) is still returned and sent: the console then opens its
/// pairing window instead.
pub fn stamped_helper(elf: &[u8]) -> Vec<u8> {
    let key = identity().map(|i| i.public());
    let (bytes, outcome) = stamp_helper_with(elf, key.as_ref());
    if let Err(why) = outcome {
        crate::log_warn!("ava1: {why}");
    }
    bytes
}

fn stamp_helper_with(elf: &[u8], key: Option<&[u8; 32]>) -> (Vec<u8>, Result<(), String>) {
    let mut bytes = elf.to_vec();
    let outcome = ava1::trust::stamp_helper(&mut bytes, key);
    (bytes, outcome)
}

/// `GET /api/ava1/identity` — the public key an app stamps into the helper ELF.
pub async fn identity_handler() -> Response {
    match identity() {
        Some(i) => Json(serde_json::json!({ "public_key": ava1::hex::encode(&i.public()) })).into_response(),
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
        let (stamped, outcome) = super::stamp_helper_with(&elf, Some(&[9; 32]));
        assert!(outcome.is_ok());
        assert_eq!(ava1::trust::read(&stamped), Some([9; 32]));
        assert_eq!(
            ava1::trust::read(&elf),
            None,
            "the bundled image is not modified"
        );
        // No identity, or an image without a slot: unchanged bytes and a reason to log.
        let (same, outcome) = super::stamp_helper_with(&elf, None);
        assert!(outcome.is_err() && same == elf);
        let old_build = vec![0x22u8; 512];
        let (same, outcome) = super::stamp_helper_with(&old_build, Some(&[9; 32]));
        assert!(outcome.is_err() && same == old_build);
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
