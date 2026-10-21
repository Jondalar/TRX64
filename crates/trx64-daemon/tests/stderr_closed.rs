//! A daemon whose stderr reader went away keeps running. Its log lines then fail with
//! EPIPE; `eprintln!` panicked on that, the panic hook logged the same way and the
//! process aborted (SIGABRT), taking any unflushed trace with it. The real binary, its
//! stderr a pipe whose read end is closed before the first log line.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

#[test]
fn a_closed_stderr_does_not_take_the_daemon_down() {
    let port = free_port();
    let dir = std::env::temp_dir().join(format!("trx64_epipe_{port}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_trx64-daemon"))
        .args(["--port", &port.to_string()])
        .current_dir(&dir)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn trx64-daemon");
    // The reader goes away before the daemon has written anything.
    drop(child.stderr.take());

    // Listening means it got through every startup log line.
    let until = Instant::now() + Duration::from_secs(20);
    let mut up = false;
    while Instant::now() < until {
        if let Ok(Some(st)) = child.try_wait() {
            let _ = std::fs::remove_dir_all(&dir);
            panic!("daemon ended while starting: {st}");
        }
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            up = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // Each connection that fails the handshake logs two lines.
    for _ in 0..3 {
        let _ = std::net::TcpStream::connect(("127.0.0.1", port));
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(500));
    let alive = child.try_wait().unwrap().is_none();
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(up, "daemon never listened");
    assert!(alive, "daemon ended after logging to a closed stderr");
}
