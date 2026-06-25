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

use crate::{
    drive_6510core::{
        drive_6510core_execute, DriveCore6510, DriveCore6510Bus, IntStatus, IK_RESET,
    },
    gcr::{GcrImage, WritebackKind},
    iec::IecbusT,
    rotation::Rotation,
    viacore::{self, Via1Irq, Via1dBackend, Via2Irq, Via2dBackend, ViaContext},
    RomError,
};

/// Disk image kind — D64 (standard 1541 format) or G64 (GCR nibble dump).
#[derive(Clone, Debug)]
pub enum DiskKind {
    D64,
    G64,
}

/// In-memory disk image attached to a drive. The GCR read path is out of scope
/// (ADR-012 isolation gate); this struct only stores the raw bytes for media
/// mount / persist / SHA256 parity.
#[derive(Clone)]
pub struct DiskImage {
    pub kind: DiskKind,
    pub bytes: Vec<u8>,
    pub backing_path: Option<String>,
    pub read_only: bool,
}

// ── 6522 VIA register index used by drive.rs (via.h:35-55) ──────────────────
// The full register-file / IFR / ACR constant set now lives in `viacore.rs`
// (both drive VIAs run through the 1:1-ported viacore). drive.rs only needs the
// PCR index for the `via1_irq_debug` diagnostic snapshot.
const VIA_PCR: usize = 12;

/// Drive 6502 bus (implements cpu::Bus). Borrows from Drive1541 fields.
struct DriveBus<'a> {
    ram: &'a mut [u8; 0x800],
    rom: &'a [u8; 0x8000],
    /// VIA1 — the 1:1-ported viacore `ViaContext` (viacore.rs). Its IEC disk-side
    /// hooks dispatch through a `Via1dBackend` built on the fly from `via1_iecbus`
    /// (its `v_iecbus`) + `via1_irq`.
    via1: &'a mut ViaContext,
    /// VIA1 IRQ-line mirror (the via1d1541 `set_int` sink — see `viacore::Via1Irq`).
    via1_irq: &'a mut Via1Irq,
    /// VIA1's `v_iecbus` — the drive's `IecbusT`. The C64 path has synced `cpu_bus`
    /// + `drv_port` into it before this run; the `store_prb` hook folds the drive's
    /// PB output into it (drv_data/drv_bus/cpu_port/drv_port), and `read_prb` reads
    /// `drv_port`. This IS via1d1541's `iecbus` pointer — the store-time wired-AND
    /// re-fold the drive sees on its NEXT `$1800` read is performed by store_prb
    /// directly (no external `fold_drv_port` shim needed).
    via1_iecbus: &'a mut IecbusT,
    /// VIA2 — the 1:1-ported viacore `ViaContext` (viacore.rs). Its disk-controller
    /// hooks dispatch through a `Via2dBackend` built on the fly from `rotation` +
    /// `via2_irq` + `pending_set_overflow`.
    via2: &'a mut ViaContext,
    /// VIA2 IRQ-line mirror (the viacore `set_int` sink — see `viacore::Via2Irq`).
    via2_irq: &'a mut Via2Irq,
    /// Live drive-CPU clock pointer (= VICE `via_context->clk_ptr`, Spec 612). The
    /// verbatim drive 6510 core advances `DriveCore6510.clk` between bus accesses
    /// via CLK_ADD; the VIA `rclk` for a register read/write and a timer-alarm
    /// catch-up must be that exact live clock at the access instant, NOT a stale
    /// snapshot. We thread it as a raw `*const u64` to `core.clk` — disjoint from
    /// the bus's borrowed RAM/ROM/VIA/rotation fields, read-only, single-threaded
    /// (the core invokes the bus synchronously), so there is no aliasing hazard.
    /// This is the literal `clk_ptr` indirection VICE keeps per VIA instance.
    /// It is `*mut` because the `cpu_reset` hook writes `*clk_ptr = 6` (VICE
    /// drivecpu.c:165 `drv->clk_ptr->value = 6`) — the 6-cycle reset sequence —
    /// exactly as VICE mutates the shared drive clock from the reset dispatch.
    clk_ptr: *mut u64,
    /// The rotating GCR disk model. `image == None` ⇒ no disk; the VIA2 read_pra/
    /// read_prb hooks then return 0xff (= the old static "no rotating disk"
    /// defaults). When a D64 is mounted this drives PRA (GCR_read), PRB bit7
    /// (SYNC), the stepper/motor/speed-zone from store_prb, and the byte-ready
    /// (SO) handshake consumed by the drive CPU's V flag.
    rotation: &'a mut Rotation,
    /// Pending `drive_cpu_set_overflow` request raised by a VIA2 store side-effect
    /// (set_ca2 on the PCR CA2 edge / store_prb on the motor edge — via2d.c
    /// set_ca2 → drive_cpu_set_overflow, store_prb motor branch). VICE delivers the
    /// byte-ready→V flush straight into the drive CPU's P register from the store;
    /// the bus borrow can't touch `cpu`, so we latch it here and `step_instruction`
    /// folds it into `reg_p` after the store cycle completes. `true` ⇒ set V.
    pending_set_overflow: bool,
}

impl<'a> DriveBus<'a> {
    /// Live drive clock at the current access (= `*clk_ptr`). See `clk_ptr`.
    #[inline]
    fn clk(&self) -> u64 {
        // SAFETY: `clk_ptr` points at `Drive1541.core.clk`, a field disjoint from
        // every field this bus borrows. The read is synchronous inside a bus call
        // the core itself invoked, single-threaded, and never aliases a live `&mut`
        // to that same u64 at the instant of the read.
        unsafe { *self.clk_ptr }
    }

    /// Write the live drive clock (= VICE `drv->clk_ptr->value = n`). Used ONLY by
    /// the `cpu_reset` hook to seed the 6-cycle reset sequence. See `clk_ptr`.
    #[inline]
    fn set_clk(&mut self, v: u64) {
        // SAFETY: same disjoint-field reasoning as `clk`. The write happens inside
        // `cpu_reset` (the DO_INTERRUPT IK_RESET dispatch), at which instant the
        // core is not concurrently writing `core.clk` (it is between CLK_ADD steps).
        unsafe { *self.clk_ptr = v };
    }

    /// VIA1 PB pin input = via1d1541.c read_prb IEC `tmp`:
    ///   tmp = (drv_port ^ 0x85) | 0x1a | driveid   (unit 8 → driveid 0)
    /// Fed to the generic 6522 PRB read as `prb_pin`, which then applies
    ///   byte = (tmp & ~DDRB) | (PRB & DDRB)
    /// — identical to VICE. Output bits (DDRB=1) read the ORB latch; input bits
    /// (DDRB=0) read the IEC bus.

    /// Build the VIA1 IEC backend (via1d1541.ts) from the bus's borrowed `v_iecbus`
    /// + IRQ-mirror, sync `via1.clk` to the live drive clock, and run `f` (a viacore
    /// entry). The `store_prb` hook folds the drive's PB output into the iecbus
    /// (drv_data/drv_bus/cpu_port/drv_port) so the drive's NEXT `$1800` read (via
    /// `read_prb` → `drv_port`) sees its own CLK/DATA pull — VICE's `store_prb`
    /// cross-domain sync, performed inline, no `fold_drv_port` shim.
    #[inline]
    fn via1_with_backend<R>(
        &mut self,
        f: impl FnOnce(&mut ViaContext, &mut Via1dBackend) -> R,
    ) -> R {
        self.via1.clk = self.clk();
        let mut backend = Via1dBackend {
            number: 0,
            iecbus: self.via1_iecbus,
            irq: self.via1_irq,
        };
        f(self.via1, &mut backend)
    }

    /// Dispatch any VIA1 alarms due at/before `clk` (= viacore run_pending_alarms,
    /// the PROCESS_ALARMS path). The alarm callbacks update IFR + the IRQ mirror.
    #[inline]
    fn via1_run_alarms(&mut self, clk: u64) {
        self.via1.clk = clk;
        self.via1_with_backend(|ctx, b| viacore::run_pending_alarms(ctx, b, clk, 0));
    }

    /// VIA1 register store (= viacore_store via the via1d1541 backend). The viacore
    /// applies its own `write_offset` (= 1) so rclk = clk - 1; the `store_prb` IEC
    /// fold reads the live `ctx.clk`/iecbus state.
    #[inline]
    fn via1_store(&mut self, addr: u16, val: u8) {
        self.via1_with_backend(|ctx, b| viacore::viacore_store(ctx, b, addr, val));
    }

    /// VIA1 register read (= viacore_read via the via1d1541 backend).
    #[inline]
    fn via1_read(&mut self, addr: u16) -> u8 {
        self.via1_with_backend(|ctx, b| viacore::viacore_read(ctx, b, addr))
    }

    /// Build the VIA2 disk-controller backend (via2d.ts) from the bus's borrowed
    /// rotation / IRQ-mirror / set-overflow fields, run `f` (a viacore entry), and
    /// flush the backend's `pending_set_overflow` back. `self.via2.clk` is synced
    /// to the live drive clock first — this IS VICE's `clk_ptr->value` indirection.
    /// `has_image` mirrors the TS `if (!drv) return` guard (no disk ⇒ hooks skip).
    #[inline]
    fn via2_with_backend<R>(
        &mut self,
        f: impl FnOnce(&mut ViaContext, &mut Via2dBackend) -> R,
    ) -> R {
        self.via2.clk = self.clk();
        let has_image = self.rotation.image.is_some();
        let mut backend = Via2dBackend {
            drive: self.rotation,
            number: 0,
            irq: self.via2_irq,
            pending_set_overflow: false,
            has_image,
        };
        let r = f(self.via2, &mut backend);
        if backend.pending_set_overflow {
            self.pending_set_overflow = true;
        }
        r
    }

    /// Dispatch any VIA2 alarms due at/before `clk` (= viacore run_pending_alarms,
    /// the PROCESS_ALARMS path). The alarm callbacks update IFR + the IRQ mirror.
    #[inline]
    fn via2_run_alarms(&mut self, clk: u64) {
        self.via2.clk = clk;
        self.via2_with_backend(|ctx, b| viacore::run_pending_alarms(ctx, b, clk, 0));
    }

    /// VIA2 register store (= viacore_store via the via2d backend). The viacore
    /// applies its own `write_offset` (= 1) so rclk = clk - 1; the rotation
    /// side-effects (store_prb / store_pcr) read the FULL clk via `ctx.clk`.
    #[inline]
    fn via2_store(&mut self, addr: u16, val: u8) {
        self.via2_with_backend(|ctx, b| viacore::viacore_store(ctx, b, addr, val));
    }

    /// VIA2 register read (= viacore_read via the via2d backend).
    #[inline]
    fn via2_read(&mut self, addr: u16) -> u8 {
        self.via2_with_backend(|ctx, b| viacore::viacore_read(ctx, b, addr))
    }
}

impl<'a> DriveCore6510Bus for DriveBus<'a> {
    /// PROCESS_ALARMS hook (6510core.c:139-146). VICE dispatches the VIA timer
    /// alarms up to `clk` here; the alarm callback raises the IFR and stamps the
    /// IRQ line. We run BOTH VIA alarm sets up to `clk` so an IFR underflow latches
    /// at the exact cycle it occurs (the per-VIA `irq_stamp` is the precise
    /// underflow rclk). The combined line is re-sampled into the core's IntStatus
    /// by the run loop at each instruction boundary (= where the drive 6510 core
    /// consults it). `clk` is the live `core.clk` the core passes in.
    #[inline]
    fn process_alarms(&mut self, clk: u64) {
        self.via1_run_alarms(clk);
        self.via2_run_alarms(clk);
    }

    /// drivecpu_rotate (drivecpu.c:423-433): advance the rotating GCR head to the
    /// live drive clock. Called by the core at the BVC/BVS/PHP opcodes and by
    /// LOCAL_SET_OVERFLOW(0) (CLV / ADC/SBC/ARR decimal-V-clear) — exactly where
    /// VICE consults the byte-ready handshake, NOT per cycle.
    #[inline]
    fn rotate(&mut self) {
        if self.rotation.image.is_some() {
            let clk = self.clk();
            self.rotation.rotate_disk(clk);
        }
    }

    /// drivecpu_byte_ready (drivecpu.c:423-433): the GCR byte-ready rising-edge
    /// flag the core folds into the V flag (SET_OVERFLOW) at BVC/BVS/PHP. Non-zero
    /// `byte_ready_edge` ⇒ a fresh byte latched since the last consult.
    #[inline]
    fn byte_ready(&mut self) -> bool {
        self.rotation.byte_ready_edge != 0
    }

    /// drivecpu_byte_ready_egde_clear (sic, drivecpu.c:423-433): clear the
    /// byte-ready rising-edge flag once consumed.
    #[inline]
    fn byte_ready_edge_clear(&mut self) {
        self.rotation.byte_ready_edge = 0;
    }

    /// cpu_reset (drivecpu.c:165-184): the drive 6502 hardware-reset sequence. VICE
    /// sets `drv->clk_ptr->value = 6` (the ~6 cycles the chip burns before the
    /// first opcode fetch) — we mutate the shared drive clock through `clk_ptr` to
    /// the same effect. The DO_INTERRUPT IK_RESET path that called us then pulls
    /// the reset vector ($FFFC/$FFFD) and JUMPs there, so the reset and the first
    /// opcode (SEI) are atomic within one execute call (first sampled record
    /// $EAA1@8, the atomic reset+SEI). The VIAs are reset by `cold_reset` (= VICE
    /// drive_reset → viacore_reset); a disk, if any, is dropped there too, so no
    /// rotation_reset is needed here for the boot path.
    #[inline]
    fn cpu_reset(&mut self) {
        self.set_clk(DRIVE_RESET_CYCLES);
    }

    #[inline]
    fn read(&mut self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x7FFF => {
                // VIA1: $1800-$1BFF (mirror every $400) — the 1:1-ported viacore
                // (viacore.rs) driven by the via1d1541 backend. PB ($1800) reads
                // the IEC bus through `read_prb` (tmp = (drv_port^0x85)|0x1a|driveid);
                // PRA ($1801) through `read_pra`; IFR/IER follow 6522 semantics so
                // the drive IRQ handler ($FE6C LDA $180D) sees the real CA1/timer
                // flags. viacore_read dispatches any due alarms itself (rclk = clk).
                if (0x1800..=0x1BFF).contains(&addr) {
                    return self.via1_read(addr);
                }
                // VIA2: $1C00-$1FFF — the 1:1-ported viacore (viacore.rs). PRA
                // ($1C01) / PRB ($1C00) reads sample the rotating disk through the
                // via2d read_pra/read_prb hooks (GCR_read / sync | wps | 0x6f) and
                // clear byte_ready_level inside the backend. viacore_read dispatches
                // any due alarms itself (rclk = clk) for PRB/timer/IFR regs.
                if (0x1C00..=0x1FFF).contains(&addr) {
                    return self.via2_read(addr);
                }
                // RAM mirrors: $0000-$07FF and all mirrors up to $7FFF
                self.ram[(addr & 0x07FF) as usize]
            }
            0x8000..=0xFFFF => self.rom[(addr & 0x7FFF) as usize],
        }
    }

    #[inline]
    fn write(&mut self, addr: u16, val: u8) {
        // Both drive VIAs run through the 1:1-ported viacore (viacore_store), which
        // applies its own per-instance `write_offset` (= 1) so the register/timer/
        // IFR/IRQ logic lands at rclk = ctx.clk - 1 (Spec 612 PL-6), while the
        // store_prb/store_pcr port hooks keep the FULL `ctx.clk` (= the live drive
        // clock the bus syncs into `ctx.clk` via `viaN_with_backend`). The viacore
        // reads `ctx.clk` itself; no manual rclk subtraction is needed here.
        match addr {
            0x0000..=0x7FFF => {
                if (0x1800..=0x1BFF).contains(&addr) {
                    // viacore_store applies its own write_offset (= 1) so rclk =
                    // ctx.clk - 1 for the register/timer/IFR/IRQ logic. The IEC
                    // side-effect (store_prb) folds the drive's composed PB output
                    // into the `v_iecbus` (drv_data/drv_bus/cpu_port/drv_port) so the
                    // drive's NEXT `$1800` read sees its own CLK/DATA pull — this IS
                    // via1d1541.c store_prb, performed inline by the backend hook
                    // (no external `fold_drv_port` shim). store_prb is gated on
                    // `byte != p_oldpb` inside viacore_store/via1d1541, so an ORB
                    // write that leaves the composed output unchanged does NOT re-fold.
                    self.via1_store(addr, val);
                    return;
                }
                if (0x1C00..=0x1FFF).contains(&addr) {
                    // viacore_store applies its own write_offset (= 1) so rclk =
                    // ctx.clk - 1 for the register/timer/IFR/IRQ logic, while the
                    // store_prb/store_pcr rotation hooks read the FULL ctx.clk —
                    // exactly the Spec 612 split. The stepper/motor/speed-zone/
                    // byte-ready side-effects run inside the via2d backend hooks.
                    self.via2_store(addr, val);
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
    /// The drive's DEDICATED verbatim 6502 core (drive_6510core.rs — the 1:1 port
    /// of VICE's 6510core.c DRIVE_CPU build). Replaces the shared C64 `Cpu6510`:
    /// the rotate / byte-ready / SET_OVERFLOW hooks are woven INTO the opcodes at
    /// the exact cycle, so the drive CPU is cycle-identical to VICE.
    pub core: DriveCore6510,
    /// Interrupt status mirror the verbatim core dispatches against (irq_clk /
    /// global_pending_int / IK_*). The combined VIA1∨VIA2 IRQ line is fed in via
    /// `int.set_irq` at each instruction boundary.
    pub int: IntStatus,
    ram: Box<[u8; 0x800]>,
    rom: Box<[u8; 0x8000]>,
    /// VIA1 — the 1:1-ported viacore `ViaContext` (viacore.rs). The IEC disk-side
    /// hooks (store_prb/read_prb CLK/DATA/ATN_ACK bit-bang, CA1=ATN IRQ) run
    /// through `Via1dBackend` exactly as via1d1541.ts does. Replaces the distilled
    /// `Via6522` VIA1.
    via1: ViaContext,
    /// VIA1 IRQ-line mirror (see `viacore::Via1Irq`): the via1d1541 `set_int` hook
    /// records the line level + rclk here; the run loop replays it into
    /// `int.set_irq(0, ..)` at the instruction boundary (VIA1 = int source 0).
    via1_irq: Via1Irq,
    /// VIA1's `v_iecbus` — the drive's owned `IecbusT` (via1d1541.ts:923). The C64
    /// path syncs `cpu_bus` + `drv_port` into it before each catch-up run; the
    /// via1d1541 `store_prb` hook folds the drive's PB output into `drv_data[8]` /
    /// `drv_bus[8]` / `cpu_port` / `drv_port`; `read_prb` reads back `drv_port`.
    /// After the run the C64 reads `drv_data[8]` to fold into the shared IEC core.
    via1_iecbus: IecbusT,
    /// VIA2 — the 1:1-ported viacore `ViaContext` (viacore.rs). Replaces the
    /// distilled `Via6522` for VIA2: the disk-controller hooks (stepper/motor/
    /// SYNC/byte-ready) run through `Via2dBackend` exactly as via2d.ts does.
    via2: ViaContext,
    /// VIA2 IRQ-line mirror (see `viacore::Via2Irq`): the viacore `set_int` hook
    /// records the line level + rclk here; the run loop replays it into
    /// `int.set_irq(1, ..)` at the instruction boundary.
    via2_irq: Via2Irq,
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
    /// Effective IEC bus state the drive reads at its VIA1 PB inputs (= VICE
    /// iecbus.drv_port: bit0=DATA_IN, bit2=CLK_IN, bit7=ATN). Refreshed by the
    /// FullBus push-flush before the drive runs, so a `read $1800` reflects the
    /// live C64-driven IEC lines. Power-on 0x85 (all released).
    pub iec_drv_port: u8,
    /// C64-side IEC intent (= VICE iecbus.cpu_bus: bit4=ATN, bit6=CLK, bit7=DATA),
    /// constant across a single drive catch-up run (the C64 only mutates it on a
    /// $DD00 write, which push-flushes the drive first). Refreshed by the FullBus
    /// push-flush alongside `iec_drv_port`. A `$1800` store inside the catch-up run
    /// re-folds the wired-AND bus against THIS fixed `cpu_bus` so the drive's next
    /// `$1800` read reflects its own pull (= via1d1541.c store_prb). Power-on 0xff
    /// (all released).
    pub iec_cpu_bus: u8,
    /// Pending 6502 hardware-reset sequence. VICE fires `cpu_reset` (drivecpu.c:165)
    /// from the 6510 core's IK_RESET dispatch on the FIRST execute round, which sets
    /// `clk_ptr = 6` (the ~6-cycle reset sequence the chip consumes before the first
    /// opcode fetch). We model that lazily, on the first cycle the drive runs, so the
    /// shared `Cpu6510::reset_to()` stays untouched (C64 CPU/VIC/CIA gates unaffected).
    reset_pending: bool,
    /// Attached disk image (None = no disk in drive).
    pub disk: Option<DiskImage>,
    /// The rotating GCR disk model (head position, bit-stream, byte-ready). Holds
    /// the per-track GCR bitstream for a mounted D64 (`rotation.image`).
    pub rotation: Rotation,
}

/// Build a powered-on VIA1 `ViaContext` (via1d1541.ts:805-943
/// via1d1541_setup_context + via1d1541.ts:790-798 via1d1541_init). Seeds the
/// calloc-zero struct, runs `viacore_setup_context` (power-on register latches,
/// write_offset=1, external cb1/cb2 high), then `viacore_init` (the 5 timer
/// alarms). Sets `int_num = 0` (the drive VIA1 is interrupt source 0; VIA2 is 1)
/// and the VICE names. The VIA1 is then cold-reset by `cold_reset()` via
/// `viacore_reset`. The `v_iecbus` pointer is the drive's owned `IecbusT`.
fn new_via1_ctx() -> ViaContext {
    let mut via = ViaContext::new();
    // via1d1541.ts:904-905 — myname / my_module_name (drive unit 8 → number 0).
    via.myname = Some("1541Drive0Via1".to_string());
    via.my_module_name = Some("1541VIA1D0".to_string());
    viacore::viacore_setup_context(&mut via);
    // via1d1541.ts:911-912 — legacy snapshot module names.
    via.my_module_name_alt1 = Some("VIA1D0".to_string());
    via.my_module_name_alt2 = Some("VIA1D1541".to_string());
    // via1d1541.ts:915 — via->irq_line = IK_IRQ = (1 << 1) = 2.
    via.irq_line = 2;
    // The drive wires VIA1 to IntStatus source 0 (VIA2 is source 1).
    via.int_num = 0;
    viacore::viacore_init(&mut via);
    via
}

/// Build a powered-on VIA2 `ViaContext` (via2d.ts:625-696 via2d_setup_context +
/// via2d.ts:612-618 via2d_init). Seeds the calloc-zero struct, runs
/// `viacore_setup_context` (power-on register latches, write_offset=1, external
/// cb1/cb2 high), then `viacore_init` (the 5 timer alarms). Sets `int_num = 1`
/// (the drive VIA2 is interrupt source 1; VIA1 is 0) and the VICE names. The
/// VIA2 is then cold-reset by `cold_reset()` via `viacore_reset`.
fn new_via2_ctx() -> ViaContext {
    let mut via = ViaContext::new();
    // via2d.ts:709-710 — myname / my_module_name (drive unit 8 → number 0).
    via.myname = Some("Drive0Via2".to_string());
    via.my_module_name = Some("VIA2D0".to_string());
    viacore::viacore_setup_context(&mut via);
    // via2d.ts:718 — via->irq_line = IK_IRQ = 2.
    via.irq_line = 2;
    // via2d.ts:729 — via->int_num. The drive wires VIA2 to IntStatus source 1.
    via.int_num = 1;
    viacore::viacore_init(&mut via);
    via
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
            core: DriveCore6510::new(),
            int: IntStatus::new(),
            ram: Box::new([0u8; 0x800]),
            rom: Box::new([0u8; 0x8000]),
            via1: new_via1_ctx(),
            via1_irq: Via1Irq::new(),
            via1_iecbus: IecbusT::new_power_on(),
            via2: new_via2_ctx(),
            via2_irq: Via2Irq::new(),
            drive_clk: 0,
            last_sample_pc: None,
            sync_accum: 0,
            stop_clk: 0,
            reset_pending: true,
            iec_drv_port: 0x85,
            iec_cpu_bus: 0xff,
            disk: None,
            rotation: Rotation::new(),
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

    /// Cold-reset the drive 6502 (VICE drivecpu_reset, drivecpu.c:193-211). Unlike
    /// the old shared-CPU path, the reset is NOT applied by pre-loading PC here:
    /// the verbatim core dispatches it through its IK_RESET path on the FIRST
    /// `drive_6510core_execute` call (the prologue sees `global_pending_int &
    /// IK_RESET`, runs `cpu_reset` → clk=6, then `load_addr($FFFC)` + JUMP). That
    /// reset and the first opcode (SEI) are atomic within one execute call, so the
    /// first sampled record is $EAA1@8 (not a spurious $EAA0@6) — exactly VICE.
    pub fn cold_reset(&mut self) {
        // Power-on register state (drivecpu cpu_regs init `{pc,ac,xr,yr,sp,flags=0}`,
        // sp=0). The drive 6502 powers on with SP=0; the IK_RESET dispatch does NOT
        // push (unlike an IRQ), so SP stays 0 through boot until the ROM's own TXS.
        self.core = DriveCore6510::new();
        // VICE drivecpu_reset: clk = 0, stop_clk = 0, last_clk = maincpu_clk (= 0 at
        // cold boot). The +6 reset-sequence cost is applied by the IK_RESET dispatch
        // (cpu_reset → clk=6) on the first run cycle, NOT here.
        self.core.clk = 0;
        // Reset the interrupt status to power-on (CLOCK_MAX sentinels, no pending)
        // and arm IK_RESET so the core's first execute dispatches the hardware reset
        // (= VICE interrupt_cpu_status_reset + interrupt_trigger_reset, the latter
        // setting `global_pending_int |= IK_RESET` — vice1541-facade.ts:659).
        self.int = IntStatus::new();
        self.int.global_pending_int |= IK_RESET;
        self.drive_clk = 0;
        self.stop_clk = 0;
        self.sync_accum = 0;
        self.reset_pending = true;
        self.last_sample_pc = None;
        self.iec_drv_port = 0x85;
        self.iec_cpu_bus = 0xff;
        // VICE viacore_reset (viacore.c:378-439) for both VIAs: clear port/ddr
        // and control regs, latch timers to power-on, clear IFR/IER. VIA1's PB/
        // DDRB start at 0 (all inputs, ORB latch 0) so the IEC read_prb formula
        // sees the right DDRB before the ROM programs $1802; VIA2's PCR → 0 so the
        // boot $1C0C read returns 0x00 as VICE does. Anchored at reset clock (0).
        // VIA1: re-seed a fresh power-on ViaContext + iecbus + IRQ mirror, then
        // viacore_reset at clk 0 (same shape as VIA2 below).
        self.via1 = new_via1_ctx();
        self.via1_irq = Via1Irq::new();
        self.via1_iecbus = IecbusT::new_power_on();
        {
            self.via1.clk = 0;
            let mut backend = Via1dBackend {
                number: 0,
                iecbus: &mut self.via1_iecbus,
                irq: &mut self.via1_irq,
            };
            viacore::viacore_reset(&mut self.via1, &mut backend);
        }
        // VIA2: re-seed a fresh power-on ViaContext, then viacore_reset at clk 0.
        // A fresh ctx clears any leftover alarm schedule / IFR / latches; the
        // viacore_reset then re-latches the timers and clears IFR/IER exactly as
        // VICE drive_reset → viacore_reset does (the via2d `reset` hook sets the
        // LED; no behavioural impact here). The IRQ mirror is cleared too.
        self.via2 = new_via2_ctx();
        self.via2_irq = Via2Irq::new();
        {
            self.via2.clk = 0;
            let mut backend = Via2dBackend {
                drive: &mut self.rotation,
                number: 0,
                irq: &mut self.via2_irq,
                pending_set_overflow: false,
                has_image: false,
            };
            viacore::viacore_reset(&mut self.via2, &mut backend);
        }
        // Seed the sync accumulator with the C64 power-on reset cycles the drive's
        // catch-up clock observes in TS (see C64_RESET_DRIVE_OFFSET). This shifts the
        // whole drive_clk schedule into phase with the golden without touching the
        // shared C64 reset path.
        self.advance_stop_clk(C64_RESET_DRIVE_OFFSET);
        // A real 1541 loses its disk on power cycle. Don't preserve disk across reset.
        self.disk = None;
        self.rotation = Rotation::new();
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

    /// Feed the combined VIA1∨VIA2 IRQ line into the verbatim core's `IntStatus`
    /// at the precise per-source rclk, mirroring VICE's `update_myviairq_rclk →
    /// set_int → interrupt_set_irq(int_status, int_num, level, rclk)` for each VIA.
    /// VIA1 is int_num 0, VIA2 is int_num 1 (both wired into the single drive CPU
    /// IRQ pin). `IntStatus::set_irq` stamps `irq_clk` only on the `nirq` 0→1 edge
    /// (first source) and arms the IK_IRQPEND tail (`irq_pending_clk = rclk + 3`)
    /// on the final deassert — exactly VICE.
    ///
    /// The rclk passed per source: on an ASSERT, the VIA's `irq_stamp` (the precise
    /// underflow / CA1-edge rclk its own `update_irq` recorded); on a DEASSERT, the
    /// live drive clock `now` (VICE clears the flag at the access rclk, which the
    /// boundary clock equals or just trails — and `irq_stamp` is the inactive
    /// `u64::MAX` sentinel there, which would overflow `rclk + 3`).
    #[inline]
    fn refresh_irq_line(int: &mut IntStatus, via1_irq: &Via1Irq, via2_irq: &Via2Irq, now: u64) {
        let s1 = if via1_irq.active { via1_irq.stamp } else { now };
        let s2 = if via2_irq.active { via2_irq.stamp } else { now };
        int.set_irq(0, via1_irq.active, s1);
        int.set_irq(1, via2_irq.active, s2);
    }

    /// Composed VIA1 PB output byte driving the IEC bus (= viacore VIA_PRB store
    /// `out = ORB | ~DDRB`). Output bits (DDRB=1) carry the ORB latch; input bits
    /// (DDRB=0) float HIGH. The IEC core inverts this to `drv_data[8]`. PB1=DATA_OUT,
    /// PB3=CLK_OUT, PB4=ATN_ACK (active-low after the 7406 / wired-AND inversion).
    ///
    /// After the 1:1 via1d1541 port, the drive's `store_prb` hook already wrote the
    /// inverted PB output into `via1_iecbus.drv_data[8]` (= VICE `*drive_data =
    /// ~byte`). The composed `(ORB | ~DDRB)` output the C64 path folds is therefore
    /// the un-inverted `~drv_data[8]` — read straight from the iecbus. (Equivalently
    /// `via1.via[VIA_PRB] | !via1.via[VIA_DDRB]`; both agree once store_prb ran.)
    #[inline]
    pub fn via1_pb_iec_output(&self) -> u8 {
        (!self.via1_iecbus.drv_data[8]) & 0xff
    }

    /// DIAGNOSTIC: snapshot the drive VIA1 IRQ/CA1 state for the ATN-IRQ probe.
    /// Returns (ifr, ier, pcr, irq_active, irq_stamp).
    #[doc(hidden)]
    pub fn via1_irq_debug(&self) -> (u8, u8, u8, bool, u64) {
        (
            self.via1.ifr,
            self.via1.ier,
            self.via1.via[VIA_PCR],
            self.via1_irq.active,
            self.via1_irq.stamp,
        )
    }

    /// Deliver an IEC ATN-line edge to the drive's VIA1 CA1 input (= VICE
    /// iecbus.c:264-266: `viacore_signal(unit->via1d1541, VIA_SIG_CA1,
    /// iec_old_atn ? 0 : VIA_SIG_RISE)`, where `iec_old_atn = cpu_bus & 0x10` is the
    /// NEW ATN line state). The C64 asserting ATN drives the drive's attention IRQ
    /// (DOS $FE67 → $E85B handler) via VIA1 CA1. `sig` is the edge code the iecbus
    /// write-conf1 path computed (`AtnEdge::Via1Ca1 { sig }`): `VIA_SIG_RISE` (1)
    /// when ATN is now LOW, `0` (no-edge) when ATN is now HIGH — exactly the value
    /// VICE hands `viacore_signal`. `clk` is the drive clock the edge is stamped at.
    ///
    /// Routed through the 1:1 `viacore_signal(ctx, VIA_SIG_CA1, edge)` via the
    /// via1d1541 backend — the verbatim CA1-edge → IFR CA1 → IRQ path, replacing
    /// the distilled `signal_ca1`.
    #[inline]
    pub fn atn_edge_to_via1_ca1(&mut self, sig: u8, clk: u64) {
        self.via1.clk = clk;
        let mut backend = Via1dBackend {
            number: 0,
            iecbus: &mut self.via1_iecbus,
            irq: &mut self.via1_irq,
        };
        viacore::run_pending_alarms(&mut self.via1, &mut backend, clk, 0);
        viacore::viacore_signal(&mut self.via1, &mut backend, crate::iec::VIA_SIG_CA1, sig);
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
    /// whole instructions while `core.clk < stop_clk` (drivecpu.c:393). The first run
    /// also consumes the 6-cycle reset sequence — but, unlike the old shared-CPU path,
    /// that is now dispatched by the verbatim core's IK_RESET path (cpu_reset → clk=6
    /// + JMP $FFFC), folded into the first execute call exactly like drivecpu.c.
    pub fn run_cycles(&mut self, n: u64) {
        // Advance the drive-clock target for this slice of main-CPU time.
        self.advance_stop_clk(n);
        // Sync the C64-side IEC state into the drive's `v_iecbus` (= via1d1541's
        // `iecbus` pointer) before the run: `cpu_bus` (the C64 intent, constant
        // across this catch-up) and `drv_port` (the effective drive-input bus the
        // C64 path folded from the shared IEC core). The drive's `read_prb` reads
        // `drv_port`; a `$1800` `store_prb` re-folds the wired-AND against this
        // `cpu_bus` so the drive sees its own CLK/DATA pull on the next read.
        self.via1_iecbus.cpu_bus = self.iec_cpu_bus;
        self.via1_iecbus.drv_port = self.iec_drv_port;
        // Disjoint split-borrow of `self`: `core`/`int`/`reset_pending` go to the
        // verbatim execute call; the rest (RAM/ROM/VIA/rotation/IEC) to the bus.
        let core = &mut self.core;
        let int = &mut self.int;
        let reset_pending = &mut self.reset_pending;
        // The bus reads the live drive clock through `clk_ptr` (= VICE clk_ptr,
        // Spec 612): the verbatim core advances `core.clk` via CLK_ADD between bus
        // accesses, so the VIA rclk for a register read/write or a timer-alarm
        // catch-up must be that exact clock at the access instant. `clk_ptr` is also
        // written by the `cpu_reset` hook (`*clk_ptr = 6`).
        let clk_ptr: *mut u64 = &mut core.clk;
        let mut bus = DriveBus {
            ram: &mut self.ram,
            rom: &self.rom,
            via1: &mut self.via1,
            via1_irq: &mut self.via1_irq,
            via1_iecbus: &mut self.via1_iecbus,
            via2: &mut self.via2,
            via2_irq: &mut self.via2_irq,
            clk_ptr,
            rotation: &mut self.rotation,
            pending_set_overflow: false,
        };
        // Run whole instructions while the drive clock is behind the stop target
        // (VICE drivecpu.c:393 — `while (*clk_ptr < stop_clk)`). Once `reset_pending`
        // is armed the first execute always runs even when `stop_clk` is still small
        // — VICE's 6510 core dispatches IK_RESET (and the first opcode, SEI) in the
        // SAME execute call, so the atomic reset+SEI lands regardless of stop_clk.
        while *reset_pending || core.clk < self.stop_clk {
            *reset_pending = false;
            // Sample the combined VIA1∨VIA2 IRQ line into the core's IntStatus at
            // the instruction boundary, BEFORE the execute call's prologue dispatch
            // (= VICE: the VIA alarm `set_int` has already stamped int_status by the
            // time DO_INTERRUPT's interrupt_check_irq_delay reads it). The VIAs'
            // alarms were brought up to `core.clk` by the prior step's PROCESS_ALARMS
            // and by every bus access.
            bus.via1_run_alarms(core.clk);
            bus.via2_run_alarms(core.clk);
            Self::refresh_irq_line(int, bus.via1_irq, bus.via2_irq, core.clk);

            // One whole drive instruction (or one interrupt/reset dispatch) on the
            // verbatim core. The rotate/byte-ready/SET_OVERFLOW hooks are woven into
            // the opcodes (BVC/BVS/PHP/CLV), so the SO handshake is exact.
            drive_6510core_execute(core, &mut bus, int);

            // drive_cpu_set_overflow flush: a VIA2 store side-effect (set_ca2 on the
            // PCR CA2 low→high edge / store_prb motor-off) latched a byte-ready→V
            // request during this instruction. VICE pushes it straight into the
            // drive CPU's P register from the store; fold it in at the instruction
            // boundary (the store completed within this execute call).
            if bus.pending_set_overflow {
                bus.pending_set_overflow = false;
                core.reg_p |= 0x40; // P_OVERFLOW
            }
        }
        self.drive_clk = core.clk;
    }

    /// Advance the drive to an ABSOLUTE C64-clock target (VICE
    /// drive_cpu_execute_one/all at the $DD00 read/write instant). `c64_ref` is the
    /// C64 clock the drive was last advanced up to; returns the new reference (=
    /// `c64_clk`). A monotonic no-op when `c64_clk <= c64_ref`.
    #[inline]
    pub fn catch_up_to(&mut self, c64_clk: u64, c64_ref: u64) -> u64 {
        if c64_clk > c64_ref {
            self.run_cycles(c64_clk - c64_ref);
        }
        c64_clk
    }

    /// Attach a disk image to this drive (replaces any existing disk). For a D64
    /// the raw bytes are encoded to the per-track GCR bitstream and handed to the
    /// rotating-disk model, parking the head at track 18. For a G64 the raw GCR
    /// nibble dump is parsed by `GcrImage::from_g64` (1:1 port of the VICE
    /// `fsimage_read_gcr_image` G64 path) into the same per-half-track array the
    /// D64 encoder produces, so the rotation engine reads it identically —
    /// including half-tracks + copy-protection.
    pub fn attach_disk(&mut self, image: DiskImage) {
        let (gcr, wb_kind) = match image.kind {
            DiskKind::D64 => (Some(GcrImage::from_d64(&image.bytes)), WritebackKind::D64),
            DiskKind::G64 => (Some(GcrImage::from_g64(&image.bytes)), WritebackKind::G64),
        };
        if let Some(gcr) = gcr {
            // Wire the raw on-disk image bytes as the write-back target so a
            // drive write (rotation `write_next_bit` → `gcr_dirty_track`) is
            // serialized back into the image on head-move / detach / flush
            // (= VICE `fsimage->fd`). 1:1 with c64re BUG-023 write-through.
            self.rotation.attach_with_writeback(
                gcr,
                self.drive_clk,
                Some((image.bytes.clone(), wb_kind, image.read_only)),
            );
        }
        self.disk = Some(image);
    }

    /// Detach (eject) the disk from this drive. Flushes any pending dirty track
    /// back into `disk.bytes` first (VICE `drive_image_detach` →
    /// `drive_gcr_data_writeback`), so an eject persists a pending write.
    pub fn detach_disk(&mut self) {
        self.flush_disk_writeback();
        self.disk = None;
        self.rotation.detach();
    }

    /// Flush any pending dirty GCR track back into `self.disk.bytes` (the
    /// authoritative on-disk image the daemon persists/hashes/snapshots). Mirrors
    /// VICE `drive_gcr_data_writeback_all` being called before `fsimage->fd` is
    /// read. Cheap no-op when nothing is dirty. Returns whether bytes changed.
    pub fn flush_disk_writeback(&mut self) -> bool {
        if !self.rotation.has_dirty_track() {
            return false;
        }
        // Serialize the dirty track into the rotation's write-back buffer, then
        // mirror it into the DiskImage the daemon reads.
        if let Some(synced) = self.rotation.writeback_bytes_synced() {
            if let Some(disk) = self.disk.as_mut() {
                disk.bytes = synced;
                return true;
            }
        }
        false
    }

    /// Get a reference to the currently attached disk image, if any.
    ///
    /// This does NOT flush in-flight drive writes (it borrows `&self`). Callers
    /// that read `disk.bytes` for persist / sha / snapshot MUST call
    /// [`flush_disk_writeback`] first (the daemon does), mirroring VICE flushing
    /// `drive_gcr_data_writeback_all` before reading `fsimage->fd`. Metadata-only
    /// callers (backing path, kind) need no flush.
    /// [`flush_disk_writeback`]: Drive1541::flush_disk_writeback
    pub fn get_attached_disk(&self) -> Option<&DiskImage> {
        self.disk.as_ref()
    }

    // ── snapshot accessors (drive_snapshot.rs — additive serialization, ADR-077) ──
    // The drive's VIA1/VIA2/RAM are private to drive.rs; these `pub(crate)` views
    // let the VICE drive-snapshot module-stream port (drive_snapshot.rs) read/write
    // them through the same `Via*dBackend` the live bus builds. No cycle/opcode
    // logic is touched — these are pure state reads/writes at an instruction
    // boundary, guarded by the byte-exact gates.

    /// Run `f` over VIA1 with the live `Via1dBackend` (mirrors `DriveBus::
    /// via1_with_backend`). `via1.clk` is synced to the drive clock first.
    pub(crate) fn snapshot_via1<R>(
        &mut self,
        f: impl FnOnce(&mut ViaContext, &mut Via1dBackend) -> R,
    ) -> R {
        self.via1.clk = self.core.clk;
        let mut backend = Via1dBackend {
            number: 0,
            iecbus: &mut self.via1_iecbus,
            irq: &mut self.via1_irq,
        };
        f(&mut self.via1, &mut backend)
    }

    /// Run `f` over VIA2 with the live `Via2dBackend` (mirrors `DriveBus::
    /// via2_with_backend`). `via2.clk` is synced to the drive clock first.
    pub(crate) fn snapshot_via2<R>(
        &mut self,
        f: impl FnOnce(&mut ViaContext, &mut Via2dBackend) -> R,
    ) -> R {
        self.via2.clk = self.core.clk;
        let has_image = self.rotation.image.is_some();
        let mut backend = Via2dBackend {
            drive: &mut self.rotation,
            number: 0,
            irq: &mut self.via2_irq,
            pending_set_overflow: false,
            has_image,
        };
        f(&mut self.via2, &mut backend)
    }

    /// Snapshot view of the 2 KB drive RAM (DRIVECPU module ARRAY field).
    pub(crate) fn snapshot_ram(&self) -> &[u8; 0x800] {
        &self.ram
    }

    /// Mutable snapshot view of the 2 KB drive RAM (DRIVECPU restore).
    pub(crate) fn snapshot_ram_mut(&mut self) -> &mut [u8; 0x800] {
        &mut self.ram
    }

    /// Drive-CPU `stop_clk` (VICE `cpu->stop_clk`) for the DRIVECPU module.
    pub(crate) fn snapshot_stop_clk(&self) -> u64 {
        self.stop_clk
    }

    /// Restore the drive-CPU `stop_clk`.
    pub(crate) fn snapshot_set_stop_clk(&mut self, v: u64) {
        self.stop_clk = v;
    }

    /// Drive-sync fixed-point accumulator (VICE `cpu->cycle_accum`).
    pub(crate) fn snapshot_sync_accum(&self) -> u32 {
        self.sync_accum
    }

    /// Restore the drive-sync fixed-point accumulator.
    pub(crate) fn snapshot_set_sync_accum(&mut self, v: u32) {
        self.sync_accum = v;
    }

    /// Re-derive the drive clock mirrors after a DRIVECPU restore (the drive_clk
    /// shadow follows `core.clk`).
    pub(crate) fn snapshot_sync_drive_clk(&mut self) {
        self.drive_clk = self.core.clk;
    }

    /// Test-only: VIA2 IFR (for the drive_snapshot round-trip test).
    #[cfg(test)]
    pub(crate) fn via2_ifr_test(&self) -> u8 {
        self.via2.ifr
    }

    /// Test-only: VIA2 PRB/DDRB register bytes.
    #[cfg(test)]
    pub(crate) fn via2_prb_ddrb_test(&self) -> (u8, u8) {
        (
            self.via2.via[crate::viacore::VIA_PRB],
            self.via2.via[crate::viacore::VIA_DDRB],
        )
    }

    /// Read a byte of the drive's 2 KB RAM (mirrored every $800). Used to inspect
    /// the DOS job queue / sector buffers (the decoded sector at $0300) for the
    /// disk-read gate. No side effects.
    #[inline]
    pub fn drive_ram_read(&self, addr: u16) -> u8 {
        self.ram[(addr & 0x07FF) as usize]
    }

    /// Write a byte of the drive's 2 KB RAM (mirrored every $800). Used to poke
    /// the DOS job queue directly ($00=$80 READ, $06/$07 = track/sector) to drive
    /// a sector read without the full IEC command handshake.
    #[inline]
    pub fn drive_ram_write(&mut self, addr: u16, val: u8) {
        self.ram[(addr & 0x07FF) as usize] = val;
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
        let pc = self.core.reg_pc;
        if self.last_sample_pc == Some(pc) {
            return None;
        }
        self.last_sample_pc = Some(pc);
        Some((
            pc,
            self.core.reg_a,
            self.core.reg_x,
            self.core.reg_y,
            self.core.reg_sp,
            self.core.status(), // composite P (= LOCAL_STATUS, flag_n/flag_z folded in)
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
        let mut clk: u64 = 0;
        // Write via base address, read via mirror
        {
            let mut bus = DriveBus {
                ram: &mut d.ram,
                rom: &d.rom,
                via1: &mut d.via1,
                via1_irq: &mut d.via1_irq,
                via1_iecbus: &mut d.via1_iecbus,
                via2: &mut d.via2,
                via2_irq: &mut d.via2_irq,
                clk_ptr: &mut clk,
                rotation: &mut d.rotation,
                pending_set_overflow: false,
            };
            bus.write(0x0010, 0xAB);
            assert_eq!(bus.read(0x0810), 0xAB, "$0810 should mirror $0010");
            assert_eq!(bus.read(0x2010), 0xAB, "$2010 should mirror $0010");
        }
    }

    #[test]
    fn drive_bus_via1_iec_pb() {
        // VIA1 ($1800) now runs through the 1:1-ported viacore + the via1d1541
        // backend. PB read = read_prb composite re-folded by viacore:
        //   tmp = (drv_port ^ 0x85) | 0x1a | driveid, then
        //   byte = (PRB & DDRB) | (tmp & ~DDRB).
        // With the power-on iecbus (drv_port = 0x85) and DDRB=0 the read returns
        // tmp = (0x85^0x85)|0x1a|0 = 0x1a. PRA read (read_pra) = (PRA & DDRA) |
        // (0xff & ~DDRA).
        let mut d = Drive1541::new();
        let mut clk: u64 = 0;
        let mut bus = DriveBus {
            ram: &mut d.ram,
            rom: &d.rom,
            via1: &mut d.via1,
            via1_irq: &mut d.via1_irq,
            via1_iecbus: &mut d.via1_iecbus,
            via2: &mut d.via2,
            via2_irq: &mut d.via2_irq,
            clk_ptr: &mut clk,
            rotation: &mut d.rotation,
            pending_set_overflow: false,
        };
        // The FIRST $1800 PB write fires store_prb (composed out 0xff != oldpb 0,
        // the power-on reset value) and folds the drive's own pull into the iecbus:
        //   drv_data[8] = ~0xff = 0x00 → drv_bus[8] = 0 → cpu_port = 0 →
        //   drv_port = ((0>>4)&4)|(0>>7)|((cpu_bus 0xff <<3)&0x80) = 0x80.
        // read_prb then sees tmp = (0x80 ^ 0x85)|0x1a|0 = 0x1f. (This is the 1:1
        // via1d1541 store_prb fold — the OLD distilled path skipped the fold when
        // the composed output was unchanged, leaving drv_port at the 0x85 power-on
        // and reading 0x1a. The 1:1 port fires on the oldpb=0 first-write edge.)
        bus.write(0x1800, 0x42); // sets ORB latch (no effect with DDRB=0)
        assert_eq!(
            bus.read(0x1800),
            0x1f,
            "$1800 PB read after the first store_prb bus fold (DDRB=0)"
        );
        // Drive all bits as outputs → read returns the ORB latch verbatim
        // (PRB & DDRB) with DDRB=$FF.
        bus.write(0x1802, 0xff); // DDRB = all outputs
        assert_eq!(
            bus.read(0x1800),
            0x42,
            "$1800 PB read = ORB latch when DDRB=$FF"
        );
        // VIA1 PRA ($1801): with DDRA=0xFF read_pra returns the stored ORA latch.
        bus.write(0x1803, 0xff); // DDRA = all outputs
        bus.write(0x1801, 0x33);
        assert_eq!(
            bus.read(0x1801),
            0x33,
            "$1801 PRA reads ORA latch with DDRA=$FF"
        );
    }

    #[test]
    fn drive_bus_via2_pcr_readback() {
        // VIA2 PCR ($1C0C) is a real 6522 register (viacore.rs): it reads back the
        // stored value, NOT the old 0xFF stub. After power-on PCR = 0x00 (the byte
        // the boot init at $F263 LDA $1C0C expects — fixes boot-basic-ready +2).
        let mut d = Drive1541::new();
        let mut clk: u64 = 0;
        let mut bus = DriveBus {
            ram: &mut d.ram,
            rom: &d.rom,
            via1: &mut d.via1,
            via1_irq: &mut d.via1_irq,
            via1_iecbus: &mut d.via1_iecbus,
            via2: &mut d.via2,
            via2_irq: &mut d.via2_irq,
            clk_ptr: &mut clk,
            rotation: &mut d.rotation,
            pending_set_overflow: false,
        };
        assert_eq!(
            bus.read(0x1C0C),
            0x00,
            "$1C0C PCR reads 0x00 after power-on"
        );
        bus.write(0x1C0C, 0xEE);
        assert_eq!(
            bus.read(0x1C0C),
            0xEE,
            "$1C0C PCR reads back the stored value"
        );
    }

    #[test]
    fn drive_via2_t1_underflow_raises_irq() {
        // Program VIA2 T1 (latch $0010) free-run + enable the T1 IRQ, then run the
        // timer past the underflow and assert the IRQ line goes active with the IFR
        // T1 bit set — the mechanism behind the periodic drive IRQ. Exercises the
        // 1:1-ported viacore (ViaContext) + a no-disk Via2dBackend driving the IRQ
        // mirror. The store offset (write_offset=1) makes rclk = clk - 1.
        use crate::viacore::{
            self as vc, Via2Irq, Via2dBackend, ViaContext, VIA_ACR_T1_FREE_RUN, VIA_IM_T1,
        };
        let mut ctx = new_via2_ctx();
        let mut irq = Via2Irq::new();
        let mut rot = Rotation::new();
        // Power-on viacore_reset at clk 0.
        ctx.clk = 0;
        {
            let mut b = Via2dBackend {
                drive: &mut rot,
                number: 0,
                irq: &mut irq,
                pending_set_overflow: false,
                has_image: false,
            };
            vc::viacore_reset(&mut ctx, &mut b);
        }
        // Helper: store / read at clk through a fresh no-disk backend.
        let store =
            |ctx: &mut ViaContext, irq: &mut Via2Irq, rot: &mut Rotation, addr, val, clk| {
                ctx.clk = clk;
                let mut b = Via2dBackend {
                    drive: rot,
                    number: 0,
                    irq,
                    pending_set_overflow: false,
                    has_image: false,
                };
                vc::viacore_store(ctx, &mut b, addr, val);
            };
        store(
            &mut ctx,
            &mut irq,
            &mut rot,
            0x1C0B,
            VIA_ACR_T1_FREE_RUN,
            10,
        ); // ACR: T1 free-run
        store(&mut ctx, &mut irq, &mut rot, 0x1C06, 0x10, 11); // T1LL = 0x10
        store(&mut ctx, &mut irq, &mut rot, 0x1C07, 0x00, 12); // T1LH = 0x00
        store(&mut ctx, &mut irq, &mut rot, 0x1C0E, 0xC0, 13); // IER: enable T1
        store(&mut ctx, &mut irq, &mut rot, 0x1C05, 0x00, 14); // T1CH write starts the timer
        assert!(!irq.active, "no IRQ before underflow");
        // Dispatch alarms past t1zero.
        {
            ctx.clk = 14 + 0x10 + 4;
            let mut b = Via2dBackend {
                drive: &mut rot,
                number: 0,
                irq: &mut irq,
                pending_set_overflow: false,
                has_image: false,
            };
            vc::run_pending_alarms(&mut ctx, &mut b, 14 + 0x10 + 4, 0);
        }
        assert!(irq.active, "T1 underflow asserts the IRQ line");
        assert_ne!(ctx.ifr & VIA_IM_T1, 0, "IFR T1 flag set");
        // Reading T1CL ($1C04) clears the T1 flag and drops the line.
        {
            ctx.clk = 14 + 0x10 + 5;
            let mut b = Via2dBackend {
                drive: &mut rot,
                number: 0,
                irq: &mut irq,
                pending_set_overflow: false,
                has_image: false,
            };
            let _ = vc::viacore_read(&mut ctx, &mut b, 0x1C04);
        }
        assert_eq!(ctx.ifr & VIA_IM_T1, 0, "reading T1CL clears the T1 flag");
        assert!(!irq.active, "IRQ line drops once IFR T1 cleared");
    }

    /// REPRO for the drive-viacore u32-deadline wedge (Spec 743 drive-viacore).
    ///
    /// The 1541 drive clock (`core.clk`, fed verbatim into `run_pending_alarms`) is
    /// u64-MONOTONIC. Before the fix the drive viacore armed alarm DEADLINES
    /// `& 0xffff_ffff` (u32). Once the drive clock crosses 2^32, the masked deadline
    /// truncates into the low u32 range and becomes permanently unreachable
    /// (`next_pending_alarm_clk` ~2^32 below `clk`), so `run_pending_alarms` either
    /// spins ~4 billion catch-up iterations or trips the safety guard and the alarm
    /// NEVER fires. This test programs a VIA2 T1 timer with the clock already past
    /// 2^32 and asserts the alarm both RETURNS and FIRES (no spin, deadline > 2^32).
    ///
    /// Before fix: deadline masked into low u32 → safety guard `break`s → no IRQ →
    /// `irq.active` assertion FAILS (and without the guard, this loop would hang).
    /// After fix: deadline is full u64 → alarm dispatches → IRQ asserts.
    #[test]
    fn drive_via_alarm_fires_past_2pow32() {
        use crate::viacore::{
            self as vc, Via2Irq, Via2dBackend, ViaContext, VIA_ACR_T1_FREE_RUN, VIA_IM_T1,
        };
        // Base clock already PAST 2^32 — the exact wedge condition from the daemon
        // (long run / warp / a checkpoint-restore over the 2^32 boundary).
        const BASE: u64 = 0x1_0000_5000;
        const LATCH: u64 = 0x10;

        let mut ctx = new_via2_ctx();
        let mut irq = Via2Irq::new();
        let mut rot = Rotation::new();
        // Power-on viacore_reset at the post-2^32 clock.
        ctx.clk = BASE;
        {
            let mut b = Via2dBackend {
                drive: &mut rot,
                number: 0,
                irq: &mut irq,
                pending_set_overflow: false,
                has_image: false,
            };
            vc::viacore_reset(&mut ctx, &mut b);
        }
        let store =
            |ctx: &mut ViaContext, irq: &mut Via2Irq, rot: &mut Rotation, addr, val, clk| {
                ctx.clk = clk;
                let mut b = Via2dBackend {
                    drive: rot,
                    number: 0,
                    irq,
                    pending_set_overflow: false,
                    has_image: false,
                };
                vc::viacore_store(ctx, &mut b, addr, val);
            };
        store(&mut ctx, &mut irq, &mut rot, 0x1C0B, VIA_ACR_T1_FREE_RUN, BASE + 10); // ACR T1 free-run
        store(&mut ctx, &mut irq, &mut rot, 0x1C06, LATCH as u8, BASE + 11); // T1LL
        store(&mut ctx, &mut irq, &mut rot, 0x1C07, 0x00, BASE + 12); // T1LH
        store(&mut ctx, &mut irq, &mut rot, 0x1C0E, 0xC0, BASE + 13); // IER: enable T1
        store(&mut ctx, &mut irq, &mut rot, 0x1C05, 0x00, BASE + 14); // T1CH arms the timer

        // The armed deadline MUST be full u64 (> 2^32), consistent with the u64 clock.
        // Before the fix it would be masked into the low u32 range (< 2^32).
        let deadline = ctx.alarm_context.next_pending_alarm_clk;
        assert!(
            deadline > (1u64 << 32),
            "T1 deadline must stay a full u64 past 2^32 (was {deadline:#x}); a low-u32 \
             value means an absolute-clock mask survived"
        );
        assert!(!irq.active, "no IRQ before the timer underflows");

        // Dispatch alarms with the clock advanced just past the deadline. This MUST
        // return (no infinite catch-up / no safety-guard bail) AND fire the alarm.
        let run_clk = BASE + 14 + LATCH + 4;
        {
            ctx.clk = run_clk;
            let mut b = Via2dBackend {
                drive: &mut rot,
                number: 0,
                irq: &mut irq,
                pending_set_overflow: false,
                has_image: false,
            };
            vc::run_pending_alarms(&mut ctx, &mut b, run_clk, 0);
        }

        assert!(
            irq.active,
            "T1 underflow must assert the IRQ even with the drive clock past 2^32 \
             (a low-u32 deadline would have been skipped by the safety guard)"
        );
        assert_ne!(ctx.ifr & VIA_IM_T1, 0, "IFR T1 flag set past 2^32");
        // The free-run alarm re-armed itself to the NEXT deadline, also > 2^32 and
        // ahead of the current clock — i.e. consistent, no permanent stall.
        let next = ctx.alarm_context.next_pending_alarm_clk;
        assert!(
            next > run_clk && next > (1u64 << 32),
            "re-armed deadline must advance past clk and stay full u64 \
             (next={next:#x}, clk={run_clk:#x})"
        );
    }

    #[test]
    fn drive_bus_rom_read() {
        let mut d = Drive1541::new();
        // Place a sentinel in the ROM region
        d.rom[0x4010] = 0xEA; // NOP at CPU $C010
        let mut clk: u64 = 0;
        let mut bus = DriveBus {
            ram: &mut d.ram,
            rom: &d.rom,
            via1: &mut d.via1,
            via1_irq: &mut d.via1_irq,
            via1_iecbus: &mut d.via1_iecbus,
            via2: &mut d.via2,
            via2_irq: &mut d.via2_irq,
            clk_ptr: &mut clk,
            rotation: &mut d.rotation,
            pending_set_overflow: false,
        };
        assert_eq!(bus.read(0xC010), 0xEA);
    }

    #[test]
    fn sample_pc_change_deduplicates() {
        let mut d = Drive1541::new();
        d.core.reg_pc = 0xEA00;
        // First call always returns Some
        assert!(d.sample_pc_change().is_some());
        // Second call with same PC returns None
        assert!(d.sample_pc_change().is_none());
        // Change PC → Some again
        d.core.reg_pc = 0xEA10;
        assert!(d.sample_pc_change().is_some());
    }
}
