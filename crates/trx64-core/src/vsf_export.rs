//! vsf_export.rs — `.c64re` machine → **VICE-x64sc-loadable** `.vsf` (Spec 791.5,
//! the "Rest und retour" return trip). The existing `vsf::save_vsf` emits the
//! *c64re-own* compact framing, which VICE cannot load; this emits each module in
//! VICE's EXACT `*_snapshot_read_module` field order + size so a real `x64sc`
//! binary loads and resumes it.
//!
//! Modelled fields (regs, RAM, colour RAM, VIC state, CIA/SID regs, cart lines)
//! come from our machine — which is a viciisc-faithful port, so they map 1:1.
//! VICE-internal sub-structures we do NOT model (the interrupt controller, the
//! `ciat` alarm blob, the VIC `draw_cycle` pipeline + the ~121 KB `raster_snapshot`
//! draw-buffer) are emitted **zeroed at their exact byte size** — VICE re-derives
//! them as it runs (a one-frame redraw). Module byte sizes are the ones a real
//! x64sc VSF carries: MAINCPU 103, C64MEM 65555, CIA 77, SID 36, VIC-IISC 123415.

use crate::Machine;

/// Little push-helpers onto a byte buffer (VICE is little-endian; CLOCK = qword).
struct W {
    buf: Vec<u8>,
}
impl W {
    fn new() -> Self {
        W { buf: Vec::new() }
    }
    fn b(&mut self, v: u8) {
        self.buf.push(v);
    }
    fn w(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn dw(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn clock(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes()); // SMW_CLOCK = qword
    }
    fn ba(&mut self, v: &[u8]) {
        self.buf.extend_from_slice(v);
    }
    fn zeros(&mut self, n: usize) {
        self.buf.resize(self.buf.len() + n, 0);
    }
    /// Pad with zeros up to an exact data length (fills un-modelled tail sub-structures).
    fn pad_to(&mut self, n: usize) {
        if self.buf.len() < n {
            let need = n - self.buf.len();
            self.zeros(need);
        }
    }
}

const NAME_LEN: usize = 16;

/// Emit a VICE module: name(16, zero-padded) + major + minor + size(dword, TOTAL
/// module bytes incl. this 22-byte header) + data.
fn module(out: &mut Vec<u8>, name: &str, major: u8, minor: u8, data: &[u8]) {
    let mut nm = [0u8; NAME_LEN];
    let nb = name.as_bytes();
    nm[..nb.len().min(NAME_LEN)].copy_from_slice(&nb[..nb.len().min(NAME_LEN)]);
    out.extend_from_slice(&nm);
    out.push(major);
    out.push(minor);
    let total = (22 + data.len()) as u32;
    out.extend_from_slice(&total.to_le_bytes());
    out.extend_from_slice(data);
}

/// The 58-byte VICE file header for an x64sc snapshot (machine "C64SC", snapshot
/// version 2.0, VICE 3.10 stamp — matches what x64sc writes; the version stamp is
/// informational, `snapshot_open` gates on magic + machine name).
fn file_header() -> Vec<u8> {
    let mut h = Vec::with_capacity(58);
    let mut magic = [0u8; 19];
    let ms = b"VICE Snapshot File\x1a";
    magic.copy_from_slice(ms);
    h.extend_from_slice(&magic); // 19
    h.push(2); // major
    h.push(0); // minor
    let mut mach = [0u8; 16];
    mach[..5].copy_from_slice(b"C64SC");
    h.extend_from_slice(&mach); // 16
    let mut vm = [0u8; 13];
    vm[..13].copy_from_slice(b"VICE Version\x1a");
    h.extend_from_slice(&vm); // 13
    h.extend_from_slice(&[3, 10, 0, 0, 0, 0, 0, 0]); // viceversion(4)+svn(4)
    debug_assert_eq!(h.len(), 58);
    h
}

fn maincpu(m: &Machine) -> Vec<u8> {
    let mut w = W::new();
    w.clock(m.c64_core.clk); // maincpu_clk (8)
    w.b(m.c64_core.reg_a);
    w.b(m.c64_core.reg_x);
    w.b(m.c64_core.reg_y);
    w.b(m.c64_core.reg_sp);
    w.w(m.c64_core.reg_pc);
    w.b(m.c64_core.status());
    w.dw(0); // last_opcode_info
    w.dw(0); // ane_log_level
    w.dw(0); // lxa_log_level
    // interrupt_write_snapshot(maincpu_int_status) — 76 bytes of interrupt-controller
    // state we don't model; VICE re-derives IRQ/NMI from the chip regs on resume.
    w.pad_to(103);
    w.buf
}

fn c64mem(m: &Machine) -> Vec<u8> {
    let mut w = W::new();
    w.b(m.port_data); // pport.data ($01)
    w.b(m.port_dir); // pport.dir ($00)
    // export.exrom / export.game — cart lines (1 = inactive / no cart pulling low).
    let (exrom, game) = cart_lines(m);
    w.b(exrom);
    w.b(game);
    w.ba(&m.ram[..]); // 64K
    // pport tail (data_out/data_read/dir_read + fall-off timing) — benign defaults.
    w.b(m.port_data); // data_out
    w.b(m.port_data); // data_read
    w.b(m.port_dir); // dir_read
    w.dw(0); // data_set_clk_bit6
    w.dw(0); // data_set_clk_bit7
    w.b(0); // data_set_bit6
    w.b(0); // data_set_bit7
    w.b(0); // data_falloff_bit6
    w.b(0); // data_falloff_bit7
    w.buf
}

fn cart_lines(m: &Machine) -> (u8, u8) {
    // export.exrom/game are stored ACTIVE-something in VICE; with no cart both are
    // released. A cartridge module (follow-up) will drive these; for now derive from
    // whether a cart is present (crude — refined with the C64CART export slice).
    if m.cartridge.is_some() {
        (0, 0)
    } else {
        (1, 1)
    }
}

fn cia(c: &crate::cia::Cia) -> Vec<u8> {
    use crate::cia::*;
    let mut w = W::new();
    w.b(c.regs[CIA_PRA]);
    w.b(c.regs[CIA_PRB]);
    w.b(c.regs[CIA_DDRA]);
    w.b(c.regs[CIA_DDRB]);
    w.w(c.ta.cnt); // ciat_read_timer(ta)
    w.w(c.tb.cnt); // ciat_read_timer(tb)
    w.b(c.regs[CIA_TOD_TEN]);
    w.b(c.regs[CIA_TOD_SEC]);
    w.b(c.regs[CIA_TOD_MIN]);
    w.b(c.regs[CIA_TOD_HR]);
    w.b(c.regs[CIA_SDR]);
    w.b(c.regs[CIA_ICR]); // ICR mask
    w.b(c.regs[CIA_CRA]);
    w.b(c.regs[CIA_CRB]);
    w.w(c.ta.latch); // ciat_read_latch(ta)
    w.w(c.tb.latch); // ciat_read_latch(tb)
    w.b(c.irqflags); // ciacore_peek(ICR) — the latched IRQ flags
    // tat/tbt/underflow composite, sr_bits, todalarm[4], rdi byte, tod flags,
    // todlatch[4], todclk(CLOCK), ciat_save_snapshot(ta/tb), shifter — un-modelled
    // internal timer/TOD blob; zero-filled to the exact x64sc CIA size (77).
    w.pad_to(77);
    w.buf
}

fn sid(m: &Machine) -> Vec<u8> {
    let mut w = W::new();
    w.b(1); // num_sids (single SID)
    w.b(1); // sound on
    w.b(0); // engine (fastsid=0; benign — VICE re-inits from model)
    w.b(0); // model (6581/8580 marker; 0 acceptable)
    w.ba(&m.sid_regs[0..32]); // sid_get_siddata(0)
    w.buf
}

fn vic_iisc(m: &Machine) -> Vec<u8> {
    let v = &m.vic;
    let mut w = W::new();
    w.b(0); // model (sanity byte)
    w.ba(&v.regs[0..64]); // 64 registers
    w.dw(v.raster_cycle as u32);
    w.dw(v.cycle_flags);
    w.dw(v.raster_line as u32);
    w.b(v.start_of_frame as u8);
    w.b(v.irq_status);
    w.dw(v.raster_irq_line as u32);
    w.b(v.raster_irq_triggered as u8);
    w.ba(&v.vbuf[0..40]);
    w.ba(&v.cbuf[0..40]);
    w.b(v.gbuf);
    w.dw(v.dbuf_offset as u32);
    // dbuf (520-byte draw line) — emit ours.
    let mut dbuf = [0u8; 520];
    for (i, s) in dbuf.iter_mut().enumerate() {
        *s = v.dbuf.get(i).copied().unwrap_or(0);
    }
    w.ba(&dbuf);
    w.dw(v.ysmooth as u32);
    w.b(v.allow_bad_lines as u8);
    w.b(v.sprite_sprite_collisions);
    w.b(v.sprite_background_collisions);
    w.b(v.clear_collisions);
    w.dw(v.idle_state as u32);
    w.dw(v.vcbase as u32);
    w.dw(v.vc as u32);
    w.dw(v.rc as u32);
    w.dw(v.vmli as u32);
    w.dw(v.bad_line as u32);
    w.b(0); // light_pen.state
    w.b(0); // light_pen.triggered
    w.dw(0); // light_pen.x
    w.dw(0); // light_pen.y
    w.dw(0); // light_pen.x_extra_bits
    w.clock(0); // light_pen.trigger_cycle
    w.b(v.reg11_delay);
    w.dw(v.prefetch_cycles as u32);
    w.dw(v.sprite_display_bits as u32);
    w.b(v.sprite_dma);
    w.b(v.last_color_reg);
    w.b(v.last_color_value);
    w.b(v.last_read_phi1);
    w.b(v.last_bus_phi2);
    w.b(v.vborder as u8);
    w.b(v.set_vborder as u8);
    w.b(v.main_border as u8);
    w.b(v.refresh_counter);
    // colour RAM (0x400, low nibble) from io_shadow[$0800..].
    let mut cr = [0u8; 0x400];
    for (i, c) in cr.iter_mut().enumerate() {
        *c = m.io_shadow[0x0800 + i] & 0x0f;
    }
    w.ba(&cr);
    // sprites 8 × (data DW, mc B, mcbase B, pointer B, exp_flop B, x DW).
    for i in 0..crate::vic::NUM_SPRITES {
        let sp = &v.sprite[i];
        w.dw(sp.data);
        w.b(sp.mc);
        w.b(sp.mcbase);
        w.b(sp.pointer);
        w.b(sp.exp_flop);
        w.dw(sp.x as u32);
    }
    // draw_cycle pipeline (174 bytes) — mid-cycle render pipeline, VICE re-derives.
    w.zeros(174);
    // raster_snapshot: current_line + draw_buffer geometry + the zeroed draw-buffer.
    w.dw(v.raster_line as u32); // current_line
    w.dw(384); // draw_buffer_width
    w.dw(312); // draw_buffer_height
    w.dw(384); // draw_buffer_pitch
    w.zeros(121344); // draw_buffer_padded_allocations[0] (VICE re-derives the picture)
    w.buf
}

/// Serialize `machine` into a VICE-x64sc `.vsf` byte image (Spec 791.5).
/// The cart module is a follow-up slice — a cart-carrying state exports its core
/// (VICE resumes cart-less until then).
pub fn save_vice_vsf(m: &Machine) -> Vec<u8> {
    let mut out = file_header();
    module(&mut out, "MAINCPU", 1, 3, &maincpu(m));
    module(&mut out, "C64MEM", 0, 1, &c64mem(m));
    module(&mut out, "CIA1", 2, 5, &cia(&m.cia1));
    module(&mut out, "CIA2", 2, 5, &cia(&m.cia2));
    module(&mut out, "SID", 1, 5, &sid(m));
    module(&mut out, "VIC-II", 1, 4, &vic_iisc(m));
    out
}
