//! A restore from a jammed machine onto a checkpoint taken before the JAM runs again:
//! `session/run` reports no jam. A real JAM after the restore still reports its own PC,
//! and so does one after `snapshot/undump`. The steps of GitHub issue #12, on the
//! reporter's maze test program, through the real binary on a free port.

use futures_util::{SinkExt, StreamExt};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// maze.prg (64tass, from the issue): four rooms in $02, joystick 2 down in room 2
/// executes a JAM at $085F; the main loop waits for the raster at $0816..$0820.
const MAZE_PRG: &[u8] = &[
      0x01, 0x08, 0x0b, 0x08, 0x0a, 0x00, 0x9e, 0x32, 0x30, 0x36, 0x31, 0x00,
      0x00, 0x00, 0x78, 0xa9, 0x00, 0x85, 0x02, 0x85, 0x03, 0x85, 0x04, 0xad,
      0x12, 0xd0, 0xc9, 0xfa, 0xd0, 0xf9, 0xad, 0x12, 0xd0, 0xc9, 0xfa, 0xf0,
      0xf9, 0xad, 0x00, 0xdc, 0x49, 0xff, 0x29, 0x1f, 0xaa, 0xa5, 0x04, 0x49,
      0xff, 0x86, 0x04, 0x85, 0x05, 0x8a, 0x25, 0x05, 0x85, 0x05, 0xa5, 0x05,
      0x29, 0x04, 0xf0, 0x06, 0xa5, 0x02, 0xf0, 0x02, 0xc6, 0x02, 0xa5, 0x05,
      0x29, 0x08, 0xf0, 0x08, 0xa5, 0x02, 0xc9, 0x03, 0xf0, 0x02, 0xe6, 0x02,
      0xa5, 0x05, 0x29, 0x02, 0xf0, 0x07, 0xa5, 0x02, 0xc9, 0x02, 0xd0, 0x01,
      0x02, 0xa5, 0x05, 0x29, 0x10, 0xf0, 0x0a, 0xa5, 0x02, 0xc9, 0x03, 0xd0,
      0x04, 0xa9, 0x01, 0x85, 0x03, 0xa5, 0x02, 0x09, 0x30, 0x8d, 0x00, 0x04,
      0xa5, 0x03, 0x09, 0x30, 0x8d, 0x01, 0x04, 0x4c, 0x16, 0x08
];
const JAM_PC: u64 = 0x085f;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

struct Daemon(Child, PathBuf);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

fn start(port: u16) -> Daemon {
    let dir = std::env::temp_dir().join(format!("trx64_jam_restore_{port}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_trx64-daemon"))
        .args(["--port", &port.to_string()])
        .current_dir(&dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn trx64-daemon");
    Daemon(child, dir)
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
                assert!(v["error"].is_null(), "{method}: {v}");
                return v["result"].clone();
            }
        }
    }
    panic!("no reply to {method}");
}

/// Room 2, stick down, run: the planted JAM.
async fn jam_in_room_2(ws: &mut Ws) -> serde_json::Value {
    rpc(ws, "monitor/exec", serde_json::json!({ "command": "wr ram 0002 02" })).await;
    rpc(ws, "session/joystick_set", serde_json::json!({ "port": 2, "down": true })).await;
    let r = rpc(ws, "session/run", serde_json::json!({ "cycles": 100_000 })).await;
    rpc(ws, "session/joystick_clear", serde_json::json!({ "port": 2 })).await;
    r
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restore_from_a_jammed_machine_runs_again() {
    let port = free_port();
    let d = start(port);
    let mut ws = connect(port).await;
    let prg = d.1.join("maze.prg");
    std::fs::write(&prg, MAZE_PRG).unwrap();
    let anchor = d.1.join("anchor.c64re");

    rpc(&mut ws, "session/create", serde_json::json!({ "model": "c64-pal" })).await;
    rpc(&mut ws, "session/run", serde_json::json!({ "cycles": 3_000_000 })).await;
    rpc(&mut ws, "session/load_prg", serde_json::json!({ "prg_path": prg.to_string_lossy(), "run": "080d" })).await;
    rpc(&mut ws, "debug/pause", serde_json::json!({})).await;
    rpc(&mut ws, "debug/break_add", serde_json::json!({ "pc": 0x0816 })).await;
    rpc(&mut ws, "session/run", serde_json::json!({ "cycles": 2_000_000 })).await;
    rpc(&mut ws, "debug/break_del", serde_json::json!({})).await;
    rpc(&mut ws, "snapshot/dump", serde_json::json!({ "path": anchor.to_string_lossy() })).await;
    rpc(&mut ws, "snapshot/undump", serde_json::json!({ "path": anchor.to_string_lossy() })).await;
    let cp = rpc(&mut ws, "checkpoint/capture", serde_json::json!({})).await;
    let cp_id = cp["ref"]["id"].as_str().expect("checkpoint id").to_string();

    let r = jam_in_room_2(&mut ws).await;
    assert_eq!(r["jam"]["pc"], JAM_PC, "the planted JAM: {r}");

    // 1. Restored to the main loop: it runs, no jam.
    rpc(&mut ws, "checkpoint/restore", serde_json::json!({ "id": cp_id, "then": "pause" })).await;
    let r = rpc(&mut ws, "session/run", serde_json::json!({ "cycles": 100_000 })).await;
    assert!(r["jam"].is_null(), "restored before the JAM, still reported jammed: {r}");

    // 2. A real JAM after the restore reports its own PC.
    rpc(&mut ws, "checkpoint/restore", serde_json::json!({ "id": cp_id, "then": "pause" })).await;
    let r = jam_in_room_2(&mut ws).await;
    assert_eq!(r["jam"]["pc"], JAM_PC, "{r}");

    // 3. The same through snapshot/undump from the jammed machine.
    rpc(&mut ws, "snapshot/undump", serde_json::json!({ "path": anchor.to_string_lossy() })).await;
    let r = rpc(&mut ws, "session/run", serde_json::json!({ "cycles": 100_000 })).await;
    assert!(r["jam"].is_null(), "undumped before the JAM, still reported jammed: {r}");
    let r = jam_in_room_2(&mut ws).await;
    assert_eq!(r["jam"]["pc"], JAM_PC, "{r}");
}
