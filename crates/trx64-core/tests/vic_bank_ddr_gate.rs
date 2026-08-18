//! vic_bank_ddr_gate.rs — the VIC bank comes from the PORT, not from the latch.
//!
//! `core/ciacore.c:810`: the byte the CIA puts on port A is `PRA | ~DDRA`. A pin
//! configured as an INPUT floats high on its pull-up and contributes 1 — whatever
//! the latch holds is irrelevant. `c64/c64cia2.c:150-151` then takes `~byte & 3`
//! as the VIC bank.
//!
//! We masked with `PRA & DDRA` instead, so an input bank bit read 0 and the VIC
//! landed three banks away. The KERNAL leaves `DDRA = $3F` — both bank bits are
//! outputs — so every normal program agrees under either formula, which is why
//! the seven-game gate stayed green for months. A loader that drives $DD00 itself
//! does not: Spindle writes `DDRA = $3C`, leaving VA14/VA15 as inputs, and every
//! VIC fetch of a 2026 demo built on it went to $C000 instead of $0000. The code
//! ran, the modes were right, and every pixel was garbage. See C64RE BUG-051.

use trx64_core::{Machine, NullSink};

/// Write DDRA then PRA straight into CIA2 and report the derived bank base.
fn bank_after(pra: u8, ddra: u8) -> u16 {
    let mut m = Machine::new();
    m.write_full(0xdd02, ddra);
    m.write_full(0xdd00, pra);
    let mut o = NullSink;
    m.run_for_vic(200, &mut o);
    m.vic_bank_base()
}

#[test]
fn both_bank_bits_driven_low_selects_bank_three() {
    // DDRA = $3F: both bank bits are outputs. PRA bits 0-1 = 00 -> bank 3.
    assert_eq!(bank_after(0x00, 0x3f), 0xC000);
}

#[test]
fn both_bank_bits_driven_high_selects_bank_zero() {
    assert_eq!(bank_after(0x03, 0x3f), 0x0000);
}

#[test]
fn one_driven_bank_bit_selects_the_middle_banks() {
    assert_eq!(bank_after(0x01, 0x3f), 0x8000); // %01 -> bank 2
    assert_eq!(bank_after(0x02, 0x3f), 0x4000); // %10 -> bank 1
}

#[test]
fn bank_bits_left_as_inputs_float_high_and_select_bank_zero() {
    // Spindle's configuration: DDRA = $3C leaves VA14/VA15 as inputs. Both pins
    // float high, so the port reads %11 and the bank is 0 — no matter what the
    // latch under them says. Masking with the DDR would answer $C000 for all
    // three of these.
    assert_eq!(bank_after(0x00, 0x3c), 0x0000);
    assert_eq!(bank_after(0xff, 0x3c), 0x0000);
    assert_eq!(bank_after(0x0a, 0x3c), 0x0000);
}

#[test]
fn one_bit_input_one_bit_driven_mixes_pin_and_latch() {
    // DDRA = $3D: VA14 (bit 0) is an output, VA15 (bit 1) an input.
    // PRA bit 0 = 0 -> pin 0 low, pin 1 floats high -> %10 -> bank 1.
    assert_eq!(bank_after(0x00, 0x3d), 0x4000);
    // PRA bit 0 = 1 -> %11 -> bank 0.
    assert_eq!(bank_after(0x01, 0x3d), 0x0000);
}
