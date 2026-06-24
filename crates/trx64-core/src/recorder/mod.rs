//! recorder — Spec 766: the runtime recorder subsystem.
//!
//! A 1:1 port of the c64re TS recorder
//!   C64ReverseEngineeringMCP/src/runtime/headless/recorder/*
//! The recorder records a session as a stream of periodic ANCHORS (full machine
//! checkpoints, RAM + chip state) plus gen-gated MEDIUM images (disk/cart), into a
//! bounded byte-ring store that gives a scrub history. A stored anchor can be
//! reconstructed into a full restorable checkpoint payload.
//!
//! Module map (each file is a 1:1 port of the same-named TS file):
//!   - anchor_codec    ← anchor-codec.ts    (self-describing binary value codec)
//!   - anchor_record   ← anchor-record.ts   (ring record framing + headers)
//!   - anchor_store    ← anchor-store.ts    (worker-owned byte-ring anchor store)
//!   - recorder_ring   ← recorder-ring.ts   (the record-handoff ring)
//!   - medium_source   ← medium-source.ts   (gen-gated disk/cart descriptors)
//!   - runtime_recorder← runtime-recorder.ts(the orchestrator)
//!
//! See each file header for the SINGLE-THREAD COLLAPSE note: the TS worker thread +
//! SharedArrayBuffer rings are a V8-GC + thread-isolation detail (BUG-049); TRX64's
//! daemon is single-threaded, so the observable contract is ported over plain
//! in-process structures with no behavioural difference the wire can see.

pub mod anchor_codec;
pub mod anchor_record;
pub mod anchor_store;
pub mod medium_source;
pub mod recorder_ring;
pub mod runtime_recorder;
