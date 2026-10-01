//! Compiles the payload's AVA1 C on the host so `cargo test` can check it against Rust.
use std::path::PathBuf;

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        return; // POSIX C; the tests are #![cfg(unix)].
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let ava1 = root.join("payload/ava1");
    let ours = [
        ava1.join("ava1_wire.c"),
        ava1.join("ava1_frame.c"),
        ava1.join("gen/ava1_gen.c"),
    ];
    cc::Build::new()
        .files(&ours)
        .include(&ava1)
        .include(ava1.join("gen"))
        .warnings(true)
        .extra_warnings(true)
        .warnings_into_errors(true)
        .compile("ava1c");
    println!("cargo:rerun-if-changed={}", ava1.display());
}
