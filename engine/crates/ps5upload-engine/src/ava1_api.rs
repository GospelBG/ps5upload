//! This engine's AVA1 identity: one key pair in `<data dir>/ava/identity` (SPEC.md §5).
use std::sync::{Arc, OnceLock};

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

pub fn identity() -> Option<Arc<ava1::keys::Identity>> {
    static ID: OnceLock<Option<Arc<ava1::keys::Identity>>> = OnceLock::new();
    ID.get_or_init(|| {
        let path = crate::remote::store::data_dir()?
            .join("ava")
            .join("identity");
        match ava1::keys::Identity::load_or_create(&path) {
            Ok(i) => Some(Arc::new(i)),
            Err(e) => {
                crate::log_warn!("ava1: no identity at {}: {e}", path.display());
                None
            }
        }
    })
    .clone()
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
    fn the_identity_is_stable_and_lives_in_the_data_dir() {
        let a = super::identity();
        let b = super::identity();
        if let (Some(a), Some(b)) = (a, b) {
            assert_eq!(a.public(), b.public());
        }
    }
}
