//! candidate.rs — Spec 796 candidate model (store data + pure ops).
//!
//! A candidate is a live, session-lifetime object: a fixed baseline anchor + a
//! bound scenario + an ACCUMULATING overlay patch-set + the cached no-patch
//! baseline run result. The run/eval composition (restore → apply → run_scenario →
//! diff) lives in the daemon dispatch (main.rs) because it needs the live session;
//! this module owns the data model + the pure patch-set ops so they are unit-tested
//! in isolation.

use serde_json::{json, Value};

/// One overlay patch — a source snippet targeted at RAM or a cart bank (Spec 795).
/// `source` (asm, org = addr) is the delta seed; `bytes` (assembled) are applied.
#[derive(Clone)]
pub struct Patch {
    pub space: String, // "ram" | "roml" | "romh"
    pub bank: Option<u16>,
    pub addr: u16,
    pub source: String,
    pub bytes: Vec<u8>,
}

impl Patch {
    /// The identity of a patch target — re-adding at the same key REPLACES.
    pub fn key(&self) -> (String, Option<u16>, u16) {
        (self.space.clone(), self.bank, self.addr)
    }

    pub fn to_json(&self) -> Value {
        json!({
            "space": self.space,
            "bank": self.bank,
            "addr": self.addr,
            "len": self.bytes.len(),
            "source": self.source,
        })
    }
}

/// A live candidate: baseline + scenario + accumulating patches + cached baseline.
#[derive(Clone)]
pub struct Candidate {
    pub id: String,
    pub baseline_anchor: String,
    /// The bound scenario `{inputs, cycleBudget}` — NO startSnapshot (the anchor is
    /// the start; a run restores the anchor then plays these inputs).
    pub scenario: Value,
    pub patches: Vec<Patch>,
    /// The cached end-checkpoint of the NO-PATCH scenario run (the equivalence ref).
    pub baseline_result: Value,
    pub last_verdict: Option<Value>,
}

impl Candidate {
    pub fn new(id: String, baseline_anchor: String, scenario: Value, baseline_result: Value) -> Self {
        Candidate {
            id,
            baseline_anchor,
            scenario,
            patches: vec![],
            baseline_result,
            last_verdict: None,
        }
    }

    /// Add a patch, or REPLACE the existing one at the same (space, bank, addr) —
    /// iterate a fix, never stack duplicates at one target.
    pub fn add_or_replace_patch(&mut self, p: Patch) {
        let k = p.key();
        if let Some(existing) = self.patches.iter_mut().find(|x| x.key() == k) {
            *existing = p;
        } else {
            self.patches.push(p);
        }
    }

    /// Remove the patch at (space, bank, addr). Returns whether one was removed.
    pub fn remove_patch(&mut self, space: &str, bank: Option<u16>, addr: u16) -> bool {
        let before = self.patches.len();
        self.patches.retain(|x| !(x.space == space && x.bank == bank && x.addr == addr));
        self.patches.len() != before
    }

    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "baselineAnchor": self.baseline_anchor,
            "patchCount": self.patches.len(),
            "patches": self.patches.iter().map(|p| p.to_json()).collect::<Vec<_>>(),
            "lastVerdict": self.last_verdict,
        })
    }

    /// The delta seed — the accumulated source-patch-set (the hand-off to #4).
    pub fn export_json(&self) -> Value {
        json!({
            "id": self.id,
            "patches": self.patches.iter().map(|p| json!({
                "space": p.space,
                "bank": p.bank,
                "addr": p.addr,
                "source": p.source,
            })).collect::<Vec<_>>(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch(space: &str, addr: u16, byte: u8) -> Patch {
        Patch { space: space.into(), bank: None, addr, source: format!("lda #${byte:02x}"), bytes: vec![byte] }
    }

    #[test]
    fn add_replaces_at_same_target_not_stacks() {
        let mut c = Candidate::new("cand-1".into(), "anchor".into(), json!({}), json!({}));
        c.add_or_replace_patch(patch("ram", 0x2000, 0x11));
        c.add_or_replace_patch(patch("ram", 0x3000, 0x22)); // different addr → accumulates
        assert_eq!(c.patches.len(), 2);
        c.add_or_replace_patch(patch("ram", 0x2000, 0x99)); // same addr → replaces
        assert_eq!(c.patches.len(), 2);
        let at_2000 = c.patches.iter().find(|p| p.addr == 0x2000).unwrap();
        assert_eq!(at_2000.bytes, vec![0x99]);
    }

    #[test]
    fn remove_and_export() {
        let mut c = Candidate::new("cand-1".into(), "anchor".into(), json!({}), json!({}));
        c.add_or_replace_patch(patch("ram", 0x2000, 0x11));
        c.add_or_replace_patch(patch("roml", 0x8000, 0x22));
        assert!(c.remove_patch("ram", None, 0x2000));
        assert!(!c.remove_patch("ram", None, 0x2000)); // already gone
        assert_eq!(c.patches.len(), 1);
        let ex = c.export_json();
        let ps = ex["patches"].as_array().unwrap();
        assert_eq!(ps.len(), 1);
        assert_eq!(ps[0]["space"], json!("roml"));
        assert_eq!(ps[0]["source"], json!("lda #$22"));
    }
}
