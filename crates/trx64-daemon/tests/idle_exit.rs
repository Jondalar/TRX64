//! Spec 887 — `--idle-exit`: the real binary, on a free port, ends itself after the
//! window with nobody there; an A/V subscriber, a recording trace or a keep-alive holds
//! it, and a plain RPC connection does not — only what it sends counts. The pure
//! clock rules (reset, keep-alive, null, holds) are unit-tested in `src/idle.rs`.

use futures_util::{SinkExt, StreamExt};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn start(port: u16, idle: u64) -> Child {
    start_with(port, idle, true)
}

fn start_with(port: u16, idle: u64, headless: bool) -> Child {
    let dir = std::env::temp_dir().join(format!("trx64_887_{port}"));
    let _ = std::fs::create_dir_all(&dir);
    let mut args = vec!["--port".to_string(), port.to_string(), "--idle-exit".to_string(), idle.to_string()];
    if headless {
        args.push("--headless".into());
    }
    Command::new(env!("CARGO_BIN_EXE_trx64-daemon"))
        .args(&args)
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
    connect_url(format!("ws://127.0.0.1:{port}")).await
}

async fn connect_url(url: String) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        match tokio_tungstenite::connect_async(url.clone()).await {
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
async fn a_silent_rpc_connection_does_not_hold_it_and_keep_alive_does() {
    let port = free_port();
    let mut child = start(port, 2);
    let mut ws = connect(port).await;
    let ping = rpc(&mut ws, "ping", serde_json::json!({})).await;
    assert_eq!(ping["idleExit"]["armedSeconds"], 2, "{ping}");
    assert!(ping["idleExit"]["deadlineMs"].as_u64().is_some(), "a plain connection holds nothing: {ping}");

    let k = rpc(&mut ws, "daemon/keep_alive", serde_json::json!({ "seconds": 6 })).await;
    assert_eq!(k["armed"], true, "{k}");
    assert!(k["keptAliveUntilMs"].as_u64().is_some(), "{k}");
    let n = rpc(&mut ws, "daemon/keep_alive", serde_json::json!({ "seconds": null })).await;
    assert_eq!(n["keptForever"], true, "{n}");
    assert!(n["deadlineMs"].is_null(), "{n}");
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(child.try_wait().unwrap().is_none(), "null holds it past the window");

    // Back to a number and then silence, with the socket still open: the keep-alive
    // (6 s) outlasts the 2 s window, and then the daemon ends under the open socket.
    rpc(&mut ws, "daemon/keep_alive", serde_json::json!({ "seconds": 6 })).await;
    let t = Instant::now();
    let code = wait_exit(&mut child, Duration::from_secs(30));
    assert_eq!(code, Some(0));
    assert!(t.elapsed() >= Duration::from_secs(5), "the keep-alive held it ({:?})", t.elapsed());
    drop(ws);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_av_subscriber_holds_it_an_av0_connection_does_not() {
    let port = free_port();
    let mut child = start_with(port, 2, false);
    let mut viewer = connect(port).await;
    let mut rpc_only = connect_url(format!("ws://127.0.0.1:{port}/?av=0")).await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(child.try_wait().unwrap().is_none(), "an A/V subscriber holds the daemon");
    let st = rpc(&mut rpc_only, "session/state", serde_json::json!({})).await;
    assert_eq!(st["idleExit"]["holding"], "subscriber", "{}", st["idleExit"]);
    let _ = viewer.close(None).await;
    drop(viewer);
    // Only the ?av=0 socket is left, and it says nothing: the daemon ends under it.
    assert_eq!(wait_exit(&mut child, Duration::from_secs(30)), Some(0), "a silent RPC-only socket holds nothing");
    drop(rpc_only);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_recording_trace_holds_it() {
    let port = free_port();
    let mut child = start(port, 2);
    let out = std::env::temp_dir().join(format!("trx64_887_{port}")).join("t.duckdb");
    let mut ws = connect(port).await;
    rpc(&mut ws, "trace/start_domains", serde_json::json!({ "domains": ["c64-cpu"], "output": out.to_string_lossy() })).await;
    drop(ws);
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(child.try_wait().unwrap().is_none(), "a recording trace holds the daemon");
    let mut ws = connect(port).await;
    rpc(&mut ws, "trace/run/stop", serde_json::json!({})).await;
    drop(ws);
    assert_eq!(wait_exit(&mut child, Duration::from_secs(30)), Some(0), "with the trace stopped and nobody there it ends");
}
