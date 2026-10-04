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
            // Whole identifiers only: strftime( formats a display timestamp (the event log), not timing.
            let hit = src.match_indices(bad).any(|(i, _)| {
                i == 0
                    || !src.as_bytes()[i - 1].is_ascii_alphanumeric()
                        && src.as_bytes()[i - 1] != b'_'
            });
            assert!(!hit, "{} uses {bad}; use CLOCK_MONOTONIC", p.display());
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

/// The console's liveness defaults must match the engine's (`Timing::default`, SPEC.md
/// section 6): a 2 s ping and a 12 s verdict. The glue file is not built on the host, so
/// this reads its text.
#[test]
fn the_payload_glue_uses_the_spec_liveness_defaults() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let glue = std::fs::read_to_string(root.join("payload/src/ava1_glue.c")).unwrap();
    assert!(glue.contains("cfg.ping_every_ms = 2000;"), "ping is 2 s");
    assert!(
        glue.contains("cfg.dead_after_ms = 12000;"),
        "dead_after is 12 s"
    );
}

/// Review 006 #1: a re-key restarts the AEAD counter at 0, so it may only happen where a
/// connection gets its keys (handshake, lane join): never from the data plane. This pins
/// every non-test `set_key` call site; a new one must be audited (AUDIT-nonce.md) first.
#[test]
fn set_key_is_called_only_where_a_connection_is_keyed() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut sites = Vec::new();
    for e in std::fs::read_dir(&src).unwrap() {
        let p = e.unwrap().path();
        let text = std::fs::read_to_string(&p).unwrap();
        // Production code only: everything before the file's test module.
        let prod = text.split("#[cfg(test)]").next().unwrap();
        let n = prod.matches(".set_key(").count();
        if n > 0 {
            sites.push((p.file_name().unwrap().to_string_lossy().into_owned(), n));
        }
    }
    sites.sort();
    assert_eq!(
        sites,
        vec![
            ("handshake.rs".to_string(), 4),
            ("server.rs".to_string(), 2),
            ("session.rs".to_string(), 2),
        ]
    );
}

/// The C side: a send/recv counter is written only at its increment (and zeroed with the
/// connection); no other code may assign it.
#[test]
fn c_counters_are_only_incremented() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../payload");
    let mut files = Vec::new();
    sources(&root.join("ava1"), &mut files);
    files.push(root.join("src/ava1_glue.c"));
    for p in &files {
        let src = std::fs::read_to_string(p).unwrap();
        for (n, line) in src.lines().enumerate() {
            for f in ["send_ctr", "recv_ctr"] {
                if let Some(i) = line.find(f) {
                    let rest = line[i + f.len()..].trim_start();
                    let writes = rest.starts_with("= ") && !rest.starts_with("== ");
                    assert!(!writes, "{}:{} assigns {f}: {line}", p.display(), n + 1);
                }
            }
        }
    }
}
