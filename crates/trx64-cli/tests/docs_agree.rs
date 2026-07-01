//! CLI-FEEL S8 — docs consistency guard.
//!
//! S8 is a docs slice: MONITOR.md gains the three-namespace cockpit note, the crate
//! README documents the namespace model / autocomplete / colors / media / readline
//! keys, and `help_text()` must agree with both. This test pins that agreement so the
//! three surfaces can't silently drift apart. It is PURE — reads the shipped `help_text`
//! strings + the two Markdown docs from disk (no ROMs, no machine boot).

use std::path::PathBuf;

use trx64_cli::engine::{fs_help_text, help_text, FS_VERBS};

/// The crate's `README.md` (cockpit doc), read from `$CARGO_MANIFEST_DIR`.
fn read_readme() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("README.md");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The repo-root `MONITOR.md` (`$CARGO_MANIFEST_DIR/../../MONITOR.md`).
fn read_monitor_md() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../MONITOR.md");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// `help_text()` presents the three namespaces + the S1 aliases + the S5 completion
/// promise. This is the in-cockpit `/help` surface the docs must agree with.
#[test]
fn help_text_lists_three_namespaces_and_aliases() {
    let h = help_text();
    // Three-namespace banner.
    assert!(h.contains("Three namespaces"), "help banner missing: {h}");
    assert!(h.contains("the machine"), "help missing /-namespace label: {h}");
    assert!(h.contains("the filesystem"), "help missing !-namespace label: {h}");
    assert!(h.contains("the monitor"), "help missing bare-namespace label: {h}");
    // S1 aliases must be documented (they were the additions over the old 2-namespace help).
    for alias in ["/umount", "/undump", "/settings"] {
        assert!(h.contains(alias), "help missing alias {alias}: {h}");
    }
    // S5: Tab completes verbs AND paths — the help must not still promise it as "coming".
    assert!(h.contains("Tab completes"), "help missing Tab-completion note: {h}");
    assert!(h.contains("paths"), "help missing path-completion note: {h}");
    assert!(!h.contains("coming"), "help still says completion is 'coming': {h}");
}

/// Every FS verb in the dispatch's `FS_VERBS` is documented in BOTH help surfaces as its
/// `!`-prefixed form — the cockpit routing layer and its help can't drift from the verb set.
#[test]
fn fs_verbs_documented_in_both_help_surfaces() {
    let h = help_text();
    let fs = fs_help_text();
    for verb in FS_VERBS {
        let bang = format!("!{verb}");
        assert!(fs.contains(&bang), "fs_help_text missing {bang}: {fs}");
        assert!(h.contains(&bang), "help_text missing {bang}: {h}");
    }
}

/// MONITOR.md carries the cockpit namespace note (all three) + the File-section caveat
/// that the FS verbs are `!`-prefixed in the cockpit but stay bare-callable elsewhere.
#[test]
fn monitor_md_documents_cockpit_namespaces() {
    let m = read_monitor_md();
    assert!(m.contains("three command namespaces"), "MONITOR.md missing namespace note");
    assert!(m.contains("`/`-prefixed"), "MONITOR.md missing /-namespace");
    assert!(m.contains("`!`-prefixed"), "MONITOR.md missing !-namespace");
    assert!(m.contains("**Tab**"), "MONITOR.md missing Tab note");
    // File-section caveat — the shared-monitor guardrail spelled out for readers.
    assert!(m.contains("bare-callable"), "MONITOR.md missing bare-callable caveat");
    assert!(m.contains("runtime_monitor"), "MONITOR.md missing C64RE bare-call reference");
}

/// The cockpit README documents the full S1-S7 model: three namespaces, the `!` table,
/// media semantics, filetype colors, and the readline keys + persistent history.
#[test]
fn readme_documents_namespaces_colors_media_and_readline() {
    let r = read_readme();
    assert!(r.contains("Three namespaces"), "README missing namespace header");
    assert!(r.contains("`!`-prefixed"), "README missing !-namespace row");
    assert!(r.contains("cockpit routing layer"), "README missing !-guardrail note");
    assert!(r.contains("Media semantics"), "README missing media-semantics section");
    assert!(r.contains("Filetype colors"), "README missing filetype-colors section");
    // Readline muscles (S6) — each documented key.
    for key in ["Ctrl-A", "Ctrl-E", "Ctrl-K", "Ctrl-U", "Ctrl-W", "Ctrl-L"] {
        assert!(r.contains(key), "README missing readline key {key}");
    }
    assert!(r.contains("~/.trx64/history"), "README missing persistent-history note");
}
