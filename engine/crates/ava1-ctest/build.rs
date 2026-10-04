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
    let b3 = p.join("third_party/blake3");
    // Portable only on the host: the x86 assembly and NEON paths are the payload's concern;
    // the test pins the algorithm, and blake3_hash_many dispatches to the portable code.
    cc::Build::new()
        .files([
            b3.join("blake3.c"),
            b3.join("blake3_dispatch.c"),
            b3.join("blake3_portable.c"),
        ])
        .define("BLAKE3_NO_SSE2", None)
        .define("BLAKE3_NO_SSE41", None)
        .define("BLAKE3_NO_AVX2", None)
        .define("BLAKE3_NO_AVX512", None)
        .define("BLAKE3_USE_NEON", "0")
        .include(&b3)
        .warnings(false)
        .compile("ava1b3");
    cc::Build::new()
        .files([
            ava1.join("ava1_wire.c"),
            ava1.join("ava1_frame.c"),
            ava1.join("ava1_keys.c"),
            ava1.join("ava1_noise.c"),
            ava1.join("ava1_aead.c"),
            ava1.join("ava1_conn.c"),
            ava1.join("ava1_store.c"),
            ava1.join("ava1_server.c"),
            ava1.join("ava1_trust.c"),
            ava1.join("ava1_ranges.c"),
            ava1.join("ava1_journal.c"),
            ava1.join("ava1_tune.c"),
            ava1.join("ava1_thread.c"),
            ava1.join("ava1_manifest.c"),
            ava1.join("ava1_job.c"),
            ava1.join("ava1_data.c"),
            ava1.join("ava1_apply.c"),
            ava1.join("ava1_recv.c"),
            ava1.join("ava1_send.c"),
            ava1.join("ava1_copy.c"),
            ava1.join("ava1_calibrate.c"),
            ava1.join("ava1_op.c"),
            ava1.join("ava1_events.c"),
            ava1.join("ava1_b3.c"),
            ava1.join("platform_posix.c"),
            ava1.join("gen/ava1_gen.c"),
            p.join("src/mgmt_rpc.c"),
            p.join("src/fs_jobs.c"),
            p.join("src/net_probe.c"),
            here.join("csrc/sizes.c"),
            here.join("csrc/test_shim.c"),
            here.join("csrc/firmware_shim.c"),
        ])
        .include(&ava1)
        .include(ava1.join("gen"))
        .include(&mono)
        .include(&b3)
        // blake3_impl.h must see the same configuration in ava1_b3.c as in the
        // ava1b3 build, whose blake3_hash_many it calls.
        .define("BLAKE3_NO_SSE2", None)
        .define("BLAKE3_NO_SSE41", None)
        .define("BLAKE3_NO_AVX2", None)
        .define("BLAKE3_NO_AVX512", None)
        .define("BLAKE3_USE_NEON", "0")
        // Only for ps5_firmware.h, which the payload's AVA1 glue uses (node.info).
        .include(p.join("include"))
        .warnings(true)
        .extra_warnings(true)
        .warnings_into_errors(true)
        .compile("ava1c");
    // P3 Task 6: the real "P3 Task 6" rows of payload/src/mgmt_table.def over stub handlers
    // (csrc/mgmt_t6_shim.c), so the tests drive the real table's methods, flags and runners.
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let def = std::fs::read_to_string(p.join("src/mgmt_table.def")).unwrap();
    let mut block = String::new();
    let mut inside = false;
    for l in def.lines() {
        if l.starts_with("/* ---- P3 Task") {
            inside = l.starts_with("/* ---- P3 Task 6");
        } else if inside
            || [
                "AVA1_METHOD_APP_LAUNCH,",
                "AVA1_METHOD_APP_LIST,",
                "AVA1_METHOD_PROC_PROCESS_LIST,",
            ]
            .iter()
            .any(|m| l.contains(m))
        {
            block.push_str(l);
            block.push('\n');
        }
    }
    assert!(!block.is_empty(), "no `P3 Task 6` block in mgmt_table.def");
    std::fs::write(out.join("mgmt_t6.def"), block).unwrap();
    cc::Build::new()
        .files([
            here.join("csrc/mgmt_t6_shim.c"),
            p.join("src/sony_api_lock.c"),
        ])
        .include(&ava1)
        .include(ava1.join("gen"))
        .include(p.join("include"))
        .include(&out)
        .warnings(true)
        .extra_warnings(true)
        .warnings_into_errors(true)
        .compile("ava1t6");
    println!(
        "cargo:rerun-if-changed={}",
        p.join("src/mgmt_table.def").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        p.join("src/sony_api_lock.c").display()
    );
    // The AVX2 ChaCha20 is its own unit, built with -mavx2 only on x86-64 (where the
    // run-time CPUID check picks it); elsewhere it compiles to nothing.
    let mut avx2 = cc::Build::new();
    avx2.file(ava1.join("ava1_chacha_avx2.c"))
        .include(&ava1)
        .warnings(true)
        .extra_warnings(true)
        .warnings_into_errors(true);
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("x86_64") {
        avx2.flag("-mavx2");
    }
    avx2.compile("ava1avx2");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        println!("cargo:rustc-link-lib=pthread");
    }
    println!("cargo:rerun-if-changed={}", ava1.display());
    println!(
        "cargo:rerun-if-changed={}",
        p.join("src/mgmt_rpc.c").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        p.join("include/mgmt_rpc.h").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        p.join("src/fs_jobs.c").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        p.join("include/fs_jobs.h").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        p.join("src/net_probe.c").display()
    );
    println!("cargo:rerun-if-changed={}", mono.display());
    println!("cargo:rerun-if-changed={}", b3.display());
    println!(
        "cargo:rerun-if-changed={}",
        p.join("include/ps5_firmware.h").display()
    );
    println!("cargo:rerun-if-changed=csrc");
}
