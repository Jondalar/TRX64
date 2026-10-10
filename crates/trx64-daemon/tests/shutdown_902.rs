//! Spec 902 (TRX64 half) — `daemon/shutdown`, SIGTERM and SIGINT run the one exit work
//! (stop the run, end a recording trace, persist the media) and end the daemon with exit
//! 0; the shutdown reply is on the caller's socket before the process goes. The real
//! binary, on a free port, in a temp working directory.

use futures_util::{SinkExt, StreamExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// A temp working directory that removes itself, also when an assertion fails.
struct Dir(PathBuf);
impl Dir {
    fn new(tag: &str, port: u16) -> Dir {
        let p = std::env::temp_dir().join(format!("trx64_902_{tag}_{port}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Dir(p)
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Kills the child if the test panics before it ended on its own.
struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(port: u16, dir: &Path) -> Daemon {
    Daemon(
        Command::new(env!("CARGO_BIN_EXE_trx64-daemon"))
            .args(["--port", &port.to_string(), "--headless"])
            .current_dir(dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn trx64-daemon"),
    )
}

fn wait_exit(d: &mut Daemon, limit: Duration) -> Option<i32> {
    let until = Instant::now() + limit;
    while Instant::now() < until {
        if let Ok(Some(st)) = d.0.try_wait() {
            return Some(st.code().unwrap_or(-1));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

async fn connect(port: u16) -> Ws {
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        match tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/?av=0")).await {
            Ok((ws, _)) => return ws,
            Err(_) if Instant::now() < until => tokio::time::sleep(Duration::from_millis(100)).await,
            Err(e) => panic!("connect: {e}"),
        }
    }
}

async fn rpc(ws: &mut Ws, method: &str, params: serde_json::Value) -> serde_json::Value {
    let req = serde_json::json!({ "jsonrpc": "2.0", "id": 7, "method": method, "params": params });
    ws.send(Message::Text(req.to_string())).await.unwrap();
    while let Some(Ok(msg)) = ws.next().await {
        if let Message::Text(t) = msg {
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            if v["id"] == 7 {
                return if v["error"].is_null() { v["result"].clone() } else { v["error"].clone() };
            }
        }
    }
    panic!("no reply to {method}");
}

/// A 32 KiB-free EasyFlash CRT: one 16 KiB bank of erased flash with a reset vector.
fn ef_crt() -> Vec<u8> {
    let mut bank0 = vec![0xffu8; 0x4000];
    bank0[0x3ffc] = 0x00;
    bank0[0x3ffd] = 0x80;
    let mut v = Vec::new();
    v.extend_from_slice(b"C64 CARTRIDGE   ");
    v.extend_from_slice(&0x40u32.to_be_bytes());
    v.extend_from_slice(&0x0100u16.to_be_bytes());
    v.extend_from_slice(&32u16.to_be_bytes());
    v.push(1);
    v.push(0);
    v.extend_from_slice(&[0u8; 6]);
    let mut nm = [0u8; 32];
    nm[..2].copy_from_slice(b"EF");
    v.extend_from_slice(&nm);
    v.extend_from_slice(b"CHIP");
    v.extend_from_slice(&(0x10u32 + bank0.len() as u32).to_be_bytes());
    v.extend_from_slice(&0u16.to_be_bytes());
    v.extend_from_slice(&0u16.to_be_bytes());
    v.extend_from_slice(&0x8000u16.to_be_bytes());
    v.extend_from_slice(&(bank0.len() as u16).to_be_bytes());
    v.extend_from_slice(&bank0);
    v
}

/// Where the byte programmed by [`dirty_cart`] lands in the re-packed CRT:
/// file header + CHIP header + the offset inside ROML.
const PROGRAMMED_AT: usize = 0x40 + 0x10 + 0x100;

/// Mount a writable cartridge and program one flash byte through the machine's own bus
/// (the AM29F040B byte-program sequence), so the cartridge is dirty and unsaved.
async fn dirty_cart(ws: &mut Ws, crt: &Path) {
    let m = rpc(ws, "media/mount", serde_json::json!({ "path": crt.to_string_lossy() })).await;
    assert!(m["error"].is_null() && m.get("code").is_none(), "mount: {m}");
    for cmd in ["wr cpu 8555 aa", "wr cpu 82aa 55", "wr cpu 8555 a0", "wr cpu 8100 42"] {
        let r = rpc(ws, "monitor/exec", serde_json::json!({ "command": cmd })).await;
        assert!(r["error"].is_null(), "{cmd}: {r}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_replies_then_exits_0_and_persists_a_dirty_cartridge() {
    let port = free_port();
    let dir = Dir::new("req", port);
    let crt = dir.0.join("save.crt");
    let orig = ef_crt();
    std::fs::write(&crt, &orig).unwrap();
    let mut d = start(port, &dir.0);
    let mut ws = connect(port).await;
    dirty_cart(&mut ws, &crt).await;
    assert_eq!(std::fs::read(&crt).unwrap(), orig, "the write is still unsaved before the shutdown");

    let t = Instant::now();
    let r = rpc(&mut ws, "daemon/shutdown", serde_json::json!({ "ignored": true })).await;
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["exitCode"], 0, "{r}");
    assert_eq!(r["persisted"]["cartridge"], crt.to_string_lossy().as_ref(), "{r}");
    assert_eq!(r["persisted"]["disks"], serde_json::json!([]), "{r}");
    assert_eq!(wait_exit(&mut d, Duration::from_secs(2)), Some(0), "exit 0 within 2 s");
    assert!(t.elapsed() < Duration::from_secs(3));
    // The caller sees the reply, THEN the connection close.
    let end = tokio::time::timeout(Duration::from_secs(2), ws.next()).await;
    assert!(matches!(end, Ok(None) | Ok(Some(Ok(Message::Close(_)))) | Ok(Some(Err(_)))), "{end:?}");

    assert_eq!(std::fs::read(&crt).unwrap()[PROGRAMMED_AT], 0x42, "the dirty flash reached the file");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bare_shutdown_with_nothing_mounted_replies_the_shape_and_exits_0() {
    let port = free_port();
    let dir = Dir::new("bare", port);
    let mut d = start(port, &dir.0);
    let mut ws = connect(port).await;
    let r = rpc(&mut ws, "daemon/shutdown", serde_json::json!({})).await;
    assert_eq!(
        r,
        serde_json::json!({ "ok": true, "persisted": { "cartridge": null, "disks": [] }, "trace": null, "exitCode": 0 })
    );
    assert_eq!(wait_exit(&mut d, Duration::from_secs(2)), Some(0));
}

#[cfg(unix)]
async fn signal_case(sig: &str, tag: &str) {
    let port = free_port();
    let dir = Dir::new(tag, port);
    let crt = dir.0.join("save.crt");
    let orig = ef_crt();
    std::fs::write(&crt, &orig).unwrap();
    let mut d = start(port, &dir.0);
    let mut ws = connect(port).await;
    dirty_cart(&mut ws, &crt).await;
    assert_eq!(std::fs::read(&crt).unwrap(), orig);

    let pid = d.0.id().to_string();
    let st = Command::new("kill").args([&format!("-{sig}"), &pid]).status().unwrap();
    assert!(st.success());
    assert_eq!(wait_exit(&mut d, Duration::from_secs(5)), Some(0), "{sig}: exit 0");
    assert_eq!(std::fs::read(&crt).unwrap()[PROGRAMMED_AT], 0x42, "{sig}: the write survived");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn sigterm_persists_the_dirty_cartridge_and_exits_0() {
    signal_case("TERM", "term").await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn sigint_persists_the_dirty_cartridge_and_exits_0() {
    signal_case("INT", "int").await;
}

/// A recording trace at shutdown is ended the way `trace/run/stop` ends it: the
/// `.c64retrace` is complete on disk and the index builds from it like from a stopped one.
#[tokio::test(flavor = "multi_thread")]
async fn a_recording_trace_is_complete_on_disk_after_shutdown() {
    let port = free_port();
    let dir = Dir::new("trace", port);
    let mut d = start(port, &dir.0);
    let mut ws = connect(port).await;

    // The control: the same capture ended by request.
    let ctl_out = dir.0.join("ctl.duckdb");
    let s = rpc(&mut ws, "trace/start_domains", serde_json::json!({ "domains": ["c64-cpu"], "output": ctl_out.to_string_lossy() })).await;
    assert!(s.get("code").is_none(), "{s}");
    rpc(&mut ws, "session/run", serde_json::json!({ "cycles": 20000 })).await;
    let stop = rpc(&mut ws, "trace/run/stop", serde_json::json!({})).await;
    assert!(stop["run"].is_object(), "{stop}");
    let ctl_len = std::fs::metadata(dir.0.join("ctl.c64retrace")).unwrap().len();

    let out = dir.0.join("t.duckdb");
    let s = rpc(&mut ws, "trace/start_domains", serde_json::json!({ "domains": ["c64-cpu"], "output": out.to_string_lossy() })).await;
    assert!(s.get("code").is_none(), "{s}");
    rpc(&mut ws, "session/run", serde_json::json!({ "cycles": 20000 })).await;

    let r = rpc(&mut ws, "daemon/shutdown", serde_json::json!({})).await;
    assert_eq!(r["ok"], true, "{r}");
    assert!(r["trace"].is_string(), "the reply names the finished trace: {r}");
    assert_eq!(wait_exit(&mut d, Duration::from_secs(2)), Some(0));

    let retrace = dir.0.join("t.c64retrace");
    let len = std::fs::metadata(&retrace).expect("the .c64retrace exists").len();
    assert!(len > 0 && len > ctl_len / 2, "complete, like the stopped control ({len} vs {ctl_len})");
    let idx = trx64_traceindex::op_index_with(&out, None, true, None).expect("the shutdown trace indexes");
    let ctl = trx64_traceindex::op_index_with(&ctl_out, None, true, None).expect("the control indexes");
    assert_eq!(
        idx.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()),
        ctl.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()),
        "same index reply shape as a trace stopped by request"
    );
}
