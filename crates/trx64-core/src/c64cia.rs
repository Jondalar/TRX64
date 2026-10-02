//! c64cia.rs — the C64's CIA 1 and CIA 2: VICE `c64/c64cia1.c` and `c64/c64cia2.c`, the
//! glue that wires two [`CiaCore`]s into the machine.
//!
//! **CIA 1** (`$DC00`). Port A drives the keyboard's port-A lines and joystick port 2,
//! and PA6/PA7 select the POT lines; port B reads the keyboard's port-B lines and
//! joystick port 1. The interrupt pin is the CPU's IRQ. FLAG is the cassette read line
//! (no datasette here), SP/CNT go to the user port (nothing attached).
//!
//! **CIA 2** (`$DD00`). Port A bits 0-1 pick the VIC bank (`~PA & 3`), bits 3-5 drive
//! ATN, CLK and DATA out onto the serial bus, bits 6-7 read CLK and DATA back; PA2 and
//! port B are the user port. The interrupt pin is the CPU's NMI. FLAG is user-port pin
//! B, SP/CNT the user port.
//!
//! **What is not connected here**, and so takes VICE's "nothing attached" path: the user
//! port (`read_userport_*` return their argument, `store_userport_*` go nowhere), the
//! datasette, a parallel cable (`parallel_cable_cpu_execute`), the burst modification
//! (`burst_mod` stays `BURST_MOD_NONE`, so `read_ciaicr` / `read_sdr` / `store_sdr` do not
//! run the drives), joyport output devices (`store_joyport_dig`), the shift-lock key
//! (`keyboard_get_shiftlock` = 0) and the light pen (`vicii_set_light_pen`: the VIC here
//! has no light-pen input yet).
//!
//! **Chosen by the model row.** VICE's `c64scmodel.c` table gives the "C64" rows
//! `CIA_MODEL_6526` and the "C64C" rows `CIA_MODEL_6526A` (`CIA_MODEL_DEFAULT_OLD/NEW`),
//! both CIAs alike; `models.toml` carries that as `cia`. `write_offset` is 0 on x64sc
//! (`cia1_setup_context`), and the TOD runs on the row's clock and mains
//! (`cia1_set_timing`).
//!
//! **The serial bus.** VICE calls `iecbus_callback_write` from inside `store_ciapa` and
//! `iecbus_callback_read` from inside `read_ciapa`. Both touch the drives, which the
//! full bus owns alongside the CIA, so [`Cia2Ports`] carries the pins the bus sampled
//! for a `$DD00` read and hands a changed port-A output back for the bus to drive. The
//! order is VICE's: nothing in `ciacore_store` / `ciacore_read` runs between the hook and
//! the end of the access, and the CIA's own alarms touch neither the bus nor the drives.

use crate::ciacore::{CiaBackend, CiaCore, CiaPins, CIA_DDRA, CIA_DDRB, CIA_MODEL_6526, CIA_MODEL_6526A, CIA_PRA, CIA_PRB};
use crate::keyboard::{joystick_active_low_mask, JoystickState, KeyboardMatrix};

/// `cia->model` for a model row's `cia` column; `None` for a part VICE does not have.
pub fn model_of(row: &str) -> Option<u32> {
    match row {
        "6526" => Some(CIA_MODEL_6526),
        "6526A" => Some(CIA_MODEL_6526A),
        _ => None,
    }
}

/// `cia1_setup_context` + `ciacore_init`: CIA 1 on `ticks_per_sec` and `power_freq`.
/// Not reset yet — the machine resets it (`machine_specific_reset` → `ciacore_reset`).
pub fn new_cia1(model: u32, ticks_per_sec: u32, power_freq: u32) -> CiaCore {
    new_cia("CIA1", model, ticks_per_sec, power_freq)
}

/// `cia2_setup_context` + `ciacore_init`.
pub fn new_cia2(model: u32, ticks_per_sec: u32, power_freq: u32) -> CiaCore {
    new_cia("CIA2", model, ticks_per_sec, power_freq)
}

fn new_cia(name: &str, model: u32, ticks_per_sec: u32, power_freq: u32) -> CiaCore {
    let mut c = CiaCore::new(name);
    c.set_timing(ticks_per_sec, power_freq);
    // x64sc: "if (machine_class == VICE_MACHINE_C64SC ...) cia->write_offset = 0".
    c.write_offset = 0;
    c.set_model(model);
    c
}

// =============================================================================
// The keyboard matrix solver (c64cia1.c:166-277)
// =============================================================================

/// `keyarr` / `rev_keyarr` at one instant.
struct Matrix {
    keyarr: [u8; 8],
    rev_keyarr: [u8; 8],
}

impl Matrix {
    fn at(kb: &KeyboardMatrix, now: u64) -> Self {
        let (keyarr, rev_keyarr) = kb.keyarr_at(now);
        Matrix { keyarr, rev_keyarr }
    }

    fn activate_row(&self, row: usize, rows: &mut u8, cols: &mut u8) {
        if (1u8 << row) & !*rows != 0 {
            *rows |= 1 << row;
            let msk = self.keyarr[row];
            for i in 0..8 {
                // activate each column connected to the given row
                if (msk & (1 << i)) & !*cols != 0 {
                    self.activate_column(i, rows, cols);
                }
            }
        }
    }

    fn activate_column(&self, column: usize, rows: &mut u8, cols: &mut u8) {
        if (1u8 << column) & !*cols != 0 {
            *cols |= 1 << column;
            let msk = self.rev_keyarr[column];
            for i in 0..8 {
                // activate each row connected to the given column
                if (msk & (1 << i)) & !*rows != 0 {
                    self.activate_row(i, rows, cols);
                }
            }
        }
    }

    fn active_rows_by_column(&self, column: usize) -> u8 {
        let (mut r, mut c) = (0, 0);
        self.activate_column(column, &mut r, &mut c);
        r
    }
    fn active_rows_by_row(&self, row: usize) -> u8 {
        let (mut r, mut c) = (0, 0);
        self.activate_row(row, &mut r, &mut c);
        r
    }
    fn active_columns_by_column(&self, column: usize) -> u8 {
        let (mut r, mut c) = (0, 0);
        self.activate_column(column, &mut r, &mut c);
        c
    }
    fn active_columns_by_row(&self, row: usize) -> u8 {
        let (mut r, mut c) = (0, 0);
        self.activate_row(row, &mut r, &mut c);
        c
    }
}

/// `c64keyboard_active` — the C64 rows TRX64 runs all have a keyboard.
const C64KEYBOARD_ACTIVE: bool = true;

/// c64cia1.c:286-339 `read_ciapa`.
pub fn cia1_read_pa(p: &CiaPins, kb: &KeyboardMatrix, now: u64, joy1: &JoystickState, joy2: &JoystickState) -> u8 {
    let mx = Matrix::at(kb, now);
    let joy1 = joystick_active_low_mask(joy1);
    let joy2 = joystick_active_low_mask(joy2);
    let mut val: u8 = 0xff;
    // loop over columns, pull down all bits connected to a column which is output and
    // active.
    let msk = p.old_pb & joy1;
    if C64KEYBOARD_ACTIVE {
        for i in 0..8 {
            if msk & (1 << i) == 0 {
                let tmp = mx.active_columns_by_column(i);
                // when scanning from port B to port A with inactive bits set to 1 in port
                // B, ghostkeys will be eliminated (pulled high) if the matrix is connected
                // to more 1 bits of port B. this does NOT happen when the respective bits
                // are set to input. (see testprogs/CIA/ciaports)
                if tmp & p.c_cia[CIA_PRB] & p.c_cia[CIA_DDRB] != 0 {
                    val &= !mx.rev_keyarr[i];
                } else {
                    val &= !mx.active_rows_by_column(i);
                }
            }
        }
    }
    // loop over rows, pull down all bits connected to a row which is output and active.
    // handles the case when port a is used for both input and output
    let msk = p.old_pa & joy2;
    if C64KEYBOARD_ACTIVE {
        for i in 0..8 {
            if msk & (1 << i) == 0 {
                val &= !mx.active_rows_by_row(i);
            }
        }
    }
    (val & (p.c_cia[CIA_PRA] | !p.c_cia[CIA_DDRA])) & joy2
}

/// c64cia1.c:341-358 `ciapb_forcelow`.
fn ciapb_forcelow(mx: &Matrix, row: usize, mask: u8) -> bool {
    if C64KEYBOARD_ACTIVE {
        // Check for shift lock — `keyboard_get_shiftlock()`, which has no source here.
        let shiftlock = false;
        if row == 1 && shiftlock {
            return true;
        }
        // Check if two or more rows are connected
        let v = mx.active_rows_by_row(row) & mask;
        if v & v.wrapping_sub(1) != 0 {
            return true;
        }
    }
    false
}

/// c64cia1.c:360-428 `read_ciapb`.
pub fn cia1_read_pb(p: &CiaPins, kb: &KeyboardMatrix, now: u64, joy1: &JoystickState, joy2: &JoystickState) -> u8 {
    let mx = Matrix::at(kb, now);
    let joy1 = joystick_active_low_mask(joy1);
    let joy2 = joystick_active_low_mask(joy2);
    let mut val: u8 = 0xff;
    // loop over rows, pull down all bits connected to a row which is output and active.
    let mut val_outhi = p.c_cia[CIA_DDRB] & p.c_cia[CIA_PRB];
    let msk = p.old_pa & joy2;
    if C64KEYBOARD_ACTIVE {
        for i in 0..8 {
            let m = 1u8 << i;
            if msk & m == 0 {
                let tmp = mx.active_columns_by_row(i);
                val &= !tmp;
                // Both ports output, port A (active) low and port B high: one port A 0 bit
                // via shift-lock, or two or more port A 0 bits on keys of the same column,
                // are needed to drive port B low (see testprogs/CIA/ciaports).
                if (p.c_cia[CIA_DDRA] & !p.c_cia[CIA_PRA] & m) != 0
                    && (p.c_cia[CIA_DDRB] & p.c_cia[CIA_PRB] & tmp) != 0
                    && ciapb_forcelow(&mx, i, p.c_cia[CIA_DDRA] & !p.c_cia[CIA_PRA])
                {
                    val_outhi &= !tmp;
                }
            }
        }
    }
    // loop over columns, pull down all bits connected to a column which is output and
    // active. handles the case when port b is used for both input and output
    let msk = p.old_pb & joy1;
    if C64KEYBOARD_ACTIVE {
        for i in 0..8 {
            if msk & (1 << i) == 0 {
                val &= !mx.active_columns_by_column(i);
            }
        }
    }
    let mut byte = val & (p.c_cia[CIA_PRB] | !p.c_cia[CIA_DDRB]);
    byte |= val_outhi;
    byte & joy1
}

/// CIA 1's board side for one access.
pub struct Cia1Ports<'a> {
    pub kb: &'a KeyboardMatrix,
    /// The keyboard's clock for this access (the CPU clock).
    pub now: u64,
    pub joy1: JoystickState,
    pub joy2: JoystickState,
    /// The POT lines (Spec 876): `set_joyport_pot_mask` moves with port A. `None` for a
    /// side-effect-free peek, which stores nothing.
    pub pot: Option<&'a mut crate::pot::PotLines>,
}

impl CiaBackend for Cia1Ports<'_> {
    /// c64cia1.c:142-147 `store_ciapa` — the light-pen check, `set_joyport_pot_mask((b >>
    /// 6) & 3)` and `store_joyport_dig(JOYPORT_2, b)`. The POT latch is settled under the
    /// selection that stood until this write (pot.rs).
    fn store_pa(&mut self, clk: u64, _byte: u8, pins: &CiaPins) {
        if let Some(pot) = self.pot.as_deref_mut() {
            pot.settle(clk, crate::pot::select(pins.old_pa));
        }
    }
    fn read_pa(&mut self, pins: &CiaPins) -> u8 {
        cia1_read_pa(pins, self.kb, self.now, &self.joy1, &self.joy2)
    }
    fn read_pb(&mut self, pins: &CiaPins) -> u8 {
        cia1_read_pb(pins, self.kb, self.now, &self.joy1, &self.joy2)
    }
}

/// CIA 2's board side for one access (module doc: the serial bus is the full bus's).
#[derive(Default)]
pub struct Cia2Ports {
    /// `iecbus_callback_read(maincpu_clk)` — bits 6/7 as the bus sampled them for this
    /// access; only a port-A read uses it.
    pub iec_pins: u8,
    /// A `store_ciapa` this access made: the new composed output, for the bus to drive.
    pub pa_store: Option<u8>,
    /// An `undump_ciapa` a snapshot restore made: the composed output to put back on the
    /// VIC bank and the serial bus (c64cia2.c:172-182).
    pub pa_undump: Option<u8>,
}

impl CiaBackend for Cia2Ports {
    /// c64cia2.c:135-169 `store_ciapa` — the user port PA2/PA3 (nothing attached), the
    /// VIC bank and the serial bus, both of which the bus applies from `pa_store`.
    fn store_pa(&mut self, _clk: u64, byte: u8, pins: &CiaPins) {
        if pins.old_pa != byte {
            self.pa_store = Some(byte);
        }
    }
    /// c64cia2.c:172-182 `undump_ciapa` — user port, VIC bank, `iecbus_cpu_undump`: the
    /// restoring caller applies the byte.
    fn undump_pa(&mut self, _rclk: u64, byte: u8, _pins: &CiaPins) {
        self.pa_undump = Some(byte);
    }
    /// c64cia2.c:194-225 `read_ciapa`.
    fn read_pa(&mut self, pins: &CiaPins) -> u8 {
        let mut value = (pins.c_cia[CIA_PRA] | !pins.c_cia[CIA_DDRA]) & 0x3f;
        value |= self.iec_pins;
        if pins.c_cia[CIA_DDRA] & 4 == 0 {
            let userval: u8 = 1; // read_userport_pa2(1): nothing attached
            if value != userval {
                value &= if userval & 1 != 0 { 0xff } else { 0xfb };
            }
        }
        if pins.c_cia[CIA_DDRA] & 8 == 0 {
            let userval: u8 = 1; // read_userport_pa3(1): nothing attached
            if value != userval {
                value &= if userval & 1 != 0 { 0xff } else { 0xf7 };
            }
        }
        value
    }
    /// c64cia2.c:228-236 `read_ciapb`.
    fn read_pb(&mut self, pins: &CiaPins) -> u8 {
        let byte: u8 = 0xff; // read_userport_pbx(0xff): nothing attached
        (byte & !pins.c_cia[CIA_DDRB]) | (pins.c_cia[CIA_PRB] & pins.c_cia[CIA_DDRB])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pins(pra: u8, ddra: u8, prb: u8, ddrb: u8) -> CiaPins {
        let mut c = [0u8; 16];
        c[CIA_PRA] = pra;
        c[CIA_DDRA] = ddra;
        c[CIA_PRB] = prb;
        c[CIA_DDRB] = ddrb;
        CiaPins { c_cia: c, old_pa: pra | !ddra, old_pb: prb | !ddrb }
    }

    /// The KERNAL scan: PA output drives one line low, PB reads the key.
    #[test]
    fn a_key_reads_on_port_b() {
        let mut kb = KeyboardMatrix::new();
        kb.key_down("SPACE"); // PA7 / PB4
        let j = JoystickState::default();
        assert_eq!(cia1_read_pb(&pins(0x7f, 0xff, 0xff, 0x00), &kb, 0, &j, &j), 0xef);
        assert_eq!(cia1_read_pb(&pins(0xbf, 0xff, 0xff, 0x00), &kb, 0, &j, &j), 0xff);
    }

    /// And backwards: PB drives, PA reads.
    #[test]
    fn a_key_reads_on_port_a() {
        let mut kb = KeyboardMatrix::new();
        kb.key_down("SPACE");
        let j = JoystickState::default();
        assert_eq!(cia1_read_pa(&pins(0xff, 0x00, 0xef, 0xff), &kb, 0, &j, &j), 0x7f);
    }

    /// Ghost keys: SPACE (PA7/PB4), Z (PA1/PB4) and A (PA1/PB2) connect PA7 to PB2 through
    /// the matrix, so driving PA7 alone reads PB2 low too — VICE's matrix solver.
    #[test]
    fn three_keys_ghost_a_fourth() {
        let mut kb = KeyboardMatrix::new();
        kb.key_down("SPACE");
        kb.key_down("Z");
        kb.key_down("A");
        let j = JoystickState::default();
        assert_eq!(cia1_read_pb(&pins(0x7f, 0xff, 0xff, 0x00), &kb, 0, &j, &j), 0xeb);
    }
}
