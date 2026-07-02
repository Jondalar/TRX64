# Capability Cut — 3 Open Decisions

Companion to `spec-c64re-trx64-split-charter.md`. Date: 2026-06-29.

## Context

The split principle is settled: **TRX64 = operate/observe the machine + medium (the instrument, the hands). C64RE = remember / compose / decide + the human (the method, the brain, the memory).** Rule for hybrids: **Capability → TRX64, Meaning/Memory → C64RE.** TRX64 produces bytes & events; C64RE turns them into knowledge.

The big allocations are done (see charter). Three questions remained open. They all cluster on **one meta-question: how much *static* capability does the instrument host natively vs. the workbench?** This doc weighs each, then shows they collapse to one consistent cut + a migration order.

**Runtime corollary:** the TRX64 Rust daemon is the default backend process (auto-discovered/spawned); the TypeScript runtime remaining in C64RE is fallback/oracle, not the product base.

Two-MCP frame (settled): `trx64-mcp` = the instrument (wielded by human + LLM); `c64re-mcp` = the workbench/method/knowledge. The LLM holds both. The monitor's annotated disasm = a **join**: raw-decode (TRX64) ⊕ annotation-overlay (light-local in TRX64, or rich from C64RE when attached). "Overturn a finding from the monitor" = write to that shared annotation surface — works **without** embedding the heavy analyzer in the emulator.

---

## Q1 — Where do the heuristic/static analyzers live?

The 9 analyzers (`pipeline/src/analysis/pipeline.ts`: code-discovery, text, sprite, charset, screen-RAM, bitmap, pointer-table, SID, probable-code) + `ram-state.ts`. Today: TS in C64RE, runtime-independent, output feeds the knowledge graph via `analysis-import.ts` (the firehose gate).

**Options**

- **A — Stays in C64RE.**
- **B — Moves wholesale to TRX64.**
- **C — Split: classification engine = TRX64 (neutral capability); schema-mapping + gate + findings = C64RE.**
- **D — Standalone shared lib both consume.**

**Pro / Con**

| Opt | Pro | Con |
|---|---|---|
| A | Zero migration; already coupled to `SegmentKind`/annotation schema + the gate. Schema + analyzer co-located. Role-refactor (evidence-gated) easier in-place in TS. | Violates Capability→TRX64. TREX-guy with only TRX64 can't statically analyze a PRG. Stays TS (against the deprecation goal). |
| B | Honors the principle. TRX64 standalone = full cracking tool. Rust = fast. Co-located with raw-decode it needs. | Big TS→Rust migration. Analyzers coupled to the segment schema → duplicate schema in TRX64 or pass it (drift risk). Output is hypotheses that still need C64RE gating → you split anyway → that's C. |
| C | Cleanest by principle: raw classification = capability (instrument), meaning = workbench. TRX64 emits neutral `{offset, kind-guess, confidence}`; C64RE maps → `SegmentKind` + deduped findings. TRX64 standalone still useful. Matches the raw-decode⊕annotation join we already chose. | Needs a neutral classification contract (TRX64→C64RE). Still a Rust migration eventually. Slight two-step indirection. |
| D | Neither owns it; clean dependency. | A third artifact for one consumer pattern. Overhead not worth it. |

**DECIDED 2026-06-29: C, phased.** Target = C (classification capability in TRX64; schema-map + gate + findings permanently C64RE). Interim = A (stays C64RE-TS until the Rust classifier exists). The `mos6502` dedupe (below) already starts pulling raw-decode to TRX64; classifiers follow. The firehose gate (`analysis-import` + dedup) is **C64RE forever** regardless.

---

## Q2 — How thin is `trx64-mcp`?

Does it only proxy live daemon verbs, or also host static capabilities (decode/parse/classify/depack) that don't need the running machine?

**Options**

- **A — Pure WS-proxy.** Every verb forwards to the daemon.
- **B — Fat MCP.** Hosts static capabilities natively + proxies live verbs.
- **C — Façade over two surfaces.** Live = daemon (WS); static = a Rust lib (shared crates); `trx64-mcp` fronts both.

**Pro / Con**

| Opt | Pro | Con |
|---|---|---|
| A | Dead simple, one source of truth (daemon). Trivially thin. | Static work (disasm a blob, parse a disk without booting) must run *inside* the running-machine daemon (odd, bloats it) or not on the TRX64 side at all (contradicts Q1-C / Q3-C). |
| B | Static capability runs without a live machine. trx64-mcp becomes "the instrument" fully. TREX-guy gets live + static from one server. | trx64-mcp gets real logic → not "thin". Code in two homes (daemon + mcp) unless shared crate. |
| C | Honest split: stateful=daemon, stateless=static lib, both Rust sharing `trx64-core` crates; mcp = façade exposing both. Static lib reusable in cli/tests without a daemon. | Most structure — but it's just normal Rust-workspace crate hygiene, not real overhead. |

**DECIDED 2026-06-29: C — thin as a *façade*, but TRX64-the-project hosts static capability natively (in a lib crate, not the daemon).** From the LLM's side it looks like B (one instrument server, live + static). Reject pure-proxy A — it forces static analysis to the wrong side and contradicts Q1/Q3. Crate shape: `trx64-core`/`trx64-daemon` (live) + `trx64-static` (decode/parse/classify) + `trx64-mcp` (façade).

---

## Q3 — Where do the static media parsers live?

D64/G64/CRT/TAP parsing — today C64RE (`disk-extractor.ts`, `disk/*.ts`, `extract_disk`/`extract_crt`). TRX64's `vice1541` already parses G64/D64/GCR to mount live.

**Options**

- **A — Stay C64RE-lib.**
- **B — Move to TRX64 static lib.**
- **C — Split: container/format parse = TRX64 static lib (shared with the live drive); payload-entity + provenance creation = C64RE.**

**Pro / Con**

| Opt | Pro | Con |
|---|---|---|
| A | Mature, handles cracking quirks (Pawn copy-protection, custom LUTs, malformed tracks). Tied to the extraction→payload→provenance flow. | Capability on the workbench side. TRX64 standalone can't statically parse media. **Duplicates** `vice1541`'s format code (two parsers, drift risk). |
| B | Honors principle; TRX64 standalone parses media. Rust fast on big G64. | Migration; must preserve the cracking quirks. Payload/provenance (meaning) is still C64RE → split anyway → C. |
| C | **Dedupe win:** GCR/sector/track/CRT-bank logic lives ONCE in TRX64, serving both live-mount AND static-parse. TRX64 exposes neutral "read sectors/tracks/banks, decode GCR, list dir"; C64RE turns bytes → payload entities + provenance. Same format code, two callers. | Contract design (TRX64 media-read API). Quirk-handling must be ported carefully. |

**DECIDED 2026-06-29: Option 2 (= C, refined by the parser-vs-glue split).** The discussion separated two things that were conflated:
- **Format DECODE primitives** (GCR, sector/track, container, bank) → **TRX64**, deduped with `vice1541` (single format home: live-mount + static-parse), exposed as MCP verbs.
- **Per-game EXTRACTION GLUE** (this game's LUT, interleave, malformed-track, depack chain — the LLM's ad-hoc per-game scripting) → **stays TS/scratch** and *calls* the primitives. Frictionless ad-hoc scripting (TS, not Rust-recompile) was the deciding concern.
- Hardest custom loaders: prefer **run-and-capture** (TRX64 live) over re-modeling the geometry statically.

C64RE keeps the loader-model orchestration (which sectors, in what order) + payload/provenance. **Lowest migration priority of the three;** interim = parsers stay TS until the duplication actually bites.

---

## Cross-cutting: it's ONE cut

All three answers are the same answer:

> **Capability (decode / classify / parse) consolidates into a TRX64 static lib** (shared crates with the daemon, eventually Rust). **Meaning (schema-map, findings/gate, payload/provenance, semantic disasm/HEAD/lineage) stays C64RE.** `trx64-mcp` = thin façade over {live daemon + static lib}. Migration is **phased**, with TS-in-C64RE as the interim each time.

The monitor's annotated view + "overturn from monitor" ride the **shared annotation surface** (light-local in TRX64, rich in C64RE), not embedding.

## Migration order (natural, dedupe-first)

1. **`mos6502.ts` dedupe** → raw-decode in TRX64. Starts the `trx64-static` crate. Smallest, clearest win (TRX64 monitor already has a decoder).
   **STATUS 2026-07-02: DONE (first slice).** `crates/trx64-static` exists; the
   daemon's `disasm_line_ts` / `disasm_line_ts_labeled` / `instr_len` /
   `disasm_one` moved there (daemon = thin re-use, no duplicate decoder);
   `trx64cli disasm <prg>` is the ROM-free static front door (text = monitor
   `d` format, `--json` = `monitorDisasm` shape). Parity: 512-case golden suite
   vs the TS oracle `disasm6502.ts` (`scripts/gen-disasm-goldens.mjs`) — also
   closed a latent gap (holes rendered `???`/`ISB`/`SBC_IMM`; oracle says
   `JAM`/`ISC`/`SBC`).
2. **Media format-parse** → `trx64-static`, shared with `vice1541`. Dedupes the second-biggest duplication.
3. **Heuristic classifiers** → `trx64-static`, neutral `{offset, kind-guess, confidence}` output.
4. Each step: C64RE consumes the new TRX64 capability over the façade; the old TS path is retired only after parity.

## Permanent homes (unaffected by migration)

- **C64RE forever:** onboarding · flow/step/agent-defs · flow-state · orchestration loop · knowledge graph (payloads/provenance/HEAD/findings, deduped) · the firehose gate (`analysis-import`) · semantic disasm + xref + lineage · annotation/HEAD curation · build/rebuild pipeline (assemblers, byte-verify) · the UI.
- **TRX64 forever:** emulator (CPU/VIC/SID/drive) · trace/monitor/reverse-debug/snapshot/scrub · render/audio · live media mount · light-local labels. (+ the static lib gained via migration.)

## Still genuinely undecided (smaller, decide at the slice)

- The exact neutral **classification contract** shape (Q1-C) and **media-read API** (Q3-C) — design at first use.
- The **annotation-sync protocol** for the shared surface (light-local ⊕ rich, join + write-back) — concept settled, format TBD.
