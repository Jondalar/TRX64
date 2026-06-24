//! medium_source.rs — Spec 766.3: recorder medium source (disk + cartridge),
//! gen-gated.
//!
//! 1:1 PORT of the c64re TS
//!   C64ReverseEngineeringMCP/src/runtime/headless/recorder/medium-source.ts
//! (`MediumDescriptor` + `collectMediumDescriptors`).
//!
//! The recorder anchor (RAM + chip state, ~70 KiB) is shipped every 0.5 s. The
//! MEDIUM (a .crt, or a D64/G64 disk image) is large and rarely changes, so re-
//! shipping it every anchor is the BUG-049 monster. Instead the recorder ships the
//! medium bytes only when its monotonic generation changed, and otherwise carries
//! just the gen-key for the store to match against what it already holds.
//!
//! TRX64 note: the TS reads the live kernel facade (drive1541.diskWriteGeneration /
//! snapshotDiskImage, cart.writableGeneration / getWritableImage). TRX64's recorder
//! is driven by the daemon, which builds the descriptors from the live `Machine`
//! (drive8 attached disk, cart). A descriptor carries the O(1) `generation` and a
//! lazy `get_bytes` closure the recorder calls ONLY on a gen change — same contract.

/// medium-source.ts:50 — `MediumKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediumKind {
    Disk,
    Cart,
}

/// medium-source.ts:58-66 — `MediumDescriptor`. A gen-gated medium handle:
/// `generation` is cheap (O(1)); the producer compares it against the last-shipped
/// gen and calls `get_bytes()` ONLY on a change.
pub struct MediumDescriptor {
    pub kind: MediumKind,
    /// Monotonic content generation. Changes iff the medium bytes changed.
    pub generation: i32,
    /// Current medium bytes (committed). Lazy — only called on a gen change.
    pub get_bytes: Box<dyn Fn() -> Option<Vec<u8>>>,
}

impl std::fmt::Debug for MediumDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediumDescriptor")
            .field("kind", &self.kind)
            .field("generation", &self.generation)
            .field("get_bytes", &"<closure>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_lazy_bytes() {
        let bytes = vec![1u8, 2, 3];
        let b = bytes.clone();
        let d = MediumDescriptor {
            kind: MediumKind::Disk,
            generation: 4,
            get_bytes: Box::new(move || Some(b.clone())),
        };
        assert_eq!(d.kind, MediumKind::Disk);
        assert_eq!(d.generation, 4);
        assert_eq!((d.get_bytes)(), Some(bytes));
    }
}
