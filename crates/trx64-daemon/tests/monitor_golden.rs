//! Spec 864 §11.1 — the golden transcript.
//!
//! The monitor is about to move into its own crate (`trx64-monitor`) with the daemon as
//! its first host. That is an extraction: not one verb may print anything different
//! afterwards. Nothing else in the suite would notice if one did — the verbs are checked
//! for the things they assert, never for the bytes they emit — so this test exists to
//! record those bytes BEFORE the move and fail on any drift after it.
//!
//! It drives `monitor/exec` through the real dispatch, on a real machine booted from the
//! real ROMs, and compares the whole transcript against `tests/golden/monitor.txt`. The
//! reply's marked address spans (Spec 804) are part of the record, because C64RE's symbol
//! join hangs off them and a lost mark would otherwise be invisible here.
//!
//! Re-bless deliberately, and read the diff first:
//!
//!     TRX64_GOLDEN_BLESS=1 cargo test -p trx64-daemon --test monitor_golden
//!
//! ROMs: `TRX64_ROM_DIR`, else `~/.trx64/roms`, else the sibling workbench's
//! `resources/roms`. Without them the test says so and SKIPS rather than passing — a
//! transcript nobody produced proves nothing.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};
use trx64_daemon::{create_embedded_state, dispatch, Request, SharedState};

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
    // The workbench beside us keeps a set; the daemon's own `rom_dir()` looks here too.
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for up in [4, 5] {
        let mut p = here.clone();
        for _ in 0..up {
            p = p.parent()?.to_path_buf();
        }
        let p = p
            .join("C64ReverseEngineeringMCP")
            .join("resources")
            .join("roms");
        if has(&p) {
            return Some(p);
        }
    }
    None
}

fn exec(state: &SharedState, id: u64, command: &str) -> Value {
    let req = Request {
        jsonrpc: "2.0".into(),
        id: json!(id),
        method: "monitor/exec".into(),
        params: json!({ "command": command }),
    };
    let res = dispatch(req, state);
    match (res.result, res.error) {
        (Some(v), _) => v,
        (None, Some(e)) => json!({ "error": e.message }),
        _ => json!({ "error": "no result and no error" }),
    }
}

/// The reply, rendered for the record: the text, the marked spans, and the machine the
/// monitor says it is on (Spec 863 — captured per reply, so a model switch shows here).
fn render(reply: &Value) -> String {
    let mut out = String::new();
    // The wire shape: `output` on success, `error` on refusal, plus `spans`, `machine`
    // and an optional modal `prompt` (main.rs, the "monitor/exec" arm).
    if let Some(err) = reply.get("error").and_then(|v| v.as_str()) {
        out.push_str(&scrub(err));
        if !out.ends_with('\n') {
            out.push('\n');
        }
    } else {
        let text = reply
            .get("output")
            .and_then(|v| v.as_str())
            .unwrap_or("(no output key)");
        out.push_str(&scrub(text));
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    if let Some(p) = reply.get("prompt").and_then(|v| v.as_str()) {
        out.push_str(&format!("   prompt: {p}\n"));
    }
    // Spec 804 — the marked spans are part of the record.
    if let Some(spans) = reply.get("spans").and_then(|v| v.as_array()) {
        if !spans.is_empty() {
            out.push_str(&format!("   spans: {}\n", spans.len()));
            for sp in spans {
                let line = sp.get("line").and_then(|v| v.as_u64()).unwrap_or(0);
                let start = sp.get("start").and_then(|v| v.as_u64()).unwrap_or(0);
                let end = sp.get("end").and_then(|v| v.as_u64()).unwrap_or(0);
                let addr = sp.get("addr").and_then(|v| v.as_u64()).unwrap_or(0);
                let space = sp.get("space").and_then(|v| v.as_str()).unwrap_or("?");
                let role = sp.get("role").and_then(|v| v.as_str()).unwrap_or("?");
                let lens = sp.get("lens").and_then(|v| v.as_str()).unwrap_or("-");
                out.push_str(&format!(
                    "     L{line} {start}..{end} ${addr:04x} {space}/{role}/{lens}\n"
                ));
            }
        }
    }
    // The banking context every reply carries (device + what the CPU port and the
    // expansion port lines are doing).
    if let Some(m) = reply.get("machine") {
        let dev = m.get("device").and_then(|v| v.as_str()).unwrap_or("?");
        let dir = m.get("cpuPortDirection").and_then(|v| v.as_u64()).unwrap_or(0);
        let val = m.get("cpuPortValue").and_then(|v| v.as_u64()).unwrap_or(0);
        let exrom = m.get("exrom").and_then(|v| v.as_u64()).unwrap_or(9);
        let game = m.get("game").and_then(|v| v.as_u64()).unwrap_or(9);
        let bank = m
            .get("cartBank")
            .and_then(|v| v.as_u64())
            .map(|b| b.to_string())
            .unwrap_or_else(|| "-".into());
        out.push_str(&format!(
            "   machine: {dev} port ${dir:02x}/${val:02x} exrom {exrom} game {game} bank {bank}\n"
        ));
    }
    out
}

/// Everything that is true of a RUN rather than of the monitor: wall-clock, absolute
/// paths, host names, a cycle counter that depends on when the test started.
fn scrub(s: &str) -> String {
    let mut out = String::new();
    for line in s.lines() {
        let mut l = line.to_string();
        if let Some(home) = std::env::var_os("HOME") {
            l = l.replace(&home.to_string_lossy().to_string(), "~");
        }
        // ISO timestamps and ms durations say when, not what.
        let l = regex_lite_replace(&l, r"\d{4}-\d{2}-\d{2}T[\d:.]+Z?", "<time>");
        let l = regex_lite_replace(&l, r"\b\d+(\.\d+)? ?ms\b", "<ms>");
        out.push_str(&l);
        out.push('\n');
    }
    out
}

/// A dependency-free stand-in for the two patterns above: this crate has no regex and the
/// transcript does not justify adding one.
fn regex_lite_replace(s: &str, pattern: &str, with: &str) -> String {
    // Only the two shapes used above are recognised; anything else is returned unchanged.
    if pattern.starts_with(r"\d{4}-\d{2}-\d{2}T") {
        let bytes: Vec<char> = s.chars().collect();
        let mut out = String::new();
        let mut i = 0;
        while i < bytes.len() {
            let looks_like_date = i + 10 < bytes.len()
                && bytes[i].is_ascii_digit()
                && bytes[i + 1].is_ascii_digit()
                && bytes[i + 2].is_ascii_digit()
                && bytes[i + 3].is_ascii_digit()
                && bytes[i + 4] == '-'
                && bytes[i + 7] == '-'
                && bytes[i + 10] == 'T';
            if looks_like_date {
                out.push_str(with);
                while i < bytes.len() && !bytes[i].is_whitespace() {
                    i += 1;
                }
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        return out;
    }
    if pattern.contains("ms") {
        let mut out = String::new();
        let mut num = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c.is_ascii_digit() || (c == '.' && !num.is_empty()) {
                num.push(c);
                continue;
            }
            if !num.is_empty() {
                let is_ms = (c == 'm' && chars.peek() == Some(&'s'))
                    || (c == ' ' && chars.peek() == Some(&'m'));
                if is_ms {
                    out.push_str(with);
                    // consume "ms" (and the space we may have eaten)
                    if c == ' ' {
                        chars.next();
                    }
                    chars.next();
                    num.clear();
                    continue;
                }
                out.push_str(&num);
                num.clear();
            }
            out.push(c);
        }
        out.push_str(&num);
        return out;
    }
    s.to_string()
}

/// Every verb the help text lists, with arguments that answer the same way twice.
///
/// Deliberately fixed: the addresses are ROM (the KERNAL reset vector region and the
/// screen editor) so the bytes are the same on every machine with the same ROM set, and
/// nothing here advances the machine except the stepping block, which advances it by a
/// counted number of instructions from a known PC.
fn script() -> Vec<&'static str> {
    vec![
        // Orientation
        "help",
        "r",
        "model",
        // Memory, every lens
        "m e000 e03f",
        "m rom e000 e00f",
        "m ram 0400 040f",
        "m io d000 d00f",
        "bank",
        "bank rom",
        "bank",
        "bank cpu",
        // Disassembly — the marked spans live here
        "d fce2 fcf0",
        "d e000 e010",
        "df fce2 8",
        "screen",
        // Static analysis over known ROM
        "h e000 e100 a2 ff",
        "c e000 e010 e000",
        // Writes into RAM, then read them back
        "wr 0400 01 02 03 04",
        "m 0400 040f",
        "f 0410 041f aa",
        "m 0410 041f",
        "t 0400 040f 0500",
        "m 0500 050f",
        // `a` arms the assemble cursor even when given an instruction inline, so the
        // empty line that LEAVES the mode is part of the verb, not decoration. Without
        // it every later command is swallowed as an instruction — which is what the
        // first blessed transcript showed, and the reason this file exists.
        "a 0600 lda #$01",
        "",
        "d 0600 0602",
        // And the mode itself, entered and left deliberately.
        "a 0610",
        "inx",
        "iny",
        "",
        "d 0610 0612",
        // Registers
        "r a=42",
        "r",
        "r a=00",
        // Breakpoints and observers, listed rather than hit
        "bk",
        "bk fce2",
        "bk",
        "obs probe when exec $fce2 do log",
        "o",
        "obs probe del",
        "o",
        "bk -fce2",
        "bk",
        // Flow, backtrace, the panels
        "flow",
        "bt",
        "focus",
        // Stepping — counted, from a known PC
        "r pc=fce2",
        "z",
        "z",
        "n",
        "r",
        "sd 4",
        // The device selector, and the read-inspect gate behind it
        "device",
        "device drive8",
        "r",
        "wr 0400 01",
        "device c64",
        "device nosuchdevice",
        // I/O and the bus
        "io",
        "io 1",
        "iec",
        // The verbs a host may not have (they must still answer on the daemon)
        "map",
        "chis",
        "whowrote d020 3",
        // Unknown verb, and an empty line
        "nosuchverb",
        "",
    ]
}

#[test]
fn the_monitor_prints_what_it_printed_before_the_extraction() {
    let Some(roms) = rom_dir() else {
        eprintln!(
            "SKIP monitor_golden: no ROM set found (set TRX64_ROM_DIR, or install into ~/.trx64/roms). \
             NOT green, NOT run."
        );
        return;
    };

    let state: SharedState = create_embedded_state(&roms).expect("boot the machine from ROMs");

    // A deterministic starting point: the machine is cold-reset and paused by
    // `create_embedded_state` (a non-streaming daemon stays paused), so the transcript
    // starts from the reset state rather than from wherever a boot happened to land.
    let mut transcript = String::new();
    transcript.push_str("# Spec 864 §11.1 — monitor golden transcript\n");
    transcript.push_str("# Produced by tests/monitor_golden.rs against a cold-reset machine.\n");
    transcript.push_str("# Re-bless with TRX64_GOLDEN_BLESS=1 and read the diff first.\n\n");

    for (i, cmd) in script().iter().enumerate() {
        transcript.push_str(&format!("$ {cmd}\n"));
        let reply = exec(&state, i as u64 + 1, cmd);
        transcript.push_str(&render(&reply));
        transcript.push('\n');
    }

    let golden = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join("monitor.txt");

    if std::env::var("TRX64_GOLDEN_BLESS").is_ok() {
        std::fs::create_dir_all(golden.parent().unwrap()).unwrap();
        std::fs::write(&golden, &transcript).unwrap();
        eprintln!("blessed {} ({} bytes)", golden.display(), transcript.len());
        return;
    }

    let expected = match std::fs::read_to_string(&golden) {
        Ok(s) => s,
        Err(_) => panic!(
            "no golden transcript at {} — produce it with TRX64_GOLDEN_BLESS=1 BEFORE moving any code",
            golden.display()
        ),
    };

    if expected != transcript {
        // Report the FIRST divergence and its window, not statistics (doctrine rule 5).
        let e: Vec<&str> = expected.lines().collect();
        let g: Vec<&str> = transcript.lines().collect();
        let at = e
            .iter()
            .zip(g.iter())
            .position(|(a, b)| a != b)
            .unwrap_or(e.len().min(g.len()));
        let lo = at.saturating_sub(6);
        let mut msg = String::new();
        msg.push_str(&format!(
            "the monitor's output changed, first at line {}:\n\n",
            at + 1
        ));
        for i in lo..at {
            msg.push_str(&format!("   {}\n", e.get(i).unwrap_or(&"")));
        }
        msg.push_str(&format!("  -{}\n", e.get(at).unwrap_or(&"(end)")));
        msg.push_str(&format!("  +{}\n", g.get(at).unwrap_or(&"(end)")));
        for i in at + 1..(at + 6).min(e.len().max(g.len())) {
            msg.push_str(&format!("   {}\n", g.get(i).unwrap_or(&"")));
        }
        panic!("{msg}");
    }
}

/// Arc is used through SharedState; keep the import honest.
#[allow(dead_code)]
fn _shared_state_is_an_arc(s: &SharedState) -> &Arc<std::sync::Mutex<trx64_daemon::State>> {
    s
}
