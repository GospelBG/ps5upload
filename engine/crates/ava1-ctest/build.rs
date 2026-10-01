//! Compiles the payload's AVA1 C on the host so `cargo test` can check it against Rust.
use std::path::PathBuf;

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        return; // POSIX C; the tests are #![cfg(unix)].
    }
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let p = here.join("../../../payload");
    let ava1 = p.join("ava1");
    let mono = p.join("third_party/monocypher");
    cc::Build::new()
        .file(mono.join("monocypher.c"))
        .include(&mono)
        .warnings(false)
        .compile("ava1third");
    cc::Build::new()
        .files([
            ava1.join("ava1_wire.c"),
            ava1.join("ava1_frame.c"),
            ava1.join("ava1_keys.c"),
            ava1.join("ava1_noise.c"),
            ava1.join("gen/ava1_gen.c"),
            here.join("csrc/sizes.c"),
        ])
        .include(&ava1)
        .include(ava1.join("gen"))
        .include(&mono)
        .warnings(true)
        .extra_warnings(true)
        .warnings_into_errors(true)
        .compile("ava1c");
    println!("cargo:rerun-if-changed={}", ava1.display());
    println!("cargo:rerun-if-changed={}", mono.display());
    println!("cargo:rerun-if-changed=csrc");
}
