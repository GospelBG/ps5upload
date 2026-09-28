//! The desktop shell's sidecar must exit when the shell dies, however it dies. The shell holds
//! the engine's stdin (the parent watch) AND reads its stderr, so a dead shell closes both: the
//! engine then has to shut down without anywhere to print.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[test]
fn the_engine_exits_when_its_parent_dies_and_stderr_is_gone() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ps5upload-engine"))
        .env("PS5UPLOAD_PARENT_WATCH", "1")
        .env("PS5UPLOAD_ENGINE_PORT", free_port().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn engine");
    // Wait until it serves, then take both pipes away, as a dying parent does.
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut line = String::new();
    let started = Instant::now();
    while !line.contains("listening on") {
        line.clear();
        if stderr.read_line(&mut line).unwrap_or(0) == 0
            || started.elapsed() > Duration::from_secs(30)
        {
            let _ = child.kill();
            panic!("engine never started");
        }
    }
    eprintln!("started in {:?}", started.elapsed());
    drop(stderr);
    drop(child.stdin.take());
    let gone = Instant::now();

    // Well inside the 10 s hard-exit fallback: the graceful drain has to do it.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            eprintln!("exited {:?} after its parent", gone.elapsed());
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("the engine outlived its parent by 5 s and would hold its port");
}
