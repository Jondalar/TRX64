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
3. ~~Trace reads work **inside the container image**~~ — **mis-specified, replaced.** The
   property that matters is "no Node dependency", and criterion 4 proves that directly and
   better. The container is a *play* deployment; it only has to **build**. (It does: verified
   below.)
4. `grep -rn "Command::new" crates/trx64-daemon/src/` is empty.
5. `grep -rn "/Users/alex" crates/` is empty.
6. A failing index build fails `trace_finalize` loudly.
7. C64RE has **no** local trace-read fallback left on the customer path.
8. Windows: `trace_store_*` works from a clean clone with only Rust installed.

## 6. Decisions (the questions this spec opened, and how they were answered)

1. **`sql` / `safeQuery`** → **`safeQuery` ported as-is** (guard verbatim: lowercase+trim, must
   start with `select`/`with`, else `only SELECT/WITH queries are allowed`; 200-row cap). The
   raw `sql` op is **dropped** — it had no caller.
2. **DuckDB linkage** → **bundled/static**, so the binary needs no system `libduckdb` anywhere.
   Measured cost: daemon 4.1 → **33.1 MB** stripped, image download +11.4 MB (32.2 → 43.6 MB
   compressed), image build 1–2 min → **9 min 37 s** (of which ~7 min 40 s is the DuckDB C++
   compile). Accepted. The dependency is confined to `trx64-traceindex`, so the FFI staticlib
   and the encoder crate stay slim.
3. **Sequencing** → **one shot**, one parity gate at the end.

## 7. Deliberate divergences from the sidecar — do NOT "fix" these

The parity gate is about being *right*, not about being bug-compatible. These differences were
each found by measurement, examined, and kept. Anyone who later sees a diff here should read
this section before "restoring parity".

**The native side is deliberately the better one:**

- **SQL injection is blocked.** `topPcs` with `cpu: "c64' OR '1'='1"` — the **sidecar executes
  the injection and returns real rows**; native returns `[]` (`queries.rs` `sq()` escaping).
  Native is correct. Never align this.
- **Malformed `limit` values do not crash or misbehave.** The sidecar's handling of these is
  JavaScript coercion accidents, not designed behaviour, so they are not reproduced:
  `limit:"zz"` → JS emits `LIMIT NaN` and raises a **SQL error**, native uses the default;
  `limit:[3]` → JS coerces the array to `3`, native uses the default; `limit:4.9` → JS yields
  5 rows, native 4; `limit:-5` → JS `slice(0,-5)` means "all but the last 5" (8514 rows),
  native returns 0. Porting these would mean porting bugs.

**Accepted losses:**

- **Legacy (Spec-217 "Shape-A") stores may break.** The `index` op reads `trace_event`
  unconditionally (`ensure.rs`) and errors on a Shape-A store, while `get_info` happens to be
  shape-aware. This inconsistency is accepted: every store in active use is Shape-B (726) —
  verified across Wasteland, LN3, Pawn, Lykia, Murder and Scramble — and the only Shape-A
  material left is old VICE-baseline capture from a retired oracle.
- **Internal SQL is single-line in Rust, multi-line in TS.** DuckDB echoes the failing query
  into its error text, so an error raised by an *internal* query differs in line breaks.
  Caller-supplied SQL (`safeQuery`) is byte-identical. Cosmetic, only reachable on a
  malformed/legacy store.
- **Index-build errors keep their context** (`open index store …: IO Error: …`). The sidecar
  delegated `index` to C64RE's TS indexer, whose failure text is a different implementation's
  entirely — there is no text to be equal to.

**A defect of the sidecar, found while proving parity against it:** the sidecar ends
`process.stdout.write(JSON.stringify(out)); process.exit(0)`. Over a pipe — exactly how the
daemon read it — `exit()` truncates at one pipe buffer: measured **89,384 bytes to a file,
exactly 65,536 through a pipe**. 17 of 389 results in the parity corpus exceeded 64 KiB, which
means *every* real structured `swimlane` request returned malformed JSON to the daemon. The
feature was broken in production and nobody noticed, because outside the author's machine the
sidecar never ran at all. Parity for those ops is therefore measured against the sidecar's
*intent* (stdout redirected to a file), not its shipped behaviour.

## 8. What is proven, and what is not

**Proven:** 562 + 787 sidecar-vs-native comparisons. `index`, `getInfo`, `topPcs`,
`findBusEvents`, `listAnchors`, `findAnchor`, `safeQuery`, `map`, `taint`, `taint_text`,
`swimlane`, `swimlane_text`/`chis`, `query_events`, `follow_path`, `profile_loader`, the whole
error/edge surface (36/36 error texts byte-identical). Workspace: 813 tests pass, 2 fail — both
verified **pre-existing** by running them on the parent commit (`trx64-ffi/tests/smoke.rs`,
autonomous-loop guard, unrelated to this work). `trx64-traceindex`: 166 unit + 12 integration,
0 failures. Both hard gates empty under adversarial widening.

**Not proven, stated rather than glossed:**

- **`taint` beyond one node.** Every comparison that returned data returned a 1-node, 0-edge
  graph; multi-node BFS, the IRQ-boundary branch and the cross-domain IEC bridge were never
  exercised, because no corpus store has both a populated `bus_access` channel and meaningful
  IEC traffic.
- **`topPcs` and `swimlane` ordering.** Both are nondeterministic on **both** sides — `ORDER BY`
  has no tie-break — proven by self-instability (6 identical repeats gave 5 distinct orderings
  on each side). Not a port defect, but not a parity claim either. A deterministic tie-break
  would have to be added on both sides for parity on them to mean anything.
- **Platform coverage.** The gate ran on macOS-arm64 only. Windows (§5.2-8) is unverified;
  the container was built arm64, not amd64.
