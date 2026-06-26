# TRX64 ↔ c64re Delta — Overnight Grind Summary (für Review)

Branch `fix/drive-viacore-u64-monotonic` (NICHT gepusht). Pin/Merge = offene Entscheidung.

## Das Fundament: Differential-Conformance-Gate
`tools/oracle/src/conformance.ts` — fährt jedes Szenario gegen BEIDE Daemons (TS=Autorität, TRX64=Kandidat), difft das Behavior. TS liefert die Wahrheit jeden Lauf → kann nicht driften, kann kein Feld vergessen. **`npm run conformance`** = die neue Merge-Gate. Du brauchst nie mehr manuell regressen.
- **Validiert bulletproof:** flaggt beide bekannten P0 korrekt RED, GREEN nach Fix.
- **Finaler Full-Sweep: 38/38 GREEN, 0 RED, 4 BLOCKED** (von mir selbst nachgefahren).

## Erledigt — RED→GREEN, gate-verifiziert (commits)
- `18146ee` Gate gebaut
- `926a399` **P0-A** stream-loop bp/observer/JAM-gate (Live-Debugger unter --stream war tot)
- `1f533ee` **P0-B** media-Ingress + Disk-Write-Persist (stiller Datenverlust beim Swap)
- `cf39cf2` B1: break_add-addr, eject-paused, reset-clears-ring, runtime/mark, sid-trace-domain, frame_available (6)
- `bd2f73e` B2: JAM-autobreak, trace-firehose-per-frame, recorder-feed, observer-drain (4)
- `8b134cb` B3: checkpoint-restore (keep-runstate, debug/stopped, frame-push, render-flag) (4)
- `99daa40` B4: debug/run-async, session/run-guard+bp, step-shape, create-trace-params (5)
- `7974a4b` B5: cart-ingress, recents-store+mountedAt, paused-cart-persist, eject-pacing (4)
- `093b58a` B6: VSF-drive-blob, .c64re-cart-flash, trace-stop-descriptor (3)
- `d548ce9` B7a: monitor `trace`/`bt`/`device` (3)
- `39f055e` B7b: monitor obs-DSL + do-log/do-trace drain (3)
- `8303647` B7c: monitor `sd`/`df`/`screen` (2)
- `8030744` B7d: monitor dump/savecrt/swapcrt + host-FS/PRG (2)
- `6e7356e` B7e: monitor label/note/.sym + inspect/xref/sym (project_knowledge.rs) (2)
- `0c7ac0f` B8: runtime/call full-AgentQueryApi-dispatch + scenario-registry file-backed (2)
- `37b1f6d` cleanup: monitor `bitmap` (charset/sprite modi) + tsc-clean
- VIC-cycle-fix: **BLOCKED (korrekt)** — KEIN Budget-Rounding (run-loop matcht TS exakt), sondern 6-cycle IRQ-exit/re-entry-Skew im **isolierten `run_for_vic`/VicBus-Pfad** (nur bei vic-trace-domain, NICHT der Produkt-`run_for_full`-Pfad). Fix = VIC-raster-IRQ/CPU-interrupt-ack-Core-Timing → 7-game-Gate-Risiko. Scoped Lead: wann cpu6510 `vic.irq_line` sampled + raster-compare-cycle, gegen TS-literal-port prüfen. Kein Commit, tree clean.

→ **~44 P1-Divergenzen + 2 P0 zu.** Plus die früheren Branch-Fixes (Alarm-u64, Background-Loops, Thumb-Store).

## 4 BLOCKED (kein TRX64-Bug — TS-Oracle-Limit, direkt per Rust-Test verifiziert)
- ws-media-3 (paused cart-persist) — Flash-Write braucht Mapper-Sequenz, kein WS-Weg. Rust-Test grün.
- background-workers-async-0 (recorder auto-feed) — TS recorder-worker lädt nicht unter tsx. Direkt: anchors 1→16.
- ws-checkpoint-scrub-1/2 (frame-push / render) — binärer VIC-Frame / Pixel-Content, kein JSON-Proxy. Rust-Tests grün.

## DEFERRED — große Subsysteme (sauber gescopet, NICHT overnight gebaut)
**1. trace/read DuckDB** (entsperrt misc-0, misc-1, misc-14 map/taint/swimlane/chis + 5 runtime/call-Methoden queryEvents/followPath/swimlaneSlice/traceTaint/profileLoader)
- TS: `trace/read` = DuckDB-DB + ~9 Analyse-Ops über v2/-Module.
- TRX64: KEIN duckdb-crate; finalize_trace fabriziert nur den .duckdb-Pfad.
- Braucht: (a) `duckdb` Rust-crate (native dep), (b) .c64retrace→DuckDB-Ingestion mit TS-kompatiblem Schema, (c) ~9 Op-Ports (swimlane/taint/follow_path/profile_loader sind echte Analyse-Algos in v2/).
- Größe: mehrtägig. **Entscheidung morgen:** voll bauen vs. Teilmenge (getInfo/topPcs/sql zuerst) vs. weiter deferren.

**2. RewindManager echte Maschinen-Rewind** (6 runtime/call: beginRewindSession/rewindTo/applyPatch/runForward/diffBranches/promoteBranch)
- TRX64 hat das Branch-Tree-MODELL (rewind.rs), fehlt = echte Maschinen-Manipulation. Checkpoint-Restore-Primitive existieren schon (P0/B3) → mittlere Verdrahtung. Tractabler als DuckDB.

**3. resolvePc/resolvePcs/diffSnapshots** (project-disasm-DB)
- resolvePc jetzt teilweise backbar (B7e project_knowledge.rs liest _analysis.json). diffSnapshots = Snapshot-Vergleich. Klein-mittel.

## Residuals (klein, für später)
- `flow`-Verb: blieb „honest main" (TRX64 hat keinen FlowTracker; `bt` ist echt). Voller FlowTracker-Port = eigener Task.
- VIC c64Cycles +1/+1/+3 (3 oracle-scenarios): pre-existing (Branch-Base de2eb56), session/run-Budget-Boundary. Builder versucht's grad.
- ~25 runtime/call-Methoden bei -32601 = die obigen Deferred-Subsysteme (nicht Dispatch, fehlende Backends).

## Hinweise
- Pre-existing reSID parallel-test-flake (`gate_a_native_byte_identical`, ±2 LSB) — taucht nur unter paralleler Last auf, grün isoliert; nicht von dieser Arbeit.
- Alle Edits in `crates/` + `tools/oracle/`. c64re-Repo NIE angefasst.
- Pin (`runtime/TRX64_VERSION`) + Merge zu main = wenn der Stand fein ist (dein „Pin als letzter Schritt").
