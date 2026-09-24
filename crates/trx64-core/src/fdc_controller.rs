//! fdc_controller.rs — Spec 875: a controller of the host's own in the 1581.
//!
//! [`FdcController`] takes the place of the 1581's WD1772 on the drive CPU's bus
//! (`$6000-$7FFF`, four registers mirrored) and of its mechanism on the CIA's glue
//! lines. A host that uses `trx64-core` as a library (UE2's model of the Ultimate 64's
//! `wd177x.vhd`, with the firmware serving sectors) fits one through
//! [`crate::Machine::attach_fdc_controller`] while the drive is off. TRX64's own WD1772
//! and its MFM surface stay the default and are not touched by any of this.
//!
//! Time is the drive's own clock (`core.clk`, 2 MHz on every model): every register
//! access carries the cycle it happens at, and the controller is caught up to the end of
//! every slice the drive runs. The drive clock restarts at 0 at every drive reset, which
//! is why [`FdcController::drive_reset`] carries the new clock.

use crate::expansion::AsAny;

/// What the drive board drives toward the controller — the U64's `side_0` and
/// `motor_on_i`. Composed from CIA port A as its pins stand (`PRA | !DDRA`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FdcBoardOut {
    /// PA0 pin level: `true` = high. The 1581 reads physical head 0 then
    /// (VICE `side = PA0 ? 0 : 1`, head-inverted surface: Spec 872 §4).
    pub side0: bool,
    /// PA2 low and the drive powered — the spindle turns.
    pub motor_on: bool,
}

impl FdcBoardOut {
    /// Port A released, as it stands after a reset: every pin an input, pulled high.
    pub const RELEASED: FdcBoardOut = FdcBoardOut { side0: true, motor_on: false };

    /// From port A's composed output byte, the drive powered.
    #[inline]
    pub fn from_pa(pa: u8) -> FdcBoardOut {
        FdcBoardOut { side0: pa & 0x01 != 0, motor_on: pa & 0x04 == 0 }
    }
}

/// What the controller's mechanism side drives back into the CIA. `true` = asserted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FdcBoardIn {
    /// PA1 /RDY low.
    pub ready: bool,
    /// PA7 /DISK CHANGE low.
    pub disk_changed: bool,
    /// PB6 /WPS low.
    pub write_protected: bool,
}

impl FdcBoardIn {
    /// The CIA's port-A input bits this drives: PA1 and PA7, 1 = high (not asserted).
    #[inline]
    pub fn pa_bits(self) -> u8 {
        (if self.ready { 0 } else { 0x02 }) | (if self.disk_changed { 0 } else { 0x80 })
    }

    /// The CIA's PB6 input: `0x40` = high (writable).
    #[inline]
    pub fn pb6(self) -> u8 {
        if self.write_protected {
            0
        } else {
            0x40
        }
    }
}

/// The 1581's drive clock: 2 MHz on every model. A drive cycle is 0.5 µs.
pub const DRIVE_HZ_1581: u32 = 2_000_000;

pub trait FdcController: AsAny + Send {
    /// Shown in refusals, the monitor's `drive` verb and a checkpoint.
    fn name(&self) -> String;

    /// The 6502 reads register `reg` (0-3) at drive cycle `clk`. The controller has
    /// caught up to `clk` itself first. May change state (a data read clears DRQ).
    fn read(&mut self, clk: u64, reg: u8) -> u8;

    /// The 6502 stores `val` into register `reg` (0-3) at drive cycle `clk`.
    fn store(&mut self, clk: u64, reg: u8, val: u8);

    /// Register `reg` as a read would return it, without side effects (monitor).
    fn peek(&self, reg: u8) -> u8;

    /// Catch up to drive cycle `clk`. Called at the end of every slice the drive
    /// runs. `clk` never decreases between two calls except across `drive_reset`
    /// and `rebase`; the same `clk` may come twice.
    fn clock_to(&mut self, clk: u64);

    /// Port A changed what it drives, at drive cycle `clk` (a CIA store that moved
    /// PA0 or PA2). The controller has been run to no later than `clk` under the old
    /// value.
    fn board_out(&mut self, clk: u64, out: FdcBoardOut);

    /// What the mechanism drives into the CIA now. Read at every CIA port A/B read
    /// and by the side-effect-free accessors; `&self`, no time passes.
    fn board_in(&self) -> FdcBoardIn;

    /// The head, physical track (0-83) and side (0/1), for the monitor and the
    /// daemon's drive panel. Required: TRX64 has no mechanism of its own here.
    fn head(&self) -> (u8, u8);

    /// The drive's RESET ran — its own RESET input, a C64 RESET over a connected
    /// line, hold, release, power-on, the machine's build. The drive clock restarts:
    /// it is `clk` now. Port A stands released after it (`side0`, motor off).
    fn drive_reset(&mut self, clk: u64);

    /// The drive's power switch. A notification only: it resets nothing, and the
    /// registers stand across an off (the U64's power bit is not part of
    /// `drv_reset`). Off: no call follows until it is on again; the motor stopped with
    /// it. On is always followed by `drive_reset` — the drive's power-on reset, a
    /// separate event.
    fn power(&mut self, _on: bool) {}

    /// The drive clock is `clk` without time having passed for the controller: at
    /// attach, and after a restore that did not carry its state.
    fn rebase(&mut self, clk: u64);

    /// Checkpoint hooks. `None` = opted out: the checkpoint is taken without the
    /// controller's state and names it.
    fn checkpoint(&self) -> Option<serde_json::Value> {
        None
    }
    fn restore(&mut self, _state: &serde_json::Value) -> Result<(), String> {
        Err(format!("{}: carries no checkpoint state", self.name()))
    }

    /// A copy for a cloned machine, or `None`. Default `None`: a host's controller
    /// belongs to the host.
    fn clone_device(&self) -> Option<Box<dyn FdcController>> {
        None
    }
}

/// What a cloned machine holds where the original had a controller that gave no copy:
/// a socket with no chip. The board reads the open bus in the register window (U6 Y3
/// selects nothing that drives D0-D7), a store goes nowhere, and nothing drives the
/// mechanism lines: PA1, PA7 and PB6 read high (the 8520's pull-ups) — not ready, no
/// disk change, not protected. The DOS then answers `74,DRIVE NOT READY` from its /RDY
/// check (`$CDBC`). A low PA7 would send it into its STEP-IN / STEP-OUT recovery first
/// (`$CD83-$CD99`), whose command stores wait at `$CBFA` for a BUSY the open bus never
/// shows.
#[derive(Clone, Debug)]
pub struct VacantFdc {
    name: String,
}

impl FdcController for VacantFdc {
    fn name(&self) -> String {
        self.name.clone()
    }
    fn read(&mut self, _clk: u64, _reg: u8) -> u8 {
        0xff
    }
    fn store(&mut self, _clk: u64, _reg: u8, _val: u8) {}
    fn peek(&self, _reg: u8) -> u8 {
        0xff
    }
    fn clock_to(&mut self, _clk: u64) {}
    fn board_out(&mut self, _clk: u64, _out: FdcBoardOut) {}
    fn board_in(&self) -> FdcBoardIn {
        FdcBoardIn { ready: false, disk_changed: false, write_protected: false }
    }
    fn head(&self) -> (u8, u8) {
        (0, 0)
    }
    fn drive_reset(&mut self, _clk: u64) {}
    fn rebase(&mut self, _clk: u64) {}
    fn clone_device(&self) -> Option<Box<dyn FdcController>> {
        Some(Box::new(self.clone()))
    }
}

/// Spec 875 §7 — the socket in a 1581 board: the controller, its name, what the board
/// last told it, the last command byte stored (for `wd().command`), and whether the last
/// restore or clone left it uncovered.
pub struct HostFdc {
    pub(crate) dev: Box<dyn FdcController>,
    pub(crate) name: String,
    /// The position's name ("A"/"B") for refusals the drive gives itself.
    pub(crate) position: &'static str,
    /// The last byte stored to register 0.
    pub(crate) last_cmd: u8,
    /// Port A as the controller last heard it (`board_out` is sent on change only).
    pub(crate) last_out: FdcBoardOut,
    /// A clone's vacancy: the register window reads the open bus.
    pub(crate) vacant: bool,
    /// Named in `Machine::fdc_uncovered`.
    pub(crate) uncovered: bool,
}

impl HostFdc {
    pub(crate) fn new(dev: Box<dyn FdcController>, position: &'static str) -> Self {
        let name = dev.name();
        HostFdc { dev, name, position, last_cmd: 0, last_out: FdcBoardOut::RELEASED, vacant: false, uncovered: false }
    }

    /// The controller's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether this is a clone's vacancy.
    pub fn is_vacant(&self) -> bool {
        self.vacant
    }

    /// What the mechanism side drives into the CIA now.
    #[inline]
    pub(crate) fn board_in(&self) -> FdcBoardIn {
        self.dev.board_in()
    }

    /// The drive's reset reached the board: port A stands released.
    pub(crate) fn drive_reset(&mut self, clk: u64) {
        self.last_out = FdcBoardOut::RELEASED;
        self.dev.drive_reset(clk);
    }
}

impl Clone for HostFdc {
    /// The controller is asked for a copy; one that gives none leaves a vacancy — the
    /// name kept, a socket with no chip — named uncovered in the clone.
    fn clone(&self) -> Self {
        let (dev, vacant, uncovered): (Box<dyn FdcController>, bool, bool) = match self.dev.clone_device() {
            Some(d) => (d, self.vacant, self.uncovered),
            None => (Box::new(VacantFdc { name: self.name.clone() }), true, true),
        };
        HostFdc {
            dev,
            name: self.name.clone(),
            position: self.position,
            last_cmd: self.last_cmd,
            last_out: self.last_out,
            vacant,
            uncovered,
        }
    }
}
