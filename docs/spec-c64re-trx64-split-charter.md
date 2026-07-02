# Charter — split C64RE into TRX64 (runtime+MCP) and C64RE (workbench)

**Status:** CHARTER (planning — a goal to chew on, not a started task).
**One line:** make the architecture match what's already true — TRX64 is the C64
**runtime** (emulator + debugger + a thin MCP façade); C64RE is the RE **workbench**
(static analysis, semantics, project knowledge, the web UI) that *consumes* TRX64.

This formalizes the direction we're already on: TRX64 is the strategic base and default
runtime, C64RE's TS runtime is demoted to a fallback / parity oracle (kept for
compatibility + differential testing, not the product base), and C64RE's `runtime_*`
tools already proxy to TRX64's WS. The split just draws the line cleanly and finishes the
demotion.

---

## 1. The boundary (the seam)

Two genuinely different concerns, today co-housed only for historical reasons:

| | **TRX64 — runtime** | **C64RE — workbench** |
|---|---|---|
| Question | what the machine *does* | what the code *means* |
| Work | execute · observe · debug | understand · annotate · document |
| Examples | cycle-accurate VIC/CIA/SID, 1541, traces, reverse-debug, checkpoints, media | heuristic disasm, annotations, segment/semantic analysis, project DB, agent doctrine, wiki |
| Truth | the live machine state | the analysis JSON + project knowledge |
| Speed | real-time / cycle | seconds / batch |

If a tool needs the **live machine**, it's TRX64. If it reads/writes the **analysis +
project knowledge**, it's C64RE.

**Leitregel: Capability → TRX64, Meaning/Memory → C64RE.** TRX64 is the strategic runtime base and the default backend process (the Rust daemon, auto-discovered/spawned) — it produces bytes, events and machine-state and owns runtime, instrument, reverse-debug, trace, checkpoints (`.c64re`/`.c64retrace`), daemon/FFI/CLI. C64RE is the reverse-engineering workbench — project knowledge, method/memory, analysis pipeline, semantic disassembly, findings/entities/questions, UI/orchestration, curation — it turns those bytes/events/state into knowledge. The TypeScript runtime in C64RE is a fallback / parity oracle, not the strategic base. Endstate: two MCP servers — `trx64-mcp` (instrument/runtime) and `c64re-mcp` (workbench/knowledge); today's C64RE `runtime_*` tools are a transition/proxy to the TRX64 backend, not their permanent home.

---

## 2. Target architecture

```
  ┌─────────────────────────────────────────────────────────────────┐
  │ C64RE — the workbench                                            │
  │   • analysis pipeline (TRXDis): disasm, annotations, segments,   │
  │     semantics V2, dual-assembler                                 │
  │   • project layer: search/wiki/findings/entities/questions       │
  │   • agent doctrine + orchestrator                                │
  │   • the web UI (browser)                                         │
  │   • a workbench MCP server (analysis + project tools)            │
  └──────────────┬───────────────────────────────┬──────────────────┘
                 │ WS JSON-RPC (runtime)          │ (browser → both)
                 ▼                                ▼
  ┌─────────────────────────────────────────────────────────────────┐
  │ TRX64 — the runtime                                              │
  │   • trx64-daemon (WS, the ONE machine)  ← the authority          │
  │   • trx64-ffi (typed in-process embedding, e.g. the Swift app)   │
  │   • trx64-cli (cockpit + emulator window)                        │
  │   • NEW: trx64-mcp — a thin MCP façade (stdio) that is a WS      │
  │     CLIENT to the daemon (no second machine; one per process)    │
  └─────────────────────────────────────────────────────────────────┘
```

An LLM doing pure runtime work connects to **trx64-mcp** alone. A full RE session
connects to **both** MCP servers. The web UI (browser) talks the runtime WS for the live
tabs and C64RE for the analysis tabs — one app, two backends.

---

## 3. The thin MCP façade (the one new piece)

TRX64 has WS + FFI + cli, but no MCP server yet. Add `trx64-mcp`:

- An **MCP stdio server** that is a **WS client** to a running `trx64-daemon` (NOT an
  embedded machine — respects one-machine-per-process; it's a client like C64RE).
- Each MCP tool = a thin wrapper: validate args → `dispatch`/WS call → return. The tool
  set mirrors the WS method surface (session/run/monitor/trace/checkpoint/reverse-debug/
  media/snapshot). Reuse the trx64-ffi typed records for the schemas where possible.
- Lifecycle: if no daemon is up, `trx64-mcp` can spawn one (the `runtimeSessions.start`
  attach contract) or connect to the existing one.
- Ship it as a 4th crate (`crates/trx64-mcp`) or a mode of the daemon binary.

This makes TRX64 a **self-contained runtime+MCP** — usable without the workbench (the
TREX / standalone case, an LLM driving just the machine).

---

## 4. Tool inventory + split

**→ TRX64 (runtime MCP).** Everything that needs the live machine:
`runtime_session_*`, `runtime_run_prg` / `load_prg` / `type` / `joystick`,
`runtime_monitor*` (+ the reverse-debug verbs rstep/whowrote/triage/diff/ringdump/ringload
via monitor), `runtime_step_*` / `until` / `follow_path`, `runtime_media_*`,
`runtime_render_screen` / `resolve_pc` / `mark`, `runtime_rewind` / `overlay_run` /
`swap_disk_and_continue`, `runtime_trace_start/finalize/status`, `runtime_query_events`,
`runtime_swimlane_slice` / `trace_taint` / `vic_inspect_at` / `profile_loader`.

**→ C64RE (workbench MCP; tool surface unchanged).** Everything static/knowledge:
`analyze_prg` / `disasm_prg` / `disasm_menu` / `assemble_source`, `propose_annotations`,
disk/CRT/G64 extraction + inspection, `inspect_address_range` / `ram_report` /
`c64ref_lookup`, the project layer (`project_*`, `save_*`, `list_*`, `link_*`), the agent
layer (`agent_*`), `build_*`, payload/cart-chunk tools, the wiki.
*(Reconciled with `capability-cut-decisions.md`, 2026-06-29: the tool surface and
the meaning layer stay C64RE, but the static decode/parse/classify **capability**
underneath these tools migrates phased into `trx64-static` — see the cut doc's
migration order; C64RE-side registration = C64RE Spec 774.)*

**Hybrid — decide explicitly:**
- **`trace_store_*` + the DuckDB store.** TRX64 *emits* the `.c64retrace` stream; C64RE
  *ingests + queries* it (the store is an analysis DB). Proposed: capture verb = TRX64,
  the DuckDB store + query verbs = C64RE. (Or: a native Rust DuckDB reader in TRX64 —
  see the standalone trace-read spec — would let TRX64 own query too.)
- **`run_prg_reverse_workflow`** (runs + analyzes): orchestration → C64RE (it calls
  TRX64's runtime, then does the analysis).
- **`trace_memory_map`**: derived from a trace → C64RE (analysis) reading TRX64 output.

---

## 5. Migration — incremental, evening-session-sized

The current proxy works, so this is safe to do in slices; nothing breaks mid-way.

1. **Stand up `trx64-mcp` (skeleton)** — stdio MCP server, WS client to the daemon, with
   ONE tool (`ping` / `session_state`). Prove the round-trip. *(one evening)*
2. **Port the read-only runtime tools** — state/monitor/disasm/registers/render_screen.
   Verify against C64RE's equivalents. *(one or two evenings)*
3. **Port run-control + reverse-debug** — run/step/until/rstep/whowrote/triage/checkpoint.
4. **Port media + trace-capture.**
5. **Resolve the hybrids** (trace store boundary, reverse_workflow).
6. **C64RE: demote the TS runtime to fallback / oracle** — TRX64 becomes C64RE's default
   runtime backend; the in-tree TS emulation is kept as a compatibility fallback +
   differential-test / parity oracle, not the product base. C64RE's `runtime_*` tools
   become thin re-exports of trx64-mcp, or the user connects trx64-mcp directly. Decide:
   does C64RE keep a runtime proxy for convenience, or do clients connect both servers?
7. **Web UI:** point the live tabs at TRX64's WS directly (already are, via C64RE proxy —
   make it explicit); analysis tabs stay on C64RE.
8. **Docs:** TRX64 README gains "runtime + MCP"; C64RE README becomes "the workbench".

Each step is independently shippable + reversible.

---

## 6. Open decisions

- **Two MCP servers (settled canon):** `trx64-mcp` = instrument/runtime and `c64re-mcp`
  = workbench/knowledge. (Not C64RE re-exporting the runtime tools as a single gateway.)
- **trace store ownership** (capture vs query split, or native Rust DuckDB in TRX64).
- **`trx64-mcp` packaging:** separate crate vs a `--mcp` mode of the daemon.
- **Repo topology:** stays two repos (C64RE ↔ TRX64). Confirmed by the existing cut.
- **Naming:** "TRX64 runtime MCP" vs a product name.

## 7. Non-goals
- No big-bang rewrite of the analysis pipeline. ~~It stays in C64RE as-is~~ —
  superseded by `capability-cut-decisions.md` (2026-06-29): the semantic layer
  stays in C64RE permanently; the static decode/parse/classify capability
  migrates *phased* into `trx64-static` (step 1 = shared 6502 decode +
  `trx64cli disasm`, shipped 2026-07-02), TS path retired only after parity.
- No second machine / no change to one-machine-per-process.
- The web UI stays browser-based in C64RE (not ported to native).

## 8. Done when
- `trx64-mcp` exposes the full runtime surface; an LLM drives the machine via it alone.
- C64RE's default runtime is TRX64 over WS for everything live; the TS runtime remains
  only as a fallback / parity oracle, not the product base.
- The hybrids have a clear owner.
- Both READMEs state the clean boundary.
