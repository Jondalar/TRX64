#!/usr/bin/env python3
"""Second round (T29 Q12/Q13 + leftovers): host-built 6502 routines at $4000,
run by turbomeas.prg command 5 between two CIA2 tick reads."""
import time
import tm

BASE = 0x4000


class Asm:
    def __init__(self, org=BASE):
        self.org, self.b, self.labels = org, bytearray(), {}

    @property
    def pc(self):
        return self.org + len(self.b)

    def emit(self, *x):
        self.b += bytes(x)

    def mark(self, name):
        self.labels[name] = self.pc

    def jmp(self, addr):
        self.emit(0x4C, addr & 0xFF, addr >> 8)

    def abs_(self, op, addr):
        self.emit(op, addr & 0xFF, addr >> 8)


LDA_ABS, STA_ABS, NOP = 0xAD, 0x8D, 0xEA


def io_loop(op, addr, val=0, per_block=64, filler=0, rep=8):
    """rep x 256 x per_block accesses (op addr), each followed by `filler` NOPs."""
    a = Asm()
    a.emit(0xA9, val)            # lda #val
    a.emit(0xA0, rep)            # ldy #rep
    l1 = a.pc
    a.emit(0xA2, 0)              # ldx #0
    l2 = a.pc
    for _ in range(per_block):
        a.abs_(op, addr)
        a.emit(*([NOP] * filler))
    a.emit(0xCA, 0xF0, 0x03)     # dex / beq +3
    a.jmp(l2)
    a.emit(0x88, 0xF0, 0x03)     # dey / beq +3
    a.jmp(l1)
    a.emit(0x60)
    return bytes(a.b), rep * 256 * per_block


def load(code):
    for i in range(0, len(code), 128):
        tm.wr(BASE + i, code[i:i + 128])


def timed(code, d031=None, d011=0x0B):
    load(code)
    tm.command(5, d031=d031, d011=d011, timeout=120)
    r = tm.rd(0xC015, 8)
    return tm.ticks(r[0:4], r[4:8])


def poke_read(writes, reads):
    """Routine: STA each (addr, val); then LDA each read addr into $C0F0+i. Returns reads."""
    a = Asm()
    for addr, val in writes:
        a.emit(0xA9, val)
        a.abs_(STA_ABS, addr)
    for i, addr in enumerate(reads):
        a.abs_(LDA_ABS, addr)
        a.abs_(STA_ABS, 0xC0F0 + i)
    a.emit(0x60)
    timed(bytes(a.b))
    return list(tm.rd(0xC0F0, len(reads))) if reads else []


def colour_rows(lines, delays, spacing_nops=1, colours=range(1, 9), base=0, sync_cia=True, phi2_wait=25):
    """Endless frame loop (until $C0FF != 0): per row, poll $D012 for the line, optionally
    sync to PHI2 and move into the visible part of the line with `phi2_wait` CIA reads, wait `delay` CPU cycles, then store each colour to $D020
    with (6 + 2*spacing_nops) cycles between stores, then the base colour."""
    a = Asm()
    a.emit(0xA9, 0)
    a.abs_(STA_ABS, 0xC0FF)
    a.emit(0xA9, base)
    a.abs_(STA_ABS, 0xD020)
    frame = a.pc
    for line, d in zip(lines, delays):
        a.emit(0xA9, line)                    # lda #line
        w = a.pc
        a.abs_(0xCD, 0xD012)                  # cmp $d012
        a.emit(0xD0, (w - (a.pc + 2)) & 0xFF)  # bne w
        if sync_cia:
            for _ in range(phi2_wait):        # each CIA read = one PHI2 cycle; also syncs to PHI2
                a.abs_(LDA_ABS, 0xDC0D)       # lda $dc0d (ICR, IRQs off)
        n2, n3 = (d // 2, 0) if d % 2 == 0 else ((d - 3) // 2, 1)
        a.emit(*([NOP] * n2))
        if n3:
            a.emit(0x24, 0xF0)                # bit $f0 (3 cycles)
        for c in colours:
            a.emit(0xA9, c)
            a.abs_(STA_ABS, 0xD020)
            a.emit(*([NOP] * spacing_nops))
        a.emit(0xA9, base)
        a.abs_(STA_ABS, 0xD020)
    a.abs_(LDA_ABS, 0xC0FF)
    a.emit(0xD0, 0x03)                        # bne +3 -> rts
    a.jmp(frame)
    a.emit(0x60)
    return bytes(a.b)


def start_async(code, d031=0x8F, d011=0x0B):
    """Load and start a routine that runs until $C0FF is set; returns at once."""
    load(code)
    params = [d031, 1, d011, 20, 1, 0x35, 32]
    tm.wr(0xC001, params)
    tm.wr(0xC000, [5])


def stop_async():
    tm.wr(0xC0FF, [1])
    time.sleep(0.3)


def mixed_rows(rows, base=6):
    """rows: list of (line, phi2_wait, body_bytes). Per row: poll the line, phi2_wait CIA reads,
    body, then the base colour. Endless until $C0FF != 0."""
    a = Asm()
    a.emit(0xA9, 0)
    a.abs_(STA_ABS, 0xC0FF)
    a.emit(0xA9, base)
    a.abs_(STA_ABS, 0xD020)
    frame = a.pc
    for line, wait, body in rows:
        a.emit(0xA9, line)
        w = a.pc
        a.abs_(0xCD, 0xD012)
        a.emit(0xD0, (w - (a.pc + 2)) & 0xFF)
        for _ in range(wait):
            a.abs_(LDA_ABS, 0xDC0D)
        a.b += body
        a.emit(0xA9, base)
        a.abs_(STA_ABS, 0xD020)
    a.abs_(LDA_ABS, 0xC0FF)
    a.emit(0xD0, 0x03)
    a.jmp(frame)
    a.emit(0x60)
    return bytes(a.b)


def b_store(col, nops=0):
    return bytes([NOP] * nops + [0xA9, col, STA_ABS, 0x20, 0xD0])


def b_store_8(cols=range(1, 9)):
    out = b""
    for c in cols:
        out += bytes([0xA9, c, STA_ABS, 0x20, 0xD0, NOP])
    return out


def b_slow_store(col):
    """switch to 1 MHz ($80), store col at 1 MHz, back to $8F."""
    return bytes([0xA9, 0x80, STA_ABS, 0x31, 0xD0, 0xA9, col, STA_ABS, 0x20, 0xD0,
                  0xA9, 0x8F, STA_ABS, 0x31, 0xD0])


def reset_hold(write_d031=None, wait=6.0):
    """resethold.bin at $8000 + REST reset; returns (ticks to change, outers, first delta, last delta)."""
    code = open(__file__.replace("tm2.py", "resethold.bin"), "rb").read()
    tm.wr(0x8100, [1 if write_d031 is not None else 0, write_d031 or 0])
    tm.wr(0x8110, [0] * 16)
    for i in range(0, len(code), 128):
        tm.wr(0x8000 + i, code[i:i + 128])
    tm.req("PUT", "/machine:reset")
    time.sleep(wait)
    r = tm.rd(0x8110, 12)
    left = (r[0] << 24) | (r[1] << 16) | (r[2] << 8) | r[3]
    return dict(changed=r[10] == 0x5A, ticks=0xFFFFFFFF - left if r[10] == 0x5A else None,
                secs=(0xFFFFFFFF - left) / 985248 if r[10] == 0x5A else None,
                outers=r[4] | r[5] << 8, first_delta=r[6] | r[7] << 8, last_delta=r[8] | r[9] << 8,
                alive=r[11])
