//! CLI-FEEL S2 — LS_COLORS-lite for the cockpit.
//!
//! A tiny, PURE filetype→color map. No I/O, no state — just a name (+ a dir flag)
//! in, a `ratatui` [`Style`] out. Consumed by `!ls` rendering (S4) and the Tab
//! candidate lists (S5); kept isolated so both call sites agree on one palette and
//! the mapping is unit-testable without a terminal.
//!
//! Palette (agreed in the spec):
//!   dir                                   → blue + bold
//!   .crt                                  → yellow   (cartridge)
//!   .d64 / .g64 / .p64                    → cyan     (disk image)
//!   .prg / .bin                           → green    (program / raw)
//!   .c64re / .c64retrace / .c64rering     → magenta  (snapshot / trace / ring)
//!   .asm / .tass / .md / .json            → gray     (source / text)
//!   else                                  → default terminal fg
//!
//! Extension match is case-insensitive; dotfiles (e.g. `.gitignore`) and
//! extension-less names fall through to [`Bucket::Other`].

use ratatui::style::{Color, Modifier, Style};

/// Coarse filetype class derived from a name's extension. Directories are NOT a
/// bucket — they are handled by the `is_dir` flag in [`style_for`], so the drive's
/// `d|-` column stays the single source of truth for dir-ness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    /// `.crt` — cartridge image.
    Cart,
    /// `.d64` / `.g64` / `.p64` — disk image.
    Disk,
    /// `.prg` / `.bin` — program / raw bytes.
    Program,
    /// `.c64re` / `.c64retrace` / `.c64rering` — snapshot / trace / checkpoint ring.
    Snapshot,
    /// `.asm` / `.tass` / `.md` / `.json` — source / text.
    Source,
    /// Anything else (incl. extension-less names and dotfiles).
    Other,
}

/// Lowercased final extension of `name`, or `None` for extension-less names and
/// dotfiles (`.gitignore` → `None`, matching `Path::extension` semantics).
fn ext_of(name: &str) -> Option<String> {
    std::path::Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
}

/// Classify a bare filename by its extension (case-insensitive). Directory-ness is
/// out of scope here — pass it separately to [`style_for`].
pub fn ext_bucket(name: &str) -> Bucket {
    match ext_of(name).as_deref() {
        Some("crt") => Bucket::Cart,
        Some("d64") | Some("g64") | Some("p64") => Bucket::Disk,
        Some("prg") | Some("bin") => Bucket::Program,
        Some("c64re") | Some("c64retrace") | Some("c64rering") => Bucket::Snapshot,
        Some("asm") | Some("tass") | Some("md") | Some("json") => Bucket::Source,
        _ => Bucket::Other,
    }
}

/// The `ratatui` [`Style`] for a directory entry. Directories win over any
/// extension the name might carry (a `foo.crt/` directory still renders dir-blue).
pub fn style_for(name: &str, is_dir: bool) -> Style {
    if is_dir {
        return Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD);
    }
    match ext_bucket(name) {
        Bucket::Cart => Style::default().fg(Color::Yellow),
        Bucket::Disk => Style::default().fg(Color::Cyan),
        Bucket::Program => Style::default().fg(Color::Green),
        Bucket::Snapshot => Style::default().fg(Color::Magenta),
        Bucket::Source => Style::default().fg(Color::Gray),
        Bucket::Other => Style::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_by_extension() {
        assert_eq!(ext_bucket("game.crt"), Bucket::Cart);
        assert_eq!(ext_bucket("disk.d64"), Bucket::Disk);
        assert_eq!(ext_bucket("disk.g64"), Bucket::Disk);
        assert_eq!(ext_bucket("disk.p64"), Bucket::Disk);
        assert_eq!(ext_bucket("loader.prg"), Bucket::Program);
        assert_eq!(ext_bucket("payload.bin"), Bucket::Program);
        assert_eq!(ext_bucket("snap.c64re"), Bucket::Snapshot);
        assert_eq!(ext_bucket("run.c64retrace"), Bucket::Snapshot);
        assert_eq!(ext_bucket("ckpt.c64rering"), Bucket::Snapshot);
        assert_eq!(ext_bucket("main.asm"), Bucket::Source);
        assert_eq!(ext_bucket("main.tass"), Bucket::Source);
        assert_eq!(ext_bucket("notes.md"), Bucket::Source);
        assert_eq!(ext_bucket("meta.json"), Bucket::Source);
    }

    #[test]
    fn extension_match_is_case_insensitive() {
        assert_eq!(ext_bucket("GAME.CRT"), Bucket::Cart);
        assert_eq!(ext_bucket("Disk.D64"), Bucket::Disk);
        assert_eq!(ext_bucket("LOADER.Prg"), Bucket::Program);
        assert_eq!(ext_bucket("SNAP.C64Re"), Bucket::Snapshot);
        assert_eq!(ext_bucket("Main.ASM"), Bucket::Source);
    }

    #[test]
    fn no_extension_and_dotfiles_are_other() {
        assert_eq!(ext_bucket("README"), Bucket::Other);
        assert_eq!(ext_bucket("Makefile"), Bucket::Other);
        assert_eq!(ext_bucket(".gitignore"), Bucket::Other);
        assert_eq!(ext_bucket(".c64re"), Bucket::Other); // leading-dot only → no ext
        assert_eq!(ext_bucket(""), Bucket::Other);
        assert_eq!(ext_bucket("weird.xyz"), Bucket::Other);
    }

    #[test]
    fn style_directory_is_blue_bold_regardless_of_name() {
        let expect = Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD);
        assert_eq!(style_for("sub", true), expect);
        // A dir named like a cart still renders as a dir (is_dir wins).
        assert_eq!(style_for("games.crt", true), expect);
    }

    #[test]
    fn style_maps_each_bucket_to_its_color() {
        assert_eq!(style_for("game.crt", false), Style::default().fg(Color::Yellow));
        assert_eq!(style_for("disk.d64", false), Style::default().fg(Color::Cyan));
        assert_eq!(style_for("loader.prg", false), Style::default().fg(Color::Green));
        assert_eq!(style_for("snap.c64re", false), Style::default().fg(Color::Magenta));
        assert_eq!(style_for("main.asm", false), Style::default().fg(Color::Gray));
    }

    #[test]
    fn style_other_is_plain_default() {
        assert_eq!(style_for("README", false), Style::default());
        assert_eq!(style_for(".gitignore", false), Style::default());
    }

    #[test]
    fn style_is_case_insensitive_too() {
        assert_eq!(style_for("GAME.CRT", false), Style::default().fg(Color::Yellow));
    }
}
