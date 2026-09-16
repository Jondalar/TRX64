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
    /// Spec 853 — a bus master with a transfer waiting. Asked at every instruction
    /// boundary while the port is active, so it is a vtable call and never a downcast.
    /// A device that only answers the bus never overrides it.
    fn dma_pending(&self) -> bool {
        false
    }
    /// A copy for a cloned machine, or None. Default None: a host's device belongs to
    /// the host, and a copy of the machine — a sandbox, a scratch instance — must not
    /// share it. A profile's device that is part of the machine (Spec 852's UCI block)
    /// returns a copy of itself.
    fn clone_device(&self) -> Option<Box<dyn ExpansionDevice>> {
        None
    }
    /// The expansion port's /RESET line reached this device.
    ///
    /// Default: nothing. 850 made "no reset calls a device" a BLANKET rule, which was
    /// adopted for the UCI block and is right there — `command_protocol.vhd:292-306`, only
    /// the FPGA system reset clears it, and a C64 reset must leave it standing. It is
    /// wrong for anything that is really out on the port: the connector carries /RESET, and
    /// VICE resets the REU with the cartridge (`c64carthooks.c:2412`). So the choice is the
    /// DEVICE's, not the machine's, and a device that says nothing keeps 850's behaviour.
    fn reset(&mut self) {}
}

/// Spec 853 D1 — several devices in one place.
///
/// 850 gave the port two places: the machine profile's own device and a host's. That was
/// enough while one thing sat on it. A `u64` running the UCI block (profile) with an REU
/// attached, inside a host that wants its own device, is three devices in two places.
///
/// VICE answers this with a LIST, not a third slot: its REU and GeoRAM live in the "IO
/// Slot", where "any number of 'IO Slot' carts can be, in theory, active at a time"
/// (`c64cart.c:180-210`), because they claim no `game`/`exrom` and map only into IO1/IO2.
/// That is exactly the shape of every device this port carries.
///
/// The core did not have to be opened for it: `ExpansionDevice` is a trait, so a device
/// that holds several and fans out satisfies the same contract. Order is insertion order
/// and the first non-`None` read answer wins, which is the rule one place already had.
#[derive(Default)]
pub struct ExpansionChain {
    devices: Vec<Box<dyn ExpansionDevice>>,
    snoop: Vec<u16>,
}

impl ExpansionChain {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, dev: Box<dyn ExpansionDevice>) {
        for &a in dev.snoop_addresses() {
            if !self.snoop.contains(&a) {
                self.snoop.push(a);
            }
        }
        self.devices.push(dev);
    }

    pub fn with(mut self, dev: Box<dyn ExpansionDevice>) -> Self {
        self.push(dev);
        self
    }

    /// Remove the first device of this type, leaving every other member — and its state —
    /// exactly where it was. A chain that can only grow is unusable for a host whose
    /// devices come and go: on a U64 the firmware turns `C64_REU_ENABLE` on and off while
    /// the machine runs.
    pub fn remove<T: 'static>(&mut self) -> Option<Box<dyn ExpansionDevice>> {
        let idx = self.devices.iter().position(|d| d.as_ref().as_any().is::<T>())?;
        let dev = self.devices.remove(idx);
        self.rebuild_snoop();
        Some(dev)
    }

    /// The cached union is only correct while the membership is. Rebuilt on every removal
    /// so an address nobody registers any more stops being snooped.
    fn rebuild_snoop(&mut self) {
        self.snoop.clear();
        for d in &self.devices {
            for &a in d.snoop_addresses() {
                if !self.snoop.contains(&a) {
                    self.snoop.push(a);
                }
            }
        }
    }

    pub fn len(&self) -> usize {
        self.devices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }

    /// The first device of this concrete type, for `Machine::reu()` and friends.
    ///
    /// NOTE `d.as_ref()` is load-bearing. `AsAny` has a blanket impl for every `T: Any`,
    /// and `Box<dyn ExpansionDevice>` is itself such a `T` — so calling `as_any()` on the
    /// BOX hands back the box as the concrete type and every downcast quietly fails. The
    /// deref to `&dyn ExpansionDevice` is what makes the device's own type visible.
    pub fn find<T: 'static>(&self) -> Option<&T> {
        self.devices.iter().find_map(|d| d.as_ref().as_any().downcast_ref::<T>())
    }

    pub fn find_mut<T: 'static>(&mut self) -> Option<&mut T> {
        self.devices.iter_mut().find_map(|d| d.as_mut().as_any_mut().downcast_mut::<T>())
    }
}

impl ExpansionDevice for ExpansionChain {
    fn read(&mut self, a: Access, cart: Option<u8>) -> Option<u8> {
        let mut answer = None;
        // EVERY device sees the access — a read has side effects on more than the one
        // that answers it — and the first answer stands.
        for d in self.devices.iter_mut() {
            let v = d.read(a, cart);
            answer = answer.or(v);
        }
        answer
    }

    fn peek(&self, addr: u16, cart: Option<u8>) -> Option<u8> {
        self.devices.iter().find_map(|d| d.peek(addr, cart))
    }

    fn write(&mut self, a: Access, value: u8) {
        for d in self.devices.iter_mut() {
            d.write(a, value);
        }
    }

    fn snoop_addresses(&self) -> &[u16] {
        &self.snoop
    }

    fn snoop_write(&mut self, a: Access, value: u8) {
        for d in self.devices.iter_mut() {
            d.snoop_write(a, value);
        }
    }

    fn lines(&self) -> PortLines {
        self.devices.iter().fold(PortLines::default(), |acc, d| acc.or(d.lines()))
    }

    fn take_stop(&mut self) -> bool {
        // Poll every one: the flag is a one-shot and a device that is never asked keeps
        // it for ever.
        let mut stop = false;
        for d in self.devices.iter_mut() {
            stop |= d.take_stop();
        }
        stop
    }

    fn dma_pending(&self) -> bool {
        self.devices.iter().any(|d| d.dma_pending())
    }

    fn reset(&mut self) {
        for d in self.devices.iter_mut() {
            d.reset();
        }
    }

    fn clone_device(&self) -> Option<Box<dyn ExpansionDevice>> {
        // All or nothing: a chain that silently dropped the members a host owns would
        // hand back a machine that is not the one that was copied.
        let mut out = ExpansionChain::new();
        for d in &self.devices {
            out.push(d.clone_device()?);
        }
        Some(Box::new(out))
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

// ── Spec 854 — the expansion RAM the host owns ────────────────────────────────────

/// Where an expansion device's RAM actually lives.
///
/// 853 gave the REU a `Vec<u8>` of its own, which is right for a standalone machine and
/// is what makes a `.c64re` dump round-trip. It is wrong inside a host: on the U64 the
/// REU's RAM IS the firmware's DDR, and the firmware preloads an image by writing there
/// with its own CPU — so a device that allocates its own gives two copies, and every
/// preload lands in the one the C64 never reads.
///
/// This is a trait the device HOLDS, not a borrow threaded through a call, for two
/// reasons. GeoRAM reads its RAM on every `$DE00-$DEFF` access rather than only during a
/// transfer, so a store handed to `run_dma` would serve the REU and not it. And a
/// `'static` device cannot hold a `&mut [u8]` without putting a lifetime on `Machine` and
/// every call site in the crate.
pub trait ExpansionRam: Send {
    fn len(&self) -> u32;
    /// `None` means "nothing backs this address" — the store is not lent right now, or the
    /// offset is past what is fitted. The DEVICE then supplies its own not-backed value,
    /// which for an REU is the floating-bus latch.
    ///
    /// This returns an `Option` because a bare `u8` cannot say it. UE2 found that the hard
    /// way: it had to return `0xFF` and note that this was correct only because it happened
    /// to match `Reu`'s default — a host picking any other value would have disagreed with
    /// the device silently, which is the worst shape a defect can take.
    fn read(&self, off: u32) -> Option<u8>;
    fn write(&mut self, off: u32, value: u8);
    /// A copy for a cloned machine, or `None` when the bytes belong to a host — the same
    /// rule `ExpansionDevice::clone_device` follows.
    fn clone_ram(&self) -> Option<Box<dyn ExpansionRam>> {
        None
    }
    /// True when this device owns the bytes. A borrowed store is in no snapshot: its
    /// contents belong to the host's memory image, which has its own persistence.
    fn is_owned(&self) -> bool {
        false
    }
}

/// The store a standalone machine gets.
pub struct OwnedRam(pub Vec<u8>);

impl OwnedRam {
    pub fn new(bytes: usize) -> Self {
        OwnedRam(vec![0; bytes])
    }
}

impl ExpansionRam for OwnedRam {
    fn len(&self) -> u32 {
        self.0.len() as u32
    }
    fn read(&self, off: u32) -> Option<u8> {
        self.0.get(off as usize).copied()
    }
    fn write(&mut self, off: u32, value: u8) {
        if let Some(b) = self.0.get_mut(off as usize) {
            *b = value;
        }
    }
    fn clone_ram(&self) -> Option<Box<dyn ExpansionRam>> {
        Some(Box::new(OwnedRam(self.0.clone())))
    }
    fn is_owned(&self) -> bool {
        true
    }
}
