//! checkpoint_ring.rs — Spec 705.B: always-on bounded in-memory checkpoint ring.
//!
//! 1:1 PORT of the c64re TS
//!   C64ReverseEngineeringMCP/src/runtime/headless/kernel/runtime-checkpoint-ring.ts
//! (class `RuntimeCheckpointRing`), carrying the SAME ring POLICY and METADATA:
//!   - the `RuntimeCheckpointRef` public view (id/frame/cycles/pinned/byteSize/createdAtMs),
//!   - the `cp_{frame}_{seq}` id scheme (`seq` is a per-ring monotonic counter),
//!   - capacity = floor(budgetBytes / SLOT_BYTES) slots (the 32 MiB / 64 KiB default ≈ 512),
//!   - round-robin eviction of the OLDEST UNPINNED entry when full (pins are exempt),
//!   - `pin`/`unpin`, `truncateAfter` (Spec 761 new-timeline cut, keep-pinned),
//!   - the content-addressed disk-image pool (Spec 714.4) — identical disk versions
//!     stored ONCE, refcounted across entries, released at zero on eviction.
//!
//! WHAT DIFFERS FROM THE TS (and why it is still 1:1 on the OBSERVABLE contract):
//!   The TS ring's Spec 765 flat `ArrayBuffer` slab is a V8-GC-churn fix (BUG-049):
//!   it packs the big typed-array buffers (RAM) into one pre-allocated slab so the
//!   old-gen object graph never grows. Rust has no GC and no such churn, and the
//!   TRX64 RuntimeCheckpoint is a `serde_json::Value` tree (ADR-077/078) — not a
//!   typed-array payload. So this port originally stored the FULL checkpoint `Value`
//!   per entry. `SLOT_BYTES`/capacity/eviction/stats are reproduced so the ring's
//!   OBSERVABLE policy (how many entries fit, which gets evicted, the stats numbers)
//!   matches. The disk-image pool IS ported (it changes which entries share bytes).
//!
//!   Spec 807 revisited that storage decision with a measurement rather than an
//!   argument. Rust has no GC, but `serde_json::Value` has its own tax: a live tree
//!   cost **11× its rendered size** (111 KiB resident for 9.9 KiB of chip state), and
//!   a ring of 500 entries reported 32 MiB while holding 102. So an entry now keeps
//!   its big buffers RAW (`raw`, `disk_pool`) and its residual tree as COMPACT BYTES,
//!   rebuilt into a `Value` only in `restore_snapshot`. Same tree out, ~2.6× less
//!   resident: capture runs 50×/s at cadence 1, restore runs when a human clicks.
//!
//! TRANSIENT: in-memory only. NOT persistence (Spec 707 `.c64re` dump does that).
//! Zero-cost when unused: an empty ring holds an empty Vec — no slab, no allocation
//! until the first `capture`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// runtime-checkpoint-ring.ts:78 — `RAM_BYTES = 0x10000` (64 KiB).
const RAM_BYTES: u64 = 0x10000;

/// runtime-checkpoint-ring.ts:91 — `SLOT_BYTES = RAM_BYTES` (RAM only; the two VIC
/// framebuffers are derivable shadows the perma-anchor omits — Spec 765 §8).
pub const SLOT_BYTES: u64 = RAM_BYTES; // 65536

/// runtime-checkpoint-ring.ts:79 — `FB_BYTES = 65*8*312` (a present-capture framebuffer).
const FB_BYTES: u64 = 65 * 8 * 312; // 162240

/// runtime-checkpoint-ring.ts:136 — `DEFAULT_CHECKPOINT_RING_BUDGET_BYTES = 32 MiB`
/// → ~512 slots at SLOT_BYTES = 64 KiB. Spec 772: this is now the SECONDARY bound —
/// the ring is sized for the UI scrub-filmstrip by the max-entries cap below; the
/// byte budget is the safety ceiling. Eviction fires on whichever bound hits first.
pub const DEFAULT_CHECKPOINT_RING_BUDGET_BYTES: u64 = 32 * 1024 * 1024;

/// Spec 772 — default ring retention in seconds (the UI scrub window). 1:1 with the
/// c64re `DEFAULT_CHECKPOINT_RING_SECONDS` (runtime-checkpoint-ring.ts).
pub const DEFAULT_CHECKPOINT_RING_SECONDS: f64 = 60.0;

/// Measured resident cost of one anchor after Spec 807 (~98 KiB). Used to size the byte
/// budget from the requested window instead of pinning it at 32 MiB — that constant was
/// 512 slots, which happened to be just over the 500 a ten-second window needs, so
/// nothing noticed. Ask for sixty seconds and the ring would have evicted at 512 and
/// silently kept ten.
pub const ANCHOR_BYTES_ESTIMATE: u64 = 100 * 1024;

/// The byte budget a window of `max_entries` anchors actually needs, with a little head
/// room. The budget is a SAFETY CEILING, so it must never be the thing that decides the
/// window — the entry cap is.
pub fn checkpoint_ring_budget_for(max_entries: u64) -> u64 {
    (max_entries.max(1) + 16) * ANCHOR_BYTES_ESTIMATE
}

/// Spec 772 — max LIVE entries the ring retains = `ceil(seconds / (cadenceFrames/50))`
/// (PAL 50 fps). At the 10s / 25-frame default that is **20**. Clamped ≥ 1. 1:1 with
/// the c64re `checkpointRingMaxEntries` (runtime-checkpoint-ring.ts).
pub fn checkpoint_ring_max_entries(seconds: f64, cadence_frames: u64) -> u64 {
    let sec = if seconds.is_finite() && seconds > 0.0 {
        seconds
    } else {
        DEFAULT_CHECKPOINT_RING_SECONDS
    };
    let cad = if cadence_frames >= 1 { cadence_frames } else { 25 } as f64;
    let seconds_per_capture = cad / 50.0; // PAL 50fps
    ((sec / seconds_per_capture).ceil() as u64).max(1)
}

/// runtime-checkpoint-ring.ts:66 — top-level payload fields holding large mutable
/// media blobs, content-addressed into the pool on capture, rehydrated on restore.
/// Order is irrelevant. (TRX64's checkpoint tree carries `driveDiskImage`,
/// `cartBytes`, `cartFlash` exactly — c64re_snapshot.rs:899-902.)
///
/// `_ringDriveDiskBytes` is a TRX64-private extra slot: the in-memory ring carries
/// no container `mediaPayloads` (unlike the `.c64re` dump), so the clean disk image
/// needed to RE-ATTACH the drive on restore rides the checkpoint tree itself. It is
/// pooled here exactly like the c64re media slots so identical disks across
/// checkpoints are stored once (an unchanged disk over a rewind ring = one copy).
const POOLED_BLOB_SLOTS: [&str; 4] =
    ["driveDiskImage", "cartBytes", "cartFlash", "_ringDriveDiskBytes"];

/// runtime-checkpoint-ring.ts:46-58 — public, payload-free view of a ring entry.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeCheckpointRef {
    pub id: String,
    /// Controller frame counter at capture.
    pub frame: u64,
    /// CPU cycle count at capture.
    pub cycles: u64,
    /// Pinned entries are exempt from eviction.
    pub pinned: bool,
    /// Estimated retained bytes (the fixed slot size + small wrapper + any framebuffers).
    pub byte_size: u64,
    /// Wall-clock capture time (ms since epoch).
    pub created_at_ms: u64,
}

impl RuntimeCheckpointRef {
    /// JSON wire shape used by the `checkpoint/*` WS replies — matches the TS
    /// `RuntimeCheckpointRef` field names (camelCase).
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "id": self.id,
            "frame": self.frame,
            "cycles": self.cycles,
            "pinned": self.pinned,
            "byteSize": self.byte_size,
            "createdAtMs": self.created_at_ms,
        })
    }
}

/// runtime-checkpoint-ring.ts:94-107 — the stored ring entry (ref + payload).
struct RingEntry {
    r: RuntimeCheckpointRef,
    /// The residual RuntimeCheckpoint tree — pooled media slots NULLED (their bytes
    /// live once in `disk_pool`, keyed by `blob_hashes`), `RAW_SLOTS` blobs moved out
    /// to `raw` — held as **compact JSON bytes, not a live `Value`** (Spec 807 §4.2).
    ///
    /// This is the single biggest line item in a ring entry, and it was invisible.
    /// Measured (`perf_bench bench_checkpoint_entry_cost_split`, 500 clones each):
    ///
    /// ```text
    ///   raw RAM per entry                64.2 KiB   (irreducible — it is the state)
    ///   residual tree as a live Value   111.1 KiB
    ///   the same tree, rendered           9.9 KiB   → the Value costs 11× its content
    /// ```
    ///
    /// A `serde_json::Value` pays for every node: a `Map` with `String` keys, an
    /// enum per scalar, a separate allocation per string. Ten kilobytes of chip state
    /// become a hundred. Holding the bytes instead costs what the bytes cost, and
    /// `serde_json::from_slice` rebuilds the identical tree in `restore_snapshot`.
    ///
    /// Bytes rather than a modelled struct ON PURPOSE: the payload carries fields no
    /// struct here owns (`_ringDriveDiskBytes`, `vicProvenance`, `media`, `audio`,
    /// `alarmsMaincpu`, `cartState`, the joystick/paddle blocks, and whatever a future
    /// spec adds). Modelling them would make the ring a second schema that has to be
    /// kept in step with `c64re_snapshot`, and the first field anyone forgot would be
    /// silently dropped on restore. Round-tripping the bytes cannot lose a field.
    payload: Vec<u8>,
    /// runtime-checkpoint-ring.ts:106 — pooled-slot → content hash for each blob.
    blob_hashes: HashMap<String, String>,
    /// Spec 807 §4.3 — the big base64 blobs that are NOT media, held as raw bytes.
    ///
    /// Measured: a lean entry cost **208.9 KiB resident** while the ring accounted
    /// 64.0 KiB, because a live `serde_json::Value` costs ~2.2× its rendered form and
    /// the 64 KiB of RAM lived inside it as an 87 KiB base64 `String`. Moving those
    /// bytes out of the tree is where the ring's memory goes back.
    ///
    /// Deliberately NOT content-addressed like `disk_pool`: RAM differs on essentially
    /// every frame, so a sha256 over 64 KiB per capture would cost more than the whole
    /// capture and never hit. Media is the opposite (unchanged across a whole ring) and
    /// stays pooled.
    raw: Vec<(&'static str, Vec<u8>)>,
}

/// Spec 807 §4.3 — payload paths whose `{ $ta }` blob is moved out of the JSON tree
/// into `RingEntry::raw`. Dotted paths are resolved one level deep.
///
/// `ram` is always present and is the whole point. The two framebuffers are present
/// only on an explicit full `checkpoint/capture` (the per-frame producers omit them
/// entirely since Spec 807 §4.1) — but when they are there they are 216 KiB each, so
/// they are worth the same treatment.
const RAW_SLOTS: [&str; 3] = [
    "ram",
    "vicPresentation.literalPortFb",
    "vicPresentation.literalPortFbStable",
];

/// Read a `RAW_SLOTS` path out of a payload tree (one level of nesting).
fn raw_slot_get<'a>(payload: &'a Value, path: &str) -> Option<&'a Value> {
    match path.split_once('.') {
        Some((parent, child)) => payload.get(parent)?.get(child),
        None => payload.get(path),
    }
}

/// Write a `RAW_SLOTS` path into a payload tree (one level of nesting). No-op when the
/// parent object is absent, which is how a checkpoint without `vicPresentation` stays
/// legal.
fn raw_slot_set(payload: &mut Value, path: &str, v: Value) {
    match path.split_once('.') {
        Some((parent, child)) => {
            if let Some(p) = payload.get_mut(parent) {
                if p.is_object() {
                    p[child] = v;
                }
            }
        }
        None => payload[path] = v,
    }
}

/// runtime-checkpoint-ring.ts:114-129 — ring telemetry.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeCheckpointRingStats {
    pub count: u64,
    pub pinned_count: u64,
    pub total_bytes: u64,
    pub budget_bytes: u64,
    pub oldest_frame: Option<u64>,
    pub newest_frame: Option<u64>,
    pub disk_image_versions: u64,
    pub disk_pool_bytes: u64,
    pub slot_bytes: u64,
    pub slot_count: u64,
    pub free_slots: u64,
}

impl RuntimeCheckpointRingStats {
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "count": self.count,
            "pinnedCount": self.pinned_count,
            "totalBytes": self.total_bytes,
            "budgetBytes": self.budget_bytes,
            "oldestFrame": self.oldest_frame,
            "newestFrame": self.newest_frame,
            "diskImageVersions": self.disk_image_versions,
            "diskPoolBytes": self.disk_pool_bytes,
            "slotBytes": self.slot_bytes,
            "slotCount": self.slot_count,
            "freeSlots": self.free_slots,
        })
    }
}

struct PooledBlob {
    bytes: Vec<u8>,
    refs: u64,
}

/// runtime-checkpoint-ring.ts:147 — `class RuntimeCheckpointRing`.
pub struct RuntimeCheckpointRing {
    budget_bytes: u64,
    /// Spec 772 — max LIVE entries (the UI-scrub cap), or `u64::MAX` = byte-budget only.
    max_entries: u64,
    slot_count: u64,
    /// runtime-checkpoint-ring.ts:154 — free slot indices (LIFO). Tracked for the
    /// stats/capacity policy only (TRX64 has no slab to index into).
    free_slots: Vec<u64>,
    /// runtime-checkpoint-ring.ts:155 — oldest first.
    entries: Vec<RingEntry>,
    /// runtime-checkpoint-ring.ts:156 — per-ring monotonic id counter.
    seq: u64,
    /// runtime-checkpoint-ring.ts:161 — content-addressed disk-image pool.
    disk_pool: HashMap<String, PooledBlob>,
    disk_pool_bytes: u64,
}

impl RuntimeCheckpointRing {
    /// runtime-checkpoint-ring.ts:164-169 — `slotCount = max(1, floor(budget/slot))`,
    /// free slots seeded `slotCount-1..=0` (LIFO). Byte-budget bound only (no entry cap).
    pub fn with_budget(budget_bytes: u64) -> Self {
        Self::with_budget_and_max_entries(budget_bytes, u64::MAX)
    }

    /// Spec 772 — like `with_budget` but with a max LIVE entries cap (the UI-scrub
    /// filmstrip size). Eviction fires on WHICHEVER bound (byte budget OR entry cap)
    /// is hit first (oldest-unpinned, pin-exempt). `max_entries == u64::MAX` = no cap.
    pub fn with_budget_and_max_entries(budget_bytes: u64, max_entries: u64) -> Self {
        let slot_count = (budget_bytes / SLOT_BYTES).max(1);
        let mut free_slots = Vec::with_capacity(slot_count as usize);
        let mut i = slot_count;
        while i > 0 {
            i -= 1;
            free_slots.push(i);
        }
        Self {
            budget_bytes,
            max_entries: max_entries.max(1),
            slot_count,
            free_slots,
            entries: Vec::new(),
            seq: 0,
            disk_pool: HashMap::new(),
            disk_pool_bytes: 0,
        }
    }

    pub fn new() -> Self {
        Self::with_budget(DEFAULT_CHECKPOINT_RING_BUDGET_BYTES)
    }

    /// Spec 807 §4.6 — retune the live-entry cap without discarding the ring.
    ///
    /// The cap is `ceil(seconds / (cadence/50))`, so changing the capture cadence
    /// changes it. Before this existed, `checkpoint_ring_max_entries()` was evaluated
    /// ONCE at daemon start while `checkpoint_capture_every_frames()` was read per
    /// frame — so a cadence change could not move the cap, and cadence 1 into a cap of
    /// 20 would have silently shrunk a 10-second window to 0.4 s with nothing turning
    /// red. Lowering the cap evicts oldest-unpinned immediately, exactly as a capture
    /// would; raising it just makes room.
    pub fn set_max_entries(&mut self, max_entries: u64) -> u64 {
        self.max_entries = max_entries.max(1);
        // Grow the byte bound with the entry cap. Eviction fires on whichever bound hits
        // first, so leaving the budget behind would mean asking for sixty seconds and
        // quietly getting ten — the cap says 3000, the slab says 512, and the slab wins.
        let want_budget = checkpoint_ring_budget_for(self.max_entries);
        if want_budget > self.budget_bytes {
            let extra = (want_budget - self.budget_bytes) / SLOT_BYTES;
            let base = self.slot_count;
            for i in 0..extra {
                self.free_slots.push(base + i);
            }
            self.slot_count += extra;
            self.budget_bytes = want_budget;
        }
        while (self.entries.len() as u64) > self.max_entries {
            match self.entries.iter().position(|e| !e.r.pinned) {
                Some(i) => self.remove_entry_at(i),
                None => break, // all pinned — the pins win, as everywhere else
            }
        }
        self.max_entries
    }

    /// The current live-entry cap.
    pub fn max_entries(&self) -> u64 {
        self.max_entries
    }

    pub fn budget_bytes(&self) -> u64 {
        self.budget_bytes
    }

    /// runtime-checkpoint-ring.ts:210-266 — `capture(snapshot, frame, cycles)`.
    ///
    /// Validates the big buffers, acquires a slot (evicting the oldest UNPINNED
    /// entry when full), extracts pooled media blobs into the content-addressed
    /// pool (nulling them in the stored payload), assigns the `cp_{frame}_{seq}`
    /// id, and appends. `payload` is the FULL RuntimeCheckpoint tree from
    /// `capture_runtime_checkpoint`.
    ///
    /// Returns Err (a "dropped checkpoint" / ring gap, never a crash for the
    /// caller) when the RAM buffer is the wrong size, a framebuffer is the wrong
    /// size, or the ring is full and every entry is pinned.
    pub fn capture(
        &mut self,
        mut payload: Value,
        frame: u64,
        cycles: u64,
    ) -> Result<RuntimeCheckpointRef, String> {
        // Validate the big buffers up front (before mutating any ring state).
        // The TRX64 checkpoint stores `ram` as a `{ $ta }`-tagged base64 blob;
        // decode just the length to validate (cheap; ts validates `ram.length`).
        let ram_len = ta_len(payload.get("ram"));
        match ram_len {
            Some(n) if n as u64 == RAM_BYTES => {}
            other => {
                return Err(format!(
                    "[checkpoint] capture: RAM must be {RAM_BYTES} bytes, got {:?}",
                    other
                ));
            }
        }
        // Framebuffers (optional): null on the perma-anchor. Validate size when present.
        let vp = payload.get("vicPresentation");
        let fb_len = vp.and_then(|v| ta_len(v.get("literalPortFb")));
        let fb_stable_len = vp.and_then(|v| ta_len(v.get("literalPortFbStable")));
        if let Some(n) = fb_len {
            if n as u64 != FB_BYTES {
                return Err(format!(
                    "[checkpoint] capture: literalPortFb must be {FB_BYTES} bytes, got {n}"
                ));
            }
        }
        if let Some(n) = fb_stable_len {
            if n as u64 != FB_BYTES {
                return Err(format!(
                    "[checkpoint] capture: literalPortFbStable must be {FB_BYTES} bytes, got {n}"
                ));
            }
        }

        // Acquire a slot (evict the oldest unpinned entry if full). This may fail
        // when every entry is pinned — error out before mutating the payload/pool.
        let slot_idx = self.acquire_slot()?;

        // Extract pooled media blobs (content-addressed dedup). For each pooled
        // slot holding a non-empty `{ $ta }` blob: hash it, refcount/insert into
        // the pool, store the hash, and NULL the slot in the stored payload.
        let mut blob_hashes: HashMap<String, String> = HashMap::new();
        for slot in POOLED_BLOB_SLOTS {
            if let Some(bytes) = ta_decode(payload.get(slot)) {
                if !bytes.is_empty() {
                    let hash = sha256_hex(&bytes);
                    match self.disk_pool.get_mut(&hash) {
                        Some(p) => p.refs += 1,
                        None => {
                            let len = bytes.len() as u64;
                            self.disk_pool.insert(hash.clone(), PooledBlob { bytes, refs: 1 });
                            self.disk_pool_bytes += len;
                        }
                    }
                    blob_hashes.insert(slot.to_string(), hash);
                    payload[slot] = Value::Null; // stored entry keeps only the hash ref
                }
            }
        }

        // Spec 807 §4.3 — move the big non-media base64 blobs OUT of the JSON tree and
        // hold them as raw bytes. `restore_snapshot` re-encodes them, so every consumer
        // still sees the identical tree; only the resident cost changes.
        let mut raw: Vec<(&'static str, Vec<u8>)> = Vec::new();
        for path in RAW_SLOTS {
            if let Some(bytes) = ta_decode(raw_slot_get(&payload, path)) {
                if !bytes.is_empty() {
                    raw.push((path, bytes));
                    raw_slot_set(&mut payload, path, Value::Null);
                }
            }
        }

        // Spec 807 §4.5 — account what the entry actually costs. The old figure was
        // `SLOT_BYTES` (+ FB_BYTES per framebuffer), i.e. the RAW size of the payload's
        // big buffers — but the entry held them base64'd inside a `Value`, so a lean
        // entry really cost 208.9 KiB against 64.0 KiB accounted (3.3× under-report,
        // `perf_bench bench_checkpoint_ring_resident_memory`). Now the big buffers ARE
        // raw, so their real cost is their length; the remaining tree is charged a
        // measured constant.
        let raw_bytes: u64 = raw.iter().map(|(_, b)| b.len() as u64).sum();
        // Spec 807 §4.2 — the residual tree is stored as compact JSON bytes, so its
        // contribution is a MEASURED length rather than the `RESIDUAL_TREE_BYTES`
        // estimate the previous step needed.
        let payload_bytes = serde_json::to_vec(&payload)
            .map_err(|e| format!("[checkpoint] capture: encode payload: {e}"))?;
        let byte_size = raw_bytes + payload_bytes.len() as u64;
        let id = format!("cp_{frame}_{}", self.seq);
        self.seq += 1;
        let r = RuntimeCheckpointRef {
            id,
            frame,
            cycles,
            pinned: false,
            byte_size,
            created_at_ms: now_ms(),
        };
        // `slot_idx` is consumed by appending the entry; the slab index itself is
        // not retained (TRX64 stores the payload directly), but we keep slot
        // accounting (free_slots) consistent with the TS for the stats contract.
        let _ = slot_idx;
        let _ = (fb_len, fb_stable_len); // validated above; no longer part of the size
        self.entries.push(RingEntry {
            r: r.clone(),
            payload: payload_bytes,
            blob_hashes,
            raw,
        });
        Ok(r)
    }

    /// runtime-checkpoint-ring.ts:269-280 (Spec 772) — get a free slot, evicting the
    /// oldest UNPINNED entry on WHICHEVER bound is hit first: the byte-budget slot
    /// count OR the max-entries cap. The cap keeps the LIVE entry count ≤ max_entries
    /// after this capture (the ring is the short UI-scrub buffer; deep history = the
    /// recorder). Pinned entries are exempt from both bounds.
    fn acquire_slot(&mut self) -> Result<u64, String> {
        // Spec 772 — entry-count cap: evict oldest-unpinned until adding one more keeps
        // the live count ≤ max_entries. (No-op when max_entries == u64::MAX.)
        while (self.entries.len() as u64) + 1 > self.max_entries {
            match self.entries.iter().position(|e| !e.r.pinned) {
                Some(i) => self.remove_entry_at(i),
                None => break, // all remaining entries pinned — let the byte budget decide
            }
        }
        // Byte-budget bound: a full slab evicts the oldest unpinned entry to free a slot.
        if self.free_slots.is_empty() {
            let idx = self.entries.iter().position(|e| !e.r.pinned);
            match idx {
                Some(i) => self.remove_entry_at(i),
                None => {
                    return Err(format!(
                        "[checkpoint] ring full: all {} slots pinned — cannot capture without evicting a pinned anchor",
                        self.slot_count
                    ));
                }
            }
        }
        Ok(self.free_slots.pop().unwrap())
    }

    /// runtime-checkpoint-ring.ts:283-288 — remove entry `idx`, release its slot +
    /// pooled disk refs.
    fn remove_entry_at(&mut self, idx: usize) {
        if idx >= self.entries.len() {
            return;
        }
        let gone = self.entries.remove(idx);
        // The freed slot returns to the pool (LIFO); we mint a fresh index by the
        // current free-list length to keep `free_slots.len()` == available slots.
        // (The exact slab index is irrelevant in TRX64 — only the count is observed.)
        self.free_slots.push(self.free_slots.len() as u64);
        for hash in gone.blob_hashes.values() {
            self.release_disk_image(Some(hash));
        }
    }

    /// runtime-checkpoint-ring.ts:291-299 — drop one ref to a pooled disk image;
    /// free it at zero.
    fn release_disk_image(&mut self, hash: Option<&String>) {
        let Some(hash) = hash else { return };
        if let Some(p) = self.disk_pool.get_mut(hash) {
            p.refs -= 1;
            if p.refs == 0 {
                let len = p.bytes.len() as u64;
                self.disk_pool.remove(hash);
                self.disk_pool_bytes -= len;
            }
        }
    }

    /// runtime-checkpoint-ring.ts:301-306 — pin (exempt from eviction).
    pub fn pin(&mut self, id: &str) -> Option<RuntimeCheckpointRef> {
        let e = self.entries.iter_mut().find(|x| x.r.id == id)?;
        e.r.pinned = true;
        Some(e.r.clone())
    }

    /// runtime-checkpoint-ring.ts:309-314 — unpin (reclaimable on the next full capture).
    pub fn unpin(&mut self, id: &str) -> Option<RuntimeCheckpointRef> {
        let e = self.entries.iter_mut().find(|x| x.r.id == id)?;
        e.r.pinned = false;
        Some(e.r.clone())
    }

    /// runtime-checkpoint-ring.ts:321-333 — Spec 761: drop anchors AFTER `id`
    /// (resume-from-X = a new timeline). Pinned anchors are kept by default.
    /// Returns the number removed; no-op if `id` is unknown.
    pub fn truncate_after(&mut self, id: &str, keep_pinned: bool) -> u64 {
        let Some(idx) = self.entries.iter().position(|x| x.r.id == id) else {
            return 0;
        };
        let mut removed = 0u64;
        // Walk from the newest down to just-after idx so removals don't shift the cut.
        let mut i = self.entries.len();
        while i > idx + 1 {
            i -= 1;
            if keep_pinned && self.entries[i].r.pinned {
                continue;
            }
            self.remove_entry_at(i);
            removed += 1;
        }
        removed
    }

    /// runtime-checkpoint-ring.ts:343-354 — the stored RuntimeCheckpoint payload
    /// tree for `id` (for the caller to `restore_runtime_checkpoint`), with the
    /// pooled media slots REHYDRATED from the disk pool. None if `id` is unknown.
    pub fn restore_snapshot(&self, id: &str) -> Option<Value> {
        let e = self.entries.iter().find(|x| x.r.id == id)?;
        // Spec 807 §4.2 — rebuild the tree from the stored bytes. This is the cold
        // path: capture runs 50×/s at cadence 1, restore runs when a human clicks.
        let mut payload: Value = serde_json::from_slice(&e.payload).ok()?;
        for (slot, hash) in &e.blob_hashes {
            payload[slot.as_str()] = match self.disk_pool.get(hash) {
                Some(p) => ta_encode(&p.bytes),
                None => Value::Null,
            };
        }
        // Spec 807 §4.3 — re-encode the raw blobs into the tree. This is the ONE place
        // the base64 cost is paid, and it is paid on the cold path: capture runs 50×/s
        // at cadence 1, restore runs when a human clicks.
        for (path, bytes) in &e.raw {
            raw_slot_set(&mut payload, path, ta_encode(bytes));
        }
        Some(payload)
    }

    /// runtime-checkpoint-ring.ts:356-359 — payload-free ref for `id`.
    pub fn get(&self, id: &str) -> Option<RuntimeCheckpointRef> {
        self.entries.iter().find(|x| x.r.id == id).map(|e| e.r.clone())
    }

    /// runtime-checkpoint-ring.ts:362-364 — payload-free refs, oldest first.
    pub fn list(&self) -> Vec<RuntimeCheckpointRef> {
        self.entries.iter().map(|e| e.r.clone()).collect()
    }

    /// runtime-checkpoint-ring.ts:366-368 — has(id).
    pub fn has(&self, id: &str) -> bool {
        self.entries.iter().any(|x| x.r.id == id)
    }

    /// runtime-checkpoint-ring.ts:370-376 — clear (reset entries/free-slots/pool).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.free_slots.clear();
        let mut i = self.slot_count;
        while i > 0 {
            i -= 1;
            self.free_slots.push(i);
        }
        self.disk_pool.clear();
        self.disk_pool_bytes = 0;
    }

    // ── Spec time-travel-tooling Piece 2 — ring dump/restore ────────────────────

    /// Dump the ring to a [`RuntimeCheckpointRingDump`] (the `.c64rering` payload):
    /// every entry with its FULLY-REHYDRATED payload (the pooled media blobs spliced
    /// back in, so each anchor is self-contained), plus its ref metadata + the budget/
    /// max-entries policy + the seq counter (so a restored ring keeps minting unique
    /// ids) + the "current" anchor id. NOT on the hot path.
    pub fn to_dump(&self, current_id: Option<String>) -> RuntimeCheckpointRingDump {
        let entries: Vec<CheckpointAnchorDump> = self
            .entries
            .iter()
            .map(|e| CheckpointAnchorDump {
                id: e.r.id.clone(),
                frame: e.r.frame,
                cycles: e.r.cycles,
                pinned: e.r.pinned,
                byte_size: e.r.byte_size,
                created_at_ms: e.r.created_at_ms,
                // The rehydrated payload (pooled blobs spliced back) — self-contained.
                payload: self.restore_snapshot(&e.r.id).unwrap_or(Value::Null),
            })
            .collect();
        RuntimeCheckpointRingDump {
            budget_bytes: self.budget_bytes,
            max_entries: self.max_entries,
            seq: self.seq,
            current_id,
            entries,
        }
    }

    /// Reconstruct a ring from a [`RuntimeCheckpointRingDump`] (inverse of [`to_dump`]).
    /// Re-inserts every dumped anchor PRESERVING its id/frame/cycles/pinned/byteSize/
    /// createdAtMs and re-pooling its media blobs (so the content-addressed dedup is
    /// rebuilt). The seq counter is restored so new captures keep minting unique ids.
    /// Bypasses the eviction policy (the dump already fits — it WAS a live ring).
    pub fn from_dump(d: &RuntimeCheckpointRingDump) -> Self {
        let mut ring =
            Self::with_budget_and_max_entries(d.budget_bytes, d.max_entries);
        ring.entries.clear();
        ring.free_slots.clear();
        for a in &d.entries {
            let mut payload = a.payload.clone();
            // Re-extract the pooled media blobs into the content-addressed pool,
            // nulling them in the stored entry (mirrors `capture`'s pooling) so the
            // restored ring dedups identical disks exactly as the live one did.
            let mut blob_hashes: HashMap<String, String> = HashMap::new();
            for slot in POOLED_BLOB_SLOTS {
                if let Some(bytes) = ta_decode(payload.get(slot)) {
                    if !bytes.is_empty() {
                        let hash = sha256_hex(&bytes);
                        match ring.disk_pool.get_mut(&hash) {
                            Some(p) => p.refs += 1,
                            None => {
                                let len = bytes.len() as u64;
                                ring.disk_pool
                                    .insert(hash.clone(), PooledBlob { bytes, refs: 1 });
                                ring.disk_pool_bytes += len;
                            }
                        }
                        blob_hashes.insert(slot.to_string(), hash);
                        payload[slot] = Value::Null;
                    }
                }
            }
            // Spec 807 §4.3 — and re-extract the big non-media blobs to raw bytes, so a
            // ring loaded from a `.c64rering` costs the same memory as the live one it
            // came from. Without this a `ringload` would silently be 3× the ring it
            // dumped.
            let mut raw: Vec<(&'static str, Vec<u8>)> = Vec::new();
            for path in RAW_SLOTS {
                if let Some(bytes) = ta_decode(raw_slot_get(&payload, path)) {
                    if !bytes.is_empty() {
                        raw.push((path, bytes));
                        raw_slot_set(&mut payload, path, Value::Null);
                    }
                }
            }
            ring.entries.push(RingEntry {
                r: RuntimeCheckpointRef {
                    id: a.id.clone(),
                    frame: a.frame,
                    cycles: a.cycles,
                    pinned: a.pinned,
                    byte_size: a.byte_size,
                    created_at_ms: a.created_at_ms,
                },
                payload: serde_json::to_vec(&payload).unwrap_or_default(),
                blob_hashes,
                raw,
            });
        }
        // Restore the seq counter (must be ≥ the count to never collide), and rebuild
        // the free-slot accounting to keep the stats contract consistent.
        ring.seq = d.seq.max(ring.entries.len() as u64);
        let used = ring.entries.len() as u64;
        let free = ring.slot_count.saturating_sub(used);
        let mut i = free;
        while i > 0 {
            i -= 1;
            ring.free_slots.push(i);
        }
        ring
    }

    /// runtime-checkpoint-ring.ts:378-394 — stats().
    pub fn stats(&self) -> RuntimeCheckpointRingStats {
        let pinned_count = self.entries.iter().filter(|e| e.r.pinned).count() as u64;
        let count = self.entries.len() as u64;
        RuntimeCheckpointRingStats {
            count,
            pinned_count,
            // Spec 807 §4.5 — sum what the entries actually cost instead of assuming
            // `SLOT_BYTES` each. Before, a ring of 500 lean entries reported 32 MiB and
            // held 102 MiB; the entries now hold their big buffers raw, and `byte_size`
            // is their real length plus the measured residual tree.
            total_bytes: self.entries.iter().map(|e| e.r.byte_size).sum::<u64>()
                + self.disk_pool_bytes,
            budget_bytes: self.budget_bytes,
            oldest_frame: self.entries.first().map(|e| e.r.frame),
            newest_frame: self.entries.last().map(|e| e.r.frame),
            disk_image_versions: self.disk_pool.len() as u64,
            disk_pool_bytes: self.disk_pool_bytes,
            slot_bytes: SLOT_BYTES,
            slot_count: self.slot_count,
            free_slots: self.free_slots.len() as u64,
        }
    }
}

impl Default for RuntimeCheckpointRing {
    fn default() -> Self {
        Self::new()
    }
}

// ── Ring dump payload (Spec time-travel-tooling Piece 2) ──────────────────────────

/// One checkpoint anchor in a [`RuntimeCheckpointRingDump`] — the ref metadata + the
/// FULLY-REHYDRATED RuntimeCheckpoint payload tree (pooled media spliced back in, so
/// the anchor is self-contained inside the `.c64rering` container).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointAnchorDump {
    pub id: String,
    pub frame: u64,
    pub cycles: u64,
    pub pinned: bool,
    pub byte_size: u64,
    pub created_at_ms: u64,
    /// The rehydrated checkpoint Value (the same shape `restore_snapshot` returns /
    /// `restore_runtime_checkpoint` consumes).
    pub payload: Value,
}

/// Serializable snapshot of a whole [`RuntimeCheckpointRing`] for the `.c64rering`
/// container: every anchor + the budget/max-entries policy + the seq counter + the
/// "current" anchor id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeCheckpointRingDump {
    pub budget_bytes: u64,
    pub max_entries: u64,
    pub seq: u64,
    pub current_id: Option<String>,
    pub entries: Vec<CheckpointAnchorDump>,
}

// ── helpers ─────────────────────────────────────────────────────────────────────

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let digest = h.finalize();
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Decode a `{ $ta }`-tagged typed-array node (native_snapshot codec) to bytes, or
/// None if the node is null/absent/not a $ta blob.
fn ta_decode(node: Option<&Value>) -> Option<Vec<u8>> {
    let node = node?;
    if node.is_null() {
        return None;
    }
    crate::native_snapshot::ta_u8_decode(node)
}

/// Encode bytes as a `{ $ta }`-tagged Uint8Array node (native_snapshot codec).
fn ta_encode(bytes: &[u8]) -> Value {
    crate::native_snapshot::ta_u8(bytes)
}

/// The byte length of a `{ $ta: "Uint8Array", b64 }` node, or None if the node is
/// null/absent/untagged. The native_snapshot codec carries no length field, so this
/// decodes the b64 payload to measure it (the validation path only — not the hot
/// restore path).
fn ta_len(node: Option<&Value>) -> Option<usize> {
    let node = node?;
    if node.is_null() {
        return None;
    }
    crate::native_snapshot::ta_u8_decode(node).map(|b| b.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Build a minimal valid checkpoint payload with a RAM blob of the right size
    /// and an optional pooled disk-image blob.
    fn mk_payload(ram_fill: u8, disk: Option<&[u8]>) -> Value {
        let ram = vec![ram_fill; RAM_BYTES as usize];
        let mut p = json!({
            "schemaVersion": 1,
            "ram": ta_encode(&ram),
            "driveDiskImage": Value::Null,
        });
        if let Some(d) = disk {
            p["driveDiskImage"] = ta_encode(d);
        }
        p
    }

    #[test]
    fn capture_assigns_cp_frame_seq_id() {
        let mut ring = RuntimeCheckpointRing::new();
        let a = ring.capture(mk_payload(0x11, None), 10, 1000).unwrap();
        let b = ring.capture(mk_payload(0x22, None), 20, 2000).unwrap();
        assert_eq!(a.id, "cp_10_0");
        assert_eq!(b.id, "cp_20_1");
        assert_eq!(a.frame, 10);
        assert_eq!(a.cycles, 1000);
        assert_eq!(b.cycles, 2000);
        assert_eq!(ring.list().len(), 2);
        // list() is oldest-first.
        let ids: Vec<_> = ring.list().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, vec!["cp_10_0", "cp_20_1"]);
    }

    #[test]
    fn rejects_wrong_ram_size() {
        let mut ring = RuntimeCheckpointRing::new();
        let p = json!({ "ram": ta_encode(&[0u8; 16]) });
        assert!(ring.capture(p, 0, 0).is_err());
        assert_eq!(ring.list().len(), 0);
    }

    #[test]
    fn restore_snapshot_roundtrips_payload_and_disk_pool() {
        let mut ring = RuntimeCheckpointRing::new();
        let disk = vec![0xAB; 4096];
        let r = ring.capture(mk_payload(0x33, Some(&disk)), 5, 500).unwrap();
        let restored = ring.restore_snapshot(&r.id).unwrap();
        // RAM survives; disk rehydrated from the pool to the same bytes.
        assert_eq!(ta_len(restored.get("ram")), Some(RAM_BYTES as usize));
        let got = ta_decode(restored.get("driveDiskImage")).unwrap();
        assert_eq!(got, disk);
    }

    #[test]
    fn disk_pool_dedups_identical_images() {
        let mut ring = RuntimeCheckpointRing::new();
        let disk = vec![0xCD; 8192];
        ring.capture(mk_payload(1, Some(&disk)), 1, 1).unwrap();
        ring.capture(mk_payload(2, Some(&disk)), 2, 2).unwrap();
        let s = ring.stats();
        assert_eq!(s.disk_image_versions, 1, "identical disk stored once");
        assert_eq!(s.disk_pool_bytes, 8192);
        assert_eq!(s.count, 2);
    }

    #[test]
    fn eviction_drops_oldest_unpinned_when_full() {
        // Tiny ring: 2 slots.
        let mut ring = RuntimeCheckpointRing::with_budget(2 * SLOT_BYTES);
        assert_eq!(ring.stats().slot_count, 2);
        let a = ring.capture(mk_payload(1, None), 1, 1).unwrap();
        let _b = ring.capture(mk_payload(2, None), 2, 2).unwrap();
        assert_eq!(ring.stats().free_slots, 0);
        // Pin the oldest so the NEXT eviction skips it.
        ring.pin(&a.id);
        let _c = ring.capture(mk_payload(3, None), 3, 3).unwrap();
        // `a` (pinned) survives; `b` (oldest unpinned) was evicted.
        assert!(ring.has(&a.id));
        let frames: Vec<_> = ring.list().into_iter().map(|r| r.frame).collect();
        assert_eq!(frames, vec![1, 3]);
    }

    #[test]
    fn capture_errors_when_all_pinned_and_full() {
        let mut ring = RuntimeCheckpointRing::with_budget(SLOT_BYTES); // 1 slot
        let a = ring.capture(mk_payload(1, None), 1, 1).unwrap();
        ring.pin(&a.id);
        let err = ring.capture(mk_payload(2, None), 2, 2);
        assert!(err.is_err(), "full + all pinned must error (ring gap)");
        assert!(ring.has(&a.id));
    }

    #[test]
    fn truncate_after_drops_newer_keeps_pinned() {
        let mut ring = RuntimeCheckpointRing::new();
        let a = ring.capture(mk_payload(1, None), 1, 1).unwrap();
        let _b = ring.capture(mk_payload(2, None), 2, 2).unwrap();
        let c = ring.capture(mk_payload(3, None), 3, 3).unwrap();
        let _d = ring.capture(mk_payload(4, None), 4, 4).unwrap();
        ring.pin(&c.id); // a pinned anchor newer than `a`
        let removed = ring.truncate_after(&a.id, true);
        // b and d removed; c kept (pinned). 2 removed.
        assert_eq!(removed, 2);
        let ids: Vec<_> = ring.list().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![a.id, c.id]);
    }

    #[test]
    fn clear_resets_everything() {
        let mut ring = RuntimeCheckpointRing::with_budget(4 * SLOT_BYTES);
        ring.capture(mk_payload(1, Some(&[0xEE; 16])), 1, 1).unwrap();
        ring.capture(mk_payload(2, None), 2, 2).unwrap();
        ring.clear();
        let s = ring.stats();
        assert_eq!(s.count, 0);
        assert_eq!(s.free_slots, 4);
        assert_eq!(s.disk_image_versions, 0);
        assert_eq!(s.disk_pool_bytes, 0);
    }
}
