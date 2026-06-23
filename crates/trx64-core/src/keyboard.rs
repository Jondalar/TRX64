//! C64 keyboard matrix model + scriptable input queue.
//!
//! Faithful port of the TS golden's `peripherals/keyboard.ts` (Sprint 79 /
//! Spec 403). Real C64 has an 8×8 key matrix scanned by CIA1: PA drives the
//! columns (active-low), PB reads the rows (active-low). `session/type` queues
//! timed key presses; the CIA1 PB read returns the active row mask for whatever
//! columns the CPU is currently driving.
//!
//! Cycle model: the TS keyboard advances `cycleNow` by each instruction's cycle
//! cost (integrated-session.ts:947 `keyboard.advance(totalCycles)`), so
//! `cycleNow == c64Cpu.cycles`. In TRX64 the CPU master clock `cpu6510.clk` is
//! that same accumulated cycle count, so the FullBus passes `clk` straight in as
//! the read-time "now" — no separate advance bookkeeping needed.

/// PETSCII key name → matrix coordinate `[col, row]` (0-based), matching the TS
/// `KEY_MATRIX`. Real hardware: PA = column drive (active-low), PB = row read
/// (active-low). KERNAL SCNKEY scan code = col*8 + row.
fn key_matrix(key: &str) -> Option<(u8, u8)> {
    let c = match key {
        // Col 7
        "RUN_STOP" => (7, 7), "Q" => (7, 6), "C_EQ" => (7, 5), "SPACE" => (7, 4),
        "2" => (7, 3), "CTRL" => (7, 2), "LARROW" => (7, 1), "1" => (7, 0),
        // Col 6
        "/" => (6, 7), "UP_ARROW" => (6, 6), "=" => (6, 5), "R_SHIFT" => (6, 4),
        "HOME" => (6, 3), ";" => (6, 2), "*" => (6, 1), "POUND" => (6, 0),
        // Col 5
        "," => (5, 7), "@" => (5, 6), ":" => (5, 5), "." => (5, 4),
        "-" => (5, 3), "L" => (5, 2), "P" => (5, 1), "+" => (5, 0),
        // Col 4
        "N" => (4, 7), "O" => (4, 6), "K" => (4, 5), "M" => (4, 4),
        "0" => (4, 3), "J" => (4, 2), "I" => (4, 1), "9" => (4, 0),
        // Col 3
        "V" => (3, 7), "U" => (3, 6), "H" => (3, 5), "B" => (3, 4),
        "8" => (3, 3), "G" => (3, 2), "Y" => (3, 1), "7" => (3, 0),
        // Col 2
        "X" => (2, 7), "T" => (2, 6), "F" => (2, 5), "C" => (2, 4),
        "6" => (2, 3), "D" => (2, 2), "R" => (2, 1), "5" => (2, 0),
        // Col 1
        "L_SHIFT" => (1, 7), "E" => (1, 6), "S" => (1, 5), "Z" => (1, 4),
        "4" => (1, 3), "A" => (1, 2), "W" => (1, 1), "3" => (1, 0),
        // Col 0
        "CRSR_DN" => (0, 7), "F5" => (0, 6), "F3" => (0, 5), "F1" => (0, 4),
        "F7" => (0, 3), "CRSR_RT" => (0, 2), "RETURN" => (0, 1), "DEL" => (0, 0),
        _ => return None,
    };
    Some(c)
}

/// Shifted PETSCII characters → their unshifted base key (the key is pressed
/// together with L_SHIFT). Mirrors the TS `SHIFTED_CHARS`.
fn shifted_base(ch: char) -> Option<&'static str> {
    Some(match ch {
        '"' => "2",
        '(' => "8",
        ')' => "9",
        '?' => "/",
        '<' => ",",
        '>' => ".",
        '[' => ":",
        ']' => ";",
        '!' => "1",
        '#' => "3",
        '$' => "4",
        '%' => "5",
        '&' => "6",
        '\'' => "7",
        _ => return None,
    })
}

/// Resolve a typed character to its matrix key + whether SHIFT must be held.
fn lookup_char(ch: char) -> Option<(String, bool)> {
    if ch == ' ' {
        return Some(("SPACE".to_string(), false));
    }
    if ch == '\n' || ch == '\r' {
        return Some(("RETURN".to_string(), false));
    }
    if ch == '\t' {
        return None;
    }
    let up = ch.to_ascii_uppercase().to_string();
    if key_matrix(&up).is_some() {
        return Some((up, false));
    }
    if let Some(base) = shifted_base(ch).or_else(|| shifted_base(ch.to_ascii_uppercase())) {
        if key_matrix(base).is_some() {
            return Some((base.to_string(), true));
        }
    }
    None
}

#[derive(Clone, Debug)]
struct KeyEvent {
    key: String,
    start_cycle: u64,
    end_cycle: u64,
}

/// Scriptable keyboard-matrix backend for CIA1 PA(column)/PB(row).
#[derive(Clone, Debug, Default)]
pub struct KeyboardMatrix {
    events: Vec<KeyEvent>,
}

impl KeyboardMatrix {
    pub fn new() -> Self {
        Self { events: Vec::new() }
    }

    /// Queue a text string as a sequence of timed key presses starting at
    /// `now_cycle`. Each character holds for `hold_cycles`, with `gap_cycles`
    /// between presses. Shifted characters also queue an L_SHIFT for the same
    /// window. Returns the number of source characters consumed (matches the
    /// TS `queued: text.length` response field — counts every char, including
    /// any skipped ones, exactly like the TS `text.length`).
    pub fn type_text(&mut self, now_cycle: u64, text: &str, hold_cycles: u64, gap_cycles: u64) {
        let mut off: u64 = 0;
        for ch in text.chars() {
            let Some((key, shift)) = lookup_char(ch) else { continue };
            let start = now_cycle + off;
            let end = start + hold_cycles;
            self.events.push(KeyEvent { key, start_cycle: start, end_cycle: end });
            if shift {
                self.events.push(KeyEvent {
                    key: "L_SHIFT".to_string(),
                    start_cycle: start,
                    end_cycle: end,
                });
            }
            off += hold_cycles + gap_cycles;
        }
    }

    /// Drop all queued events (reset / cold-reset path).
    pub fn clear(&mut self) {
        self.events.clear();
    }

    /// Active row mask (active-low: bit cleared = key pressed) for the columns
    /// currently driven low by `pa_value`, evaluated at cycle `now`. Mirrors the
    /// TS `readRowsForPa`.
    pub fn read_rows_for_pa(&self, now: u64, pa_value: u8) -> u8 {
        let mut row_mask: u8 = 0xff;
        for ev in &self.events {
            if now < ev.start_cycle || now >= ev.end_cycle {
                continue;
            }
            if let Some((col, row)) = key_matrix(&ev.key) {
                if pa_value & (1 << col) == 0 {
                    row_mask &= !(1 << row);
                }
            }
        }
        row_mask
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_load_directory_resolves_quote_with_shift() {
        // LOAD"$",8 — the '"' is shifted (base key "2"), '$' is shifted (base "4").
        let mut kb = KeyboardMatrix::new();
        kb.type_text(0, "LOAD\"$\",8\r", 100, 50);
        // First char 'L' at col 5 row 2 → during [0,100) PA driving col5 low pulls row2.
        let pa_no_col5 = 0xff; // no column driven low
        assert_eq!(kb.read_rows_for_pa(0, pa_no_col5), 0xff, "no column driven = no rows");
        let pa_col5 = 0xff & !(1 << 5);
        assert_eq!(kb.read_rows_for_pa(0, pa_col5), 0xff & !(1 << 2), "L pulls row2 on col5");
    }

    #[test]
    fn shifted_quote_queues_left_shift() {
        let mut kb = KeyboardMatrix::new();
        kb.type_text(0, "\"", 100, 50);
        // '"' base key "2" = col7 row3; plus L_SHIFT = col1 row7.
        let pa_col7 = 0xff & !(1 << 7);
        assert_eq!(kb.read_rows_for_pa(0, pa_col7), 0xff & !(1 << 3));
        let pa_col1 = 0xff & !(1 << 1);
        assert_eq!(kb.read_rows_for_pa(0, pa_col1), 0xff & !(1 << 7), "L_SHIFT on col1 row7");
    }

    #[test]
    fn events_expire_after_hold() {
        let mut kb = KeyboardMatrix::new();
        kb.type_text(0, "A", 100, 50);
        let pa_col1 = 0xff & !(1 << 1); // 'A' = col1 row2
        assert_eq!(kb.read_rows_for_pa(0, pa_col1), 0xff & !(1 << 2));
        assert_eq!(kb.read_rows_for_pa(100, pa_col1), 0xff, "expired at end_cycle");
    }
}
