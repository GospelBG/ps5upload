#![cfg(unix)]
use ava1::frame::Header;
use ava1::gen;
use ava1::wire::SplitMix;
use ava1_ctest::*;

#[test]
fn crc_and_headers_match_rust() {
    assert_eq!(c_crc32c(b"123456789"), 0xE306_9283);
    let mut rng = SplitMix(1);
    for _ in 0..1000 {
        let h = Header {
            ty: rng.next_u64() as u8,
            flags: rng.next_u64() as u8,
            channel: rng.next_u64() as u32,
            body_len: rng.below(ava1::frame::MAX_BODY as u64 + 1) as u32,
        };
        let b = h.encode();
        assert_eq!(c_header_encode(h), b);
        assert_eq!(c_header_decode(&b), Ok(h));
    }
    let mut bad = Header {
        ty: 1,
        flags: 0,
        channel: 0,
        body_len: 0,
    }
    .encode();
    bad[3] ^= 0x40;
    assert!(c_header_decode(&bad).is_err());
}

#[test]
fn golden_vectors_round_trip_in_c() {
    for l in include_str!("../../../../protocol/ava1/vectors/messages.txt").lines() {
        if l.starts_with('#') || l.trim().is_empty() {
            continue;
        }
        let (name, h) = l.split_once(' ').unwrap();
        let b = ava1::hex::decode(h).unwrap();
        assert_eq!(c_roundtrip(name, &b), Ok(b.clone()), "{name}");
    }
}

#[test]
fn rust_samples_round_trip_in_c() {
    let mut rng = SplitMix(42);
    for _ in 0..500 {
        for name in gen::ALL {
            let b = gen::sample(name, &mut rng).unwrap();
            assert_eq!(c_roundtrip(name, &b), Ok(b.clone()), "{name}");
        }
    }
}

#[test]
fn c_and_rust_agree_on_garbage() {
    // Random bytes: both sides must accept exactly the same inputs, and re-encode them identically.
    let mut rng = SplitMix(9);
    for _ in 0..20_000 {
        let name = gen::ALL[rng.below(gen::ALL.len() as u64) as usize];
        let n = rng.below(120) as usize;
        let mut b = vec![0u8; n];
        rng.fill(&mut b);
        let rust = gen::roundtrip(name, &b).unwrap();
        let c = c_roundtrip(name, &b);
        assert_eq!(rust.is_ok(), c.is_ok(), "{name} {}", ava1::hex::encode(&b));
        if let (Ok(r), Ok(c)) = (rust, c) {
            assert_eq!(r, c);
        }
    }
}
