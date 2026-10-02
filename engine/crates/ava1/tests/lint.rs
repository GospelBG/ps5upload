//! The payload's AVA1 code must never use the wall clock for timing: a settimeofday on
//! the console (date sync) once looked like a resume and woke the wake watchdog (#289).
use std::path::Path;

/// Every .c and .h under `dir`, subdirectories (gen/, fuzz/) included.
fn sources(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            sources(&p, out);
        } else if matches!(p.extension().and_then(|x| x.to_str()), Some("c" | "h")) {
            out.push(p);
        }
    }
}

#[test]
fn payload_ava1_uses_only_monotonic_clocks() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let mut files = Vec::new();
    sources(&root.join("payload/ava1"), &mut files);
    files.push(root.join("payload/src/ava1_glue.c"));
    files.push(root.join("payload/include/ava1_glue.h"));
    for p in &files {
        let src = std::fs::read_to_string(p).unwrap();
        for bad in ["gettimeofday", "CLOCK_REALTIME", "clock_settime", "ftime("] {
            assert!(
                !src.contains(bad),
                "{} uses {bad}; use CLOCK_MONOTONIC",
                p.display()
            );
        }
    }
    let count = |ext: &str, under: &str| {
        files
            .iter()
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some(ext))
            .filter(|p| p.to_string_lossy().contains(under))
            .count()
    };
    assert!(
        count("c", "ava1") >= 9,
        "found only {} C files",
        count("c", "ava1")
    );
    assert!(
        count("h", "ava1") >= 8,
        "found only {} headers",
        count("h", "ava1")
    );
    assert!(
        count("c", "/gen/") >= 1 && count("h", "/gen/") >= 1,
        "the generated sources under gen/ were not scanned"
    );
}
