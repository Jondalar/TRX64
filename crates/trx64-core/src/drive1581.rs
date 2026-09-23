//! drive1581.rs — the 1581 board (Spec 872 D1): its own NMOS 6502 at 2 MHz, 8 KiB RAM,
//! 32 KiB ROM, an 8520 CIA (`ciacore.rs`) and a WD1772 with its mechanism
//! (`wd177x.rs`, `fdd.rs`). The drive CPU stays its own 6502 (`DriveCore6510`, the
//! same verbatim core the 1541 runs) and is never merged into the C64's.
//!
//! The map, from `memiec.c:249-255` and schematic #252380 sheet 1 (U6 74LS139 decodes
//! A13/A14 with A15 as enable):
//!
//! | range         | device                                    |
//! |---------------|-------------------------------------------|
//! | `$0000-$1FFF` | 8 KiB RAM (U6 Y0)                         |
//! | `$2000-$3FFF` | nothing (Y1 unconnected): the open bus    |
//! | `$4000-$5FFF` | 8520 CIA, 16-byte mirror (Y2 `/6526SEL`)  |
//! | `$6000-$7FFF` | WD1772, 4-byte mirror (Y3 `/WDSEL`)       |
//! | `$8000-$FFFF` | 32 KiB ROM; a write selects nothing       |
//!
//! The CIA's ports are the glue (cia1581d.c): PA0 side select (low = side 1), PA1 /RDY
//! in (VICE reads 0: always ready), PA2 /MOTOR, PA3-4 the device jumpers, PA5 power
//! LED, PA6 activity LED, PA7 /DISK CHANGE in; PB0 DATA in, PB1 DATA out, PB2 CLK in,
//! PB3 CLK out, PB4 ATN acknowledge, PB5 fast-serial direction, PB6 /WPS in, PB7 ATN
//! in. ATN also reaches FLAG (a falling edge, `iecbus.c:250-252`). IRQ is the CIA
//! alone; NMI is not connected; neither INTRQ nor DRQ of the WD reaches the CPU.
//!
//! The IEC wiring follows the 1541 port: the board owns its `IecbusT` (VICE's `iecbus`
//! pointer), a port-B store folds the drive's lines into it with the 1581 formula
//! (the OR form of the ATN acknowledge), and the machine reads `drv_data[unit]` back
//! after the catch-up and folds it into the shared IEC core.

use crate::ciacore::{CiaBackend, CiaCore, CIA_DDRA, CIA_PRA, CIA_PRB};
use crate::drive_6510core::{drive_6510core_execute, DriveCore6510, DriveCore6510Bus, IntStatus, IK_IRQ, IK_RESET};
use crate::iec::IecbusT;
use crate::wd177x::Wd1770;

/// The 6502 hardware-reset sequence (drivecpu.c:165 `cpu_reset` → clk = 6).
const DRIVE_RESET_CYCLES: u64 = 6;
/// The C64-side reset origin the drive's catch-up clock observes (drive.rs
/// `C64_RESET_DRIVE_OFFSET`).
const C64_RESET_DRIVE_OFFSET: u64 = 1;
/// drivesync.c:98-103 — a 1581's `clock_frequency`.
pub const CLOCK_FREQUENCY_1581: u32 = 2;

/// The PB5 (fast-serial direction) history a gate reads: `(drive clock, level)` at
/// every change, and how many bytes the serial port shifted out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FastSerialLog {
    pub pb5_changes: Vec<(u64, bool)>,
    pub bytes_out: u64,
}

/// The CIA's port hooks, over the board's parts (cia1581d.c).
struct Ports<'a> {
    wd: &'a mut Wd1770,
    iecbus: &'a mut IecbusT,
    /// `cia1581p->number` — unit − 8.
    number: usize,
    /// `drive->read_only` — the medium's write-protect, for /WPS.
    read_only: bool,
    fast_dir: &'a mut bool,
    fast_log: &'a mut FastSerialLog,
    led: &'a mut bool,
    clk: u64,
}

impl CiaBackend for Ports<'_> {
    /// cia1581d.c:117-137 store_ciapa.
    fn store_pa(&mut self, _clk: u64, byte: u8) {
        self.wd.fdd.select_head(if byte & 0x01 != 0 { 0 } else { 1 });
        self.wd.fdd.set_motor(byte & 0x04 == 0);
        *self.led = byte & 0x40 != 0;
    }

    /// cia1581d.c:139-183 store_ciapb — fold into the drive's `iecbus` with the 1581
    /// formula, and hand PB5 to the fast-serial direction.
    fn store_pb(&mut self, _clk: u64, byte: u8, old_pb: u8) {
        if byte != old_pb {
            let slot = self.number + 8;
            let drive_data = !byte;
            self.iecbus.drv_data[slot] = drive_data;
            let dd = drive_data as u32;
            let cb = self.iecbus.cpu_bus as u32;
            self.iecbus.drv_bus[slot] = (((dd << 3) & 0x40) | ((dd << 6) & ((dd | cb) << 3) & 0x80)) as u8;
            let mut cpu_port = self.iecbus.cpu_bus;
            for unit in 4..(8 + crate::iec::NUM_DISK_UNITS) {
                cpu_port &= self.iecbus.drv_bus[unit];
            }
            self.iecbus.cpu_port = cpu_port;
            let cp = cpu_port as u32;
            self.iecbus.drv_port = (((cp >> 4) & 0x4) | (cp >> 7) | ((cb << 3) & 0x80)) as u8;
            // iec_fast_drive_direction(byte & 0x20, number).
            let dir = byte & 0x20 != 0;
            if dir != *self.fast_dir {
                *self.fast_dir = dir;
                self.fast_log.pb5_changes.push((self.clk, dir));
            }
        }
    }

    /// cia1581d.c:185-201 read_ciapa — jumpers on PA3-4, /DISK CHANGE on PA7 (1 = not
    /// changed), /RDY on PA1 reading 0 (always ready, the VICE default, §10.3).
    fn read_pa(&mut self, c: &[u8; 16]) -> u8 {
        let mut tmp = (8 * self.number) as u8;
        if !self.wd.fdd.disk_change() {
            tmp |= 0x80;
        }
        (tmp & !c[CIA_DDRA]) | (c[CIA_PRA] & c[CIA_DDRA])
    }

    /// cia1581d.c:203-223 read_ciapb.
    fn read_pb(&mut self, c: &[u8; 16]) -> u8 {
        (((c[CIA_PRB] & 0x1a) | self.iecbus.drv_port) ^ 0x85) | if self.read_only { 0 } else { 0x40 }
    }

    /// cia1581d.c:233-240 store_sdr → iec_fast_drive_write: a stock C64 has no burst
    /// path (`BURST_MOD_NONE`, §D1a), so the byte goes nowhere; it is counted.
    fn store_sdr(&mut self, _byte: u8) {
        self.fast_log.bytes_out += 1;
    }

    /// cia1581d.c:108-115 undump_ciapa.
    fn undump_pa(&mut self, _rclk: u64, byte: u8) {
        *self.led = byte & 0x40 != 0;
    }

    /// cia1581d.c:84-90 do_reset_cia.
    fn do_reset(&mut self) {
        *self.led = true;
    }
}

/// The port hooks over a `Drive1581`'s own fields (disjoint from its CIA).
macro_rules! ports_of {
    ($s:ident, $number:expr, $clk:expr) => {
        Ports {
            wd: &mut $s.wd,
            iecbus: &mut $s.iecbus,
            number: $number,
            read_only: $s.read_only,
            fast_dir: &mut $s.fast_dir,
            fast_log: &mut $s.fast_log,
            led: &mut $s.led,
            clk: $clk,
        }
    };
}

/// The drive CPU's bus (memiec.c 1581 map).
struct Bus1581<'a> {
    ram: &'a mut [u8; 0x2000],
    rom: &'a [u8; 0x8000],
    cia: &'a mut CiaCore,
    ports: Ports<'a>,
    clk_ptr: *mut u64,
    cpu_last_data: &'a mut u8,
}

impl Bus1581<'_> {
    #[inline]
    fn clk(&self) -> u64 {
        // SAFETY: `clk_ptr` points at `Drive1581.core.clk`, disjoint from every field
        // this bus borrows; read synchronously inside a bus call the core made (the
        // same reasoning as the 1541's `DriveBus::clk`).
        unsafe { *self.clk_ptr }
    }
}

impl DriveCore6510Bus for Bus1581<'_> {
    #[inline]
    fn read(&mut self, addr: u16) -> u8 {
        let v = match addr >> 13 {
            0 => self.ram[(addr & 0x1fff) as usize],
            1 => return *self.cpu_last_data, // drive_read_free
            2 => {
                let clk = self.clk();
                self.cia.clk = clk;
                self.ports.clk = clk;
                self.cia.read(&mut self.ports, addr)
            }
            3 => {
                let clk = self.clk();
                self.ports.wd.read(clk, addr & 3)
            }
            _ => self.rom[(addr & 0x7fff) as usize],
        };
        *self.cpu_last_data = v;
        v
    }

    #[inline]
    fn write(&mut self, addr: u16, val: u8) {
        *self.cpu_last_data = val;
        match addr >> 13 {
            0 => self.ram[(addr & 0x1fff) as usize] = val,
            2 => {
                let clk = self.clk();
                self.cia.clk = clk;
                self.ports.clk = clk;
                self.cia.store(&mut self.ports, addr, val);
            }
            3 => {
                let clk = self.clk();
                self.ports.wd.store(clk, addr & 3, val);
            }
            _ => {} // $2000-$3FFF selects nothing; the ROM ignores a write.
        }
    }

    // No rotating GCR surface on a 1581: VICE's `rotation_rotate_disk` returns at once
    // (no VIA2 has switched the motor), and no byte-ready reaches the V flag.
    #[inline]
    fn rotate(&mut self) {}
    #[inline]
    fn byte_ready(&mut self) -> bool {
        false
    }
    #[inline]
    fn byte_ready_edge_clear(&mut self) {}

    /// PROCESS_ALARMS (6510core.c:139-143): every CIA alarm `<= clk`.
    #[inline]
    fn process_alarms(&mut self, clk: u64) {
        if clk >= self.cia.next_alarm_clk() {
            self.cia.clk = clk;
            self.ports.clk = clk;
            self.cia.process_alarms(&mut self.ports, clk);
        }
    }

    #[inline]
    fn cpu_reset(&mut self) {
        // SAFETY: see `clk`; drivecpu.c:165 `drv->clk_ptr->value = 6`.
        unsafe { *self.clk_ptr = DRIVE_RESET_CYCLES };
    }
}

/// The CIA ports as the pins see them (Spec 872 §7). Side-effect free.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ports1581 {
    /// Port A / B composed outputs, `PRx | ~DDRx`.
    pub pa_out: u8,
    pub pb_out: u8,
    /// Port A / B as a CPU read would return them now.
    pub pa: u8,
    pub pb: u8,
    /// PA0 — the side the head reads: 0 or 1 (`side = PA0 ? 0 : 1`).
    pub side: u8,
    /// PA1 /RDY as the CIA reads it (false = ready).
    pub not_ready: bool,
    /// PA2 low — the spindle motor runs.
    pub motor_on: bool,
    /// PA3-4 — the device jumpers, as a unit number.
    pub jumpers: u8,
    /// PA5 / PA6 — power and activity LED.
    pub power_led: bool,
    pub activity_led: bool,
    /// PA7 low — the disk-change latch is set.
    pub disk_changed: bool,
    /// PB1 / PB3 / PB4 — DATA out, CLK out, ATN acknowledge (1 = asserted by the drive).
    pub data_out: bool,
    pub clk_out: bool,
    pub atn_ack: bool,
    /// PB5 — the fast-serial direction (1 = SP drives DATA).
    pub fast_serial_out: bool,
    /// PB6 /WPS — 1 = writable.
    pub writable: bool,
    /// PB0 / PB2 / PB7 — DATA, CLK, ATN as the drive reads them (1 = asserted).
    pub data_in: bool,
    pub clk_in: bool,
    pub atn_in: bool,
}

/// The WD's state as the drive CPU would read it, without running the microcode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WdState {
    pub track: u8,
    pub sector: u8,
    pub data: u8,
    pub status: u8,
    pub command: u8,
    pub busy: bool,
    /// The microcode's position: the command's type (-1/0 idle) and step.
    pub type_: i32,
    pub step: i32,
}

/// The 1581 board.
#[derive(Clone)]
pub struct Drive1581 {
    pub core: DriveCore6510,
    pub int: IntStatus,
    ram: Box<[u8; 0x2000]>,
    rom: Box<[u8; 0x8000]>,
    /// A ROM given since the last power-on: in force at the next one (870 D3).
    rom_next: Option<Box<[u8; 0x8000]>>,
    pub cia: CiaCore,
    pub wd: Wd1770,
    /// The drive's `iecbus` (VICE's `iecbus` pointer), see the module doc.
    pub iecbus: IecbusT,
    pub cpu_last_data: u8,
    stop_clk: u64,
    sync_accum: u32,
    reset_pending: bool,
    pub drive_clk: u64,
    last_sample_pc: Option<u16>,
    /// The medium's write-protect as /WPS reads it (`drive->read_only`).
    pub read_only: bool,
    fast_dir: bool,
    pub fast_log: FastSerialLog,
    led: bool,
    /// `cia1581p->number` — unit − 8, as of the last reset or catch-up.
    number: usize,
}

impl Drive1581 {
    /// A fresh board: RAM zero, a zero ROM, the chips at their power-on context. Its
    /// reset runs at its power-on (`power_on_reset`).
    pub fn new(mynumber: u32) -> Self {
        Self {
            core: DriveCore6510::new(),
            int: IntStatus::new(),
            ram: Box::new([0u8; 0x2000]),
            rom: Box::new([0u8; 0x8000]),
            rom_next: None,
            cia: CiaCore::new(&format!("CIA1581D{mynumber}")),
            wd: Wd1770::new(mynumber),
            iecbus: IecbusT::new_power_on(),
            cpu_last_data: 0,
            stop_clk: 0,
            sync_accum: 0,
            reset_pending: true,
            drive_clk: 0,
            last_sample_pc: None,
            read_only: false,
            fast_dir: false,
            fast_log: FastSerialLog::default(),
            led: false,
            number: mynumber as usize,
        }
    }

    /// Spec 872 §5 — a 1581 takes exactly 32 KiB at `$8000-$FFFF` (VICE loads with
    /// min = max = 0x8000). Anything else is refused, naming the size and the type. In
    /// force from the board's next power-on.
    pub fn set_rom(&mut self, bytes: &[u8]) -> Result<(), String> {
        if bytes.len() != 0x8000 {
            return Err(format!(
                "a 1581 DOS ROM is 32768 bytes; {} bytes refused (a 1581 takes no 16 KiB ROM)",
                bytes.len()
            ));
        }
        let mut rom = Box::new([0u8; 0x8000]);
        rom.copy_from_slice(bytes);
        self.rom_next = Some(rom);
        Ok(())
    }

    /// The ROM in force.
    pub fn rom(&self) -> &[u8] {
        &self.rom[..]
    }

    /// The ROM given but not yet in force, if any.
    pub fn rom_pending(&self) -> bool {
        self.rom_next.is_some()
    }

    pub(crate) fn latch_rom(&mut self) {
        if let Some(rom) = self.rom_next.take() {
            self.rom = rom;
        }
    }


    /// The electronics' reset (iec.c:108-111 for the 1581: `ciacore_reset` +
    /// `wd1770_reset`, and the drive CPU's reset). The mechanism and the medium are
    /// not touched: the head is mechanical and the disk is a medium.
    pub fn reset(&mut self, number: usize) {
        self.number = number;
        self.core = DriveCore6510::new();
        self.core.clk = 0;
        self.int = IntStatus::new();
        self.int.global_pending_int |= IK_RESET;
        self.drive_clk = 0;
        self.stop_clk = 0;
        self.sync_accum = 0;
        self.reset_pending = true;
        self.last_sample_pc = None;
        self.iecbus = IecbusT::new_power_on();
        self.fast_dir = false;
        let name = self.cia.myname.clone();
        self.cia = CiaCore::new(&name);
        self.cia.clk = 0;
        {
            let mut p = ports_of!(self, number, 0);
            self.cia.reset(&mut p);
        }
        self.cia.irq_events.clear();
        self.wd.reset(0);
    }

    /// The board's power-on: RAM zero (VICE `lib_calloc`), the ROM given since the last
    /// power-on into force, then the reset. The medium stays in the mechanism.
    pub fn power_on(&mut self, number: usize) {
        self.ram.fill(0);
        self.cpu_last_data = 0;
        self.latch_rom();
        self.reset(number);
    }

    fn advance_stop_clk(&mut self, c64_cycles: u64, sync_factor: u32) {
        let mut remaining = c64_cycles;
        while remaining != 0 {
            let tcycles = remaining.min(10000) as u32;
            remaining -= tcycles as u64;
            self.sync_accum = self.sync_accum.wrapping_add(sync_factor.wrapping_mul(tcycles));
            self.stop_clk = self.stop_clk.wrapping_add((self.sync_accum >> 16) as u64);
            self.sync_accum &= 0xffff;
        }
    }

    /// The first catch-up target after a reset gets the C64's reset-origin offset, as
    /// the 1541's does (drive.rs `C64_RESET_DRIVE_OFFSET`).
    pub(crate) fn seed_reset_offset(&mut self, sync_factor_1mhz: u32) {
        self.advance_stop_clk(C64_RESET_DRIVE_OFFSET, sync_factor_1mhz * CLOCK_FREQUENCY_1581);
    }

    /// Advance by `n` C64 cycles: `sync_factor_1mhz` is the machine's ratio for a 1 MHz
    /// drive; a 1581 runs `clock_frequency` = 2 of it (drivesync.c:47-60). `drv_port`,
    /// `cpu_bus` and `drv_bus` are the bus as the machine's IEC core holds it.
    pub fn run_cycles(
        &mut self,
        n: u64,
        sync_factor_1mhz: u32,
        number: usize,
        drv_port: u8,
        cpu_bus: u8,
        drv_bus: &[u8; crate::iec::IECBUS_NUM],
    ) {
        self.number = number;
        self.advance_stop_clk(n, sync_factor_1mhz * CLOCK_FREQUENCY_1581);
        self.iecbus.cpu_bus = cpu_bus;
        self.iecbus.drv_port = drv_port;
        let own = number + 8;
        let disk_slots = 8..(8 + crate::iec::NUM_DISK_UNITS);
        for (slot, (mine, theirs)) in self.iecbus.drv_bus.iter_mut().zip(drv_bus).enumerate() {
            if disk_slots.contains(&slot) && slot != own {
                *mine = *theirs;
            }
        }
        let core = &mut self.core;
        let int = &mut self.int;
        let reset_pending = &mut self.reset_pending;
        let clk_ptr: *mut u64 = &mut core.clk;
        let mut bus = Bus1581 {
            ram: &mut self.ram,
            rom: &self.rom,
            cia: &mut self.cia,
            ports: Ports {
                wd: &mut self.wd,
                iecbus: &mut self.iecbus,
                number,
                read_only: self.read_only,
                fast_dir: &mut self.fast_dir,
                fast_log: &mut self.fast_log,
                led: &mut self.led,
                clk: 0,
            },
            clk_ptr,
            cpu_last_data: &mut self.cpu_last_data,
        };
        while *reset_pending || core.clk < self.stop_clk {
            *reset_pending = false;
            // The instruction boundary: alarms due now, then the IRQ line as the CIA
            // drove it since the last boundary, in order (ciacore doc).
            let clk = core.clk;
            bus.process_alarms(clk);
            for (level, rclk) in bus.cia.irq_events.drain(..) {
                int.set_irq(0, level, rclk);
            }
            drive_6510core_execute(core, &mut bus, int);
        }
        for (level, rclk) in bus.cia.irq_events.drain(..) {
            int.set_irq(0, level, rclk);
        }
        self.drive_clk = self.core.clk;
    }

    /// A falling ATN edge on FLAG, at the drive clock it reached (iecbus.c:250-252).
    pub fn atn_flag(&mut self) {
        self.cia.clk = self.core.clk;
        self.cia.set_flag();
    }

    /// The port-B output the machine folds into the shared IEC core (`~drv_data`).
    #[inline]
    pub fn pb_iec_output(&self) -> u8 {
        !self.iecbus.drv_data[self.number + 8]
    }

    // ── the medium ──────────────────────────────────────────────────────────

    /// Mount a D81's bytes (fdd_image_attach): the disk-change latch is set.
    pub fn attach(&mut self, bytes: Vec<u8>, read_only: bool) {
        self.read_only = read_only;
        self.wd.fdd.image_attach(bytes, read_only);
    }

    /// Eject (fdd_image_detach): the resident track is written back first.
    pub fn detach(&mut self) -> Option<Vec<u8>> {
        self.read_only = false;
        self.wd.fdd.image_detach()
    }

    /// Bring a dirty resident track into the image (`fdd_flush_raw`).
    pub fn flush(&mut self) {
        self.wd.fdd.flush_raw();
    }

    /// Bumped whenever a flush wrote into the image.
    pub fn image_gen(&self) -> u64 {
        self.wd.fdd.image_gen
    }

    /// The image as it stands (flushed tracks only).
    pub fn image(&self) -> Option<&Vec<u8>> {
        self.wd.fdd.image.as_ref()
    }

    /// The image with the resident track decoded in, on a copy.
    pub fn image_as_written(&self) -> Option<Vec<u8>> {
        self.wd.fdd.image_as_written()
    }

    // ── accessors (Spec 872 §7) ─────────────────────────────────────────────

    /// The 8 KiB RAM.
    pub fn ram(&self) -> &[u8] {
        &self.ram[..]
    }
    pub fn ram_mut(&mut self) -> &mut [u8] {
        &mut self.ram[..]
    }

    /// The head: physical track (0-83) and the side it reads (`fdd.head`).
    pub fn head(&self) -> (u8, u8) {
        (self.wd.fdd.track as u8, self.wd.fdd.head as u8)
    }

    /// The activity LED (PA6).
    pub fn led_on(&self) -> bool {
        self.led
    }

    /// The WD's registers and microcode position.
    pub fn wd(&self) -> WdState {
        WdState {
            track: self.wd.track,
            sector: self.wd.sector,
            data: self.wd.data,
            status: self.wd.status,
            command: self.wd.cmd,
            busy: self.wd.status & crate::wd177x::WD_BSY != 0,
            type_: self.wd.type_,
            step: self.wd.step,
        }
    }

    /// The CIA ports as the pins see them.
    pub fn ports(&self) -> Ports1581 {
        let number = self.number;
        let c = &self.cia.c_cia;
        let pa_out = self.cia.pa_out();
        let pb_out = c[CIA_PRB] | !c[crate::ciacore::CIA_DDRB];
        let mut tmp = (8 * number) as u8;
        if !self.wd.fdd.disk_change() {
            tmp |= 0x80;
        }
        let pa = (tmp & !c[CIA_DDRA]) | (c[CIA_PRA] & c[CIA_DDRA]);
        let pb = (((c[CIA_PRB] & 0x1a) | self.iecbus.drv_port) ^ 0x85) | if self.read_only { 0 } else { 0x40 };
        Ports1581 {
            pa_out,
            pb_out,
            pa,
            pb,
            side: if pa_out & 0x01 != 0 { 0 } else { 1 },
            not_ready: pa & 0x02 != 0,
            motor_on: pa_out & 0x04 == 0,
            jumpers: 8 + ((pa >> 3) & 3),
            power_led: pa_out & 0x20 != 0,
            activity_led: pa_out & 0x40 != 0,
            disk_changed: pa & 0x80 == 0,
            data_out: pb_out & 0x02 != 0,
            clk_out: pb_out & 0x08 != 0,
            atn_ack: pb_out & 0x10 != 0,
            fast_serial_out: pb_out & 0x20 != 0,
            writable: pb & 0x40 != 0,
            data_in: pb & 0x01 != 0,
            clk_in: pb & 0x04 != 0,
            atn_in: pb & 0x80 != 0,
        }
    }

    /// Side-effect-free peek of the drive CPU's map.
    pub fn peek(&self, addr: u16) -> u8 {
        match addr >> 13 {
            0 => self.ram[(addr & 0x1fff) as usize],
            1 => self.cpu_last_data,
            2 => {
                if addr & 0xf == 0 {
                    return self.ports_pa_read();
                }
                if addr & 0xf == 1 {
                    return self.ports_pb_read();
                }
                self.cia.peek(addr)
            }
            3 => self.wd.peek(addr & 3),
            _ => self.rom[(addr & 0x7fff) as usize],
        }
    }

    fn ports_pa_read(&self) -> u8 {
        // The jumpers need the unit; a peek reports the latched PA as the CPU sees it
        // with the jumpers the drive reads (bits 3-4 of the stored read are the unit).
        let c = &self.cia.c_cia;
        let number = self.number;
        let mut tmp = (8 * number) as u8;
        if !self.wd.fdd.disk_change() {
            tmp |= 0x80;
        }
        (tmp & !c[CIA_DDRA]) | (c[CIA_PRA] & c[CIA_DDRA])
    }

    fn ports_pb_read(&self) -> u8 {
        let c = &self.cia.c_cia;
        (((c[CIA_PRB] & 0x1a) | self.iecbus.drv_port) ^ 0x85) | if self.read_only { 0 } else { 0x40 }
    }

    /// Deduplicated drive-PC sample (the 1541's `sample_pc_change`).
    pub fn sample_pc_change(&mut self) -> Option<(u16, u8, u8, u8, u8, u8, u64)> {
        let pc = self.core.reg_pc;
        if self.last_sample_pc == Some(pc) {
            return None;
        }
        self.last_sample_pc = Some(pc);
        Some((pc, self.core.reg_a, self.core.reg_x, self.core.reg_y, self.core.reg_sp, self.core.status(), self.drive_clk))
    }

    // ── snapshot hooks (drive_snapshot.rs) ──────────────────────────────────

    pub(crate) fn snapshot_stop_clk(&self) -> u64 {
        self.stop_clk
    }
    pub(crate) fn snapshot_set_stop_clk(&mut self, v: u64) {
        self.stop_clk = v;
    }
    pub(crate) fn snapshot_sync_accum(&self) -> u32 {
        self.sync_accum
    }
    pub(crate) fn snapshot_set_sync_accum(&mut self, v: u32) {
        self.sync_accum = v;
    }
    pub(crate) fn snapshot_clear_pending_reset(&mut self) {
        self.reset_pending = false;
        self.int.global_pending_int &= !IK_RESET;
    }

    /// Write the CIA module at the live clock.
    pub(crate) fn snapshot_write_cia(&mut self, s: &mut crate::vice_snapshot_stream::SnapshotT) {
        let number = self.number;
        let clk = self.core.clk;
        self.cia.clk = clk;
        let mut p = ports_of!(self, number, clk);
        self.cia.snapshot_write_module(&mut p, s);
        // The settle may have moved the line; hand it to the CPU as the loop would.
        for (level, rclk) in self.cia.irq_events.drain(..) {
            self.int.set_irq(0, level, rclk);
        }
    }

    /// Read the CIA module at the restored clock; `cia_restore_int` sets the source's
    /// pending bit to the restored line level.
    pub(crate) fn snapshot_read_cia(&mut self, s: &mut crate::vice_snapshot_stream::SnapshotT) -> Result<(), String> {
        let number = self.number;
        let clk = self.core.clk;
        self.cia.clk = clk;
        let mut p = ports_of!(self, number, clk);
        let irq = self.cia.snapshot_read_module(&mut p, s)?;
        // The restore's own IFR_NEXT events belong to the restored machine's future.
        let evs: Vec<(bool, u64)> = self.cia.irq_events.drain(..).collect();
        // interrupt_restore_irq: the pending bit follows the restored level.
        if irq {
            self.int.pending_int[0] |= IK_IRQ;
        } else {
            self.int.pending_int[0] &= !IK_IRQ;
        }
        for (level, rclk) in evs {
            self.int.set_irq(0, level, rclk);
        }
        Ok(())
    }

    /// The fast-serial direction as the board last drove it (PB5).
    pub fn fast_dir(&self) -> bool {
        self.fast_dir
    }

    /// Re-derive the drive's own IEC output (`drv_data[unit]`) and PB5 from the CIA's
    /// port B as it stands — what `store_ciapb` last wrote. VICE's `undump_ciapb` for the
    /// 1581 is empty (its global `iecbus` rides the machine's snapshot); here the drive
    /// keeps its own copy, so a restore puts it back from the pins.
    pub(crate) fn resync_iec_output(&mut self) {
        let pb = self.cia.pb_out();
        let slot = self.number + 8;
        let dd = !pb;
        self.iecbus.drv_data[slot] = dd;
        let d = dd as u32;
        let cb = self.iecbus.cpu_bus as u32;
        self.iecbus.drv_bus[slot] = (((d << 3) & 0x40) | ((d << 6) & ((d | cb) << 3) & 0x80)) as u8;
        self.fast_dir = pb & 0x20 != 0;
    }

    /// `unit − 8` as the board last saw it.
    pub fn number(&self) -> usize {
        self.number
    }
    pub(crate) fn set_number(&mut self, number: usize) {
        self.number = number;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spec 872 D1 — the map: RAM at $0000-$1FFF, nothing at $2000-$3FFF (the open bus:
    /// the last byte on it), the CIA every 16 bytes through $4000-$5FFF, the WD every 4
    /// through $6000-$7FFF, the ROM from $8000, which a write does not reach.
    #[test]
    fn the_map_has_an_open_bus_and_mirrors() {
        let mut d = Drive1581::new(0);
        let mut rom = vec![0u8; 0x8000];
        rom[0] = 0x42;
        d.set_rom(&rom).unwrap();
        d.power_on(0);
        let mut clk = 100u64;
        let core_clk: *mut u64 = &mut clk;
        let mut bus = Bus1581 {
            ram: &mut d.ram,
            rom: &d.rom,
            cia: &mut d.cia,
            ports: ports_of!(d, 0, 0),
            clk_ptr: core_clk,
            cpu_last_data: &mut d.cpu_last_data,
        };
        bus.write(0x1fff, 0x5a);
        assert_eq!(bus.read(0x1fff), 0x5a, "RAM to $1FFF");
        bus.write(0x0010, 0x77);
        assert_eq!(bus.read(0x2010), 0x77, "$2000-$3FFF reads the open bus");
        bus.write(0x3456, 0x13);
        assert_eq!(bus.read(0x3ffe), 0x13, "a write there only puts its byte on the bus");
        assert_eq!(bus.read(0x0010), 0x77, "and reaches no RAM");
        bus.write(0x4003, 0xa5); // DDRB
        assert_eq!(bus.read(0x5ff3), 0xa5, "the CIA every 16 bytes");
        bus.write(0x7ffd, 0x21); // WD track register through the last mirror
        assert_eq!(bus.read(0x6001), 0x21, "the WD every 4 bytes");
        bus.write(0x8000, 0x00);
        assert_eq!(bus.read(0x8000), 0x42, "the ROM ignores a write");
    }
}
