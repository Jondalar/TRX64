//! iec.rs — the C64↔1541 IEC serial bus.
//!
//! # STRICT 1:1 PORT
//!
//! This is a line-for-line port of the TS oracle bus model:
//!
//!   • `vice1541/iecbus.ts`  (VICE `src/iecbus/iecbus.c` — the `iecbus_t` bus, the
//!     conf0..conf3 read/write callbacks, `iecbus_init`, `iecbus_cpu_undump`,
//!     `iecbus_status_set`, `calculate_callback_index`, `iecbus_device_*`).
//!   • `vice1541/c64iec.ts` (VICE `src/c64/c64iec.c` — `iec_update_cpu_bus`,
//!     `iec_update_ports`, `iec_update_ports_embedded`, `iec_drive_write`,
//!     `iec_drive_read`, `iecbus_drive_port`, `c64iec_init`, etc.).
//!
//! Every VICE function is ported with its verbatim snake_case name as a method
//! (or free fn) carrying a `ts:`/`vice:` tag. The full 16-unit `iecbus_t` arrays
//! (`drv_bus[16]`, `drv_data[16]`) are kept — NOT the old single-unit distillation
//! — together with `drv_port`, `cpu_bus`, `cpu_port`, `iec_fast_1541`, and the
//! module-level `iec_old_atn` / `iecbus_device[16]` / callback selector.
//!
//! ## Cross-domain wiring note (NOT a deviation from VICE)
//!
//! In VICE `iecbus_cpu_write_conf1` calls `drive_cpu_execute_one(unit, clock)` and
//! `viacore_signal(unit->via1d1541, VIA_SIG_CA1, ...)` directly, because the drive
//! lives in the same address space. In TRX64 the drive (`Drive1541`) is a SEPARATE
//! borrow from the IEC core (the `Machine` split-borrows them into `FullBus`), so
//! `IecbusT` cannot reach the drive. Those two drive-touching steps therefore stay
//! in `full.rs` (`iec_catch_up_to` = `drive_cpu_execute_one`; the `atn_edge_to_
//! via1_ca1` call = the `viacore_signal(VIA_SIG_CA1)` branch), wired EXACTLY in the
//! VICE order: execute_one → `iec_update_cpu_bus` → ATN-edge → per-type drv_bus
//! recompute → `iec_update_ports`. `iecbus_cpu_write_conf1` here performs every
//! step it owns and RETURNS the ATN-edge decision so `full.rs` can fire the CA1
//! signal at the drive clock the catch-up reached — the same edge VICE computes.
//!
//! Line semantics (open-drain, wired-AND): a bit SET (=1) means "this driver is
//! NOT asserting" (line released / pulled HIGH); a bit CLEAR (=0) means "asserting"
//! (line pulled LOW). The effective line is the AND of every driver.

// =============================================================================
// ts: vice1541/drivetypes.ts — IECBUS_NUM / NUM_DISK_UNITS / DRIVE_TYPE_*
// vice: src/iecbus.h:35 / src/drive/drive.h:44 / src/drive/drivetypes.h
// =============================================================================

/// ts: drivetypes.ts IECBUS_NUM = 16 (vice: src/iecbus.h:35).
pub const IECBUS_NUM: usize = 16;
/// ts: drivetypes.ts NUM_DISK_UNITS = 4 (vice: src/drive/drive.h:44).
pub const NUM_DISK_UNITS: usize = 4;

// Drive type discriminants (ts: drivetypes.ts DRIVE_TYPE_*). Only the `default`
// (1541-class) branch is exercised by the single-1541 shape, but the full switch
// is ported so the per-type drv_bus formula matches VICE byte-for-byte. We carry
// the type per unit as a small enum so the `switch (unit.type)` reads 1:1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriveType {
    /// 1540/1541/1570/1571/2031/1001/… — the default VICE branch (via1d1541 CA1).
    Drive1541,
    /// DRIVE_TYPE_1581 (cia1581).
    Drive1581,
    /// DRIVE_TYPE_2000 (via4000 CA2).
    Drive2000,
    /// DRIVE_TYPE_4000 (via4000 CA2).
    Drive4000,
    /// DRIVE_TYPE_CMDHD (cmdhd via10 CA1).
    DriveCmdHd,
}

// =============================================================================
// ts: iecbus.ts:101-106 (IECBUS_DEVICE_READ_/_WRITE_/ATNA bits)
// vice: src/iecbus.h:46-54
// =============================================================================

/// ts: iecbus.ts:101 (iecbus.h:46) IECBUS_DEVICE_READ_DATA.
pub const IECBUS_DEVICE_READ_DATA: u8 = 0x01;
/// ts: iecbus.ts:102 (iecbus.h:47) IECBUS_DEVICE_READ_CLK.
pub const IECBUS_DEVICE_READ_CLK: u8 = 0x04;
/// ts: iecbus.ts:103 (iecbus.h:48) IECBUS_DEVICE_READ_ATN.
pub const IECBUS_DEVICE_READ_ATN: u8 = 0x80;
/// ts: iecbus.ts:104 (iecbus.h:50) IECBUS_DEVICE_ATNA.
pub const IECBUS_DEVICE_ATNA: u8 = 0x10;
/// ts: iecbus.ts:105 (iecbus.h:52) IECBUS_DEVICE_WRITE_CLK.
pub const IECBUS_DEVICE_WRITE_CLK: u8 = 0x40;
/// ts: iecbus.ts:106 (iecbus.h:53) IECBUS_DEVICE_WRITE_DATA.
pub const IECBUS_DEVICE_WRITE_DATA: u8 = 0x80;

// =============================================================================
// ts: iecbus.ts:112-115 (IECBUS_STATUS_*)  vice: src/iecbus.h:37-40
// =============================================================================

/// ts: iecbus.ts:112 IECBUS_STATUS_TRUEDRIVE.
pub const IECBUS_STATUS_TRUEDRIVE: u8 = 0;
/// ts: iecbus.ts:113 IECBUS_STATUS_DRIVETYPE.
pub const IECBUS_STATUS_DRIVETYPE: u8 = 1;
/// ts: iecbus.ts:114 IECBUS_STATUS_IECDEVICE.
pub const IECBUS_STATUS_IECDEVICE: u8 = 2;
/// ts: iecbus.ts:115 IECBUS_STATUS_TRAPDEVICE.
pub const IECBUS_STATUS_TRAPDEVICE: u8 = 3;

// =============================================================================
// ts: iecbus.ts:123-125 (IECBUS_DEVICE_* device-class)  vice: iecbus.c:52-54
// =============================================================================

/// ts: iecbus.ts:123 (iecbus.c:52) IECBUS_DEVICE_NONE.
pub const IECBUS_DEVICE_NONE: u8 = 0;
/// ts: iecbus.ts:124 (iecbus.c:53) IECBUS_DEVICE_TRUEDRIVE.
pub const IECBUS_DEVICE_TRUEDRIVE: u8 = 1;
/// ts: iecbus.ts:125 (iecbus.c:54) IECBUS_DEVICE_IECDEVICE.
pub const IECBUS_DEVICE_IECDEVICE: u8 = 2;

// =============================================================================
// ts: c64iec.ts:79-81 (IEC_BUS_* bitmask)  vice: src/iecdrive.h:38-40
// =============================================================================

/// ts: c64iec.ts:79 (iecdrive.h:38) IEC_BUS_IEC.
pub const IEC_BUS_IEC: u8 = 0x01;
/// ts: c64iec.ts:80 (iecdrive.h:39) IEC_BUS_IEEE.
pub const IEC_BUS_IEEE: u8 = 0x02;
/// ts: c64iec.ts:81 (iecdrive.h:40) IEC_BUS_TCBM.
pub const IEC_BUS_TCBM: u8 = 0x04;

// =============================================================================
// VIA_SIG_* — ts: drivetypes.ts (vice src/via.h). The CA1/CA2 edge codes the
// ATN-edge branch hands to `viacore_signal`. The actual signal is delivered in
// full.rs (the drive borrow); these constants exist so the conf1/conf2/conf3
// switch reads the same edge value VICE passes.
// =============================================================================

/// ts: drivetypes.ts VIA_SIG_CA1 (vice via.h).
pub const VIA_SIG_CA1: u8 = 0;
/// ts: drivetypes.ts VIA_SIG_CA2 (vice via.h).
pub const VIA_SIG_CA2: u8 = 1;
/// ts: drivetypes.ts VIA_SIG_FALL (vice via.h).
pub const VIA_SIG_FALL: u8 = 0;
/// ts: drivetypes.ts VIA_SIG_RISE (vice via.h).
pub const VIA_SIG_RISE: u8 = 1;

// =============================================================================
// ts: iecbus.ts:80-95 (iecbus_t)  vice: src/iecbus.h:56-83
// =============================================================================

/// ts: iecbus.ts:80 `iecbus_t` (vice: src/iecbus.h:56). VICE struct fields verbatim.
/// `drv_bus` / `drv_data` are `uint8_t[IECBUS_NUM]`.
#[derive(Clone, Copy, Debug)]
pub struct IecbusT {
    /// ts: iecbus.ts:82 — drive output ports as described by IECBUS_DEVICE_WRITE_*.
    pub drv_bus: [u8; IECBUS_NUM],
    /// ts: iecbus.ts:84 — drive output ports as seen by the drive.
    pub drv_data: [u8; IECBUS_NUM],
    /// ts: iecbus.ts:87 — drive input ports, as seen by the drive / READ_* macros.
    pub drv_port: u8,
    /// ts: iecbus.ts:90 — computer output ports as described by WRITE_* macros.
    pub cpu_bus: u8,
    /// ts: iecbus.ts:92 — computer output ports as seen by the computer.
    pub cpu_port: u8,
    /// ts: iecbus.ts:94 — burst-mode (1541 fast IEC) hardware bit.
    pub iec_fast_1541: u8,
}

// =============================================================================
// ts: iecbus.ts:706-723 / iecbus.ts:445-462 (calculate_callback_index dispatch)
// vice: iecbus.c:432-463 — `iecbus_callback_read/_write` function pointers.
// =============================================================================
// VICE selects one of four (read,write) callback pairs by `calculate_callback_
// index`. Rust has no clean module-mutable fn-pointer extern; we model the same
// indirection with an enum tag (Conf0..Conf3) the C64-side dispatch matches on,
// exactly tracking which conf VICE's pointer would hold.

/// ts: iecbus.ts callback selector (vice: the iecbus_callback_read/_write pair).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IecbusCallback {
    /// vice iecbus_cpu_read_conf0 / iecbus_cpu_write_conf0 — no drive enabled.
    Conf0,
    /// vice iecbus_cpu_read_conf1 / iecbus_cpu_write_conf1 — drive 8 only.
    Conf1,
    /// vice iecbus_cpu_read_conf2 / iecbus_cpu_write_conf2 — drive 9 only.
    Conf2,
    /// vice iecbus_cpu_read_conf3 / iecbus_cpu_write_conf3 — arbitrary mix.
    Conf3,
}

// =============================================================================
// ts: iecbus.ts:732-749 (iecbus_device_index[16])  vice: iecbus.c:493-510
// =============================================================================

/// ts: iecbus.ts:732 `iecbus_device_index[16]` (vice: iecbus.c:493-510).
const IECBUS_DEVICE_INDEX: [u8; 16] = [
    IECBUS_DEVICE_NONE,      // 0000
    IECBUS_DEVICE_NONE,      // 0001
    IECBUS_DEVICE_IECDEVICE, // 0010
    IECBUS_DEVICE_IECDEVICE, // 0011
    IECBUS_DEVICE_NONE,      // 0100
    IECBUS_DEVICE_NONE,      // 0101
    IECBUS_DEVICE_IECDEVICE, // 0110
    IECBUS_DEVICE_IECDEVICE, // 0111
    IECBUS_DEVICE_NONE,      // 1000
    IECBUS_DEVICE_NONE,      // 1001
    IECBUS_DEVICE_IECDEVICE, // 1010
    IECBUS_DEVICE_IECDEVICE, // 1011
    IECBUS_DEVICE_TRUEDRIVE, // 1100
    IECBUS_DEVICE_TRUEDRIVE, // 1101
    IECBUS_DEVICE_IECDEVICE, // 1110
    IECBUS_DEVICE_IECDEVICE, // 1111
];

/// ATN-edge action returned by the conf1/conf2/conf3 write path so the caller
/// (`full.rs`, which holds the drive borrow) can deliver the `viacore_signal`
/// that VICE fires inline. `None` = no ATN edge (no signal). One variant per
/// VICE switch-case; the single-1541 shape only ever yields `Via1Ca1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtnEdge {
    /// vice default: `viacore_signal(via1d1541, VIA_SIG_CA1, iec_old_atn ? 0 : RISE)`.
    /// Payload = the edge code (0 = no-edge/clear, VIA_SIG_RISE on ATN low).
    Via1Ca1 { sig: u8 },
    /// vice DRIVE_TYPE_1581: `if (!iec_old_atn) ciacore_set_flag(cia1581)`.
    /// Payload = whether the flag fires (iec_old_atn == 0).
    Cia1581Flag { fire: bool },
    /// vice DRIVE_TYPE_2000/4000: `viacore_signal(via4000, VIA_SIG_CA2, ...)`.
    Via4000Ca2 { sig: u8 },
    /// vice DRIVE_TYPE_CMDHD: `viacore_signal(via10, VIA_SIG_CA1, RISE|FALL)`.
    CmdHdVia10Ca1 { sig: u8 },
}

/// Full IEC bus model — owns the VICE module-level globals of `iecbus.c` as one
/// value the `Machine` holds and split-borrows into the `FullBus`:
///   • `iecbus` (the `iecbus_t` struct)            — ts: iecbus.ts:165
///   • `iec_old_atn`                               — ts: iecbus.ts:178
///   • `iecbus_device[IECBUS_NUM]`                 — ts: iecbus.ts:175
///   • the active callback pair                    — ts: iecbus.ts:139-143
///   • the per-unit drive `type` (for the switch)  — ts: unit.type lookups
#[derive(Clone, Copy, Debug)]
pub struct IecCore {
    /// ts: iecbus.ts:165 `iecbus` (vice: `iecbus_t iecbus;`).
    pub iecbus: IecbusT,
    /// ts: iecbus.ts:178 `iec_old_atn` (vice: `static uint8_t iec_old_atn = 0x10`).
    pub iec_old_atn: u8,
    /// ts: iecbus.ts:175 `iecbus_device[IECBUS_NUM]` (vice: iecbus.c:63).
    pub iecbus_device: [u8; IECBUS_NUM],
    /// ts: iecbus.ts:139-143 active read/write callback pair (vice fn pointers).
    pub iecbus_callback: IecbusCallback,
    /// Per-unit drive `type` (ts: `diskunit_context[dnr].type`). Single-1541 shape:
    /// unit 8 = Drive1541, rest unused. Drives the conf1/2/3 `switch (unit.type)`.
    pub unit_type: [DriveType; IECBUS_NUM],
    /// `c64iec.ts:110` `c64iec_active` (vice: `int c64iec_active = 1;`).
    pub c64iec_active: u8,
}

impl Default for IecCore {
    fn default() -> Self {
        Self::new()
    }
}

impl IecCore {
    /// Power-on construction: equivalent to `iecbus_init()` + `c64iec_init()` +
    /// `iecbus_status_set(TRUEDRIVE, 8, 1)` + `iecbus_status_set(DRIVETYPE, 8, 1)`
    /// (a single 1541 on unit 8). Matches the runtime boot wiring the TS oracle
    /// performs (iec-bus.ts:178-179). After this:
    ///   • iecbus memset 0xff, drv_port = READ_DATA|READ_CLK|READ_ATN = 0x85,
    ///   • iec_old_atn = 0x10, callback = Conf1 (drive 8 truedrive only).
    pub fn new() -> Self {
        let mut s = Self {
            iecbus: IecbusT {
                drv_bus: [0; IECBUS_NUM],
                drv_data: [0; IECBUS_NUM],
                drv_port: 0,
                cpu_bus: 0,
                cpu_port: 0,
                iec_fast_1541: 0,
            },
            iec_old_atn: 0x10,
            iecbus_device: [0; IECBUS_NUM],
            iecbus_callback: IecbusCallback::Conf0,
            unit_type: [DriveType::Drive1541; IECBUS_NUM],
            c64iec_active: 1,
        };
        s.iecbus_init();
        // Power-on cpu_bus/cpu_port released (memset 0xff already set them); the
        // first $DD00 write recomputes them. drv_port is fixed by iecbus_init.
        //
        // Enable a single true-drive on unit 8 EXACTLY as the runtime/VICE does:
        // BOTH IECBUS_STATUS_TRUEDRIVE and IECBUS_STATUS_DRIVETYPE (vice c64bus.c
        // `machine_bus_status_truedrive_set` + `machine_bus_status_drivetype_set`;
        // ts iec-bus.ts:178-179). The resolved per-device nibble is `0b1100` = 12,
        // which `iecbus_device_index[12]` maps to TRUEDRIVE → callback Conf1. Setting
        // TRUEDRIVE alone yields nibble `0b1000` = 8 → NONE → Conf0 (the dead path).
        s.iecbus_status_set(IECBUS_STATUS_TRUEDRIVE, 8, 1);
        s.iecbus_status_set(IECBUS_STATUS_DRIVETYPE, 8, 1);
        s
    }

    // =========================================================================
    // ts: iecbus.ts:313-324 (iecbus_init)  vice: iecbus.c:197-203
    // =========================================================================

    /// ts: iecbus.ts:313 `iecbus_init` (vice: iecbus.c:197-203).
    pub fn iecbus_init(&mut self) {
        // vice: `memset(&iecbus, 0xff, sizeof(iecbus_t));`
        self.iecbus.drv_bus = [0xff; IECBUS_NUM];
        self.iecbus.drv_data = [0xff; IECBUS_NUM];
        self.iecbus.drv_port = 0xff;
        self.iecbus.cpu_bus = 0xff;
        self.iecbus.cpu_port = 0xff;
        self.iecbus.iec_fast_1541 = 0xff;
        // vice: `iecbus.drv_port = IECBUS_DEVICE_READ_DATA|_READ_CLK|_READ_ATN;`
        self.iecbus.drv_port =
            IECBUS_DEVICE_READ_DATA | IECBUS_DEVICE_READ_CLK | IECBUS_DEVICE_READ_ATN;
    }

    // =========================================================================
    // ts: iecbus.ts:357-360 (iecbus_cpu_undump)  vice: iecbus.c:205-209
    // =========================================================================

    /// ts: iecbus.ts:357 `iecbus_cpu_undump` (vice: iecbus.c:205-209).
    pub fn iecbus_cpu_undump(&mut self, data: u8) {
        self.iec_update_cpu_bus(data);
        self.iec_old_atn = self.iecbus.cpu_bus & 0x10;
    }

    // =========================================================================
    // ts: c64iec.ts:171-178 (iec_update_cpu_bus)  vice: c64iec.c:121-124
    // =========================================================================

    /// ts: c64iec.ts:171 `iec_update_cpu_bus` (vice: c64iec.c:121-124).
    /// `data` = the inverted CIA2 PA byte (`~PA`).
    #[inline]
    pub fn iec_update_cpu_bus(&mut self, data: u8) {
        let d = data as u32;
        // vice: `iecbus.cpu_bus = (((data << 2) & 0x80) | ((data << 2) & 0x40)
        //                       | ((data << 1) & 0x10));`
        self.iecbus.cpu_bus =
            (((d << 2) & 0x80) | ((d << 2) & 0x40) | ((d << 1) & 0x10)) as u8;
    }

    // =========================================================================
    // ts: c64iec.ts:185-204 (iec_update_ports)  vice: c64iec.c:126-138
    // =========================================================================

    /// ts: c64iec.ts:185 `iec_update_ports` (vice: c64iec.c:126-138). The wired-AND
    /// fold: `cpu_port = cpu_bus & drv_bus[4..8+NUM_DISK_UNITS]`, then derive
    /// `drv_port`. This IS `iecbus_update_ports` (c64iec_init installs it).
    #[inline]
    pub fn iec_update_ports(&mut self) {
        let mut cpu_port = self.iecbus.cpu_bus as u32;
        // vice: `for (unit = 4; unit < 8 + NUM_DISK_UNITS; unit++)`
        for unit in 4..(8 + NUM_DISK_UNITS) {
            cpu_port &= self.iecbus.drv_bus[unit] as u32;
        }
        self.iecbus.cpu_port = (cpu_port & 0xff) as u8;

        // vice: `iecbus.drv_port = (((cpu_port >> 4) & 0x4) | (cpu_port >> 7)
        //                        | ((cpu_bus << 3) & 0x80));`
        let cp = self.iecbus.cpu_port as u32;
        let cb = self.iecbus.cpu_bus as u32;
        self.iecbus.drv_port =
            (((cp >> 4) & 0x4) | (cp >> 7) | ((cb << 3) & 0x80)) as u8;
    }

    // =========================================================================
    // ts: c64iec.ts:213-215 (iec_update_ports_embedded)  vice: c64iec.c:140-143
    // =========================================================================

    /// ts: c64iec.ts:213 `iec_update_ports_embedded` (vice: c64iec.c:140-143).
    #[inline]
    pub fn iec_update_ports_embedded(&mut self) {
        self.iec_update_ports();
    }

    // =========================================================================
    // ts: c64iec.ts:222-230 (iec_drive_write)  vice: c64iec.c:145-150
    // =========================================================================

    /// ts: c64iec.ts:222 `iec_drive_write` (vice: c64iec.c:145-150). The drive's
    /// VIA1 PB output (`data`) folds into `drv_bus[dnr+8]` + `drv_data[dnr+8]`,
    /// then `iec_update_ports`. `data` = the drive's PB output `(ORB | ~DDRB)`.
    #[inline]
    pub fn iec_drive_write(&mut self, data: u8, dnr: usize) {
        let d = data as u32;
        let cb = self.iecbus.cpu_bus as u32;
        // vice: `iecbus.drv_bus[dnr+8] = (((data << 3) & 0x40)
        //                       | ((data << 6) & ((~data ^ cpu_bus) << 3) & 0x80));`
        let inv = (!d) ^ cb;
        self.iecbus.drv_bus[dnr + 8] =
            (((d << 3) & 0x40) | ((d << 6) & (inv << 3) & 0x80)) as u8;
        self.iecbus.drv_data[dnr + 8] = data;
        self.iec_update_ports();
    }

    // =========================================================================
    // ts: c64iec.ts:237-239 (iec_drive_read)  vice: c64iec.c:152-155
    // =========================================================================

    /// ts: c64iec.ts:237 `iec_drive_read` (vice: c64iec.c:152-155).
    #[inline]
    pub fn iec_drive_read(&self, _dnr: usize) -> u8 {
        self.iecbus.drv_port
    }

    // =========================================================================
    // ts: c64iec.ts:259-268 (iec_available_busses)  vice: c64iec.c:163-171
    // =========================================================================

    /// ts: c64iec.ts:259 `iec_available_busses` (vice: c64iec.c:163-171).
    /// `cartridge_type_enabled` is stubbed to 0 (no IEEE488 cart) — c64iec.ts:483.
    pub fn iec_available_busses(&self) -> u8 {
        // cartridge_type_enabled(IEEE488 | IEEEFLASH64) == 0 ⇒ pure IEC.
        IEC_BUS_IEC
    }

    // =========================================================================
    // ts: c64iec.ts:300-302 (c64iec_enable)  vice: c64iec.c:178-181
    // =========================================================================

    /// ts: c64iec.ts:300 `c64iec_enable` (vice: c64iec.c:178-181).
    pub fn c64iec_enable(&mut self, val: u8) {
        self.c64iec_active = if val != 0 { 1 } else { 0 };
    }

    /// ts: c64iec.ts:309 `c64iec_get_active_state` (vice: c64iec.c:183-186).
    pub fn c64iec_get_active_state(&self) -> u8 {
        self.c64iec_active
    }

    // =========================================================================
    // ts: iecbus.ts:367-378 (conf0 — no drive enabled)  vice: iecbus.c:212-224
    // =========================================================================

    /// ts: iecbus.ts:367 `iecbus_cpu_read_conf0` (vice: iecbus.c:212-217).
    #[inline]
    pub fn iecbus_cpu_read_conf0(&self, _clock: u64) -> u8 {
        ((self.iecbus.iec_fast_1541 as u32 & 0x30) << 2) as u8
    }

    /// ts: iecbus.ts:374 `iecbus_cpu_write_conf0` (vice: iecbus.c:219-224).
    #[inline]
    pub fn iecbus_cpu_write_conf0(&mut self, data: u8, _clock: u64) {
        self.iecbus.iec_fast_1541 = data;
    }

    // =========================================================================
    // ts: iecbus.ts:385-389 (iecbus_cpu_read_conf1)  vice: iecbus.c:227-234
    // =========================================================================
    // NOTE: VICE calls `drive_cpu_execute_all(clock)` here FIRST. In TRX64 the
    // drive borrow is separate, so the caller (full.rs) runs the catch-up before
    // calling this read; the body then returns the freshly-folded cpu_port.

    /// ts: iecbus.ts:385 `iecbus_cpu_read_conf1` (vice: iecbus.c:227-234).
    /// The `drive_cpu_execute_all(clock)` is performed by the caller (full.rs).
    #[inline]
    pub fn iecbus_cpu_read_conf1(&self, _clock: u64) -> u8 {
        self.iecbus.cpu_port
    }

    // =========================================================================
    // ts: iecbus.ts:392-478 (iecbus_cpu_write_conf1)  vice: iecbus.c:237-287
    // =========================================================================

    /// ts: iecbus.ts:392 `iecbus_cpu_write_conf1` (vice: iecbus.c:237-287).
    /// Drive 8 only. `data` = inverted CIA2 PA. Returns the ATN-edge action the
    /// caller delivers to the drive's VIA1 CA1 (= VICE's inline `viacore_signal`).
    ///
    /// The VICE-order steps THIS function owns:
    ///   iec_update_cpu_bus(data)
    ///   → ATN-edge detect + per-type signal decision
    ///   → per-unit-type drv_bus[8] recompute
    ///   → iec_update_ports()
    /// The two steps it does NOT own (the drive borrow): `drive_cpu_execute_one`
    /// (BEFORE this call) and the actual `viacore_signal` (AFTER, via the return).
    #[inline]
    pub fn iecbus_cpu_write_conf1(&mut self, data: u8, _clock: u64) -> Option<AtnEdge> {
        // vice: `diskunit_context_t *unit = diskunit_context[0];` — type lookup.
        let unit_type = self.unit_type[8];

        // (drive_cpu_execute_one(unit, clock) — done by caller.)

        self.iec_update_cpu_bus(data);

        let mut edge: Option<AtnEdge> = None;
        if self.iec_old_atn != (self.iecbus.cpu_bus & 0x10) {
            self.iec_old_atn = self.iecbus.cpu_bus & 0x10;
            edge = Some(Self::atn_signal_for(unit_type, self.iec_old_atn));
        }

        // Per-unit-type drv_bus formula (vice iecbus.c:270-285), unit slot 8.
        self.recompute_drv_bus_unit(8, unit_type);

        self.iec_update_ports();
        edge
    }

    // =========================================================================
    // ts: iecbus.ts:485-489 (iecbus_cpu_read_conf2)  vice: iecbus.c:290-297
    // =========================================================================

    /// ts: iecbus.ts:485 `iecbus_cpu_read_conf2` (vice: iecbus.c:290-297).
    #[inline]
    pub fn iecbus_cpu_read_conf2(&self, _clock: u64) -> u8 {
        self.iecbus.cpu_port
    }

    // =========================================================================
    // ts: iecbus.ts:494-576 (iecbus_cpu_write_conf2)  vice: iecbus.c:300-351
    // =========================================================================

    /// ts: iecbus.ts:494 `iecbus_cpu_write_conf2` (vice: iecbus.c:300-351).
    /// Drive 9 only — targets `diskunit_context[1]` and `drv_bus[9]`.
    #[inline]
    pub fn iecbus_cpu_write_conf2(&mut self, data: u8, _clock: u64) -> Option<AtnEdge> {
        let unit_type = self.unit_type[9];

        self.iec_update_cpu_bus(data);

        let mut edge: Option<AtnEdge> = None;
        if self.iec_old_atn != (self.iecbus.cpu_bus & 0x10) {
            self.iec_old_atn = self.iecbus.cpu_bus & 0x10;
            edge = Some(Self::atn_signal_for(unit_type, self.iec_old_atn));
        }

        // Per-unit-type drv_bus formula (vice iecbus.c:333-348), unit slot 9.
        self.recompute_drv_bus_unit(9, unit_type);

        self.iec_update_ports();
        edge
    }

    // =========================================================================
    // ts: iecbus.ts:583-588 (iecbus_cpu_read_conf3)  vice: iecbus.c:353-361
    // =========================================================================

    /// ts: iecbus.ts:583 `iecbus_cpu_read_conf3` (vice: iecbus.c:353-361).
    /// `serial_iec_device_exec(clock)` is a no-op in the 1541 shape (ts:266).
    #[inline]
    pub fn iecbus_cpu_read_conf3(&self, _clock: u64) -> u8 {
        // (drive_cpu_execute_all + serial_iec_device_exec done by caller / no-op.)
        self.iecbus.cpu_port
    }

    // =========================================================================
    // ts: iecbus.ts:592-686 (iecbus_cpu_write_conf3)  vice: iecbus.c:364-430
    // =========================================================================

    /// ts: iecbus.ts:592 `iecbus_cpu_write_conf3` (vice: iecbus.c:364-430).
    /// Multi-drive loop over NUM_DISK_UNITS. Returns the ATN edge for each
    /// TRUEDRIVE unit (in unit order) so the caller can deliver every signal.
    #[inline]
    pub fn iecbus_cpu_write_conf3(&mut self, data: u8, _clock: u64) -> Vec<(usize, AtnEdge)> {
        // (drive_cpu_execute_all + serial_iec_device_exec done by caller / no-op.)
        self.iec_update_cpu_bus(data);

        let mut edges: Vec<(usize, AtnEdge)> = Vec::new();
        if self.iec_old_atn != (self.iecbus.cpu_bus & 0x10) {
            self.iec_old_atn = self.iecbus.cpu_bus & 0x10;

            for dnr in 0..NUM_DISK_UNITS {
                if self.iecbus_device[8 + dnr] == IECBUS_DEVICE_TRUEDRIVE {
                    let unit_type = self.unit_type[dnr];
                    edges.push((dnr, Self::atn_signal_for(unit_type, self.iec_old_atn)));
                }
            }
        }

        for dnr in 0..NUM_DISK_UNITS {
            if self.iecbus_device[8 + dnr] == IECBUS_DEVICE_TRUEDRIVE {
                let unit_slot = dnr + 8;
                let unit_type = self.unit_type[dnr];
                self.recompute_drv_bus_unit(unit_slot, unit_type);
            }
        }

        self.iec_update_ports();
        edges
    }

    /// Shared per-unit-type drv_bus recompute (vice iecbus.c:270-285 /
    /// 333-348 / 410-425 — identical formula per slot). `slot` = unit + 8.
    #[inline]
    fn recompute_drv_bus_unit(&mut self, slot: usize, unit_type: DriveType) {
        let dd = self.iecbus.drv_data[slot] as u32;
        let cb = self.iecbus.cpu_bus as u32;
        match unit_type {
            DriveType::Drive1581
            | DriveType::Drive2000
            | DriveType::Drive4000
            | DriveType::DriveCmdHd => {
                // vice: `(((dd << 3) & 0x40) | ((dd << 6) & ((dd | cpu_bus) << 3) & 0x80))`
                self.iecbus.drv_bus[slot] =
                    (((dd << 3) & 0x40) | ((dd << 6) & ((dd | cb) << 3) & 0x80)) as u8;
            }
            DriveType::Drive1541 => {
                // vice: `(((dd << 3) & 0x40) | ((dd << 6) & ((~dd ^ cpu_bus) << 3) & 0x80))`
                let inv = (!dd) ^ cb;
                self.iecbus.drv_bus[slot] =
                    (((dd << 3) & 0x40) | ((dd << 6) & (inv << 3) & 0x80)) as u8;
            }
        }
    }

    /// The per-drive-type ATN-edge signal decision shared by conf1/conf2/conf3.
    /// `new_atn` = `iec_old_atn` (the freshly latched `cpu_bus & 0x10`). Mirrors
    /// the VICE `switch (unit->type)` exactly (iecbus.c:249-267 / 312-330 /
    /// 382-400). Returns the `viacore_signal` / `ciacore_set_flag` action.
    #[inline]
    fn atn_signal_for(unit_type: DriveType, new_atn: u8) -> AtnEdge {
        match unit_type {
            DriveType::Drive1581 => {
                // vice: `if (!iec_old_atn) { ciacore_set_flag(unit->cia1581); }`
                AtnEdge::Cia1581Flag { fire: new_atn == 0 }
            }
            DriveType::Drive2000 | DriveType::Drive4000 => {
                // vice: `viacore_signal(via4000, VIA_SIG_CA2, iec_old_atn ? 0 : VIA_SIG_RISE)`
                let sig = if new_atn != 0 { 0 } else { VIA_SIG_RISE };
                AtnEdge::Via4000Ca2 { sig }
            }
            DriveType::DriveCmdHd => {
                // vice: `viacore_signal(via10, VIA_SIG_CA1, iec_old_atn ? RISE : FALL)`
                let sig = if new_atn != 0 { VIA_SIG_RISE } else { VIA_SIG_FALL };
                AtnEdge::CmdHdVia10Ca1 { sig }
            }
            DriveType::Drive1541 => {
                // vice: `viacore_signal(via1d1541, VIA_SIG_CA1, iec_old_atn ? 0 : VIA_SIG_RISE)`
                let sig = if new_atn != 0 { 0 } else { VIA_SIG_RISE };
                AtnEdge::Via1Ca1 { sig }
            }
        }
    }

    // =========================================================================
    // ts: iecbus.ts:695-724 (calculate_callback_index)  vice: iecbus.c:432-463
    // =========================================================================

    /// ts: iecbus.ts:695 `calculate_callback_index` (vice: iecbus.c:432-463).
    pub fn calculate_callback_index(&mut self) {
        let callback_index = ((self.iecbus_device[8] as u32) << 0)
            | ((self.iecbus_device[9] as u32) << 2)
            | ((self.iecbus_device[10] as u32) << 6)
            | ((self.iecbus_device[11] as u32) << 8)
            | ((self.iecbus_device[4] as u32) << 10)
            | ((self.iecbus_device[5] as u32) << 12)
            | ((self.iecbus_device[6] as u32) << 14)
            | ((self.iecbus_device[7] as u32) << 16);

        self.iecbus_callback = if callback_index == 0 {
            IecbusCallback::Conf0
        } else if callback_index == (IECBUS_DEVICE_TRUEDRIVE as u32) << 0 {
            IecbusCallback::Conf1
        } else if callback_index == (IECBUS_DEVICE_TRUEDRIVE as u32) << 2 {
            IecbusCallback::Conf2
        } else {
            IecbusCallback::Conf3
        };
    }

    // =========================================================================
    // ts: iecbus.ts:764-796 (iecbus_status_set)  vice: iecbus.c:512-548
    // =========================================================================
    // The four `static` per-device arrays VICE keeps inside the function body
    // (truedrive/drivetype/iecdevice/virtualdevices) live as IecCore fields below
    // so their persistence-across-calls semantics match.

    /// ts: iecbus.ts:764 `iecbus_status_set` (vice: iecbus.c:512-548).
    pub fn iecbus_status_set(&mut self, type_: u8, unit: usize, enable: u8) {
        // The four `static` arrays from the C function body — module-persistent.
        // We thread them via the dedicated IecCore-adjacent storage below.
        match type_ {
            IECBUS_STATUS_TRUEDRIVE => {
                IECBUS_STATUS_ARRAYS.with_truedrive(unit, if enable != 0 { 1 << 3 } else { 0 });
            }
            IECBUS_STATUS_DRIVETYPE => {
                IECBUS_STATUS_ARRAYS.with_drivetype(unit, if enable != 0 { 1 << 2 } else { 0 });
            }
            IECBUS_STATUS_IECDEVICE => {
                IECBUS_STATUS_ARRAYS.with_iecdevice(unit, if enable != 0 { 1 << 1 } else { 0 });
            }
            IECBUS_STATUS_TRAPDEVICE => {
                IECBUS_STATUS_ARRAYS.with_virtualdevices(unit, if enable != 0 { 1 << 0 } else { 0 });
            }
            _ => {}
        }

        for dev in 0..IECBUS_NUM {
            let index = IECBUS_STATUS_ARRAYS.index_for(dev);
            self.iecbus_device[dev] = IECBUS_DEVICE_INDEX[index as usize];
        }

        self.calculate_callback_index();
    }

    // =========================================================================
    // ts: iecbus.ts:803-805 (iecbus_device_read)  vice: iecbus.c:551-554
    // =========================================================================

    /// ts: iecbus.ts:803 `iecbus_device_read` (vice: iecbus.c:551-554).
    #[inline]
    pub fn iecbus_device_read(&self) -> u8 {
        self.iecbus.drv_port
    }

    // =========================================================================
    // ts: iecbus.ts:812-824 (iecbus_device_write)  vice: iecbus.c:557-570
    // =========================================================================

    /// ts: iecbus.ts:812 `iecbus_device_write` (vice: iecbus.c:557-570).
    /// `iecbus_update_ports` is always installed (c64iec_init) ⇒ returns 1.
    #[inline]
    pub fn iecbus_device_write(&mut self, unit: usize, data: u8) -> i32 {
        if unit < IECBUS_NUM {
            self.iecbus.drv_bus[unit] = data;
            // iecbus_update_ports != NULL (c64iec_init installed iec_update_ports).
            self.iec_update_ports();
            1
        } else {
            0
        }
    }

    // =========================================================================
    // CALLER-FACING DISPATCH — the VICE function-pointer call, as the C64-side
    // store/read indirection. `full.rs` invokes these instead of reaching for an
    // extern fn pointer (the VICE link-time `iecbus_callback_write/_read`).
    // =========================================================================

    /// Dispatch the active write callback (= `(*iecbus_callback_write)(data, clk)`).
    /// Returns the ATN edges to deliver to the drive(s). Conf1/Conf2 yield at most
    /// one; Conf3 yields one per truedrive; Conf0 yields none.
    #[inline]
    pub fn iecbus_callback_write(&mut self, data: u8, clock: u64) -> Vec<(usize, AtnEdge)> {
        match self.iecbus_callback {
            IecbusCallback::Conf0 => {
                self.iecbus_cpu_write_conf0(data, clock);
                Vec::new()
            }
            IecbusCallback::Conf1 => match self.iecbus_cpu_write_conf1(data, clock) {
                Some(e) => vec![(0, e)],
                None => Vec::new(),
            },
            IecbusCallback::Conf2 => match self.iecbus_cpu_write_conf2(data, clock) {
                Some(e) => vec![(1, e)],
                None => Vec::new(),
            },
            IecbusCallback::Conf3 => self.iecbus_cpu_write_conf3(data, clock),
        }
    }

    /// Dispatch the active read callback (= `(*iecbus_callback_read)(clk)`).
    #[inline]
    pub fn iecbus_callback_read(&self, clock: u64) -> u8 {
        match self.iecbus_callback {
            IecbusCallback::Conf0 => self.iecbus_cpu_read_conf0(clock),
            IecbusCallback::Conf1 => self.iecbus_cpu_read_conf1(clock),
            IecbusCallback::Conf2 => self.iecbus_cpu_read_conf2(clock),
            IecbusCallback::Conf3 => self.iecbus_cpu_read_conf3(clock),
        }
    }

    // =========================================================================
    // Convenience accessors used by full.rs / drive.rs / tests. These mirror the
    // VICE `iecbus.<field>` direct reads, just spelled as inherent fields/methods
    // so the existing call sites (which read cpu_port / drv_port / cpu_bus, and
    // the diagnostic probes that read drv_data[8] / drv_bus[8]) keep working.
    // =========================================================================

    /// ts: `iecbus.cpu_bus` — C64-side intent (vice: iecbus.cpu_bus).
    #[inline]
    pub fn cpu_bus(&self) -> u8 {
        self.iecbus.cpu_bus
    }
    /// ts: `iecbus.cpu_port` — effective bus the C64 reads (vice: iecbus.cpu_port).
    #[inline]
    pub fn cpu_port(&self) -> u8 {
        self.iecbus.cpu_port
    }
    /// ts: `iecbus.drv_port` — effective bus the drive reads (vice: iecbus.drv_port).
    #[inline]
    pub fn drv_port(&self) -> u8 {
        self.iecbus.drv_port
    }

    /// Refresh `drv_data[8]` from the drive's CURRENT VIA1 PB output WITHOUT folding
    /// the wired-AND bus or updating ports. CROSS-DOMAIN accommodation (NOT a VICE
    /// function): in TRX64 the drive's VIA1 is a separate borrow, so the catch-up
    /// run's `$1800` `store_prb` stores never propagate `drv_data[8]` into this IEC
    /// core. On the $DD00 WRITE path we re-read the live drive pull here; the SINGLE
    /// authoritative wired-AND fold is then done by `iecbus_cpu_write_conf1` (via
    /// `iecbus_callback_write`) against the NEW cpu_bus — matching VICE order. We
    /// store `~pb_out` exactly as VICE `store_prb` does (`*drive_data = ~byte`).
    #[inline]
    pub fn drive_set_data_no_fold(&mut self, pb_out: u8) {
        self.iecbus.drv_data[8] = (!pb_out) & 0xff;
    }
}

// =============================================================================
// ts: iecbus.ts:758-761 (the four `static` arrays inside iecbus_status_set)
// vice: iecbus.c:514-515 — `static unsigned int truedrive[IECBUS_NUM], ...`
// =============================================================================
// VICE keeps `truedrive/drivetype/iecdevice/virtualdevices` as function-static
// arrays — one global instance, persistent across calls. The TS port hoists them
// to module scope (iecbus.ts:758). Here they live behind a tiny module-static
// holder so the persistence is identical. `iecbus_status_set` is only ever called
// at session setup (single-threaded boot), so a `Cell`-backed static is exact.

use std::cell::Cell;

struct IecbusStatusArrays {
    truedrive: [Cell<u8>; IECBUS_NUM],
    drivetype: [Cell<u8>; IECBUS_NUM],
    iecdevice: [Cell<u8>; IECBUS_NUM],
    virtualdevices: [Cell<u8>; IECBUS_NUM],
}

impl IecbusStatusArrays {
    #[inline]
    fn with_truedrive(&self, unit: usize, v: u8) {
        self.truedrive[unit].set(v);
    }
    #[inline]
    fn with_drivetype(&self, unit: usize, v: u8) {
        self.drivetype[unit].set(v);
    }
    #[inline]
    fn with_iecdevice(&self, unit: usize, v: u8) {
        self.iecdevice[unit].set(v);
    }
    #[inline]
    fn with_virtualdevices(&self, unit: usize, v: u8) {
        self.virtualdevices[unit].set(v);
    }
    /// vice: `index = truedrive[dev] | drivetype[dev] | iecdevice[dev] | virtualdevices[dev];`
    #[inline]
    fn index_for(&self, dev: usize) -> u8 {
        self.truedrive[dev].get()
            | self.drivetype[dev].get()
            | self.iecdevice[dev].get()
            | self.virtualdevices[dev].get()
    }
}

thread_local! {
    static IECBUS_STATUS_ARRAYS_TLS: IecbusStatusArrays = IecbusStatusArrays {
        truedrive: Default::default(),
        drivetype: Default::default(),
        iecdevice: Default::default(),
        virtualdevices: Default::default(),
    };
}

// A zero-sized facade so call sites read `IECBUS_STATUS_ARRAYS.with_truedrive(..)`,
// matching the `static`-array spelling of the C body while routing through the
// thread-local storage (the headless runtime is single-threaded per session).
struct IecbusStatusArraysFacade;
impl IecbusStatusArraysFacade {
    #[inline]
    fn with_truedrive(&self, unit: usize, v: u8) {
        IECBUS_STATUS_ARRAYS_TLS.with(|a| a.with_truedrive(unit, v));
    }
    #[inline]
    fn with_drivetype(&self, unit: usize, v: u8) {
        IECBUS_STATUS_ARRAYS_TLS.with(|a| a.with_drivetype(unit, v));
    }
    #[inline]
    fn with_iecdevice(&self, unit: usize, v: u8) {
        IECBUS_STATUS_ARRAYS_TLS.with(|a| a.with_iecdevice(unit, v));
    }
    #[inline]
    fn with_virtualdevices(&self, unit: usize, v: u8) {
        IECBUS_STATUS_ARRAYS_TLS.with(|a| a.with_virtualdevices(unit, v));
    }
    #[inline]
    fn index_for(&self, dev: usize) -> u8 {
        IECBUS_STATUS_ARRAYS_TLS.with(|a| a.index_for(dev))
    }
}
#[allow(non_upper_case_globals)]
const IECBUS_STATUS_ARRAYS: IecbusStatusArraysFacade = IecbusStatusArraysFacade;

// =============================================================================
// fold_drv_port — drive-side mid-run wired-AND re-fold (= the via1d1541.c
// store_prb cross-domain sync). Kept as a free fn because the drive borrow
// (drive.rs) calls it without access to the IEC core; it reproduces exactly the
// `iec_drive_write` → drv_bus[8] formula + the drv_port derive (iec_update_ports)
// for the single-drive slot, against a FIXED cpu_bus.
// =============================================================================

/// Re-fold the wired-AND bus from the drive's CURRENT VIA1 PB output against a
/// FIXED C64-side `cpu_bus`, returning the `drv_port` byte the drive reads at its
/// VIA1 PB inputs. This is the pure-function shape of VICE `via1d1541.c store_prb`
/// (lines 222-241, the `iecbus != NULL` branch): `*drive_data = ~byte`, then
/// `*drive_bus = (((drive_data<<3)&0x40) | ((drive_data<<6) & ((~drive_data ^
/// cpu_bus)<<3) & 0x80))`, then the `iec_update_ports` cpu_port/drv_port derive for
/// the single-drive slot. VICE recomputes drv_bus[8]/cpu_port/drv_port immediately
/// when the drive changes its own IEC output mid-run. `pb_out` = `(ORB | ~DDRB)`,
/// the RAW composed PB output (`byte` in store_prb) — this fn inverts it to
/// `drive_data = ~pb_out` exactly like store_prb (NOT pre-inverted by the caller).
#[inline]
pub fn fold_drv_port(cpu_bus: u8, pb_out: u8) -> u8 {
    // store_prb: `*drive_data = ~byte` — invert the raw PB output.
    let drive_data = (!pb_out) as u32;
    let cb = cpu_bus as u32;
    // `*drive_bus = (((drive_data<<3)&0x40) | ((drive_data<<6) & ((~drive_data ^ cpu_bus)<<3) & 0x80))`
    let inv = (!drive_data) ^ cb;
    let drv_bus = (((drive_data << 3) & 0x40) | ((drive_data << 6) & (inv << 3) & 0x80)) as u8;
    // iec_update_ports single-slot fold: cpu_port = cpu_bus & drv_bus[8].
    let cpu_port = cpu_bus & drv_bus;
    // drv_port = ((cpu_port>>4)&0x4) | (cpu_port>>7) | ((cpu_bus<<3)&0x80).
    (((cpu_port as u32 >> 4) & 0x4) | (cpu_port as u32 >> 7) | ((cb << 3) & 0x80)) as u8
}
