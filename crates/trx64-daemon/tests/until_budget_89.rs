//! C64RE #89 — `api/call until` honours the caller's cycle budget (`args[1].budget`), stops
//! at the first instruction boundary at or past it, and `instructionsElapsed` counts
//! instructions, not cycles. `stepOver` reads its budget from `args[0].budget`. The real
//! binary, headless, on a free port, warmed to the BASIC prompt.

use futures_util::{SinkExt, StreamExt};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

struct Daemon(Child, std::path::PathBuf);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

fn start(port: u16) -> Daemon {
    let dir = std::env::temp_dir().join(format!("trx64_89_{port}_{}", std::process::id()));
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

async fn api(ws: &mut Ws, method: &str, args: serde_json::Value) -> serde_json::Value {
    rpc(ws, "api/call", serde_json::json!({ "method": method, "args": args })).await
}

fn u(v: &serde_json::Value, k: &str) -> u64 {
    v[k].as_u64().unwrap_or_else(|| panic!("{k} missing: {v}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn until_honours_the_budget_and_counts_instructions() {
    let port = free_port();
    let _d = start(port);
    let mut ws = connect(port).await;
    // 120 PAL frames: the BASIC prompt, the KERNAL cursor-blink IRQ running.
    rpc(&mut ws, "session/run", serde_json::json!({ "cycles": 120 * 19_656 })).await;

    // $0002 is never executed at the prompt: the run ends on the budget.
    for budget in [1_000u64, 100_000] {
        let r = api(&mut ws, "until", serde_json::json!([2, { "budget": budget }])).await;
        assert_eq!(r["halted"], false, "{r}");
        assert_eq!(r["budgetExhausted"], true, "{r}");
        let cyc = u(&r, "cyclesElapsed");
        assert!((budget..=budget + 7).contains(&cyc), "budget {budget}: {cyc} cycles ({r})");
        let ins = u(&r, "instructionsElapsed");
        assert!(ins > 0 && ins < cyc, "budget {budget}: {ins} instructions over {cyc} cycles ({r})");
    }

    // No budget: the default (10M) still applies, and a reachable target halts. $FF48 is
    // the KERNAL IRQ entry, reached once a frame.
    let r = api(&mut ws, "until", serde_json::json!([0xff48])).await;
    assert_eq!(r["halted"], true, "{r}");
    assert_eq!(u(&r, "finalPc"), 0xff48, "{r}");
    assert!(u(&r, "cyclesElapsed") <= 19_656 + 7, "{r}");

    // stepOver: args[0].budget. One instruction at the IRQ entry (PHA), not a run.
    let r = api(&mut ws, "stepOver", serde_json::json!([{ "budget": 1_000 }])).await;
    assert_eq!(r["halted"], true, "{r}");
    assert_eq!(u(&r, "instructionsElapsed"), 1, "{r}");
    assert!(u(&r, "cyclesElapsed") < 10, "{r}");
}
