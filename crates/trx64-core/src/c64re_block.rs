//! c64re_block.rs — the BLOCK section of a format 2 `.c64re` (Spec 919 D8).
//!
//! 58 words of the board's state bus. The bit positions are the board's
//! (`trx_c64re_nodes.cc`, read off the gateware); this file maps a TRX64 checkpoint into them
//! and back. Going in (a TRX64 dump) every field the firmware's own `trx_c64re_from_nodes`
//! fills is filled, and every word the emulator has no source for is zero — [`ZEROED`] names
//! them. Going out (a board file) is what the firmware's `write_checkpoint` does: the node
//! values the board's words hold, nothing invented.

use serde_json::{json, Value};

use crate::ciacore::CiaCore;
use crate::c64re_snapshot::{restore_cia, CiaSnapshot};

pub const BLOCK_WORDS: usize = 58;
/// `u32 w[58]`, `u16 x`, `u16 flags`, `u32 turbo pack`.
pub const BLOCK_BYTES: usize = BLOCK_WORDS * 4 + 8;

const W_GLUE: usize = 0;
const W_CPU: usize = 1;
const W_VIC: usize = 4;
const W_CIA1: usize = 16;
const W_CIA2: usize = 21;
const W_CART: usize = 26;
const W_CPUH: usize = 27;
const W_CIA1H: usize = 34;
const W_CIA2H: usize = 46;
const GLUE_CART: u32 = 0x08;

// chip 1 words 8-14
const CH_A: usize = 0;
const CH_X: usize = 8;
const CH_Y: usize = 16;
const CH_S: usize = 24;
const CH_P: usize = 32;
const CH_PCL: usize = 40;
const CH_PCH: usize = 48;
const CH_PDIR: usize = 144;
const CH_POUT: usize = 152;
// chips 3 and 4 words 8-19
const CI_DDRB: usize = 8;
const CI_DDRA: usize = 16;
const CI_PRB: usize = 24;
const CI_PRA: usize = 32;
const CI_TA_LATCH: usize = 43;
const CI_TA_CNT: usize = 59;
const CI_TB_LATCH: usize = 83;
const CI_TB_CNT: usize = 99;
const CI_ALARM: usize = 123;
const CI_CLOCK: usize = 155;
const CI_SDR: usize = 244;
const CI_ICR: usize = 293;
const CI_FLAGS: usize = 303;
const CI_MASK: usize = 311;
const CI_CRA: usize = 317;
const CI_CRB: usize = 327;

/// What a TRX64 dump leaves zero in the BLOCK because the emulator has no source for it.
pub const ZEROED: &[&str] = &[
    "glue word 0 except the cartridge bit",
    "cpu sequencer (state, bus muxes, dreg, ireg, din, the interrupt-sampling flops)",
    "cpu pending nmi/irq flops",
    "vic registers $2F",
    "cia edge detectors, timer prescalers, serial shifter, interrupt-delay flops, tod counter",
    "cia2 and cia1 register image word 4",
];

pub struct Block {
    pub w: [u32; BLOCK_WORDS],
    pub x: u16,
    /// The board's turbo word (`flags` bit 0), packed the way the firmware packs it.
    pub turbo: Option<u32>,
}

impl Block {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(BLOCK_BYTES);
        for w in &self.w {
            out.extend_from_slice(&w.to_le_bytes());
        }
        out.extend_from_slice(&self.x.to_le_bytes());
        out.extend_from_slice(&(self.turbo.is_some() as u16).to_le_bytes());
        out.extend_from_slice(&self.turbo.unwrap_or(0).to_le_bytes());
        out
    }

    pub fn from_bytes(b: &[u8]) -> Result<Block, String> {
        if b.len() != BLOCK_BYTES {
            return Err(format!("the BLOCK section is {} bytes, not {BLOCK_BYTES}", b.len()));
        }
        let mut w = [0u32; BLOCK_WORDS];
        for (i, c) in b[..BLOCK_WORDS * 4].chunks_exact(4).enumerate() {
            w[i] = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
        }
        let o = BLOCK_WORDS * 4;
        let x = u16::from_le_bytes([b[o], b[o + 1]]);
        let flags = u16::from_le_bytes([b[o + 2], b[o + 3]]);
        let pack = u32::from_le_bytes([b[o + 4], b[o + 5], b[o + 6], b[o + 7]]);
        Ok(Block { w, x, turbo: (flags & 1 != 0).then_some(pack) })
    }
}

fn bits(w: &[u32], lo: usize, n: usize) -> u32 {
    let mut v = 0u32;
    for b in 0..n {
        v |= ((w[(lo + b) / 32] >> ((lo + b) % 32)) & 1) << b;
    }
    v
}

fn set_bits(w: &mut [u32], lo: usize, n: usize, v: u32) {
    for b in 0..n {
        let (i, s) = ((lo + b) / 32, (lo + b) % 32);
        w[i] = (w[i] & !(1 << s)) | (((v >> b) & 1) << s);
    }
}

fn wbyte(w: &[u32], word0: usize, reg: usize) -> u8 {
    (w[word0 + reg / 4] >> (8 * (reg & 3))) as u8
}

fn set_byte(w: &mut [u32], word0: usize, reg: usize, v: u8) {
    let (i, s) = (word0 + reg / 4, 8 * (reg & 3));
    w[i] = (w[i] & !(0xFF << s)) | ((v as u32) << s);
}

fn num(v: &Value, k: &str) -> Option<i64> {
    v.get(k).and_then(|x| x.as_i64())
}

fn cia_regs(cp: &Value, name: &str) -> Option<CiaSnapshot> {
    serde_json::from_value(cp.get(name)?.clone()).ok()
}

/// One CIA of the checkpoint into its register image and hidden words, with the timers and the
/// interrupt flags as the CPU would read them at `cycles`.
fn cia_in(w: &mut [u32], wr: usize, wh: usize, s: &CiaSnapshot, cycles: u64) {
    let mut chip = CiaCore::new("cia");
    restore_cia(&mut chip, s, "cia");
    chip.clk = cycles;
    let c = |i: usize| s.c_cia.get(i).copied().unwrap_or(0) as u8;
    let ta_l = (s.ta_latch as u32) & 0xFFFF;
    let tb_l = (s.tb_latch as u32) & 0xFFFF;
    let ta_c = chip.peek(4) as u32 | (chip.peek(5) as u32) << 8;
    let tb_c = chip.peek(6) as u32 | (chip.peek(7) as u32) << 8;
    let ifr = chip.peek(13) as u32;
    let alarm = |i: usize| s.todalarm.get(i).copied().unwrap_or(0) as u32;
    let h = &mut w[wh..];
    set_bits(h, CI_DDRB, 8, c(3) as u32);
    set_bits(h, CI_DDRA, 8, c(2) as u32);
    set_bits(h, CI_PRB, 8, c(1) as u32);
    set_bits(h, CI_PRA, 8, c(0) as u32);
    set_bits(h, CI_TA_LATCH, 16, ta_l);
    set_bits(h, CI_TB_LATCH, 16, tb_l);
    set_bits(h, CI_TA_CNT, 16, ta_c);
    set_bits(h, CI_TB_CNT, 16, tb_c);
    set_bits(h, CI_CRA, 8, c(14) as u32);
    set_bits(h, CI_CRB, 8, c(15) as u32);
    set_bits(h, CI_SDR, 8, c(12) as u32);
    set_bits(h, CI_FLAGS, 5, ifr & 0x1F);
    set_bits(h, CI_ICR, 8, ifr);
    set_bits(h, CI_MASK, 5, (c(13) & 0x1F) as u32);
    set_bits(h, CI_ALARM, 8, alarm(3)); // hour
    set_bits(h, CI_ALARM + 8, 8, alarm(2)); // minute
    set_bits(h, CI_ALARM + 16, 8, alarm(1)); // second
    set_bits(h, CI_ALARM + 24, 8, alarm(0)); // tenth
    set_bits(h, CI_CLOCK, 8, c(11) as u32);
    set_bits(h, CI_CLOCK + 8, 8, c(10) as u32);
    set_bits(h, CI_CLOCK + 16, 8, c(9) as u32);
    set_bits(h, CI_CLOCK + 24, 8, c(8) as u32);
    for r in 0..16usize {
        set_byte(w, wr, r, c(r));
    }
    set_byte(w, wr, 4, ta_c as u8);
    set_byte(w, wr, 5, (ta_c >> 8) as u8);
    set_byte(w, wr, 6, tb_c as u8);
    set_byte(w, wr, 7, (tb_c >> 8) as u8);
    set_byte(w, wr, 13, ifr as u8);
}

/// The turbo word the board packs (`trx_ring_set_turbo`): `hold >> 8` above bit 17, `$D031`,
/// `$D030` bit 0, the enable word, the menu stalls and the menu index.
fn turbo_pack(t: &Value) -> Option<u32> {
    let en = (num(t, "enable")? as u32) & 7;
    let mi = (num(t, "menuIndex")? as u32) & 15;
    let stalls = t.get("menuStalls").and_then(|v| v.as_bool()).unwrap_or(true) as u32;
    let d031 = (num(t, "d031")? as u32) & 0xFF;
    let d030 = (num(t, "d030")? as u32) & 1;
    let hold = (num(t, "resetHold").unwrap_or(0) as u32) & 0x7F_FFFF;
    Some(((hold >> 8) << 17) | (d031 << 9) | (d030 << 8) | (en << 5) | (stalls << 4) | mi)
}

/// The checkpoint's CPU, CIAs, VIC registers, cartridge registers and turbo state as the
/// board's BLOCK. `cart` is the cartridge's `$DE00` / `$DE02` when a cartridge of the board's
/// table is on.
pub fn block_from_checkpoint(cp: &Value, cart: Option<(u8, u8)>) -> Result<Block, String> {
    let cpu = cp.get("cpu").ok_or("the checkpoint has no cpu node")?;
    let pc = num(cpu, "pc").ok_or("the cpu node has no pc")? as u32 & 0xFFFF;
    let a = num(cpu, "a").unwrap_or(0) as u32 & 0xFF;
    let xr = num(cpu, "x").unwrap_or(0) as u32 & 0xFF;
    let yr = num(cpu, "y").unwrap_or(0) as u32 & 0xFF;
    let sp = num(cpu, "sp").unwrap_or(0xFF) as u32 & 0xFF;
    let p = num(cpu, "flags").unwrap_or(0x20) as u32 & 0xFF;
    let cycles = num(cpu, "cycles").unwrap_or(0).max(0) as u64;
    let mut w = [0u32; BLOCK_WORDS];
    {
        let h = &mut w[W_CPUH..];
        set_bits(h, CH_A, 8, a);
        set_bits(h, CH_X, 8, xr);
        set_bits(h, CH_Y, 8, yr);
        set_bits(h, CH_S, 8, sp);
        set_bits(h, CH_P, 8, p);
        set_bits(h, CH_PCL, 8, pc & 0xFF);
        set_bits(h, CH_PCH, 8, pc >> 8);
    }
    w[W_CPU] = a | (xr << 8) | (yr << 16) | (sp << 24);
    w[W_CPU + 1] = pc | (p << 16);
    if let (Some(dir), Some(val)) = (num(cp, "cpuPortDirection"), num(cp, "cpuPortValue")) {
        let (dir, val) = (dir as u32 & 0xFF, val as u32 & 0xFF);
        set_bits(&mut w[W_CPUH..], CH_PDIR, 8, dir);
        set_bits(&mut w[W_CPUH..], CH_POUT, 8, val);
        // $01 as read: the output bits as driven, the input bits as the C64 pulls them
        let rd = (val & dir) | (0x17 & !dir & 0xFF);
        w[W_CPU + 2] = dir | (val << 8) | (rd << 16);
    }
    if let Some(s) = cia_regs(cp, "cia1") {
        cia_in(&mut w, W_CIA1, W_CIA1H, &s, cycles);
    }
    if let Some(s) = cia_regs(cp, "cia2") {
        cia_in(&mut w, W_CIA2, W_CIA2H, &s, cycles);
    }
    let vic = cp.get("vic").ok_or("the checkpoint has no vic node")?;
    let regs: Vec<u8> = vic
        .get("regs")
        .and_then(|r| r.as_array())
        .map(|a| a.iter().map(|x| x.as_u64().unwrap_or(0) as u8).collect())
        .unwrap_or_default();
    for r in 0..=0x2Eusize {
        set_byte(&mut w, W_VIC, r, regs.get(r).copied().unwrap_or(0));
    }
    let irq = num(vic, "irq_status").unwrap_or(0) as u32 & 0x0F;
    set_byte(&mut w, W_VIC, 0x19, (irq | if irq != 0 { 0x80 } else { 0 }) as u8);
    set_byte(&mut w, W_VIC, 0x1E, num(vic, "sprite_sprite_collisions").unwrap_or(0) as u8);
    set_byte(&mut w, W_VIC, 0x1F, num(vic, "sprite_background_collisions").unwrap_or(0) as u8);
    if let Some((r0, r1)) = cart {
        w[W_CART] = r0 as u32 | (r1 as u32) << 8;
        w[W_GLUE] |= GLUE_CART;
    }
    let cycle = num(vic, "raster_cycle").unwrap_or(0).max(0) as u32;
    Ok(Block {
        w,
        x: (cycle * 8).clamp(96, 504) as u16,
        turbo: cp.get("turbo").filter(|t| !t.is_null()).and_then(turbo_pack),
    })
}

/// The board's cartridge-on bit of word 0 and its banking registers.
pub fn board_cart_regs(b: &Block) -> Option<(u8, u8)> {
    (b.w[W_GLUE] & GLUE_CART != 0).then(|| (b.w[W_CART] as u8, (b.w[W_CART] >> 8) as u8))
}

// ── the board's BLOCK as nodes ──────────────────────────────────────────────────────────────

/// VICE's timer state word for a control register, as the firmware writes it.
fn timer_state(cr: u8) -> i64 {
    if cr & 1 == 0 {
        return if cr & 8 != 0 { 0x008 } else { 0 };
    }
    if cr & 0x60 != 0 {
        return 0x001 | (cr & 8) as i64; // counts CNT or timer A: no phi2 input
    }
    0x863 | if cr & 8 != 0 { 0x1108 } else { 0 }
}

/// One CIA node (`CiaSnapshot` v2 — the pre-ciacore record the restore converts, with a note).
fn cia_node(w: &[u32], wr: usize, wh: usize, cycles: u64) -> Value {
    let h = &w[wh..];
    let rd: Vec<u8> = (0..16).map(|i| wbyte(w, wr, i)).collect();
    let ta = bits(h, CI_TA_LATCH, 16);
    let tb = bits(h, CI_TB_LATCH, 16);
    let mut c: Vec<i64> = rd.iter().map(|&b| b as i64).collect();
    c[0] = bits(h, CI_PRA, 8) as i64;
    c[1] = bits(h, CI_PRB, 8) as i64;
    c[2] = bits(h, CI_DDRA, 8) as i64;
    c[3] = bits(h, CI_DDRB, 8) as i64;
    c[4] = (ta & 0xFF) as i64;
    c[5] = (ta >> 8) as i64;
    c[6] = (tb & 0xFF) as i64;
    c[7] = (tb >> 8) as i64;
    c[13] = bits(h, CI_MASK, 5) as i64;
    let alarm = [bits(h, CI_ALARM + 24, 8), bits(h, CI_ALARM + 16, 8), bits(h, CI_ALARM + 8, 8), bits(h, CI_ALARM, 8)];
    let mut m = serde_json::Map::new();
    let mut put = |k: &str, v: Value| {
        m.insert(k.to_string(), v);
    };
    put("v", json!(2));
    put("c_cia", json!(c));
    put("irqflags", json!(rd[13] & 0x1F));
    for k in ["ack_irqflags", "new_irqflags", "rdi", "ifr_clock", "ifr_delay", "tat", "tbt"] {
        put(k, json!(0));
    }
    put("irq_enabled", json!(((rd[13] as i64 & c[13] & 0x1F) != 0) as i64));
    put("ta_state", json!(timer_state(rd[14])));
    put("ta_latch", json!(ta));
    put("ta_cnt", json!(rd[4] as u32 | (rd[5] as u32) << 8));
    put("ta_clk", json!(cycles));
    put("tb_state", json!(timer_state(rd[15])));
    put("tb_latch", json!(tb));
    put("tb_cnt", json!(rd[6] as u32 | (rd[7] as u32) << 8));
    put("tb_clk", json!(cycles));
    for k in [
        "sr_bits", "sdr_valid", "sdr_force_finish", "shifter", "sdr_delay", "sp_in_state", "cnt_in_state",
        "cnt_out_state", "todlatched", "todstopped", "todticks", "todclk", "todtickcounter", "power_tickcounter",
        "power_ticks", "read_offset", "last_read", "model",
    ] {
        put(k, json!(0));
    }
    put("todalarm", json!(alarm));
    put("todlatch", json!([rd[8], rd[9], rd[10], rd[11]]));
    put("old_pa", json!(255));
    put("old_pb", json!(255));
    put("read_clk", json!(cycles));
    Value::Object(m)
}

/// What the board's BLOCK holds, as checkpoint nodes. `cycles` is the file's cycle count.
pub struct BoardNodes {
    pub cpu: Value,
    pub port: Option<(u8, u8)>,
    pub cia1: Value,
    pub cia2: Value,
    /// `$D000-$D02E` as the block holds them.
    pub vic_regs: Vec<u8>,
    pub vic_irq_status: u8,
    pub sprite_sprite: u8,
    pub sprite_background: u8,
    /// Which interrupt sources were asserting: CIA1 (IRQ), CIA2 (NMI), VIC (IRQ).
    pub irq_cia1: bool,
    pub nmi_cia2: bool,
    pub irq_vic: bool,
}

pub fn nodes_from_block(b: &Block, cycles: u64) -> BoardNodes {
    let w = &b.w;
    let r1 = w[W_CPU + 1];
    let cpu = json!({
        "pc": r1 & 0xFFFF, "a": w[W_CPU] & 0xFF, "x": (w[W_CPU] >> 8) & 0xFF,
        "y": (w[W_CPU] >> 16) & 0xFF, "sp": (w[W_CPU] >> 24) & 0xFF,
        "flags": (r1 >> 16) & 0xFF, "cycles": cycles,
    });
    let r2 = w[W_CPU + 2];
    let port = Some((r2 as u8, (r2 >> 8) as u8));
    let vic_regs: Vec<u8> = (0..=0x2Fusize).map(|r| wbyte(w, W_VIC, r)).collect();
    let irq_vic = wbyte(w, W_VIC, 0x19) & 0x80 != 0;
    BoardNodes {
        cpu,
        port,
        cia1: cia_node(w, W_CIA1, W_CIA1H, cycles),
        cia2: cia_node(w, W_CIA2, W_CIA2H, cycles),
        vic_irq_status: wbyte(w, W_VIC, 0x19) & 0x0F,
        sprite_sprite: wbyte(w, W_VIC, 0x1E),
        sprite_background: wbyte(w, W_VIC, 0x1F),
        irq_cia1: wbyte(w, W_CIA1, 13) & 0x80 != 0,
        nmi_cia2: wbyte(w, W_CIA2, 13) & 0x80 != 0,
        irq_vic,
        vic_regs,
    }
}

/// The board's turbo word unpacked (`flags` bit 0 set): menu index, stalls, enable, `$D030`
/// bit, `$D031`, and the reset hold with its low byte lost.
pub fn turbo_node(pack: u32) -> Value {
    json!({
        "enable": (pack >> 5) & 7, "menuIndex": pack & 15, "menuStalls": (pack >> 4) & 1 == 1,
        "d031": (pack >> 9) & 0xFF, "d030": (pack >> 8) & 1, "resetHold": (pack >> 17) << 8,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_and_bytes_cross_words() {
        let mut w = [0u32; 4];
        set_bits(&mut w, 28, 8, 0xA5);
        assert_eq!(bits(&w, 28, 8), 0xA5);
        assert_eq!(w[0] >> 28, 0x5);
        assert_eq!(w[1] & 0xF, 0xA);
        set_byte(&mut w, 2, 1, 0x7E);
        assert_eq!(wbyte(&w, 2, 1), 0x7E);
    }

    #[test]
    fn the_block_bytes_round_trip() {
        let mut b = Block { w: [0; BLOCK_WORDS], x: 119, turbo: Some(0x10) };
        b.w[3] = 0x35352f;
        let r = Block::from_bytes(&b.to_bytes()).unwrap();
        assert_eq!((r.w[3], r.x, r.turbo), (0x35352f, 119, Some(0x10)));
        assert!(Block::from_bytes(&[0u8; 8]).is_err());
    }
}
