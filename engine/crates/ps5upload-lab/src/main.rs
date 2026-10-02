//! ps5upload-lab — CLI tool for exercising the payload2 control channel.
//!
//! Usage:
//!   ps5upload-lab [ADDR] COMMAND [ARGS...]
//!
//! Default ADDR: 192.168.137.2:9113
//!
//! Commands:
//!   hello                           send HELLO, print HELLO_ACK
//!   status                          send STATUS, print STATUS_ACK body
//!   shutdown                        send SHUTDOWN
//!   takeover                        send TAKEOVER_REQUEST
//!   begin-tx TX_ID_HEX              send BEGIN_TX with the given 32-hex-char tx_id
//!   query-tx TX_ID_HEX              send QUERY_TX for the given tx_id
//!   abort-tx TX_ID_HEX              send ABORT_TX for the given tx_id
//!   send-shard TX_ID_HEX SEQ        send a dummy STREAM_SHARD and print SHARD_ACK
//!   transfer TX_ID_HEX DEST FILE    single-file transfer (begin→shards→commit→query)
//!   transfer-dir TX_ID_HEX DEST DIR multi-file transfer of a local directory
//!   volumes                         enumerate PS5 storage volumes (FS_LIST_VOLUMES)

use anyhow::{bail, Context, Result};
use ftx2_proto::{FrameType, ShardAck, ShardHeader, TxMeta};
use ps5upload_core::connection::Connection;
use ps5upload_core::diagnostics::shell_run;
use ps5upload_core::fs_ops::{app_launch, app_list_registered, app_register, app_unregister};
use ps5upload_core::hash_shard;
use ps5upload_core::hw::{hw_info, hw_temps, syslog_tail};
use ps5upload_core::saves::list_saves;
use ps5upload_core::transfer::{
    inspect_zip, transfer_dir, transfer_file, transfer_zip, TransferConfig,
};
use ps5upload_core::volumes::list_volumes;

const DEFAULT_ADDR: &str = "192.168.137.2:9113";

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn parse_tx_id(hex: &str) -> Result<[u8; 16]> {
    if hex.len() != 32 {
        bail!("tx_id must be exactly 32 hex chars, got {}", hex.len());
    }
    let mut out = [0u8; 16];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let hi = hex_val(chunk[0])?;
        let lo = hex_val(chunk[1])?;
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
}

fn hex_val(b: u8) -> Result<u8> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(10 + b - b'a'),
        b'A'..=b'F' => Ok(10 + b - b'A'),
        _ => bail!("invalid hex char: {}", b as char),
    }
}

fn tx_meta_body(tx_id: [u8; 16], kind: u32, extra: &[u8]) -> Vec<u8> {
    let meta = TxMeta {
        tx_id,
        kind,
        flags: 0,
    };
    let mut buf = meta.encode().to_vec();
    buf.extend_from_slice(extra);
    buf
}

/// Swap a transfer-port address (`ip:9113`) for the matching mgmt-port
/// address (`ip:9114`). The transfer-style do_transfer / do_transfer_dir
/// / do_transfer_zip flows verify success post-commit by calling
/// `do_query_tx`, but QueryTx is a mgmt-port frame and the lab tool
/// was previously passing the same transfer-port addr through —
/// resulting in a `wrong_port` Error frame and a non-zero exit code
/// on what was actually a successful upload (observed during the
/// v2.17.7 huge-folder verification run). Engine binary has its own
/// `mgmt_addr_for` helper; this is the lab-only inline copy since
/// ps5upload-core doesn't export the engine version.
fn to_mgmt_addr(transfer_addr: &str) -> String {
    match transfer_addr.rsplit_once(':') {
        Some((host, _)) => format!("{host}:9114"),
        None => format!("{transfer_addr}:9114"),
    }
}

fn expect_frame(conn: &mut Connection, expected: FrameType) -> Result<Vec<u8>> {
    let (hdr, body) = conn.recv_frame()?;
    let ft = hdr.frame_type().unwrap_or(FrameType::Error);
    println!("frame_type={ft:?}");
    if ft != expected {
        eprintln!("  body: {}", String::from_utf8_lossy(&body));
        bail!("expected {expected:?}, got {ft:?}");
    }
    Ok(body)
}

// ─── Commands ────────────────────────────────────────────────────────────────

fn do_volumes(addr: &str) -> Result<()> {
    let vols = list_volumes(addr)?;
    if vols.volumes.is_empty() {
        println!("(no volumes detected)");
        return Ok(());
    }
    // Render a small human-readable table. Fixed-width columns so the
    // common case (3-5 volumes, short mount names) is easy to eyeball.
    println!(
        "{:<8}  {:<8}  {:>14}  {:>14}  RW",
        "PATH", "FS", "TOTAL", "FREE"
    );
    for v in &vols.volumes {
        println!(
            "{:<8}  {:<8}  {:>14}  {:>14}  {}",
            v.path,
            v.fs_type,
            format_bytes(v.total_bytes),
            format_bytes(v.free_bytes),
            if v.writable { "rw" } else { "ro" }
        );
    }
    Ok(())
}

fn format_bytes(b: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{:.2} {}", v, UNITS[i])
}

fn do_hello(addr: &str) -> Result<()> {
    let mut c = Connection::connect(addr)?;
    c.send_frame(FrameType::Hello, b"{}")?;
    let body = expect_frame(&mut c, FrameType::HelloAck)?;
    println!("{}", String::from_utf8_lossy(&body));
    Ok(())
}

fn do_status(addr: &str) -> Result<()> {
    let mut c = Connection::connect(addr)?;
    c.send_frame(FrameType::Status, b"")?;
    let body = expect_frame(&mut c, FrameType::StatusAck)?;
    println!("{}", String::from_utf8_lossy(&body));
    Ok(())
}

fn do_hw_info(addr: &str) -> Result<()> {
    let info = hw_info(addr)?;
    println!("{info:?}");
    Ok(())
}

fn do_hw_temps(addr: &str, extended: bool) -> Result<()> {
    let t = hw_temps(addr, extended)?;
    println!("{t:?}");
    Ok(())
}

fn do_shutdown(addr: &str) -> Result<()> {
    let mut c = Connection::connect(addr)?;
    c.send_frame(FrameType::Shutdown, b"")?;
    let (hdr, body) = c.recv_frame()?;
    println!(
        "frame_type={:?}",
        hdr.frame_type().unwrap_or(FrameType::Error)
    );
    println!("{}", String::from_utf8_lossy(&body));
    Ok(())
}

fn do_takeover(addr: &str) -> Result<()> {
    let mut c = Connection::connect(addr)?;
    c.send_frame(FrameType::TakeoverRequest, b"")?;
    let (hdr, body) = c.recv_frame()?;
    println!(
        "frame_type={:?}",
        hdr.frame_type().unwrap_or(FrameType::Error)
    );
    println!("{}", String::from_utf8_lossy(&body));
    Ok(())
}

fn do_begin_tx(addr: &str, tx_id_hex: &str) -> Result<()> {
    let tx_id = parse_tx_id(tx_id_hex)?;
    let extra = format!(r#"{{"tx_id":"{}"}}"#, tx_id_hex);
    let body = tx_meta_body(tx_id, 1 /* upload_tree */, extra.as_bytes());
    let mut c = Connection::connect(addr)?;
    c.send_frame(FrameType::BeginTx, &body)?;
    let resp = expect_frame(&mut c, FrameType::BeginTxAck)?;
    println!("{}", String::from_utf8_lossy(&resp));
    Ok(())
}

fn do_query_tx(addr: &str, tx_id_hex: &str) -> Result<()> {
    let tx_id = parse_tx_id(tx_id_hex)?;
    let body = tx_meta_body(tx_id, 0, b"");
    let mut c = Connection::connect(addr)?;
    c.send_frame(FrameType::QueryTx, &body)?;
    let resp = expect_frame(&mut c, FrameType::QueryTxAck)?;
    println!("{}", String::from_utf8_lossy(&resp));
    Ok(())
}

fn do_commit_tx(addr: &str, tx_id_hex: &str) -> Result<()> {
    let tx_id = parse_tx_id(tx_id_hex)?;
    let body = tx_meta_body(tx_id, 0, b"");
    let mut c = Connection::connect(addr)?;
    c.send_frame(FrameType::CommitTx, &body)?;
    let resp = expect_frame(&mut c, FrameType::CommitTxAck)?;
    println!("{}", String::from_utf8_lossy(&resp));
    Ok(())
}

fn do_abort_tx(addr: &str, tx_id_hex: &str) -> Result<()> {
    let tx_id = parse_tx_id(tx_id_hex)?;
    let body = tx_meta_body(tx_id, 0, b"");
    let mut c = Connection::connect(addr)?;
    c.send_frame(FrameType::AbortTx, &body)?;
    let resp = expect_frame(&mut c, FrameType::AbortTxAck)?;
    println!("{}", String::from_utf8_lossy(&resp));
    Ok(())
}

fn do_transfer(addr: &str, tx_id_hex: &str, dest: &str, file_path: &str) -> Result<()> {
    let tx_id = parse_tx_id(tx_id_hex)?;
    let data = std::fs::read(file_path).with_context(|| format!("read {file_path}"))?;
    let cfg = TransferConfig::new(addr);
    println!(
        "transfer: file={file_path} bytes={} dest={dest}",
        data.len()
    );
    let r = transfer_file(&cfg, tx_id, dest, &data)?;
    println!(
        "done: shards={} bytes={} tx={}",
        r.shards_sent, r.bytes_sent, r.tx_id_hex
    );
    println!("commit_ack: {}", r.commit_ack_body);
    // QueryTx is a mgmt-port frame; the transfer flows above use the
    // :9113 transfer-port addr to drive the upload, so we swap to
    // the matching :9114 mgmt addr for the post-commit verification
    // step. Without the swap the payload returned `wrong_port` and
    // the lab tool exited non-zero on actually-successful uploads
    // (v2.17.7-era observed bug).
    do_query_tx(&to_mgmt_addr(addr), tx_id_hex)
}

fn do_transfer_dir(addr: &str, tx_id_hex: &str, dest_root: &str, src_dir: &str) -> Result<()> {
    let tx_id = parse_tx_id(tx_id_hex)?;
    let cfg = TransferConfig::new(addr);
    let r = transfer_dir(&cfg, tx_id, dest_root, std::path::Path::new(src_dir))?;
    println!(
        "done: shards={} bytes={} tx={}",
        r.shards_sent, r.bytes_sent, r.tx_id_hex
    );
    println!("commit_ack: {}", r.commit_ack_body);
    // QueryTx is a mgmt-port frame; the transfer flows above use the
    // :9113 transfer-port addr to drive the upload, so we swap to
    // the matching :9114 mgmt addr for the post-commit verification
    // step. Without the swap the payload returned `wrong_port` and
    // the lab tool exited non-zero on actually-successful uploads
    // (v2.17.7-era observed bug).
    do_query_tx(&to_mgmt_addr(addr), tx_id_hex)
}

fn do_saves(addr: &str) -> Result<()> {
    let list = list_saves(addr, 0)?;
    for s in &list.saves {
        println!(
            "{:<16} size={:<12} kind={:<3} path={}",
            s.title_id, s.size, s.kind, s.path
        );
    }
    println!("({} save(s))", list.saves.len());
    Ok(())
}

fn do_transfer_zip(addr: &str, tx_id_hex: &str, dest_root: &str, zip_path: &str) -> Result<()> {
    let tx_id = parse_tx_id(tx_id_hex)?;
    let cfg = TransferConfig::new(addr);
    let zp = std::path::Path::new(zip_path);
    let ins = inspect_zip(zp)?;
    println!(
        "zip: {} files, {} zipped -> {} extracted{}",
        ins.file_count,
        ins.compressed_size,
        ins.total_uncompressed,
        ins.title
            .as_deref()
            .map(|t| format!(" [{t} / {}]", ins.title_id.as_deref().unwrap_or("?")))
            .unwrap_or_default()
    );
    let r = transfer_zip(&cfg, tx_id, dest_root, zp)?;
    println!(
        "done: shards={} bytes={} tx={}",
        r.shards_sent, r.bytes_sent, r.tx_id_hex
    );
    println!("commit_ack: {}", r.commit_ack_body);
    // QueryTx is a mgmt-port frame; the transfer flows above use the
    // :9113 transfer-port addr to drive the upload, so we swap to
    // the matching :9114 mgmt addr for the post-commit verification
    // step. Without the swap the payload returned `wrong_port` and
    // the lab tool exited non-zero on actually-successful uploads
    // (v2.17.7-era observed bug).
    do_query_tx(&to_mgmt_addr(addr), tx_id_hex)
}

fn do_transfer_7z(addr: &str, tx_id_hex: &str, dest_root: &str, archive_path: &str) -> Result<()> {
    let tx_id = parse_tx_id(tx_id_hex)?;
    let cfg = TransferConfig::new(addr);
    let ap = std::path::Path::new(archive_path);
    let ins = ps5upload_core::transfer::inspect_7z(ap)?;
    println!(
        "7z: {} files, {} compressed -> {} extracted",
        ins.file_count, ins.compressed_size, ins.total_uncompressed
    );
    let r = ps5upload_core::transfer::transfer_7z_with_opts(&cfg, tx_id, dest_root, ap, 0)?;
    println!(
        "done: shards={} bytes={} tx={}",
        r.shards_sent, r.bytes_sent, r.tx_id_hex
    );
    println!("commit_ack: {}", r.commit_ack_body);
    do_query_tx(&to_mgmt_addr(addr), tx_id_hex)
}

fn do_transfer_rar(
    addr: &str,
    tx_id_hex: &str,
    dest_root: &str,
    archive_path: &str,
    password: Option<&str>,
) -> Result<()> {
    let tx_id = parse_tx_id(tx_id_hex)?;
    let cfg = TransferConfig::new(addr);
    let ap = std::path::Path::new(archive_path);
    let ins = ps5upload_core::transfer::inspect_rar(ap, password)?;
    println!(
        "rar: {} files, {} extracted",
        ins.file_count, ins.total_uncompressed
    );
    let r = ps5upload_core::transfer::transfer_rar_resumable(
        &cfg, tx_id, dest_root, ap, password, 3, 0,
    )?;
    println!(
        "done: shards={} bytes={} tx={}",
        r.shards_sent, r.bytes_sent, r.tx_id_hex
    );
    println!("commit_ack: {}", r.commit_ack_body);
    do_query_tx(&to_mgmt_addr(addr), tx_id_hex)
}

fn do_profile_info(addr: &str) -> Result<()> {
    let info = ps5upload_core::profile::profile_info(&to_mgmt_addr(addr))?;
    println!(
        "foreground user: uid={} ({}) name={:?}",
        info.uid, info.uid_hex, info.username
    );
    println!("local users ({}):", info.users.len());
    for u in &info.users {
        println!("  {} uid={} name={:?}", u.uid_hex, u.uid, u.username);
    }
    if info.slots.is_empty() {
        println!("(no offline-account name slots populated)");
    }
    for s in &info.slots {
        println!(
            "  slot {:>2}: name={:?} type={:?} flags={} id={} activated={}",
            s.slot, s.name, s.type_, s.flags, s.id, s.activated
        );
    }
    Ok(())
}

fn do_profile_set_username(addr: &str, slot: i32, name: &str) -> Result<()> {
    ps5upload_core::profile::profile_set_username(&to_mgmt_addr(addr), slot, name)?;
    println!("renamed slot {slot} -> {name:?}");
    Ok(())
}

fn do_profile_rename_user(addr: &str, uid: u32, name: &str) -> Result<()> {
    ps5upload_core::profile::profile_set_local_username(&to_mgmt_addr(addr), uid, name)?;
    println!("renamed user 0x{uid:08X} -> {name:?}");
    Ok(())
}

fn do_profile_activate(addr: &str, slot: i32, id: Option<u64>) -> Result<()> {
    let id = ps5upload_core::profile::profile_activate(&to_mgmt_addr(addr), slot, id)?;
    println!("activated slot {slot}, id={id}");
    Ok(())
}

fn do_profile_clear_slot(addr: &str, slot: i32) -> Result<()> {
    ps5upload_core::profile::profile_clear_slot(&to_mgmt_addr(addr), slot)?;
    println!("cleared slot {slot}");
    Ok(())
}

fn do_profile_apply_avatar(
    addr: &str,
    image_path: &str,
    mode: &str,
    uid: Option<u32>,
) -> Result<()> {
    let bytes = std::fs::read(image_path)?;
    let mode = ps5upload_core::profile::SquareMode::parse(mode);
    let applied = ps5upload_core::profile::profile_apply_avatar(
        &to_mgmt_addr(addr),
        uid.unwrap_or(0),
        None,
        &bytes,
        mode,
    )?;
    println!(
        "avatar applied: uid={} username={:?} files_copied={}",
        applied.uid, applied.username, applied.files_copied
    );
    Ok(())
}

fn do_send_shard(addr: &str, tx_id_hex: &str, shard_seq: u64) -> Result<()> {
    let tx_id = parse_tx_id(tx_id_hex)?;

    // Dummy shard: 256 bytes of 0xAB payload.
    let shard_data = vec![0xABu8; 256];
    let shard_hdr = ShardHeader {
        tx_id,
        shard_seq,
        shard_digest: hash_shard(&shard_data),
        record_count: 1,
        flags: 0,
    };
    let hdr_bytes = shard_hdr.encode();

    let mut c = Connection::connect(addr)?;
    c.send_frame_split(FrameType::StreamShard, &hdr_bytes, &shard_data)?;

    // Receive SHARD_ACK (binary body)
    let (resp_hdr, resp_body) = c.recv_frame()?;
    let ft = resp_hdr.frame_type().unwrap_or(FrameType::Error);
    println!("frame_type={ft:?}");

    if ft == FrameType::ShardAck {
        match ShardAck::decode(&resp_body) {
            Ok(ack) => {
                println!(
                    "shard_seq={} ack_state={:?} bytes_committed={} files_committed={}",
                    ack.shard_seq,
                    ack.ack_state,
                    ack.bytes_committed_total,
                    ack.files_committed_total,
                );
            }
            Err(e) => eprintln!("failed to decode SHARD_ACK: {e}"),
        }
    } else {
        eprintln!("body: {}", String::from_utf8_lossy(&resp_body));
        bail!("expected SHARD_ACK, got {ft:?}");
    }
    Ok(())
}

// ─── Entry point ─────────────────────────────────────────────────────────────

mod ava1_cmds {
    use std::io::{BufRead, Write};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use anyhow::{anyhow, bail, Context, Result};
    use ava1::keys::Identity;
    use ava1::peers::PeerStore;
    use ava1::session::{connect, Session, Timing};

    /// Same files the engine uses (`<data dir>/ava/`), so a lab-stamped payload trusts the engine.
    fn ava_dir() -> PathBuf {
        if let Ok(p) = std::env::var("AVA1_DIR") {
            return PathBuf::from(p);
        }
        let base = std::env::var("PS5UPLOAD_DATA_DIR")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                // Same order as the engine's data_dir(): HOME, then USERPROFILE.
                let home = std::env::var("HOME")
                    .or_else(|_| std::env::var("USERPROFILE"))
                    .unwrap_or_else(|_| ".".into());
                PathBuf::from(home).join(".ps5upload")
            });
        base.join("ava")
    }

    fn identity() -> Result<Arc<Identity>> {
        let p = ava_dir().join("identity");
        Ok(Arc::new(
            Identity::load_or_create(&p).with_context(|| format!("identity {}", p.display()))?,
        ))
    }

    /// `192.168.1.5` or `192.168.1.5:9113` → `192.168.1.5:9120` (`AVA1_PORT` overrides the
    /// port, e.g. to go through a local chaos proxy).
    pub fn ava1_addr(addr: &str) -> String {
        let host = addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(addr);
        let port = std::env::var("AVA1_PORT")
            .ok()
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(ava1::gen::DEFAULT_PORT);
        format!("{host}:{port}")
    }

    async fn session(addr: &str) -> Result<Session> {
        let peers = Arc::new(Mutex::new(PeerStore::load(&ava_dir().join("peers"))?));
        let mut s = connect(
            &ava1_addr(addr),
            identity()?,
            peers,
            "ps5upload-lab",
            Timing::default(),
        )
        .await?;
        if let Some(code) = s.pairing_code() {
            print!(
                "Pairing with {} — code {code:06}. Does the console show the same code? [y/N] ",
                s.peer_name()
            );
            std::io::stdout().flush()?;
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line)?;
            if !line.trim().eq_ignore_ascii_case("y") {
                bail!("pairing not confirmed");
            }
            s.confirm_pairing().await?;
            println!("paired");
        }
        Ok(s)
    }

    pub async fn ping(addr: &str, seconds: u64) -> Result<()> {
        let s = session(addr).await?;
        let info = s.node_info().await?;
        println!(
            "node.info: {} {} {} fw={}",
            info.name,
            info.platform,
            info.version,
            info.firmware.unwrap_or_default()
        );
        for i in 0..seconds {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if s.is_closed() {
                bail!("session ended after {i} s: {}", s.closed().await);
            }
            println!("{:>4} s  rtt {:?}", i + 1, s.rtt());
        }
        s.close().await;
        println!("ok");
        Ok(())
    }

    pub async fn lanes(addr: &str, n: usize, seconds: u64) -> Result<()> {
        let s = session(addr).await?;
        let mut lanes = Vec::new();
        for _ in 0..n {
            lanes.push(s.open_lane().await?);
        }
        println!("{} lanes open", lanes.len());
        for i in 0..seconds {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let dead = lanes.iter().filter(|l| l.is_closed()).count();
            if dead > 0 || s.is_closed() {
                bail!(
                    "after {i} s: {dead} lanes closed, session closed: {}",
                    s.is_closed()
                );
            }
        }
        println!("ok");
        Ok(())
    }

    /// Asks the console to accept new pairings for `seconds` (this machine must be paired).
    pub async fn pairing_open(addr: &str, seconds: u16) -> Result<()> {
        let s = session(addr).await?;
        s.open_pairing(seconds).await?;
        println!("pairing open on {} for {seconds} s", s.peer_name());
        s.close().await;
        Ok(())
    }

    /// Frame AEAD (ChaCha20-Poly1305) cost on the console (1 MiB frames sealed and opened
    /// in its memory with the code its transfers use) and on this computer.
    pub async fn cryptobench(addr: &str, mib: u16) -> Result<()> {
        use ava1::wire::Message;
        let s = session(addr).await?;
        let body = ava1::gen::CryptoBench { mib }.to_bytes()?;
        let r = s.rpc(ava1::gen::METHOD_CRYPTO_BENCH, &body).await?;
        if r.status != ava1::gen::STATUS_OK {
            bail!("crypto.bench failed with status {}", r.status);
        }
        let res = ava1::gen::CryptoBenchResult::decode(&r.body)?;
        let rate = |micros: u64| res.bytes as f64 / micros.max(1) as f64; // bytes per µs = MB/s
        let seal = rate(res.micros);
        println!(
            "console: seal {seal:.0} MB/s, open {} on one core ({} MiB, ChaCha20 path: {})",
            res.open_micros
                .map_or("n/a (older helper)".into(), |m| format!(
                    "{:.0} MB/s",
                    rate(m)
                )),
            res.bytes >> 20,
            res.backend
                .as_deref()
                .unwrap_or("unreported (older helper)")
        );
        let (here_seal, here_open) = local_aead_rate(mib.max(1));
        println!("this computer: seal {here_seal:.0} MB/s, open {here_open:.0} MB/s on one core");
        let worst = res.open_micros.map_or(seal, |m| seal.min(rate(m)));
        println!(
            "110 MB/s of transfer costs {:.1}% of one console core (target: at most 15%)",
            110.0 / worst * 100.0
        );
        s.close().await;
        Ok(())
    }

    /// MB/s of `ava1::keys::seal` and `open` here, on `mib` 1 MiB frames.
    fn local_aead_rate(mib: u16) -> (f64, f64) {
        const MIB: usize = 1 << 20;
        let key = [0x11u8; 32];
        let mut buf = vec![0x5au8; MIB];
        let t = std::time::Instant::now();
        for i in 0..u64::from(mib) {
            ava1::keys::seal(&key, i, &[], &mut buf);
            buf.truncate(MIB);
        }
        let seal = t.elapsed();
        let mut sealed = vec![0x5au8; MIB];
        ava1::keys::seal(&key, 0, &[], &mut sealed);
        // Opening needs the same frame every round: copy it in, and take the copies' time out.
        let t = std::time::Instant::now();
        for _ in 0..mib {
            buf.clear();
            buf.extend_from_slice(std::hint::black_box(&sealed));
        }
        let copy = t.elapsed();
        let t = std::time::Instant::now();
        for _ in 0..mib {
            buf.clear();
            buf.extend_from_slice(&sealed);
            assert!(
                ava1::keys::open(&key, 0, &[], &mut buf),
                "local open failed"
            );
        }
        let open = t.elapsed().saturating_sub(copy);
        let rate = |d: std::time::Duration| {
            f64::from(u32::from(mib)) * MIB as f64 / d.as_micros().max(1) as f64
        };
        (rate(seal), rate(open))
    }

    pub fn stamp(input: &str, output: &str) -> Result<()> {
        let mut b = std::fs::read(input).with_context(|| format!("read {input}"))?;
        let key = identity()?.public();
        ava1::trust::stamp(&mut b, &key).map_err(|e| anyhow!("{input}: {e}"))?;
        std::fs::write(output, &b).with_context(|| format!("write {output}"))?;
        println!("stamped {output} with {}", ava1::hex::encode(&key));
        Ok(())
    }

    pub async fn chaos(listen_port: u16, upstream: &str, args: &[String]) -> Result<()> {
        let mut cfg = ava1_chaos::ChaosConfig::default();
        let mut it = args.iter();
        while let Some(a) = it.next() {
            let v: u64 = it
                .next()
                .ok_or_else(|| anyhow!("{a} needs a value"))?
                .parse()?;
            match a.as_str() {
                "--delay-ms" => cfg.delay = Duration::from_millis(v),
                "--kbps" => cfg.bytes_per_sec = Some(v * 1024),
                "--kill-every-s" => cfg.kill_every = Some(Duration::from_secs(v)),
                other => bail!("unknown option {other}"),
            }
        }
        let up = tokio::net::lookup_host(upstream)
            .await?
            .next()
            .ok_or_else(|| anyhow!("resolve {upstream}"))?;
        let p =
            ava1_chaos::ChaosProxy::start_on(&format!("0.0.0.0:{listen_port}"), up, cfg).await?;
        println!(
            "chaos proxy {} -> {up}. Enter: b = toggle blackhole, k = kill all, q = quit",
            p.addr
        );
        let mut on = false;
        for line in std::io::stdin().lock().lines() {
            match line?.trim() {
                "b" => {
                    on = !on;
                    p.blackhole(on);
                    println!("blackhole {on}");
                }
                "k" => {
                    p.kill_all();
                    println!("killed");
                }
                "q" => break,
                _ => {}
            }
        }
        Ok(())
    }
}

fn usage() -> ! {
    eprintln!(
        "  ava1-ping [SECONDS]            AVA1 handshake (pairs if needed), node.info, heartbeats"
    );
    eprintln!("  ava1-lanes N [SECONDS]         open N data lanes and keep them up");
    eprintln!("  ava1-cryptobench [MIB]         frame AEAD cost on the console vs this computer");
    eprintln!("  ava1-pairing-open [SECONDS]    let another device pair with the console");
    eprintln!("  ava1-stamp IN.elf OUT.elf      stamp this machine's AVA1 key into a payload");
    eprintln!("  chaos-proxy PORT HOST:PORT [--delay-ms N] [--kbps N] [--kill-every-s N]");
    eprintln!("Usage: ps5upload-lab [ADDR] COMMAND [ARGS...]");
    eprintln!("  Default ADDR: {DEFAULT_ADDR}");
    eprintln!("Commands:");
    eprintln!("  hello");
    eprintln!("  status");
    eprintln!("  shutdown");
    eprintln!("  takeover");
    eprintln!("  begin-tx TX_ID_HEX");
    eprintln!("  query-tx TX_ID_HEX");
    eprintln!("  commit-tx TX_ID_HEX");
    eprintln!("  abort-tx  TX_ID_HEX");
    eprintln!("  send-shard   TX_ID_HEX SHARD_SEQ");
    eprintln!("  transfer     TX_ID_HEX DEST_FILE FILE_PATH");
    eprintln!("  transfer-dir TX_ID_HEX DEST_ROOT SRC_DIR");
    eprintln!("  transfer-zip TX_ID_HEX DEST_ROOT ZIP_PATH  decompress+stream a .zip");
    eprintln!("  transfer-7z  TX_ID_HEX DEST_ROOT 7Z_PATH   decompress+stream a .7z");
    eprintln!("  transfer-rar TX_ID_HEX DEST_ROOT RAR_PATH [PASSWORD]  host-extract+stream a .rar");
    eprintln!("  register     SRC_PATH      register a game folder");
    eprintln!("  unregister   TITLE_ID      reverse registration");
    eprintln!("  launch       TITLE_ID      sceLncUtilLaunchApp");
    eprintln!("  power        tick|standby|reboot|shutdown  (mgmt port; tick = keep-awake)");
    eprintln!("  apps                       list titles present in app.db");
    eprintln!(
        "  processes                  detailed process list (pid/comm/title/mem/threads/kind)"
    );
    eprintln!("  process-kill <pid>         SIGKILL a process by pid");
    eprintln!("  saves                      list save-data folders + sizes (:9114)");
    eprintln!("  profile-info                       foreground user + account name slots (:9114)");
    eprintln!("  profile-set-username SLOT NAME     rename an account-name slot");
    eprintln!("  profile-rename-user  UID_HEX NAME  rename a local console user");
    eprintln!("  profile-activate     SLOT [ID_HEX] activate a slot (derive id if omitted)");
    eprintln!("  profile-clear-slot   SLOT          de-activate a slot (zero id+flags)");
    eprintln!("  profile-apply-avatar IMAGE [crop|fit]  set the foreground user's avatar");
    eprintln!("  shell       SESSION CWD CMD...   run shell command via :9114");
    std::process::exit(1);
}

fn do_register(addr: &str, src_path: &str) -> Result<()> {
    /* lab CLI defaults to NOT patching DRM-type — for ad-hoc
     * registration of well-formed dumps. The desktop client UI
     * exposes the toggle. */
    let res = app_register(addr, src_path, false)?;
    println!(
        "registered: title_id={} title_name={} used_nullfs={}",
        res.title_id, res.title_name, res.used_nullfs
    );
    Ok(())
}

fn do_unregister(addr: &str, title_id: &str) -> Result<()> {
    let outcome = app_unregister(addr, title_id)?;
    if outcome.sony_refused() {
        println!(
            "unregistered: {title_id} (our teardown ok, but the console's own \
             uninstaller REFUSED with rc=0x{:08X} — the title may remain in \
             Settings > Storage)",
            outcome.sony_uninstall_rc
        );
    } else {
        println!("unregistered: {title_id}");
    }
    Ok(())
}

fn do_launch(addr: &str, title_id: &str) -> Result<()> {
    app_launch(addr, title_id)?;
    println!("launched: {title_id}");
    Ok(())
}

fn do_apps(addr: &str) -> Result<()> {
    let apps = app_list_registered(addr)?;
    if apps.apps.is_empty() {
        println!("(no registered titles)");
        return Ok(());
    }
    println!("{:<12} {:<40} {:<30} IMG", "TITLE_ID", "TITLE_NAME", "SRC");
    for a in &apps.apps {
        println!(
            "{:<12} {:<40.40} {:<30.30} {}",
            a.title_id,
            a.title_name,
            if a.src.is_empty() { "-" } else { &a.src },
            if a.image_backed { "yes" } else { "no" }
        );
    }
    Ok(())
}

fn do_processes(addr: &str) -> Result<()> {
    let res = ps5upload_core::process_mgr::process_list(addr)?;
    if res.processes.is_empty() {
        println!("(no processes)");
        return Ok(());
    }
    println!(
        "{:>6} {:<6} {:<20} {:<14} {:>8} {:>4}  NAME",
        "PID", "KIND", "COMM", "TITLE_ID", "MEM_MB", "THR"
    );
    for p in &res.processes {
        println!(
            "{:>6} {:<6} {:<20.20} {:<14} {:>8.1} {:>4}  {}{}",
            p.pid,
            p.kind,
            p.comm,
            if p.title_id.is_empty() {
                "-"
            } else {
                &p.title_id
            },
            p.memory_mib,
            p.threads,
            p.name,
            if p.is_self { "  <-- SELF (helper)" } else { "" },
        );
    }
    println!(
        "\n{} process(es){}",
        res.processes.len(),
        if res.truncated {
            " (list truncated)"
        } else {
            ""
        }
    );
    Ok(())
}

fn do_process_kill(addr: &str, pid: i32) -> Result<()> {
    let ack = ps5upload_core::process_mgr::process_kill(addr, pid)?;
    println!("kill ack: ok={} pid={}", ack.ok, ack.pid);
    Ok(())
}

fn do_shell(addr: &str, session: &str, cwd: &str, cmd: &str) -> Result<()> {
    let res = shell_run(addr, cmd, Some(session), Some(cwd), 30)?;
    println!("exit_code={:?}", res.exit_code);
    println!("timed_out={}", res.timed_out);
    println!("cwd={}", res.cwd.as_deref().unwrap_or(""));
    println!("session_id={}", res.session_id.as_deref().unwrap_or(""));
    print!("{}", res.stdout);
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        usage();
    }

    // If first arg looks like host:port or a bare IP, use it as address.
    let (addr, rest) =
        if args[0].contains(':') || args[0].chars().next().is_some_and(|c| c.is_ascii_digit()) {
            (args[0].as_str(), &args[1..])
        } else {
            (DEFAULT_ADDR, &args[..])
        };

    if rest.is_empty() {
        usage();
    }

    match rest[0].as_str() {
        "ava1-ping" | "ava1-lanes" | "ava1-cryptobench" | "ava1-pairing-open" | "chaos-proxy" => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async {
                match rest[0].as_str() {
                    "ava1-cryptobench" => {
                        ava1_cmds::cryptobench(
                            addr,
                            rest.get(1).and_then(|s| s.parse().ok()).unwrap_or(256),
                        )
                        .await
                    }
                    "ava1-pairing-open" => {
                        ava1_cmds::pairing_open(
                            addr,
                            rest.get(1).and_then(|s| s.parse().ok()).unwrap_or(120),
                        )
                        .await
                    }
                    "ava1-ping" => {
                        ava1_cmds::ping(
                            addr,
                            rest.get(1).and_then(|s| s.parse().ok()).unwrap_or(10),
                        )
                        .await
                    }
                    "ava1-lanes" => {
                        let n = rest.get(1).and_then(|s| s.parse().ok()).unwrap_or(8);
                        ava1_cmds::lanes(
                            addr,
                            n,
                            rest.get(2).and_then(|s| s.parse().ok()).unwrap_or(30),
                        )
                        .await
                    }
                    _ => {
                        let port: u16 = rest
                            .get(1)
                            .and_then(|s| s.parse().ok())
                            .unwrap_or_else(|| usage());
                        let up = rest.get(2).map(|s| s.as_str()).unwrap_or_else(|| usage());
                        ava1_cmds::chaos(port, up, &rest[3..]).await
                    }
                }
            })
        }
        "ava1-stamp" => {
            let i = rest.get(1).map(|s| s.as_str()).unwrap_or_else(|| usage());
            let o = rest.get(2).map(|s| s.as_str()).unwrap_or_else(|| usage());
            ava1_cmds::stamp(i, o)
        }
        "hello" => do_hello(addr),
        "status" => do_status(addr),
        "volumes" => do_volumes(addr),
        "shutdown" => do_shutdown(addr),
        "takeover" => do_takeover(addr),
        "begin-tx" => {
            let tx_id = rest.get(1).map(|s| s.as_str()).unwrap_or_else(|| usage());
            do_begin_tx(addr, tx_id)
        }
        "query-tx" => {
            let tx_id = rest.get(1).map(|s| s.as_str()).unwrap_or_else(|| usage());
            do_query_tx(addr, tx_id)
        }
        "commit-tx" => {
            let tx_id = rest.get(1).map(|s| s.as_str()).unwrap_or_else(|| usage());
            do_commit_tx(addr, tx_id)
        }
        "abort-tx" => {
            let tx_id = rest.get(1).map(|s| s.as_str()).unwrap_or_else(|| usage());
            do_abort_tx(addr, tx_id)
        }
        "send-shard" => {
            let tx_id = rest.get(1).map(|s| s.as_str()).unwrap_or_else(|| usage());
            let seq: u64 = rest.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);
            do_send_shard(addr, tx_id, seq)
        }
        "transfer" => {
            let tx_id = rest.get(1).map(|s| s.as_str()).unwrap_or_else(|| usage());
            let dest_root = rest.get(2).map(|s| s.as_str()).unwrap_or_else(|| usage());
            let file_path = rest.get(3).map(|s| s.as_str()).unwrap_or_else(|| usage());
            do_transfer(addr, tx_id, dest_root, file_path)
        }
        "transfer-dir" => {
            let tx_id = rest.get(1).map(|s| s.as_str()).unwrap_or_else(|| usage());
            let dest_root = rest.get(2).map(|s| s.as_str()).unwrap_or_else(|| usage());
            let src_dir = rest.get(3).map(|s| s.as_str()).unwrap_or_else(|| usage());
            do_transfer_dir(addr, tx_id, dest_root, src_dir)
        }
        "transfer-zip" => {
            let tx_id = rest.get(1).map(|s| s.as_str()).unwrap_or_else(|| usage());
            let dest_root = rest.get(2).map(|s| s.as_str()).unwrap_or_else(|| usage());
            let zip_path = rest.get(3).map(|s| s.as_str()).unwrap_or_else(|| usage());
            do_transfer_zip(addr, tx_id, dest_root, zip_path)
        }
        "transfer-7z" => {
            let tx_id = rest.get(1).map(|s| s.as_str()).unwrap_or_else(|| usage());
            let dest_root = rest.get(2).map(|s| s.as_str()).unwrap_or_else(|| usage());
            let arc = rest.get(3).map(|s| s.as_str()).unwrap_or_else(|| usage());
            do_transfer_7z(addr, tx_id, dest_root, arc)
        }
        "transfer-rar" => {
            let tx_id = rest.get(1).map(|s| s.as_str()).unwrap_or_else(|| usage());
            let dest_root = rest.get(2).map(|s| s.as_str()).unwrap_or_else(|| usage());
            let arc = rest.get(3).map(|s| s.as_str()).unwrap_or_else(|| usage());
            let password = rest.get(4).map(|s| s.as_str());
            do_transfer_rar(addr, tx_id, dest_root, arc, password)
        }
        "register" => {
            let src_path = rest.get(1).map(|s| s.as_str()).unwrap_or_else(|| usage());
            do_register(addr, src_path)
        }
        "unregister" => {
            let title_id = rest.get(1).map(|s| s.as_str()).unwrap_or_else(|| usage());
            do_unregister(addr, title_id)
        }
        "launch" => {
            let title_id = rest.get(1).map(|s| s.as_str()).unwrap_or_else(|| usage());
            do_launch(addr, title_id)
        }
        "apps" => do_apps(addr),
        // processes: detailed process manager enumerate (pid/comm/title/mem/
        // threads/kind). process-kill <pid>: SIGKILL one pid (guarded payload-
        // side). Hardware-verifies the in-app process manager.
        "processes" => do_processes(addr),
        "process-kill" => {
            let pid: i32 = rest
                .get(1)
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or_else(|| usage());
            do_process_kill(addr, pid)
        }
        // hw-info: mgmt-port hardware read. hw-temps / hw-temps-x: live
        // CPU/SoC sensor read — hw-temps-x (extended) drives the ShellUI
        // ptrace path (sys_ptrace authid swap under kernel_rw_lock), the exact
        // kernel-R/W path that must serialize against installs. Used to stress
        // the kernel_rw_lock concurrently from many connections.
        "hw-info" => do_hw_info(addr),
        "syslog" => {
            print!("{}", syslog_tail(addr)?);
            Ok(())
        }
        // power <tick|standby|reboot|shutdown>: drives the SystemControl
        // mgmt frame. `tick` (sceSystemServicePowerTick) is the keep-awake
        // primitive — non-destructive, resets the console's auto-standby
        // idle timer. Added for real-hardware verification of the client's
        // keep-PS5-awake feature.
        "power" => {
            let action = match rest.get(1).map(|s| s.as_str()) {
                Some("tick") => ps5upload_core::system_control::PowerAction::Tick,
                Some("standby") => ps5upload_core::system_control::PowerAction::Standby,
                Some("reboot") => ps5upload_core::system_control::PowerAction::Reboot,
                Some("shutdown") => ps5upload_core::system_control::PowerAction::Shutdown,
                _ => usage(),
            };
            let ack = ps5upload_core::system_control::system_control(addr, action)?;
            println!(
                "power ack: ok={} action={} err={} code={}",
                ack.ok,
                ack.action.as_deref().unwrap_or("-"),
                ack.err.as_deref().unwrap_or("-"),
                ack.code.map_or("-".to_string(), |c| c.to_string()),
            );
            if !ack.ok {
                bail!("power action failed");
            }
            Ok(())
        }
        "hw-temps" => do_hw_temps(addr, false),
        "hw-temps-x" => do_hw_temps(addr, true),
        "saves" => do_saves(addr),
        "profile-info" => do_profile_info(addr),
        "profile-set-username" => {
            let slot: i32 = rest
                .get(1)
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| usage());
            let name = rest.get(2).map(|s| s.as_str()).unwrap_or_else(|| usage());
            do_profile_set_username(addr, slot, name)
        }
        "profile-rename-user" => {
            let uid = rest.get(1).and_then(|s| {
                let s = s
                    .strip_prefix("0x")
                    .or_else(|| s.strip_prefix("0X"))
                    .unwrap_or(s);
                u32::from_str_radix(s, 16).ok()
            });
            let name = rest.get(2).map(|s| s.as_str()).unwrap_or_else(|| usage());
            match uid {
                Some(u) => do_profile_rename_user(addr, u, name),
                None => usage(),
            }
        }
        "profile-activate" => {
            let slot: i32 = rest
                .get(1)
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| usage());
            let id = rest.get(2).and_then(|s| {
                let s = s
                    .strip_prefix("0x")
                    .or_else(|| s.strip_prefix("0X"))
                    .unwrap_or(s);
                u64::from_str_radix(s, 16).ok()
            });
            do_profile_activate(addr, slot, id)
        }
        "profile-clear-slot" => {
            let slot: i32 = rest
                .get(1)
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| usage());
            do_profile_clear_slot(addr, slot)
        }
        "profile-apply-avatar" => {
            let image = rest.get(1).map(|s| s.as_str()).unwrap_or_else(|| usage());
            let mode = rest.get(2).map(|s| s.as_str()).unwrap_or("crop");
            let uid = rest.get(3).and_then(|s| {
                let s = s
                    .strip_prefix("0x")
                    .or_else(|| s.strip_prefix("0X"))
                    .unwrap_or(s);
                u32::from_str_radix(s, 16).ok()
            });
            do_profile_apply_avatar(addr, image, mode, uid)
        }
        "shell" => {
            let session = rest.get(1).map(|s| s.as_str()).unwrap_or_else(|| usage());
            let cwd = rest.get(2).map(|s| s.as_str()).unwrap_or_else(|| usage());
            let cmd = rest
                .get(3..)
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| usage())
                .join(" ");
            do_shell(addr, session, cwd, &cmd)
        }
        cmd => bail!("unknown command: {cmd}"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn ava1_commands_use_port_9120() {
        assert_eq!(
            super::ava1_cmds::ava1_addr("192.168.86.100:9113"),
            "192.168.86.100:9120"
        );
        assert_eq!(
            super::ava1_cmds::ava1_addr("192.168.86.100"),
            "192.168.86.100:9120"
        );
    }
}
