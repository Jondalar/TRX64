//! iec_device.rs — Spec 874: a device of the host's own on the IEC bus.
//!
//! [`IecDevice`] is the door to an `IECBUS_DEVICE_IECDEVICE` slot (4-11). The folder
//! device of Spec 873 is its first implementor; a host that uses `trx64-core` as a
//! library puts its own (UE2's Ultimate IEC processor, say) beside the 1541s through
//! [`crate::Machine::attach_iec_device`].
//!
//! A device is clocked in C64 cycles at every sync point the drives have — the end of
//! each instruction, a `$DD00` read, a `$DD00`/`$DD02` write that changes the port, and
//! the end of a held span ([`iec_devices_sync`]). What it pulls goes into its own
//! `drv_bus` slot and is folded into the wired-AND with everybody else's.

use crate::expansion::AsAny;
use crate::iec::IecCore;

/// The lines as the REST of the bus drives them — the C64 and every other device,
/// not this one. `true` = high (released). ATN is only ever driven by the C64.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IecLines {
    pub atn: bool,
    pub clk: bool,
    pub data: bool,
}

impl IecLines {
    /// Every line released.
    pub const RELEASED: IecLines = IecLines { atn: true, clk: true, data: true };

    /// The wire as the device reads it: the rest ANDed with its own pulls (the
    /// Ultimate's `inputs_raw`).
    pub fn with(self, own: IecOut) -> IecLines {
        IecLines { atn: self.atn, clk: self.clk && !own.clk, data: self.data && !own.data }
    }
}

/// What the device pulls low, open collector. `false` = released.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IecOut {
    pub clk: bool,
    pub data: bool,
}

impl IecOut {
    /// The device's `drv_bus` byte: `IECBUS_DEVICE_WRITE_CLK` (`0x40`) and
    /// `IECBUS_DEVICE_WRITE_DATA` (`0x80`) set when released, bits 0-5 clear — `0xc0`
    /// for a device that pulls nothing, the byte Spec 873's folder writes.
    #[inline]
    pub fn slot_byte(self) -> u8 {
        ((!self.clk as u8) << 6) | ((!self.data as u8) << 7)
    }
}

pub trait IecDevice: AsAny + Send {
    /// Shown in refusals, the monitor's `iec` column and a checkpoint's list.
    fn name(&self) -> String;

    /// Catch up to exactly C64 cycle `clk`. Until `clk` the lines were the ones the
    /// previous call gave; from `clk` on they are `bus`. `clk` never decreases between
    /// two calls except across `rebase`; the same `clk` may come twice (a `$DD00` read
    /// and the end of the same instruction).
    fn clock_to(&mut self, clk: u64, bus: IecLines);

    /// Its pulls now. Read right after every `clock_to`; folded into the wired-AND.
    fn outputs(&self) -> IecOut;

    /// The machine's clock is `clk` and the lines are `bus`, without time having
    /// passed for the device: at attach, after a restore that did not carry its
    /// state, after a C64 power cycle it lived through.
    fn rebase(&mut self, clk: u64, bus: IecLines);

    /// ATN changed to `level` at cycle `clk` (a `$DD00`/`$DD02` write). Always
    /// followed by `clock_to(clk, …)` carrying the same level; the device has been run
    /// to no later than `clk` under the old lines. Redundant with the level — ATN
    /// changes only at a write, and every write is a sync point — and provided because
    /// the Ultimate's processor takes ATN as a vector and the drives get it the same way.
    fn atn_edge(&mut self, _clk: u64, _level: bool) {}

    /// The unit numbers it answers to, a bit per unit (bit 8 = unit 8). Advisory —
    /// used for refusals only. Default: none the machine can know.
    fn units(&self) -> u16 {
        0
    }

    /// The model's clock rate, at attach and at every model switch (Spec 863).
    fn set_cpu_hz(&mut self, _hz: u32) {}

    /// The C64's RESET reached the bus (cold/warm reset, power-on). Whether a device
    /// is wired to the IEC RESET line is its own business. Default: nothing.
    fn c64_reset(&mut self) {}

    /// Checkpoint hooks. `None` = opted out: the checkpoint is taken without the
    /// device's state and names it.
    fn checkpoint(&self) -> Option<serde_json::Value> {
        None
    }
    fn restore(&mut self, _state: &serde_json::Value) -> Result<(), String> {
        Err(format!("{}: carries no checkpoint state", self.name()))
    }

    /// A copy for a cloned machine, or `None`. Default `None`, as
    /// `ExpansionDevice::clone_device`: a host's device belongs to the host.
    fn clone_device(&self) -> Option<Box<dyn IecDevice>> {
        None
    }
}

/// What a cloned machine holds where the original had a device that gave no copy:
/// the slot, the name and the claimed units, released lines, every call a no-op.
#[derive(Clone, Debug)]
pub struct VacantDevice {
    name: String,
    units: u16,
}

impl IecDevice for VacantDevice {
    fn name(&self) -> String {
        self.name.clone()
    }
    fn clock_to(&mut self, _clk: u64, _bus: IecLines) {}
    fn outputs(&self) -> IecOut {
        IecOut::default()
    }
    fn rebase(&mut self, _clk: u64, _bus: IecLines) {}
    fn units(&self) -> u16 {
        self.units
    }
    fn clone_device(&self) -> Option<Box<dyn IecDevice>> {
        Some(Box::new(self.clone()))
    }
}

/// The device at one slot.
pub struct SlottedDevice {
    pub slot: u8,
    pub dev: Box<dyn IecDevice>,
}

impl SlottedDevice {
    /// A vacancy left by a clone.
    pub fn is_vacant(&self) -> bool {
        self.dev.as_ref().as_any().is::<VacantDevice>()
    }

    /// The units it takes from drives and folders: its slot when that is 8-11, and
    /// whatever it claims.
    pub fn claims(&self) -> u16 {
        let own = if (8..=11).contains(&self.slot) { 1u16 << self.slot } else { 0 };
        own | self.dev.units()
    }
}

/// Spec 874 §5 — the devices on the bus, in slot order, plus the names a restore or a
/// clone could not cover.
#[derive(Default)]
pub struct IecDevices {
    list: Vec<SlottedDevice>,
    uncovered: Vec<String>,
}

impl Clone for IecDevices {
    /// Each device is asked for a copy; one that gives none leaves a vacancy — same
    /// slot, same name, lines released — and is named uncovered in the clone.
    fn clone(&self) -> Self {
        let mut uncovered = self.uncovered.clone();
        let list = self
            .list
            .iter()
            .map(|s| {
                let dev = s.dev.clone_device().unwrap_or_else(|| {
                    let name = s.dev.name();
                    if !uncovered.contains(&name) {
                        uncovered.push(name.clone());
                    }
                    Box::new(VacantDevice { name, units: s.dev.units() })
                });
                SlottedDevice { slot: s.slot, dev }
            })
            .collect();
        IecDevices { list, uncovered }
    }
}

impl IecDevices {
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &SlottedDevice> {
        self.list.iter()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut SlottedDevice> {
        self.list.iter_mut()
    }

    pub fn get(&self, slot: u8) -> Option<&dyn IecDevice> {
        self.list.iter().find(|s| s.slot == slot).map(|s| s.dev.as_ref())
    }

    pub fn get_mut(&mut self, slot: u8) -> Option<&mut (dyn IecDevice + 'static)> {
        self.list.iter_mut().find(|s| s.slot == slot).map(|s| s.dev.as_mut())
    }

    /// The occupied slots, a bit per slot.
    pub fn slots(&self) -> u16 {
        self.list.iter().fold(0u16, |m, s| m | (1 << s.slot))
    }

    /// Put `dev` at `slot`, keeping slot order. The caller has checked the slot is free.
    pub(crate) fn insert(&mut self, slot: u8, dev: Box<dyn IecDevice>) {
        let at = self.list.iter().position(|s| s.slot > slot).unwrap_or(self.list.len());
        self.list.insert(at, SlottedDevice { slot, dev });
    }

    /// Take the device at `slot` off the list; its name leaves the uncovered list.
    pub(crate) fn remove(&mut self, slot: u8) -> Option<Box<dyn IecDevice>> {
        let i = self.list.iter().position(|s| s.slot == slot)?;
        let s = self.list.remove(i);
        let name = s.dev.name();
        self.uncovered.retain(|n| *n != name);
        Some(s.dev)
    }

    /// Take every device matching `pred` off the list, in slot order.
    pub(crate) fn take_where(&mut self, pred: impl Fn(&SlottedDevice) -> bool) -> Vec<SlottedDevice> {
        let (taken, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.list).into_iter().partition(|s| pred(s));
        self.list = kept;
        taken
    }

    /// The names the last restore or clone could not cover.
    pub fn uncovered(&self) -> &[String] {
        &self.uncovered
    }

    pub(crate) fn set_uncovered(&mut self, names: Vec<String>) {
        self.uncovered = names;
    }

    /// The device whose slot or claim takes `unit`, if one does.
    pub fn claimant_of_unit(&self, unit: u8) -> Option<&SlottedDevice> {
        self.list.iter().find(|s| s.claims() & (1 << unit) != 0)
    }

    /// Deliver an ATN edge to every device, in slot order.
    #[inline]
    pub(crate) fn atn_edge(&mut self, clk: u64, level: bool) {
        for s in self.list.iter_mut() {
            s.dev.atn_edge(clk, level);
        }
    }
}

/// The lines as everybody but `slot` drives them.
#[inline]
pub fn lines_without(iec: &IecCore, slot: usize) -> IecLines {
    let mut others = iec.iecbus.cpu_bus;
    for s in 4..(8 + crate::iec::NUM_DISK_UNITS) {
        if s != slot {
            others &= iec.iecbus.drv_bus[s];
        }
    }
    IecLines { atn: iec.iecbus.cpu_bus & 0x10 != 0, clk: others & 0x40 != 0, data: others & 0x80 != 0 }
}

/// Spec 874 §5 — run every device up to C64 cycle `t` against the lines as they now
/// stand, in slot order, each pull in its slot before the next device looks; one fold
/// at the end. Called at each sync point after the drives have been fed, run and
/// written back.
pub fn iec_devices_sync(devs: &mut IecDevices, iec: &mut IecCore, t: u64) {
    for s in devs.list.iter_mut() {
        let slot = s.slot as usize;
        let bus = lines_without(iec, slot);
        s.dev.clock_to(t, bus);
        iec.iecbus.drv_bus[slot] = s.dev.outputs().slot_byte();
    }
    iec.iec_update_ports();
}
