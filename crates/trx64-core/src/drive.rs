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

/// Disk image kind — D64 (standard 1541 format), G64 (GCR nibble dump), or D81 (the
/// 1581's sector image, Spec 872 D3: 80-83 tracks, with or without error bytes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiskKind {
    D64,
    G64,
    D81,
}

impl DiskKind {
    /// The board a medium of this kind goes into.
    pub fn board(&self) -> crate::iec::DriveType {
        match self {
            DiskKind::D64 | DiskKind::G64 => crate::iec::DriveType::Drive1541,
            DiskKind::D81 => crate::iec::DriveType::Drive1581,
        }
    }

    /// "d64" / "g64" / "d81".
    pub fn name(&self) -> &'static str {
        match self {
            DiskKind::D64 => "d64",
            DiskKind::G64 => "g64",
            DiskKind::D81 => "d81",
        }
    }
}

/// "1541" / "1581" — the name a refusal and the wire use for a board type.
pub fn board_name(t: crate::iec::DriveType) -> &'static str {
    match t {
        crate::iec::DriveType::Drive1581 => "1581",
        _ => "1541",
    }
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
    /// VICE `drv->cpu->cpu_last_data` — the last byte on the drive's data bus, and
    /// therefore what an UNMAPPED address reads back (`drive_read_free`).
    cpu_last_data: &'a mut u8,
    /// Pending `drive_cpu_set_overflow` request raised by a VIA2 store side-effect
    /// (set_ca2 on the PCR CA2 edge / store_prb on the motor edge — via2d.c
    /// set_ca2 → drive_cpu_set_overflow, store_prb motor branch). VICE delivers the
    /// byte-ready→V flush straight into the drive CPU's P register from the store;
    /// the bus borrow can't touch `cpu`, so we latch it here and `step_instruction`
    /// folds it into `reg_p` after the store cycle completes. `true` ⇒ set V.
    pending_set_overflow: bool,
    /// `via1p->number` / `via2p->number` — the drive number the VIA backends are
    /// built with (unit − 8). It sets the device-ID jumper bits VIA1 port B reads
    /// (`(number << 5) & 0x60`) and the IEC bus slot (`number + 8`). Spec 870 D4.
    number: usize,
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
            number: self.number,
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
            number: self.number,
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
                // The 1541 address decode is NOT a continuous RAM mirror. VICE builds
                // it in `memiec_init` (drive/iec/memiec.c:137-155) by filling the whole
                // table with `drive_read_free` and then overlaying, in 2 KB / 1 KB
                // windows:
                //
                //   $0000-$07FF  RAM            $1800-$1BFF  VIA1   $1C00-$1FFF  VIA2
                //   $2000-$27FF  RAM mirror     $3800-$3BFF  VIA1   $3C00-$3FFF  VIA2
                //   $4000-$47FF  RAM mirror     $5800-$5BFF  VIA1   $5C00-$5FFF  VIA2
                //   $6000-$67FF  RAM mirror     $7800-$7BFF  VIA1   $7C00-$7FFF  VIA2
                //
                // Everything else below $8000 stays unmapped and reads the open bus.
                // This used to be `ram[addr & 0x7ff]` for the whole of $0000-$7FFF
                // with only the $1800/$1C00 windows carved out — so THREE of the four
                // VIA images were plain RAM here. Drive code that reaches a VIA
                // through a mirror (a routine ORs $6000 onto its pointers, or walks
                // $7C00 to save a byte) then wrote into RAM and toggled no line at
                // all — including CLK, which is what the C64 sits waiting for.
                let page = addr >> 8;
                if matches!(page & 0x1f, 0x18..=0x1b) {
                    let v = self.via1_read(addr);
                    *self.cpu_last_data = v;
                    return v;
                }
                if matches!(page & 0x1f, 0x1c..=0x1f) {
                    let v = self.via2_read(addr);
                    *self.cpu_last_data = v;
                    return v;
                }
                if (page & 0x1f) < 0x08 {
                    let v = self.ram[(addr & 0x07FF) as usize];
                    *self.cpu_last_data = v;
                    return v;
                }
                // Unmapped — `drive_read_free` returns cpu_last_data.
                *self.cpu_last_data
            }
            0x8000..=0xFFFF => {
                let v = self.rom[(addr & 0x7FFF) as usize];
                *self.cpu_last_data = v;
                v
            }
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
        // Every store puts the byte on the bus first — `drive_store_free` does it
        // even for an unmapped address (drivemem.c:81-85), which is what makes the
        // next open-bus READ return it.
        *self.cpu_last_data = val;
        match addr {
            0x0000..=0x7FFF => {
                // Same window map as `read` — see the comment there. The decode
                // repeats every $2000, so the page's low five bits select it.
                let page = addr >> 8;
                if matches!(page & 0x1f, 0x18..=0x1b) {
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
                if matches!(page & 0x1f, 0x1c..=0x1f) {
                    // viacore_store applies its own write_offset (= 1) so rclk =
                    // ctx.clk - 1 for the register/timer/IFR/IRQ logic, while the
                    // store_prb/store_pcr rotation hooks read the FULL ctx.clk —
                    // exactly the Spec 612 split. The stepper/motor/speed-zone/
                    // byte-ready side-effects run inside the via2d backend hooks.
                    self.via2_store(addr, val);
                    return;
                }
                if (page & 0x1f) < 0x08 {
                    self.ram[(addr & 0x07FF) as usize] = val;
                }
                // Unmapped: the byte is on the bus and goes nowhere else.
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
    // (`led_on()` below reads VIA2 PB bit 3 out of this — see there.)
    /// VIA2 IRQ-line mirror (see `viacore::Via2Irq`): the viacore `set_int` hook
    /// records the line level + rclk here; the run loop replays it into
    /// `int.set_irq(1, ..)` at the instruction boundary.
    via2_irq: Via2Irq,
    /// Monotonic drive clock (mirrors cpu.clk after each run).
    pub drive_clk: u64,
    /// VICE `drv->cpu->cpu_last_data` — the last byte that was on the drive's data
    /// bus. Every mapped read and store updates it, and it is what an UNMAPPED
    /// address returns (`drive_read_free`, drivemem.c:75-91). The 1541's address
    /// decode leaves large holes ($0800-$17FF, $2800-$37FF, $4800-$57FF,
    /// $6800-$77FF) and reading one of those on real hardware yields whatever the
    /// bus last carried, not RAM.
    pub cpu_last_data: u8,
    /// Last sampled PC for drive8-cpu deduplication (sampleDrivePc pattern).
    last_sample_pc: Option<u16>,
    /// VICE drive-sync fixed-point accumulator (drivecpu.c:383-390 `cycle_accum`).
    /// Low 16 fractional bits of accumulated `sync_factor * c64_cycles`; the carry
    /// out of bit 16 is the integer number of drive cycles to advance `stop_clk`.
    sync_accum: u32,
    /// VICE `drv->cpud->sync_factor` (drivesync.c:53-62): `floor(65536 * 1e6 / cpu_hz)`,
    /// the 16.16 ratio of drive cycles to C64 cycles. The 1541 runs at a true 1 MHz on
    /// every machine; only this ratio follows the C64's clock (Spec 863 — the model's,
    /// 66517 PAL, 64079 NTSC). A drive reset keeps it: it is the machine's, not the drive's.
    pub sync_factor: u32,
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
    /// Spec 871 — every device's contribution to the wired-AND (`iecbus.drv_bus`) as
    /// the machine's IEC core held it when this catch-up began. VICE's via1d1541
    /// `store_prb` folds against the one global `iecbus`, so a drive's own store sees
    /// the other drive's pull; here each drive runs on its own `v_iecbus`, and this is
    /// what puts the other slots into it. Fed by [`Self::feed_iec`] for both drives
    /// before either runs, so the order the two are advanced in cannot change what
    /// either reads. On a machine with one drive every other slot is released (0xff),
    /// which is what `v_iecbus` already held.
    iec_drv_bus: [u8; crate::iec::IECBUS_NUM],
    /// Pending 6502 hardware-reset sequence. VICE fires `cpu_reset` (drivecpu.c:165)
    /// from the 6510 core's IK_RESET dispatch on the FIRST execute round, which sets
    /// `clk_ptr = 6` (the ~6-cycle reset sequence the chip consumes before the first
    /// opcode fetch). We model that lazily, on the first cycle the drive runs, so the
    /// shared `Cpu6510::reset_to()` stays untouched (C64 CPU/VIC/CIA gates unaffected).
    reset_pending: bool,
    /// Attached disk image (None = no disk in drive).
    pub disk: Option<DiskImage>,
    /// The `rotation.writeback_gen` that `disk.bytes` was last brought up to. A head
    /// move folds a dirty track into the rotation's write-back image without
    /// touching `disk.bytes`; this is how [`Self::flush_disk_writeback`] knows.
    disk_synced_gen: u64,
    /// `disk.bytes` holds a write no [`Self::flush_disk_writeback`] has reported
    /// yet. Written into `disk.bytes` and reported to whoever writes the host file
    /// are two facts: the drive's reset and power switch do the first and leave the
    /// second to the next flush, and the flag rides through the reset with the disk.
    disk_write_unreported: bool,
    /// The rotating GCR disk model (head position, bit-stream, byte-ready). Holds
    /// the per-track GCR bitstream for a mounted D64 (`rotation.image`).
    pub rotation: Rotation,

    // ── Spec 870 — the drive as a part ──────────────────────────────────────
    /// D1 — power. Off: not clocked, not on the IEC bus. Default on.
    powered: bool,
    /// D2 — the drive's own reset input held low. Not clocked, not on the bus.
    reset_held: bool,
    /// D2a — powered, clock frozen, outputs kept (the U64's `stop_when_frozen`).
    stopped: bool,
    /// D2 — whether the C64's RESET reaches this drive (the IEC RESET line).
    reset_line_connected: bool,
    /// D4 — the unit number the drive answers to, 8-11: the device-ID jumpers as
    /// the DOS read them at the drive's last reset. Sets the VIA backends' `number`
    /// (unit − 8) and the IEC bus slot.
    unit: u8,
    /// D4 — where the jumpers stand now. Latched into `unit` at the next reset.
    unit_jumpers: u8,
    /// D3 — a ROM given but not yet in force: it replaces `rom` at the next reset.
    rom_next: Option<Box<[u8; 0x8000]>>,
    /// D2a — ATN edges that arrived while stopped: the CA1 level the VIA last saw
    /// (`None` = no edge arrived) and the latest level. See `atn_edge_to_via1_ca1`.
    atn_stop_origin: Option<u8>,
    atn_stop_latest: u8,

    // ── Spec 872 — the board in this position ──────────────────────────────
    /// `Some` when the position holds a 1581 (D1). The 1541 electronics above stay in
    /// the struct and are neither clocked nor on the bus then; the part state (power,
    /// reset, stopped, reset line, unit, jumpers) is the position's and means the same
    /// thing for either board (§5).
    board_1581: Option<Box<crate::drive1581::Drive1581>>,
    /// The 1581 DOS this position has been given: what a 1581 board built here gets
    /// as its ROM, in force at its power-on.
    rom_1581: Option<Box<[u8; 0x8000]>>,
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

/// The default model's drive sync factor (VICE drivesync.c:53-62
/// `drive_set_machine_parameter`): `sync_factor = floor(65536 * 1_000_000 / cycles_per_sec)`
/// with the `c64-pal` clock 985 248 → 66517. The 1541's `clock_frequency` is 1, so
/// `drv.cpud.sync_factor` = sync_factor * 1. A machine sets its model's
/// (`Timing::drive_sync_factor`).
const DEFAULT_SYNC_FACTOR: u32 = 66517;

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
            cpu_last_data: 0,
            last_sample_pc: None,
            sync_accum: 0,
            sync_factor: DEFAULT_SYNC_FACTOR,
            stop_clk: 0,
            reset_pending: true,
            iec_drv_port: 0x85,
            iec_cpu_bus: 0xff,
            iec_drv_bus: [0xff; crate::iec::IECBUS_NUM],
            disk: None,
            disk_synced_gen: 0,
            disk_write_unreported: false,
            rotation: Rotation::new(),
            powered: true,
            reset_held: false,
            stopped: false,
            reset_line_connected: true,
            unit: 8,
            unit_jumpers: 8,
            rom_next: None,
            atn_stop_origin: None,
            atn_stop_latest: 0,
            board_1581: None,
            rom_1581: None,
        }
    }

    // ── Spec 872 — the board type ────────────────────────────────────────────

    /// The board this position holds.
    pub fn board_type(&self) -> crate::iec::DriveType {
        if self.board_1581.is_some() {
            crate::iec::DriveType::Drive1581
        } else {
            crate::iec::DriveType::Drive1541
        }
    }

    /// The 1581 board, when the position holds one.
    pub fn board_1581(&self) -> Option<&crate::drive1581::Drive1581> {
        self.board_1581.as_deref()
    }
    pub fn board_1581_mut(&mut self) -> Option<&mut crate::drive1581::Drive1581> {
        self.board_1581.as_deref_mut()
    }

    /// The drive CPU in force — the 1541's or the 1581's.
    pub fn cpu(&self) -> &crate::drive_6510core::DriveCore6510 {
        match &self.board_1581 {
            Some(b) => &b.core,
            None => &self.core,
        }
    }

    /// Spec 872 §5 — change the board in this position. Refused while the drive is
    /// powered: the type is chosen at power-on. Off, the new board is built fresh (RAM
    /// zero, its own ROM — the 1581 DOS this position was given — in force at its
    /// power-on). The mounted medium is kept only if it fits the new board; otherwise
    /// it is written back and ejected, and returned to the caller, whose persist it is.
    pub fn set_board_type(&mut self, t: crate::iec::DriveType) -> Result<Option<DiskImage>, String> {
        if self.powered {
            return Err(format!(
                "the drive is powered; switch it off before changing its type to {}",
                board_name(t)
            ));
        }
        Ok(self.force_board_type(t))
    }

    /// [`Self::set_board_type`] without the power rule — a checkpoint restore, which
    /// puts the type back whatever it was.
    pub(crate) fn force_board_type(&mut self, t: crate::iec::DriveType) -> Option<DiskImage> {
        use crate::iec::DriveType;
        let t = if t == DriveType::Drive1581 { t } else { DriveType::Drive1541 };
        if t == self.board_type() {
            return None;
        }
        // Spec 875 — a host's controller is taken out only by its host: a board with
        // one is never replaced here (a restore that would need it refuses by name).
        if self.host_fdc_name().is_some() {
            return None;
        }
        // Write-back first, into `disk.bytes`, whatever happens to the medium.
        self.sync_disk_bytes();
        let medium = self.disk.take();
        let unreported = self.disk_write_unreported;
        // Unmount it from the old board's mechanism without another write-back.
        if let Some(b) = self.board_1581.as_mut() {
            b.detach();
        } else {
            self.rotation.detach();
        }
        if t == DriveType::Drive1581 {
            let mut b = Box::new(crate::drive1581::Drive1581::new(0));
            if let Some(rom) = &self.rom_1581 {
                let _ = b.set_rom(&rom[..]);
            }
            b.latch_rom();
            b.set_number(self.dnr());
            self.board_1581 = Some(b);
        } else {
            self.board_1581 = None;
            self.ram.fill(0);
        }
        self.cpu_last_data = 0;
        // Fresh electronics at their reset state; nothing runs while the drive is off.
        self.cold_reset();
        match medium {
            Some(img) if img.kind.board() == t => {
                self.attach_disk_with_unreported_write(img, unreported);
                None
            }
            other => {
                self.disk_write_unreported = false;
                other
            }
        }
    }

    /// Spec 872 §7 — give this position the 1581 DOS: exactly 32 KiB, refused
    /// otherwise, naming the size and the type. A 1581 board here takes it at its next
    /// power-on; a 1541 here keeps it for when the position becomes a 1581.
    pub fn set_rom_1581(&mut self, bytes: &[u8]) -> Result<(), RomError> {
        if bytes.len() != 0x8000 {
            return Err(RomError::BadDriveRomSizeFor(bytes.len(), "1581"));
        }
        let mut rom = Box::new([0u8; 0x8000]);
        rom.copy_from_slice(bytes);
        if let Some(b) = self.board_1581.as_mut() {
            let _ = b.set_rom(bytes);
        }
        self.rom_1581 = Some(rom);
        Ok(())
    }

    /// Load the 1581 DOS from `rom_dir`: `dos1581-318045-02.bin` (VICE's name), then
    /// the aliases `1581.bin` and `1581.rom` (Ultimate's). Not bundled (Commodore IP).
    pub fn load_rom_1581(&mut self, rom_dir: &std::path::Path) -> Result<(), RomError> {
        let data = std::fs::read(rom_dir.join("dos1581-318045-02.bin"))
            .or_else(|_| std::fs::read(rom_dir.join("1581.bin")))
            .or_else(|_| std::fs::read(rom_dir.join("1581.rom")))?;
        self.set_rom_1581(&data)
    }

    /// Whether a medium of `kind` fits the board in this position (§5): a D81 goes into
    /// a 1581, a D64 or G64 into a 1541. The refusal names the board.
    pub fn medium_fits(&self, kind: &DiskKind) -> Result<(), String> {
        let t = self.board_type();
        if kind.board() == t {
            Ok(())
        } else {
            Err(format!(
                "a {} does not fit a {}: {} media go into a {}",
                kind.name().to_uppercase(),
                board_name(t),
                kind.name().to_uppercase(),
                board_name(kind.board())
            ))
        }
    }

    /// Spec 875 §4 — the name of the host's controller fitted in this position's 1581
    /// (or of a clone's vacancy), if any.
    pub fn host_fdc_name(&self) -> Option<&str> {
        self.board_1581.as_ref()?.host_fdc().map(|h| h.name())
    }

    /// Spec 875 §4 — no medium of TRX64's while a host's controller is fitted: the D81
    /// is the host's.
    fn refuse_medium_for_host(&self) -> Result<(), String> {
        match self.board_1581.as_ref().and_then(|b| b.host_fdc()) {
            Some(h) => Err(format!("drive position {} has {}; its medium is the host's", h.position, h.name())),
            None => Ok(()),
        }
    }

    /// Spec 875 §4 — fit a host's controller into this position's 1581. The caller
    /// (`Machine::attach_fdc_controller`) has checked the refusals. A mounted D81 is
    /// written back, ejected and returned.
    pub(crate) fn fit_host_fdc(&mut self, dev: Box<dyn crate::fdc_controller::FdcController>, position: &'static str) -> Option<DiskImage> {
        self.sync_disk_bytes();
        let medium = self.disk.take();
        self.disk_write_unreported = false;
        let b = self.board_1581.as_mut().expect("a 1581 in the position");
        b.detach();
        b.fit_host_fdc(dev, position);
        self.disk_synced_gen = b.image_gen();
        medium
    }

    /// Spec 875 §4 — take the host's controller out; TRX64's WD1772 is back, no disk.
    pub(crate) fn unfit_host_fdc(&mut self) -> Option<Box<dyn crate::fdc_controller::FdcController>> {
        let b = self.board_1581.as_mut()?;
        let dev = b.unfit_host_fdc()?;
        self.disk = None;
        self.disk_synced_gen = b.image_gen();
        Some(dev)
    }

    /// Mount `image` if it fits the board ([`Self::medium_fits`]); refused otherwise and
    /// nothing changes. What media verbs call.
    pub fn mount(&mut self, image: DiskImage) -> Result<(), String> {
        self.refuse_medium_for_host()?;
        self.medium_fits(&image.kind)?;
        if image.kind == DiskKind::D81 && crate::fdd::d81_geometry(image.bytes.len()).is_none() {
            return Err(format!(
                "{} bytes is not a D81 size (80-83 tracks, with or without error bytes)",
                image.bytes.len()
            ));
        }
        self.attach_disk(image);
        Ok(())
    }

    /// Spec 872 §6 — a checkpoint's D81 (as written) into this 1581 position's medium,
    /// leaving the mechanism as the restore put it. A mounted D81 keeps its backing
    /// path and write-protect; with none mounted the image becomes the medium.
    pub(crate) fn restore_medium_1581(&mut self, bytes: Vec<u8>) {
        let Some(b) = self.board_1581.as_mut() else { return };
        b.wd.fdd.image_tracks = crate::fdd::d81_geometry(bytes.len()).map(|g| g.0).unwrap_or(80);
        b.wd.fdd.image = Some(bytes.clone());
        self.disk_synced_gen = b.image_gen();
        match self.disk.as_mut() {
            Some(d) if d.kind == DiskKind::D81 => d.bytes = bytes,
            _ => {
                b.read_only = false;
                self.disk = Some(DiskImage { kind: DiskKind::D81, bytes, backing_path: None, read_only: false });
            }
        }
        if let (Some(d), Some(b)) = (self.disk.as_ref(), self.board_1581.as_mut()) {
            b.read_only = d.read_only;
        }
    }

    /// Spec 871 — the drive in position B as a machine is built: a complete 1541,
    /// switched off, its jumpers at unit 9 (the U64's drive B default). Off, it is
    /// neither clocked nor on the bus, so a machine with it is the machine without it.
    pub fn new_position_b() -> Self {
        let mut d = Self::new();
        let p = DrivePart::default_for(DrivePosition::B);
        d.powered = p.powered;
        d.unit = p.unit;
        d.unit_jumpers = p.unit_jumpers;
        d
    }

    /// Load the 1541 DOS ROM from `rom_dir` — a convenience over [`Self::set_rom`].
    ///
    /// Tries `dos1541-325302-01+901229-05.bin` first, then the alias `1541.bin`, and
    /// hands the bytes to `set_rom`: in force from the drive's next power-on.
    /// On failure returns `RomError` — caller may choose to continue with zeroed ROM.
    pub fn load_rom(&mut self, rom_dir: &std::path::Path) -> Result<(), RomError> {
        let data = std::fs::read(rom_dir.join("dos1541-325302-01+901229-05.bin"))
            .or_else(|_| std::fs::read(rom_dir.join("1541.bin")))?;
        self.set_rom_1541(&data)
    }

    /// Spec 870 D3 — give the drive its ROM as bytes. 16 KiB goes to `$C000-$FFFF`
    /// (`$8000-$BFFF` stays zero, as the file loader always left it); 32 KiB is the
    /// whole `$8000-$FFFF`. Any other size is refused and nothing changes.
    ///
    /// The ROM takes effect at the drive's next POWER-ON, and only then. A ROM is not
    /// something a running drive changes: even a board with a ROM switch has to be
    /// switched off and on for the other ROM to run, and a reset of a powered drive
    /// keeps the ROM it has.
    pub fn set_rom(&mut self, bytes: &[u8]) -> Result<(), RomError> {
        // Spec 872 §5 — a 1581 position takes exactly 32 KiB; 870's 16 KiB form stays
        // a 1541 thing.
        if self.board_1581.is_some() {
            return self.set_rom_1581(bytes);
        }
        self.set_rom_1541(bytes)
    }

    /// The 1541 half of [`Self::set_rom`]: the 1541 electronics' ROM, whatever board the
    /// position holds now (in force at the 1541's next power-on).
    pub fn set_rom_1541(&mut self, bytes: &[u8]) -> Result<(), RomError> {
        let mut rom = Box::new([0u8; 0x8000]);
        match bytes.len() {
            0x4000 => rom[0x4000..0x8000].copy_from_slice(bytes),
            0x8000 => rom.copy_from_slice(bytes),
            n => return Err(RomError::BadDriveRomSize(n)),
        }
        self.rom_next = Some(rom);
        Ok(())
    }

    /// The drive number the VIA backends use (VICE `via1p->number`): unit − 8.
    #[inline]
    fn dnr(&self) -> usize {
        (self.unit - 8) as usize
    }

    /// Spec 870 — whether the drive runs at all: powered, not held, not stopped.
    #[inline]
    pub fn is_clocked(&self) -> bool {
        self.powered && !self.reset_held && !self.stopped
    }

    /// Spec 870 — the IEC slot this drive drives, or `None` when it drives nothing
    /// (off, or held in reset). A stopped drive keeps its slot: its outputs stand.
    #[inline]
    pub fn bus_slot(&self) -> Option<usize> {
        if self.powered && !self.reset_held {
            Some(self.unit as usize)
        } else {
            None
        }
    }

    /// Spec 871 — give the drive the bus as the IEC core holds it now, for the next
    /// catch-up: the lines it reads (`drv_port`), the C64's intent (`cpu_bus`) and
    /// every device's pull (`drv_bus`, see `iec_drv_bus`).
    #[inline]
    pub fn feed_iec(&mut self, iec: &crate::iec::IecCore) {
        self.iec_drv_port = iec.iecbus.drv_port;
        self.iec_cpu_bus = iec.iecbus.cpu_bus;
        self.iec_drv_bus = iec.iecbus.drv_bus;
    }

    /// The reset sequence the drive's RESET input runs: bring a pending disk write
    /// into the image, reset the electronics (`cold_reset`), keep the disk — it is a
    /// medium in the mechanism, not state of the electronics. A write the image
    /// takes here is still reported by the next [`Self::flush_disk_writeback`].
    fn reset_keeping_disk(&mut self) {
        self.sync_disk_bytes();
        if self.board_1581.is_some() {
            // The 1581's reset does not touch the mechanism (Spec 872 §3): the medium
            // stays mounted, the head where it is, the disk-change latch as it was.
            self.cold_reset();
            return;
        }
        let mounted_disk = self.disk.take();
        let unreported = self.disk_write_unreported;
        self.cold_reset();
        if let Some(image) = mounted_disk {
            self.attach_disk_with_unreported_write(image, unreported);
        }
    }

    /// Spec 870 D3 — a ROM given since the last power-on comes into force. Called only
    /// on the way into power, never from a reset.
    pub(crate) fn latch_rom(&mut self) {
        if let Some(b) = self.board_1581.as_mut() {
            b.latch_rom();
            return;
        }
        if let Some(rom) = self.rom_next.take() {
            self.rom = rom;
        }
    }

    /// Spec 870 D3 — the drive's power-on: the ROM given since the last power-on comes
    /// into force, then the electronics start from their reset state. What the machine's
    /// own power-on (`boot_from_dir`) and the daemon's `session/drive_power` press run.
    pub fn power_on_reset(&mut self) {
        self.latch_rom();
        if self.powered {
            if let Some(b) = self.board_1581.as_mut() {
                b.host_power(true);
            }
        }
        self.cold_reset();
    }

    /// Spec 870 D2 — a pulse on the drive's own RESET input. A drive without power
    /// ignores it. The reset reaches a stopped drive too — RESET is not a clocked
    /// input — and it stays stopped, standing at the reset state.
    pub fn reset(&mut self) {
        if self.powered {
            self.reset_keeping_disk();
        }
    }

    /// Spec 870 D2 — the C64's RESET, as it arrives over the IEC RESET line: a drive
    /// reset when the line is connected, nothing when it is not.
    pub fn reset_from_c64(&mut self) {
        if self.reset_line_connected {
            self.reset();
        }
    }

    /// Spec 870 D1 — switch the drive on or off.
    ///
    /// Off: a pending disk write is brought into the image (VICE `drive_disable`),
    /// then the drive is neither clocked nor on the bus; the next
    /// [`Self::flush_disk_writeback`] still reports it. On from off is a power-on:
    /// RAM cleared (VICE allocates it zeroed), CPU and VIAs through their reset, the
    /// disk kept. Setting the state it already has changes nothing.
    pub fn set_power(&mut self, on: bool) {
        if on == self.powered {
            return;
        }
        if on {
            self.powered = true;
            self.ram.fill(0);
            self.cpu_last_data = 0;
            if let Some(b) = self.board_1581.as_mut() {
                b.ram_mut().fill(0);
                b.cpu_last_data = 0;
                // Spec 875 §6 — `power(true)`, then the power-on reset's `drive_reset`.
                b.host_power(true);
            }
            self.latch_rom();
            self.reset_keeping_disk();
        } else {
            self.sync_disk_bytes();
            self.powered = false;
            if let Some(b) = self.board_1581.as_mut() {
                b.host_power(false);
            }
        }
    }

    /// Spec 870 D2 — hold the drive's RESET input low, or release it. Held: not
    /// clocked, the electronics at their reset state, nothing driven on the bus.
    /// Release runs the reset sequence. Without power only the flag moves.
    pub fn set_reset_held(&mut self, held: bool) {
        if held == self.reset_held {
            return;
        }
        self.reset_held = held;
        if self.powered {
            self.reset_keeping_disk();
        }
    }

    /// Spec 870 D2a — stop the drive's clock, or let it run again.
    ///
    /// Stopped: no cycle runs — no catch-up, no rotation — and its VIA outputs keep
    /// driving the IEC lines as they were. Released, it continues where it stood, no
    /// reset; the C64 time it was stopped does not happen to it (`run_cycles` never
    /// advanced its target, and the catch-up reference moved on without it). An ATN
    /// change that arrived while stopped is delivered to VIA1 CA1 now, once, if the
    /// line ended up somewhere other than where the VIA last saw it.
    pub fn set_stopped(&mut self, stopped: bool) {
        if stopped == self.stopped {
            return;
        }
        self.stopped = stopped;
        if !stopped {
            if let Some(origin) = self.atn_stop_origin.take() {
                let latest = self.atn_stop_latest;
                if latest != origin {
                    let clk = self.core.clk;
                    self.atn_edge_to_via1_ca1(latest, clk);
                }
            }
        }
    }

    /// Spec 870 D2 — connect or cut the IEC RESET line between the C64 and this drive.
    pub fn set_reset_line_connected(&mut self, connected: bool) {
        self.reset_line_connected = connected;
    }

    /// Spec 870 D4 — set the device-ID jumpers to `unit` (8-11). The DOS reads them
    /// at reset, so the drive answers to the new number from its next reset on.
    /// Anything outside 8-11 is refused by name: a 1541 has no jumper for it.
    pub fn set_unit(&mut self, unit: u8) -> Result<(), String> {
        if !(8..=11).contains(&unit) {
            return Err(format!(
                "unit {unit} is not a 1541 jumper setting (8-11); a higher number is the DOS's, set after reset"
            ));
        }
        self.unit_jumpers = unit;
        Ok(())
    }

    // ── Spec 870 D5 — read-only state ──────────────────────────────────────────

    /// Powered (D1).
    pub fn powered(&self) -> bool {
        self.powered
    }
    /// Held in reset (D2).
    pub fn reset_held(&self) -> bool {
        self.reset_held
    }
    /// Stopped (D2a).
    pub fn stopped(&self) -> bool {
        self.stopped
    }
    /// The IEC RESET line to the C64 is connected (D2).
    pub fn reset_line_connected(&self) -> bool {
        self.reset_line_connected
    }
    /// The unit number the drive answers to — the jumpers as of its last reset (D4).
    pub fn unit(&self) -> u8 {
        self.unit
    }
    /// Where the jumpers stand now; differs from `unit()` until the next reset (D4).
    pub fn unit_jumpers(&self) -> u8 {
        self.unit_jumpers
    }
    /// The 2 KiB drive RAM, whole.
    pub fn ram(&self) -> &[u8] {
        match &self.board_1581 {
            Some(b) => b.ram(),
            None => &self.ram[..],
        }
    }
    /// The head's current half-track (2 = track 1).
    pub fn half_track(&self) -> u32 {
        match &self.board_1581 {
            // fdd.c:708 `current_half_track = (track + 1) * 2`.
            Some(b) => (b.head().0 as u32 + 1) * 2,
            None => self.rotation.current_half_track,
        }
    }

    /// The VIA ports as the pins see them. Outputs are the composed `ORx | !DDRx`
    /// byte the chip hands its port hooks — what the IEC lines and the mechanism
    /// act on (VIA2's is `oldpb`, the byte its `store_prb` last received).
    pub fn ports(&self) -> DrivePorts {
        use crate::viacore::{VIA_DDRA, VIA_PCR, VIA_PRA};
        let via2_pb = self.via2.oldpb;
        let pcr = self.via2.via[VIA_PCR];
        DrivePorts {
            via1_pa: self.drive_peek(0x1801),
            via1_pb: self.drive_peek(0x1800),
            via1_pb_out: self.via1_pb_iec_output(),
            via2_pa_out: (self.via2.via[VIA_PRA] | !self.via2.via[VIA_DDRA]) & 0xff,
            via2_pb_out: via2_pb,
            via2_pcr: pcr,
            motor_on: via2_pb & 0x04 != 0,
            led_on: self.led_on(),
            step_phase: via2_pb & 0x03,
            density: (via2_pb >> 5) & 0x03,
            // via2d_update_pcr: `read_write_mode = pcrval & 0x20` — bit 5 clear is write.
            write_mode: pcr & 0x20 == 0,
        }
    }

    /// Cold-reset the drive 6502 (VICE drivecpu_reset, drivecpu.c:193-211). Unlike
    /// the old shared-CPU path, the reset is NOT applied by pre-loading PC here:
    /// the verbatim core dispatches it through its IK_RESET path on the FIRST
    /// `drive_6510core_execute` call (the prologue sees `global_pending_int &
    /// IK_RESET`, runs `cpu_reset` → clk=6, then `load_addr($FFFC)` + JUMP). That
    /// reset and the first opcode (SEI) are atomic within one execute call, so the
    /// first sampled record is $EAA1@8 (not a spurious $EAA0@6) — exactly VICE.
    pub fn cold_reset(&mut self) {
        // Spec 870 D4 — the device-ID jumpers as they stand now: the DOS reads them
        // during its reset. (The ROM is NOT taken here — see `latch_rom`.)
        self.unit = self.unit_jumpers;
        self.atn_stop_origin = None;
        let dnr = self.dnr();
        if let Some(b) = self.board_1581.as_mut() {
            // Spec 872 — the 1581's electronics: CPU, CIA, WD (iec.c:108-111). The
            // mechanism and the medium are not the electronics.
            b.reset(dnr);
            b.seed_reset_offset(self.sync_factor);
            self.drive_clk = 0;
            self.last_sample_pc = None;
            self.iec_drv_port = 0x85;
            self.iec_cpu_bus = 0xff;
            return;
        }
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
                number: dnr,
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
                number: dnr,
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
        // The electronics' reset knows nothing of a disk: the rotation model starts
        // empty. A caller that keeps the medium across the reset re-attaches it (the
        // drive's own `reset` / `set_power` do; `Machine::boot_from_dir` has none yet).
        self.disk = None;
        self.rotation = Rotation::new();
    }

    /// Advance the drive's `stop_clk` target by `c64_cycles` of main-CPU time,
    /// applying the machine's sync factor (drivecpu.c:383-390). The integer carry out
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
                .wrapping_add(self.sync_factor.wrapping_mul(tcycles));
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
        if let Some(b) = &self.board_1581 {
            // Spec 872 — the 1581's CIA port B, the same `~drv_data[unit]` shape.
            return b.pb_iec_output();
        }
        (!self.via1_iecbus.drv_data[self.unit as usize]) & 0xff
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
        // Spec 870 §3a — a stopped drive's VIAs are not clocked, so they latch no
        // edge. Remember where ATN stood and where it went; `set_stopped(false)`
        // delivers the net change, the one a clocked edge detector sees on resume.
        if self.stopped {
            if self.atn_stop_origin.is_none() {
                self.atn_stop_origin = Some(if sig != 0 { 0 } else { crate::iec::VIA_SIG_RISE });
            }
            self.atn_stop_latest = sig;
            return;
        }
        if let Some(b) = self.board_1581.as_mut() {
            // Spec 872 — the 1581 takes ATN on its CIA's FLAG pin, falling edge only
            // (iecbus.c:250-252: `if (!iec_old_atn) ciacore_set_flag`). `sig` is RISE
            // exactly when ATN is now low.
            if sig != 0 {
                b.atn_flag();
            }
            return;
        }
        let dnr = self.dnr();
        self.via1.clk = clk;
        let mut backend = Via1dBackend {
            number: dnr,
            iecbus: &mut self.via1_iecbus,
            irq: &mut self.via1_irq,
        };
        viacore::run_pending_alarms(&mut self.via1, &mut backend, clk, 0);
        viacore::viacore_signal(&mut self.via1, &mut backend, crate::iec::VIA_SIG_CA1, sig);
    }

    /// Reset PC from the ROM vector (re-read). Returns the resolved PC.
    pub fn reset_pc(&self) -> u16 {
        if let Some(b) = &self.board_1581 {
            return b.rom()[0x7ffc] as u16 | (b.rom()[0x7ffd] as u16) << 8;
        }
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
        // Spec 870 — off, held in reset or stopped: no cycle runs, and the target does
        // not move either, so a drive let run again continues from where it stood
        // instead of replaying the C64 time it missed.
        if !self.is_clocked() {
            return;
        }
        if let Some(b) = self.board_1581.as_mut() {
            // Spec 872 — the 1581 board, at twice the 1541's ratio (clock_frequency 2).
            let dnr = (self.unit - 8) as usize;
            b.run_cycles(n, self.sync_factor, dnr, self.iec_drv_port, self.iec_cpu_bus, &self.iec_drv_bus);
            self.drive_clk = b.drive_clk;
            return;
        }
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
        // Spec 871 — the other devices' pulls, so a `$1800` store re-folds against
        // the whole bus (VICE's one global `iecbus`), not against this drive alone.
        // Spec 874 — slots 4-7 too, where a host's device may stand (`0xff` without).
        let own = self.unit as usize;
        for slot in 4..(8 + crate::iec::NUM_DISK_UNITS) {
            if slot != own {
                self.via1_iecbus.drv_bus[slot] = self.iec_drv_bus[slot];
            }
        }
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
        let dnr = (self.unit - 8) as usize;
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
            cpu_last_data: &mut self.cpu_last_data,
            pending_set_overflow: false,
            number: dnr,
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
    /// `c64_clk`). A monotonic no-op when `c64_clk <= c64_ref`. A drive that is not
    /// clocked (Spec 870) runs nothing and is re-anchored all the same.
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
        self.attach_disk_with_unreported_write(image, false);
    }

    /// [`Self::attach_disk`] for a disk that goes back into a drive with a write in
    /// `image.bytes` that the flush which took it there did not report (the flush's
    /// `true`) — a reset keeping the disk, a C64 power cycle carrying it through the
    /// media registry. The next [`Self::flush_disk_writeback`] reports it.
    pub fn attach_disk_with_unreported_write(&mut self, image: DiskImage, unreported: bool) {
        if let Err(e) = self.refuse_medium_for_host().and_then(|_| self.medium_fits(&image.kind)) {
            // A caller that can refuse goes through `mount`; this path has no answer to
            // give, so the medium does not go in.
            eprintln!("[drive] attach refused: {e}");
            return;
        }
        if let Some(b) = self.board_1581.as_mut() {
            // Spec 872 D3 — the D81 goes into the 1581's mechanism; `disk.bytes` stays
            // the medium the host persists, the FDD's image the working copy.
            b.attach(image.bytes.clone(), image.read_only);
            self.disk_synced_gen = b.image_gen();
            self.disk = Some(image);
            self.disk_write_unreported = unreported;
            return;
        }
        let (gcr, wb_kind) = match image.kind {
            DiskKind::D64 => (Some(GcrImage::from_d64(&image.bytes)), WritebackKind::D64),
            DiskKind::G64 => (Some(GcrImage::from_g64(&image.bytes)), WritebackKind::G64),
            DiskKind::D81 => (None, WritebackKind::D64),
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
        self.disk_synced_gen = self.rotation.writeback_gen;
        self.disk_write_unreported = unreported;
    }

    /// Detach (eject) the disk from this drive. Flushes any pending dirty track
    /// back into `disk.bytes` first (VICE `drive_image_detach` →
    /// `drive_gcr_data_writeback`); the image leaves the drive with it, and the
    /// caller that wants it in the host file persists before the eject.
    pub fn detach_disk(&mut self) {
        self.flush_disk_writeback();
        self.disk = None;
        if let Some(b) = self.board_1581.as_mut() {
            b.detach();
            return;
        }
        self.rotation.detach();
    }

    /// Bring `self.disk.bytes` (the image the daemon persists/hashes/snapshots) up
    /// to the drive's write-back image: flush the pending dirty track into it (VICE
    /// `drive_gcr_data_writeback_all` before `fsimage->fd` is read), and copy it out
    /// whenever a track has been folded in since the last time — by this flush or
    /// by a head move, which folds the track it leaves without asking anyone.
    /// Cheap no-op when nothing was written. Returns whether `disk.bytes` holds a
    /// write no flush has reported before: one this flush brought in, or one the
    /// drive's reset or power switch brought in since the last flush. Each write is
    /// reported once — the daemon's lazy host-file write arms on that report.
    pub fn flush_disk_writeback(&mut self) -> bool {
        self.sync_disk_bytes();
        std::mem::take(&mut self.disk_write_unreported)
    }

    /// The drive's own half of [`Self::flush_disk_writeback`]: bring `disk.bytes` up
    /// to the write-back image and note a write it took as not yet reported.
    fn sync_disk_bytes(&mut self) {
        if let Some(b) = self.board_1581.as_mut() {
            b.flush();
            if b.image_gen() == self.disk_synced_gen {
                return;
            }
            self.disk_synced_gen = b.image_gen();
            if let (Some(img), Some(disk)) = (b.image(), self.disk.as_mut()) {
                disk.bytes.clone_from(img);
                self.disk_write_unreported = true;
            }
            return;
        }
        self.rotation.drive_gcr_data_writeback_all();
        if self.rotation.writeback_gen == self.disk_synced_gen {
            return;
        }
        self.disk_synced_gen = self.rotation.writeback_gen;
        if let (Some(synced), Some(disk)) = (self.rotation.writeback_bytes.as_ref(), self.disk.as_mut()) {
            disk.bytes.clone_from(synced);
            self.disk_write_unreported = true;
        }
    }

    /// The attached disk as a flush would leave it — the image a persist writes —
    /// built on a copy: [`Rotation::writeback_image`] (the write-back image with the
    /// pending dirty track folded in by the same encoder). The drive is left exactly
    /// as it was: the dirty track stays dirty, `disk.bytes` is not brought up to
    /// date. What a checkpoint embeds as the drive's medium. `None` without a disk.
    ///
    /// [`Rotation::writeback_image`]: crate::rotation::Rotation::writeback_image
    pub fn disk_as_written(&self) -> Option<DiskImage> {
        let disk = self.disk.as_ref()?;
        let bytes = match &self.board_1581 {
            Some(b) => b.image_as_written().unwrap_or_else(|| disk.bytes.clone()),
            None => self.rotation.writeback_image().unwrap_or_else(|| disk.bytes.clone()),
        };
        Some(DiskImage {
            kind: disk.kind.clone(),
            bytes,
            backing_path: disk.backing_path.clone(),
            read_only: disk.read_only,
        })
    }

    /// Get a reference to the currently attached disk image, if any.
    ///
    /// This does NOT flush in-flight drive writes (it borrows `&self`): `bytes`
    /// may be behind the drive. A caller that WRITES the disk to its file flushes
    /// first ([`flush_disk_writeback`], VICE `drive_gcr_data_writeback_all` before
    /// reading `fsimage->fd`). A caller that only reads the content — a hash, a
    /// snapshot — takes [`disk_as_written`] instead: a flush reports a new write
    /// once, and the daemon's lazy host-file write arms itself on that report.
    /// Metadata-only callers (backing path, kind) need neither.
    /// [`flush_disk_writeback`]: Drive1541::flush_disk_writeback
    /// [`disk_as_written`]: Drive1541::disk_as_written
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
        let dnr = self.dnr();
        let mut backend = Via1dBackend {
            number: dnr,
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
        let dnr = self.dnr();
        let mut backend = Via2dBackend {
            drive: &mut self.rotation,
            number: dnr,
            irq: &mut self.via2_irq,
            pending_set_overflow: false,
            has_image,
        };
        f(&mut self.via2, &mut backend)
    }

    /// Spec 870 — the part state for a checkpoint.
    pub fn part(&self) -> DrivePart {
        DrivePart {
            powered: self.powered,
            reset_held: self.reset_held,
            stopped: self.stopped,
            reset_line_connected: self.reset_line_connected,
            unit: self.unit,
            unit_jumpers: self.unit_jumpers,
            atn_stop_origin: self.atn_stop_origin,
            atn_stop_latest: self.atn_stop_latest,
            board_type: board_name(self.board_type()).parse().unwrap_or(1541),
        }
    }

    /// Spec 870 — put the part state back exactly (a restore: no reset runs, no
    /// power-on clears RAM). A unit outside 8-11 is refused and nothing changes.
    pub(crate) fn restore_part(&mut self, p: &DrivePart) -> Result<(), String> {
        for u in [p.unit, p.unit_jumpers] {
            if !(8..=11).contains(&u) {
                return Err(format!("drivePart: unit {u} is not 8-11"));
            }
        }
        // Spec 872 §6 — a position whose type changes on restore gets its power-on for
        // the ROM (the path 871 built for B).
        let t = if p.board_type == 1581 { crate::iec::DriveType::Drive1581 } else { crate::iec::DriveType::Drive1541 };
        if t != self.board_type() {
            self.force_board_type(t);
            self.latch_rom();
        }
        self.powered = p.powered;
        self.reset_held = p.reset_held;
        if let Some(b) = self.board_1581.as_mut() {
            b.set_number((p.unit - 8) as usize);
            b.resync_iec_output();
        }
        self.stopped = p.stopped;
        self.reset_line_connected = p.reset_line_connected;
        self.unit = p.unit;
        self.unit_jumpers = p.unit_jumpers;
        self.atn_stop_origin = p.atn_stop_origin;
        self.atn_stop_latest = p.atn_stop_latest;
        Ok(())
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

    /// Bring the disk rotation up to the drive clock (what a VIA2 access does first).
    pub(crate) fn snapshot_catch_up_rotation(&mut self) {
        let clk = self.core.clk;
        self.rotation.rotate_disk(clk);
    }

    /// The second half of VICE's `interrupt_restore_irq` (interrupt.c:191-198),
    /// which the VIA snapshot read calls through `restore_int`: each source's
    /// `pending_int` IRQ bit follows the level its VIA was restored with (the VIA
    /// backends' `restore_int` already set the level mirror). `nirq`, `irq_clk` and
    /// `global_pending_int` are not touched — they came from the DRIVECPU module.
    pub(crate) fn snapshot_restore_pending_int(&mut self) {
        use crate::drive_6510core::IK_IRQ;
        for (n, active) in [self.via1_irq.active, self.via2_irq.active].into_iter().enumerate() {
            if active {
                self.int.pending_int[n] |= IK_IRQ;
            } else {
                self.int.pending_int[n] &= !IK_IRQ;
            }
        }
    }

    /// Spec 871 — a restored drive is mid-program, not at a reset. A drive that has
    /// never run since its last `cold_reset` still has the hardware reset armed
    /// (`reset_pending` + `IK_RESET`). `reset_pending` is TRX64's own latch and rides
    /// no module, and a DRIVECPU module older than 1.4 carries no interrupt status to
    /// overwrite `IK_RESET`, so without this its first catch-up after the restore
    /// would run the reset sequence over the restored CPU. Position B is such a drive
    /// whenever it was off until the restore.
    pub(crate) fn snapshot_clear_pending_reset(&mut self) {
        self.reset_pending = false;
        self.int.global_pending_int &= !IK_RESET;
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
        if let Some(b) = &self.board_1581 {
            return b.ram()[(addr & 0x1fff) as usize];
        }
        self.ram[(addr & 0x07FF) as usize]
    }

    /// The activity LED — VIA2 port B bit 3, driven only when DDRB says that pin is an
    /// output (VICE's `drive->led_status` is set from the same bit).
    ///
    /// There is no `led` field in this port; the monitor's drive panel simply omitted the
    /// LED because of that, losing the one at-a-glance signal for "is the drive doing
    /// something". Derived here rather than mirrored into new state, so it cannot go
    /// stale.
    pub fn led_on(&self) -> bool {
        if let Some(b) = &self.board_1581 {
            return b.led_on();
        }
        let prb = self.via2.via[crate::viacore::VIA_PRB];
        let ddrb = self.via2.via[crate::viacore::VIA_DDRB];
        (prb & ddrb & 0x08) != 0
    }

    /// Side-effect-free peek of the drive CPU's address space (= the monitor-shell
    /// `driveProbe.peek` used by `device drive8` r/m/d). Same decode as `read` — RAM
    /// windows, the four VIA images, open bus, ROM — but the VIA registers are read
    /// without their port hooks, so a peek never turns the disk, clears
    /// byte_ready/IFR or dispatches a timer alarm. Read-inspect only.
    pub fn drive_peek(&self, addr: u16) -> u8 {
        if let Some(b) = &self.board_1581 {
            return b.peek(addr);
        }
        // via1d1541.c:345 — driveid = (number << 5) & 0x60, the device-ID jumpers.
        let driveid = ((self.dnr() << 5) & 0x60) as u8;
        match addr {
            // VIA1 ($1800) — the IEC port, and the register that answers "who is
            // holding DATA low". The whole window used to peek as `0`, both DDRs
            // included, which is impossible for a running 1541 and read as a fact.
            //
            // PRB is composed here rather than through the backend so the peek can
            // stay `&self`: it is the same expression `Via1dBackend::read_prb` uses
            // (via1d1541.c:345-350) —
            //     tmp  = (drv_port ^ 0x85) | 0x1a | driveid
            //     byte = (PRB & DDRB) | (tmp & ~DDRB)
            // — and it reads the IEC core, which has no state to disturb.
            0x1800..=0x1BFF => {
                let a = (addr & 0xf) as usize;
                if a == crate::viacore::VIA_PRB {
                    let ctx = &self.via1;
                    let tmp = ((self.via1_iecbus.drv_port ^ 0x85) | 0x1a | driveid) & 0xff;
                    let ddrb = ctx.via[crate::viacore::VIA_DDRB];
                    ((ctx.via[crate::viacore::VIA_PRB] & ddrb) | (tmp & !ddrb)) & 0xff
                } else {
                    viacore::viacore_peek_no_hooks(&self.via1, addr)
                }
            }
            // VIA2 ($1C00) — the disk controller. Its port reads TURN THE DISK
            // (`rotate_disk`, and they clear `byte_ready_level`), so a debugger may
            // not call them: inspecting the drive would change it. Latches, DDRs,
            // timers and IFR/IER are exact; PRA/PRB report what the VIA drives
            // rather than what the head sees.
            0x1C00..=0x1FFF => viacore::viacore_peek_no_hooks(&self.via2, addr),
            // The rest of the 1541 decode, same windows as `read`/`write`: the VIAs
            // appear FOUR times ($1800/$3800/$5800/$7800 and $1C00/$3C00/$5C00/
            // $7C00), RAM only in 2 KB blocks every 8 KB, and the gaps are open bus.
            // A debugger that showed RAM where the chip has a VIA mirror would be
            // describing a machine nobody is running.
            0x0000..=0x7FFF => {
                let page = addr >> 8;
                match page & 0x1f {
                    0x18..=0x1b => {
                        let a = (addr & 0xf) as usize;
                        if a == crate::viacore::VIA_PRB {
                            let ctx = &self.via1;
                            let tmp = ((self.via1_iecbus.drv_port ^ 0x85) | 0x1a | driveid) & 0xff;
                            let ddrb = ctx.via[crate::viacore::VIA_DDRB];
                            ((ctx.via[crate::viacore::VIA_PRB] & ddrb) | (tmp & !ddrb)) & 0xff
                        } else {
                            viacore::viacore_peek_no_hooks(&self.via1, addr)
                        }
                    }
                    0x1c..=0x1f => viacore::viacore_peek_no_hooks(&self.via2, addr),
                    z if z < 0x08 => self.ram[(addr & 0x07FF) as usize],
                    // `drive_peek_free` — the open bus, reported and not invented.
                    _ => self.cpu_last_data,
                }
            }
            0x8000..=0xFFFF => self.rom[(addr & 0x7FFF) as usize],
        }
    }

    /// Write a byte of the drive's 2 KB RAM (mirrored every $800). Used to poke
    /// the DOS job queue directly ($00=$80 READ, $06/$07 = track/sector) to drive
    /// a sector read without the full IEC command handshake.
    #[inline]
    pub fn drive_ram_write(&mut self, addr: u16, val: u8) {
        if let Some(b) = self.board_1581.as_mut() {
            b.ram_mut()[(addr & 0x1fff) as usize] = val;
            return;
        }
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
        if let Some(b) = self.board_1581.as_mut() {
            return b.sample_pc_change();
        }
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

/// Spec 870 — the drive-as-a-part state a checkpoint carries (`drivePart`): power,
/// reset held, stopped, the reset-line connection and the unit number (in force and
/// on the jumpers), plus the ATN change a stopped drive has not yet seen. A
/// checkpoint without the node restores [`DrivePart::default`] — the stock drive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DrivePart {
    pub powered: bool,
    pub reset_held: bool,
    pub stopped: bool,
    pub reset_line_connected: bool,
    pub unit: u8,
    pub unit_jumpers: u8,
    #[serde(default)]
    pub atn_stop_origin: Option<u8>,
    #[serde(default)]
    pub atn_stop_latest: u8,
    /// Spec 872 §6 — the board in the position: 1541 or 1581. Absent (a checkpoint from
    /// before 872) is a 1541, and a 1541 is not written, so a checkpoint of a machine
    /// without a 1581 is the tree it was.
    #[serde(default = "board_type_1541", skip_serializing_if = "is_board_1541")]
    pub board_type: u16,
}

fn board_type_1541() -> u16 {
    1541
}

fn is_board_1541(t: &u16) -> bool {
    *t == 1541
}

impl Default for DrivePart {
    fn default() -> Self {
        Self {
            powered: true,
            reset_held: false,
            stopped: false,
            reset_line_connected: true,
            unit: 8,
            unit_jumpers: 8,
            atn_stop_origin: None,
            atn_stop_latest: 0,
            board_type: 1541,
        }
    }
}

impl DrivePart {
    /// Spec 871 — the part state a position starts in: A on at unit 8 (the stock
    /// drive, [`DrivePart::default`]), B off with its jumpers at 9.
    pub fn default_for(pos: DrivePosition) -> Self {
        match pos {
            DrivePosition::A => Self::default(),
            DrivePosition::B => Self { powered: false, unit: 9, unit_jumpers: 9, ..Self::default() },
        }
    }
}

/// Spec 871 — the machine's two drive positions, as the U64 has them. Inside the
/// machine a drive is a position; on the wire it is addressed by the unit number it
/// answers to (§8, decided: unit numbers on the wire, positions inside).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DrivePosition {
    /// `Machine::drive8` — the name is historical; A can stand at unit 8-11.
    A,
    /// `Machine::drive_b`.
    B,
}

impl DrivePosition {
    /// "A" / "B" — the name a refusal uses.
    pub fn name(self) -> &'static str {
        match self {
            DrivePosition::A => "A",
            DrivePosition::B => "B",
        }
    }
}

/// Spec 871 D2 — the IEC slots the two positions occupy: each drive's
/// [`Drive1541::bus_slot`], except that B never shares A's (the machine refuses that
/// configuration; if it arises anyway, B stays off the bus).
#[inline]
pub fn pair_bus_slots(a: &Drive1541, b: &Drive1541) -> (Option<usize>, Option<usize>) {
    let sa = a.bus_slot();
    let sb = b.bus_slot();
    (sa, if sb.is_some() && sb == sa { None } else { sb })
}

/// Spec 872 — tell the IEC core which board answers at each occupied slot, so the
/// conf1/2/3 paths fold and signal ATN per type. Two array stores per sync point.
#[inline]
pub fn pair_slot_types(a: &Drive1541, b: &Drive1541, sa: Option<usize>, sb: Option<usize>, iec: &mut crate::iec::IecCore) {
    if let Some(s) = sa {
        iec.set_slot_type(s, a.board_type());
    }
    if let Some(s) = sb {
        iec.set_slot_type(s, b.board_type());
    }
}

/// Spec 871 D2 — catch both drives up to the C64-clock `target` and put their port-B
/// outputs into their slots WITHOUT the wired-AND fold (the `$DD00` write path folds
/// once itself). Both are fed the bus as it stands BEFORE either runs, and both run
/// before either is written back, so the order they are advanced in cannot change
/// what the bus reads. B that is not clocked costs two flag tests. Returns the new
/// catch-up reference.
#[inline]
pub fn pair_catch_up(
    a: &mut Drive1541,
    b: &mut Drive1541,
    iec: &mut crate::iec::IecCore,
    target: u64,
    c64_ref: u64,
    c64_pa_out: u8,
) -> u64 {
    let (sa, sb) = pair_bus_slots(a, b);
    pair_slot_types(a, b, sa, sb, iec);
    iec.sync_drive_slots(sa, sb, c64_pa_out);
    let b_runs = b.is_clocked();
    a.feed_iec(iec);
    if b_runs {
        b.feed_iec(iec);
    }
    let r = a.catch_up_to(target, c64_ref);
    if b_runs {
        b.catch_up_to(target, c64_ref);
    }
    if let Some(slot) = sa {
        iec.drive_set_data_no_fold_slot(slot, a.via1_pb_iec_output());
    }
    if let Some(slot) = sb {
        iec.drive_set_data_no_fold_slot(slot, b.via1_pb_iec_output());
    }
    r
}

/// Spec 870/871 — fold both drives' VIA1 port-B outputs into the IEC core (= VICE
/// `iec_drive_write(~byte, dnr)` per drive), each into its own slot; a drive that is
/// off or held folds nothing. The wired-AND is the AND of every slot, so the order
/// of the two folds does not matter.
#[inline]
pub fn pair_fold_into_iec(a: &Drive1541, b: &Drive1541, iec: &mut crate::iec::IecCore, c64_pa_out: u8) {
    let (sa, sb) = pair_bus_slots(a, b);
    pair_slot_types(a, b, sa, sb, iec);
    iec.sync_drive_slots(sa, sb, c64_pa_out);
    if let Some(slot) = sa {
        iec.iec_drive_write_typed(!a.via1_pb_iec_output(), slot - 8, a.board_type());
    }
    if let Some(slot) = sb {
        iec.iec_drive_write_typed(!b.via1_pb_iec_output(), slot - 8, b.board_type());
    }
}

/// Spec 871 D2 — deliver an ATN edge the IEC core computed for drive number `dnr`
/// (slot `dnr + 8`) to whichever position answers there. `edge` is the IEC core's
/// per-type decision: VIA1 CA1 for a 1541, the CIA's FLAG for a 1581 (Spec 872).
#[inline]
pub fn pair_deliver_atn_edge(a: &mut Drive1541, b: &mut Drive1541, dnr: usize, edge: crate::iec::AtnEdge) {
    use crate::iec::AtnEdge;
    let sig = match edge {
        AtnEdge::Via1Ca1 { sig } => sig,
        // `if (!iec_old_atn) ciacore_set_flag` — FLAG fires on the falling ATN only;
        // carried as the CA1 code the stopped-drive bookkeeping already speaks.
        AtnEdge::Cia1581Flag { fire } => {
            if fire {
                crate::iec::VIA_SIG_RISE
            } else {
                0
            }
        }
        // No position holds a 2000/4000/CMD HD.
        _ => return,
    };
    pair_deliver_atn(a, b, dnr, sig);
}

/// [`pair_deliver_atn_edge`] with the VIA1 CA1 edge code.
#[inline]
pub fn pair_deliver_atn(a: &mut Drive1541, b: &mut Drive1541, dnr: usize, sig: u8) {
    let (sa, sb) = pair_bus_slots(a, b);
    let slot = Some(dnr + 8);
    if sa == slot {
        let clk = a.drive_clk;
        a.atn_edge_to_via1_ca1(sig, clk);
    } else if sb == slot {
        let clk = b.drive_clk;
        b.atn_edge_to_via1_ca1(sig, clk);
    }
}

/// Spec 870 D5 — the drive's VIA ports as the pins see them, read without side
/// effects. See [`Drive1541::ports`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DrivePorts {
    /// VIA1 port A (`$1801`), unused on a 1541 without a parallel cable.
    pub via1_pa: u8,
    /// VIA1 port B as a `$1800` read would return it now: IEC inputs, jumpers, outputs.
    pub via1_pb: u8,
    /// VIA1 port B output driving the IEC lines: PB1 DATA, PB3 CLK, PB4 ATN-ack.
    pub via1_pb_out: u8,
    /// VIA2 port A output (the GCR byte latch toward the head in write mode).
    pub via2_pa_out: u8,
    /// VIA2 port B output the mechanism acts on: PB0-1 stepper, PB2 motor, PB3 LED,
    /// PB5-6 density.
    pub via2_pb_out: u8,
    /// VIA2 PCR — CA2 byte-ready enable, CB2 read/write mode.
    pub via2_pcr: u8,
    /// Spindle motor on (VIA2 PB2).
    pub motor_on: bool,
    /// Activity LED lit — [`Drive1541::led_on`].
    pub led_on: bool,
    /// Stepper phase (VIA2 PB0-1).
    pub step_phase: u8,
    /// Density zone (VIA2 PB5-6), 0-3.
    pub density: u8,
    /// Write mode (VIA2 PCR bit 5 clear — CB2 low), as `via2d_update_pcr` reads it.
    pub write_mode: bool,
}

impl Default for Drive1541 {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BUG-045 follow-up — the drive's activity LED is the LED, not the motor.
    ///
    /// `session/drive_status` used to report `ledOn = motorOn`, which is a
    /// different fact: the motor keeps spinning through idle waits and spin-down,
    /// so a drive doing nothing looked busy and a drive working looked the same.
    /// The real line is VIA2 PB bit 3, and it is only driven when DDRB says that
    /// pin is an output.
    #[test]
    fn the_activity_led_is_pb3_and_not_the_motor() {
        use crate::viacore::{VIA_DDRB, VIA_PRB};
        let mut d = Drive1541::new();
        d.via2.via[VIA_DDRB] = 0xff; // every PB pin an output

        // Motor on (bit 2), LED off — the state that used to read as "busy".
        d.via2.via[VIA_PRB] = 0x04;
        assert!(!d.led_on(), "a spinning motor is not an active LED");

        // LED on, motor off — the state that used to be invisible.
        d.via2.via[VIA_PRB] = 0x08;
        assert!(d.led_on(), "PB3 high with PB3 an output means the LED is lit");

        // A pin that is an INPUT drives nothing, whatever the register holds.
        d.via2.via[VIA_DDRB] = 0xff & !0x08;
        assert!(!d.led_on(), "PB3 as an input cannot light the LED");
    }

    /// The LED's brightness is a DUTY CYCLE, integrated — not the level sampled
    /// at whatever moment a client happened to poll. A fastloader pulses the LED
    /// far faster than anyone polls; sampling it turns "working" into a coin flip.
    #[test]
    fn led_brightness_integrates_the_duty_cycle() {
        let mut r = crate::rotation::Rotation::new();

        // Held on for the whole period → full brightness.
        r.led_last_change_clk = 0;
        r.led_last_uiupdate_clk = 0;
        assert_eq!(r.led_pwm(1000, true), 1000, "on for the whole period");

        // Held off for the whole period → dark.
        assert_eq!(r.led_pwm(2000, false), 0, "off for the whole period");

        // On for half of it: 500/1000 linear, bent by the perceptual square root
        // VICE applies (drive.c:910-915) → ~707, i.e. clearly bright rather than
        // "half", which is the point of the curve.
        r.led_active_ticks = 500;
        let pwm = r.led_pwm(3000, false);
        assert!(
            (700..=715).contains(&pwm),
            "a 50% duty cycle must read as clearly lit, got {pwm}"
        );

        // Reading CONSUMES the accumulator — the question is "since you last asked".
        assert_eq!(r.led_active_ticks, 0);

        // More on-time than the period (a reset while lit) clamps instead of
        // overflowing past 1000 — drive.c:902-907 guards the same case.
        r.led_active_ticks = 10_000;
        assert_eq!(r.led_pwm(4000, false), 1000);
    }

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
                cpu_last_data: &mut d.cpu_last_data,
                pending_set_overflow: false,
                number: 0,
            };
            bus.write(0x0010, 0xAB);

            // The RAM mirrors are 2 KB blocks every 8 KB — VICE `memiec_init`
            // overlays $0000-$07FF, $2000-$27FF, $4000-$47FF, $6000-$67FF and
            // NOTHING in between.
            assert_eq!(bus.read(0x2010), 0xAB, "$2010 mirrors $0010");
            assert_eq!(bus.read(0x4010), 0xAB, "$4010 mirrors $0010");
            assert_eq!(bus.read(0x6010), 0xAB, "$6010 mirrors $0010");

            // $0810 is NOT a mirror. This assertion used to read
            //     assert_eq!(bus.read(0x0810), 0xAB, "$0810 should mirror $0010");
            // and it is why the wrong decode survived: the whole of $0000-$7FFF
            // was RAM here except the two VIA windows, so $0800-$17FF answered
            // from RAM instead of the open bus — and three of the FOUR VIA images
            // ($3800/$5800/$7800 and $3C00/$5C00/$7C00) were RAM as well. Drive
            // code that reaches a VIA through a mirror wrote into RAM and pulled
            // no line at all.
            bus.write(0x0011, 0x5A); // put a known byte on the bus
            assert_eq!(
                bus.read(0x0810),
                0x5A,
                "$0810 is unmapped — it reads the open bus (cpu_last_data), not RAM"
            );
            assert_eq!(bus.read(0x2810), 0x5A, "$2810 is unmapped too");
            assert_eq!(bus.read(0x4810), 0x5A, "$4810 is unmapped too");
            assert_eq!(bus.read(0x6810), 0x5A, "$6810 is unmapped too");
        }
    }

    /// The 1541 has ONE VIA1 and ONE VIA2, and the address decoder shows each of
    /// them four times below $8000. Reaching a VIA through a mirror must land on
    /// the chip — it is a common way for drive code to save a byte, and if the
    /// mirror is RAM the write is swallowed and no bus line moves.
    #[test]
    fn the_drive_vias_appear_at_all_four_of_their_addresses() {
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
            cpu_last_data: &mut d.cpu_last_data,
            pending_set_overflow: false,
            number: 0,
        };

        // VIA1: write the DDRB latch through the base window, read it back through
        // each mirror. A RAM mirror would answer with whatever RAM holds there.
        bus.write(0x1802, 0x5a);
        for a in [0x1802u16, 0x3802, 0x5802, 0x7802] {
            assert_eq!(bus.read(a), 0x5a, "VIA1 DDRB through ${a:04X}");
        }
        // ...and a write through a MIRROR reaches the same register.
        bus.write(0x7802, 0xa5);
        assert_eq!(bus.read(0x1802), 0xa5, "a write through $7802 reaches VIA1");

        // VIA2 the same, through its four windows.
        bus.write(0x1c02, 0x3c);
        for a in [0x1c02u16, 0x3c02, 0x5c02, 0x7c02] {
            assert_eq!(bus.read(a), 0x3c, "VIA2 DDRB through ${a:04X}");
        }
        bus.write(0x5c02, 0xc3);
        assert_eq!(bus.read(0x1c02), 0xc3, "a write through $5C02 reaches VIA2");
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
            cpu_last_data: &mut d.cpu_last_data,
            pending_set_overflow: false,
            number: 0,
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
            cpu_last_data: &mut d.cpu_last_data,
            pending_set_overflow: false,
            number: 0,
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
            cpu_last_data: &mut d.cpu_last_data,
            pending_set_overflow: false,
            number: 0,
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
