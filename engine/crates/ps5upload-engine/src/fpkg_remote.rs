//! Convert reading a game in place: the converter's opener for `remote://` (a saved server,
//! over the same server file system — read-ahead and retries — uploads use) and `ps5://`
//! (the console, through its FTP server: ftpsrv on 2121, one of the payloads installs need).

use std::path::Path;
use std::sync::Arc;

use ps5upload_core::source_fs::SourceFs;
use ps5upload_fpkg::remote_source::{self, RemoteFiles};
use ps5upload_fpkg::ReadSeek;

use crate::remote::source_fs::RemoteSourceFs;

/// Let `source::open` (build, estimate, inspect, the package viewer) read `remote://` paths.
pub(crate) fn register() {
    remote_source::register(Box::new(open));
}

fn open(url: &str) -> ps5upload_fpkg::Result<(Arc<dyn RemoteFiles>, String)> {
    let fail = |m: String| ps5upload_fpkg::Error::Format(m);
    // Converter work runs on blocking threads, which can wait on the runtime.
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| fail(format!("{url}: no runtime to reach the server from")))?;
    if url.starts_with("ps5://") {
        return open_console(&handle, url);
    }
    let (fs, path) = handle
        .block_on(RemoteSourceFs::for_path(url))
        .map_err(|e| fail(format!("{url}: {e}")))?;
    let label = format!("server {}", connection_of(url));
    Ok((
        Arc::new(ServerFiles { fs, label }),
        path.to_string_lossy().into_owned(),
    ))
}

fn connection_of(url: &str) -> &str {
    let rest = url.strip_prefix("remote://").unwrap_or(url);
    rest.split('/').next().unwrap_or(rest)
}

/// The console's FTP server (ftpsrv).
const CONSOLE_FTP_PORT: u16 = 2121;

/// `ps5://192.168.1.5/mnt/ext0/games/X.exfat` → ("192.168.1.5", "/mnt/ext0/games/X.exfat").
/// An installed game is refused: its files are encrypted, so only a dump converts.
fn console_target(url: &str) -> Result<(String, String), String> {
    let rest = url
        .strip_prefix("ps5://")
        .ok_or_else(|| format!("{url} is not a ps5:// path"))?;
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = host.split(':').next().unwrap_or(host);
    if host.is_empty() {
        return Err(format!("{url} names no console"));
    }
    let path = format!("/{path}");
    let installed = ["/user/app/", "/system_ex/app/", "/user/patch/"];
    if installed
        .iter()
        .any(|p| path.starts_with(p) || path == p.trim_end_matches('/'))
    {
        return Err(format!(
            "{path} is an installed game, and installed games are encrypted; convert a dump (a game folder or image) instead"
        ));
    }
    Ok((host.to_string(), path))
}

fn open_console(
    handle: &tokio::runtime::Handle,
    url: &str,
) -> ps5upload_fpkg::Result<(Arc<dyn RemoteFiles>, String)> {
    use crate::remote::store::{Connection, Protocol, Secret};
    let fail = |m: String| ps5upload_fpkg::Error::Format(m);
    let (host, path) = console_target(url).map_err(fail)?;
    let conn = Connection {
        id: String::new(),
        name: "PS5".into(),
        protocol: Protocol::Ftp,
        host: host.clone(),
        port: CONSOLE_FTP_PORT,
        share: String::new(),
        user: String::new(),
        start_path: String::new(),
        host_key: None,
    };
    let fs = handle
        .block_on(crate::remote::ftp_fs::FtpFs::connect(
            &conn,
            &Secret::None,
            false,
        ))
        .map_err(|e| {
            fail(format!(
                "can't read the console's files at {host}:{CONSOLE_FTP_PORT} ({e}); is ftpsrv running?"
            ))
        })?;
    Ok((
        Arc::new(ConsoleFiles {
            fs: Arc::new(fs),
            handle: handle.clone(),
            label: format!("the PS5 at {host}"),
        }),
        path,
    ))
}

/// The console's files over FTP, read through the same read-ahead as a saved server.
struct ConsoleFiles {
    fs: Arc<dyn crate::remote::RemoteFs>,
    handle: tokio::runtime::Handle,
    label: String,
}

fn remote_io(e: crate::remote::RemoteError) -> std::io::Error {
    let kind = match e {
        crate::remote::RemoteError::NotFound(_) => std::io::ErrorKind::NotFound,
        _ => std::io::ErrorKind::Other,
    };
    std::io::Error::new(kind, e.to_string())
}

impl RemoteFiles for ConsoleFiles {
    fn open(&self, path: &str) -> std::io::Result<Box<dyn ReadSeek>> {
        let file = self
            .handle
            .block_on(self.fs.open(path))
            .map_err(remote_io)?;
        Ok(Box::new(crate::remote::source_fs::read_ahead(
            self.handle.clone(),
            file,
        )))
    }

    fn stat(&self, path: &str) -> std::io::Result<(u64, bool)> {
        let e = self
            .handle
            .block_on(self.fs.stat(path))
            .map_err(remote_io)?;
        Ok((e.size, e.is_dir))
    }

    fn list(&self, dir: &str) -> std::io::Result<Vec<(String, bool, u64)>> {
        let mut out = Vec::new();
        let mut cursor = None;
        loop {
            let page = self
                .handle
                .block_on(self.fs.list(dir, cursor))
                .map_err(remote_io)?;
            out.extend(page.entries.into_iter().map(|e| (e.name, e.is_dir, e.size)));
            match page.next_cursor {
                Some(c) => cursor = Some(c),
                None => return Ok(out),
            }
        }
    }

    fn label(&self) -> String {
        self.label.clone()
    }
}

struct ServerFiles {
    fs: Arc<RemoteSourceFs>,
    label: String,
}

impl RemoteFiles for ServerFiles {
    fn open(&self, path: &str) -> std::io::Result<Box<dyn ReadSeek>> {
        let r = self.fs.open(Path::new(path))?;
        Ok(Box::new(r))
    }

    fn stat(&self, path: &str) -> std::io::Result<(u64, bool)> {
        let m = self.fs.metadata(Path::new(path))?;
        Ok((m.len, m.is_dir))
    }

    fn list(&self, dir: &str) -> std::io::Result<Vec<(String, bool, u64)>> {
        self.fs
            .read_dir(Path::new(dir))?
            .into_iter()
            .map(|(child, is_dir)| {
                // The listing already said the size; this answers from its cache.
                let size = if is_dir {
                    0
                } else {
                    self.fs.metadata(&child)?.len
                };
                let name = child
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                Ok((name, is_dir, size))
            })
            .collect()
    }

    fn label(&self) -> String {
        self.label.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A game image on a saved server converts without a copy here: the converter reads it
    /// through the server file system.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_game_image_on_a_server_opens_and_builds_in_place() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../ps5upload-fpkg/tests/fixtures/mini.exfat");
        let bytes = std::fs::read(&fixture).unwrap();
        let r = crate::remote::pool::testing::install_global(&[
            ("/fpkg-remote/mini.exfat", &bytes),
            ("/fpkg-remote/game/sce_sys/param.json", b"{}"),
        ]);
        let id = r
            .store
            .add(
                crate::remote::store::conn("NAS", crate::remote::store::Protocol::Smb),
                crate::remote::store::Secret::None,
            )
            .unwrap()
            .conn
            .id;
        register();
        let url = format!("remote://{id}/fpkg-remote/mini.exfat");
        let out = std::env::temp_dir().join(format!("fpkg-remote-build-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        let (u, o) = (url.clone(), out.clone());
        let report = tokio::task::spawn_blocking(move || {
            let local = ps5upload_fpkg::source::open(&fixture).unwrap();
            let remote = ps5upload_fpkg::source::open(Path::new(&u)).unwrap();
            assert_eq!(local.files(), remote.files());
            assert!(
                remote.describe().contains("server"),
                "{}",
                remote.describe()
            );
            let request = ps5upload_fpkg::build::BuildRequest {
                kraken: false,
                ..ps5upload_fpkg::build::BuildRequest::new(Path::new(&u), &o)
            };
            ps5upload_fpkg::build::build(&request, &mut |_| {})
        })
        .await
        .unwrap()
        .unwrap();
        assert!(report.verify.ok(), "{}", report.verify);
        let folder = format!("remote://{id}/fpkg-remote/game");
        let tree = tokio::task::spawn_blocking(move || {
            ps5upload_fpkg::source::open(Path::new(&folder)).map(|t| t.files().to_vec())
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].path, "sce_sys/param.json");
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn a_console_path_names_the_host_and_refuses_installed_games() {
        assert_eq!(
            console_target("ps5://192.168.86.99/mnt/ext0/g/X.exfat").unwrap(),
            (
                "192.168.86.99".to_string(),
                "/mnt/ext0/g/X.exfat".to_string()
            )
        );
        assert_eq!(
            console_target("ps5://10.0.0.2:9113/data/homebrew/G")
                .unwrap()
                .0,
            "10.0.0.2"
        );
        for p in [
            "ps5://h/user/app/PPSA01234",
            "ps5://h/user/app",
            "ps5://h/system_ex/app/X",
        ] {
            let e = console_target(p).unwrap_err();
            assert!(e.contains("encrypted"), "{p}: {e}");
        }
        assert!(console_target("ps5:///x").is_err());
    }

    #[test]
    fn a_remote_path_is_not_made_relative_to_the_engine() {
        assert_eq!(
            crate::fpkg_api::resolve_engine_path("ps5://h/data/x"),
            std::path::PathBuf::from("ps5://h/data/x")
        );
        assert_eq!(
            crate::fpkg_api::resolve_engine_path("remote://abc/games/x"),
            std::path::PathBuf::from("remote://abc/games/x")
        );
    }

    /// Throughput of the console reader Convert uses (ftpsrv + read-ahead), on a real file.
    /// Run by hand: PS5UPLOAD_SPEED_HOST=192.168.86.99 PS5UPLOAD_SPEED_FILE=/user/app/X/app.pkg
    /// cargo test -p ps5upload-engine --release --lib console_read_speed -- --ignored --nocapture
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn console_read_speed() {
        use std::io::Read;
        let host = std::env::var("PS5UPLOAD_SPEED_HOST").expect("PS5UPLOAD_SPEED_HOST");
        let path = std::env::var("PS5UPLOAD_SPEED_FILE").expect("PS5UPLOAD_SPEED_FILE");
        let limit: u64 = std::env::var("PS5UPLOAD_SPEED_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2 << 30);
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            use crate::remote::store::{Connection, Protocol, Secret};
            let conn = Connection {
                id: String::new(),
                name: "PS5".into(),
                protocol: Protocol::Ftp,
                host,
                port: CONSOLE_FTP_PORT,
                share: String::new(),
                user: String::new(),
                start_path: String::new(),
                host_key: None,
            };
            let fs = handle
                .block_on(crate::remote::ftp_fs::FtpFs::connect(
                    &conn,
                    &Secret::None,
                    false,
                ))
                .unwrap();
            let files = ConsoleFiles {
                fs: Arc::new(fs),
                handle: handle.clone(),
                label: "speed".into(),
            };
            let mut r = files.open(&path).unwrap();
            // The builder asks for block-sized ranges, front to back.
            let mut buf = vec![0u8; 2 << 20];
            let started = std::time::Instant::now();
            let mut done = 0u64;
            while done < limit {
                let n = r.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                done += n as u64;
            }
            let secs = started.elapsed().as_secs_f64();
            println!(
                "console read: {:.2} GiB in {:.1} s = {:.1} MB/s",
                done as f64 / (1u64 << 30) as f64,
                secs,
                done as f64 / 1e6 / secs
            );
        })
        .await
        .unwrap();
    }
}
