//! Spec 910 D1 / S5 / S6 — a daemon started with `--scratch <dir>` never writes a medium
//! back into the file it was handed.
//!
//! Two kinds of proof:
//!
//! - STRUCTURAL (no ROMs needed): `main.rs` is read as text, and every place that takes a
//!   path from a request, resolves a path for a monitor verb or writes a file must be one
//!   this test knows by name and class. A medium door must go through the one resolver
//!   (`medium_path` / `scratch::open_folder` / `scratch::open_medium`), and a raw
//!   `fs::write` may sit only in the two write helpers. A door or a write added later that
//!   is on neither list makes this test fail until it is classified, which is the moment the
//!   author has to decide how it keeps the source file safe.
//! - BEHAVIOURAL (ROMs needed, else skipped loudly): the real binary, on a free port, is
//!   handed a D64 that a BASIC program SAVEs to, an EasyFlash CRT that a program flash-writes
//!   and a folder the program SAVEs into, through every door the daemon offers; the source is
//!   hashed before and after, across every way the daemon ends; the session saw its own write.
//!   S6: without the option the same persist writes the original.
//!
//! ROMs: `TRX64_ROM_DIR`, `~/.trx64/roms`, or the sibling workbench.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value as J};
use sha2::{Digest, Sha256};
use tokio_tungstenite::tungstenite::Message;

// ════════════════════════════════════════════════════════════════════════════════
//  STRUCTURAL
// ════════════════════════════════════════════════════════════════════════════════

/// What a place that touches a path is allowed to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// A path that names a medium the machine writes to: must go through the resolver.
    Medium,
    /// A file read once into RAM or a trace/snapshot read back: nothing writes to it.
    ReadOnce,
    /// A directory listing or a project directory: not a medium.
    Dir,
    /// A file the daemon writes at the caller's request (a dump, a save, an export): goes
    /// through `write_output`, which refuses a file the daemon was handed as a medium.
    Output,
    /// The project's own data (scenarios), written by design.
    ProjectData,
    /// The resolver itself and the two write helpers.
    Helper,
}

use Class::*;

/// Every place `main.rs` reads a path-like request parameter (`.get("…path…")`, `file`, `dir`,
/// `folder`, `image`, `output`, `out`) or resolves a path for a monitor verb, by
/// `function::arm` and the thing read. Exact: a stale row fails as well.
const PATH_DOORS: &[(&str, &str, Class)] = &[
    ("trace_store_for_analysis", "param:duckdb_path", ReadOnce),
    ("trace_store_for_analysis", "param:retrace_path", ReadOnce),
    ("run_monitor_marked::bitmap", "resolve:resolve_fs_path", Output),
    ("run_monitor_marked::traprules", "resolve:resolve_fs_path", ReadOnce),
    ("run_monitor_marked::identify", "resolve:resolve_fs_path_with_state", ReadOnce),
    ("run_monitor_marked::buildtrace", "param:retrace_path", ReadOnce),
    ("run_monitor_marked::ringdump", "resolve:resolve_fs_path_with_state", Output),
    ("run_monitor_marked::ringload", "resolve:resolve_fs_path_with_state", ReadOnce),
    ("run_monitor_marked::dump", "resolve:resolve_fs_path", Output),
    ("run_monitor_marked::savecrt", "resolve:resolve_fs_path", Output),
    ("run_monitor_marked::swapcrt", "resolve:resolve_fs_path", Medium),
    ("run_monitor_marked::cd", "resolve:resolve_fs_path", Dir),
    ("run_monitor_marked::ls", "resolve:resolve_fs_path", Dir),
    ("run_monitor_marked::mkdir", "resolve:resolve_fs_path", Dir),
    ("run_monitor_marked::rmdir", "resolve:resolve_fs_path", Dir),
    ("run_monitor_marked::load", "resolve:resolve_fs_path", ReadOnce),
    ("run_monitor_marked::save", "resolve:resolve_fs_path", Output),
    ("run_monitor_marked::asm-file", "resolve:resolve_fs_path_with_state", ReadOnce),
    ("run_monitor_marked::bload", "resolve:resolve_fs_path", ReadOnce),
    ("run_monitor_marked::bsave", "resolve:resolve_fs_path", Output),
    ("dispatch_request::project/set", "param:path", Dir),
    ("dispatch_request::session/create", "param:trace_out", Output),
    ("dispatch_request::session/load_prg", "param:prg_path", ReadOnce),
    ("dispatch_request::session/load_prg", "resolve:resolve_fs_path_with_state", ReadOnce),
    ("dispatch_request::device/folder_attach", "param:path", Medium),
    ("dispatch_request::fs/complete", "resolve:resolve_fs_path_with_state", Dir),
    ("dispatch_request::runtime/run_prg", "param:prg_path", ReadOnce),
    ("dispatch_request::runtime/run_prg", "resolve:resolve_fs_path_with_state", ReadOnce),
    ("dispatch_request::runtime/swap_disk_and_continue", "param:path", Medium),
    ("dispatch_request::media/browse", "param:path", Dir),
    ("dispatch_request::media/ingress", "param:path", Medium),
    ("dispatch_request::media/open", "param:path", Medium),
    ("dispatch_request::media/mount", "param:path", Medium),
    ("dispatch_request::media/swap", "param:path", Medium),
    ("dispatch_request::audio/export", "param:out_path", Output),
    ("dispatch_request::trace/start_domains", "param:output", Output),
    ("dispatch_request::trace/build_from_ring", "param:output_path", Output),
    ("dispatch_request::trace/run/start", "param:output", Output),
    ("dispatch_request::trace/index", "param:retrace_path", ReadOnce),
    ("dispatch_request::trace/read", "param:duckdb_path", ReadOnce),
    ("dispatch_request::recorder/dump", "param:path", Output),
    ("dispatch_request::snapshot/dump", "param:path", Output),
    ("dispatch_request::snapshot/undump", "param:path", ReadOnce),
    ("dispatch_request::vsf/save", "param:output_path", Output),
    ("dispatch_request::vsf/load", "param:input_path", ReadOnce),
    ("dispatch_request::ringbuffer/dump", "param:path", Output),
    ("dispatch_request::ringbuffer/restore", "param:path", ReadOnce),
    ("medium_path", "resolve:resolve_fs_path_with_state", Helper),
];

/// Every `fs::write` / `copy` / `rename` / `remove_*` / `File::create` / `OpenOptions` in
/// production code, by `function::arm`. A raw `fs::write` is allowed only in the two helpers,
/// the crash log and the scenario registry.
const FS_MUTATIONS: &[(&str, &str, Class)] = &[
    ("build_trace_from_ring", "fs:remove_file", Output),
    ("run_monitor_marked::rmdir", "fs:remove_dir", Dir),
    ("dispatch_request::runtime/scenario_save", "fs:write", ProjectData),
    ("dispatch_request::runtime/scenario_delete", "fs:remove_file", ProjectData),
    ("write_medium", "fs:write", Helper),
    ("write_output", "fs:write", Helper),
    ("purge_materialized_media", "fs:remove_dir_all", Output),
    ("main", "fs:write", Output),
];

struct Finding {
    line: usize,
    attr: String,
    what: String,
    /// The text of the arm (or fn) the finding is in.
    body: String,
}

/// Production lines of `main.rs`: the `#[cfg(test)]` modules and items are left out.
fn production(src: &str) -> Vec<(usize, &str)> {
    let l: Vec<&str> = src.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < l.len() {
        if l[i].trim() == "#[cfg(test)]" {
            if l.get(i + 1).map_or(false, |n| n.starts_with("mod ") || n.starts_with("pub mod ")) {
                let mut j = i + 1;
                while l[j] != "}" {
                    j += 1;
                }
                i = j + 1;
            } else {
                i += 2;
            }
            continue;
        }
        out.push((i + 1, l[i]));
        i += 1;
    }
    out
}

fn fn_name(line: &str) -> Option<String> {
    let mut rest = line;
    for p in ["pub(crate) ", "pub ", "async "] {
        rest = rest.strip_prefix(p).unwrap_or(rest);
    }
    let rest = rest.strip_prefix("fn ")?;
    let name: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
    (!name.is_empty()).then_some(name)
}

/// `        "a/b" | "c" =>` -> `a/b`.
fn arm_name(line: &str) -> Option<String> {
    let rest = line.strip_prefix("        \"")?;
    let end = rest.find('"')?;
    let name = &rest[..end];
    let after = rest[end + 1..].trim_start();
    let mut after = after;
    while let Some(a) = after.strip_prefix("| \"") {
        let e = a.find('"')?;
        after = a[e + 1..].trim_start();
    }
    after.starts_with("=>").then(|| name.to_string())
}

fn path_like(key: &str) -> bool {
    key.contains("path") || matches!(key, "file" | "dir" | "folder" | "image" | "output" | "out" | "trace_out")
}

#[allow(unused_assignments)]
fn scan(src: &str) -> Vec<Finding> {
    let prod = production(src);
    let mut findings = Vec::new();
    let (mut fnn, mut arm): (Option<String>, Option<String>);
    fnn = None;
    arm = None;
    // (start index in `prod`, attr) of the current region, to cut bodies afterwards.
    let mut regions: Vec<(usize, String)> = Vec::new();
    let attr_of = |f: &Option<String>, a: &Option<String>| match (f, a) {
        (Some(f), Some(a)) => format!("{f}::{a}"),
        (Some(f), None) => f.clone(),
        _ => String::new(),
    };
    for (idx, (_, l)) in prod.iter().enumerate() {
        if let Some(n) = fn_name(l) {
            fnn = Some(n);
            arm = None;
            regions.push((idx, attr_of(&fnn, &arm)));
        } else if let Some(a) = arm_name(l) {
            arm = Some(a);
            regions.push((idx, attr_of(&fnn, &arm)));
        }
    }
    let body_of = |idx: usize| -> String {
        let pos = regions.iter().rposition(|(s, _)| *s <= idx).unwrap_or(0);
        let end = regions.get(pos + 1).map_or(prod.len(), |(s, _)| *s);
        prod[regions[pos].0..end].iter().map(|(_, t)| *t).collect::<Vec<_>>().join("\n")
    };
    let (mut fnn, mut arm): (Option<String>, Option<String>) = (None, None);
    for (idx, (n, l)) in prod.iter().enumerate() {
        if let Some(f) = fn_name(l) {
            fnn = Some(f);
            arm = None;
        } else if let Some(a) = arm_name(l) {
            arm = Some(a);
        }
        if l.trim_start().starts_with("//") {
            continue;
        }
        let attr = attr_of(&fnn, &arm);
        let mut push = |what: String| {
            findings.push(Finding { line: *n, attr: attr.clone(), what, body: body_of(idx) });
        };
        // `.get("key")`
        let mut rest = *l;
        while let Some(p) = rest.find(".get(\"") {
            let after = &rest[p + 6..];
            if let Some(e) = after.find("\")") {
                let key = &after[..e];
                if path_like(key) {
                    push(format!("param:{key}"));
                }
            }
            rest = &rest[p + 6..];
        }
        for name in ["resolve_fs_path_with_state", "resolve_fs_path"] {
            let call = format!("{name}(");
            let mut from = 0;
            while let Some(p) = l[from..].find(&call) {
                let at = from + p;
                let before_ok = at == 0 || !l.as_bytes()[at - 1].is_ascii_alphanumeric() && l.as_bytes()[at - 1] != b'_';
                let is_def = l.contains("fn resolve_fs_path") || l.contains("let resolve_fs_path =");
                if before_ok && !is_def {
                    push(format!("resolve:{name}"));
                }
                from = at + call.len();
            }
        }
        for op in ["write", "copy", "rename", "remove_file", "remove_dir_all", "remove_dir"] {
            let call = format!("fs::{op}(");
            if l.contains(&call) {
                push(format!("fs:{op}"));
            }
        }
        if l.contains("File::create") || l.contains("OpenOptions") {
            push("fs:create".to_string());
        }
    }
    findings
}

fn main_rs() -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join("main.rs")).expect("read src/main.rs")
}

fn check_table(found: &[Finding], table: &[(&str, &str, Class)], what: &str, only: impl Fn(&str) -> bool) {
    let got: BTreeSet<(String, String)> =
        found.iter().filter(|f| only(&f.what)).map(|f| (f.attr.clone(), f.what.clone())).collect();
    let known: BTreeSet<(String, String)> = table.iter().map(|(a, w, _)| (a.to_string(), w.to_string())).collect();
    let unclassified: Vec<_> = got.difference(&known).collect();
    let stale: Vec<_> = known.difference(&got).collect();
    assert!(
        unclassified.is_empty(),
        "{what}: a place that takes a path or writes a file is not classified in tests/no_writeback.rs. \
         Decide how it keeps a file the daemon was handed safe under --scratch (medium: go through \
         medium_path / scratch::open_folder; output: write_output; medium backing file: write_medium), \
         then add it: {unclassified:#?}"
    );
    assert!(stale.is_empty(), "{what}: rows with no such place any more: {stale:#?}");
}

#[test]
fn every_path_taking_door_is_classified_and_every_medium_door_goes_through_the_resolver() {
    let src = main_rs();
    let found = scan(&src);
    assert!(found.len() > 40, "the scan found {} places; the scanner is broken", found.len());
    check_table(&found, PATH_DOORS, "path doors", |w| w.starts_with("param:") || w.starts_with("resolve:"));

    for (attr, what, class) in PATH_DOORS {
        if *class != Medium {
            continue;
        }
        let f = found.iter().find(|f| f.attr == *attr && f.what == *what).expect("classified above");
        let through = f.body.contains("medium_path(")
            || f.body.contains("scratch::open_folder(")
            || f.body.contains("scratch::open_medium(");
        assert!(through, "{attr} (line {}) reads a medium path {what} but does not go through the resolver", f.line);
        // A medium door must not resolve the path some other way first and read that.
        assert!(
            !f.body.contains("resolve_fs_path_with_state(&st, &path_str)") && !f.body.contains("resolve_fs_path_with_state(&st, path_in)"),
            "{attr}: resolves a medium path by the plain resolver"
        );
    }
}

#[test]
fn a_file_is_written_only_by_the_two_write_helpers() {
    let src = main_rs();
    let found = scan(&src);
    check_table(&found, FS_MUTATIONS, "file mutations", |w| w.starts_with("fs:"));
    // Nothing that persists a medium may write by itself.
    for f in &found {
        if f.what == "fs:write" {
            let ok = matches!(
                f.attr.as_str(),
                "write_medium" | "write_output" | "main" | "dispatch_request::runtime/scenario_save"
            );
            assert!(ok, "{} (line {}) writes a file by itself", f.attr, f.line);
        }
    }
    // The persist functions use the medium helper.
    for name in ["persist_cart_for_eject", "persist_outgoing_disk_at", "autopersist_disk_at"] {
        let start = src.find(&format!("fn {name}(")).unwrap_or_else(|| panic!("fn {name} is gone"));
        let end = src[start..].find("\n}\n").map_or(src.len(), |e| start + e);
        assert!(src[start..end].contains("write_medium("), "{name} does not write through write_medium");
    }
}

// ════════════════════════════════════════════════════════════════════════════════
//  BEHAVIOURAL
// ════════════════════════════════════════════════════════════════════════════════

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

macro_rules! skip {
    ($($arg:tt)*) => {{
        eprintln!("SKIP no_writeback: {}. NOT green, NOT run.", format!($($arg)*));
        return;
    }};
}

fn rom_dir() -> Option<PathBuf> {
    let has = |p: &Path| p.join("kernal-901227-03.bin").exists();
    if let Ok(d) = std::env::var("TRX64_ROM_DIR") {
        let p = PathBuf::from(d);
        if has(&p) {
            return Some(p);
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = PathBuf::from(home).join(".trx64").join("roms");
        if has(&p) {
            return Some(p);
        }
    }
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for up in [4, 5] {
        let mut p = here.clone();
        for _ in 0..up {
            p = p.parent()?.to_path_buf();
        }
        let p = p.join("C64ReverseEngineeringMCP").join("resources").join("roms");
        if has(&p) {
            return Some(p);
        }
    }
    None
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A test's directory; removed when the test ends, whatever way.
struct Root(PathBuf);

impl std::ops::Deref for Root {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn root_dir(tag: &str) -> Root {
    let d = std::env::temp_dir().join(format!("trx64_nowb_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    Root(d)
}

fn start(root: &Path, port: u16, args: &[&str]) -> Daemon {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_trx64-daemon"));
    cmd.args(["--port", &port.to_string()]).args(args).current_dir(root).stdout(Stdio::null()).stderr(Stdio::null());
    if let Some(r) = rom_dir() {
        cmd.env("TRX64_ROM_DIR", r);
    }
    Daemon { child: cmd.spawn().expect("spawn trx64-daemon") }
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
    let req = json!({ "jsonrpc": "2.0", "id": 7, "method": method, "params": params });
    ws.send(Message::Text(req.to_string())).await.unwrap();
    while let Some(Ok(msg)) = ws.next().await {
        if let Message::Text(t) = msg {
            let v: J = serde_json::from_str(&t).unwrap();
            if v["id"] == 7 {
                return if v["error"].is_null() { Ok(v["result"].clone()) } else { Err(v["error"].to_string()) };
            }
        }
    }
    Err(format!("the connection ended before {method} was answered"))
}

async fn rpc(ws: &mut Ws, method: &str, params: J) -> J {
    call(ws, method, params).await.unwrap_or_else(|e| panic!("{method}: {e}"))
}

async fn mon(ws: &mut Ws, command: &str) -> String {
    let r = rpc(ws, "monitor/exec", json!({ "command": command })).await;
    r["output"].as_str().or_else(|| r["error"].as_str()).unwrap_or("").to_string()
}

async fn go(ws: &mut Ws, cycles: u64) {
    rpc(ws, "debug/run", json!({ "cycles": cycles, "pace": "warp", "wait": true })).await;
}

async fn peek(ws: &mut Ws, addr: u64) -> u8 {
    let r = rpc(ws, "session/read_memory", json!({ "ranges": [{ "addr": addr, "len": 1 }] })).await;
    let t = r["chunks"][0]["bytes"].as_str().unwrap_or_else(|| panic!("read_memory shape: {r}"));
    // One byte in base64: `XX==`.
    const T: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let v = |c: char| T.find(c).unwrap() as u32;
    let c: Vec<char> = t.chars().collect();
    (((v(c[0]) << 2) | (v(c[1]) >> 4)) & 0xff) as u8
}

fn sha(path: &Path) -> String {
    hex::encode(Sha256::digest(std::fs::read(path).unwrap()))
}

/// The hash of a whole tree: names and contents.
fn tree_sha(dir: &Path) -> String {
    let mut h = Sha256::new();
    let mut names: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).collect();
    names.sort();
    for p in names {
        h.update(p.file_name().unwrap().to_string_lossy().as_bytes());
        if p.is_dir() {
            h.update(tree_sha(&p).as_bytes());
        } else {
            h.update(std::fs::read(&p).unwrap());
        }
    }
    hex::encode(h.finalize())
}

// ── fixtures ─────────────────────────────────────────────────────────────────────

fn spt(t: usize) -> usize {
    match t {
        1..=17 => 21,
        18..=24 => 19,
        25..=30 => 18,
        _ => 17,
    }
}

/// A blank, formatted 35-track D64 the 1541 can SAVE to.
fn blank_d64(name: &[u8]) -> Vec<u8> {
    let mut d = vec![0u8; 174_848];
    let bam: usize = (1..18).map(spt).sum::<usize>() * 256;
    d[bam] = 18;
    d[bam + 1] = 1;
    d[bam + 2] = 0x41;
    for t in 1..=35usize {
        let n = spt(t);
        let used: u32 = if t == 18 { 0b11 } else { 0 };
        let free: u32 = ((1u32 << n) - 1) & !used;
        let e = bam + 4 + (t - 1) * 4;
        d[e] = free.count_ones() as u8;
        d[e + 1] = free as u8;
        d[e + 2] = (free >> 8) as u8;
        d[e + 3] = (free >> 16) as u8;
    }
    for i in 0..16 {
        d[bam + 0x90 + i] = *name.get(i).unwrap_or(&0xa0);
    }
    d[bam + 0xa0] = 0xa0;
    d[bam + 0xa1] = 0xa0;
    d[bam + 0xa2] = b'A';
    d[bam + 0xa3] = b'B';
    d[bam + 0xa4] = 0xa0;
    d[bam + 0xa5] = b'2';
    d[bam + 0xa6] = b'A';
    d[bam + 256 + 1] = 0xff;
    d
}

/// The names in the directory of a D64 (track 18 from sector 1).
fn d64_files(d: &[u8]) -> Vec<String> {
    let off = |t: usize, s: usize| ((1..t).map(spt).sum::<usize>() + s) * 256;
    let mut out = vec![];
    let (mut t, mut s) = (18usize, 1usize);
    for _ in 0..19 {
        let b = off(t, s);
        for i in 0..8 {
            let e = b + i * 32;
            if d[e + 2] & 0x80 != 0 {
                let name: Vec<u8> = d[e + 5..e + 21].iter().copied().take_while(|c| *c != 0xa0).collect();
                out.push(String::from_utf8_lossy(&name).to_string());
            }
        }
        if d[b] == 0 {
            break;
        }
        (t, s) = (d[b] as usize, d[b + 1] as usize);
    }
    out
}

/// `10 SAVE"0:T",<unit>` / `20 VERIFY"T",<unit>` / `30 POKE2,42`: the program writes, reads its
/// own write back through the drive (or the folder device), and only then marks `$0002`.
fn save_program(unit: u8) -> Vec<u8> {
    let lines: Vec<(u16, Vec<u8>)> = vec![
        (10, [&[0x94u8][..], format!("\"0:T\",{unit}").as_bytes()].concat()),
        (20, [&[0x95u8][..], format!("\"T\",{unit}").as_bytes()].concat()),
        (30, [&[0x97u8][..], b"2,42"].concat()),
    ];
    let mut prg = vec![0x01, 0x08];
    let mut addr = 0x0801u16;
    for (n, body) in &lines {
        let next = addr + 4 + body.len() as u16 + 1;
        prg.extend_from_slice(&next.to_le_bytes());
        prg.extend_from_slice(&n.to_le_bytes());
        prg.extend_from_slice(body);
        prg.push(0);
        addr = next;
    }
    prg.extend_from_slice(&[0, 0]);
    prg
}

fn b64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        s.push(T[(n >> 18) as usize & 63] as char);
        s.push(T[(n >> 12) as usize & 63] as char);
        s.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        s.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    s
}

/// An EasyFlash CRT whose ultimax boot code (in ROMH, so the chip being programmed is not the
/// chip being read) programs the byte at `$8100` of ROML to `$42` and spins.
fn ef_crt() -> Vec<u8> {
    let mut romh = vec![0xffu8; 0x2000];
    let code: &[u8] = &[
        0x78, // SEI
        0xa9, 0xaa, 0x8d, 0x55, 0x85, // LDA #$AA : STA $8555
        0xa9, 0x55, 0x8d, 0xaa, 0x82, // LDA #$55 : STA $82AA
        0xa9, 0xa0, 0x8d, 0x55, 0x85, // LDA #$A0 : STA $8555  (program)
        0xa9, 0x42, 0x8d, 0x00, 0x81, // LDA #$42 : STA $8100
        0x4c, 0x15, 0xe0, // JMP $E015
    ];
    romh[..code.len()].copy_from_slice(code);
    for v in [0x1ffa, 0x1ffc, 0x1ffe] {
        romh[v] = if v == 0x1ffc { 0x00 } else { 0x15 };
        romh[v + 1] = 0xe0;
    }
    let roml = vec![0xffu8; 0x2000];
    let mut v = Vec::new();
    v.extend_from_slice(b"C64 CARTRIDGE   ");
    v.extend_from_slice(&0x40u32.to_be_bytes());
    v.extend_from_slice(&0x0100u16.to_be_bytes());
    v.extend_from_slice(&32u16.to_be_bytes()); // EasyFlash
    v.extend_from_slice(&[1, 0]); // EXROM 1, GAME 0: ultimax
    v.extend_from_slice(&[0u8; 6]);
    let mut name = [0u8; 32];
    name[..8].copy_from_slice(b"NOWRITEB");
    v.extend_from_slice(&name);
    for (load, data) in [(0x8000u16, &roml), (0xa000u16, &romh)] {
        v.extend_from_slice(b"CHIP");
        v.extend_from_slice(&(0x10 + data.len() as u32).to_be_bytes());
        v.extend_from_slice(&0u16.to_be_bytes());
        v.extend_from_slice(&0u16.to_be_bytes());
        v.extend_from_slice(&load.to_be_bytes());
        v.extend_from_slice(&(data.len() as u16).to_be_bytes());
        v.extend_from_slice(data);
    }
    v
}

// ── the ways a daemon ends ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
enum End {
    /// `daemon/shutdown`.
    Shutdown,
    /// SIGTERM (exits through the same work).
    Sigterm,
    /// `--idle-exit`, with the socket left open and silent.
    Idle,
    /// SIGKILL: no exit work at all.
    Killed,
}

async fn end(d: &mut Daemon, ws: &mut Ws, how: End) {
    let wait = |d: &mut Daemon| {
        let until = Instant::now() + Duration::from_secs(40);
        loop {
            if let Ok(Some(_)) = d.child.try_wait() {
                return;
            }
            assert!(Instant::now() < until, "the daemon did not end ({how:?})");
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    match how {
        End::Shutdown => {
            let _ = call(ws, "daemon/shutdown", json!({})).await;
            wait(d);
        }
        End::Sigterm => {
            let st = Command::new("kill").args(["-TERM", &d.child.id().to_string()]).status().unwrap();
            assert!(st.success());
            wait(d);
        }
        End::Idle => wait(d),
        End::Killed => {
            d.child.kill().unwrap();
            wait(d);
        }
    }
}

// ── the ways a medium reaches the daemon ──────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
enum Way {
    MediaOpen,
    MediaMount,
    MediaIngress,
    MediaSwap,
    MonitorMount,
    /// The monitor's `swapcrt` (cartridges only).
    MonitorSwapcrt,
    /// `runtime/swap_disk_and_continue` (disks only).
    SwapDiskAndContinue,
}

async fn hand_over(ws: &mut Ws, way: Way, kind: &str, src: &Path, decoy: &Path) {
    let p = src.to_string_lossy().to_string();
    match way {
        Way::MediaOpen => {
            rpc(ws, "media/open", json!({ "path": p })).await;
        }
        Way::MediaMount => {
            rpc(ws, "media/mount", json!({ "path": p })).await;
        }
        Way::MediaIngress => {
            rpc(ws, "media/ingress", json!({ "kind": if kind == "crt" { "crt" } else { "disk" }, "path": p })).await;
        }
        Way::MediaSwap => {
            rpc(ws, "media/open", json!({ "path": decoy.to_string_lossy() })).await;
            go(ws, 1_000_000).await;
            rpc(ws, "media/swap", json!({ "path": p })).await;
        }
        Way::MonitorMount => {
            let out = mon(ws, &format!("mount {p}")).await;
            assert!(out.contains("MOUNT"), "{out}");
        }
        Way::MonitorSwapcrt => {
            rpc(ws, "media/open", json!({ "path": decoy.to_string_lossy() })).await;
            go(ws, 1_000_000).await;
            let out = mon(ws, &format!("swapcrt \"{p}\"")).await;
            assert!(out.contains("swapped"), "{out}");
        }
        Way::SwapDiskAndContinue => {
            let r = rpc(ws, "runtime/swap_disk_and_continue", json!({ "path": p })).await;
            assert!(r.get("error").is_none(), "{r}");
        }
    }
}

fn decoy_crt() -> Vec<u8> {
    let mut c = ef_crt();
    // The same cartridge with its boot code replaced by a spin: it never writes.
    let at = c.len() - 0x2000;
    c[at..at + 4].copy_from_slice(&[0x4c, 0x00, 0xe0, 0xea]);
    c
}

/// Run until `$0002` holds 42 (the program saw its write through the drive), at most `max` cycles.
async fn until_marked(ws: &mut Ws, max: u64) {
    let mut ran = 0;
    while ran < max {
        if peek(ws, 2).await == 42 {
            return;
        }
        go(ws, 500_000).await;
        ran += 500_000;
    }
    panic!("the program did not mark `$0002` in {max} cycles: it did not save and verify");
}

fn only_copy(scratch: &Path, base: &str) -> PathBuf {
    let hits: Vec<PathBuf> = std::fs::read_dir(scratch)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().unwrap().to_string_lossy().ends_with(&format!("-{base}")))
        .collect();
    assert_eq!(hits.len(), 1, "the scratch dir holds one copy of {base}: {hits:?}");
    hits.into_iter().next().unwrap()
}

/// S5 for a D64: hand it over by `way`, SAVE to it, end the daemon `how`, and the source is
/// byte-identical. The session saw its write (VERIFY read it back through the drive), and the
/// copy holds it after a graceful end.
async fn disk_case(tag: &str, way: Way, how: End) {
    let Some(_) = rom_dir() else { skip!("no ROMs ({tag})") };
    let root = root_dir(tag);
    let (src, decoy) = (root.join("work.d64"), root.join("decoy.d64"));
    std::fs::write(&src, blank_d64(b"WORK")).unwrap();
    std::fs::write(&decoy, blank_d64(b"DECOY")).unwrap();
    let (before, decoy_before) = (sha(&src), sha(&decoy));
    let scratch = root.join("scratch");
    let port = free_port();
    let idle = "3";
    let mut args = vec!["--scratch", scratch.to_str().unwrap()];
    if how == End::Idle {
        args.extend(["--idle-exit", idle]);
    }
    let mut d = start(&root, port, &args);
    let mut ws = connect(port).await;
    let s = rpc(&mut ws, "session/state", json!({})).await;
    assert_eq!(s["noWriteback"], true, "{s}");
    assert!(s["scratchDir"].as_str().unwrap().ends_with("scratch"), "{s}");

    go(&mut ws, 3_000_000).await; // to BASIC
    hand_over(&mut ws, way, "disk", &src, &decoy).await;
    go(&mut ws, 500_000).await;
    rpc(&mut ws, "runtime/run_prg", json!({ "bytes_b64": b64(&save_program(8)) })).await;
    until_marked(&mut ws, 40_000_000).await;

    let state = rpc(&mut ws, "session/state", json!({})).await;
    let mounted = state["media"]["disk"]["path"].as_str().unwrap_or("").to_string();
    assert!(
        Path::new(&mounted).starts_with(std::fs::canonicalize(&scratch).unwrap()),
        "{way:?}: the disk in the drive is a copy in the scratch dir, not the source: {mounted}"
    );
    assert_eq!(sha(&src), before, "{way:?}: source changed while the daemon ran");

    end(&mut d, &mut ws, how).await;
    assert_eq!(sha(&src), before, "{way:?}/{how:?}: the source D64 changed");
    assert_eq!(sha(&decoy), decoy_before, "{way:?}/{how:?}: the decoy D64 changed");
    if how != End::Killed {
        let copy = only_copy(&scratch, "work.d64");
        let files = d64_files(&std::fs::read(&copy).unwrap());
        assert!(files.iter().any(|f| f == "T"), "{way:?}/{how:?}: the copy holds the session's SAVE: {files:?}");
    }
}

/// S5 for an EasyFlash CRT the program flash-writes.
async fn crt_case(tag: &str, way: Way, how: End) {
    let Some(_) = rom_dir() else { skip!("no ROMs ({tag})") };
    let root = root_dir(tag);
    let (src, decoy) = (root.join("work.crt"), root.join("decoy.crt"));
    std::fs::write(&src, ef_crt()).unwrap();
    std::fs::write(&decoy, decoy_crt()).unwrap();
    let (before, decoy_before) = (sha(&src), sha(&decoy));
    let scratch = root.join("scratch");
    let port = free_port();
    let mut args = vec!["--scratch", scratch.to_str().unwrap()];
    if how == End::Idle {
        args.extend(["--idle-exit", "3"]);
    }
    let mut d = start(&root, port, &args);
    let mut ws = connect(port).await;
    go(&mut ws, 2_000_000).await;
    hand_over(&mut ws, way, "crt", &src, &decoy).await;
    go(&mut ws, 2_000_000).await;

    // The session saw its write: the flash byte reads back as programmed.
    assert_eq!(peek(&mut ws, 0x8100).await, 0x42, "{way:?}: the session reads its own flash write");
    let state = rpc(&mut ws, "session/state", json!({})).await;
    let mounted = state["media"]["cart"]["path"].as_str().unwrap_or("").to_string();
    assert!(
        Path::new(&mounted).starts_with(std::fs::canonicalize(&scratch).unwrap()),
        "{way:?}: the cartridge is a copy in the scratch dir, not the source: {mounted}"
    );
    assert_eq!(sha(&src), before, "{way:?}: source changed while the daemon ran");

    end(&mut d, &mut ws, how).await;
    assert_eq!(sha(&src), before, "{way:?}/{how:?}: the source CRT changed");
    assert_eq!(sha(&decoy), decoy_before, "{way:?}/{how:?}: the decoy CRT changed");
    if how != End::Killed {
        let copy = std::fs::read(only_copy(&scratch, "work.crt")).unwrap();
        let orig = ef_crt();
        assert_eq!(copy.len(), orig.len());
        let diff: Vec<(usize, u8, u8)> = (0..copy.len()).filter(|&i| copy[i] != orig[i]).map(|i| (i, orig[i], copy[i])).collect();
        assert_eq!(diff.len(), 1, "{way:?}/{how:?}: the copy holds exactly the flash write: {diff:?}");
        assert_eq!((diff[0].1, diff[0].2), (0xff, 0x42));
    }
}

macro_rules! cases {
    ($($name:ident: $f:ident($way:expr, $end:expr);)*) => {
        $(
            #[tokio::test(flavor = "multi_thread")]
            async fn $name() {
                $f(stringify!($name), $way, $end).await;
            }
        )*
    };
}

cases! {
    // every door a D64 can come through (graceful end)
    s5_disk_media_open: disk_case(Way::MediaOpen, End::Shutdown);
    s5_disk_media_mount: disk_case(Way::MediaMount, End::Shutdown);
    s5_disk_media_ingress: disk_case(Way::MediaIngress, End::Shutdown);
    s5_disk_media_swap: disk_case(Way::MediaSwap, End::Shutdown);
    s5_disk_monitor_mount: disk_case(Way::MonitorMount, End::Shutdown);
    s5_disk_swap_disk_and_continue: disk_case(Way::SwapDiskAndContinue, End::Shutdown);
    // every way the daemon ends (one door)
    s5_disk_ends_by_sigterm: disk_case(Way::MediaOpen, End::Sigterm);
    s5_disk_ends_by_idle_exit: disk_case(Way::MediaOpen, End::Idle);
    s5_disk_ends_killed: disk_case(Way::MediaOpen, End::Killed);
    // every door a cartridge can come through
    s5_crt_media_open: crt_case(Way::MediaOpen, End::Shutdown);
    s5_crt_media_mount: crt_case(Way::MediaMount, End::Shutdown);
    s5_crt_media_ingress: crt_case(Way::MediaIngress, End::Shutdown);
    s5_crt_media_swap: crt_case(Way::MediaSwap, End::Shutdown);
    s5_crt_monitor_mount: crt_case(Way::MonitorMount, End::Shutdown);
    s5_crt_monitor_swapcrt: crt_case(Way::MonitorSwapcrt, End::Shutdown);
    s5_crt_ends_by_sigterm: crt_case(Way::MediaOpen, End::Sigterm);
    s5_crt_ends_by_idle_exit: crt_case(Way::MediaOpen, End::Idle);
    s5_crt_ends_killed: crt_case(Way::MediaOpen, End::Killed);
}

/// A folder served as a device, which the program SAVEs into: the source tree is untouched,
/// the copy holds the file, and the session reads it back (VERIFY).
#[tokio::test(flavor = "multi_thread")]
async fn s5_folder_device_attach() {
    let Some(_) = rom_dir() else { skip!("no ROMs") };
    let root = root_dir("folder");
    let src = root.join("host");
    std::fs::create_dir_all(src.join("sub")).unwrap();
    std::fs::write(src.join("README"), b"hello").unwrap();
    std::fs::write(src.join("sub").join("X"), b"x").unwrap();
    let before = tree_sha(&src);
    let scratch = root.join("scratch");
    let port = free_port();
    let mut d = start(&root, port, &["--scratch", scratch.to_str().unwrap()]);
    let mut ws = connect(port).await;
    go(&mut ws, 3_000_000).await;
    rpc(&mut ws, "device/folder_attach", json!({ "unit": 10, "path": src.to_string_lossy() })).await;
    rpc(&mut ws, "runtime/run_prg", json!({ "bytes_b64": b64(&save_program(10)) })).await;
    until_marked(&mut ws, 40_000_000).await;
    assert_eq!(tree_sha(&src), before, "the source folder changed while the daemon ran");
    end(&mut d, &mut ws, End::Shutdown).await;
    assert_eq!(tree_sha(&src), before, "the source folder changed");
    let copies: Vec<PathBuf> = std::fs::read_dir(&scratch).unwrap().flatten().map(|e| e.path()).collect();
    assert_eq!(copies.len(), 1, "{copies:?}");
    let names: Vec<String> =
        std::fs::read_dir(&copies[0]).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
    assert!(names.iter().any(|n| n.eq_ignore_ascii_case("T") || n.to_lowercase().starts_with("t.")), "the copy holds the SAVE: {names:?}");
}

/// A snapshot that embeds a disk: undumping it puts the media in the scratch dir, not in a
/// `_media` folder beside the snapshot, and nothing it holds is written back.
#[tokio::test(flavor = "multi_thread")]
async fn s5_snapshot_undump_with_embedded_media() {
    let Some(_) = rom_dir() else { skip!("no ROMs") };
    let root = root_dir("undump");
    let src = root.join("work.d64");
    std::fs::write(&src, blank_d64(b"WORK")).unwrap();
    let before = sha(&src);
    let snap = root.join("snaps").join("s.c64re");
    std::fs::create_dir_all(snap.parent().unwrap()).unwrap();

    // Daemon A makes the snapshot (its disk, with the SAVE, is embedded).
    let (scratch_a, scratch_b) = (root.join("scratch_a"), root.join("scratch_b"));
    let port = free_port();
    let mut a = start(&root, port, &["--scratch", scratch_a.to_str().unwrap()]);
    let mut ws = connect(port).await;
    go(&mut ws, 3_000_000).await;
    rpc(&mut ws, "media/open", json!({ "path": src.to_string_lossy() })).await;
    rpc(&mut ws, "runtime/run_prg", json!({ "bytes_b64": b64(&save_program(8)) })).await;
    until_marked(&mut ws, 40_000_000).await;
    rpc(&mut ws, "snapshot/dump", json!({ "path": snap.to_string_lossy() })).await;
    end(&mut a, &mut ws, End::Shutdown).await;
    assert_eq!(sha(&src), before);
    let snap_before = sha(&snap);

    // Daemon B undumps it by every door that reads a snapshot.
    for (i, door) in ["media/open", "snapshot/undump"].iter().enumerate() {
        let port = free_port();
        let mut b = start(&root, port, &["--scratch", scratch_b.to_str().unwrap()]);
        let mut ws = connect(port).await;
        go(&mut ws, 1_000_000).await;
        rpc(&mut ws, door, json!({ "path": snap.to_string_lossy() })).await;
        let state = rpc(&mut ws, "session/state", json!({})).await;
        let mounted = state["media"]["disk"]["path"].as_str().unwrap_or("").to_string();
        assert!(
            Path::new(&mounted).starts_with(std::fs::canonicalize(&scratch_b).unwrap()),
            "{door}: the embedded disk is a file in the scratch dir: {mounted}"
        );
        let r = rpc(&mut ws, "media/persist", json!({})).await;
        assert!(r["path"].as_str().map_or(true, |p| Path::new(p).starts_with(std::fs::canonicalize(&scratch_b).unwrap())), "{r}");
        end(&mut b, &mut ws, if i == 0 { End::Shutdown } else { End::Sigterm }).await;
        assert_eq!(sha(&snap), snap_before, "{door}: the snapshot file changed");
        assert!(!root.join("snaps").join("s_media").exists(), "{door}: a `_media` folder appeared beside the snapshot");
        assert_eq!(sha(&src), before);
    }
}

/// The write side on its own: bytes handed over with a backing path that names no file (so no
/// door had a file to copy) are persisted into the scratch dir, and the path is never created.
#[tokio::test(flavor = "multi_thread")]
async fn s5_a_backing_path_with_no_file_behind_it_is_still_not_written() {
    let Some(_) = rom_dir() else { skip!("no ROMs") };
    let root = root_dir("sink");
    let ghost = root.join("ghost.d64");
    let scratch = root.join("scratch");
    let port = free_port();
    let mut d = start(&root, port, &["--scratch", scratch.to_str().unwrap()]);
    let mut ws = connect(port).await;
    go(&mut ws, 3_000_000).await;
    rpc(&mut ws, "media/ingress", json!({ "kind": "disk", "bytes_b64": b64(&blank_d64(b"GHOST")), "path": ghost.to_string_lossy() })).await;
    let r = rpc(&mut ws, "media/persist", json!({})).await;
    assert_eq!(r["written"], true, "{r}");
    assert!(
        Path::new(r["path"].as_str().unwrap()).starts_with(std::fs::canonicalize(&scratch).unwrap()),
        "the persist went into the scratch dir: {r}"
    );
    end(&mut d, &mut ws, End::Shutdown).await;
    assert!(!ghost.exists(), "the named path was created");
}

/// S6: without the option nothing changes — `media/persist` writes the original.
#[tokio::test(flavor = "multi_thread")]
async fn s6_without_the_option_persist_writes_the_original() {
    let Some(_) = rom_dir() else { skip!("no ROMs") };
    let root = root_dir("s6");
    let src = root.join("work.d64");
    std::fs::write(&src, blank_d64(b"WORK")).unwrap();
    let before = sha(&src);
    let port = free_port();
    let mut d = start(&root, port, &[]);
    let mut ws = connect(port).await;
    let s = rpc(&mut ws, "session/state", json!({})).await;
    assert_eq!(s["noWriteback"], false, "{s}");
    assert!(s["scratchDir"].is_null(), "{s}");
    go(&mut ws, 3_000_000).await;
    rpc(&mut ws, "media/open", json!({ "path": src.to_string_lossy() })).await;
    rpc(&mut ws, "runtime/run_prg", json!({ "bytes_b64": b64(&save_program(8)) })).await;
    until_marked(&mut ws, 40_000_000).await;
    let r = rpc(&mut ws, "media/persist", json!({})).await;
    assert_eq!(r["written"], true, "{r}");
    assert_eq!(Path::new(r["path"].as_str().unwrap()), src, "{r}");
    assert_ne!(sha(&src), before, "the original holds the write");
    assert!(d64_files(&std::fs::read(&src).unwrap()).iter().any(|f| f == "T"));
    end(&mut d, &mut ws, End::Shutdown).await;
}

/// The option cannot be given twice and a directory it cannot make is a clean exit 2.
#[test]
fn an_unusable_scratch_dir_is_a_clean_refusal() {
    let root = root_dir("badscratch");
    let file = root.join("a-file");
    std::fs::write(&file, b"x").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_trx64-daemon"))
        .args(["--port", &free_port().to_string(), "--scratch", file.join("sub").to_str().unwrap()])
        .current_dir(&*root)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stderr));
}
