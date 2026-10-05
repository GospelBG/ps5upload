//! CLEANUP RPC — asks the payload to recursively remove a path under one of
//! its allowlisted prefixes (see `payload/src/runtime.c cleanup_path_allowed`).
//!
//! This is *not* a general-purpose delete primitive; the payload refuses
//! anything outside the unified test sandbox:
//!
//!   - `/data/ps5upload/tests[/...]`
//!   - `/mnt/{ext,usb}<digits>/ps5upload/tests[/...]`
//!
//! Intended use: bench sweep + smoke harness reset-between-profiles so
//! generated artifacts do not pile up on PS5.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::mgmt::{self, m};

/// Read-timeout cap for the CLEANUP → CLEANUP_ACK wait.
///
/// CLEANUP is one frame that asks the payload to recursively unlink an
/// entire tree, so the ACK cannot arrive until the last file is gone.
/// Under the connection's default 30 s that made cleanup of a large
/// sandbox fail with a bare "Resource temporarily unavailable (os error
/// 35)" — observed live on FW 5.10 clearing ~20k files, where the call
/// errored despite the payload still deleting successfully (a retry
/// picked up where it left off and reported the remaining count).
///
/// Same reasoning and shape as `transfer::COMMIT_TX_ACK_TIMEOUT`: outlast
/// a realistic worst case, but still surface a genuinely wedged payload.
/// A crashed console surfaces immediately via TCP RST regardless.
const CLEANUP_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanupResult {
    pub ok: bool,
    pub path: String,
    pub removed_files: u64,
    pub removed_dirs: u64,
}

/// Send `node.cleanup` `{"path":...}` and return the parsed body.
///
/// Returns an error if the payload rejects the path (e.g. `cleanup_path_denied`) or the
/// call fails; the payload's reason is in the error text so bench tooling can display it.
pub fn cleanup_path(addr: &str, path: &str) -> Result<CleanupResult> {
    let body = serde_json::to_vec(&serde_json::json!({ "path": path }))
        .context("serialize cleanup body")?;
    // The whole recursive delete happens before the payload answers: wait longer than the default.
    let resp = mgmt::call_with(
        addr,
        m::NODE_CLEANUP,
        "CLEANUP",
        &body,
        Some(CLEANUP_ACK_TIMEOUT),
    )?;
    let parsed: CleanupResult =
        serde_json::from_slice(&resp).context("decode CLEANUP_ACK body as JSON")?;
    Ok(parsed)
}
