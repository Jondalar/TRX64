# Spec 802 — TRX64 stands alone: native trace read, zero external processes

**Status:** PROPOSED
**Repos:** cross-repo — TRX64 (native reader, drop the spawns) and C64RE (stop reading trace
stores itself; consume TRX64).
**Number:** 802 (shared board `C64ReverseEngineeringMCP/specs/README.md`).
**Trigger:** an external bug report (Windows 11, 2026-08-06): every trace-read op fails with
`trace/read sidecar spawn failed — Node/tsx is required`. The report is accurate; what it exposed
is an architecture violation, not a missing dependency.

---

## 1. The rule this spec enforces

> **TRX64 is the runtime AND the monitor.** Every monitor verb, `dump`, `ringdump`, trace capture
> **and trace read** runs autonomously — no Node, no TypeScript, no C64RE source tree, no spawned
> helper of any kind.
>
> **C64RE uses TRX64 and does none of it itself.** It does not carry a second reader for the same
> data.

Everything below follows from that. It is the Leitregel taken literally: capability — including
reading back what the runtime just recorded — belongs to TRX64; C64RE owns meaning, memory and
the knowledge layer.

## 2. Problem

TRX64 **writes** its trace format in Rust and **cannot read it in Rust**. Every read op shells out
to a Node/TypeScript sidecar which dynamically imports the *C64RE* TypeScript:

```
consumer → WS → trx64-daemon (Rust) → spawn tsx
                                       → TRX64/tools/trace-read-sidecar/sidecar.ts   (364 LOC)
                                           → dynamic import of C64RE's runtime modules
                                               → DuckDB
```

The Rust daemon is a **pass-through**. Typing `map` in the TRX64 monitor executes C64RE
TypeScript. A C64RE tool asking the daemon for trace data is a **round trip back into C64RE's own
code** — the layering is not just violated, it is circular.

Confirmed consequences:

1. **It works only on the author's machine.** The sidecar needs Node + `tsx`, *and* the C64RE
   source on disk, *and* a correct root path. `tools/oracle/node_modules/` is gitignored, so a
   fresh clone silently has no reader: the build is green, the feature is dead.
2. **On Windows no path works at all.** `resolve_tsx()` prefers the extensionless npm shim (a
   POSIX script `CreateProcess` cannot execute); the bare-`tsx` fallback only ever resolves as
   `tsx.exe`, which npm does not ship. `npm install` moves the error, it does not fix it.
3. **The container cannot read traces.** The published image has no Node — unnoticed because
   nobody has read a trace inside the sidecar yet.
4. **`trace_finalize` reports success on unreadable data.** Index building goes through the same
   sidecar; a failed read leaves a complete `.c64retrace`, no index, and a healthy-looking
   summary. The failure surfaces later, somewhere else.
5. **A hard-coded author path** (`/Users/alex/Development/C64/Tools/TRX64`) is the final fallback
   of `trx64_root()` — in a public repository.
6. **A second, quieter spawn:** `media/browse` runs `node -e` to sort filenames with
   `localeCompare`, because Rust's ordering differs from the TS implementation it was matched to.
   It degrades to an ASCII sort instead of failing, so nobody noticed — but it is the same
   disease: TRX64 imitating C64RE instead of being authoritative.

## 3. Goals

1. **Zero external processes in the daemon.** `Command::new` disappears from the runtime path.
2. **Native trace read** for every op the daemon exposes, with no format change.
3. **Works everywhere the daemon runs**, container included, from a clean clone with only Rust.
4. **C64RE stops duplicating**: its trace tools are thin calls to TRX64; it keeps no second
   reader on the customer path.
5. **Honest failures**: capture never reports success on data that cannot be read.

### Non-goals

- No `.c64retrace` / index **format** change. Existing stores keep working.
- Not moving analysis out of the monitor — `map` / `taint` / `swimlane` / `chis` are monitor verbs
  and stay TRX64's. A debugger answering questions about the run it just recorded is capability.
- Not touching capture, `dump`, `ringdump`, `snapshot/*` — verified already native (zero external
  calls). They are the model the read path should have followed.

## 4. Scope

### 4.1 TRX64 — port the reader (`tools/trace-read-sidecar/sidecar.ts`, 364 LOC)

| op | args | notes |
|---|---|---|
| `index` | `{wait}` | build/refresh the `.duckdb` index from the `.c64retrace` — everything else depends on it |
| `store_fn` | `{fn, …}` | `getInfo`, `topPcs`, `findBusEvents`, `listAnchors`, `findAnchor`, `safeQuery`, `sql` |
| `map` | `{cpu}` | memory map |
| `taint` | text | taint analysis |
| `swimlane` | text | swimlane / `chis` rendering |

The writer, the format definition and the anchor model are already Rust — this is **reader +
query**, not a re-derivation.

### 4.2 TRX64 — the two spawns and the hard-coded path

- Delete the `tsx` spawn together with `tools/trace-read-sidecar/` and `resolve_tsx()` — **after**
  the parity gate (§5.2), never before.
- `media/browse`: sort natively and make TRX64's order authoritative. Matching a TS
  `localeCompare` was right while TS was the golden implementation; it no longer is.
- `trx64_root()`: drop the hard-coded fallback **now**, independently of the port — return an
  error naming `TRX64_ROOT` instead of pointing at a directory that exists on one machine.

### 4.3 C64RE — consume, do not duplicate

- `trace_store_*`, `runtime_query_events`, `trace_memory_map`, `runtime_swimlane_slice`,
  `runtime_trace_taint` become **pure calls to TRX64**. The in-process reader fallback
  (`daemonTraceRead`'s local branch) goes, exactly like the runtime fallbacks cut earlier: one
  path, no silent second implementation.
- The TypeScript reader modules leave the customer path. They may survive as internal dev/parity
  material (they are the reference for §5.2), but nothing a user reaches goes through them.

## 5. Design + acceptance

### 5.1 Design notes

- **DuckDB from Rust** keeps the format and the existing stores. Candidate: the `duckdb` crate —
  verify up front that it builds on linux-x86_64 / macOS-arm64 / windows-x86_64 **and** in the
  slim container image. A native dependency that fails on one target would merely relocate the
  portability problem this spec exists to remove.
- **The indexer belongs next to the writer** (both Rust, same format knowledge). This also fixes
  (4): index failures become visible at `trace_finalize`.
- **Text renderers last** — they are formatting over query results; keep the output shape so
  monitor transcripts do not change.

### 5.2 Acceptance

1. Every op answers with **no Node installed** and **no C64RE checkout present**.
2. **Parity gate** (the condition for deleting the sidecar): over a recorded corpus of
   `.c64retrace` files, native output equals sidecar output for every op — exact for structured
   results, whitespace-normalised for text renderers.
3. Trace reads work **inside the container image**, with no Node added to it.
4. `grep -rn "Command::new" crates/trx64-daemon/src/` is empty.
5. `grep -rn "/Users/alex" crates/` is empty.
6. A failing index build fails `trace_finalize` loudly.
7. C64RE has **no** local trace-read fallback left on the customer path.
8. Windows: `trace_store_*` works from a clean clone with only Rust installed.

## 6. Open questions

1. **`sql` / `safeQuery`** — a raw SQL passthrough. Keep it (useful) or drop it with the port
   (smaller surface on a daemon that binds `0.0.0.0` in the container)?
2. **DuckDB linkage**: vendored/static (no runtime shared library, bigger image) vs dynamic.
3. **Sequencing**: `index` + `store_fn` first (unblocks everyone, including the reporter), text
   renderers second — or everything behind one parity gate?
