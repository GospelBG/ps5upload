#![cfg(unix)]
use ava1::hex;
use ava1::keys::{self, Handshake, Identity};
use ava1_ctest::{ffi, CHandshake};

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
    let (cn, sn, sid) = ([4u8; 16], [5u8; 16], [3u8; 16]);
    for lane in [0u16, 1, 8, 0xffff] {
        unsafe {
            ffi::ava1_lane_key(
                dir.as_ptr(),
                lane,
                cn.as_ptr(),
                sn.as_ptr(),
                out32.as_mut_ptr(),
            )
        };
        assert_eq!(out32, keys::lane_key(&dir, lane, &cn, &sn));
    }
    unsafe { ffi::ava1_control_key(dir.as_ptr(), out32.as_mut_ptr()) };
    assert_eq!(out32, keys::control_key(&dir));
    unsafe {
        ffi::ava1_join_tag(
            dir.as_ptr(),
            sid.as_ptr(),
            5,
            cn.as_ptr(),
            out16.as_mut_ptr(),
        )
    };
    assert_eq!(out16, keys::join_tag(&dir, &sid, 5, &cn));
    unsafe {
        ffi::ava1_join_ack_tag(
            dir.as_ptr(),
            sid.as_ptr(),
            5,
            cn.as_ptr(),
            sn.as_ptr(),
            out16.as_mut_ptr(),
        )
    };
    assert_eq!(out16, keys::join_ack_tag(&dir, &sid, 5, &cn, &sn));
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
        // The header is the AD: a frame whose header was altered (CRC fixed up) never opens.
        let mut bad_ad = ad;
        bad_ad[4] ^= 1;
        let mut tampered = rust[..len].to_vec();
        assert_ne!(
            unsafe {
                ffi::ava1_open(
                    dir.as_ptr(),
                    42,
                    bad_ad.as_ptr(),
                    bad_ad.len(),
                    tampered.as_mut_ptr(),
                    tampered.len(),
                    tag.as_ptr(),
                )
            },
            0
        );
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

#[test]
fn c_key_derivations_reproduce_the_vectors() {
    let h = |s: &str| hex::decode(s).unwrap();
    let mut n = 0;
    for l in include_str!("../../../../protocol/ava1/vectors/keys.txt").lines() {
        if l.starts_with('#') || l.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = l.split_whitespace().collect();
        let (dir, lane) = (h(f[1]), f[2].parse::<u16>().unwrap());
        let cn = h(f[3]);
        let mut out = vec![0u8; if f[0] == "lane_key" { 32 } else { 16 }];
        unsafe {
            match f[0] {
                "lane_key" => ffi::ava1_lane_key(
                    dir.as_ptr(),
                    lane,
                    cn.as_ptr(),
                    h(f[4]).as_ptr(),
                    out.as_mut_ptr(),
                ),
                "join_tag" => ffi::ava1_join_tag(
                    dir.as_ptr(),
                    h(f[5]).as_ptr(),
                    lane,
                    cn.as_ptr(),
                    out.as_mut_ptr(),
                ),
                "join_ack_tag" => ffi::ava1_join_ack_tag(
                    dir.as_ptr(),
                    h(f[5]).as_ptr(),
                    lane,
                    cn.as_ptr(),
                    h(f[4]).as_ptr(),
                    out.as_mut_ptr(),
                ),
                k => panic!("unknown vector kind {k}"),
            }
        }
        assert_eq!(hex::encode(&out), *f.last().unwrap(), "{l}");
        n += 1;
    }
    assert!(n >= 5);
}

/// A sealed frame from the Rust writer, read back by the C reader: as sent it opens; with
/// any header byte changed (CRC recomputed, so only the AD check can catch it) it does not.
#[test]
fn the_c_reader_refuses_a_frame_whose_header_was_altered() {
    use ava1::conn::FrameWriter;
    let key = [0x21u8; 32];
    let frame = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(async {
            let mut w = FrameWriter::new(Vec::new());
            w.set_key(key);
            w.send(0x09, 7, b"heartbeat body").await.unwrap();
            w.into_inner()
        });
    let open =
        |f: &[u8]| unsafe { ffi::ava1_test_conn_open_frame(key.as_ptr(), f.as_ptr(), f.len()) };
    assert_eq!(open(&frame), 0, "the untouched frame opens");
    for at in [2usize, 4, 7] {
        let mut t = frame.clone();
        t[at] ^= 0x01; // type, channel
        let crc = ava1::crc32c::crc32c(&t[..12]);
        t[12..16].copy_from_slice(&crc.to_le_bytes());
        assert_ne!(open(&t), 0, "header byte {at} altered");
    }
}
