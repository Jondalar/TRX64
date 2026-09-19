//! Spec 863 — the model is chosen before anything runs, on the daemon's command line.
//!
//! `--model` names a `models.toml` row: a row this build can run starts the daemon on it,
//! one that needs a building block this build lacks is refused at startup with the block's
//! name, and an unknown one with the rows it could have been. `--video pal|ntsc` is the
//! shorthand for the two base rows.
//!
//! These start the real binary on a free port (never the shared daemon's) and read what it
//! says on stderr; a daemon that starts is killed as soon as it is listening.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Start the daemon with `args`; return (exited?, exit success, the stderr lines seen until
/// it exited or said it was listening).
fn start(args: &[&str]) -> (bool, bool, Vec<String>) {
    let port = free_port().to_string();
    let mut child = Command::new(env!("CARGO_BIN_EXE_trx64-daemon"))
        .args(["--headless", "--port", &port])
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn trx64-daemon");
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut lines = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            while let Ok(l) = rx.recv_timeout(Duration::from_millis(200)) {
                lines.push(l);
            }
            return (true, status.success(), lines);
        }
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(l) => {
                let listening = l.contains("listening on ws://");
                lines.push(l);
                if listening {
                    let _ = child.kill();
                    let _ = child.wait();
                    return (false, true, lines);
                }
            }
            Err(_) if Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("the daemon neither exited nor listened: {lines:?}");
            }
            Err(_) => {}
        }
    }
}

/// Acceptance 8 — `--model c64c-pal` is refused at startup, naming the missing block.
#[test]
fn a_row_that_cannot_run_is_refused_at_startup_by_name() {
    let (exited, ok, lines) = start(&["--model", "c64c-pal"]);
    assert!(exited && !ok, "refused: {lines:?}");
    let text = lines.join("\n");
    assert!(text.contains("6526A"), "names the block: {text}");
    assert!(!text.contains("listening"), "never listened: {text}");

    let (exited, ok, lines) = start(&["--model", "c64-secam"]);
    assert!(exited && !ok);
    assert!(lines.join("\n").contains("unknown model"), "{lines:?}");
}

/// Acceptance 8 — `c64-paln` runs 65 × 312 at 1 023 440 Hz; `--video ntsc` is `c64-ntsc`.
#[test]
fn a_row_that_runs_starts_the_daemon_on_it() {
    let (exited, _, lines) = start(&["--model", "c64-paln"]);
    let text = lines.join("\n");
    assert!(!exited, "it runs: {text}");
    assert!(text.contains("model = c64-paln") && text.contains("65 × 312 at 1023440 Hz"), "{text}");

    let (exited, _, lines) = start(&["--video", "ntsc"]);
    let text = lines.join("\n");
    assert!(!exited, "{text}");
    assert!(text.contains("model = c64-ntsc") && text.contains("6567R8"), "{text}");

    // The default says nothing about a model: it is the machine it always was.
    let (exited, _, lines) = start(&[]);
    assert!(!exited);
    assert!(!lines.join("\n").contains("model ="), "{lines:?}");
}
