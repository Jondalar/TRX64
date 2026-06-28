//! Host keyboard → c64re key id mapping for the emulator window.
//!
//! The c64re key ids are the matrix names the runtime's `session/key_down`/`key_up`
//! accept (see `trx64-core/src/keyboard.rs::key_matrix`): "A".."Z", "0".."9",
//! "RETURN", "SPACE", "RUN_STOP", "L_SHIFT"/"R_SHIFT", "CTRL", "C_EQ", cursor keys
//! ("CRSR_DN"/"CRSR_RT" with shift for up/left in the matrix), function keys, and
//! the punctuation matrix keys.
//!
//! winit gives us `winit::keyboard::Key` (logical) + `KeyCode` (physical). We map the
//! physical `KeyCode` for letters/digits/named keys (layout-independent for the core
//! gameplay set) and fall through to the logical character for punctuation.

use winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};

/// A mapped key press: the c64re key id plus whether L_SHIFT must be co-held to
/// reach the desired PETSCII symbol (for shifted punctuation typed on a host).
pub struct Mapped {
    pub key: &'static str,
    pub shift: bool,
}

/// Map a winit physical key code to a c64re key id. Returns `None` for keys we don't
/// forward (modifiers are handled separately; unknown keys are ignored).
pub fn map_physical(code: KeyCode) -> Option<Mapped> {
    let m = |key: &'static str| Some(Mapped { key, shift: false });
    match code {
        // Letters
        KeyCode::KeyA => m("A"), KeyCode::KeyB => m("B"), KeyCode::KeyC => m("C"),
        KeyCode::KeyD => m("D"), KeyCode::KeyE => m("E"), KeyCode::KeyF => m("F"),
        KeyCode::KeyG => m("G"), KeyCode::KeyH => m("H"), KeyCode::KeyI => m("I"),
        KeyCode::KeyJ => m("J"), KeyCode::KeyK => m("K"), KeyCode::KeyL => m("L"),
        KeyCode::KeyM => m("M"), KeyCode::KeyN => m("N"), KeyCode::KeyO => m("O"),
        KeyCode::KeyP => m("P"), KeyCode::KeyQ => m("Q"), KeyCode::KeyR => m("R"),
        KeyCode::KeyS => m("S"), KeyCode::KeyT => m("T"), KeyCode::KeyU => m("U"),
        KeyCode::KeyV => m("V"), KeyCode::KeyW => m("W"), KeyCode::KeyX => m("X"),
        KeyCode::KeyY => m("Y"), KeyCode::KeyZ => m("Z"),
        // Digits (unshifted)
        KeyCode::Digit0 => m("0"), KeyCode::Digit1 => m("1"), KeyCode::Digit2 => m("2"),
        KeyCode::Digit3 => m("3"), KeyCode::Digit4 => m("4"), KeyCode::Digit5 => m("5"),
        KeyCode::Digit6 => m("6"), KeyCode::Digit7 => m("7"), KeyCode::Digit8 => m("8"),
        KeyCode::Digit9 => m("9"),
        // Named / control
        KeyCode::Enter | KeyCode::NumpadEnter => m("RETURN"),
        KeyCode::Space => m("SPACE"),
        KeyCode::Backspace | KeyCode::Delete => m("DEL"),
        KeyCode::Escape => m("RUN_STOP"),
        KeyCode::Home => m("HOME"),
        // Cursor keys — the C64 matrix has CRSR_DN + CRSR_RT; up/left are shift+those.
        KeyCode::ArrowDown => m("CRSR_DN"),
        KeyCode::ArrowRight => m("CRSR_RT"),
        KeyCode::ArrowUp => Some(Mapped { key: "CRSR_DN", shift: true }),
        KeyCode::ArrowLeft => Some(Mapped { key: "CRSR_RT", shift: true }),
        // Function keys (F2/F4/F6/F8 are shift+F1/F3/F5/F7 on the C64).
        KeyCode::F1 => m("F1"), KeyCode::F3 => m("F3"),
        KeyCode::F5 => m("F5"), KeyCode::F7 => m("F7"),
        KeyCode::F2 => Some(Mapped { key: "F1", shift: true }),
        KeyCode::F4 => Some(Mapped { key: "F3", shift: true }),
        KeyCode::F6 => Some(Mapped { key: "F5", shift: true }),
        KeyCode::F8 => Some(Mapped { key: "F7", shift: true }),
        // Punctuation that maps directly onto a C64 matrix key.
        KeyCode::Comma => m(","), KeyCode::Period => m("."),
        KeyCode::Slash => m("/"), KeyCode::Semicolon => m(";"),
        KeyCode::Equal => m("="), KeyCode::Minus => m("-"),
        KeyCode::Quote => m("@"),
        _ => None,
    }
}

/// Map a winit named key to a c64re modifier/control id (for keys best read from the
/// logical `Key` — shifts/ctrl). Returns the c64re key id.
pub fn map_named(named: NamedKey) -> Option<&'static str> {
    match named {
        NamedKey::Shift => Some("L_SHIFT"),
        NamedKey::Control => Some("CTRL"),
        // The Commodore key — map the host Super/Meta (Cmd) to C= so EQ-style combos
        // work in the window.
        NamedKey::Super | NamedKey::Meta => Some("C_EQ"),
        _ => None,
    }
}

/// Best-effort logical-key mapping used as a fallback for character keys that the
/// physical map missed.
pub fn map_logical(key: &Key) -> Option<&'static str> {
    if let Key::Named(named) = key {
        return map_named(*named);
    }
    None
}

/// Identify a physical key as a modifier (so the window can track held-shift for the
/// matrix). Returns true for shift/ctrl/cmd physical codes.
pub fn is_modifier(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::ShiftLeft
            | KeyCode::ShiftRight
            | KeyCode::ControlLeft
            | KeyCode::ControlRight
            | KeyCode::SuperLeft
            | KeyCode::SuperRight
    )
}

/// Map a modifier physical code to its c64re key id.
pub fn modifier_id(code: KeyCode) -> Option<&'static str> {
    match code {
        KeyCode::ShiftLeft => Some("L_SHIFT"),
        KeyCode::ShiftRight => Some("R_SHIFT"),
        KeyCode::ControlLeft | KeyCode::ControlRight => Some("CTRL"),
        KeyCode::SuperLeft | KeyCode::SuperRight => Some("C_EQ"),
        _ => None,
    }
}

/// Convenience: resolve a `winit` key event's physical key into our mapping, honouring
/// the modifier passthrough first.
pub fn resolve(physical: PhysicalKey) -> Option<Mapped> {
    match physical {
        PhysicalKey::Code(code) => {
            if let Some(id) = modifier_id(code) {
                return Some(Mapped { key: id, shift: false });
            }
            map_physical(code)
        }
        PhysicalKey::Unidentified(_) => None,
    }
}
