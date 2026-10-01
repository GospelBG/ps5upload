//! AVA1 — Adaptive Verified Assembly, version 1. Normative spec: `protocol/ava1/SPEC.md`.
pub mod conn;
pub mod crc32c;
mod error;
pub mod frame;
#[rustfmt::skip]
pub mod gen;
pub mod handshake;
pub mod hex;
pub mod keys;
mod link;
pub mod peers;
pub mod server;
pub mod session;
pub mod trust;
pub mod wire;

pub use error::Ava1Error;
