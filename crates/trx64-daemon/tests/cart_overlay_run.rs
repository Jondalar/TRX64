//! A banked cart overlay through `runtime/overlay_run` on synthetic multi-bank carts of the
//! non-EasyFlash families: a patch into a bank that is NOT mapped is applied, shows up when
//! the cart switches to that bank, and is gone again after the next anchor restore (a
//! read-only Ocean and a flash GMod2). Real binary, free port, synthetic ROM bytes only.

use futures_util::{SinkExt, StreamExt};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

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
    let dir = std::env::temp_dir().join(format!("trx64_cart_overlay_{port}_{}", std::process::id()));
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

async fn rpc_raw(ws: &mut Ws, method: &str, params: serde_json::Value) -> serde_json::Value {
    let req = serde_json::json!({ "jsonrpc": "2.0", "id": 7, "method": method, "params": params });
    ws.send(Message::Text(req.to_string())).await.unwrap();
    while let Some(Ok(msg)) = ws.next().await {
        if let Message::Text(t) = msg {
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            if v["id"] == 7 {
                return v;
            }
        }
    }
    panic!("no reply to {method}");
}

async fn rpc(ws: &mut Ws, method: &str, params: serde_json::Value) -> serde_json::Value {
    let v = rpc_raw(ws, method, params).await;
    assert!(v["error"].is_null(), "{method}: {v}");
    v["result"].clone()
}

/// A CRT of 8 KiB ROML banks; every byte of bank `n` is `fill(n)`.
fn crt(hw_type: u16, exrom: u8, game: u8, banks: &[u16], fill: impl Fn(u16) -> u8) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"C64 CARTRIDGE   ");
    out.extend_from_slice(&0x40u32.to_be_bytes());
    out.extend_from_slice(&0x0100u16.to_be_bytes());
    out.extend_from_slice(&hw_type.to_be_bytes());
    out.extend_from_slice(&[exrom, game, 0, 0, 0, 0, 0, 0]);
    let mut name = b"overlay test".to_vec();
    name.resize(32, 0);
    out.extend_from_slice(&name);
    for &b in banks {
        out.extend_from_slice(b"CHIP");
        out.extend_from_slice(&0x2010u32.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&b.to_be_bytes());
        out.extend_from_slice(&0x8000u16.to_be_bytes());
        out.extend_from_slice(&0x2000u16.to_be_bytes());
        out.extend(std::iter::repeat_n(fill(b), 0x2000));
    }
    out
}

/// The byte the CPU sees at $9F00 right now (banked view, no side effects).
async fn cpu_9f00(ws: &mut Ws) -> String {
    let r = rpc(ws, "monitor/exec", serde_json::json!({ "command": "m cpu 9f00 9f00" })).await;
    r.to_string()
}

async fn overlay_read(ws: &mut Ws, anchor: &str, bytes: &[u8]) -> serde_json::Value {
    let r = rpc(
        ws,
        "runtime/overlay_run",
        serde_json::json!({
            "anchor_id": anchor,
            "patches": [{ "space": "roml", "bank": 5, "addr": 0x9f00, "bytes": bytes, "read": true }],
            "run_cycles": 0
        }),
    )
    .await;
    r["reads"]["roml:5:$9f00"].clone()
}

async fn run_family(label: &str, hw_type: u16, exrom: u8, game: u8, banks: &[u16], select_bank5: &str) {
    let port = free_port();
    let d = start(port);
    let mut ws = connect(port).await;
    let path = d.1.join(format!("{label}.crt"));
    std::fs::write(&path, crt(hw_type, exrom, game, banks, |b| b as u8)).unwrap();

    rpc(&mut ws, "session/create", serde_json::json!({ "model": "c64-pal" })).await;
    rpc(&mut ws, "media/mount", serde_json::json!({ "path": path.to_string_lossy() })).await;
    rpc(&mut ws, "debug/pause", serde_json::json!({})).await;
    rpc(&mut ws, "session/run", serde_json::json!({ "cycles": 2_000_000 })).await;
    let cp = rpc(&mut ws, "checkpoint/capture", serde_json::json!({})).await;
    let anchor = cp["ref"]["id"].as_str().expect("checkpoint id").to_string();

    // Bank 0 is mapped; bank 5 is not. Patch bank 5: applied, read back.
    assert_eq!(overlay_read(&mut ws, &anchor, &[0xab]).await, 0xab, "{label}: patch applied");
    // Bank 0 still shows its own byte at $9F00, bank 5 shows the patch once selected.
    assert!(cpu_9f00(&mut ws).await.contains("00 "), "{label}: bank 0 untouched");
    rpc(&mut ws, "monitor/exec", serde_json::json!({ "command": select_bank5 })).await;
    let shown = cpu_9f00(&mut ws).await;
    assert!(shown.contains("ab"), "{label}: live window after switching to bank 5: {shown}");

    // The next overlay restores the anchor first: the patch is gone, bank 5 reads its image.
    assert_eq!(overlay_read(&mut ws, &anchor, &[]).await, 5, "{label}: rolled back by the restore");
    assert!(!cpu_9f00(&mut ws).await.contains("ab"), "{label}: live window clean after restore");

    // And the failure that started this: a bank past the cart is refused by name.
    let v = rpc_raw(
        &mut ws,
        "runtime/overlay_run",
        serde_json::json!({
            "anchor_id": anchor,
            "patches": [{ "space": "roml", "bank": 70, "addr": 0x9f00, "bytes": [1] }],
            "run_cycles": 0
        }),
    )
    .await;
    let msg = v["error"]["message"].as_str().unwrap_or("").to_string();
    assert!(msg.contains("bank 70"), "{label}: {v}");
    assert!(!msg.contains("not implemented"), "{label}: {v}");
}

#[tokio::test(flavor = "multi_thread")]
async fn overlay_run_patches_a_bank_of_a_read_only_ocean_and_rolls_it_back() {
    run_family("ocean", 5, 0, 0, &[0, 5], "wr cpu de00 05").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn overlay_run_patches_a_bank_of_a_flash_gmod2_and_rolls_it_back() {
    run_family("gmod2", 60, 0, 1, &[0, 5], "wr cpu de00 05").await;
}
