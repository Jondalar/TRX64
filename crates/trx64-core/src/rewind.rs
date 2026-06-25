//! rewind.rs — Spec 243 / Spec 769: time-travel branch tracker.
//!
//! 1:1 PORT of the c64re TS
//!   C64ReverseEngineeringMCP/src/runtime/headless/v2/rewind.ts
//! (class `RewindManager`), carrying the SAME branch-tree MODEL + METADATA:
//!   - the root snapshot + root branch captured at construction,
//!   - the `SnapshotBranch` public view (id/parentId/rootId/atCycle/patches/
//!     startSnapshotId/endCycle/endSnapshotId/resultHash/children),
//!   - the `RewindHandle` view (scenarioId/rootSnapshotId/rootBranchId/branches/
//!     ringSize) returned by `handle()`,
//!   - `DEFAULT_RING_SIZE = 32`,
//!   - `promoteBranch(branchId)` → { scenarioId, scenario, patches } and its
//!     throw-on-unknown-branch behaviour.
//!
//! WHAT DIFFERS FROM THE TS (and why it is still 1:1 on the OBSERVABLE contract):
//!   The WS handlers `runtime/snapshot_tree` and `runtime/promote_branch`
//!   (workspace-ui/ws-server.ts:1891 / :1911) construct a FRESH RewindManager via
//!   `api.beginRewindSession()` on EVERY call — there is NO persistent per-session
//!   RewindManager held across calls. So the OBSERVABLE result of those two handlers
//!   depends only on construction (the root snapshot + root branch) and the supplied
//!   `branch_id`. This port reproduces exactly that:
//!     • `snapshot_tree` → a freshly-built handle with the single root branch,
//!     • `promote_branch` → succeeds ONLY if `branch_id` is the freshly-generated
//!       root id (which a caller cannot know in advance, the same as TS where each
//!       call mints a new `randomUUID()` root), else throws "branch <id> not found"
//!       exactly like `RewindManager.promoteBranch`.
//!   The snapshot bytes / disk-image VSF write are construction-internal storage
//!   details of the TS side; the OBSERVABLE wire shape (the handle + the promote
//!   result/error) is what is reproduced here. The cycle anchor (`atCycle`/
//!   `endCycle`) is taken from the live machine clock, as in the TS ctor
//!   (`this.session.c64Cpu.cycles`).

use serde_json::{json, Value};

/// rewind.ts:67 — `DEFAULT_RING_SIZE = 32`.
pub const DEFAULT_RING_SIZE: u64 = 32;

/// rewind.ts:38-50 — `SnapshotBranch`. Field names mirror the TS interface
/// verbatim; the JSON wire shape (camelCase) is what the WS handler serializes
/// (`for (const [k, v] of handle.branches) branches[k] = v`).
#[derive(Debug, Clone)]
pub struct SnapshotBranch {
    pub id: String,
    /// Undefined on the root branch (omitted from JSON when None, like TS).
    pub parent_id: Option<String>,
    pub root_id: Option<String>,
    pub at_cycle: u64,
    /// PokePatch[]; the root branch has [].
    pub patches: Vec<Value>,
    pub start_snapshot_id: String,
    pub end_cycle: Option<u64>,
    pub end_snapshot_id: Option<String>,
    pub result_hash: Option<String>,
    pub children: Vec<String>,
}

impl SnapshotBranch {
    /// JSON wire shape used by `runtime/snapshot_tree` — matches the TS
    /// `SnapshotBranch` field names (camelCase). `undefined` fields are omitted
    /// (serde skips None via explicit construction here) exactly like a TS object
    /// literal whose property is `undefined`.
    pub fn to_json(&self) -> Value {
        let mut m = serde_json::Map::new();
        m.insert("id".into(), json!(self.id));
        if let Some(p) = &self.parent_id {
            m.insert("parentId".into(), json!(p));
        }
        if let Some(r) = &self.root_id {
            m.insert("rootId".into(), json!(r));
        }
        m.insert("atCycle".into(), json!(self.at_cycle));
        m.insert("patches".into(), json!(self.patches));
        m.insert("startSnapshotId".into(), json!(self.start_snapshot_id));
        if let Some(e) = self.end_cycle {
            m.insert("endCycle".into(), json!(e));
        }
        if let Some(e) = &self.end_snapshot_id {
            m.insert("endSnapshotId".into(), json!(e));
        }
        if let Some(h) = &self.result_hash {
            m.insert("resultHash".into(), json!(h));
        }
        m.insert("children".into(), json!(self.children));
        Value::Object(m)
    }
}

/// rewind.ts:57-63 — `RewindHandle` returned by `handle()`.
pub struct RewindHandle {
    pub scenario_id: String,
    pub root_snapshot_id: String,
    pub root_branch_id: String,
    /// rewind.ts uses a `Map<BranchId, SnapshotBranch>`; an insertion-ordered
    /// Vec of (id, branch) reproduces the same iteration the WS handler does.
    pub branches: Vec<(String, SnapshotBranch)>,
    pub ring_size: u64,
}

impl RewindHandle {
    /// JSON wire shape used by `runtime/snapshot_tree` (ws-server.ts:1900-1907).
    pub fn to_json(&self) -> Value {
        let mut branches = serde_json::Map::new();
        for (k, v) in &self.branches {
            branches.insert(k.clone(), v.to_json());
        }
        json!({
            "scenarioId": self.scenario_id,
            "rootBranchId": self.root_branch_id,
            "rootSnapshotId": self.root_snapshot_id,
            "ringSize": self.ring_size,
            "branches": Value::Object(branches),
        })
    }
}

/// rewind.ts:78-111 — `RewindManager` (construction + handle + promoteBranch).
///
/// The TS ctor captures the root snapshot, mints a root branch id, and stores the
/// single root branch. We carry only the OBSERVABLE state needed by the two WS
/// handlers: the ids, the ring size, and the branch map.
pub struct RewindManager {
    scenario_id: String,
    disk_path: String,
    root_snapshot_id: String,
    root_branch_id: String,
    ring_size: u64,
    at_cycle: u64,
    branches: Vec<(String, SnapshotBranch)>,
}

impl RewindManager {
    /// rewind.ts:90-111 — ctor. `at_cycle` = the live machine clock at construction
    /// (`this.session.c64Cpu.cycles`). `ring_size` defaults to `DEFAULT_RING_SIZE`.
    pub fn new(scenario_id: &str, disk_path: &str, at_cycle: u64, ring_size: Option<u64>) -> Self {
        let ring_size = ring_size.unwrap_or(DEFAULT_RING_SIZE);
        // rewind.ts:98 — root snapshot stored; :99 — root branch id = randomUUID().
        let root_snapshot_id = new_uuid();
        let root_branch_id = new_uuid();
        let root_branch = SnapshotBranch {
            id: root_branch_id.clone(),
            parent_id: None,
            // rewind.ts:109 — rootBranch.rootId = this.rootBranchId.
            root_id: Some(root_branch_id.clone()),
            at_cycle,
            patches: vec![],
            start_snapshot_id: root_snapshot_id.clone(),
            end_cycle: Some(at_cycle),
            end_snapshot_id: Some(root_snapshot_id.clone()),
            result_hash: None,
            children: vec![],
        };
        RewindManager {
            scenario_id: scenario_id.to_string(),
            disk_path: disk_path.to_string(),
            root_snapshot_id,
            root_branch_id: root_branch_id.clone(),
            ring_size,
            at_cycle,
            branches: vec![(root_branch_id, root_branch)],
        }
    }

    /// rewind.ts:113-120 — handle().
    pub fn handle(&self) -> RewindHandle {
        RewindHandle {
            scenario_id: self.scenario_id.clone(),
            root_snapshot_id: self.root_snapshot_id.clone(),
            root_branch_id: self.root_branch_id.clone(),
            branches: self.branches.clone(),
            ring_size: self.ring_size,
        }
    }

    fn find_branch(&self, branch_id: &str) -> Option<&SnapshotBranch> {
        self.branches.iter().find(|(k, _)| k == branch_id).map(|(_, b)| b)
    }

    /// rewind.ts:250-269 — promoteBranch(branchId). On a freshly-constructed
    /// manager only the root branch exists, so this returns the promoted Scenario
    /// only for the root id; any other id yields the same "not found" error the TS
    /// throws (`promoteBranch: branch <id> not found`).
    ///
    /// `mode` is the ScenarioMode string (the WS handler always passes
    /// "true-drive" — Spec 723.2: never bake a fast-trap scenario into a branch).
    pub fn promote_branch(&self, branch_id: &str, mode: &str) -> Result<Value, String> {
        let branch = self
            .find_branch(branch_id)
            .ok_or_else(|| format!("promoteBranch: branch {branch_id} not found"))?;
        // rewind.ts:253-254 — start snapshot must exist. (On a fresh manager the
        // root start snapshot always exists, so this never trips here; kept for
        // 1:1 fidelity with the TS guard.)
        // rewind.ts:257 — newScenarioId = `${scenarioId}-branch-${branchId.slice(0,8)}`.
        let short: String = branch_id.chars().take(8).collect();
        let new_scenario_id = format!("{}-branch-{}", self.scenario_id, short);
        // rewind.ts:259-261 — start snapshot bytes written to a tmp VSF; the
        // Scenario.startSnapshot points to that path. (Path is construction-
        // internal; reproduced with the same dir/name scheme.)
        let tmp_dir = std::env::temp_dir().join("c64re-rewind-promote");
        let _ = std::fs::create_dir_all(&tmp_dir);
        let path = tmp_dir.join(format!("{new_scenario_id}.vsf"));
        // rewind.ts:262-268 — Scenario record. cycleBudget = endCycle - atCycle
        // (0 for the root branch, where endCycle == atCycle).
        let cycle_budget = branch
            .end_cycle
            .map(|e| e.saturating_sub(branch.at_cycle))
            .unwrap_or(0);
        let scenario = json!({
            "id": new_scenario_id,
            "startSnapshot": path.to_string_lossy(),
            "inputs": [],
            "cycleBudget": cycle_budget,
            "diskPath": self.disk_path,
            "mode": mode,
        });
        // rewind.ts:268 — return { scenarioId, scenario, patches: [...branch.patches] }.
        Ok(json!({
            "scenarioId": new_scenario_id,
            "scenario": scenario,
            "patches": branch.patches.clone(),
        }))
    }

    pub fn at_cycle(&self) -> u64 {
        self.at_cycle
    }
}

/// A UUID-v4-shaped id, matching the SHAPE of node's `randomUUID()` used by
/// rewind.ts (the actual value is non-deterministic per call in TS too). No
/// external crate: seeded from a monotonic counter + wall-clock nanos, mixed
/// with a small xorshift to fill the 122 random bits.
fn new_uuid() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut s = nanos ^ (c.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let mut next = || {
        // xorshift64
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let a = next();
    let b = next();
    // Lay out as 8-4-4-4-12 hex with version (4) + variant (8..b) nibbles set.
    let p0 = (a >> 32) as u32;
    let p1 = (a >> 16) as u16;
    let p2 = ((a & 0xffff) as u16 & 0x0fff) | 0x4000; // version 4
    let p3 = ((b >> 48) as u16 & 0x3fff) | 0x8000; // variant
    let p4 = b & 0xffff_ffff_ffff;
    format!("{p0:08x}-{p1:04x}-{p2:04x}-{p3:04x}-{p4:012x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_handle_has_single_root_branch() {
        let rm = RewindManager::new("sess-1", "/disks/x.d64", 12345, None);
        let h = rm.handle();
        assert_eq!(h.scenario_id, "sess-1");
        assert_eq!(h.ring_size, DEFAULT_RING_SIZE);
        assert_eq!(h.branches.len(), 1);
        let (k, root) = &h.branches[0];
        assert_eq!(*k, h.root_branch_id);
        assert_eq!(root.id, h.root_branch_id);
        assert_eq!(root.root_id.as_deref(), Some(h.root_branch_id.as_str()));
        assert!(root.parent_id.is_none());
        assert_eq!(root.at_cycle, 12345);
        assert_eq!(root.start_snapshot_id, h.root_snapshot_id);
    }

    #[test]
    fn snapshot_tree_json_shape() {
        let rm = RewindManager::new("sess-1", "/disks/x.d64", 7, None);
        let v = rm.handle().to_json();
        assert_eq!(v["scenarioId"], json!("sess-1"));
        assert_eq!(v["ringSize"], json!(32));
        let branches = v["branches"].as_object().unwrap();
        assert_eq!(branches.len(), 1);
        let root_id = v["rootBranchId"].as_str().unwrap();
        let root = &branches[root_id];
        assert_eq!(root["id"], json!(root_id));
        assert!(root["parentId"].is_null()); // omitted → null on lookup
        assert_eq!(root["patches"], json!([]));
        assert_eq!(root["children"], json!([]));
    }

    #[test]
    fn promote_unknown_branch_errors_like_ts() {
        let rm = RewindManager::new("sess-1", "/disks/x.d64", 0, None);
        let e = rm.promote_branch("nope-not-a-real-id", "true-drive").unwrap_err();
        assert!(e.contains("not found"), "got: {e}");
    }

    #[test]
    fn promote_root_branch_returns_scenario() {
        let rm = RewindManager::new("sess-1", "/disks/x.d64", 100, None);
        let root = rm.handle().root_branch_id;
        let v = rm.promote_branch(&root, "true-drive").unwrap();
        let sid = v["scenarioId"].as_str().unwrap();
        assert!(sid.starts_with("sess-1-branch-"));
        assert_eq!(v["scenario"]["mode"], json!("true-drive"));
        assert_eq!(v["scenario"]["diskPath"], json!("/disks/x.d64"));
        assert_eq!(v["scenario"]["cycleBudget"], json!(0)); // root: endCycle==atCycle
        assert_eq!(v["patches"], json!([]));
    }

    #[test]
    fn uuids_are_distinct_and_shaped() {
        let a = new_uuid();
        let b = new_uuid();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(a.as_bytes()[14], b'4'); // version nibble
    }
}
