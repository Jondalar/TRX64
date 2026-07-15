//! snapshot_roundtrip_fidelity.rs — Spec 792.2, the round-trip fidelity GATE.
//!
//! Ground truth for "a snapshot restores exactly". For each scenario:
//!   1. Build a machine in a NON-TRIVIAL running state.
//!   2. Capture → the `.c64re` container (`write_native_snapshot`) AND a ring
//!      checkpoint (`checkpoint_ring`).
//!   3. Restore into a FRESH machine via BOTH paths.
//!   4. BYTE-IDENTICAL STATE ASSERT: 64K RAM, CPU regs+clk, CIA1+CIA2, VIC,
//!      `cart.get_state()`, drive half-track + writable image, keyboard, cpu-port.
//!      Every differing field is listed (a gap the gate enumerated).
//!   5. N-CYCLE-IDENTICAL CONTINUATION ASSERT: run the original-continued machine
//!      AND the restored machine the same N cycles under a hashing observer
//!      (PC+opcode+A per retired instruction); the hashes must match. This catches
//!      behaviour-affecting state that is not in the struct compare.
//!
//! Scenarios: (i) plain (booted to READY), (ii) banked EasyFlash cart at bank != 0,
//! (iii) drive-active (synthetic disk, drive seeking). ROMs come from the C64RE
//! `resources/roms` dir; the whole gate SKIPS cleanly when they are absent (a fresh
//! clone with no ROMs). All fixtures are synthetic — no game/sample filenames.
//!
//! Run: cargo test -p trx64-core --test snapshot_roundtrip_fidelity -- --nocapture

use trx64_core::c64re_snapshot::{
    capture_cart_state, capture_cia, capture_cpu, capture_iec, capture_int_status, capture_sid,
    capture_vic, capture_runtime_checkpoint, restore_runtime_checkpoint,
};
use trx64_core::cart::BankInfo;
use trx64_core::checkpoint_ring::RuntimeCheckpointRing;
use trx64_core::drive::{DiskImage, DiskKind};
use trx64_core::drive_snapshot::{capture_drive1541, capture_drive_disk_image};
use trx64_core::native_snapshot::{
    read_native_snapshot, write_native_snapshot, WriteNativeSnapshotArgs,
};
use trx64_core::{BusKind, Machine, Observer};

const ROM_DIR: &str = "/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP/resources/roms";
const N_CYCLES: u64 = 200_000;

fn roms_present() -> bool {
    let d = std::path::Path::new(ROM_DIR);
    d.join("kernal-901227-03.bin").exists()
        && d.join("basic-901226-01.bin").exists()
        && d.join("chargen-901225-01.bin").exists()
}

// ── a hashing observer (PC + opcode + A per retired instruction) ────────────────

struct HashSink {
    h: u64,
    n: u64,
}
impl HashSink {
    fn new() -> Self {
        HashSink { h: 0xcbf29ce484222325, n: 0 } // FNV-1a offset basis
    }
    #[inline]
    fn fold(&mut self, v: u64) {
        self.h ^= v;
        self.h = self.h.wrapping_mul(0x100000001b3);
    }
}
impl Observer for HashSink {
    #[inline]
    #[allow(clippy::too_many_arguments)]
    fn on_instruction(
        &mut self,
        pc: u16,
        opcode: u8,
        _b1: u8,
        _b2: u8,
        a: u8,
        _x: u8,
        _y: u8,
        _sp: u8,
        _p: u8,
        _clk: u64,
    ) {
        self.fold(pc as u64);
        self.fold(opcode as u64);
        self.fold(a as u64);
        self.n += 1;
    }
    #[inline]
    fn on_bus(&mut self, _: BusKind, _: u16, _: u8, _: u16, _: u64, _: u8) {}
    #[inline]
    fn on_interrupt(&mut self, _: u16, _: u64) {}
}

fn run_and_hash(m: &mut Machine, cycles: u64) -> (u64, u64) {
    let mut sink = HashSink::new();
    m.run_for_full(cycles, &mut sink, |_, _, _, _, _, _, _| {});
    (sink.h, sink.n)
}

// ── the state fingerprint + field-level diff ────────────────────────────────────

/// Recursively push every differing leaf path between two JSON values into `out`.
fn json_leaf_diffs(prefix: &str, a: &serde_json::Value, b: &serde_json::Value, out: &mut Vec<String>) {
    use serde_json::Value;
    match (a, b) {
        (Value::Object(ma), Value::Object(mb)) => {
            let mut keys: Vec<&String> = ma.keys().chain(mb.keys()).collect();
            keys.sort();
            keys.dedup();
            for k in keys {
                let p = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                match (ma.get(k), mb.get(k)) {
                    (Some(va), Some(vb)) => json_leaf_diffs(&p, va, vb, out),
                    (a, b) => out.push(format!("{p}: {a:?} vs {b:?}")),
                }
            }
        }
        (Value::Array(aa), Value::Array(ba)) => {
            if aa.len() != ba.len() {
                out.push(format!("{prefix}.len: {} vs {}", aa.len(), ba.len()));
            }
            for (i, (va, vb)) in aa.iter().zip(ba.iter()).enumerate() {
                if va != vb {
                    json_leaf_diffs(&format!("{prefix}[{i}]"), va, vb, out);
                }
            }
        }
        _ => {
            if a != b {
                out.push(format!("{prefix}: {a} vs {b}"));
            }
        }
    }
}

fn chip_json(m: &Machine) -> serde_json::Value {
    serde_json::json!({
        "cpu": serde_json::to_value(capture_cpu(m)).unwrap(),
        "cia1": serde_json::to_value(capture_cia(&m.cia1)).unwrap(),
        "cia2": serde_json::to_value(capture_cia(&m.cia2)).unwrap(),
        "sid": serde_json::to_value(capture_sid(m)).unwrap(),
        "iec": serde_json::to_value(capture_iec(m)).unwrap(),
        "cpuIntStatus": serde_json::to_value(capture_int_status(m)).unwrap(),
        "vic": serde_json::to_value(capture_vic(m)).unwrap(),
        "cartState": m.cartridge.as_ref()
            .map(|c| serde_json::to_value(capture_cart_state(c.as_ref())).unwrap())
            .unwrap_or(serde_json::Value::Null),
        "cpuPortDirection": m.port_dir,
        "cpuPortValue": m.port_data,
        "keyboard": m.keyboard.pressed_keys(),
        "driveHalfTrack": m.drive8.rotation.current_half_track,
        "driveHeadOffset": m.drive8.rotation.gcr_head_offset,
        "drivePc": m.drive8.core.reg_pc,
        "driveA": m.drive8.core.reg_a,
        "driveX": m.drive8.core.reg_x,
        "driveY": m.drive8.core.reg_y,
        "driveSp": m.drive8.core.reg_sp,
        "driveClk": m.drive8.core.clk,
    })
}

/// The FULL field-level diff between two machines. Returns the list of differing
/// fields (empty = byte-identical machine state on the compared surface).
fn diff_machines(a: &mut Machine, b: &mut Machine) -> Vec<String> {
    let mut out = Vec::new();

    // 64K RAM (first divergence).
    if a.ram != b.ram {
        for i in 0..0x10000usize {
            if a.ram[i] != b.ram[i] {
                out.push(format!("ram[{:#06x}]: {:#04x} vs {:#04x}", i, a.ram[i], b.ram[i]));
                break;
            }
        }
    }

    // Chip register/state surface (leaf-level).
    json_leaf_diffs("", &chip_json(a), &chip_json(b), &mut out);

    // Drive RAM (2KB, first divergence).
    for addr in 0..0x800u16 {
        if a.drive8.drive_ram_read(addr) != b.drive8.drive_ram_read(addr) {
            out.push(format!(
                "driveRam[{:#06x}]: {:#04x} vs {:#04x}",
                addr,
                a.drive8.drive_ram_read(addr),
                b.drive8.drive_ram_read(addr)
            ));
            break;
        }
    }

    // Cart writable image (flash DATA array) — only when both carry one.
    let clk_a = a.c64_core.clk;
    let clk_b = b.c64_core.clk;
    let wa = a.cartridge.as_mut().and_then(|c| c.writable_image(clk_a));
    let wb = b.cartridge.as_mut().and_then(|c| c.writable_image(clk_b));
    match (wa, wb) {
        (Some(x), Some(y)) => {
            if x != y {
                let first = x.iter().zip(y.iter()).position(|(p, q)| p != q);
                out.push(format!("cartWritableImage differs (first byte idx {:?})", first));
            }
        }
        (None, None) => {}
        (a, b) => out.push(format!("cartWritableImage presence: {} vs {}", a.is_some(), b.is_some())),
    }

    out
}

// ── capture the full checkpoint tree from a live machine ────────────────────────

fn capture_full(m: &mut Machine, cart_bytes: Option<&[u8]>) -> serde_json::Value {
    let drive1541 = capture_drive1541(&mut m.drive8);
    let disk_blob = capture_drive_disk_image(&m.drive8);
    let clk = m.c64_core.clk;
    let cart_flash: Option<Vec<u8>> = m
        .cartridge
        .as_mut()
        .filter(|c| c.persists_writable_state())
        .and_then(|c| c.writable_image(clk));
    capture_runtime_checkpoint(
        m,
        "",
        "",
        Some(&drive1541),
        disk_blob.as_deref(),
        cart_bytes,
        cart_flash.as_deref(),
    )
}

/// Round-trip `checkpoint` through the `.c64re` binary container.
fn roundtrip_native(checkpoint: &serde_json::Value) -> serde_json::Value {
    let bytes = write_native_snapshot(WriteNativeSnapshotArgs {
        checkpoint: checkpoint.clone(),
        schema_version: 1,
        media: vec![],
        runtime_version: "trx64/792-gate".into(),
        machine_model: "c64-pal".into(),
        provenance: None,
        pc: 0,
        cycle: 0,
    });
    read_native_snapshot(&bytes).expect("read .c64re container").checkpoint
}

/// Round-trip `checkpoint` through the checkpoint ring (765).
fn roundtrip_ring(checkpoint: &serde_json::Value) -> serde_json::Value {
    let mut ring = RuntimeCheckpointRing::new();
    let r = ring.capture(checkpoint.clone(), 1, 1).expect("ring capture");
    ring.restore_snapshot(&r.id).expect("ring restore_snapshot")
}

fn fresh_booted(disk: Option<&DiskImage>) -> Machine {
    let mut m = Machine::new();
    m.boot_from_dir(std::path::Path::new(ROM_DIR)).expect("boot ROMs");
    if let Some(d) = disk {
        m.drive8.attach_disk(d.clone());
    }
    m
}

fn report_state(scenario: &str, path: &str, gaps: &[String]) {
    if gaps.is_empty() {
        eprintln!("  [{scenario}/{path}] STATE: byte-identical (no gaps)");
    } else {
        eprintln!("  [{scenario}/{path}] STATE GAPS ({}):", gaps.len());
        for g in gaps {
            eprintln!("      - {g}");
        }
    }
}

fn report_cont(scenario: &str, path: &str, restored: (u64, u64), orig: (u64, u64)) -> bool {
    let ok = restored == orig;
    if ok {
        eprintln!("  [{scenario}/{path}] CONTINUATION: hash match over {} instr", orig.1);
    } else {
        eprintln!(
            "  [{scenario}/{path}] CONTINUATION MISMATCH: restored {:#018x}/{} vs orig {:#018x}/{}",
            restored.0, restored.1, orig.0, orig.1
        );
    }
    ok
}

/// Full gate for one scenario: build → capture → restore via `.c64re` AND ring →
/// assert byte-identical state (restored vs the TRUE original, pre-run) + N-cycle-
/// identical continuation. Panics with the enumerated field list on any gap.
fn run_scenario(scenario: &str, mut orig: Machine, cart_bytes: Option<Vec<u8>>, disk: Option<DiskImage>) {
    eprintln!("\n=== scenario: {scenario} ===");

    // Capture the checkpoint tree from the original (still at its non-trivial state).
    let checkpoint = capture_full(&mut orig, cart_bytes.as_deref());
    let native_cp = roundtrip_native(&checkpoint);
    let ring_cp = roundtrip_ring(&checkpoint);

    // Restore both paths into fresh booted machines.
    let mut m_native = fresh_booted(disk.as_ref());
    restore_runtime_checkpoint(&mut m_native, &native_cp).expect("restore .c64re");
    let mut m_ring = fresh_booted(disk.as_ref());
    restore_runtime_checkpoint(&mut m_ring, &ring_cp).expect("restore ring");

    // 1) BYTE-IDENTICAL STATE: each restored machine vs the TRUE original (which is
    //    still at its pre-run state — nothing has been run forward yet).
    let gaps_native = diff_machines(&mut orig, &mut m_native);
    let gaps_ring = diff_machines(&mut orig, &mut m_ring);
    report_state(scenario, ".c64re", &gaps_native);
    report_state(scenario, "ring", &gaps_ring);

    // 2) N-CYCLE-IDENTICAL CONTINUATION: run the original-continued machine AND each
    //    restored machine the same N cycles under the hashing observer.
    let orig_hash = run_and_hash(&mut orig, N_CYCLES);
    eprintln!(
        "  [{scenario}] original continuation hash {:#018x} over {} instr",
        orig_hash.0, orig_hash.1
    );
    let cont_native = report_cont(scenario, ".c64re", run_and_hash(&mut m_native, N_CYCLES), orig_hash);
    let cont_ring = report_cont(scenario, "ring", run_and_hash(&mut m_ring, N_CYCLES), orig_hash);

    // Hard asserts: both paths must be byte-identical AND N-cycle-identical.
    assert!(gaps_native.is_empty(), "[{scenario}/.c64re] state gaps: {gaps_native:?}");
    assert!(gaps_ring.is_empty(), "[{scenario}/ring] state gaps: {gaps_ring:?}");
    assert!(cont_native, "[{scenario}/.c64re] continuation diverged");
    assert!(cont_ring, "[{scenario}/ring] continuation diverged");
}

// ── fixtures (synthetic) ────────────────────────────────────────────────────────

/// A minimal valid CRT header (0x40 bytes) + N CHIP packets. `hw` = hardware type,
/// `exrom`/`game` = the header lines. Each chip is (bank, load_addr, data).
fn build_crt(hw: u16, exrom: u8, game: u8, name: &str, chips: &[(u16, u16, Vec<u8>)]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"C64 CARTRIDGE   ");
    v.extend_from_slice(&0x40u32.to_be_bytes());
    v.extend_from_slice(&0x0100u16.to_be_bytes());
    v.extend_from_slice(&hw.to_be_bytes());
    v.push(exrom);
    v.push(game);
    v.extend_from_slice(&[0u8; 6]);
    let mut nm = [0u8; 32];
    let nb = name.as_bytes();
    nm[..nb.len().min(32)].copy_from_slice(&nb[..nb.len().min(32)]);
    v.extend_from_slice(&nm);
    assert_eq!(v.len(), 0x40);
    for (bank, load, data) in chips {
        v.extend_from_slice(b"CHIP");
        let packet_len = 0x10 + data.len() as u32;
        v.extend_from_slice(&packet_len.to_be_bytes());
        v.extend_from_slice(&0u16.to_be_bytes());
        v.extend_from_slice(&bank.to_be_bytes());
        v.extend_from_slice(&load.to_be_bytes());
        v.extend_from_slice(&(data.len() as u16).to_be_bytes());
        v.extend_from_slice(data);
    }
    v
}

/// A `$DE00`-writable BankInfo (the read-only decode fields the mapper ignores).
fn bi() -> BankInfo {
    BankInfo {
        cpu_port_direction: 0x2f,
        cpu_port_value: 0x37,
        basic_visible: true,
        kernal_visible: true,
        io_visible: true,
        char_visible: false,
        cartridge_attached: true,
        cartridge_exrom: None,
        cartridge_game: None,
        phi1: 0xff,
    }
}

/// Bank ROML: `LDA $9FFF ; JMP $8000` loop, with a bank-identifying byte at $9FFF
/// so the retired-instruction A stream differs per bank (the continuation observer
/// folds A → a bank-0 reset on restore is caught).
fn ef_bank_roml(id_byte: u8) -> Vec<u8> {
    let mut b = vec![0xeau8; 0x2000];
    b[0] = 0xad; // LDA abs
    b[1] = 0xff;
    b[2] = 0x9f; // $9FFF
    b[3] = 0x4c; // JMP abs
    b[4] = 0x00;
    b[5] = 0x80; // $8000
    b[0x1fff] = id_byte;
    b
}

// ── scenarios ────────────────────────────────────────────────────────────────────

#[test]
fn plain_booted_roundtrip() {
    if !roms_present() {
        eprintln!("SKIP plain_booted_roundtrip: ROMs absent at {ROM_DIR}");
        return;
    }
    let mut m = Machine::new();
    m.boot_from_dir(std::path::Path::new(ROM_DIR)).expect("boot");
    // Boot to BASIC READY (non-trivial: KERNAL editor loop + live jiffy IRQ + full
    // zero-page / screen RAM).
    let mut sink = trx64_core::NullSink;
    m.run_for_full(2_500_000, &mut sink, |_, _, _, _, _, _, _| {});
    run_scenario("plain", m, None, None);
}

#[test]
fn banked_easyflash_roundtrip() {
    if !roms_present() {
        eprintln!("SKIP banked_easyflash_roundtrip: ROMs absent at {ROM_DIR}");
        return;
    }
    let mut m = Machine::new();
    m.boot_from_dir(std::path::Path::new(ROM_DIR)).expect("boot");
    let mut sink = trx64_core::NullSink;
    m.run_for_full(2_500_000, &mut sink, |_, _, _, _, _, _, _| {});

    // Synthesize a 2-bank EasyFlash cart (hw type 32); each bank's ROML has the same
    // loop code but a DISTINCT $9FFF id byte.
    let crt = build_crt(
        32,
        1,
        0,
        "EFGATE",
        &[(0, 0x8000, ef_bank_roml(0x00)), (1, 0x8000, ef_bank_roml(0xb1))],
    );
    m.attach_cart_from_bytes(&crt, "efgate").expect("attach EF");

    // Drive the mapper into a NON-ZERO bank + 8K mode + non-default IO2 RAM.
    let clk = m.c64_core.clk;
    let info = bi();
    if let Some(cart) = m.cartridge.as_mut() {
        cart.write(0xde02, 0x06, &info, clk); // register_02 = 6 → 8K game mode
        cart.write(0xde00, 0x01, &info, clk); // bank 1
        cart.write(0xdf00, 0xa5, &info, clk); // IO2 RAM
        cart.write(0xdf20, 0x5a, &info, clk);
    }
    // Recompute memconfig from the restored cart lines (8K game maps $8000-$9FFF).
    m.memconfig = m.memconfig_table[m.pla_index()];
    // Point the CPU at the cart loop so the continuation executes bank-1 code.
    m.c64_core.reg_pc = 0x8000;
    m.cpu6510.reg_pc = 0x8000;

    // Sanity: the live mapper really is at bank 1 / register 6.
    let st = m.cartridge.as_ref().unwrap().get_state();
    assert_eq!(st.current_bank, 1, "setup: EF at bank 1");
    assert_eq!(st.control_register, Some(0x06), "setup: EF register_02 = 6");

    run_scenario("banked-ef", m, Some(crt), None);
}

#[test]
fn drive_active_roundtrip() {
    if !roms_present() {
        eprintln!("SKIP drive_active_roundtrip: ROMs absent at {ROM_DIR}");
        return;
    }
    // Drive ROM is required for the drive to run; boot_from_dir loads it non-fatally.
    let drive_rom = std::path::Path::new(ROM_DIR).join("dos1541-325302-01+901229-05.bin").exists()
        || std::path::Path::new(ROM_DIR).join("1541.bin").exists();
    if !drive_rom {
        eprintln!("SKIP drive_active_roundtrip: 1541 DOS ROM absent at {ROM_DIR}");
        return;
    }

    let mut m = Machine::new();
    m.boot_from_dir(std::path::Path::new(ROM_DIR)).expect("boot");
    let mut sink = trx64_core::NullSink;
    m.run_for_full(2_500_000, &mut sink, |_, _, _, _, _, _, _| {});

    // Synthetic blank D64 (a valid 174848-byte GCR image builds from all-zero bytes).
    let disk = DiskImage {
        kind: DiskKind::D64,
        bytes: vec![0u8; 174848],
        backing_path: None,
        read_only: false,
    };
    m.drive8.attach_disk(disk.clone());
    let ht_before = m.drive8.rotation.current_half_track;
    let head_before = m.drive8.rotation.gcr_head_offset;

    // Ask the drive to read the directory — the DOS seeks the head to track 18 and
    // spins looking for sync, so the drive CPU is live + the head has moved.
    for (i, b) in b"LOAD\"$\",8\r".iter().enumerate() {
        m.poke(0x0277 + i as u16, &[*b]);
    }
    m.poke(0x00c6, &[9]);
    m.run_for_full(4_000_000, &mut sink, |_, _, _, _, _, _, _| {});

    let ht_after = m.drive8.rotation.current_half_track;
    let head_after = m.drive8.rotation.gcr_head_offset;
    eprintln!(
        "  [drive-active] head half-track {ht_before} -> {ht_after}, rotation {head_before} -> {head_after}"
    );
    // The drive is genuinely active (head seeked and/or the disk rotated).
    assert!(
        ht_after != ht_before || head_after != head_before,
        "drive did not become active (head/rotation unchanged)"
    );

    run_scenario("drive-active", m, None, Some(disk));
}
