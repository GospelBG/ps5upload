//! node.info's firmware: the user-visible version, from the same `kern.version` string the
//! helper's STATUS frame reports (the app parses it the same way, client/src/lib/ps5Firmware.ts).
#![cfg(unix)]
use ava1_ctest::c_firmware_from_kernel as firmware;

#[test]
fn the_firmware_is_read_from_the_kernel_build_string() {
    let cases = [
        (
            "FreeBSD 11.0-RELEASE-p0 #1 r218215/releases/09.60: Jul 18 2023",
            "9.60",
        ),
        (
            "FreeBSD 11.0-RELEASE-p0 #0 r218215/releases/10.00-00 Feb  1 2024",
            "10.00",
        ),
        (
            "FreeBSD 11.0-RELEASE-p0 #0 r251133/releases/13.60: Aug 20 2026",
            "13.60",
        ),
        ("FreeBSD 11.0-RELEASE-p0 #0 r1/branch/05.10-01 x", "5.10"),
        ("FreeBSD 11.0-RELEASE PlayStation 5 12.70 build", "12.70"),
    ];
    for (kv, want) in cases {
        assert_eq!(firmware(kv, 64), want, "{kv}");
    }
}

#[test]
fn an_unrecognised_kernel_string_is_reported_whole_never_empty() {
    assert_eq!(
        firmware("FreeBSD 12.0.0 PlayStation(R)5\n", 64),
        "FreeBSD 12.0.0 PlayStation(R)5"
    );
    // Bounded by the buffer, always terminated.
    assert_eq!(firmware("FreeBSD 12.0.0 PlayStation(R)5", 8), "FreeBSD");
    assert_eq!(firmware("", 64), "unknown");
    assert_eq!(
        firmware("kernel \u{e9}\tx", 64),
        "kernel ???x",
        "printable ASCII only"
    );
}
