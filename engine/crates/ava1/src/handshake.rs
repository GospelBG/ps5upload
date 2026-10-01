//! The session handshake: Noise XX in frames Hs1..Hs3, then Welcome (SPEC.md §5).
use tokio::io::{AsyncRead, AsyncWrite};

use crate::conn::{Frame, FrameReader, FrameWriter};
use crate::gen::{self, ClientInfo, HelloInfo, Hs1, Hs2, Hs3, ServerInfo, Welcome};
use crate::keys::{self, Handshake, Identity, SessionKeys};
use crate::wire::{FrameMessage, Message};
use crate::Ava1Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairingState {
    /// Both devices show this; the user confirms they match.
    pub code: u32,
    /// The server does not know us yet and must accept a PairConfirm.
    pub server_must_confirm: bool,
}

pub struct Established {
    pub keys: SessionKeys,
    pub session_id: [u8; 16],
    pub peer_key: [u8; 32],
    pub peer_name: String,
    /// `Some` until both devices have accepted each other.
    pub pairing: Option<PairingState>,
}

pub(crate) fn refused(f: &Frame) -> Ava1Error {
    match f.decode::<gen::Error>() {
        Ok(e) => Ava1Error::Refused {
            code: e.code,
            message: e.message,
        },
        Err(e) => e,
    }
}

pub async fn refuse<W: AsyncWrite + Unpin>(w: &mut FrameWriter<W>, code: u16, message: &str) {
    let _ = w
        .send_msg(
            0,
            &gen::Error {
                code,
                message: message.into(),
            },
        )
        .await;
}

pub async fn client<R, W>(
    r: &mut FrameReader<R>,
    w: &mut FrameWriter<W>,
    me: &Identity,
    my_name: &str,
    knows: impl Fn(&[u8; 32]) -> bool,
) -> Result<Established, Ava1Error>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let v = gen::PROTOCOL_VERSION;
    let mut hs = Handshake::initiator(me)?;
    let hello = HelloInfo {
        version_min: v,
        version_max: v,
        caps: 0,
    }
    .to_bytes()?;
    w.send_msg(
        0,
        &Hs1 {
            noise: hs.write(&hello)?,
        },
    )
    .await?;
    let f = r.recv().await?;
    if f.ty == gen::Error::TYPE {
        return Err(refused(&f));
    }
    let m2: Hs2 = f.decode()?;
    let info = ServerInfo::decode(&hs.read(&m2.noise)?)?;
    if info.version != v {
        return Err(Ava1Error::Version {
            min: info.version,
            max: info.version,
            ours: v,
        });
    }
    let peer_key = hs.remote_static().ok_or(Ava1Error::WeakKey)?;
    let ci = ClientInfo {
        name: Some(my_name.to_string()),
    }
    .to_bytes()?;
    w.send_msg(
        0,
        &Hs3 {
            noise: hs.write(&ci)?,
        },
    )
    .await?;
    let keys = hs.finish();
    w.set_key(keys::lane_key(&keys.c2s, 0));
    r.set_key(keys::lane_key(&keys.s2c, 0));
    let f = r.recv().await?;
    if f.ty == gen::Error::TYPE {
        return Err(refused(&f));
    }
    let welcome: Welcome = f.decode()?;
    let known = knows(&peer_key);
    let pairing = (!known || welcome.knows_you == 0).then(|| PairingState {
        code: keys::pairing_code(&keys.hash),
        server_must_confirm: welcome.knows_you == 0,
    });
    Ok(Established {
        keys,
        session_id: info.session_id,
        peer_key,
        peer_name: info.name.unwrap_or_default(),
        pairing,
    })
}

pub async fn server<R, W>(
    r: &mut FrameReader<R>,
    w: &mut FrameWriter<W>,
    hs1_frame: Frame,
    me: &Identity,
    my_name: &str,
    knows: impl Fn(&[u8; 32]) -> bool,
    pairing_open: bool,
) -> Result<Established, Ava1Error>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let v = gen::PROTOCOL_VERSION;
    let m1: Hs1 = hs1_frame.decode()?;
    let mut hs = Handshake::responder(me)?;
    let hello = HelloInfo::decode(&hs.read(&m1.noise)?)?;
    if hello.version_min > v || hello.version_max < v {
        refuse(
            w,
            gen::ERR_UNSUPPORTED_VERSION,
            "no protocol version in common",
        )
        .await;
        return Err(Ava1Error::Version {
            min: hello.version_min,
            max: hello.version_max,
            ours: v,
        });
    }
    let session_id: [u8; 16] = keys::random_bytes()?;
    let si = ServerInfo {
        version: v,
        caps: 0,
        session_id,
        name: Some(my_name.to_string()),
    }
    .to_bytes()?;
    w.send_msg(
        0,
        &Hs2 {
            noise: hs.write(&si)?,
        },
    )
    .await?;
    let m3: Hs3 = r.recv().await?.decode()?;
    let ci = ClientInfo::decode(&hs.read(&m3.noise)?)?;
    let peer_key = hs.remote_static().ok_or(Ava1Error::WeakKey)?;
    let keys = hs.finish();
    w.set_key(keys::lane_key(&keys.s2c, 0));
    r.set_key(keys::lane_key(&keys.c2s, 0));
    let known = knows(&peer_key);
    if !known && !pairing_open {
        refuse(
            w,
            gen::ERR_PAIRING_CLOSED,
            "this device is not paired and pairing is closed",
        )
        .await;
        return Err(Ava1Error::NotPaired);
    }
    w.send_msg(
        0,
        &Welcome {
            knows_you: u8::from(known),
        },
    )
    .await?;
    Ok(Established {
        pairing: (!known).then(|| PairingState {
            code: keys::pairing_code(&keys.hash),
            server_must_confirm: true,
        }),
        keys,
        session_id,
        peer_key,
        peer_name: ci.name.unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::{FrameReader, FrameWriter};
    use tokio::io::{split, DuplexStream, ReadHalf, WriteHalf};

    type R = FrameReader<ReadHalf<DuplexStream>>;
    type W = FrameWriter<WriteHalf<DuplexStream>>;

    fn pipe() -> ((R, W), (R, W)) {
        let (a, b) = tokio::io::duplex(1 << 16);
        let ((ar, aw), (br, bw)) = (split(a), split(b));
        (
            (FrameReader::new(ar), FrameWriter::new(aw)),
            (FrameReader::new(br), FrameWriter::new(bw)),
        )
    }

    struct Ends {
        c: Result<Established, Ava1Error>,
        s: Result<Established, Ava1Error>,
        c_pub: [u8; 32],
        s_pub: [u8; 32],
    }

    async fn run(server_knows_client: bool, client_knows_server: bool, pairing_open: bool) -> Ends {
        let (c_id, s_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (c_pub, s_pub) = (c_id.public(), s_id.public());
        let ((mut cr, mut cw), (mut sr, mut sw)) = pipe();
        let c_fut = async move {
            client(&mut cr, &mut cw, &c_id, "laptop", |k| {
                client_knows_server && *k == s_pub
            })
            .await
        };
        let s_fut = async move {
            let first = sr.recv().await?;
            server(
                &mut sr,
                &mut sw,
                first,
                &s_id,
                "console",
                |k| server_knows_client && *k == c_pub,
                pairing_open,
            )
            .await
        };
        let (c, s) = tokio::join!(c_fut, s_fut);
        Ends { c, s, c_pub, s_pub }
    }

    #[tokio::test]
    async fn paired_devices_agree_on_keys_peers_and_names() {
        let e = run(true, true, false).await;
        let (c, s) = (e.c.unwrap(), e.s.unwrap());
        assert_eq!(
            (c.keys.c2s, c.keys.s2c, c.keys.hash),
            (s.keys.c2s, s.keys.s2c, s.keys.hash)
        );
        assert_eq!(c.session_id, s.session_id);
        assert_eq!((c.peer_key, s.peer_key), (e.s_pub, e.c_pub));
        assert_eq!(
            (c.peer_name.as_str(), s.peer_name.as_str()),
            ("console", "laptop")
        );
        assert_eq!((c.pairing, s.pairing), (None, None));
    }

    #[tokio::test]
    async fn an_unknown_client_pairs_with_matching_codes_while_the_window_is_open() {
        let e = run(false, false, true).await;
        let (cp, sp) = (e.c.unwrap().pairing.unwrap(), e.s.unwrap().pairing.unwrap());
        assert_eq!(cp.code, sp.code);
        assert!(cp.server_must_confirm);
    }

    #[tokio::test]
    async fn an_unknown_client_is_refused_when_pairing_is_closed() {
        let e = run(false, true, false).await;
        assert!(
            matches!(e.c, Err(Ava1Error::Refused { code, .. }) if code == gen::ERR_PAIRING_CLOSED),
            "{:?}",
            e.c.err()
        );
        assert!(matches!(e.s, Err(Ava1Error::NotPaired)));
    }

    #[tokio::test]
    async fn a_client_that_does_not_know_the_server_confirms_locally_only() {
        let e = run(true, false, false).await;
        assert!(!e.c.unwrap().pairing.unwrap().server_must_confirm);
        assert_eq!(e.s.unwrap().pairing, None);
    }

    #[tokio::test]
    async fn a_version_the_server_cannot_speak_is_refused() {
        let s_id = Identity::generate().unwrap();
        let c_id = Identity::generate().unwrap();
        let ((mut cr, mut cw), (mut sr, mut sw)) = pipe();
        let mut hs = Handshake::initiator(&c_id).unwrap();
        let hello = HelloInfo {
            version_min: 2,
            version_max: 2,
            caps: 0,
        }
        .to_bytes()
        .unwrap();
        cw.send_msg(
            0,
            &Hs1 {
                noise: hs.write(&hello).unwrap(),
            },
        )
        .await
        .unwrap();
        let first = sr.recv().await.unwrap();
        let s = server(&mut sr, &mut sw, first, &s_id, "console", |_| true, true).await;
        assert!(matches!(
            s,
            Err(Ava1Error::Version {
                min: 2,
                max: 2,
                ours: 1
            })
        ));
        let e: gen::Error = cr.recv().await.unwrap().decode().unwrap();
        assert_eq!(e.code, gen::ERR_UNSUPPORTED_VERSION);
    }

    #[tokio::test]
    async fn after_the_handshake_frames_are_sealed_and_replays_fail() {
        let (c_id, s_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let ((mut cr, mut cw), (mut sr, mut sw)) = pipe();
        let s_task = async {
            let first = sr.recv().await.unwrap();
            server(&mut sr, &mut sw, first, &s_id, "s", |_| true, false)
                .await
                .unwrap();
            sr
        };
        let c_task = async {
            client(&mut cr, &mut cw, &c_id, "c", |_| true)
                .await
                .unwrap()
        };
        let (est, mut sr) = tokio::join!(c_task, s_task);
        cw.send_msg(0, &gen::Ping { seq: 1, t_us: 1 })
            .await
            .unwrap();
        let f = sr.recv().await.unwrap();
        assert_eq!(f.ty, gen::Ping::TYPE);
        assert_eq!(
            f.flags & crate::frame::FLAG_SEALED,
            crate::frame::FLAG_SEALED
        );
        // Replay: resetting the key restarts the counter at 0, which the receiver has passed.
        cw.set_key(keys::lane_key(&est.keys.c2s, 0));
        cw.send_msg(0, &gen::Ping { seq: 2, t_us: 2 })
            .await
            .unwrap();
        assert!(matches!(sr.recv().await, Err(Ava1Error::BadTag)));
    }

    #[tokio::test]
    async fn a_frame_sealed_with_the_wrong_key_is_rejected() {
        let ((_cr, mut cw), (mut sr, _sw)) = pipe();
        cw.set_key([1; 32]);
        sr.set_key([2; 32]);
        cw.send_msg(0, &gen::Ping { seq: 1, t_us: 1 })
            .await
            .unwrap();
        assert!(matches!(sr.recv().await, Err(Ava1Error::BadTag)));
    }

    #[tokio::test]
    async fn an_unsealed_frame_after_keying_is_rejected() {
        let ((_cr, mut cw), (mut sr, _sw)) = pipe();
        sr.set_key([2; 32]);
        cw.send_msg(0, &gen::Ping { seq: 1, t_us: 1 })
            .await
            .unwrap();
        assert!(matches!(sr.recv().await, Err(Ava1Error::BadTag)));
    }

    #[tokio::test]
    async fn an_oversized_body_is_refused_before_it_is_read() {
        let ((_cr, mut cw), (mut sr, _sw)) = pipe();
        sr.set_max_body(1024);
        cw.send(gen::Ping::TYPE, 0, &vec![0u8; 2048]).await.unwrap();
        assert!(matches!(
            sr.recv().await,
            Err(Ava1Error::Header(crate::frame::HeaderError::TooLong(2048)))
        ));
    }

    #[tokio::test]
    async fn the_ignorable_flag_reaches_the_receiver() {
        let ((_cr, mut cw), (mut sr, _sw)) = pipe();
        cw.send_ignorable(0x5e, 0, b"future").await.unwrap();
        let f = sr.recv().await.unwrap();
        assert!(f.ignorable());
        assert_eq!(f.body, b"future");
    }
}
