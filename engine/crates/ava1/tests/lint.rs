//! The payload's AVA1 code must never use the wall clock for timing: a settimeofday on
//! the console (date sync) once looked like a resume and woke the wake watchdog (#289).
#[test]
fn payload_ava1_uses_only_monotonic_clocks() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../payload/ava1");
    let mut checked = 0;
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().and_then(|x| x.to_str()) != Some("c") {
            continue;
        }
        let src = std::fs::read_to_string(&p).unwrap();
        for bad in ["gettimeofday", "CLOCK_REALTIME"] {
            assert!(
                !src.contains(bad),
                "{} uses {bad}; use CLOCK_MONOTONIC",
                p.display()
            );
        }
        checked += 1;
    }
    assert!(
        checked >= 8,
        "found only {checked} C files in {}",
        dir.display()
    );
}
