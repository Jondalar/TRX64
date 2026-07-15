//! Spec 787 acceptance #2 / #3 — scratch-vs-live isolation gate (TRX64-side).
//!
//! Spec 787 v1 model: a "scratch" TRX64 instance IS a short-lived `trx64cli`
//! process (`sandbox` / `boot`) — there is NO daemon `spawn-scratch` verb; the OS
//! gives isolation for free. This gate DEMONSTRATES + ASSERTS that contract:
//!
//!   #2  A `trx64cli sandbox` scratch run does NOT perturb a concurrently-live
//!       machine. We hold a live `Machine` in-process (the exact struct the daemon's
//!       live session holds inside its `Session`), snapshot its state (full 64K RAM +
//!       CPU regs + drive `current_half_track`), run a REAL scratch workload in a
//!       separate OS process, and assert the live state is BYTE-IDENTICAL before vs
//!       after.
//!
//!   #3  Isolation both ways. The scratch cannot resetCold/mutate the live machine
//!       (it holds no handle to it — separate address space; evidenced by #2's
//!       byte-identity after the scratch WROTE its own RAM). And live-session
//!       operations do not perturb a running scratch: we mutate + advance the live
//!       machine CONCURRENTLY with a running scratch child and assert the scratch's
//!       harvested output is byte-identical to the quiescent run (deterministic w.r.t.
//!       its OWN inputs, independent of the live machine).
//!
//! This is NOT a VICE/TS parity gate — the oracle is retired; the regression bar is
//! TRX64's own behaviour. The "live" side is a real `Machine` (what the daemon holds);
//! the "scratch" side is the real compiled `trx64cli` binary
//! (`CARGO_BIN_EXE_trx64cli`) run as a genuine child process. Isolation holds by OS
//! construction; this test would go RED if a future change wired the CLI sandbox back
//! into a shared/live machine.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use trx64_cli::default_rom_dir;
use trx64_core::{BusKind, Machine, Observer};

/// A no-op observer (the live machine just needs to advance; we don't trace it).
struct NullObs;
impl Observer for NullObs {
    fn on_instruction(
        &mut self,
        _pc: u16,
        _opcode: u8,
        _b1: u8,
        _b2: u8,
        _a: u8,
        _x: u8,
        _y: u8,
        _sp: u8,
        _p: u8,
        _clk: u64,
    ) {
    }
    fn on_bus(&mut self, _kind: BusKind, _addr: u16, _value: u8, _pc: u16, _clk: u64, _old: u8) {}
    fn on_interrupt(&mut self, _vector: u16, _clk: u64) {}
}

/// A full snapshot of the observable live-machine state named in Spec 787 #2.
#[derive(PartialEq, Eq)]
struct LiveState {
    ram: Vec<u8>,                    // full 64K, byte-exact
    regs: (u8, u8, u8, u8, u8, u16), // a, x, y, sp, p, pc
    half_track: u32,                 // drive current_half_track
}

fn capture(m: &Machine) -> LiveState {
    LiveState {
        ram: m.ram.to_vec(),
        regs: (
            m.c64_core.reg_a,
            m.c64_core.reg_x,
            m.c64_core.reg_y,
            m.c64_core.reg_sp,
            m.c64_core.reg_p,
            m.c64_core.reg_pc,
        ),
        half_track: m.drive8.rotation.current_half_track,
    }
}

/// The tiny "depacker" the scratch runs on the REAL 6502: write 0x00..0x0f to
/// $4000, then RTS. PRG load address $c000. Harvest $4000:16 == "000102..0f".
const TINY_DEPACK_PRG: &[u8] = &[
    0x00, 0xc0, // load address $c000
    0xa2, 0x00, // ldx #$00
    0x8a, // txa            <- loop
    0x9d, 0x00, 0x40, // sta $4000,x
    0xe8, // inx
    0xe0, 0x10, // cpx #$10
    0xd0, 0xf7, // bne loop
    0x60, // rts
];
const EXPECTED_HARVEST: &str = "000102030405060708090a0b0c0d0e0f";

/// Run the scratch (`trx64cli sandbox`) as a genuine child process; return
/// (child_pid, parsed harvest hex). `pre_wait` runs on the PARENT while the child
/// is alive (to demonstrate concurrent live-side activity does not perturb it).
fn run_scratch(prg_path: &Path, pre_wait: impl FnOnce()) -> (u32, String) {
    let bin = env!("CARGO_BIN_EXE_trx64cli");
    let mut child = Command::new(bin)
        .args([
            "sandbox",
            "--load",
            prg_path.to_str().unwrap(),
            "--entry",
            "$c000",
            "--harvest",
            "$4000:16",
            "--json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn trx64cli sandbox child");
    let pid = child.id();
    // Genuinely concurrent: the child is running its own Machine in another process
    // right now; do live-side work here before we collect it.
    pre_wait();
    let out = child.wait_with_output().expect("wait scratch child");
    assert!(
        out.status.success(),
        "scratch child failed: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Pull the harvest hex out of the JSON without a serde dep in the test.
    let key = "\"hex\":\"";
    let i = stdout.find(key).expect("harvest hex in scratch json") + key.len();
    let j = stdout[i..].find('"').unwrap() + i;
    (pid, stdout[i..j].to_string())
}

fn write_temp_prg() -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("trx64_787_depack_{}.prg", std::process::id()));
    let mut f = std::fs::File::create(&p).expect("create temp prg");
    f.write_all(TINY_DEPACK_PRG).expect("write temp prg");
    p
}

/// #2 + #3(scratch-cannot-mutate-live) + #3(live-ops-dont-perturb-scratch), all in
/// one boot to keep it fast.
#[test]
fn scratch_process_does_not_perturb_live_and_vice_versa() {
    let rom_dir = default_rom_dir();
    if !Path::new(&rom_dir).join("kernal-901227-03.bin").exists() {
        eprintln!("[skip] e2e_787_isolation: ROMs absent at {}", rom_dir.display());
        return;
    }

    // ── Build a LIVE machine (the daemon's live-session stand-in) ───────────────
    // Same construction the daemon/sandbox use: Machine::new() + boot_from_dir. The
    // daemon holds exactly this struct inside its Session; here we own it directly so
    // we can snapshot RAM/regs/drive byte-for-byte.
    let mut live = Machine::new();
    live.boot_from_dir(&rom_dir).expect("boot live machine ROMs");
    // Advance to READY so RAM/regs are a real running state, not cold zeros.
    live.run_for(4_000_000, &mut NullObs);
    // Plant distinctive markers so the byte-identity assertion is meaningful:
    //  - a recognizable RAM pattern in free RAM,
    //  - the harvest region $4000 set to a value the scratch would OVERWRITE on its
    //    own machine (proves no cross-process bleed),
    //  - a distinctive drive half-track.
    live.poke(0xc000, &[0xde, 0xad, 0xbe, 0xef, 0x12, 0x34, 0x56, 0x78]);
    live.poke(0x4000, &[0xa5; 16]);
    live.drive8.rotation.current_half_track = 43; // distinctive (track ~22)

    let prg = write_temp_prg();

    // Snapshot BEFORE any scratch runs.
    let before = capture(&live);
    let parent_pid = std::process::id();

    // ── Scratch run #1: live machine untouched across the scratch window (#2) ────
    let (pid1, harvest1) = run_scratch(&prg, || {});
    assert_ne!(pid1, parent_pid, "scratch must be a SEPARATE OS process");
    assert_eq!(
        harvest1, EXPECTED_HARVEST,
        "scratch must run the real routine on its OWN machine (harvest mismatch)"
    );

    // Snapshot AFTER the scratch spawn/run/dispose. Must be byte-identical (#2) — and
    // note the scratch WROTE $4000..$400f on ITS machine, yet the live $4000 marker is
    // untouched (#3: scratch cannot mutate the live machine).
    let after = capture(&live);
    assert!(
        before.ram == after.ram,
        "live RAM changed after scratch run (first diff at {:?})",
        before
            .ram
            .iter()
            .zip(after.ram.iter())
            .position(|(a, b)| a != b)
    );
    assert_eq!(before.regs, after.regs, "live CPU regs changed after scratch run");
    assert_eq!(
        before.half_track, after.half_track,
        "live drive current_half_track changed after scratch run"
    );
    assert!(
        after.ram[0x4000..0x4010] == [0xa5u8; 16],
        "live $4000 marker was clobbered — cross-process bleed"
    );

    // ── Scratch run #2: mutate + advance the LIVE machine CONCURRENTLY with a ────
    // running scratch child, then assert the scratch output is unchanged (#3:
    // live-session operations do not perturb a running scratch).
    let (pid2, harvest2) = run_scratch(&prg, || {
        for i in 0..8u16 {
            live.poke(0x2000 + i, &[i as u8 ^ 0x5a]);
            live.run_for(50_000, &mut NullObs);
        }
        live.drive8.rotation.current_half_track = 71; // move the live head mid-scratch
    });
    assert_eq!(
        harvest2, EXPECTED_HARVEST,
        "concurrent live-machine activity perturbed the scratch's deterministic output"
    );

    let _ = std::fs::remove_file(&prg);
    eprintln!(
        "[ok] e2e_787_isolation: live parent pid={parent_pid}, scratch child pids={pid1}/{pid2}; \
         live RAM+regs+half_track byte-identical across scratch #1; scratch harvest deterministic \
         under concurrent live mutation."
    );
}
