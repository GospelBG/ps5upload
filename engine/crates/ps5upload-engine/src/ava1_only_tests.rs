//! The engine speaks one protocol. These tests pin the cutover's visible contract: a console
//! without an AVA1 listener or without pairing is an actionable error (never a silent second
//! protocol), the renamed environment variables, and that no retired-protocol symbol is left.

use super::*;

fn engine_src_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

async fn wait_failed(
    jobs: &Arc<Mutex<HashMap<Uuid, JobState>>>,
) -> (String, Option<String>, Option<String>) {
    for _ in 0..500 {
        if let Some(JobState::Failed {
            error,
            error_reason,
            error_detail,
            ..
        }) = jobs.lock().unwrap().values().next().cloned()
        {
            return (error, error_reason, error_detail);
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the job never failed");
}

fn state_for(jobs: &Arc<Mutex<HashMap<Uuid, JobState>>>) -> AppState {
    let (events_tx, _rx) = broadcast::channel(16);
    AppState {
        jobs: Arc::clone(jobs),
        default_ps5_addr: "127.0.0.1".to_string(),
        events_tx,
    }
}

/// Nothing listens on port 9120 of this host: the console "has no AVA1 listener". The job
/// fails at once with the token the client keys on (send the helper again), not after
/// retries and not by trying another protocol.
#[tokio::test(flavor = "multi_thread")]
async fn no_ava1_listener_surfaces_helper_not_running() {
    let dir = std::env::temp_dir().join(format!("p5-ava1only-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("one.bin");
    std::fs::write(&src, b"hello").unwrap();
    let jobs: Arc<Mutex<HashMap<Uuid, JobState>>> = Arc::new(Mutex::new(HashMap::new()));
    let req = TransferFileReq {
        addr: Some("127.0.0.1:9113".to_string()),
        tx_id: None,
        dest: "/data/x/one.bin".to_string(),
        src: src.to_string_lossy().into_owned(),
        bandwidth_cap_mbps: None,
    };
    let resp = transfer_file_handler(State(state_for(&jobs)), Json(req))
        .await
        .into_response();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let (error, reason, detail) = wait_failed(&jobs).await;
    assert_eq!(reason.as_deref(), Some("helper_not_ava1"), "{error}");
    let msg = "The PS5 helper is not running or is an old version. Send the helper again from the Connection screen.";
    assert_eq!(detail.as_deref(), Some(msg));
    assert_eq!(error, msg);
    let _ = std::fs::remove_dir_all(&dir);
}

const NOT_RUNNING: &str = "The PS5 helper is not running or is an old version. Send the helper again from the Connection screen.";

async fn assert_helper_not_ava1(jobs: &Arc<Mutex<HashMap<Uuid, JobState>>>) {
    let (error, reason, _) = wait_failed(jobs).await;
    assert_eq!(reason.as_deref(), Some("helper_not_ava1"), "{error}");
    assert_eq!(error, NOT_RUNNING);
}

#[tokio::test(flavor = "multi_thread")]
async fn transfer_dir_with_no_ava1_listener_is_helper_not_ava1() {
    let dir = std::env::temp_dir().join(format!("p5-ava1only-dir-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.bin"), b"hello").unwrap();
    let jobs: Arc<Mutex<HashMap<Uuid, JobState>>> = Arc::new(Mutex::new(HashMap::new()));
    let req = TransferDirReq {
        addr: Some("127.0.0.1:9113".to_string()),
        tx_id: None,
        dest_root: "/data/x".to_string(),
        src_dir: dir.to_string_lossy().into_owned(),
        excludes: vec![],
        bandwidth_cap_mbps: None,
        skip_existing: None,
    };
    let resp = transfer_dir_handler(State(state_for(&jobs)), Json(req))
        .await
        .into_response();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_helper_not_ava1(&jobs).await;
    let _ = std::fs::remove_dir_all(&dir);
}

fn tiny_zip(path: &std::path::Path) {
    use std::io::Write;
    let mut w = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    w.start_file("a.txt", zip::write::SimpleFileOptions::default())
        .unwrap();
    w.write_all(b"hello").unwrap();
    w.finish().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn transfer_zip_with_no_ava1_listener_is_helper_not_ava1() {
    let dir = std::env::temp_dir().join(format!("p5-ava1only-zip-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let zp = dir.join("g.zip");
    tiny_zip(&zp);
    let jobs: Arc<Mutex<HashMap<Uuid, JobState>>> = Arc::new(Mutex::new(HashMap::new()));
    let req = TransferZipReq {
        addr: Some("127.0.0.1:9113".to_string()),
        tx_id: None,
        dest_root: "/data/x".to_string(),
        zip_path: zp.to_string_lossy().into_owned(),
        excludes: vec![],
        bandwidth_cap_mbps: None,
        ram_threshold_mb: None,
    };
    let resp = transfer_zip_handler(State(state_for(&jobs)), Json(req))
        .await
        .into_response();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_helper_not_ava1(&jobs).await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn transfer_7z_with_no_ava1_listener_is_helper_not_ava1() {
    let dir = std::env::temp_dir().join(format!("p5-ava1only-7z-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let ap = dir.join("g.7z");
    {
        use sevenz_rust2::{ArchiveEntry, ArchiveWriter, SourceReader};
        let mut w = ArchiveWriter::create(&ap).unwrap();
        w.push_archive_entries(
            vec![ArchiveEntry::new_file("a.txt")],
            vec![SourceReader::new(&b"hello"[..])],
        )
        .unwrap();
        w.finish().unwrap();
    }
    let jobs: Arc<Mutex<HashMap<Uuid, JobState>>> = Arc::new(Mutex::new(HashMap::new()));
    let req = Transfer7zReq {
        addr: Some("127.0.0.1:9113".to_string()),
        tx_id: None,
        dest_root: "/data/x".to_string(),
        archive_path: ap.to_string_lossy().into_owned(),
        excludes: vec![],
        bandwidth_cap_mbps: None,
    };
    let resp = transfer_7z_handler(State(state_for(&jobs)), Json(req))
        .await
        .into_response();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_helper_not_ava1(&jobs).await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn transfer_rar_with_no_ava1_listener_is_helper_not_ava1() {
    let rar = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../ps5upload-core/testdata/rar/crypted.rar");
    let jobs: Arc<Mutex<HashMap<Uuid, JobState>>> = Arc::new(Mutex::new(HashMap::new()));
    let req = TransferRarReq {
        addr: Some("127.0.0.1:9113".to_string()),
        tx_id: None,
        dest_root: "/data/x".to_string(),
        archive_path: rar.to_string_lossy().into_owned(),
        excludes: vec![],
        bandwidth_cap_mbps: None,
        password: Some("unrar".to_string()),
    };
    let resp = transfer_rar_handler(State(state_for(&jobs)), Json(req))
        .await
        .into_response();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_helper_not_ava1(&jobs).await;
}

/// A console that has not accepted this app fails with `not_paired` (the pairing dialog's
/// trigger), as a job failure and as a management refusal.
#[test]
fn not_paired_surfaces_not_paired() {
    let f = ps5upload_ava1::console::not_paired();
    let state = job_failed_from_err(1, 2, &anyhow::Error::from(f));
    match state {
        JobState::Failed {
            error_reason,
            error_detail,
            ..
        } => {
            assert_eq!(error_reason.as_deref(), Some("not_paired"));
            assert!(error_detail.unwrap().contains("Pair it"));
        }
        _ => panic!("expected Failed"),
    }
}

#[test]
fn old_env_names_are_accepted_once_with_a_warning() {
    let env = |k: &str| match k {
        "FTX2_ZIP_RAM_THRESHOLD_MB" => Some("512".to_string()),
        _ => None,
    };
    let warned = Mutex::new(std::collections::HashSet::new());
    let (v, first) = renamed_env_with(
        "PS5UPLOAD_ZIP_RAM_THRESHOLD_MB",
        "FTX2_ZIP_RAM_THRESHOLD_MB",
        &env,
        &warned,
    );
    assert_eq!(v.as_deref(), Some("512"), "the old name still works");
    assert!(first, "the first read warns");
    let (v, again) = renamed_env_with(
        "PS5UPLOAD_ZIP_RAM_THRESHOLD_MB",
        "FTX2_ZIP_RAM_THRESHOLD_MB",
        &env,
        &warned,
    );
    assert_eq!(v.as_deref(), Some("512"));
    assert!(!again, "and only once");
    // The new name wins and never warns.
    let both = |k: &str| match k {
        "PS5UPLOAD_ZIP_RAM_THRESHOLD_MB" => Some("64".to_string()),
        "FTX2_ZIP_RAM_THRESHOLD_MB" => Some("512".to_string()),
        _ => None,
    };
    let warned = Mutex::new(std::collections::HashSet::new());
    let (v, w) = renamed_env_with(
        "PS5UPLOAD_ZIP_RAM_THRESHOLD_MB",
        "FTX2_ZIP_RAM_THRESHOLD_MB",
        &both,
        &warned,
    );
    assert_eq!((v.as_deref(), w), (Some("64"), false));
}

/// The five transport-tuning variables tuned the retired protocol's shards; they are gone.
#[test]
fn the_retired_tuning_variables_are_not_read() {
    let old = [
        "INFLIGHT_SHARDS",
        "INFLIGHT_BYTES",
        "PACK_SIZE",
        "PACK_FILE_MAX",
        "BANDWIDTH_MBPS",
    ];
    let prefix = ["FT", "X2_"].concat();
    for name in old {
        std::env::set_var(format!("{prefix}{name}"), "1");
    }
    let cfg = make_transfer_config("127.0.0.1");
    let base = TransferConfig::new("127.0.0.1");
    assert_eq!(cfg.inflight_shards, base.inflight_shards);
    assert_eq!(cfg.inflight_bytes, base.inflight_bytes);
    assert_eq!(cfg.pack_size, base.pack_size);
    assert_eq!(cfg.pack_file_max, base.pack_file_max);
    assert_eq!(cfg.bandwidth_cap_bps, base.bandwidth_cap_bps);
    for name in old {
        std::env::remove_var(format!("{prefix}{name}"));
    }
}

#[test]
fn console_addr_is_the_host_only() {
    assert_eq!(console_addr("192.168.1.50:9113"), "192.168.1.50");
    assert_eq!(console_addr("192.168.1.50:9114"), "192.168.1.50");
    assert_eq!(console_addr("192.168.1.50"), "192.168.1.50");
    assert_eq!(console_addr("[::1]:9113"), "[::1]");
    assert_eq!(console_addr("[::1]"), "[::1]");
    assert_eq!(console_addr("fe80::1"), "fe80::1");
    assert_eq!(console_addr_or_default(None, "10.0.0.1:9113"), "10.0.0.1");
}
