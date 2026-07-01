# Lykia / MegaByter CPU-Divergence — Follow-up Loop Charter

Status: **PARKED** — do not start until the CLI-feel loop is DONE and Alex gives
the go. Scope + approach assembled 2026-07-01. Owner: Alex (TRX/TREX), remote.

## 1. Problem

The **Lykia** cartridge (Protovision, **MegaByter** mapper, hw 0x56) **crashes in
TRX64 (Rust)** — garbage screen, CPU ends at **PC=$0002** — but **runs fully in the
C64RE TypeScript runtime** (boots to the "Lykia / Play / Prologue" menu). Same
`.crt`, same mapper. TS is therefore the **parity oracle**: the artifact is good,
TRX64 is wrong.

### Already ruled out (do NOT re-investigate)
The **entire TRX64 MegaByter cart layer is a faithful 1:1 port of the TS** and was
read-diffed this session:
- `MegabyterMapper` (`crates/trx64-core/src/cart.rs:1264`) read/write/getLines/reset/
  flashOffset — identical to `cartridge.ts:1060`.
- Bus RAM-fallthrough at $8000 in non-ultimax (`full.rs:707` — so Lykia's stage-code-
  into-$9Fxx-RAM works), PLA reconfig on `$DE02` write, `build_linear_chip_data`.
- The MegaByter write-routing fix (flash only in ultimax `register_02&3==3`) already
  landed and is correct.

### Symptom detail (from the 2026-05-24 + 06-30 notes)
- Autostart entry **$8009** is reached; the cart cold-start runs.
- **$82D3** = `LDA $D011 / BPL` raster top-of-frame sync — completes fine.
- Then the **bank-0 loader** crashes to **PC=$0002** *without ever banking*
  (`register_02` / current_bank stay 0). So it is NOT the MegaByter banking (the
  synthetic `probe-713-devcore` gate is green) — it is a real execution-path issue.

## 2. Hypothesis space

The divergence is **outside the cart**, in one of:
- **(A) CPU core opcode/addressing/flag bug** — the Rust `Cpu65xxVice` port
  (`crates/trx64-core/src/cpu.rs` + `c64_6510core.rs`) mis-executes a specific
  opcode, addressing mode, or flag update that the TS `Cpu65xxVice` gets right.
- **(B) Memory/IO READ returning a wrong byte** — a read during the bank-0 loader
  returns a different value in Rust than TS (RAM-under-ROML / CPU-port $00/$01 /
  open-bus / a VIC/SID/CIA register read / cart read). This would send the loader
  down a wrong branch or through a corrupt pointer → `JMP ($xxxx)`/`RTS` into $0002.

**The forward differential trace distinguishes A from B**: if the executed **PC/reg
sequence** diverges while all **read values** matched up to that point → (A) opcode/
control-flow. If a **read value** differs first (same PC, same regs, different byte
returned) → (B) memory/IO read path. Do not guess which; let the trace decide.

`PC=$0002` strongly suggests a **wild `JMP ($ptr)` through a zeroed/garbage pointer
or an `RTS`/`RTI` off a corrupted stack** — i.e. the *effect*. The *cause* (the read
or opcode that corrupted the pointer/stack) is upstream. Backward triage bounds the
effect; forward diff finds the cause.

## 3. Approach — two complementary attacks, converge on the root

Doctrine (mandatory): **trace into DuckDB, never one-off scripts**
(`feedback_trace_into_duckdb`); **first-divergence single record, NOT statistics**
(`feedback_trace_step_not_stats`); **read the oracle source before hypothesising**
(adapt `feedback_read_vice_first` → read the **TS** `Cpu65xxVice` here); **separate
backend, never the live UI session** (`feedback_no_scripts_on_live_ui_session`).

### Phase 0 — deterministic repro on BOTH runtimes (separate backends)
- Cart under test: `/Users/alex/Development/C64/Cracking/Lykia/build/out/lykia_rebuilt.crt`
  (the rebuilt one Alex tests). Cross-check: the original
  `.../Lykia/INPUT/lykia_protovision.crt` and the C64RE sample
  `samples/lykia_MEGABYTER.crt`.
- **TS oracle**: a *separate* C64RE backend process (NOT the live :4312 UI session).
  Mount cart → cold boot → confirm it reaches the menu (the known-good).
- **TRX64**: a *separate* trx64cli/daemon process (NOT the shared machine). Mount
  cart → cold boot → confirm the crash to $0002 reproduces.
- Define the shared **anchor**: PC first reaches **$8009**. Both traces start here.

### Phase 1 — backward crash-triage in TRX64 (bound the window)
Use the always-on reverse-debug ring (`crash_triage.rs` / `delta_ring.rs` /
`cpu_history.rs`) via the monitor:
- `triage` (auto-printed on the crash) → the causal chain: crash PC → the wild
  `JMP`/`RTS` → the stack/pointer corruptor.
- `whowrote 0001`/`whowrote <ptr>` / `whowrote 01f0..01ff` → who last wrote the
  pointer or the stack slots the bad `RTS` pulled.
- `chis` + `rstep` around the wild jump → the last KNOWN-GOOD instruction.
- Output: the **PC window** `[last-good … crash]` that the forward diff must cover.

### Phase 2 — forward differential trace TS ↔ TRX64 → first divergence (the core)
- Capture on BOTH, from the $8009 anchor forward, **per retired instruction**:
  `PC, opcode, A, X, Y, SP, P` **and every memory read** `(addr, value)`, into
  **DuckDB** via the trace-store infra (`runtime_trace_start`/`runtime_trace_finalize`
  + `trace_store_query`/`trace_store_bus_find` on the C64RE side; `trace on` +
  `traceindex` → `.duckdb` on the TRX64 side). Same columns both sides.
- **Alignment (the one real gotcha):** the `$82D3` raster-sync loop trips a
  VIC-timing-dependent number of times, so a naïve cycle/index diff will flag the
  `$D011` raster *value* as a false "first divergence". Handle it by either:
  1. anchoring the diff at the **raster-sync EXIT** (first PC that leaves the
     `$82Dx` loop and falls into the bank-0 loader) and diffing instruction-lockstep
     from there — the crash is *after* the sync, in the deterministic loader; **or**
  2. aligning on the **PC/reg sequence** (fold the sync loop) and treating a read
     divergence as real only if it changes control flow.
  Prefer (1): snapshot both at the sync-exit PC, then step-lockstep.
- Report **only the first divergence + ~20 events around it** (doctrine: not stats,
  not hotspots, not top-PC buckets). One record: the first cycle/instr where PC, a
  register, or a read `value` differs.

### Phase 3 — classify + read oracle-vs-port source
- From the first-divergence record, classify **A (opcode/flow)** vs **B (read)**.
- **A:** identify the exact opcode + addressing mode + which register/flag differs.
  Read the **TS** `Cpu65xxVice` implementation of that opcode/mode vs the Rust
  `cpu.rs`/`c64_6510core.rs` implementation, side-by-side. Walk the **10 TS→Rust
  conversion-bug families** (from CLAUDE.md Spec 620, adapted): missing mask after
  arithmetic, signed/unsigned mixup, sign-extension lost, pre/post-increment order,
  macro/const expansion lost, dropped file-scope state, wrong branch arm, operator
  precedence, array-as-pointer decay, implicit widen — plus Rust-specific: `u8`
  wrapping vs `i32` intermediate, `as` truncation, `wrapping_add` vs `+`.
- **B:** identify which read path returned the wrong byte (bus map / CPU-port
  $00/$01 / open-bus / IO register / RAM-under-ROML) and read that path TS vs Rust.

### Phase 4 — fix + prove convergence
- Fix the ROOT cause (one conversion-family bug). No stubs, no test deletion.
- **Differential test** for the fixed element: a `.diff` test (adapt
  `feedback_c_to_ts_diff_test` → TS↔Rust) asserting byte-equal state on the opcode/
  read over fuzzed inputs, so it can never regress.
- **Re-trace** Phase 2: the first-divergence must now be GONE — PC/regs/reads
  identical through the whole bank-0 loader.
- **Lykia boots in TRX64**: renders the "Lykia / Play / Prologue" menu (screenshot
  parity with TS).

### Phase 5 — regression sweep
- `lykianew.crt` (Protovision logo) renders in TRX64.
- The 4 real cart families still boot (EF / MagicDesk / GMOD2 / MegaByter).
- `probe-713-devcore` + any CPU diff-tests green.
- No 7-game gate needed (CPU-core change → run the CPU/cart gates only, per
  `feedback_scale_gates_to_change`).

## 4. Tools (concrete)
- **C64RE (TS oracle):** `runtime_session_start` (separate backend), `runtime_media_mount`,
  `runtime_trace_start`/`runtime_trace_finalize`, `trace_store_query`,
  `trace_store_bus_find`, `runtime_query_events`, `runtime_monitor*`. NEVER the live
  :4312 session.
- **TRX64:** `trx64cli` / daemon (separate process); monitor `trace on` + `traceindex`,
  `triage`, `whowrote`, `chis`, `rstep`, `diff`, `d`, `r`, `m`. Reverse-debug backend
  = `crash_triage.rs` / `delta_ring.rs` / `cpu_history.rs`.
- **Diff:** trace-store DuckDB on both → SQL first-divergence (adapt
  `scripts/trace-store-diff.mjs`). VICE = tertiary tiebreaker ONLY if TS and TRX64
  ever both look wrong (they don't — TS runs it); do not make VICE the default.

## 5. Files in scope
- `crates/trx64-core/src/cpu.rs` — the C64 CPU (microcoded Cpu65xxVice port). **Primary suspect.**
- `crates/trx64-core/src/c64_6510core.rs` — the C64 6510 core wrapper (memory-port $00/$01, read/write dispatch).
- `crates/trx64-core/src/full.rs` (bus/PLA read path) — only if the divergence is a READ.
- OUT of scope: `drive_6510core.rs` (separate 1541 CPU), the whole cart layer (ruled out), C64RE/UI/MCP.

## 6. Done definition (gate)
1. First-divergence trace TS↔TRX64 through the bank-0 loader = **empty** (converged).
2. `lykia_rebuilt.crt` boots to menu in TRX64 (screenshot parity with TS).
3. A TS↔Rust diff-test pins the fixed opcode/read against regression.
4. Regression sweep green (Phase 5).
5. Committed (never pushed). Root cause + conversion-family documented in the commit
   + `project_cart_real_samples` memory updated.

## 7. Guardrails
- Separate backend processes for BOTH runtimes; **never** the live UI session.
- Trace into **DuckDB** via trace-store infra; **no one-off JSONL/CSV dump scripts**.
- **First-divergence single record**, never statistics/hotspots.
- Read the **TS oracle source** for the localized opcode/read **before** hypothesising.
- Never push (commit only). Never touch C64RE/UI/MCP source, the cart layer, or the
  drive CPU.

## 8. Open questions / knobs (resolve during the loop, not now)
- Is the divergence A (opcode) or B (read)? — the trace answers in Phase 2.
- Does the raster-sync exit PC need pinning by disasm, or is sequence-fold enough?
- Do the original `lykia_protovision.crt` and the rebuilt `lykia_rebuilt.crt` diverge
  at the SAME first point? (If yes → pure core bug; if only the rebuilt → also check
  the rebuild.) TS runs both, so use whichever reproduces most cleanly.
