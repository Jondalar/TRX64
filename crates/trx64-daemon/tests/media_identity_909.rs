//! Spec 909 (TRX64 half) — the daemon names each mounted medium by its sha256: in
//! `session/state`, in a trace header, and as `media/1` records while a trace runs. A disk
//! swap is recorded inside the capture; a cartridge swap is a power cycle, so the capture
//! ends there with the cartridge going out and the power going off, and the next capture
//! names the new cartridge. The real binary, headless, on a free port.

use futures_util::{SinkExt, StreamExt};
use sha2::Digest;
use std::path::{Path, PathBuf};
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
    let dir = std::env::temp_dir().join(format!("trx64_909_{port}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_trx64-daemon"))
        .args(["--port", &port.to_string(), "--headless"])
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

fn sha(b: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(b))
}

/// A D64 whose bytes differ by `fill`.
fn d64(fill: u8) -> Vec<u8> {
    vec![fill; 174_848]
}

/// A one-bank EasyFlash CRT named `name` in its header.
fn ef_crt(name: &[u8]) -> Vec<u8> {
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
    nm[..name.len()].copy_from_slice(name);
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

/// Index a finished capture into its own store and return its `trace_media` rows as
/// `(cycle, slot, action, sha256)`, and the header's meta.
fn media_rows(retrace: &Path, store: &Path) -> (Vec<(u64, String, String, String)>, trx64_traceindex::TraceFileMeta) {
    trx64_traceindex::index_binary_log(retrace, store, None).expect("index");
    let conn = trx64_traceindex::open_read_only(store).unwrap();
    let rows = trx64_traceindex::query_json(
        &conn,
        "SELECT cycle, coalesce(slot,''), action, coalesce(sha256,'') FROM trace_media ORDER BY cycle, rowid",
    )
    .unwrap();
    let marks = trx64_traceindex::query_json(&conn, "SELECT count(*) FROM trace_mark").unwrap();
    assert_eq!(marks[0][0], 0, "a media record is not a user mark");
    let rows = rows
        .into_iter()
        .map(|r| {
            (
                r[0].as_u64().unwrap(),
                r[1].as_str().unwrap().to_string(),
                r[2].as_str().unwrap().to_string(),
                r[3].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let (header, _) = trx64_traceindex::stream_retrace(retrace, |_| {}).unwrap();
    (rows, header.meta)
}

#[tokio::test(flavor = "multi_thread")]
async fn each_medium_is_named_by_hash_in_state_header_and_records() {
    let port = free_port();
    let d = start(port);
    let dir = d.1.clone();
    let mut ws = connect(port).await;

    let (a, b) = (d64(0x00), d64(0x11));
    let (ef1, ef2) = (ef_crt(b"FIRST CART"), ef_crt(b"SECOND"));
    for (n, bytes) in [("a.d64", &a), ("b.d64", &b), ("one.crt", &ef1), ("two.crt", &ef2)] {
        std::fs::write(dir.join(n), bytes).unwrap();
    }
    let p = |n: &str| dir.join(n).to_string_lossy().to_string();

    // ── state ──────────────────────────────────────────────────────────────
    rpc(&mut ws, "media/mount", serde_json::json!({ "path": p("a.d64") })).await;
    rpc(&mut ws, "debug/pause", serde_json::json!({})).await;
    let s = rpc(&mut ws, "session/state", serde_json::json!({})).await;
    assert_eq!(s["media"]["disk"]["sha256"], sha(&a), "{}", s["media"]);
    assert_eq!(s["drives"][0]["disk"]["sha256"], sha(&a), "{}", s["drives"]);

    // ── a disk swap inside one capture ─────────────────────────────────────
    let out = dir.join("disk.duckdb");
    rpc(&mut ws, "trace/start_domains", serde_json::json!({ "domains": ["c64-cpu"], "output": out.to_string_lossy() })).await;
    rpc(&mut ws, "session/run", serde_json::json!({ "cycles": 20_000 })).await;
    rpc(&mut ws, "media/swap", serde_json::json!({ "path": p("b.d64") })).await;
    rpc(&mut ws, "debug/pause", serde_json::json!({})).await;
    let swap_clk = rpc(&mut ws, "session/state", serde_json::json!({})).await["cpu"]["cycles"].as_u64().unwrap();
    rpc(&mut ws, "session/run", serde_json::json!({ "cycles": 20_000 })).await;
    let stop = rpc(&mut ws, "trace/run/stop", serde_json::json!({})).await;
    assert_eq!(stop["run"]["media"]["sha256"], sha(&a), "the run's drive 8 at its start: {stop}");

    let (rows, meta) = media_rows(&dir.join("disk.c64retrace"), &dir.join("disk_check.duckdb"));
    assert_eq!(meta.media_sha.as_deref(), Some(sha(&a).as_str()), "header mediaSha = drive 8");
    let m = meta.media.expect("header media");
    assert_eq!(m["v"], 1);
    assert_eq!(m["drives"]["8"]["sha256"], sha(&a));
    assert_eq!(m["drives"]["8"]["sourceName"], "a.d64");
    assert!(m.get("cart").is_none(), "no cartridge in: {m}");
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!((rows[0].1.as_str(), rows[0].2.as_str(), rows[0].3.as_str()), ("drive8", "start", sha(&a).as_str()));
    assert_eq!((rows[1].1.as_str(), rows[1].2.as_str(), rows[1].3.as_str()), ("drive8", "out", sha(&a).as_str()));
    assert_eq!((rows[2].1.as_str(), rows[2].2.as_str(), rows[2].3.as_str()), ("drive8", "in", sha(&b).as_str()));
    assert_eq!(rows[1].0, rows[2].0, "a swap is an out and an in at one cycle");
    assert!(rows[1].0 > rows[0].0, "inside the capture: {rows:?}");
    assert_eq!(rows[1].0, swap_clk, "at the cycle the machine stood at when the disk changed");

    // ── a cartridge swap ends the capture ──────────────────────────────────
    rpc(&mut ws, "media/mount", serde_json::json!({ "path": p("one.crt") })).await;
    rpc(&mut ws, "debug/pause", serde_json::json!({})).await;
    let s = rpc(&mut ws, "session/state", serde_json::json!({})).await;
    let cart = &s["media"]["cart"];
    assert_eq!(cart["sha256"], sha(&ef1), "{cart}");
    assert_eq!(cart["hwType"], 32, "{cart}");
    assert_eq!(cart["banks"], 1, "{cart}");
    assert_eq!(cart["crtName"], "FIRST CART", "{cart}");
    assert_eq!(cart["mapperType"], "easyflash", "{cart}");

    let out = dir.join("cart.duckdb");
    rpc(&mut ws, "trace/start_domains", serde_json::json!({ "domains": ["c64-cpu"], "output": out.to_string_lossy() })).await;
    rpc(&mut ws, "session/run", serde_json::json!({ "cycles": 20_000 })).await;
    let before = rpc(&mut ws, "session/state", serde_json::json!({})).await["cpu"]["cycles"].as_u64().unwrap();
    rpc(&mut ws, "media/swap", serde_json::json!({ "path": p("two.crt") })).await;
    rpc(&mut ws, "debug/pause", serde_json::json!({})).await;
    let (rows, meta) = media_rows(&dir.join("cart.c64retrace"), &dir.join("cart_check.duckdb"));
    let m = meta.media.expect("header media");
    assert_eq!(m["cart"]["sha256"], sha(&ef1));
    assert_eq!(m["cart"]["crtName"], "FIRST CART");
    assert_eq!(m["drives"]["8"]["sha256"], sha(&b), "the disk stayed in");
    let tail: Vec<_> = rows.iter().filter(|r| r.2 != "start").map(|r| (r.1.as_str(), r.2.as_str(), r.3.as_str())).collect();
    assert_eq!(tail, [("cart", "out", sha(&ef1).as_str()), ("", "power-off", "")], "{rows:?}");
    let ends: Vec<u64> = rows.iter().filter(|r| r.2 != "start").map(|r| r.0).collect();
    assert_eq!(ends[0], ends[1]);
    assert_eq!(ends[0], before, "the cycle the old machine stood at when it went off");

    // The next capture names the new cartridge.
    let out = dir.join("next.duckdb");
    rpc(&mut ws, "trace/start_domains", serde_json::json!({ "domains": ["c64-cpu"], "output": out.to_string_lossy() })).await;
    rpc(&mut ws, "session/run", serde_json::json!({ "cycles": 1_000 })).await;
    rpc(&mut ws, "trace/run/stop", serde_json::json!({})).await;
    let (rows, _) = media_rows(&dir.join("next.c64retrace"), &dir.join("next_check.duckdb"));
    assert!(rows.iter().any(|r| r.1 == "cart" && r.2 == "start" && r.3 == sha(&ef2)), "{rows:?}");
    assert!(rows.iter().all(|r| r.2 == "start"), "{rows:?}");
}
