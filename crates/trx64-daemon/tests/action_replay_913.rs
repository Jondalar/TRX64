//! Action Replay V5 / V6 through the real daemon binary, on free ports.
//!
//! S7 is hermetic (a synthetic cartridge with an NMI handler, over two fresh daemons).
//! S1, S2, S4 and S5 run on the real images in `~/Development/C64/Tools/Cartridges/
//! Action_Replay/` (Commodore/Datel ROMs are not checked in) and say `SKIP` when that
//! directory is absent. Every image is mounted from a COPY: a mount may persist.

use futures_util::{SinkExt, StreamExt};
use sha2::Digest;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type J = serde_json::Value;

const CORPUS: &str = "/Users/alex/Development/C64/Tools/Cartridges/Action_Replay";

fn corpus() -> Option<PathBuf> {
    let d = std::env::var("TRX64_AR_CORPUS").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(CORPUS));
    d.join("Action_Replay_V5.0_5.0_1988_PAL.crt").exists().then_some(d)
}

macro_rules! skip {
    ($($why:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), "SKIP: {} — {}", module_path!(), format!($($why)*));
        return;
    }};
}

// ── the daemon ───────────────────────────────────────────────────────────────────

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
    let dir = std::env::temp_dir().join(format!("trx64_ar913_{port}_{}", std::process::id()));
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

async fn call(ws: &mut Ws, method: &str, params: J) -> Result<J, String> {
    let req = serde_json::json!({ "jsonrpc": "2.0", "id": 7, "method": method, "params": params });
    ws.send(Message::Text(req.to_string())).await.unwrap();
    while let Some(Ok(msg)) = ws.next().await {
        if let Message::Text(t) = msg {
            let v: J = serde_json::from_str(&t).unwrap();
            if v["id"] == 7 {
                return if v["error"].is_null() { Ok(v["result"].clone()) } else { Err(v["error"].to_string()) };
            }
        }
    }
    panic!("no reply to {method}");
}

async fn rpc(ws: &mut Ws, method: &str, params: J) -> J {
    call(ws, method, params).await.unwrap_or_else(|e| panic!("{method}: {e}"))
}

fn b64(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => continue,
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

/// C64 RAM (the RAM lens, whatever the banking) over `[addr, addr+len)`.
async fn ram(ws: &mut Ws, addr: u32, len: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let (mut a, end) = (addr, addr + len);
    while a < end {
        let n = (end - a).min(0x4000);
        let r = rpc(ws, "session/read_memory", serde_json::json!({ "ranges": [{ "addr": a, "len": n, "lens": "ram" }] })).await;
        out.extend(b64(r["chunks"][0]["bytes"].as_str().unwrap()));
        a += n;
    }
    out
}

/// The 25 text lines of screen RAM, as the screen codes spell them.
async fn screen(ws: &mut Ws) -> Vec<String> {
    let mem = ram(ws, 0x0400, 1000).await;
    mem.chunks(40)
        .map(|row| {
            let s: String = row
                .iter()
                .map(|&b| match b & 0x7f {
                    0 => '@',
                    c @ 1..=26 => (b'A' + c - 1) as char,
                    c @ 32..=63 => c as char,
                    27..=31 => '?',
                    _ => '#',
                })
                .collect();
            s.trim_end().to_string()
        })
        .collect()
}

fn shows(lines: &[String], text: &str) -> bool {
    lines.iter().any(|l| l.contains(text))
}

async fn cycles(ws: &mut Ws) -> u64 {
    rpc(ws, "session/state", serde_json::json!({})).await["cpu"]["cycles"].as_u64().unwrap()
}

async fn run(ws: &mut Ws, n: u64) -> J {
    rpc(ws, "session/run", serde_json::json!({ "cycles": n })).await
}

/// Mount and bring the machine to where the door used to leave it. A mount (a power cycle)
/// runs nothing (Spec 910 D8): the machine stands on its first instruction, running, and the
/// run loop takes it from there. So the first instruction is made a stop — the same cycle 0 on
/// every fresh daemon — and the boot the door used to run before it answered, 5M cycles, is
/// the test's own `session/run`.
async fn mount_and_warm(ws: &mut Ws, params: J) -> J {
    rpc(ws, "monitor/exec", serde_json::json!({ "command": "obs first when exec 0000..ffff do break" })).await;
    let r = rpc(ws, "media/mount", params).await;
    let until = Instant::now() + Duration::from_secs(10);
    while rpc(ws, "debug/state", serde_json::json!({})).await["runState"] != "paused" {
        assert!(Instant::now() < until, "the machine did not stop on its first instruction");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    rpc(ws, "monitor/exec", serde_json::json!({ "command": "obs first del" })).await;
    run(ws, 5_000_000).await;
    r
}

/// Hold `key` for `hold` cycles and let go.
async fn tap(ws: &mut Ws, key: &str) {
    rpc(ws, "session/key_down", serde_json::json!({ "key": key })).await;
    run(ws, 150_000).await;
    rpc(ws, "session/key_up", serde_json::json!({ "key": key })).await;
    run(ws, 150_000).await;
}

async fn cart(ws: &mut Ws) -> J {
    rpc(ws, "session/cart_status", serde_json::json!({})).await
}

fn sha(b: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(b))
}

fn copy_to(d: &Daemon, src: &Path, name: &str) -> PathBuf {
    let p = d.1.join(name);
    std::fs::copy(src, &p).unwrap();
    p
}

// ── a BASIC counter ──────────────────────────────────────────────────────────────

/// `10 A=A+1:POKE49152,A-INT(A/256)*256:POKE49153,INT(A/256):GOTO10`, tokenised. The counter
/// is the 16-bit value at $C000/$C001.
fn counter_prg() -> Vec<u8> {
    let mut body: Vec<u8> = vec![];
    body.extend([0x41, 0xB2, 0x41, 0xAA, 0x31, 0x3A]);
    body.extend([0x97, 0x34, 0x39, 0x31, 0x35, 0x32, 0x2C, 0x41, 0xAB, 0xB5, 0x28, 0x41, 0xAD, 0x32, 0x35, 0x36, 0x29, 0xAC, 0x32, 0x35, 0x36, 0x3A]);
    body.extend([0x97, 0x34, 0x39, 0x31, 0x35, 0x33, 0x2C, 0xB5, 0x28, 0x41, 0xAD, 0x32, 0x35, 0x36, 0x29, 0x3A]);
    body.extend([0x89, 0x31, 0x30, 0x00]);
    let next = 0x0801 + 4 + body.len() as u16;
    let mut p = vec![0x01, 0x08, (next & 0xff) as u8, (next >> 8) as u8, 10, 0];
    p.extend(body);
    p.extend([0, 0]);
    p
}

async fn counter(ws: &mut Ws) -> u32 {
    let m = ram(ws, 0xc000, 2).await;
    m[0] as u32 | (m[1] as u32) << 8
}

/// Mount `path`, leave the cartridge's start-up screen with F3 (NORMAL RESET), put the counter
/// program in and RUN it.
async fn boot_counter(ws: &mut Ws, d: &Daemon, path: &Path) {
    mount_and_warm(ws, serde_json::json!({ "path": path.to_string_lossy() })).await;
    tap(ws, "F3").await;
    run(ws, 2_000_000).await;
    let prg = d.1.join("counter.prg");
    let bytes = counter_prg();
    std::fs::write(&prg, &bytes).unwrap();
    rpc(ws, "session/load_prg", serde_json::json!({ "prg_path": prg.to_string_lossy() })).await;
    let end = 0x0801 + (bytes.len() as u32 - 2);
    for a in [0x2d, 0x2f, 0x31] {
        rpc(ws, "monitor/exec", serde_json::json!({ "command": format!("wr ram {a:04x} {:02x} {:02x}", end & 0xff, end >> 8) })).await;
    }
    rpc(ws, "session/type", serde_json::json!({ "text": "RUN\n" })).await;
    run(ws, 3_000_000).await;
}

// ── the images ───────────────────────────────────────────────────────────────────

/// (file stem, the start-up screen's title line, its copyright line, the freezer menu's title line)
const IMAGES: [(&str, &str, &str, &str); 4] = [
    ("Action_Replay_V5.0_5.0_1988_PAL", "ACTION REPLAY V5.0  PROFESSIONAL", "(C) DATEL ELECTRONICS 1988", "ACTION REPLAY V5.0  PROFESSIONAL"),
    ("Action_Replay_V5.0_5.0_1988_NTSC", "ACTION REPLAY V5.0  PROFESSIONAL", "(C) DATEL ELECTRONICS 1988", "ACTION REPLAY V5.0  PROFESSIONAL"),
    ("Action_Replay_V5.0_5.1a_1988_PAL", "ACTION REPLAY V5.0  PROFESSIONAL", "(C) DATEL ELECTRONICS 1988", "ACTION REPLAY V5.0  PROFESSIONAL"),
    ("Action_Replay_V6.0_6.0_1989_PAL", "ACTION REPLAY PROFESSIONAL  V6.0", "(C) DATEL ELECTRONICS 1989", "ACTION REPLAY V6.0  PROFESSIONAL"),
];

/// The freezer menu's own words, the same on every image: the keys that leave it.
const MENU: &str = "F1 - BACKUP  F3 - RESTART  F7 - SCREEN";

// ── S1 + S2: mount as .crt and .bin, boot to the start-up screen ─────────────────

#[tokio::test(flavor = "multi_thread")]
async fn s1_s2_each_image_mounts_as_crt_and_bin_and_boots_to_its_own_screen() {
    let Some(dir) = corpus() else { skip!("no Action Replay corpus at {CORPUS}") };
    let port = free_port();
    let d = start(port);
    let mut ws = connect(port).await;
    for (stem, title, copyright, _) in IMAGES {
        let mut shots = Vec::new();
        for (ext, extra) in [("crt", serde_json::json!({})), ("bin", serde_json::json!({ "cart_type": "ar" }))] {
            let src = dir.join(format!("{stem}.{ext}"));
            let bytes = std::fs::read(&src).unwrap();
            let p = copy_to(&d, &src, &format!("{stem}.{ext}"));
            let mut params = serde_json::json!({ "path": p.to_string_lossy() });
            params.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            let r = mount_and_warm(&mut ws, params).await;
            assert_eq!(r["mapperType"], "action-replay", "{stem}.{ext}: {r}");
            run(&mut ws, 3_000_000).await;

            // session/state names the cartridge by family, banks and sha256
            let s = rpc(&mut ws, "session/state", serde_json::json!({})).await;
            let c = &s["media"]["cart"];
            assert_eq!(c["mapperType"], "action-replay", "{stem}.{ext}: {c}");
            assert_eq!(c["banks"], 4, "{stem}.{ext}: {c}");
            assert_eq!(c["sha256"], sha(&bytes), "{stem}.{ext}: {c}");
            if ext == "crt" {
                assert_eq!(c["hwType"], 1, "{stem}: {c}");
            }
            let st = cart(&mut ws).await;
            assert_eq!(st["type"], "action-replay", "{st}");
            assert_eq!(st["freezer"]["active"], true);
            assert_eq!(st["freezer"]["mode"], 0, "it boots as an 8K cartridge: {st}");
            assert_eq!(st["bank"], 0);
            assert_eq!(st["buttons"], serde_json::json!(["freeze"]));

            // its own start-up screen
            let lines = screen(&mut ws).await;
            assert!(shows(&lines, title), "{stem}.{ext}: title line {title:?} not on screen:\n{}", lines.join("\n"));
            assert!(shows(&lines, copyright), "{stem}.{ext}: {copyright:?}\n{}", lines.join("\n"));
            assert!(shows(&lines, "F1 - CONFIGURE MEMORY"), "{stem}.{ext}:\n{}", lines.join("\n"));
            assert!(shows(&lines, "F3 - NORMAL RESET"));
            shots.push(ram(&mut ws, 0x0400, 1000).await);
        }
        assert_eq!(shots[0], shots[1], "{stem}: the .bin boots to the same screen as the .crt");
    }
}

// ── S4: freeze and return ────────────────────────────────────────────────────────

/// Where the cartridge's NMI vector points, read from the image's bank 0 (`$1FFA` of the ROMH
/// window): the first instruction the CPU runs when it takes the freeze NMI.
fn nmi_entry(rom: &[u8]) -> u16 {
    rom[0x1ffa] as u16 | (rom[0x1ffb] as u16) << 8
}

async fn halted_at(ws: &mut Ws, pc: u16) -> J {
    rpc(ws, "debug/break_add", serde_json::json!({ "pc": pc })).await;
    let r = run(ws, 3_000_000).await;
    rpc(ws, "debug/break_del", serde_json::json!({})).await;
    r
}

/// The RAM a program leaves alone: the low pages and the screen, the program text, and the
/// free RAM above the program up to the BASIC top and the $C100 page up. Zero page, the stack
/// and the variables are the running program's own.
const KEPT: [(u32, u32); 5] = [(0x0200, 0x0200), (0x0400, 0x0400), (0x0801, 0x0038), (0x0900, 0x9700), (0xc100, 0x0f00)];

async fn kept_ram(ws: &mut Ws) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for (a, n) in KEPT {
        out.push(ram(ws, a, n).await);
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn s4_freeze_and_return_on_each_image() {
    let Some(dir) = corpus() else { skip!("no Action Replay corpus at {CORPUS}") };
    for (stem, _, _, title) in IMAGES {
        let port = free_port();
        let d = start(port);
        let mut ws = connect(port).await;
        let src = dir.join(format!("{stem}.crt"));
        let rom = std::fs::read(dir.join(format!("{stem}.bin"))).unwrap();
        let p = copy_to(&d, &src, &format!("{stem}.crt"));
        boot_counter(&mut ws, &d, &p).await;

        let c_run = counter(&mut ws).await;
        run(&mut ws, 1_000_000).await;
        assert!(counter(&mut ws).await > c_run, "{stem}: the BASIC program counts");

        // the press: on the cycle it is made, then run to the first instruction of the NMI
        // entry — reachable only through the cartridge's own vector, so only in ultimax
        let before_press = kept_ram(&mut ws).await;
        let at = cycles(&mut ws).await;
        let r = rpc(&mut ws, "cart/button", serde_json::json!({ "button": "freeze" })).await;
        assert_eq!((r["button"].as_str(), r["cycle"].as_u64(), r["cartType"].as_str()), (Some("freeze"), Some(at), Some("action-replay")), "{r}");
        let hit = halted_at(&mut ws, nmi_entry(&rom)).await;
        assert_eq!(hit["breakpoint"]["pc"], nmi_entry(&rom) as u64, "{stem}: the CPU took the NMI through the cartridge's vector: {hit}");
        let st = cart(&mut ws).await;
        assert_eq!(st["freezer"]["mode"], 3, "{stem}: ultimax: {st}");
        assert_eq!(st["freezer"]["ram"], true, "{st}");
        assert_eq!(st["freezer"]["frozen"], true, "{st}");
        assert_eq!(st["freezer"]["nmi"], true, "{stem}: the cartridge holds NMI until the menu releases it: {st}");
        assert_eq!(st["bank"], 0);
        let frozen_counter = counter(&mut ws).await;
        assert!(frozen_counter >= c_run, "{stem}: {frozen_counter} {c_run}");

        // the freezer menu is on screen
        run(&mut ws, 1_500_000).await;
        let lines = screen(&mut ws).await;
        assert!(shows(&lines, title), "{stem}: the menu's own title:\n{}", lines.join("\n"));
        assert!(shows(&lines, MENU), "{stem}: the menu's keys:\n{}", lines.join("\n"));
        assert!(shows(&lines, "M - MONITOR"), "{stem}:\n{}", lines.join("\n"));
        let st = cart(&mut ws).await;
        assert_eq!(st["freezer"]["nmi"], false, "{stem}: the menu let NMI go: {st}");

        // F3 (RESTART) returns to the program at the place it stopped
        tap(&mut ws, "F3").await;
        run(&mut ws, 1_000_000).await;
        let lines = screen(&mut ws).await;
        assert!(!shows(&lines, MENU), "{stem}: the menu is gone:\n{}", lines.join("\n"));
        assert!(!shows(&lines, "SYNTAX"), "{stem}:\n{}", lines.join("\n"));
        let resumed = counter(&mut ws).await;
        assert!(resumed >= frozen_counter, "{stem}: the counter runs on from {frozen_counter}, not from the start: {resumed}");
        run(&mut ws, 1_000_000).await;
        let later = counter(&mut ws).await;
        assert!(later > resumed, "{stem}: and keeps running: {resumed} -> {later}");
        assert!(later - frozen_counter < 400, "{stem}: from the frozen value, not another: {frozen_counter} -> {later}");
        assert_eq!(kept_ram(&mut ws).await, before_press, "{stem}: RAM below the program and the program itself are as before");
        assert_eq!(cart(&mut ws).await["freezer"]["nmi"], false);
    }
}

// ── S5: rewind across a freeze; .c64re dump and undump while frozen ──────────────

/// The freezer part of a `.c64re` dump, from the file: (cartridge RAM, the rest as JSON).
fn freezer_of(path: &Path) -> (Vec<u8>, J) {
    let bytes = std::fs::read(path).unwrap();
    let r = trx64_core::native_snapshot::read_native_snapshot(&bytes).expect("a readable .c64re");
    let f = r.checkpoint["cartState"]["freezerState"].clone();
    assert!(f.is_object(), "the dump carries the cartridge's freezer state: {}", r.checkpoint["cartState"]);
    let ram = trx64_core::native_snapshot::ta_u8_decode(&f["ram"]).expect("the RAM is a byte node");
    let mut rest = f;
    rest.as_object_mut().unwrap().remove("ram");
    (ram, rest)
}

#[tokio::test(flavor = "multi_thread")]
async fn s5_rewind_and_dump_across_a_freeze_on_each_image() {
    let Some(dir) = corpus() else { skip!("no Action Replay corpus at {CORPUS}") };
    for (stem, _, _, title) in IMAGES {
        let port = free_port();
        let d = start(port);
        let mut ws = connect(port).await;
        let p = copy_to(&d, &dir.join(format!("{stem}.crt")), &format!("{stem}.crt"));
        boot_counter(&mut ws, &d, &p).await;

        // frozen: the menu on screen, the cartridge RAM written by it
        rpc(&mut ws, "cart/button", serde_json::json!({ "button": "freeze" })).await;
        run(&mut ws, 3_000_000).await;
        assert!(shows(&screen(&mut ws).await, MENU), "{stem}: frozen, menu on screen");
        let cp = rpc(&mut ws, "checkpoint/capture", serde_json::json!({})).await;
        let id = cp["ref"]["id"].as_str().expect("checkpoint id").to_string();
        let frozen_cart = cart(&mut ws).await["freezer"].clone();
        let frozen_screen = ram(&mut ws, 0x0400, 1000).await;
        let dump_a = d.1.join("frozen_a.c64re");
        rpc(&mut ws, "snapshot/dump", serde_json::json!({ "path": dump_a.to_string_lossy() })).await;
        let (ram_a, rest_a) = freezer_of(&dump_a);
        assert_eq!(ram_a.len(), 0x2000);
        assert!(ram_a.iter().filter(|&&b| b != 0xff).count() > 1000, "{stem}: the menu wrote into the cartridge RAM");

        // return to the program and let it run
        tap(&mut ws, "F3").await;
        run(&mut ws, 1_500_000).await;
        assert!(!shows(&screen(&mut ws).await, MENU), "{stem}: back in the program");
        let c1 = counter(&mut ws).await;
        run(&mut ws, 1_000_000).await;
        assert!(counter(&mut ws).await > c1);
        assert_ne!(cart(&mut ws).await["freezer"], frozen_cart, "{stem}: the cartridge moved on");

        // 1. the checkpoint: the menu is on screen again, the cartridge is what it was
        rpc(&mut ws, "checkpoint/restore", serde_json::json!({ "id": id, "then": "pause" })).await;
        let lines = screen(&mut ws).await;
        assert!(shows(&lines, MENU) && shows(&lines, title), "{stem}: the menu is back:\n{}", lines.join("\n"));
        assert_eq!(ram(&mut ws, 0x0400, 1000).await, frozen_screen, "{stem}");
        assert_eq!(cart(&mut ws).await["freezer"], frozen_cart, "{stem}: registers, lines and NMI as captured");
        let dump_b = d.1.join("frozen_b.c64re");
        rpc(&mut ws, "snapshot/dump", serde_json::json!({ "path": dump_b.to_string_lossy() })).await;
        let (ram_b, rest_b) = freezer_of(&dump_b);
        assert_eq!(ram_b, ram_a, "{stem}: the cartridge RAM is the checkpoint's");
        assert_eq!(rest_b, rest_a);
        // and it carries on as before: F3 returns to the program again
        run(&mut ws, 100_000).await;
        tap(&mut ws, "F3").await;
        run(&mut ws, 2_000_000).await;
        assert!(!shows(&screen(&mut ws).await, MENU), "{stem}: restore, then return: back in the program");
        let c2 = counter(&mut ws).await;
        run(&mut ws, 1_000_000).await;
        assert!(counter(&mut ws).await > c2, "{stem}: and it runs");

        // 2. the .c64re dump taken while frozen, undumped from the running program
        rpc(&mut ws, "snapshot/undump", serde_json::json!({ "path": dump_a.to_string_lossy() })).await;
        let lines = screen(&mut ws).await;
        assert!(shows(&lines, MENU), "{stem}: undump while frozen:\n{}", lines.join("\n"));
        assert_eq!(cart(&mut ws).await["freezer"], frozen_cart, "{stem}");
        let dump_c = d.1.join("frozen_c.c64re");
        rpc(&mut ws, "snapshot/dump", serde_json::json!({ "path": dump_c.to_string_lossy() })).await;
        assert_eq!(freezer_of(&dump_c).0, ram_a, "{stem}: the cartridge RAM survives dump and undump");
        tap(&mut ws, "F3").await;
        run(&mut ws, 2_000_000).await;
        assert!(!shows(&screen(&mut ws).await, MENU), "{stem}: and the undumped freeze returns to the program");
    }
}

// ── S7: the same press on the same cycle is the same run ─────────────────────────

fn tag(n: usize, i: usize) -> u8 {
    ((n * 0x41) as u8) ^ (i as u8) ^ ((i >> 8) as u8).wrapping_mul(7) ^ 0x5a
}

/// A synthetic Action Replay CRT: bank 0 holds a CBM80 autostart into a counting loop in
/// $0334 and an NMI handler (via the cartridge's own vector) that counts in $0335, builds
/// `LDA #$40 / STA $DE00 / RTI` in RAM and runs it — the register store lets NMI go and puts
/// the cartridge back to an 8K game, the RTI resumes the loop.
fn synthetic_ar() -> Vec<u8> {
    let mut banks: Vec<Vec<u8>> = (0..4).map(|n| (0..0x2000).map(|i| tag(n, i)).collect()).collect();
    let b = &mut banks[0];
    b.iter_mut().for_each(|x| *x = 0xea);
    let put = |b: &mut Vec<u8>, at: usize, code: &[u8]| b[at..at + code.len()].copy_from_slice(code);
    put(b, 0, &[0x09, 0x80, 0x09, 0x80, 0xc3, 0xc2, 0xcd, 0x38, 0x30]);
    put(
        b,
        9,
        &[
            0x8e, 0x16, 0xd0, 0x20, 0xa3, 0xfd, 0x20, 0x50, 0xfd, 0x20, 0x15, 0xfd, 0x20, 0x5b, 0xff, // init
            0xee, 0x34, 0x03, 0x4c, 0x18, 0x80, // loop: INC $0334 / JMP loop
        ],
    );
    put(
        b,
        0x1000,
        &[
            0xa9, 0xa9, 0x8d, 0x40, 0x03, 0xa9, 0x40, 0x8d, 0x41, 0x03, 0xa9, 0x8d, 0x8d, 0x42, 0x03, 0xa9, 0x00, 0x8d, 0x43, 0x03,
            0xa9, 0xde, 0x8d, 0x44, 0x03, 0xa9, 0x40, 0x8d, 0x45, 0x03, 0xee, 0x35, 0x03, 0x4c, 0x40, 0x03,
        ],
    );
    b[0x1ffa..0x2000].copy_from_slice(&[0x00, 0xf0, 0x09, 0x80, 0x00, 0xf0]);
    let mut v = Vec::new();
    v.extend_from_slice(b"C64 CARTRIDGE   ");
    v.extend_from_slice(&0x40u32.to_be_bytes());
    v.extend_from_slice(&0x0100u16.to_be_bytes());
    v.extend_from_slice(&1u16.to_be_bytes());
    v.extend_from_slice(&[0, 0]);
    v.extend_from_slice(&[0u8; 6]);
    let mut name = [0u8; 32];
    name[..8].copy_from_slice(b"AR SYNTH");
    v.extend_from_slice(&name);
    for (n, data) in banks.iter().enumerate() {
        v.extend_from_slice(b"CHIP");
        v.extend_from_slice(&(0x10 + data.len() as u32).to_be_bytes());
        v.extend_from_slice(&0u16.to_be_bytes());
        v.extend_from_slice(&(n as u16).to_be_bytes());
        v.extend_from_slice(&0x8000u16.to_be_bytes());
        v.extend_from_slice(&(data.len() as u16).to_be_bytes());
        v.extend_from_slice(data);
    }
    v
}

/// One fresh daemon, the synthetic cartridge, a press 5000 cycles ahead, a run past it: what
/// the machine is, and the CPU trace of the stretch around the press.
async fn synthetic_run() -> (J, Vec<u8>, J, Vec<String>) {
    let port = free_port();
    let d = start(port);
    let mut ws = connect(port).await;
    let p = d.1.join("synth.crt");
    std::fs::write(&p, synthetic_ar()).unwrap();
    mount_and_warm(&mut ws, serde_json::json!({ "path": p.to_string_lossy() })).await;
    run(&mut ws, 1_000_000).await;
    assert_ne!(ram(&mut ws, 0x0334, 1).await[0], 0, "the cartridge's program runs");
    assert_eq!(ram(&mut ws, 0x0335, 1).await[0], 0);
    let out = d.1.join("synth.duckdb");
    rpc(&mut ws, "trace/start_domains", serde_json::json!({ "domains": ["c64-cpu"], "output": out.to_string_lossy() })).await;
    let at = cycles(&mut ws).await + 5_000;
    let r = rpc(&mut ws, "cart/button", serde_json::json!({ "button": "freeze", "cycle": at })).await;
    assert_eq!(r["cycle"], at);
    assert_eq!(cart(&mut ws).await["freezer"]["pendingFreezeAt"], at, "held until its cycle");
    run(&mut ws, 20_000).await;
    rpc(&mut ws, "trace/run/stop", serde_json::json!({})).await;
    rpc(&mut ws, "debug/pause", serde_json::json!({})).await;
    assert_eq!(ram(&mut ws, 0x0335, 1).await[0], 1, "the handler ran once, at the press");
    let st = cart(&mut ws).await;
    assert_eq!(st["freezer"]["mode"], 0, "and the register store put the cartridge back");
    assert_eq!(st["freezer"]["nmi"], false);
    let state = rpc(&mut ws, "session/state", serde_json::json!({})).await["cpu"].clone();
    let mem = ram(&mut ws, 0, 0x4000).await;
    // the capture's records, not its file: the header names the output path and the time
    let mut events = Vec::new();
    trx64_traceindex::stream_retrace(&d.1.join("synth.c64retrace"), |e| events.push(format!("{e:?}"))).expect("the capture");
    (state, mem, st["freezer"].clone(), events)
}

#[tokio::test(flavor = "multi_thread")]
async fn s7_same_press_same_cycle_is_the_same_run_over_fresh_daemons() {
    let (s1, m1, f1, t1) = synthetic_run().await;
    let (s2, m2, f2, t2) = synthetic_run().await;
    assert_eq!(s1, s2, "the CPU");
    assert_eq!(m1, m2, "RAM");
    assert_eq!(f1, f2, "the cartridge");
    assert!(t1.len() > 1000, "a capture of the stretch: {} records", t1.len());
    assert!(t1.iter().any(|e| e.contains("f000") || e.contains("61440")), "the handler is in the capture");
    assert_eq!(t1, t2, "the traces are identical, record for record");
}

// ── S6: a peek does nothing ──────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn s6_the_monitor_and_state_reads_leave_the_register_and_the_map_alone() {
    let port = free_port();
    let d = start(port);
    let mut ws = connect(port).await;
    let p = d.1.join("synth.crt");
    std::fs::write(&p, synthetic_ar()).unwrap();
    mount_and_warm(&mut ws, serde_json::json!({ "path": p.to_string_lossy() })).await;
    // a register value a stray read could change: RAM on, 16K game, bank 2
    rpc(&mut ws, "monitor/exec", serde_json::json!({ "command": "wr cpu de00 31" })).await;
    let before = cart(&mut ws).await;
    assert_eq!((before["freezer"]["register"].as_u64(), before["freezer"]["mode"].as_u64(), before["bank"].as_u64()), (Some(0x31), Some(1), Some(2)), "{before}");
    let cycles_before = cycles(&mut ws).await;
    for lens in ["cpu", "cart", "io", "ram"] {
        rpc(&mut ws, "session/read_memory", serde_json::json!({ "ranges": [{ "addr": 0xde00, "len": 0x200, "lens": lens }] })).await;
    }
    rpc(&mut ws, "monitor/exec", serde_json::json!({ "command": "m de00 dfff" })).await;
    rpc(&mut ws, "monitor/exec", serde_json::json!({ "command": "m 8000 8100" })).await;
    rpc(&mut ws, "session/state", serde_json::json!({})).await;
    rpc(&mut ws, "monitor/state", serde_json::json!({})).await;
    assert_eq!(cart(&mut ws).await, before, "no read fired a side effect");
    assert_eq!(cycles(&mut ws).await, cycles_before);
    let r = rpc(&mut ws, "session/read_memory", serde_json::json!({ "ranges": [{ "addr": 0xde00, "len": 1, "lens": "cart" }] })).await;
    assert_eq!(b64(r["chunks"][0]["bytes"].as_str().unwrap()), vec![0x31], "the monitor shows the register");
}
