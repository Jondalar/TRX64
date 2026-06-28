//! Host keyboard → c64re key id mapping for the emulator window.
//!
//! Mirrors C64RE's **Spec 310 symbolic mapping** (`ui/.../Live.tsx::keyEventToC64Keys`):
//!
//! - **Special non-printable keys** use the PHYSICAL position (layout-independent):
//!   RETURN, DEL, RUN/STOP, cursor edges, function keys, shifts, the left-edge keys.
//! - **Printable keys** use the LOGICAL character (already host-layout + shift resolved
//!   by the OS), so a German QWERTZ keyboard types the right letters and symbols — no
//!   Y/Z swap, correct punctuation. Physical-position mapping (the old approach) was
//!   wrong on any non-US layout.
//!
//! Each mapping returns 1–2 c64re matrix ids (the base key + an optional `L_SHIFT` for
//! the shifted symbols). The ids are the matrix names the runtime's
//! `session/key_down`/`key_up` accept (`trx64-core/src/keyboard.rs::key_matrix`).

use winit::keyboard::KeyCode;

const LETTERS: [&str; 26] = [
    "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R", "S",
    "T", "U", "V", "W", "X", "Y", "Z",
];
const DIGITS: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];

/// SPECIAL keys by physical position (= C64RE's `e.code` switch). Layout-independent.
/// `None` ⇒ not a special key (try [`map_char`] with the logical character).
pub fn map_special(code: KeyCode) -> Option<Vec<&'static str>> {
    use KeyCode::*;
    let r = |v: &[&'static str]| Some(v.to_vec());
    match code {
        Enter | NumpadEnter => r(&["RETURN"]),
        Backspace | Delete => r(&["DEL"]),
        // Left-edge layout (C64RE BUG-026), matching the physical C64 keyboard:
        //   ESC (top-left)  → C64 ← (LARROW)
        //   Backquote (^)   → C64 CTRL
        //   TAB             → C64 RUN/STOP
        Escape => r(&["LARROW"]),
        Tab => r(&["RUN_STOP"]),
        Backquote => r(&["CTRL"]),
        Home => r(&["HOME"]),
        ControlLeft | ControlRight => r(&["CTRL"]),
        ShiftLeft => r(&["L_SHIFT"]),
        ShiftRight => r(&["R_SHIFT"]),
        // The Commodore key — host Cmd/Super.
        SuperLeft | SuperRight => r(&["C_EQ"]),
        // F2/F4/F6/F8 are SHIFT + F1/F3/F5/F7 on the C64.
        F1 => r(&["F1"]),
        F2 => r(&["L_SHIFT", "F1"]),
        F3 => r(&["F3"]),
        F4 => r(&["L_SHIFT", "F3"]),
        F5 => r(&["F5"]),
        F6 => r(&["L_SHIFT", "F5"]),
        F7 => r(&["F7"]),
        F8 => r(&["L_SHIFT", "F7"]),
        Space => r(&["SPACE"]),
        // Cursor keys — the C64 matrix has CRSR_DN + CRSR_RT; up/left are SHIFT + those.
        ArrowDown => r(&["CRSR_DN"]),
        ArrowRight => r(&["CRSR_RT"]),
        ArrowUp => r(&["L_SHIFT", "CRSR_DN"]),
        ArrowLeft => r(&["L_SHIFT", "CRSR_RT"]),
        _ => None,
    }
}

/// Virtual-joystick directions (= C64RE's `joystickBitForCode`, Spec 310): **WASD +
/// Space**. Only consulted when joystick mode is enabled; otherwise these are normal
/// keyboard keys (W/A/S/D letters, Space). `None` ⇒ not a joystick key.
#[derive(Clone, Copy, PartialEq)]
pub enum JoyBit {
    Up,
    Down,
    Left,
    Right,
    Fire,
}

pub fn joy_bit(code: KeyCode) -> Option<JoyBit> {
    match code {
        KeyCode::KeyW => Some(JoyBit::Up),
        KeyCode::KeyA => Some(JoyBit::Left),
        KeyCode::KeyS => Some(JoyBit::Down),
        KeyCode::KeyD => Some(JoyBit::Right),
        KeyCode::Space => Some(JoyBit::Fire),
        _ => None,
    }
}

/// PRINTABLE keys by logical character (= C64RE's `e.key` branch). `ch` is the
/// host-layout + shift resolved character (winit `logical_key`); `shift` = whether a
/// Shift modifier is held. Returns the c64re matrix id(s), or `None`.
pub fn map_char(ch: char, shift: bool) -> Option<Vec<&'static str>> {
    // Letters A–Z (the C64 matrix is uppercase; shift adds L_SHIFT for the graphic).
    if ch.is_ascii_alphabetic() {
        let key = LETTERS[(ch.to_ascii_uppercase() as u8 - b'A') as usize];
        return Some(if shift { vec!["L_SHIFT", key] } else { vec![key] });
    }
    // Digits 0–9 (unshifted).
    if ch.is_ascii_digit() && !shift {
        return Some(vec![DIGITS[(ch as u8 - b'0') as usize]]);
    }
    // Common punctuation: the unshifted host character maps straight onto a C64 key.
    let direct = match ch {
        '+' => Some("+"),
        '-' => Some("-"),
        '*' => Some("*"),
        '/' => Some("/"),
        '=' => Some("="),
        ':' => Some(":"),
        ';' => Some(";"),
        ',' => Some(","),
        '.' => Some("."),
        '@' => Some("@"),
        _ => None,
    };
    if let Some(k) = direct {
        return Some(vec![k]);
    }
    // Shifted punctuation → L_SHIFT + the base C64 key per the matrix. The shifted
    // symbols are unambiguous, but gate on `shift` to mirror C64RE exactly.
    if shift {
        let base = match ch {
            '"' => Some("2"),
            '?' => Some("/"),
            '(' => Some("8"),
            ')' => Some("9"),
            '<' => Some(","),
            '>' => Some("."),
            '!' => Some("1"),
            '$' => Some("4"),
            '%' => Some("5"),
            '&' => Some("6"),
            '\'' => Some("7"),
            _ => None,
        };
        if let Some(b) = base {
            return Some(vec!["L_SHIFT", b]);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_symbolic_not_positional() {
        // A German user pressing the key labelled "Z" yields logical 'z' → C64 "Z"
        // (positional mapping would have produced "Y" — the bug this fixes).
        assert_eq!(map_char('z', false), Some(vec!["Z"]));
        assert_eq!(map_char('y', false), Some(vec!["Y"]));
        assert_eq!(map_char('A', true), Some(vec!["L_SHIFT", "A"]));
    }

    #[test]
    fn shifted_punctuation_maps_to_matrix_base() {
        // German Shift+2 = '"' → C64 " is L_SHIFT+2.
        assert_eq!(map_char('"', true), Some(vec!["L_SHIFT", "2"]));
        assert_eq!(map_char('?', true), Some(vec!["L_SHIFT", "/"]));
        assert_eq!(map_char('+', false), Some(vec!["+"]));
        assert_eq!(map_char('5', false), Some(vec!["5"]));
    }

    #[test]
    fn special_keys_by_position() {
        assert_eq!(map_special(KeyCode::Enter), Some(vec!["RETURN"]));
        assert_eq!(map_special(KeyCode::Escape), Some(vec!["LARROW"]));
        assert_eq!(map_special(KeyCode::F2), Some(vec!["L_SHIFT", "F1"]));
        assert_eq!(map_special(KeyCode::KeyA), None); // a letter → map_char
    }
}
