//! iec.rs — the C64↔1541 IEC serial bus wired-AND core.
//!
//! 1:1 port of the TS oracle's `IecBusCore` (iec/iec-bus-core.ts), which mirrors
//! VICE 3.7.1 `iecbus_t` (src/iecbus/iecbus.c) + `iec_update_cpu_bus` /
//! `iec_update_ports` (src/c64/c64iec.c) + the 1541 VIA1 PB drive-side
//! contribution (src/drive/iec/via1d1541.c store_prb/read_prb).
//!
//! Line semantics (open-drain, wired-AND): a bit SET (=1) means "this driver is
//! NOT asserting" (line released / pulled HIGH); a bit CLEAR (=0) means "asserting"
//! (line pulled LOW). The effective line is the AND of every driver.
//!
//! Single-1541 baseline: only unit 8 is modelled. The other 15 drv_bus slots stay
//! 0xff (= memset, AND-identity) exactly like VICE for the missing drives.
//!
//! Bit maps (see iec-bus-core.ts §17.1):
//!   CIA2 PA (raw, pre-invert): bit3=ATN_OUT, bit4=CLK_OUT, bit5=DATA_OUT,
//!                              bit6=CLK_IN, bit7=DATA_IN.
//!   cpu_bus (post `data = ~PA`): bit4=ATN, bit6=CLK, bit7=DATA (C64 intent).
//!   cpu_port: AND-fold of cpu_bus + all drv_bus; bit6=CLK line, bit7=DATA line.
//!   drv_data[8] = ~(drive VIA1 PB output): bit1=DATA_OUT, bit3=CLK_OUT, bit4=ATN_ACK.

/// IEC bus core state (unit-8 baseline). Lives on the `Machine`, borrowed into the
/// `FullBus` for the duration of each instruction.
#[derive(Clone, Copy, Debug)]
pub struct IecCore {
    /// C64-side intent (post `~PA` invert) — VICE iecbus.cpu_bus.
    pub cpu_bus: u8,
    /// Effective bus state the C64 reads ($DD00 bits 6/7) — VICE iecbus.cpu_port.
    pub cpu_port: u8,
    /// Unit-8 bus contribution — VICE iecbus.drv_bus[8].
    pub drv_bus_8: u8,
    /// Raw drive VIA1 PB output, inverted (= ~ORB-out) — VICE iecbus.drv_data[8].
    pub drv_data_8: u8,
    /// Effective bus state the DRIVE reads (wired to VIA1 PB inputs) — VICE
    /// iecbus.drv_port. bit0=DATA_IN, bit2=CLK_IN, bit7=ATN. Power-on 0x85.
    pub drv_port: u8,
    /// ATN edge-detect latch (VICE iecbus.c iec_old_atn). cpu_bus&0x10 of last write.
    pub iec_old_atn: u8,
}

impl Default for IecCore {
    fn default() -> Self {
        Self::new()
    }
}

impl IecCore {
    /// Power-on state — matches `iecbus_init()` (memset 0xff) + initial cpu_port/
    /// drv_data released, iec_old_atn = 0x10 (ATN released).
    pub fn new() -> Self {
        Self {
            cpu_bus: 0xff,
            cpu_port: 0xff,
            drv_bus_8: 0xff,
            drv_data_8: 0xff,
            drv_port: 0x85,
            iec_old_atn: 0x10,
        }
    }

    /// `iec_update_cpu_bus` (c64iec.c:121-124). `data` = INVERTED PA latch (`~PA`).
    #[inline]
    pub fn update_cpu_bus(&mut self, data: u8) {
        let d = data;
        self.cpu_bus =
            (((d << 2) & 0x80) | ((d << 2) & 0x40) | ((d << 1) & 0x10)) & 0xff;
    }

    /// `iec_update_ports` (c64iec.c:126-138): AND-fold cpu_bus with every drv_bus
    /// (only unit 8 is non-0xff here), then derive `drv_port` (what the drive's
    /// VIA1 PB reads):
    ///   drv_port = ((cpu_port>>4)&0x04)   // CLK line (cpu_port bit6) → PB2 CLK_IN
    ///            | (cpu_port>>7)           // DATA line (cpu_port bit7) → PB0 DATA_IN
    ///            | ((cpu_bus<<3)&0x80)     // ATN intent (cpu_bus bit4) → PB7 ATN_IN
    /// ATN comes from raw cpu_bus (C64-driven only), NOT the post-AND cpu_port.
    #[inline]
    pub fn update_ports(&mut self) {
        self.cpu_port = (self.cpu_bus & self.drv_bus_8) & 0xff;
        self.drv_port = (((self.cpu_port >> 4) & 0x04)
            | (self.cpu_port >> 7)
            | ((self.cpu_bus << 3) & 0x80))
            & 0xff;
    }

    /// `drv_bus[unit]` recomputation for a type-1541 drive (iecbus.c:281-285).
    /// drv_bus = ((dd<<3)&0x40) | ((dd<<6) & ((~dd ^ cpu_bus)<<3) & 0x80).
    /// The second term is the hardware ATN-acknowledge: it folds cpu_bus (ATN) so
    /// the drive auto-pulls DATA when ATN is asserted and the drive released DATA.
    #[inline]
    pub fn recompute_drv_bus(&mut self) {
        let dd = self.drv_data_8 as u32;
        let term1 = (dd << 3) & 0x40;
        let xor = ((!dd) ^ (self.cpu_bus as u32)) & 0xffff_ffff;
        let shifted = (xor << 3) & 0xffff_ffff;
        let term2 = (dd << 6) & shifted & 0x80;
        self.drv_bus_8 = ((term1 | term2) & 0xff) as u8;
    }

    /// Drive writes its VIA1 PB output (= via1d1541 store_prb). `pb_out` is the
    /// composed PB output byte `(ORB | ~DDRB)` (viacore VIA_PRB/VIA_DDRB store).
    /// drv_data = ~pb_out, recompute drv_bus, update ports.
    #[inline]
    pub fn drive_store_pb(&mut self, pb_out: u8) {
        self.drv_data_8 = (!pb_out) & 0xff;
        self.recompute_drv_bus();
        self.update_ports();
    }

    /// C64 stores $DD00 PA (= iecbus_cpu_write_conf1). `data` = INVERTED PA byte.
    /// Returns `Some(atn_high)` when the ATN line edge flipped (for VIA1 CA1
    /// signalling); `None` if ATN unchanged. Mutation order matches VICE:
    /// update cpu_bus → ATN-edge → recompute drv_bus[8] → update ports.
    #[inline]
    pub fn c64_store_dd00(&mut self, data: u8) -> Option<bool> {
        self.update_cpu_bus(data);
        let new_atn = self.cpu_bus & 0x10;
        let edge = if self.iec_old_atn != new_atn {
            self.iec_old_atn = new_atn;
            Some(new_atn != 0)
        } else {
            None
        };
        self.recompute_drv_bus();
        self.update_ports();
        edge
    }
}
