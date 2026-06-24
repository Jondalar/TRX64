# TRX64 — Feature-Complete vs c64re TS Headless

Stand: 48 done, ~281 commits. Legend: ✅ done · 🔄 in progress · ⬜ open

## Kern — verbatim VICE (✅ komplett + bewiesen)
- [x] CPU 6510 — x64sc SC core (verbatim)
- [x] VIC-II — viciisc per-cycle + per-cycle Pixel/Sprite-Draw (vicii-draw-cycle)
- [x] 1541 Drive — 6510core + 1:1 viacore / rotation / iecbus / via1
- [x] CIA · SID (fastsid) · IEC · GCR
- [x] Cartridge read-only (Normal / MagicDesk / Ocean)
- [x] G64-Mounting (GCR/Half-Tracks)
- [x] reSID-Audio (cc-FFI, byte-deterministisch)
- [x] **7-Spiele-Gate 7/7** · scramble sauber · ~8–10× schneller als TS

## Surface / Tooling
- [x] Observability-Tick-Hooks (on_interrupt / breakpoint / on_access-watch)
- [x] Breakpoints / Watchpoints / conditional / until (ObserverRegistry 1:1)
- [x] WS protocol-surface b1 — 13 Methoden (live vs c64re verifiziert)
- [x] WS protocol-surface b2 — key_down/up held-key (interaktiver Input)
- [x] A/V-Push — ws-av-tap real-time recording
- [x] Snapshots: VSF byte-parity (beide Richtungen) + Real-VICE .vsf lesen
- [x] **.c64re Runtime-Snapshot — 100% cross-runtime (C64 + Drive, TS↔Rust beide Richtungen)**
- [x] **checkpoint-ring** (ring + checkpoint/* 6/7 + vic/inspect 3/9) (705.B) — Rewind-Ring + 7 `checkpoint/*`-Methoden + granular `vic/inspect/*`  ← läuft
- [x] recorder/* (6) + runtime/scenario_* (5) WS  (baut auf checkpoint-ring)
- [x] audio/* (3) + media/* (9) + batch/* (3) WS  (mechanisch)
- [x] **Flash-Cart writable** (Flash040+M93C86, EasyFlash/GMOD2 booten) — Flash040 + EAPI + m93c86 + EasyFlash/GMOD/MegaCart  (~1.5 KLOC, groß)
- [x] **Drive write-back** (.g64/.d64 persist, D64+G64 round-trip) — .g64/.d64 schreiben (fsimage_gcr_write_half_track)
- [x] **integration** — breiter Corpus end-to-end Cross-Runtime-Validierung (`tools/oracle/src/integration.ts`): treibt TRX64-Daemon + live c64re-Daemon durch dieselbe WS-Sequenz; 3 Achsen (Corpus boot+mount+LOAD+RUN→game-live · WS-Surface auf laufendem Programm: screenshot/monitor/checkpoint-rewind/breakpoint_hit/audio · Cross-Runtime .c64re-Snapshot dump↔undump beide Richtungen). Self-test GREEN (11/11 surface, 2/2 snapshot); Report `docs/integration-report.md`.

## Grob
Feature-complete-vs-TS-Capstone (integration) erreicht: TRX64-Daemon ≡ c64re-Daemon
über die WS-Surface, inkl. Cross-Runtime-.c64re-Snapshot auf einem laufenden Programm
(beide Richtungen). Byte-exakte Gates GREEN (269 passed), 7-Spiele-Gate 7/7.

## ✅ FEATURE-COMPLETE vs c64re TS headless (2026-06-25, ADR-087)
