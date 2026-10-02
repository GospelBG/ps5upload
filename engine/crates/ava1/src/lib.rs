//! AVA1 — Adaptive Verified Assembly, version 1. Normative spec: `protocol/ava1/SPEC.md`.
pub mod conn;
pub mod crc32c;
mod error;
pub mod frame;
#[rustfmt::skip]
pub mod gen;
pub mod governor;
pub mod handshake;
pub mod hex;
pub mod journal;
pub mod keys;
pub mod launch;
mod link;
pub mod manifest;
pub mod peers;
pub mod ranges;
pub mod router;
pub mod server;
pub mod session;
pub mod source;
pub mod trust;
pub mod verify;
pub mod wire;

pub use error::Ava1Error;
