//! Several C64RE servers warm-start a daemon on the same port at once. The losers must do
//! no machine work and leave cleanly — exit 0 with one line — so every client attaches to
//! the winner. It used to boot a whole machine and then panic on the bind (exit 101).

use std::process::{Command, Stdio};

#[test]
fn a_port_already_owned_is_a_clean_exit_before_any_boot() {
    // Something else owns the port (here: this test).
    let owner = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = owner.local_addr().unwrap().port().to_string();
    let out = Command::new(env!("CARGO_BIN_EXE_trx64-daemon"))
        .args(["--headless", "--port", &port])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("run trx64-daemon");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "exit {:?}, stderr:\n{stderr}", out.status.code());
    assert!(stderr.contains("already owned by another runtime — exiting cleanly"), "{stderr}");
    assert!(!stderr.contains("PANIC"), "{stderr}");
    assert!(!stderr.contains("loading ROMs") && !stderr.contains("boot ok"), "the loser did machine work:\n{stderr}");
    drop(owner);
}
