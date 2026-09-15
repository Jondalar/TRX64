//! Spec 850 — the expansion port as a device interface.
//!
//! The port used to know one thing: `Machine::cartridge`, a mapper with ROM lines. A
//! device that has registers in `$DE00-$DFFF` and no ROM — the Ultimate Command
//! Interface, an REU, an ACIA, a sampler — had no way onto the bus except by pretending
//! to be a cartridge, and the machine then believed it had one everywhere (BankInfo,
//! the VSF export, the reset that calls `CartMapper::reset`).
//!
//! Nothing here is a cartridge. There are two places:
//!
//! * the machine PROFILE's own device (the UCI block on the `u64` profile, Spec 852),
//!   asked first — on the hardware the UCI's register answer is tested before cartridge
//!   I/O data (`slot_slave.vhd:292-301`);
//! * a HOST device, attached by whoever embeds the core.
//!
//! Both see every access; the answer is chosen profile > host > cartridge > open bus.
//! With neither attached every hook is an `Option` that is `None`, so a stock machine
//! runs the pre-850 path.

use std::any::Any;

/// Who put the access on the bus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessKind {
    /// A real CPU read or write cycle.
    Cpu,
    /// A dummy read (page-crossing indexed addressing, the interrupt prologue) or the
    /// RMW dummy write-back. The hardware sees these cycles like any other.
    Dummy,
    /// The host reaching in between instructions (`read_full_live`, `write_full`): a
    /// debugger, or a firmware's DMA. Not a C64 bus cycle.
    Host,
}

/// One access as the port sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Access {
    pub addr: u16,
    pub clk: u64,
    pub kind: AccessKind,
    /// Cycles the BA steal took immediately before this read (0 for writes and host
    /// access).
    pub stalled: u32,
    /// Those of `stalled` in which AEC was still high, so the CPU's address — and
    /// IO1/IO2 — was on the bus. The rest the VIC owned: once AEC falls it drives the
    /// address and the PLA selects no I/O.
    pub stalled_on_bus: u32,
}

/// What a device drives onto the port.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PortLines {
    pub irq: bool,
    pub nmi: bool,
    /// Hold the 6510 (DMA / freeze). Checked at every instruction boundary.
    pub hold: bool,
}

impl PortLines {
    #[inline]
    pub fn or(self, other: PortLines) -> PortLines {
        PortLines { irq: self.irq || other.irq, nmi: self.nmi || other.nmi, hold: self.hold || other.hold }
    }
}

/// The 6510 is not executing while the rest of the machine runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hold {
    /// DMA / freeze / C64_STOP: VIC, CIAs, SID and drive 8 keep running.
    Cpu,
    /// The RESET line held: only the VIC runs — the 6569 has no reset pin. CIAs, SID and
    /// the drive stand still; releasing this is followed by the reset itself, which
    /// rebuilds them.
    Reset,
}

/// Lets the machine reach the concrete device behind a profile place (Spec 852's
/// `Machine::uci`). Implemented for every `'static` type; nobody writes it by hand.
pub trait AsAny {
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

impl<T: Any> AsAny for T {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

pub trait ExpansionDevice: AsAny + Send {
    /// A read of `$DE00-$DFFF` while I/O is banked in. `cart` is the cartridge's answer
    /// (None: it had none). `Some(v)` is what the bus sees; `None` keeps the cartridge
    /// byte, else the open bus.
    fn read(&mut self, a: Access, cart: Option<u8>) -> Option<u8>;
    /// Side-effect-free, for `read_full`, `peek_lens` and the monitor.
    fn peek(&self, addr: u16, cart: Option<u8>) -> Option<u8>;
    /// A write to `$DE00-$DFFF` while I/O is banked in, in addition to the cartridge.
    /// It does not change whether the cartridge consumed the write.
    fn write(&mut self, a: Access, value: u8);
    /// Addresses outside `$DE00-$DFFF` whose writes this device sees whatever the
    /// banking — the `$FF00` REU/UCI trigger, the U64 unlock at `$D036`/`$D038`.
    fn snoop_addresses(&self) -> &[u16] {
        &[]
    }
    /// A write to one of the snooped addresses. Every write cycle is reported, the RMW
    /// dummy write-back included; the write itself proceeds unchanged.
    fn snoop_write(&mut self, _a: Access, _value: u8) {}
    /// What the device drives onto the port now. Sampled every cycle for IRQ/NMI and at
    /// every instruction boundary for `hold`.
    fn lines(&self) -> PortLines {
        PortLines::default()
    }
    /// One-shot: true once after an access that must end the run at the next
    /// instruction boundary (`RunStop::Device`).
    fn take_stop(&mut self) -> bool {
        false
    }
    /// A copy for a cloned machine, or None. Default None: a host's device belongs to
    /// the host, and a copy of the machine — a sandbox, a scratch instance — must not
    /// share it. A profile's device that is part of the machine (Spec 852's UCI block)
    /// returns a copy of itself.
    fn clone_device(&self) -> Option<Box<dyn ExpansionDevice>> {
        None
    }
}

/// One place on the port. Behaves like the `Option` it wraps; exists so that cloning a
/// `Machine` asks the device (`clone_device`) instead of requiring every device to be
/// `Clone`.
#[derive(Default)]
pub struct PortSlot(pub Option<Box<dyn ExpansionDevice>>);

impl Clone for PortSlot {
    fn clone(&self) -> Self {
        PortSlot(self.0.as_ref().and_then(|d| d.clone_device()))
    }
}

impl std::ops::Deref for PortSlot {
    type Target = Option<Box<dyn ExpansionDevice>>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for PortSlot {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// The union of every attached device's snooped addresses, one bit per address, so the
/// write path asks a device only for an address somebody registered.
#[derive(Clone)]
pub struct SnoopSet {
    bits: [u64; 1024],
}

impl Default for SnoopSet {
    fn default() -> Self {
        Self { bits: [0; 1024] }
    }
}

impl SnoopSet {
    pub fn insert(&mut self, addr: u16) {
        self.bits[(addr >> 6) as usize] |= 1u64 << (addr & 63);
    }

    #[inline]
    pub fn contains(&self, addr: u16) -> bool {
        self.bits[(addr >> 6) as usize] & (1u64 << (addr & 63)) != 0
    }

    pub fn is_empty(&self) -> bool {
        self.bits.iter().all(|w| *w == 0)
    }
}
