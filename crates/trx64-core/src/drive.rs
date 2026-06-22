//! drive.rs — 1541 floppy drive: 6502 CPU + minimal bus (2 KB RAM + VIA stubs + ROM).
//!
//! Isolation gate (ADR-012): no IEC cross-machine wiring. The drive boots from its
//! DOS ROM, runs its idle/init loop with no external stimulus. VIA chips are
//! register-stub skeletons that return 0xFF on read and silently drop writes, which
//! is enough for the ROM init path to run without jamming.
//!
//! Memory map (1541 per VICE memiec.c / memiec.ts):
//!   $0000-$07FF  2 KB RAM (mirrored at $0800-$1FFF, $2000-$3FFF, $4000-$7FFF)
//!   $1800-$1BFF  VIA1 (open-bus stub: read=0xFF, write ignored)
//!   $1C00-$1FFF  VIA2 (open-bus stub)
//!   $8000-$BFFF  rom[addr & 0x7FFF] = zero (open bus, rom buffer [0x0000..0x4000])
//!   $C000-$FFFF  rom[addr & 0x7FFF] = DOS ROM bytes (rom buffer [0x4000..0x8000])
//!
//! ROM layout: the 16 KB dos1541 file is placed at rom[0x4000..0x8000].
//! Reset vector $FFFC/$FFFD = rom[0x7FFC]/rom[0x7FFD] = file offset 0x3FFC/0x3FFD.

use crate::{cpu::{Bus, Cpu6510}, NullSink, Observer, RomError};

/// Minimal 6522 VIA stub: register file (16 bytes), returns 0xFF on uninitialized
/// reads. The isolation gate does not need timer IRQ delivery.
#[derive(Clone)]
pub struct Via6522 {
    regs: [u8; 16],
}

impl Via6522 {
    fn new() -> Self {
        Self { regs: [0xFF; 16] }
    }
    #[inline]
    fn read(&self, addr: u16) -> u8 {
        self.regs[(addr & 0x0F) as usize]
    }
    #[inline]
    fn write(&mut self, addr: u16, val: u8) {
        self.regs[(addr & 0x0F) as usize] = val;
    }
}

/// Drive 6502 bus (implements cpu::Bus). Borrows from Drive1541 fields.
struct DriveBus<'a> {
    ram: &'a mut [u8; 0x800],
    rom: &'a [u8; 0x8000],
    via1: &'a mut Via6522,
    via2: &'a mut Via6522,
}

impl<'a> Bus for DriveBus<'a> {
    #[inline]
    fn read(&mut self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x7FFF => {
                // VIA1: $1800-$1BFF (mirror every $400)
                if (0x1800..=0x1BFF).contains(&addr) {
                    return self.via1.read(addr);
                }
                // VIA2: $1C00-$1FFF
                if (0x1C00..=0x1FFF).contains(&addr) {
                    return self.via2.read(addr);
                }
                // RAM mirrors: $0000-$07FF and all mirrors up to $7FFF
                self.ram[(addr & 0x07FF) as usize]
            }
            0x8000..=0xFFFF => {
                self.rom[(addr & 0x7FFF) as usize]
            }
        }
    }

    #[inline]
    fn write(&mut self, addr: u16, val: u8) {
        match addr {
            0x0000..=0x7FFF => {
                if (0x1800..=0x1BFF).contains(&addr) {
                    self.via1.write(addr, val);
                    return;
                }
                if (0x1C00..=0x1FFF).contains(&addr) {
                    self.via2.write(addr, val);
                    return;
                }
                // RAM mirrors — write to the base 2 KB
                self.ram[(addr & 0x07FF) as usize] = val;
            }
            0x8000..=0xFFFF => {
                // ROM write: silently ignored (open bus)
            }
        }
    }
}

/// 1541 drive emulator: cycle-exact 6502 + 2 KB RAM + VIA stubs + DOS ROM.
///
/// `Clone` is required so `Machine` (which contains `Drive1541`) remains cloneable
/// for Phase-2 COW forks.
#[derive(Clone)]
pub struct Drive1541 {
    pub cpu: Cpu6510,
    ram: Box<[u8; 0x800]>,
    rom: Box<[u8; 0x8000]>,
    via1: Via6522,
    via2: Via6522,
    /// Monotonic drive clock (mirrors cpu.clk after each run).
    pub drive_clk: u64,
    /// Last sampled PC for drive8-cpu deduplication (sampleDrivePc pattern).
    last_sample_pc: Option<u16>,
    /// VICE drive-sync fixed-point accumulator (drivecpu.c:383-390 `cycle_accum`).
    /// Low 16 fractional bits of accumulated `sync_factor * c64_cycles`; the carry
    /// out of bit 16 is the integer number of drive cycles to advance `stop_clk`.
    sync_accum: u32,
    /// Absolute drive clock the CPU may run up to (VICE `cpu->stop_clk`). The drive
    /// 6502 executes whole instructions while `cpu.clk < stop_clk`.
    stop_clk: u64,
    /// Pending 6502 hardware-reset sequence. VICE fires `cpu_reset` (drivecpu.c:165)
    /// from the 6510 core's IK_RESET dispatch on the FIRST execute round, which sets
    /// `clk_ptr = 6` (the ~6-cycle reset sequence the chip consumes before the first
    /// opcode fetch). We model that lazily, on the first cycle the drive runs, so the
    /// shared `Cpu6510::reset_to()` stays untouched (C64 CPU/VIC/CIA gates unaffected).
    reset_pending: bool,
}

/// PAL drive sync factor (VICE drivesync.c:53-62 `drive_set_machine_parameter`):
///   sync_factor = floor(65536 * 1_000_000 / cycles_per_sec)
/// with the C64 PAL clock cycles_per_sec = 985_248 (vice1541-facade.ts:319). The
/// 1541's `clock_frequency` is 1, so `drv.cpud.sync_factor` = sync_factor * 1.
/// floor(65536 * 1e6 / 985248) = 66517.
const DRIVE_SYNC_FACTOR_PAL: u32 = 66517;

/// 6502 hardware-reset sequence cost the drive consumes before the first opcode
/// fetch (VICE drivecpu.c:165-184 `cpu_reset` → `drv->clk_ptr = 6`).
const DRIVE_RESET_CYCLES: u64 = 6;

/// C64 main-CPU reset-sequence cycles the drive's catch-up clock observes BEFORE the
/// first traced C64 instruction.
///
/// In the TS oracle the drive catches up to `c64Cpu.cycles`, whose origin includes
/// the cycles the C64's own power-on reset consumed reading the $FFFC/$FFFD vector
/// (cpu65xx-vice.ts:531-538). TRX64's shared `Cpu6510::reset_to()` injects PC
/// directly and starts `clk` at 0, so its main-clock origin sits one cycle earlier
/// than TS's. The drive's catch-up targets are therefore uniformly 1 lower than the
/// golden's. We must NOT shift `reset_to()` (it would move the byte-exact C64
/// CPU/VIC/CIA gate cycle stamps), so the drive instead seeds its sync accumulator
/// with this offset at cold reset — a drive-boot-local correction.
const C64_RESET_DRIVE_OFFSET: u64 = 1;

impl Drive1541 {
    pub fn new() -> Self {
        Self {
            cpu: Cpu6510::new(),
            ram: Box::new([0u8; 0x800]),
            rom: Box::new([0u8; 0x8000]),
            via1: Via6522::new(),
            via2: Via6522::new(),
            drive_clk: 0,
            last_sample_pc: None,
            sync_accum: 0,
            stop_clk: 0,
            reset_pending: true,
        }
    }

    /// Load the 16 KB 1541 DOS ROM from `rom_dir`.
    ///
    /// Tries `dos1541-325302-01+901229-05.bin` first, then the alias `1541.bin`.
    /// On success the file bytes land at `rom[0x4000..0x8000]`.
    /// On failure returns `RomError` — caller may choose to continue with zeroed ROM.
    pub fn load_rom(&mut self, rom_dir: &std::path::Path) -> Result<(), RomError> {
        let data = std::fs::read(rom_dir.join("dos1541-325302-01+901229-05.bin"))
            .or_else(|_| std::fs::read(rom_dir.join("1541.bin")))?;
        if data.len() != 0x4000 {
            return Err(RomError::BadSize(data.len(), 0x4000));
        }
        self.rom[0x4000..0x8000].copy_from_slice(&data);
        Ok(())
    }

    /// Reset the drive 6502: read reset vector from $FFFC/$FFFD (in ROM) and
    /// set PC. Sets I flag (IRQ disabled) to match real 1541 power-on.
    pub fn cold_reset(&mut self) {
        let lo = self.rom[0x7FFC] as u16;
        let hi = self.rom[0x7FFD] as u16;
        let pc = lo | (hi << 8);
        self.cpu.reset_to(pc);
        self.cpu.reg_p |= 0x04; // I flag
        // The drive 6502 powers on with SP=0 (drivecpu.ts:459 — cpu_regs init
        // `{ pc:0, ac:0, xr:0, yr:0, sp:0, flags:0 }`), and the VICE drive reset
        // dispatch (drive_6510core.ts:2105-2113) does NOT push (unlike an IRQ), so SP
        // stays 0 through boot until the ROM's own TXS. The shared `reset_to()` seeds
        // SP=$FF for the C64; override it here so the drive matches VICE byte-exact.
        self.cpu.reg_sp = 0;
        // VICE drivecpu_reset (drivecpu.c:193-211): clk = 0, stop_clk = 0,
        // last_clk = maincpu_clk (= 0 at cold boot). The +6 reset-sequence cost is
        // applied lazily by `step_instruction` on the first run cycle, matching the
        // IK_RESET dispatch order in VICE's 6510 core (drivecpu.c:165 cpu_reset).
        self.cpu.clk = 0;
        self.drive_clk = 0;
        self.stop_clk = 0;
        self.sync_accum = 0;
        self.reset_pending = true;
        self.last_sample_pc = None;
        // Seed the sync accumulator with the C64 power-on reset cycles the drive's
        // catch-up clock observes in TS (see C64_RESET_DRIVE_OFFSET). This shifts the
        // whole drive_clk schedule into phase with the golden without touching the
        // shared C64 reset path.
        self.advance_stop_clk(C64_RESET_DRIVE_OFFSET);
    }

    /// Advance the drive's `stop_clk` target by `c64_cycles` of main-CPU time,
    /// applying the VICE PAL sync factor (drivecpu.c:383-390). The integer carry out
    /// of the 16-bit fixed-point accumulator is the number of drive cycles to add.
    #[inline]
    fn advance_stop_clk(&mut self, c64_cycles: u64) {
        // VICE processes in 10000-cycle chunks to bound `sync_factor * tcycles`
        // inside 32 bits; mirror that so the carry math is bit-identical.
        let mut remaining = c64_cycles;
        while remaining != 0 {
            let tcycles = remaining.min(10000) as u32;
            remaining -= tcycles as u64;
            self.sync_accum = self
                .sync_accum
                .wrapping_add(DRIVE_SYNC_FACTOR_PAL.wrapping_mul(tcycles));
            self.stop_clk = self.stop_clk.wrapping_add((self.sync_accum >> 16) as u64);
            self.sync_accum &= 0xFFFF;
        }
    }

    /// Execute one whole drive 6502 instruction over an already-borrowed bus,
    /// folding in the pending hardware reset cost on the very first call.
    ///
    /// VICE's drive 6510 core runs ONE opcode per `drive_6510core_execute` call, and
    /// the IK_RESET dispatch (drivecpu.c:165 `cpu_reset` → clk=6, JUMP $FFFC) happens
    /// in the SAME call body as the opcode fetch+execute (drive_6510core.ts:1672-1733).
    /// So the reset sequence and the first instruction (SEI) are atomic: the drive
    /// goes 0 → reset(clk=6) → SEI(clk=8, PC=$EAA1) without ever stopping at $EAA0.
    /// Modelling them as one step is what makes the first sampled record $EAA1@8
    /// instead of a spurious $EAA0@6.
    ///
    /// Free function (not `&mut self`) so the caller can keep `DriveBus` borrowed
    /// from `self.ram/rom/via*` while we mutate `self.cpu` — the two are disjoint.
    #[inline]
    fn step_instruction<B: Bus, O: Observer>(
        cpu: &mut Cpu6510,
        reset_pending: &mut bool,
        bus: &mut B,
        obs: &mut O,
    ) {
        if *reset_pending {
            *reset_pending = false;
            // cpu_reset: the 6502 reset sequence consumes 6 cycles before the first
            // opcode of the same execute call runs.
            cpu.clk = DRIVE_RESET_CYCLES;
        }
        loop {
            cpu.execute_cycle(bus, obs);
            if cpu.is_at_boundary() {
                break;
            }
        }
    }

    /// Reset PC from the ROM vector (re-read). Returns the resolved PC.
    pub fn reset_pc(&self) -> u16 {
        let lo = self.rom[0x7FFC] as u16;
        let hi = self.rom[0x7FFD] as u16;
        lo | (hi << 8)
    }

    /// Advance the drive by `n` cycles of C64 main-CPU time (VICE
    /// `drivecpu_execute` shape, drivecpu.c:353-445).
    ///
    /// The drive 1541 runs at ~1 MHz while the C64 PAL clock is 985_248 Hz, so VICE
    /// scales main-CPU cycles into drive cycles through the fixed-point `sync_factor`
    /// accumulator (`advance_stop_clk`) rather than 1:1. The drive 6502 then executes
    /// whole instructions while `cpu.clk < stop_clk`. The first run also consumes the
    /// 6-cycle reset sequence (folded into `step_instruction`).
    ///
    /// Uses `NullSink` — the sampling approach (not per-instruction firehose) is
    /// handled externally by `sample_pc_change`.
    pub fn run_cycles(&mut self, n: u64) {
        // Advance the drive-clock target for this slice of main-CPU time.
        self.advance_stop_clk(n);
        let mut obs = NullSink;
        let mut bus = DriveBus {
            ram: &mut self.ram,
            rom: &self.rom,
            via1: &mut self.via1,
            via2: &mut self.via2,
        };
        // Run whole instructions while the drive clock is behind the stop target
        // (VICE drivecpu.c:393 — `while (*clk_ptr < stop_clk)`). The reset sequence
        // is folded into the first instruction (see step_drive_instruction): VICE's
        // 6510 core dispatches IK_RESET and the first opcode in the SAME execute call,
        // so once `reset_pending` is armed the first `step_drive_instruction` always
        // runs even when `stop_clk` is still small — matching VICE's atomic reset+SEI.
        while self.reset_pending || self.cpu.clk < self.stop_clk {
            Self::step_instruction(&mut self.cpu, &mut self.reset_pending, &mut bus, &mut obs);
        }
        self.drive_clk = self.cpu.clk;
    }

    /// Sample the current drive PC for the drive8-cpu trace domain.
    ///
    /// Mirrors the TS `sampleDrivePc()` deduplication: returns `Some(...)` only
    /// when the PC has changed since the last call. This is called once per C64
    /// instruction boundary (not per drive instruction) — the "sampled" pattern
    /// described in integrated-session.ts:855 and ADR-015.
    ///
    /// Returns `(pc, a, x, y, sp, p, drive_clk)` on change, `None` if unchanged.
    pub fn sample_pc_change(&mut self) -> Option<(u16, u8, u8, u8, u8, u8, u64)> {
        let pc = self.cpu.reg_pc;
        if self.last_sample_pc == Some(pc) {
            return None;
        }
        self.last_sample_pc = Some(pc);
        Some((
            pc,
            self.cpu.reg_a,
            self.cpu.reg_x,
            self.cpu.reg_y,
            self.cpu.reg_sp,
            self.cpu.flags(),
            self.drive_clk,
        ))
    }
}

impl Default for Drive1541 {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_bus_ram_mirror() {
        let mut d = Drive1541::new();
        // Write via base address, read via mirror
        {
            let mut bus = DriveBus {
                ram: &mut d.ram,
                rom: &d.rom,
                via1: &mut d.via1,
                via2: &mut d.via2,
            };
            bus.write(0x0010, 0xAB);
            assert_eq!(bus.read(0x0810), 0xAB, "$0810 should mirror $0010");
            assert_eq!(bus.read(0x2010), 0xAB, "$2010 should mirror $0010");
        }
    }

    #[test]
    fn drive_bus_via_stub() {
        let mut d = Drive1541::new();
        let mut bus = DriveBus {
            ram: &mut d.ram,
            rom: &d.rom,
            via1: &mut d.via1,
            via2: &mut d.via2,
        };
        // VIA1 at $1800 — write then read back via register stub
        bus.write(0x1800, 0x42);
        assert_eq!(bus.read(0x1800), 0x42);
        // VIA2 at $1C00
        bus.write(0x1C01, 0x55);
        assert_eq!(bus.read(0x1C01), 0x55);
    }

    #[test]
    fn drive_bus_rom_read() {
        let mut d = Drive1541::new();
        // Place a sentinel in the ROM region
        d.rom[0x4010] = 0xEA; // NOP at CPU $C010
        let mut bus = DriveBus {
            ram: &mut d.ram,
            rom: &d.rom,
            via1: &mut d.via1,
            via2: &mut d.via2,
        };
        assert_eq!(bus.read(0xC010), 0xEA);
    }

    #[test]
    fn sample_pc_change_deduplicates() {
        let mut d = Drive1541::new();
        d.cpu.reg_pc = 0xEA00;
        // First call always returns Some
        assert!(d.sample_pc_change().is_some());
        // Second call with same PC returns None
        assert!(d.sample_pc_change().is_none());
        // Change PC → Some again
        d.cpu.reg_pc = 0xEA10;
        assert!(d.sample_pc_change().is_some());
    }
}
