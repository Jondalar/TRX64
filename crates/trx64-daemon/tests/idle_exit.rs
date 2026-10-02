//! Spec 887 — `--idle-exit`: the real binary, on a free port, ends itself after the
//! window with nobody there, and a connected client or a keep-alive holds it. The pure
//! clock rules (reset, keep-alive, null, holds) are unit-tested in `src/idle.rs`.

use futures_util::{SinkExt, StreamExt};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn start(port: u16, idle: u64) -> Child {
    let dir = std::env::temp_dir().join(format!("trx64_887_{port}"));
    let _ = std::fs::create_dir_all(&dir);
    Command::new(env!("CARGO_BIN_EXE_trx64-daemon"))
        .args(["--headless", "--port", &port.to_string(), "--idle-exit", &idle.to_string()])
        .current_dir(&dir)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn trx64-daemon")
}

/// Wait up to `limit` for the child to exit; Some(exit code) if it did.
fn wait_exit(child: &mut Child, limit: Duration) -> Option<i32> {
    let until = Instant::now() + limit;
    while Instant::now() < until {
        if let Ok(Some(st)) = child.try_wait() {
            return Some(st.code().unwrap_or(-1));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

async fn connect(port: u16) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        match tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}")).await {
            Ok((ws, _)) => return ws,
            Err(_) if Instant::now() < until => tokio::time::sleep(Duration::from_millis(100)).await,
            Err(e) => panic!("connect: {e}"),
        }
    }
}

async fn rpc(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let req = serde_json::json!({ "jsonrpc": "2.0", "id": 7, "method": method, "params": params });
    ws.send(Message::Text(req.to_string())).await.unwrap();
    while let Some(Ok(msg)) = ws.next().await {
        if let Message::Text(t) = msg {
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            if v["id"] == 7 {
                return v["result"].clone();
            }
        }
    }
    panic!("no reply to {method}");
}

#[test]
fn nobody_there_for_the_window_ends_it_with_exit_0() {
    let port = free_port();
    let mut child = start(port, 2);
    let code = wait_exit(&mut child, Duration::from_secs(30));
    let mut err = String::new();
    use std::io::Read;
    child.stderr.take().unwrap().read_to_string(&mut err).ok();
    assert_eq!(code, Some(0), "stderr:\n{err}");
    assert!(err.contains("[trx64] idle for 2 s — exiting"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connected_client_holds_it_and_keep_alive_reports() {
    let port = free_port();
    let mut child = start(port, 2);
    let mut ws = connect(port).await;
    let ping = rpc(&mut ws, "ping", serde_json::json!({})).await;
    assert_eq!(ping["idleExit"]["armedSeconds"], 2, "{ping}");
    // Connected: held well past the window.
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(child.try_wait().unwrap().is_none(), "a connected client holds the daemon");
    let st = rpc(&mut ws, "session/state", serde_json::json!({})).await;
    assert_eq!(st["idleExit"]["holding"], "client", "{}", st["idleExit"]);
    assert!(st["idleExit"]["deadlineMs"].is_null());

    let k = rpc(&mut ws, "daemon/keep_alive", serde_json::json!({ "seconds": 6 })).await;
    assert_eq!(k["armed"], true, "{k}");
    assert!(k["keptAliveUntilMs"].as_u64().is_some(), "{k}");
    let n = rpc(&mut ws, "daemon/keep_alive", serde_json::json!({ "seconds": null })).await;
    assert_eq!(n["keptForever"], true, "{n}");
    // Back to a number, then leave: the keep-alive (6 s) outlasts the 2 s window.
    rpc(&mut ws, "daemon/keep_alive", serde_json::json!({ "seconds": 6 })).await;
    drop(ws);
    let t = Instant::now();
    let code = wait_exit(&mut child, Duration::from_secs(30));
    assert_eq!(code, Some(0));
    assert!(t.elapsed() >= Duration::from_secs(5), "the keep-alive held it ({:?})", t.elapsed());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_recording_trace_holds_it_with_nobody_connected() {
    let port = free_port();
    let mut child = start(port, 2);
    let out = std::env::temp_dir().join(format!("trx64_887_{port}")).join("t.duckdb");
    let mut ws = connect(port).await;
    rpc(&mut ws, "trace/start_domains", serde_json::json!({ "domains": ["c64-cpu"], "output": out.to_string_lossy() })).await;
    drop(ws);
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(child.try_wait().unwrap().is_none(), "a recording trace holds the daemon with no client");
    let mut ws = connect(port).await;
    rpc(&mut ws, "trace/run/stop", serde_json::json!({})).await;
    drop(ws);
    assert_eq!(wait_exit(&mut child, Duration::from_secs(30)), Some(0), "with the trace stopped and nobody there it ends");
}
