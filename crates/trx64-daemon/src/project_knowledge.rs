//! project_knowledge.rs — which project this daemon serves (Spec 858), and nothing else.
//!
//! This module used to carry a whole knowledge layer: the monitor's `label` / `unlabel` /
//! `note` / `save_labels` / `load_labels` wrote into the C64RE project's `knowledge/`
//! stores, and `sym` / `inspect` / `xref` plus the `resolvePc` / `resolvePcs` WS methods
//! read its `*_analysis.json`, `*_annotations.json` and `*_disasm.asm`. Spec 804 removed
//! all of it (2026-09-19): TRX64 is a runtime — bits and bytes — and holds no symbols.
//! C64RE owns meaning and joins names itself, from the address spans `monitor/exec`
//! returns. Two owners of one store had already broken it: C64RE's 822.2 cut-over moves
//! `labels.user.json` into `knowledge/_legacy-822/`, so a label typed here vanished from
//! this daemon's own `d` on the next C64RE open.
//!
//! What stays is the project BINDING: the runtime still needs to know which directory a
//! relative path resolves against and which project `project/set` moved it to.

/// Spec 858 D1 — the project `project/set` moved this daemon to. `None` until it is
/// called; the startup chain below answers until then.
///
/// Process-wide in the daemon. In the unit tests it is per THREAD: the tests run in
/// parallel threads of one process, and a test that moves the project must not move it
/// under a neighbour that is reading `media/recent` at the same moment.
#[cfg(not(test))]
static PROJECT_OVERRIDE: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);
#[cfg(test)]
thread_local! {
    static PROJECT_OVERRIDE: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

fn project_override() -> Option<String> {
    #[cfg(not(test))]
    return PROJECT_OVERRIDE.read().unwrap_or_else(|e| e.into_inner()).clone();
    #[cfg(test)]
    return PROJECT_OVERRIDE.with(|o| o.borrow().clone());
}

/// Spec 858 D3 — move the daemon to another project. The caller has canonicalised the
/// path and done everything that belongs to leaving the old one.
pub fn set_project_override(path: String) {
    #[cfg(not(test))]
    {
        *PROJECT_OVERRIDE.write().unwrap_or_else(|e| e.into_inner()) = Some(path);
    }
    #[cfg(test)]
    PROJECT_OVERRIDE.with(|o| *o.borrow_mut() = Some(path));
}

/// Spec 858 D1 — the project this daemon serves, or `None` when nothing named one: the
/// runtime override from `project/set`, else `--project <dir>`, else `C64RE_PROJECT_DIR`,
/// empty strings ignored throughout.
///
/// Seven sites used to read `--project` straight out of the process arguments, each with
/// its own copy of this chain and three of them differing in detail. A daemon whose project
/// can change at runtime needs one answer, so they all ask here.
pub fn bound_project() -> Option<String> {
    if let Some(p) = project_override() {
        return Some(p);
    }
    std::env::args()
        .skip_while(|a| a != "--project")
        .nth(1)
        .filter(|p| !p.is_empty())
        .or_else(|| std::env::var("C64RE_PROJECT_DIR").ok().filter(|p| !p.is_empty()))
}

/// The active project dir = `bound_project()` ?? cwd. For callers that RESOLVE paths (the
/// monitor's file shell, media paths); a caller deciding whether it may WRITE into a
/// project asks `bound_project()` instead.
pub fn active_project_dir() -> String {
    bound_project().unwrap_or_else(|| {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default()
    })
}
