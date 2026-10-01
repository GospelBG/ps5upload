#![cfg(unix)]
use ava1::hex;
use ava1::keys::{self, Handshake, Identity};
use ava1_ctest::{ffi, CHandshake};
use std::ffi::CString;

fn v() -> serde_json::Value {
    serde_json::from_str(include_str!(
        "../../../../protocol/ava1/vectors/noise_xx.json"
    ))
    .unwrap()
}
fn b32(v: &serde_json::Value, k: &str) -> [u8; 32] {
    hex::decode(v[k].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap()
}

#[test]
fn the_c_noise_struct_layout_matches() {
    assert_eq!(
        unsafe { ffi::ava1_test_sizeof_noise() },
        std::mem::size_of::<ffi::CNoise>()
    );
}

#[test]
fn c_reproduces_the_published_vector() {
    let v = v();
    let pro = hex::decode(v["init_prologue"].as_str().unwrap()).unwrap();
    let mut i = CHandshake::new(
        true,
        b32(&v, "init_static"),
        b32(&v, "init_ephemeral"),
        &pro,
    );
    let mut r = CHandshake::new(
        false,
        b32(&v, "resp_static"),
        b32(&v, "resp_ephemeral"),
        &pro,
    );
    for (n, m) in v["messages"].as_array().unwrap().iter().take(3).enumerate() {
        let payload = hex::decode(m["payload"].as_str().unwrap()).unwrap();
        let (w, rd) = if n % 2 == 0 {
            (&mut i, &mut r)
        } else {
            (&mut r, &mut i)
        };
        let msg = w.write(&payload).unwrap();
        assert_eq!(
            hex::encode(&msg),
            m["ciphertext"].as_str().unwrap(),
            "message {n}"
        );
        assert_eq!(rd.read(&msg).unwrap(), payload);
    }
    assert_eq!(
        hex::encode(&i.hash()),
        v["handshake_hash"].as_str().unwrap()
    );
    assert_eq!(i.split(), r.split());
}

#[test]
fn c_and_snow_complete_handshakes_with_each_other() {
    for c_is_initiator in [true, false] {
        let (a, b) = (
            keys::random_bytes::<32>().unwrap(),
            keys::random_bytes::<32>().unwrap(),
        );
        let eph = keys::random_bytes::<32>().unwrap();
        let rust_id = Identity::from_secret(b);
        let mut c = CHandshake::new(c_is_initiator, a, eph, keys::PROLOGUE);
        let mut r = if c_is_initiator {
            Handshake::responder(&rust_id)
        } else {
            Handshake::initiator(&rust_id)
        }
        .unwrap();
        for step in 0..3 {
            let c_turn = (step % 2 == 0) == c_is_initiator;
            if c_turn {
                let m = c.write(format!("c{step}").as_bytes()).unwrap();
                assert_eq!(r.read(&m).unwrap(), format!("c{step}").as_bytes());
            } else {
                let m = r.write(format!("r{step}").as_bytes()).unwrap();
                assert_eq!(c.read(&m).unwrap(), format!("r{step}").as_bytes());
            }
        }
        assert_eq!(c.remote_static(), rust_id.public());
        let keys = r.finish();
        assert_eq!(c.hash(), keys.hash);
        assert_eq!(c.split(), (keys.c2s, keys.s2c));
    }
}

#[test]
fn c_derivations_and_sealing_match_rust() {
    let dir = [0x11u8; 32];
    let mut out32 = [0u8; 32];
    let mut out16 = [0u8; 16];
    for lane in [0u16, 1, 8, 0xffff] {
        unsafe { ffi::ava1_lane_key(dir.as_ptr(), lane, out32.as_mut_ptr()) };
        assert_eq!(out32, keys::lane_key(&dir, lane));
    }
    let (sid, nonce) = ([3u8; 16], [4u8; 16]);
    for label in ["join", "join-ack"] {
        let l = CString::new(label).unwrap();
        unsafe {
            ffi::ava1_join_tag(
                dir.as_ptr(),
                l.as_ptr(),
                sid.as_ptr(),
                5,
                nonce.as_ptr(),
                out16.as_mut_ptr(),
            )
        };
        assert_eq!(
            out16,
            keys::join_tag(&dir, label.as_bytes(), &sid, 5, &nonce)
        );
    }
    let hash = [0x77u8; 64];
    assert_eq!(
        unsafe { ffi::ava1_pairing_code(hash.as_ptr()) },
        keys::pairing_code(&hash)
    );

    for len in [0usize, 1, 15, 16, 17, 4096, 70_000] {
        let body: Vec<u8> = (0..len).map(|i| i as u8).collect();
        let ad = [9u8; 12];
        let mut rust = body.clone();
        keys::seal(&dir, 42, &ad, &mut rust);
        let mut c = body.clone();
        let mut mac = [0u8; 16];
        unsafe {
            ffi::ava1_seal(
                dir.as_ptr(),
                42,
                ad.as_ptr(),
                ad.len(),
                c.as_mut_ptr(),
                c.len(),
                mac.as_mut_ptr(),
            )
        };
        c.extend_from_slice(&mac);
        assert_eq!(c, rust, "len {len}");
        let (mut ct, tag) = (rust[..len].to_vec(), rust[len..].to_vec());
        assert_eq!(
            unsafe {
                ffi::ava1_open(
                    dir.as_ptr(),
                    42,
                    ad.as_ptr(),
                    ad.len(),
                    ct.as_mut_ptr(),
                    ct.len(),
                    tag.as_ptr(),
                )
            },
            0
        );
        assert_eq!(ct, body);
        let mut forged = rust[..len].to_vec();
        assert_ne!(
            unsafe {
                ffi::ava1_open(
                    dir.as_ptr(),
                    43,
                    ad.as_ptr(),
                    ad.len(),
                    forged.as_mut_ptr(),
                    forged.len(),
                    tag.as_ptr(),
                )
            },
            0
        );
    }
}

#[test]
fn c_refuses_garbage_and_out_of_turn_messages() {
    let mut resp = CHandshake::new(false, [1; 32], [2; 32], keys::PROLOGUE);
    assert!(resp.write(b"").is_err(), "a responder cannot speak first");
    assert!(
        resp.read(&[0u8; 5]).is_err(),
        "message 1 is at least 32 bytes"
    );
    let mut init = CHandshake::new(true, [3; 32], [4; 32], keys::PROLOGUE);
    let m1 = init.write(b"").unwrap();
    let mut resp = CHandshake::new(false, [1; 32], [2; 32], keys::PROLOGUE);
    resp.read(&m1).unwrap();
    let mut m2 = resp.write(b"").unwrap();
    m2[50] ^= 1;
    assert!(init.read(&m2).is_err(), "a tampered message 2 is refused");
}
