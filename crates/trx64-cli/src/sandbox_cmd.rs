//! `trx64cli sandbox` — one-shot real-core execution sandbox (Spec 787 v1 + 788).
//!
//! Boot a FRESH machine (this process = one throwaway scratch instance, Spec 787
//! v1: the CLI already runs its own in-process machine, no daemon), load bytes,
//! plant a tiny entry stub, run the title's OWN routine to a sentinel, and harvest
//! a RAM slice. The routine runs on the AUTHORITATIVE 6502 (`trx64-core`), not the
//! TS `Cpu6502` shadow — so a depacker that touches banking/IO executes for real.
//!
//! The stub (Boris/KoronisRift pattern, minus the title-specific filename setup):
//!   sei; lda #io; sta $01; jsr entry; jmp self
//! `jsr entry` pushes its own return address, so when `entry` RTSs the PC lands on
//! the `jmp self` at `stub+8`, which we breakpoint — no manual stack push needed.
//! Banking ($01) is set via a real CPU store so the memconfig updates (a raw poke
//! of $01 would not). The harvest reads the raw 64K `ram` field (ignores banking =
//! the unpacked bytes as written), matching the TS sandbox's write-map semantics.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::json;
use trx64_core::{Machine, NullSink, RunStop};

use crate::disasm_cmd::parse_addr;

const DEFAULT_STUB_ADDR: u16 = 0x02a7; // free RAM ($02a7-$02ff), untouched by our no-boot path
const DEFAULT_IO: u8 = 0x37; // KERNAL+BASIC+IO visible (standard post-reset config)
const DEFAULT_CYC_CAP: u64 = 100_000_000;
const DEFAULT_INSTR_CAP: u64 = 40_000_000;

/// A `--load FILE@ADDR` spec. `addr = None` ⇒ FILE is a `.prg` whose 2-byte header
/// supplies the load address.
#[derive(Clone)]
pub struct SandboxLoad {
    pub file: PathBuf,
    pub addr: Option<u16>,
}

/// Parse `FILE@ADDR` (ADDR hex: `$c000` / `0xc000` / `c000`). No `@ADDR` ⇒ PRG header.
pub fn parse_load(s: &str) -> Result<SandboxLoad, String> {
    match s.rsplit_once('@') {
        Some((f, a)) => Ok(SandboxLoad { file: PathBuf::from(f), addr: Some(parse_addr(a)?) }),
        None => Ok(SandboxLoad { file: PathBuf::from(s), addr: None }),
    }
}

/// Parse `ADDR:LEN` — ADDR hex, LEN decimal or `0x`/`$` hex.
fn parse_harvest(s: &str) -> Result<(u16, usize), String> {
    let (a, l) = s
        .split_once(':')
        .ok_or_else(|| format!("bad --harvest '{s}' (want ADDR:LEN, e.g. $4000:0x800)"))?;
    let addr = parse_addr(a)?;
    let len = if let Some(h) = l.strip_prefix("0x").or_else(|| l.strip_prefix('$')) {
        usize::from_str_radix(h, 16)
    } else {
        l.parse::<usize>()
    }
    .map_err(|_| format!("bad --harvest length '{l}' (decimal or 0x-hex)"))?;
    Ok((addr, len))
}

/// Parse `ADDR=VAL` — a zero-page byte to seed before the run (ADDR 00-ff hex,
/// VAL hex). Depackers take their src/dst pointers here (e.g. `--zp $fb=$00`).
fn parse_zp(s: &str) -> Result<(u16, u8), String> {
    let (a, v) = s
        .split_once('=')
        .ok_or_else(|| format!("bad --zp '{s}' (want ADDR=VAL, e.g. $fb=$00)"))?;
    let addr = parse_addr(a)?;
    if addr > 0xff {
        return Err(format!("--zp address ${addr:04x} is not zero-page (00-ff)"));
    }
    let vh = v.strip_prefix('$').or_else(|| v.strip_prefix("0x")).unwrap_or(v);
    let val = u8::from_str_radix(vh, 16).map_err(|_| format!("bad --zp value '{v}' (hex byte)"))?;
    Ok((addr, val))
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// The parsed, defaulted sandbox request.
pub struct SandboxArgs {
    pub rom_dir: PathBuf,
    pub loads: Vec<SandboxLoad>,
    pub entry: u16,
    pub harvest_addr: u16,
    pub harvest_len: usize,
    pub zp: Vec<(u16, u8)>,
    pub sentinel: Option<u16>,
    pub io: u8,
    pub stub_addr: u16,
    pub cyc_cap: u64,
    pub instr_cap: u64,
    pub json: bool,
}

/// Thin CLI adapter: parse the raw clap fields, apply defaults, run.
#[allow(clippy::too_many_arguments)]
pub fn run_sandbox_cli(
    rom_dir: &Path,
    load: &[String],
    entry: u16,
    harvest: &str,
    zp: &[String],
    sentinel: Option<u16>,
    io: Option<&str>,
    stub_addr: Option<u16>,
    cyc_cap: Option<u64>,
    instr_cap: Option<u64>,
    json: bool,
) -> Result<String, String> {
    let loads = load.iter().map(|s| parse_load(s)).collect::<Result<Vec<_>, _>>()?;
    let (harvest_addr, harvest_len) = parse_harvest(harvest)?;
    let zp = zp.iter().map(|s| parse_zp(s)).collect::<Result<Vec<_>, _>>()?;
    let io = match io {
        Some(s) => {
            let hex = s.strip_prefix('$').or_else(|| s.strip_prefix("0x")).unwrap_or(s);
            u8::from_str_radix(hex, 16).map_err(|_| format!("bad --io '{s}' (hex byte, e.g. $37)"))?
        }
        None => DEFAULT_IO,
    };
    run_sandbox(&SandboxArgs {
        rom_dir: rom_dir.to_path_buf(),
        loads,
        entry,
        harvest_addr,
        harvest_len,
        zp,
        sentinel,
        io,
        stub_addr: stub_addr.unwrap_or(DEFAULT_STUB_ADDR),
        cyc_cap: cyc_cap.unwrap_or(DEFAULT_CYC_CAP),
        instr_cap: instr_cap.unwrap_or(DEFAULT_INSTR_CAP),
        json,
    })
}

pub fn run_sandbox(args: &SandboxArgs) -> Result<String, String> {
    let mut m = Machine::new();
    m.boot_from_dir(&args.rom_dir)
        .map_err(|e| format!("boot ROMs from {}: {e:?}", args.rom_dir.display()))?;

    // Apply --load blobs (the PRG header supplies the address when @ADDR is omitted).
    for ld in &args.loads {
        let data = std::fs::read(&ld.file).map_err(|e| format!("read {}: {e}", ld.file.display()))?;
        let (addr, body): (u16, &[u8]) = match ld.addr {
            Some(a) => (a, &data[..]),
            None => {
                if data.len() < 2 {
                    return Err(format!(
                        "{}: too short for a PRG (2-byte load-address header); use FILE@ADDR",
                        ld.file.display()
                    ));
                }
                ((data[0] as u16) | ((data[1] as u16) << 8), &data[2..])
            }
        };
        // A load that overlaps the entry stub would clobber it (or vice-versa).
        let s = args.stub_addr as u32;
        let (l0, l1) = (addr as u32, addr as u32 + body.len() as u32);
        if l0 < s + 11 && s < l1 {
            eprintln!(
                "warning: load ${:04x}..+{} overlaps the entry stub ${:04x}..+11 — move it with --stub-addr",
                addr,
                body.len(),
                args.stub_addr
            );
        }
        m.poke(addr, body);
    }

    // Seed zero-page bytes (depacker src/dst pointers etc.). Note: $00/$01 are the
    // CPU port — $01 is set by the stub via --io, so a --zp $01 would be overwritten.
    for (addr, val) in &args.zp {
        m.poke(*addr, &[*val]);
    }

    // Entry stub: sei; lda #io; sta $01; jsr entry; jmp self. `entry`'s RTS returns
    // to the jmp-self at stub+8 (jsr pushed it), which we breakpoint.
    let s = args.stub_addr;
    let ret = s.wrapping_add(8);
    let stub = [
        0x78, // sei
        0xa9, args.io, // lda #io
        0x85, 0x01, // sta $01
        0x20, (args.entry & 0xff) as u8, (args.entry >> 8) as u8, // jsr entry
        0x4c, (ret & 0xff) as u8, (ret >> 8) as u8, // jmp ret (self-loop)
    ];
    m.poke(s, &stub);

    // PC gotcha (Spec 788 §6): the full-machine run reads c64_core.reg_pc; the
    // legacy set_pc writes only cpu6510. Set BOTH cores' PC to the stub.
    m.c64_core.reg_pc = s;
    m.cpu6510.reg_pc = s;

    let mut bp: HashSet<u16> = HashSet::new();
    bp.insert(ret); // routine finished (RTS'd back to the stub)
    if let Some(extra) = args.sentinel {
        bp.insert(extra);
    }

    let clk0 = m.c64_core.clk;
    let mut obs = NullSink;
    let stop = m.run_for_full_capped_dbg(
        args.cyc_cap,
        args.instr_cap,
        Some(&bp),
        None,
        None,
        &mut obs,
        |_, _, _, _, _, _, _| {},
    );
    let cycles = m.c64_core.clk.wrapping_sub(clk0);

    let stop_reason = match stop {
        RunStop::Breakpoint(pc) if pc == ret => "sentinel_rts",
        RunStop::Breakpoint(_) => "sentinel",
        RunStop::Completed => "completed",
        RunStop::CycleBudget => "cycle_budget",
        _ => "capped",
    };
    let hit = matches!(stop, RunStop::Breakpoint(_));

    // Harvest the raw RAM slice (ignores banking = the unpacked bytes as written).
    let start = args.harvest_addr as usize;
    let end = (start + args.harvest_len).min(0x1_0000);
    let slice = &m.ram[start..end];

    if args.json {
        let out = json!({
            "ok": hit,
            "stopReason": stop_reason,
            "pc": m.c64_core.reg_pc,
            "cycles": cycles,
            "harvest": { "addr": args.harvest_addr, "len": slice.len(), "hex": hex(slice) },
        });
        serde_json::to_string(&out).map_err(|e| e.to_string())
    } else {
        Ok(format!(
            "sandbox: stop={stop_reason} pc=${:04x} cycles={cycles}  harvest ${:04x}..+{} = {}",
            m.c64_core.reg_pc,
            args.harvest_addr,
            slice.len(),
            hex(slice)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_load_with_and_without_addr() {
        let a = parse_load("foo.bin@$2000").unwrap();
        assert_eq!(a.addr, Some(0x2000));
        let b = parse_load("game.prg").unwrap();
        assert_eq!(b.addr, None);
    }

    #[test]
    fn parse_harvest_dec_and_hex() {
        assert_eq!(parse_harvest("$4000:0x800").unwrap(), (0x4000, 0x800));
        assert_eq!(parse_harvest("c000:16").unwrap(), (0xc000, 16));
        assert!(parse_harvest("nope").is_err());
    }

    #[test]
    fn parse_zp_ok_and_bounds() {
        assert_eq!(parse_zp("$fb=$00").unwrap(), (0xfb, 0x00));
        assert_eq!(parse_zp("fd=40").unwrap(), (0xfd, 0x40));
        assert!(parse_zp("$1000=$00").is_err()); // not zero-page
        assert!(parse_zp("nope").is_err());
    }
}
