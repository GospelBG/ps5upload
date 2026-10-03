#![cfg(unix)]
//! P3 Task 4: the node, log, net and filesystem management methods on the C payload (host build),
//! driven over AVA1 by a Rust client. `fs.*` are the payload's own native runners
//! (`payload/src/mgmt_fs.c`) acting on a real temp directory; the log/net/node runners run over
//! stub handlers that answer through the real capture sink.
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use ava1::gen::{self, FsEntry, FsList, FsListResult, FsPath, FsRead, FsReadResult, FsStat, FsWrite, MgmtText};
use ava1::keys::Identity;
use ava1::peers::PeerStore;
use ava1::session::{connect, Session, Timing};
use ava1::wire::Message;
use ava1_ctest::*;

const SECRET: [u8; 32] = [0x43; 32];
const OK: u16 = gen::STATUS_OK;

/// The installed table and policy are process-wide: one test at a time.
static ONE: Mutex<()> = Mutex::new(());

struct Rig {
    _one: MutexGuard<'static, ()>,
    _srv: CServer,
    s: Session,
    root: PathBuf,
}

fn fast() -> Timing {
    Timing {
        ping_every: Duration::from_millis(100),
        dead_after: Duration::from_millis(3000),
        handshake: Duration::from_millis(800),
        ..Timing::default()
    }
}

async fn rig(tag: &str) -> Rig {
    let one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    let base = std::env::temp_dir().join(format!("ava1-mfs-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    std::fs::create_dir_all(&root).unwrap();
    // The policy compares against the path as the C code sees it: the canonical one.
    let root = root.canonicalize().unwrap();
    assert_eq!(mgmt_fs::install(&root), 0);
    mgmt_fs::set(false, 0, 0);
    let me = Arc::new(Identity::generate().unwrap());
    PeerStore::load(&base.join("peers"))
        .unwrap()
        .add(me.public(), "rust client")
        .unwrap();
    let mut mine = PeerStore::in_memory();
    mine.add(Identity::from_secret(SECRET).public(), "C test server")
        .unwrap();
    let srv = CServer::start(SECRET, &base.join("peers"), 0, 100, 3000, 800);
    let s = connect(&srv.addr(), me, Arc::new(Mutex::new(mine)), "laptop", fast())
        .await
        .unwrap();
    Rig {
        _one: one,
        _srv: srv,
        s,
        root,
    }
}

impl Rig {
    fn p(&self, rel: &str) -> String {
        format!("{}/{rel}", self.root.display())
    }
    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }
    async fn rpc<M: Message>(&self, method: u16, m: &M) -> (u16, Vec<u8>) {
        let r = self.s.rpc(method, &m.to_bytes().unwrap()).await.unwrap();
        (r.status, r.body)
    }
    async fn raw(&self, method: u16, body: &[u8]) -> (u16, Vec<u8>) {
        let r = self.s.rpc(method, body).await.unwrap();
        (r.status, r.body)
    }
    async fn list(&self, path: &str, offset: u32, limit: u16) -> FsListResult {
        let (st, b) = self
            .rpc(
                gen::METHOD_FS_LIST,
                &FsList {
                    path: path.into(),
                    offset,
                    limit,
                },
            )
            .await;
        assert_eq!(st, OK, "{}", String::from_utf8_lossy(&b));
        FsListResult::decode(&b).unwrap()
    }
    async fn write(&self, rel: &str, offset: u64, flags: u32, data: &[u8], mode: Option<u32>) -> (u16, Vec<u8>) {
        self.rpc(
            gen::METHOD_FS_WRITE,
            &FsWrite {
                path: self.p(rel),
                offset,
                flags,
                data: data.to_vec(),
                mode,
            },
        )
        .await
    }
    async fn read(&self, rel: &str, offset: u64, len: u32, flags: u32) -> (u16, Vec<u8>) {
        self.rpc(
            gen::METHOD_FS_READ,
            &FsRead {
                path: self.p(rel),
                offset,
                len,
                flags,
            },
        )
        .await
    }
}

fn cause(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn text(s: &str) -> Vec<u8> {
    MgmtText {
        body: s.as_bytes().to_vec(),
        more: None,
    }
    .to_bytes()
    .unwrap()
}

// ---- fs.list ----

#[tokio::test(flavor = "multi_thread")]
async fn list_dir_pages_cover_a_20k_entry_directory() {
    let r = rig("list20k").await;
    let d = r.path("big");
    std::fs::create_dir_all(&d).unwrap();
    for i in 0..20_000u32 {
        std::fs::write(d.join(format!("f{i:05}")), b"").unwrap();
    }
    let mut seen = std::collections::BTreeSet::new();
    let (mut offset, mut pages) = (0u32, 0);
    loop {
        let page = r.list(&r.p("big"), offset, 256).await;
        assert!(page.entries.len() <= 256);
        pages += 1;
        for e in &page.entries {
            assert!(seen.insert(e.name.clone()), "{} listed twice", e.name);
        }
        offset += page.entries.len() as u32;
        if page.more == 0 {
            assert_eq!(page.total_scanned, 20_000, "the last page counts everything");
            break;
        }
        assert_eq!(page.entries.len(), 256, "a page that says more is full");
    }
    assert_eq!(seen.len(), 20_000);
    assert_eq!(pages, 79, "20,000 / 256 rounded up");
}

#[tokio::test(flavor = "multi_thread")]
async fn list_reports_kinds_sizes_mtime_and_mode_and_defaults_the_limit() {
    let r = rig("listkinds").await;
    std::fs::create_dir_all(r.path("d/sub")).unwrap();
    std::fs::write(r.path("d/file"), b"12345").unwrap();
    std::fs::set_permissions(r.path("d/file"), std::fs::Permissions::from_mode(0o640)).unwrap();
    std::os::unix::fs::symlink("file", r.path("d/link")).unwrap();
    std::os::unix::fs::symlink("/nonexistent-target", r.path("d/dangling")).unwrap();
    let page = r.list(&r.p("d"), 0, 0).await; // limit 0 = the default (256)
    let by: std::collections::HashMap<String, &FsEntry> =
        page.entries.iter().map(|e| (e.name.clone(), e)).collect();
    assert_eq!(by.len(), 4);
    assert_eq!((by["file"].kind, by["file"].size), (gen::ENTRY_FILE, 5));
    assert_eq!(by["file"].mode, Some(0o640));
    assert!(by["file"].mtime.unwrap() > 1_600_000_000);
    assert_eq!(by["sub"].kind, gen::ENTRY_DIR);
    // a symbolic link is reported as a link, not followed (a dangling one too)
    assert_eq!(by["link"].kind, gen::ENTRY_LINK);
    assert_eq!(by["dangling"].kind, gen::ENTRY_LINK);
    assert_eq!(page.more, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn list_refuses_what_the_ftx2_handler_refused() {
    let r = rig("listerr").await;
    let ask = |p: &str| FsList {
        path: p.into(),
        offset: 0,
        limit: 10,
    };
    let (st, b) = r.rpc(gen::METHOD_FS_LIST, &ask("relative/path")).await;
    assert_eq!((st, cause(&b).as_str()), (gen::ERR_PATH, "fs_list_dir_bad_path"));
    let (st, b) = r.rpc(gen::METHOD_FS_LIST, &ask("/a/../b")).await;
    assert_eq!((st, cause(&b).as_str()), (gen::ERR_PATH, "fs_list_dir_path_denied"));
    let (st, b) = r.rpc(gen::METHOD_FS_LIST, &ask(&r.p("missing"))).await;
    assert_eq!((st, cause(&b).as_str()), (gen::ERR_IO, "fs_list_dir_opendir_errno_2"));
    // a name that merely contains ".." is fine
    std::fs::create_dir_all(r.path("..cache/x..bak")).unwrap();
    assert_eq!(r.list(&r.p("..cache"), 0, 10).await.entries.len(), 1);
    let (st, _) = r.raw(gen::METHOD_FS_LIST, &[1, 2]).await;
    assert_eq!(st, gen::ERR_PROTOCOL);
}

// ---- fs.stat ----

#[tokio::test(flavor = "multi_thread")]
async fn fs_stat_reports_dev() {
    let r = rig("stat").await;
    std::fs::write(r.path("f"), b"hello").unwrap();
    std::fs::create_dir_all(r.path("d")).unwrap();
    std::os::unix::fs::symlink("/nonexistent-target", r.path("dangling")).unwrap();
    let stat = |rel: &str| FsPath { path: r.p(rel) };
    let (st, b) = r.rpc(gen::METHOD_FS_STAT, &stat("f")).await;
    assert_eq!(st, OK);
    let s = FsStat::decode(&b).unwrap();
    let md = std::fs::metadata(r.path("f")).unwrap();
    assert_eq!((s.kind, s.size, s.dev), (gen::ENTRY_FILE, 5, md.dev()));
    assert_eq!(s.mode, md.mode() & 0o7777);
    assert_eq!(s.mtime, md.mtime() as u64);
    let (_, b) = r.rpc(gen::METHOD_FS_STAT, &stat("d")).await;
    assert_eq!(FsStat::decode(&b).unwrap().kind, gen::ENTRY_DIR);
    let (st, b) = r.rpc(gen::METHOD_FS_STAT, &stat("dangling")).await;
    assert_eq!(st, OK, "a dangling link exists as a link");
    assert_eq!(FsStat::decode(&b).unwrap().kind, gen::ENTRY_LINK);
    // a missing path is an error, not an empty OK: that is what the 1-byte FsRead probe was
    let (st, b) = r.rpc(gen::METHOD_FS_STAT, &stat("nope")).await;
    assert_eq!((st, cause(&b).as_str()), (gen::ERR_IO, "fs_stat_failed_errno_2"));
    let (st, _) = r
        .rpc(gen::METHOD_FS_STAT, &FsPath { path: "rel".into() })
        .await;
    assert_eq!(st, gen::ERR_PATH);
    let (st, _) = r
        .rpc(gen::METHOD_FS_STAT, &FsPath { path: "/a/../b".into() })
        .await;
    assert_eq!(st, gen::ERR_PATH);
}

// ---- fs.mkdir ----

#[tokio::test(flavor = "multi_thread")]
async fn mkdir_honours_mode_and_parents() {
    let r = rig("mkdir").await;
    let mk = |rel: &str, mode: u32, parents: u8| gen::FsMkdir {
        path: r.p(rel),
        mode,
        parents,
    };
    let (st, b) = r.rpc(gen::METHOD_FS_MKDIR, &mk("a/b/c", 0o750, 1)).await;
    assert_eq!((st, b.len()), (OK, 0));
    assert_eq!(
        std::fs::metadata(r.path("a/b/c")).unwrap().mode() & 0o7777,
        0o750,
        "mode applies to the new directory (the umask does not eat it)"
    );
    // parents = 0: a missing parent is an error and nothing is created
    let (st, b) = r.rpc(gen::METHOD_FS_MKDIR, &mk("x/y", 0o755, 0)).await;
    assert_eq!((st, cause(&b).as_str()), (gen::ERR_IO, "fs_mkdir_failed"));
    assert!(!r.path("x").exists());
    // an existing directory is fine (mkdir -p) and keeps its mode; a file in the way is not
    let (st, _) = r.rpc(gen::METHOD_FS_MKDIR, &mk("a/b/c", 0o700, 1)).await;
    assert_eq!(st, OK);
    assert_eq!(std::fs::metadata(r.path("a/b/c")).unwrap().mode() & 0o7777, 0o750);
    std::fs::write(r.path("file"), b"").unwrap();
    let (st, b) = r.rpc(gen::METHOD_FS_MKDIR, &mk("file", 0o755, 1)).await;
    assert_eq!((st, cause(&b).as_str()), (gen::ERR_EXISTS, "fs_mkdir_exists_not_dir"));
    // outside the policy
    let (st, b) = r
        .rpc(
            gen::METHOD_FS_MKDIR,
            &gen::FsMkdir {
                path: "/etc/nope".into(),
                mode: 0o755,
                parents: 1,
            },
        )
        .await;
    assert_eq!((st, cause(&b).as_str()), (gen::ERR_PATH, "fs_mkdir_path_not_allowed"));
}

// ---- fs.rename ----

fn rename(r: &Rig, from: &str, to: &str, overwrite: u8) -> gen::FsRename {
    gen::FsRename {
        from: r.p(from),
        to: r.p(to),
        overwrite,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn fs_rename_cross_device_is_refused_before_any_rename() {
    let r = rig("xdev").await;
    std::fs::create_dir_all(r.path("mnt2")).unwrap();
    std::fs::write(r.path("src"), b"data").unwrap();
    // The guard compares the source's device with the destination's parent: here "/mnt2" is another device.
    mgmt_fs::set(true, 0, 0);
    let (st, b) = r
        .rpc(gen::METHOD_FS_RENAME, &rename(&r, "src", "mnt2/dst", 1))
        .await;
    assert_eq!(st, gen::ERR_CROSS_DEVICE);
    assert_eq!(cause(&b), "fs_move_cross_mount");
    assert!(r.path("src").exists(), "nothing moved");
    assert!(!r.path("mnt2/dst").exists());
    // Same device: it renames.
    let (st, b) = r
        .rpc(gen::METHOD_FS_RENAME, &rename(&r, "src", "dst", 1))
        .await;
    assert_eq!((st, b.len()), (OK, 0));
    assert!(r.path("dst").exists() && !r.path("src").exists());
    // And an unknown device (a missing source) is not read as "same": the plain errno comes back.
    let (st, b) = r
        .rpc(gen::METHOD_FS_RENAME, &rename(&r, "ghost", "dst2", 1))
        .await;
    assert_eq!((st, cause(&b).as_str()), (gen::ERR_IO, "fs_move_failed"));
}

#[tokio::test(flavor = "multi_thread")]
async fn fs_rename_overwrite_and_policy() {
    let r = rig("rename").await;
    std::fs::write(r.path("a"), b"A").unwrap();
    std::fs::write(r.path("b"), b"B").unwrap();
    let (st, b) = r
        .rpc(gen::METHOD_FS_RENAME, &rename(&r, "a", "b", 0))
        .await;
    assert_eq!((st, cause(&b).as_str()), (gen::ERR_EXISTS, "fs_move_exists"));
    assert_eq!(std::fs::read(r.path("b")).unwrap(), b"B");
    let (st, _) = r
        .rpc(gen::METHOD_FS_RENAME, &rename(&r, "a", "b", 1))
        .await;
    assert_eq!(st, OK);
    assert_eq!(std::fs::read(r.path("b")).unwrap(), b"A");
    let (st, b) = r
        .rpc(
            gen::METHOD_FS_RENAME,
            &gen::FsRename {
                from: r.p("b"),
                to: "/etc/passwd2".into(),
                overwrite: 1,
            },
        )
        .await;
    assert_eq!((st, cause(&b).as_str()), (gen::ERR_PATH, "fs_move_path_not_allowed"));
}

// ---- fs.chmod ----

#[tokio::test(flavor = "multi_thread")]
async fn fs_chmod_sets_the_bits_and_obeys_the_policy() {
    let r = rig("chmod").await;
    std::fs::write(r.path("f"), b"").unwrap();
    let (st, b) = r
        .rpc(
            gen::METHOD_FS_CHMOD,
            &gen::FsChmod {
                path: r.p("f"),
                mode: 0o600,
            },
        )
        .await;
    assert_eq!((st, b.len()), (OK, 0));
    assert_eq!(std::fs::metadata(r.path("f")).unwrap().mode() & 0o7777, 0o600);
    let (st, b) = r
        .rpc(
            gen::METHOD_FS_CHMOD,
            &gen::FsChmod {
                path: r.p("missing"),
                mode: 0o600,
            },
        )
        .await;
    assert_eq!((st, cause(&b).as_str()), (gen::ERR_IO, "fs_chmod_failed"));
    let (st, b) = r
        .rpc(
            gen::METHOD_FS_CHMOD,
            &gen::FsChmod {
                path: "/etc/hosts".into(),
                mode: 0o777,
            },
        )
        .await;
    assert_eq!((st, cause(&b).as_str()), (gen::ERR_PATH, "fs_chmod_path_not_allowed"));
}

// ---- fs.read ----

#[tokio::test(flavor = "multi_thread")]
async fn fs_read_windows_eof_and_the_unsafe_flag() {
    let r = rig("read").await;
    let data: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(r.path("big"), &data).unwrap();
    // a window in the middle: not the end
    let (st, b) = r.read("big", 10, 100, 0).await;
    assert_eq!(st, OK);
    let m = FsReadResult::decode(&b).unwrap();
    assert_eq!((&m.data[..], m.eof), (&data[10..110], 0));
    // the cap: a longer ask is a short read (eof 0), never an error
    let (_, b) = r.read("big", 0, u32::MAX, 0).await;
    let m = FsReadResult::decode(&b).unwrap();
    assert_eq!((m.data.len(), m.eof), (gen::FS_READ_MAX as usize, 0));
    assert_eq!(m.data, &data[..gen::FS_READ_MAX as usize]);
    // the last bytes carry eof; an ask ending exactly at the end does too
    let (_, b) = r.read("big", 999_900, 1000, 0).await;
    let m = FsReadResult::decode(&b).unwrap();
    assert_eq!((m.data.len(), m.eof), (100, 1));
    let (_, b) = r.read("big", 999_900, 100, 0).await;
    assert_eq!(FsReadResult::decode(&b).unwrap().eof, 1);
    // at and past the end: empty with eof
    for off in [1_000_000u64, 5_000_000] {
        let (st, b) = r.read("big", off, 10, 0).await;
        let m = FsReadResult::decode(&b).unwrap();
        assert_eq!((st, m.data.len(), m.eof), (OK, 0, 1));
    }
    // errors keep the legacy tokens
    let (st, b) = r.read("nope", 0, 10, 0).await;
    assert_eq!((st, cause(&b).as_str()), (gen::ERR_IO, "fs_read_stat_failed"));
    std::fs::create_dir_all(r.path("dir")).unwrap();
    let (st, b) = r.read("dir", 0, 10, 0).await;
    assert_eq!((st, cause(&b).as_str()), (gen::ERR_IO, "fs_read_not_regular_file"));
    // the system tree is readable only with FSR_UNSAFE (the policy sees the flag)
    std::fs::create_dir_all(r.path("sys")).unwrap();
    std::fs::write(r.path("sys/lib"), b"elf").unwrap();
    let (st, b) = r.read("sys/lib", 0, 10, 0).await;
    assert_eq!((st, cause(&b).as_str()), (gen::ERR_PATH, "fs_read_path_not_allowed"));
    let (st, b) = r.read("sys/lib", 0, 10, gen::FSR_UNSAFE).await;
    assert_eq!(st, OK);
    assert_eq!(FsReadResult::decode(&b).unwrap().data, b"elf");
    assert!(mgmt_fs::stats().2 >= 1, "the unsafe flag reached the policy");
    let (st, _) = r
        .rpc(
            gen::METHOD_FS_READ,
            &FsRead {
                path: "/etc/hosts".into(),
                offset: 0,
                len: 10,
                flags: 0,
            },
        )
        .await;
    assert_eq!(st, gen::ERR_PATH);
}

// ---- fs.write ----

const CREATE: u32 = gen::FSW_CREATE;
const OVERWRITE: u32 = gen::FSW_OVERWRITE;
const AT: u32 = gen::FSW_AT_OFFSET;
const COMMIT: u32 = gen::FSW_COMMIT;

#[tokio::test(flavor = "multi_thread")]
async fn fs_write_whole_file_is_atomic_and_honours_create_and_mode() {
    let r = rig("write1").await;
    let (st, b) = r.write("w", 0, OVERWRITE, b"hello", None).await;
    assert_eq!((st, b.len()), (OK, 0));
    assert_eq!(std::fs::read(r.path("w")).unwrap(), b"hello");
    assert_eq!(std::fs::metadata(r.path("w")).unwrap().mode() & 0o7777, 0o644);
    assert!(!r.path("w.ps5upload.tmp").exists(), "committed in the same call");
    // default (neither flag) overwrites; CREATE refuses an existing file and writes nothing
    let (st, _) = r.write("w", 0, 0, b"v2", Some(0o600)).await;
    assert_eq!(st, OK);
    assert_eq!(std::fs::read(r.path("w")).unwrap(), b"v2");
    assert_eq!(std::fs::metadata(r.path("w")).unwrap().mode() & 0o7777, 0o600);
    let (st, b) = r.write("w", 0, CREATE, b"v3", None).await;
    assert_eq!((st, cause(&b).as_str()), (gen::ERR_EXISTS, "exists"));
    assert_eq!(std::fs::read(r.path("w")).unwrap(), b"v2");
    assert!(!r.path("w.ps5upload.tmp").exists());
    let (st, _) = r.write("new", 0, CREATE, b"x", None).await;
    assert_eq!(st, OK);
    // an empty file is a valid file
    let (st, _) = r.write("empty", 0, 0, b"", None).await;
    assert_eq!(st, OK);
    assert_eq!(std::fs::read(r.path("empty")).unwrap(), b"");
    assert!(mgmt_fs::stats().0 >= 4, "successful writes are counted as commands");
}

#[tokio::test(flavor = "multi_thread")]
async fn fs_write_chunks_commit_on_the_last_and_a_retry_starts_clean() {
    let r = rig("write2").await;
    let part = |i: usize| vec![b'a' + i as u8; 1000];
    // chunk 0 at offset 0, chunk 1 at 1000, last (with COMMIT) at 2000
    let (st, _) = r.write("c", 0, OVERWRITE | AT, &part(0), None).await;
    assert_eq!(st, OK);
    assert!(!r.path("c").exists(), "not committed yet");
    assert_eq!(std::fs::metadata(r.path("c.ps5upload.tmp")).unwrap().len(), 1000);
    let (st, _) = r.write("c", 1000, OVERWRITE | AT, &part(1), None).await;
    assert_eq!(st, OK);
    // an abandoned attempt: a new chunk at offset 0 truncates the tmp file first
    let (st, _) = r.write("c", 0, OVERWRITE | AT, &part(0), None).await;
    assert_eq!(st, OK);
    assert_eq!(std::fs::metadata(r.path("c.ps5upload.tmp")).unwrap().len(), 1000);
    let (st, _) = r.write("c", 1000, OVERWRITE | AT, &part(1), None).await;
    assert_eq!(st, OK);
    let (st, _) = r.write("c", 2000, OVERWRITE | AT | COMMIT, &part(2), Some(0o640)).await;
    assert_eq!(st, OK);
    let mut want = part(0);
    want.extend(part(1));
    want.extend(part(2));
    assert_eq!(std::fs::read(r.path("c")).unwrap(), want);
    assert!(!r.path("c.ps5upload.tmp").exists());
    assert_eq!(std::fs::metadata(r.path("c")).unwrap().mode() & 0o7777, 0o640);
    // CREATE is checked at commit: the target appeared while chunks were sent
    let (st, _) = r.write("d", 0, CREATE | AT, b"zz", None).await;
    assert_eq!(st, OK);
    std::fs::write(r.path("d"), b"someone else").unwrap();
    let (st, b) = r.write("d", 2, CREATE | AT | COMMIT, b"yy", None).await;
    assert_eq!((st, cause(&b).as_str()), (gen::ERR_EXISTS, "exists"));
    assert_eq!(std::fs::read(r.path("d")).unwrap(), b"someone else");
    assert!(!r.path("d.ps5upload.tmp").exists(), "a refused commit leaves no tmp");
    // APPEND adds to the end whatever offset says
    let (st, _) = r.write("e", 0, OVERWRITE | gen::FSW_APPEND, b"12", None).await;
    assert_eq!(st, OK);
    let (st, _) = r
        .write("e", 0, OVERWRITE | gen::FSW_APPEND | COMMIT, b"34", None)
        .await;
    assert_eq!(st, OK);
    assert_eq!(std::fs::read(r.path("e")).unwrap(), b"1234");
}

#[tokio::test(flavor = "multi_thread")]
async fn fs_write_over_48k_is_refused_not_truncated() {
    let r = rig("write3").await;
    let big = vec![7u8; gen::FSW_CHUNK_MAX as usize + 1];
    let (st, b) = r.write("big", 0, 0, &big, None).await;
    assert_eq!((st, cause(&b).as_str()), (gen::ERR_PROTOCOL, "too_large"));
    assert!(!r.path("big").exists() && !r.path("big.ps5upload.tmp").exists());
    // exactly one chunk is fine
    let ok = vec![7u8; gen::FSW_CHUNK_MAX as usize];
    let (st, _) = r.write("big", 0, 0, &ok, None).await;
    assert_eq!(st, OK);
    assert_eq!(std::fs::metadata(r.path("big")).unwrap().len(), ok.len() as u64);
}

#[tokio::test(flavor = "multi_thread")]
async fn fs_write_validates_flags_and_paths() {
    let r = rig("write4").await;
    let (st, b) = r.write("f", 0, CREATE | OVERWRITE, b"x", None).await;
    assert_eq!((st, cause(&b).as_str()), (gen::ERR_PROTOCOL, "fs_write_flags_conflict"));
    let (st, _) = r.write("f", 0, AT | gen::FSW_APPEND, b"x", None).await;
    assert_eq!(st, gen::ERR_PROTOCOL);
    let (st, b) = r.write("f", 5, 0, b"x", None).await;
    assert_eq!((st, cause(&b).as_str()), (gen::ERR_PROTOCOL, "fs_write_offset_without_chunk"));
    let (st, b) = r
        .rpc(
            gen::METHOD_FS_WRITE,
            &FsWrite {
                path: "/etc/evil".into(),
                offset: 0,
                flags: 0,
                data: vec![1],
                mode: None,
            },
        )
        .await;
    assert_eq!((st, cause(&b).as_str()), (gen::ERR_PATH, "path_unsafe"));
    let (st, b) = r
        .rpc(
            gen::METHOD_FS_WRITE,
            &FsWrite {
                path: String::new(),
                offset: 0,
                flags: 0,
                data: vec![1],
                mode: None,
            },
        )
        .await;
    assert_eq!((st, cause(&b).as_str()), (gen::ERR_PROTOCOL, "path_required"));
    // a directory in the way: the rename fails and leaves no tmp
    std::fs::create_dir_all(r.path("dir/inner")).unwrap();
    let (st, b) = r.write("dir", 0, 0, b"x", None).await;
    assert_eq!((st, cause(&b).as_str()), (gen::ERR_IO, "rename_failed"));
    assert!(!r.path("dir.ps5upload.tmp").exists());
}

// ---- node, log, net ----

#[tokio::test(flavor = "multi_thread")]
async fn klog_read_marks_more() {
    let r = rig("klog").await;
    let ask = |n: Option<u32>| match n {
        Some(n) => text(&format!("{{\"max_bytes\":{n}}}")),
        None => text("{}"),
    };
    // buffer holds 100 000 bytes: the default ask (16 KiB) is full, so more is set
    mgmt_fs::set(false, 100_000, 0);
    let (st, b) = r.raw(gen::METHOD_LOG_KLOG, &ask(None)).await;
    assert_eq!(st, OK);
    let t = MgmtText::decode(&b).unwrap();
    assert_eq!((t.body.len(), t.more), (16 * 1024, Some(1)));
    // an ask bigger than the cap is held to 64 KiB
    let (_, b) = r.raw(gen::METHOD_LOG_KLOG, &ask(Some(1_000_000))).await;
    let t = MgmtText::decode(&b).unwrap();
    assert_eq!((t.body.len(), t.more), (64 * 1024, Some(1)));
    // the buffer is smaller than the ask: everything, no more
    mgmt_fs::set(false, 500, 0);
    let (_, b) = r.raw(gen::METHOD_LOG_KLOG, &ask(Some(4096))).await;
    let t = MgmtText::decode(&b).unwrap();
    assert_eq!((t.body.len(), t.more), (500, Some(0)));
    // an empty buffer is an empty OK
    mgmt_fs::set(false, 0, 0);
    let (st, b) = r.raw(gen::METHOD_LOG_KLOG, &ask(None)).await;
    assert_eq!(st, OK);
    assert_eq!(MgmtText::decode(&b).unwrap().body.len(), 0);
    // an open failure is an error with its token
    mgmt_fs::set(false, u32::MAX, 0);
    let (st, b) = r.raw(gen::METHOD_LOG_KLOG, &ask(None)).await;
    assert_eq!((st, cause(&b).as_str()), (gen::ERR_INTERNAL, "open_klog_failed"));
}

#[tokio::test(flavor = "multi_thread")]
async fn syslog_returns_the_tail_and_says_older_text_was_cut() {
    let r = rig("syslog").await;
    let expect = |from: usize, to: usize| -> Vec<u8> { (from..to).map(|i| b'a' + (i % 26) as u8).collect() };
    // 1 MiB (the handler's cap) does not fit a reply: the LAST RPC_TEXT_MAX bytes are returned
    let total = 1024 * 1024;
    mgmt_fs::set(false, 0, total as u32);
    let (st, b) = r.raw(gen::METHOD_LOG_SYSLOG, &text("")).await;
    assert_eq!(st, OK);
    let t = MgmtText::decode(&b).unwrap();
    let keep = gen::RPC_TEXT_MAX as usize;
    assert_eq!((t.body.len(), t.more), (keep, Some(1)));
    assert_eq!(t.body, expect(total - keep, total), "the newest text, not the oldest");
    // a smaller buffer comes whole
    mgmt_fs::set(false, 0, 5000);
    let (_, b) = r.raw(gen::METHOD_LOG_SYSLOG, &text("")).await;
    let t = MgmtText::decode(&b).unwrap();
    assert_eq!((t.body, t.more), (expect(0, 5000), Some(0)));
    // max_bytes asks for less: still the tail
    let (_, b) = r
        .raw(gen::METHOD_LOG_SYSLOG, &text("{\"max_bytes\":100}"))
        .await;
    let t = MgmtText::decode(&b).unwrap();
    assert_eq!((t.body, t.more), (expect(4900, 5000), Some(1)));
    // empty and failing
    mgmt_fs::set(false, 0, 0);
    let (st, b) = r.raw(gen::METHOD_LOG_SYSLOG, &text("")).await;
    assert_eq!((st, MgmtText::decode(&b).unwrap().body.len()), (OK, 0));
    mgmt_fs::set(false, 0, u32::MAX);
    let (st, b) = r.raw(gen::METHOD_LOG_SYSLOG, &text("")).await;
    assert_eq!((st, cause(&b).as_str()), (gen::ERR_IO, "syslog_tail_sysctl_errno_5"));
}

#[tokio::test(flavor = "multi_thread")]
async fn failures_that_carry_data_keep_it_in_the_cause() {
    let r = rig("keep").await;
    // net.reach: the caller reads timed_out/errno/ms from the failure
    let (st, b) = r
        .raw(gen::METHOD_NET_REACH, &text(r#"{"host":"unreachable"}"#))
        .await;
    assert_eq!(st, gen::ERR_INTERNAL);
    let v: serde_json::Value = serde_json::from_slice(&b).expect("the cause is the failure body");
    assert_eq!((v["ok"].as_bool(), v["timed_out"].as_bool(), v["ms"].as_u64()), (Some(false), Some(true), Some(3000)));
    let (st, b) = r.raw(gen::METHOD_NET_REACH, &text(r#"{"host":"10.0.0.1"}"#)).await;
    assert_eq!(st, OK);
    assert_eq!(MgmtText::decode(&b).unwrap().body, br#"{"ok":true,"ms":4}"#);
    // a failure with only a token keeps the token
    let (st, b) = r.raw(gen::METHOD_NET_REACH, &text("{}")).await;
    assert_eq!(st, gen::ERR_PROTOCOL);
    assert_eq!(cause(&b), r#"{"ok":false,"err":"bad_request"}"#);
    // a mount's code and mount point
    let (st, b) = r.raw(gen::METHOD_FS_MOUNT_PKG, &text("{}")).await;
    assert_ne!(st, OK);
    let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(v["code"].as_i64(), Some(-2147352567));
    assert_eq!(v["mount_point"], "/mnt/ps5upload/x.pkg.mount");
}

#[tokio::test(flavor = "multi_thread")]
async fn node_and_net_methods_answer_and_map_their_errors() {
    let r = rig("node").await;
    // node.shutdown: an empty reply, the handler ran once
    let (st, b) = r.raw(gen::METHOD_NODE_SHUTDOWN, &[]).await;
    assert_eq!((st, b.len()), (OK, 0));
    assert_eq!(mgmt_fs::stats().1, 1);
    // node.cleanup: text in and out, tokens mapped
    let (st, b) = r
        .raw(gen::METHOD_NODE_CLEANUP, &text(r#"{"path":"/data/x"}"#))
        .await;
    assert_eq!(st, OK);
    assert!(cause(&MgmtText::decode(&b).unwrap().body).contains("removed_files"));
    let (st, b) = r
        .raw(gen::METHOD_NODE_CLEANUP, &text(r#"{"path":"/denied"}"#))
        .await;
    assert_eq!((st, cause(&b).as_str()), (gen::ERR_PATH, "cleanup_path_denied"));
    let (st, b) = r.raw(gen::METHOD_NODE_CLEANUP, &text("{}")).await;
    assert_eq!((st, cause(&b).as_str()), (gen::ERR_PROTOCOL, "cleanup_missing_path"));
    // net.interfaces and net.speedtest are text methods
    let (st, b) = r.raw(gen::METHOD_NET_INTERFACES, &text("")).await;
    assert_eq!(st, OK);
    assert!(cause(&MgmtText::decode(&b).unwrap().body).contains("em0"));
    let (st, b) = r.raw(gen::METHOD_NET_SPEEDTEST, &[]).await;
    assert_eq!(st, OK);
    assert_eq!(MgmtText::decode(&b).unwrap().body, br#"{"ok":true}"#);
}

#[allow(dead_code)]
fn _unused(_: &Path) {}
