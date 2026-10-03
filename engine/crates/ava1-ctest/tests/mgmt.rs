#![cfg(unix)]
//! P3 Task 2: the management dispatcher (payload/src/mgmt_rpc.c) driven over AVA1 against
//! stub handlers, plus static audits of the real table (payload/src/mgmt_table.def).
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ava1::gen::{self, FsMkdir, MgmtText};
use ava1::keys::Identity;
use ava1::peers::PeerStore;
use ava1::session::{connect, Session, Timing};
use ava1::wire::Message;
use ava1_ctest::*;

const SECRET: [u8; 32] = [0x42; 32];
const OK: u16 = gen::STATUS_OK;

fn fast() -> Timing {
    Timing {
        ping_every: Duration::from_millis(100),
        dead_after: Duration::from_millis(2000),
        handshake: Duration::from_millis(500),
        ..Timing::default()
    }
}

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ava1-mgmt-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A started C server with the stub table installed and a paired client.
async fn rig(tag: &str) -> (CServer, Session) {
    let d = dir(tag);
    let me = Arc::new(Identity::generate().unwrap());
    PeerStore::load(&d.join("peers"))
        .unwrap()
        .add(me.public(), "rust client")
        .unwrap();
    let mut mine = PeerStore::in_memory();
    mine.add(Identity::from_secret(SECRET).public(), "C test server")
        .unwrap();
    let srv = CServer::start(SECRET, &d.join("peers"), 0, 100, 2000, 500);
    assert_eq!(mgmt::install(), 0);
    let s = connect(
        &srv.addr(),
        me,
        Arc::new(Mutex::new(mine)),
        "laptop",
        fast(),
    )
    .await
    .unwrap();
    (srv, s)
}

fn text(s: &str) -> Vec<u8> {
    MgmtText {
        body: s.as_bytes().to_vec(),
        more: None,
    }
    .to_bytes()
    .unwrap()
}

fn untext(b: &[u8]) -> MgmtText {
    MgmtText::decode(b).expect("a MgmtText reply")
}

fn mkdir(path: &str) -> Vec<u8> {
    FsMkdir {
        path: path.into(),
        mode: 0o755,
        parents: 1,
    }
    .to_bytes()
    .unwrap()
}

const VOLUMES: u16 = gen::METHOD_FS_VOLUMES;
const MKDIR: u16 = gen::METHOD_FS_MKDIR;
const LAUNCH: u16 = gen::METHOD_APP_LAUNCH;
const APP_LIST: u16 = gen::METHOD_APP_LIST;
const BIG: u16 = gen::METHOD_PROC_PROCESS_LIST;
const ENV: u16 = gen::METHOD_FS_MOUNT;
const TWO: u16 = gen::METHOD_FS_UNMOUNT;
const SILENT: u16 = gen::METHOD_FS_MOUNT_PKG;

// ---- the table ----

fn payload() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../payload")
}

/// Runs one check of payload/tools/mgmt_audit.py; its output is the failure message.
fn audit(check: &str) {
    let out = Command::new("python3")
        .arg(payload().join("tools/mgmt_audit.py"))
        .arg(check)
        .output()
        .expect("python3 runs payload/tools/mgmt_audit.py");
    assert!(
        out.status.success(),
        "mgmt_audit.py {check}:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The real table's lines: (method name, flags).
fn real_table() -> Vec<(String, String)> {
    let src = std::fs::read_to_string(payload().join("src/mgmt_table.def")).unwrap();
    src.lines()
        .filter(|l| l.starts_with("MGMT_H"))
        .map(|l| {
            let inner = &l[l.find('(').unwrap() + 1..l.rfind(')').unwrap()];
            let f: Vec<&str> = inner.split(',').map(str::trim).collect();
            assert_eq!(f.len(), 6, "{l}");
            (f[0].to_string(), f[3].to_string())
        })
        .collect()
}

#[test]
fn c_mgmt_table_has_no_duplicate_methods() {
    // The installer refuses a repeated method...
    assert_eq!(mgmt::install_duplicate(), -1);
    // ...and the real table has none, names real constants and real handlers.
    let mut names: Vec<String> = real_table().into_iter().map(|t| t.0).collect();
    let n = names.len();
    assert!(n >= 5, "the first slice is in the table");
    names.sort();
    names.dedup();
    assert_eq!(names.len(), n, "duplicate method in mgmt_table.def");
    audit("table");
}

#[test]
fn c_mgmt_table_flags_sony_methods() {
    // Every handler that can reach register/profile/registry/Remote Play/notification code or a
    // Sony API is flagged MGMT_SONY (the audit derives it from the call graph).
    audit("sony");
    let t = real_table();
    let flag = |m: &str| t.iter().find(|e| e.0 == m).unwrap().1.clone();
    assert_eq!(flag("AVA1_METHOD_APP_LAUNCH"), "MGMT_SONY");
    assert_eq!(flag("AVA1_METHOD_FS_MKDIR"), "0");
}

#[test]
fn c_mgmt_handlers_never_read_the_socket_and_keep_small_stacks() {
    // The capture path calls handlers with fd = -1: none may read client_fd after the header.
    audit("recv");
    // And no table handler reaches a stack array of 16 KiB or more (SPEC.md section 7.3).
    audit("stack");
}

// ---- the dispatcher ----

#[tokio::test(flavor = "multi_thread")]
async fn c_mgmt_unknown_method_is_err_unknown_method() {
    let (_srv, s) = rig("unknown").await;
    for m in [0x7fffu16, 141, 4, 100] {
        let r = s.rpc(m, &[]).await.unwrap();
        assert_eq!(r.status, gen::ERR_UNKNOWN_METHOD, "method {m}");
        assert_eq!(r.body, b"unknown method");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn c_mgmt_text_method_answers_a_mgmt_text() {
    let (_srv, s) = rig("text").await;
    let r = s.rpc(VOLUMES, &[]).await.unwrap();
    assert_eq!(r.status, OK);
    let t = untext(&r.body);
    assert_eq!(t.body, br#"{"volumes":[{"path":"/data"}]}"#);
    assert_eq!(t.more, None);
    // A request that is not a MgmtText is the peer's error.
    let r = s.rpc(VOLUMES, &[1, 2, 3]).await.unwrap();
    assert_eq!(r.status, gen::ERR_PROTOCOL);
    assert_eq!(r.body, b"bad MgmtText request");
}

#[tokio::test(flavor = "multi_thread")]
async fn c_mgmt_error_frame_becomes_status_and_cause() {
    let (_srv, s) = rig("errors").await;
    // typed fs method: ok, and the path reaches the legacy handler JSON-escaped
    let r = s.rpc(MKDIR, &mkdir("/data/a")).await.unwrap();
    assert_eq!((r.status, r.body.len()), (OK, 0));
    assert_eq!(mgmt::last_path(), "/data/a");
    let r = s.rpc(MKDIR, &mkdir("/data/q\"uote")).await.unwrap();
    assert_eq!(r.status, OK);
    assert_eq!(mgmt::last_path(), "/data/q\\\"uote");
    // legacy ERROR frames: the token is the cause, the status is the closest ERR_*
    let r = s.rpc(MKDIR, &mkdir("/denied")).await.unwrap();
    assert_eq!(
        (r.status, r.body.as_slice()),
        (gen::ERR_PATH, &b"fs_mkdir_path_not_allowed"[..])
    );
    let r = s.rpc(MKDIR, &mkdir("/fail")).await.unwrap();
    assert_eq!(
        (r.status, r.body.as_slice()),
        (gen::ERR_IO, &b"fs_mkdir_failed"[..])
    );
    let r = s.rpc(MKDIR, &[0xff]).await.unwrap();
    assert_eq!(r.status, gen::ERR_PROTOCOL);
    // a successful frame whose body is {"ok":false,...} is an error status, never OK
    let r = s
        .rpc(LAUNCH, &text(r#"{"title_id":"NOPE00001"}"#))
        .await
        .unwrap();
    assert_eq!(
        (r.status, r.body.as_slice()),
        (gen::ERR_INTERNAL, &b"launch_failed"[..])
    );
    // a missing argument is the peer's
    let r = s.rpc(LAUNCH, &text("{}")).await.unwrap();
    assert_eq!(
        (r.status, r.body.as_slice()),
        (gen::ERR_PROTOCOL, &b"launch_title_id_missing"[..])
    );
    // the first error frame wins over a later success frame
    let r = s.rpc(TWO, &text("{}")).await.unwrap();
    assert_eq!(
        (r.status, r.body.as_slice()),
        (gen::ERR_INTERNAL, &b"fs_unmount_failed"[..])
    );
    // a handler that sends nothing is an error, not an empty OK
    let r = s.rpc(SILENT, &text("{}")).await.unwrap();
    assert_eq!(
        (r.status, r.body.as_slice()),
        (gen::ERR_INTERNAL, &b"handler sent no reply"[..])
    );
    // success still works
    let r = s
        .rpc(LAUNCH, &text(r#"{"title_id":"PPSA00001"}"#))
        .await
        .unwrap();
    assert_eq!((r.status, untext(&r.body).body.len()), (OK, 0));
}

#[tokio::test(flavor = "multi_thread")]
async fn c_mgmt_reply_over_cap_is_an_error_not_a_truncation() {
    let (_srv, s) = rig("cap").await;
    let max = gen::RPC_TEXT_MAX as usize;
    let ask = |n: usize| text(&format!("{{\"n\":{n}}}"));
    // exactly RPC_TEXT_MAX bytes of text fit a reply
    let r = s.rpc(BIG, &ask(max)).await.unwrap();
    assert_eq!(r.status, OK);
    let t = untext(&r.body);
    assert_eq!(t.body.len(), max);
    assert!(t.body.iter().all(|b| *b == b'x'));
    // one byte more, and far more, are ERR_INTERNAL "reply truncated", never a clipped OK
    for n in [max + 1, max + 16, 400_000] {
        let r = s.rpc(BIG, &ask(n)).await.unwrap();
        assert_eq!(r.status, gen::ERR_INTERNAL, "n = {n}");
        assert_eq!(r.body, b"reply truncated");
    }
    // the session carries on
    assert_eq!(s.rpc(VOLUMES, &[]).await.unwrap().status, OK);
}

#[tokio::test(flavor = "multi_thread")]
async fn c_mgmt_workers_use_512k_stacks_and_elevate() {
    let (_srv, s) = rig("env").await;
    let (e0, l0, _, _) = mgmt::stats();
    let r = s.rpc(ENV, &text("{}")).await.unwrap();
    assert_eq!(r.status, OK);
    let body = String::from_utf8(untext(&r.body).body).unwrap();
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let stack = v["stack"].as_u64().unwrap() as usize;
    // 512 KiB (macOS pads the reported allocation by up to ~64 KiB)
    assert!(
        (512 * 1024..=512 * 1024 + 64 * 1024).contains(&stack),
        "stack {stack}"
    );
    // the environment hook ran on the handler's own thread, with the legacy frame number
    assert_eq!(
        v["marker"].as_u64().unwrap(),
        52,
        "enter() set the marker the handler saw"
    );
    let (e1, l1, last, _) = mgmt::stats();
    assert_eq!(
        (e1 - e0, l1 - l0, last),
        (1, 1, 52),
        "enter and leave ran once, for FsMount"
    );
    // a data-plane method number keeps the 256 KiB worker
    let r = s.rpc(19, &[]).await.unwrap();
    assert_eq!(r.status, OK);
    let small: usize = String::from_utf8(r.body).unwrap().parse().unwrap();
    assert!(small <= 256 * 1024 + 64 * 1024, "method 19 stack {small}");
    // errors leave the environment too
    let _ = s.rpc(MKDIR, &mkdir("/denied")).await.unwrap();
    let (e2, l2, _, _) = mgmt::stats();
    assert_eq!(e2 - e1, l2 - l1);
}

#[tokio::test(flavor = "multi_thread")]
async fn c_mgmt_sony_method_calls_are_serialised_by_the_handler_not_the_dispatcher() {
    // Four concurrent calls reach the handler; the stub's stand-in for sony_api_lock never
    // sees two inside at once. (The dispatcher adds no lock and no Sony call of its own.)
    let (_srv, s) = rig("sony").await;
    let s = Arc::new(s);
    let mut js = Vec::new();
    for _ in 0..4 {
        let s = s.clone();
        js.push(tokio::spawn(async move {
            s.rpc(LAUNCH, &text(r#"{"title_id":"PPSA00001"}"#))
                .await
                .unwrap()
                .status
        }));
    }
    for j in js {
        assert_eq!(j.await.unwrap(), OK);
    }
    assert_eq!(mgmt::stats().3, 1);
}

fn app_ids(t: &MgmtText) -> Vec<String> {
    let v: serde_json::Value = serde_json::from_slice(&t.body).expect("a page is valid JSON");
    v["apps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["title_id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn c_mgmt_app_list_pages_cover_every_entry() {
    let (_srv, s) = rig("pages").await;
    // 6,000 entries is more than one 256 KiB reply holds (the handler's own buffer is bigger).
    mgmt::set_apps(6000);
    let all: Vec<String> = (0..6000).map(|i| format!("PPSA{i:05}")).collect();

    // no limit: the node fills each reply and says `more`
    let mut got = Vec::new();
    let mut calls = 0;
    loop {
        let req = text(&format!("{{\"offset\":{}}}", got.len()));
        let r = s.rpc(APP_LIST, &req).await.unwrap();
        assert_eq!(r.status, OK, "{}", String::from_utf8_lossy(&r.body));
        let t = untext(&r.body);
        assert!(t.body.len() <= gen::RPC_TEXT_MAX as usize);
        let ids = app_ids(&t);
        assert!(!ids.is_empty());
        got.extend(ids);
        calls += 1;
        match t.more {
            Some(1) => continue,
            Some(0) | None => break,
            m => panic!("more = {m:?}"),
        }
    }
    assert!(
        calls >= 2,
        "6,000 entries must not fit one reply (took {calls})"
    );
    assert_eq!(got, all, "every entry exactly once, in order");

    // an explicit limit
    let r = s
        .rpc(APP_LIST, &text(r#"{"offset":10,"limit":3}"#))
        .await
        .unwrap();
    let t = untext(&r.body);
    assert_eq!(app_ids(&t), ["PPSA00010", "PPSA00011", "PPSA00012"]);
    assert_eq!(t.more, Some(1));
    // the last page: no more
    let r = s
        .rpc(APP_LIST, &text(r#"{"offset":5998,"limit":50}"#))
        .await
        .unwrap();
    let t = untext(&r.body);
    assert_eq!(app_ids(&t), ["PPSA05998", "PPSA05999"]);
    assert_eq!(t.more, Some(0));
    // past the end: an empty array, not an error
    let r = s.rpc(APP_LIST, &text(r#"{"offset":9999}"#)).await.unwrap();
    assert_eq!(r.status, OK);
    let t = untext(&r.body);
    assert!(app_ids(&t).is_empty());
    assert_eq!(t.more, Some(0));

    // a small list is one reply, with the legacy shape intact
    mgmt::set_apps(2);
    let r = s.rpc(APP_LIST, &[]).await.unwrap();
    let t = untext(&r.body);
    assert_eq!(app_ids(&t), ["PPSA00000", "PPSA00001"]);
    assert_eq!(t.more, Some(0));
    mgmt::set_apps(0);
    let r = s.rpc(APP_LIST, &[]).await.unwrap();
    assert_eq!(untext(&r.body).body, br#"{"apps":[]}"#);
}
