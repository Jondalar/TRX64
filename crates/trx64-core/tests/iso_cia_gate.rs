//! The iso CIA corpus (`tools/oracle/corpus/cia/*`), replayed on the CPU-isolated CIA bus
//! and compared record for record with the golden trace the TS oracle recorded.
//!
//! The oracle is gone, so the goldens are evidence of what the old runtime did, not of
//! what is right. A case whose expectation differs from VICE's `core/ciacore.c` is listed
//! in `VICE_DIVERGES` with the reason, and the comparison stops being exact there: the
//! replay must then show VICE's value, which the case's own assertion states.
//!
//! Each scenario is `session/create {exerciser}` + `wr 0800 <bytes>` + `r pc=0800` +
//! `session/run {cycles}` with the `c64-cpu` and `memory` domains traced. The trace has
//! two families: `cpu` (one per retired instruction) and `ram` (bus records: `access` 1 =
//! an IO write, 0 = an IO read, 129 = a RAM write with its old byte).

use serde_json::Value;
use trx64_core::{BusKind, Machine, Observer};

const CORPUS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tools/oracle/corpus/cia");

#[derive(Debug, Clone, PartialEq, Eq)]
enum Rec {
    Cpu { cycle: u64, pc: u16, op: u8, a: u8, x: u8, y: u8, sp: u8, p: u8, b1: u8, b2: u8 },
    Ram { cycle: u64, addr: u16, value: u8, pc: u16, access: u8, old: u8 },
}

#[derive(Default)]
struct Rec0(Vec<Rec>);
impl Observer for Rec0 {
    #[allow(clippy::too_many_arguments)]
    fn on_instruction(&mut self, pc: u16, op: u8, b1: u8, b2: u8, a: u8, x: u8, y: u8, sp: u8, p: u8, clk: u64) {
        self.0.push(Rec::Cpu { cycle: clk, pc, op, a, x, y, sp, p, b1, b2 });
    }
    fn on_bus(&mut self, kind: BusKind, addr: u16, value: u8, pc: u16, clk: u64, old: u8) {
        let io = (0xd000..0xe000).contains(&addr);
        let access = match kind {
            BusKind::Write if io => 1,
            BusKind::Write => 129,
            BusKind::Read if io => 0,
            _ => return,
        };
        let old = if access == 129 { old } else { 0 };
        self.0.push(Rec::Ram { cycle: clk, addr, value, pc, access, old });
    }
    fn on_interrupt(&mut self, _vector: u16, _clk: u64) {}
}

fn golden_records(g: &Value) -> Vec<Rec> {
    let u = |f: &Value, k: &str| f[k].as_u64().unwrap_or(0);
    g["trace"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            let f = &r["fields"];
            let cycle = r["cycle"].as_u64().unwrap();
            match r["family"].as_str().unwrap() {
                "cpu" => Rec::Cpu {
                    cycle,
                    pc: u(f, "pc") as u16,
                    op: u(f, "opcode") as u8,
                    a: u(f, "a") as u8,
                    x: u(f, "x") as u8,
                    y: u(f, "y") as u8,
                    sp: u(f, "sp") as u8,
                    p: u(f, "p") as u8,
                    b1: u(f, "b1") as u8,
                    b2: u(f, "b2") as u8,
                },
                "ram" => Rec::Ram {
                    cycle,
                    addr: u(f, "addr") as u16,
                    value: u(f, "value") as u8,
                    pc: u(f, "pc") as u16,
                    access: u(f, "access") as u8,
                    old: u(f, "old") as u8,
                },
                other => panic!("unknown family {other}"),
            }
        })
        .collect()
}

/// Replay one scenario: the bytes of its `wr 0800`, the cycles of its `session/run`.
fn replay(scenario: &Value) -> Vec<Rec> {
    let mut prog = Vec::new();
    let mut cycles = 0;
    for s in scenario["steps"].as_array().unwrap() {
        let p = &s["params"];
        if let Some(cmd) = p["command"].as_str() {
            let toks: Vec<&str> = cmd.split_whitespace().collect();
            if toks[0] == "wr" {
                assert_eq!(toks[1], "0800");
                prog = toks[2..].iter().map(|t| u8::from_str_radix(t, 16).unwrap()).collect();
            }
        }
        if s["method"] == "session/run" {
            cycles = p["cycles"].as_u64().unwrap();
        }
    }
    let mut m = Machine::new();
    m.poke(0x0800, &prog);
    m.set_pc(0x0800);
    let mut rec = Rec0::default();
    m.run_for_cia(cycles, &mut rec);
    rec.0
}

/// The corpus cases whose golden is not VICE, with the first record that moves and why.
/// Empty until a case is shown to diverge.
const VICE_DIVERGES: &[(&str, &str)] = &[];

#[test]
fn the_iso_cia_corpus_replays_on_the_one_core() {
    let mut names: Vec<String> = std::fs::read_dir(CORPUS)
        .expect("corpus dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".json") && !n.ends_with(".golden.json"))
        .collect();
    names.sort();
    assert_eq!(names.len(), 7, "{names:?}");
    let mut report = Vec::new();
    for n in &names {
        let stem = n.trim_end_matches(".json");
        let scenario: Value = serde_json::from_str(&std::fs::read_to_string(format!("{CORPUS}/{n}")).unwrap()).unwrap();
        let golden: Value =
            serde_json::from_str(&std::fs::read_to_string(format!("{CORPUS}/{stem}.golden.json")).unwrap()).unwrap();
        let want = golden_records(&golden);
        let got = replay(&scenario);
        let first = want.iter().zip(&got).position(|(a, b)| a != b);
        match first {
            None if want.len() == got.len() => {}
            None => report.push(format!("{stem}: {} golden records, {} replayed", want.len(), got.len())),
            Some(i) => report.push(format!(
                "{stem}: first divergence at record {i}:\n    golden {:?}\n    got    {:?}",
                want[i], got[i]
            )),
        }
    }
    let unexplained: Vec<&String> =
        report.iter().filter(|r| !VICE_DIVERGES.iter().any(|(n, _)| r.starts_with(&format!("{n}:")))).collect();
    for r in &report {
        eprintln!("{r}");
    }
    assert!(unexplained.is_empty(), "unexplained divergences from the golden:\n{unexplained:#?}");
}
