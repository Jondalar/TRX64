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
///
/// Two parallel input sources, both folded into `read_rows_for_pa`:
///   - `events`: the timed-event queue (`type_text` schedules `[start,end)`
///     windows, gated on the cycle). Unchanged from Sprint 79.
///   - `live_pressed`: the Spec 310 *held-key* set — a key stays pressed until
///     an explicit release and is queryable via `pressed_keys()`. 1:1 with the
///     c64re `KeyboardMatrix.livePressed: Set<KeyName>` (keyboard.ts:65).
///
/// The held set is purely additive: when it is empty, `read_rows_for_pa` is
/// byte-identical to the timed-event-only path, so all type_text-based gates
/// are unchanged.
#[derive(Clone, Debug, Default)]
pub struct KeyboardMatrix {
    events: Vec<KeyEvent>,
    /// Spec 310 — held keys (browser keydown without a matching keyup). Stores
    /// the c64re key id (e.g. "A", "L_SHIFT", "RUN_STOP"), matching the TS
    /// `Set<KeyName>` so `pressed_keys()` returns the same names the WS layer
    /// reports. Insertion order preserved (mirrors JS `Set` iteration order)
    /// so `pressed_keys()` is deterministic.
    live_pressed: Vec<String>,
}

impl KeyboardMatrix {
    pub fn new() -> Self {
        Self {
            events: Vec::new(),
            live_pressed: Vec::new(),
        }
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

    /// Drop all queued events AND the held-key set (reset / cold-reset path).
    /// Mirrors the TS `KeyboardMatrix.clear()` (keyboard.ts:142) which wipes
    /// `events` + `livePressed`. The reset path in c64re calls this via the
    /// `kb.clear?.()` probe (integrated-session.ts:737).
    pub fn clear(&mut self) {
        self.events.clear();
        self.live_pressed.clear();
    }

    /// Spec 310 — mark a key as held (browser keydown). 1:1 with the TS
    /// `setKeyDown` (keyboard.ts:104): adds to the held set, no-op if already
    /// present. `key` is a c64re key id (see `key_matrix`); unknown ids are
    /// still recorded (so `pressed_keys()` round-trips) but contribute nothing
    /// to the row mask, exactly like the TS (`KEY_MATRIX[key]` miss → skipped).
    pub fn key_down(&mut self, key: &str) {
        if !self.live_pressed.iter().any(|k| k == key) {
            self.live_pressed.push(key.to_string());
        }
    }

    /// Spec 310 — release a held key (browser keyup). 1:1 with `setKeyUp`
    /// (keyboard.ts:105): removes from the held set, no-op if absent.
    pub fn key_up(&mut self, key: &str) {
        self.live_pressed.retain(|k| k != key);
    }

    /// Spec 310 — release every held key. 1:1 with `releaseAllLive`
    /// (keyboard.ts:106). Does NOT touch the timed-event queue.
    pub fn release_keys(&mut self) {
        self.live_pressed.clear();
    }

    /// Spec 310 — the held-key set as a list of c64re key ids. 1:1 with
    /// `livePressedKeys` (keyboard.ts:107) — iteration order = insertion order.
    pub fn pressed_keys(&self) -> Vec<String> {
        self.live_pressed.clone()
    }

    /// Active row mask (active-low: bit cleared = key pressed) for the columns
    /// currently driven low by `pa_value`, evaluated at cycle `now`. Mirrors the
    /// TS `readRowsForPa` — folds BOTH the timed events (cycle-window gated) AND
    /// the held-key set (Spec 310) into the mask. With no keys held the result
    /// is byte-identical to the events-only path.
    pub fn read_rows_for_pa(&self, now: u64, pa_value: u8) -> u8 {
        let mut row_mask: u8 = 0xff;
        // Queued events (type_text / timed-window).
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
        // Spec 310 — held keys (browser passthrough), no cycle gating.
        for key in &self.live_pressed {
            if let Some((col, row)) = key_matrix(key) {
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

    #[test]
    fn no_held_keys_is_byte_identical_to_events_only() {
        // The held set is additive: with nothing held, the read path matches
        // the pre-Spec-310 events-only behaviour for every column/cycle.
        let mut kb = KeyboardMatrix::new();
        kb.type_text(0, "LOAD\"$\",8\r", 100, 50);
        assert!(kb.pressed_keys().is_empty());
        for now in [0u64, 50, 99, 150, 300] {
            for pa in 0u16..=0xff {
                let pa = pa as u8;
                // Re-derive the events-only mask inline and compare.
                let mut expected: u8 = 0xff;
                for ev in &kb.events {
                    if now < ev.start_cycle || now >= ev.end_cycle {
                        continue;
                    }
                    if let Some((col, row)) = key_matrix(&ev.key) {
                        if pa & (1 << col) == 0 {
                            expected &= !(1 << row);
                        }
                    }
                }
                assert_eq!(kb.read_rows_for_pa(now, pa), expected, "now={now} pa={pa:#x}");
            }
        }
    }

    #[test]
    fn held_key_presses_until_key_up_regardless_of_cycle() {
        // key_down 'A' (col1 row2): pressed at ANY cycle, with no timed window.
        let mut kb = KeyboardMatrix::new();
        let pa_col1 = 0xff & !(1 << 1);
        // Nothing held yet.
        assert_eq!(kb.read_rows_for_pa(0, pa_col1), 0xff);
        kb.key_down("A");
        assert_eq!(kb.pressed_keys(), vec!["A".to_string()]);
        // Pressed at cycle 0, at a huge cycle, everywhere in between.
        assert_eq!(kb.read_rows_for_pa(0, pa_col1), 0xff & !(1 << 2));
        assert_eq!(kb.read_rows_for_pa(9_999_999, pa_col1), 0xff & !(1 << 2));
        // Only pulls the row when its column is driven low.
        let pa_other = 0xff & !(1 << 5);
        assert_eq!(kb.read_rows_for_pa(0, pa_other), 0xff, "A not on col5");
        // key_up releases it.
        kb.key_up("A");
        assert!(kb.pressed_keys().is_empty());
        assert_eq!(kb.read_rows_for_pa(0, pa_col1), 0xff, "released");
    }

    #[test]
    fn key_down_is_idempotent_and_key_up_unknown_is_noop() {
        let mut kb = KeyboardMatrix::new();
        kb.key_down("Q");
        kb.key_down("Q");
        assert_eq!(kb.pressed_keys(), vec!["Q".to_string()], "no duplicate");
        kb.key_up("NOT_HELD"); // no-op
        assert_eq!(kb.pressed_keys(), vec!["Q".to_string()]);
    }

    #[test]
    fn release_keys_clears_held_but_not_timed_events() {
        let mut kb = KeyboardMatrix::new();
        kb.type_text(0, "A", 100, 50); // timed 'A' in [0,100)
        kb.key_down("Q"); // held 'Q' = col7 row6
        kb.release_keys();
        assert!(kb.pressed_keys().is_empty(), "held cleared");
        // The timed 'A' event survives release_keys.
        let pa_col1 = 0xff & !(1 << 1);
        assert_eq!(kb.read_rows_for_pa(0, pa_col1), 0xff & !(1 << 2), "timed A still there");
    }

    #[test]
    fn held_and_timed_combine_in_one_read() {
        // Hold L_SHIFT (col1 row7) while a timed '2' (col7 row3) is in window:
        // a single PA-drive of either column reflects its key.
        let mut kb = KeyboardMatrix::new();
        kb.type_text(0, "2", 100, 50); // '2' = col7 row3
        kb.key_down("L_SHIFT"); // col1 row7
        let pa_col7 = 0xff & !(1 << 7);
        assert_eq!(kb.read_rows_for_pa(0, pa_col7), 0xff & !(1 << 3), "timed 2");
        let pa_col1 = 0xff & !(1 << 1);
        assert_eq!(kb.read_rows_for_pa(0, pa_col1), 0xff & !(1 << 7), "held L_SHIFT");
        // Drive both columns at once → both rows pulled.
        let pa_both = 0xff & !(1 << 7) & !(1 << 1);
        assert_eq!(
            kb.read_rows_for_pa(0, pa_both),
            0xff & !(1 << 3) & !(1 << 7),
            "both rows"
        );
    }

    #[test]
    fn clear_wipes_held_and_events() {
        let mut kb = KeyboardMatrix::new();
        kb.type_text(0, "A", 100, 50);
        kb.key_down("Q");
        kb.clear();
        assert!(kb.pressed_keys().is_empty());
        let pa_col1 = 0xff & !(1 << 1);
        assert_eq!(kb.read_rows_for_pa(0, pa_col1), 0xff, "events gone too");
    }
}
