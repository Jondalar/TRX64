# Spec 804 — Symbolized runtime surface: names everywhere, and as data

**Status:** PROPOSED
**Repos:** TRX64 (every surface that prints or returns an address) + C64RE (the durable name
store it already owns; no second renderer).
**Number:** 804 (shared board `C64ReverseEngineeringMCP/specs/README.md`).
**Framing:** not "add labels to the memory dump" but **close the symbol surface and make it
machine-readable** — names on every rendered surface, and a `symbol` field next to the raw
address in every structured response, each name carrying where it came from.

---

## 1. What exists today (verified, not assumed)

**The machinery is there.** The monitor has `sym`, `label`, `unlabel`, `note`, `save_labels`/`sl`,
`load_labels`/`ll` (`crates/trx64-daemon/src/main.rs:5419-5508`). Persistence goes to
`<project>/knowledge/labels.user.json` through `project_knowledge.rs`; `label` also writes a
`memory-address` entity to `knowledge/entities.json` and `note` a finding to
`knowledge/findings.json`. `save_labels` emits a VICE `.sym` (`al C:<hx> .<label>`), and
`parse_sym_line` (`project_knowledge.rs:419`) reads VICE `al`, KickAssembler `.label n=$x` and
plain `n = $x`.

**One consumer.** `user_label_index()` (`project_knowledge.rs:397`) is the addr→name index, and
across the whole daemon it has **exactly one call site**: `main.rs:3482`, inside the `d`/`disass`
verb. `disasm_line_ts_labeled` (`crates/trx64-static/src/disasm6502.rs:97`) — which appends
`; → name` for a branch/JSR target and prepends `name:` for the instruction's own address, keeping
both the name and the numeric address — is used at `main.rs:3499` and `:3515` and nowhere else.

Which surfaces symbolize:

| surface | symbolizes? | evidence |
|---|---|---|
| `d` / `disass` | **yes** — user labels only | `main.rs:3482`, `3499`, `3515` |
| `sym <name>` | **yes** — but from analysis JSON *only* | `project_read_sym`, `project_knowledge.rs:1013` |
| `inspect` / `xref` | yes — analysis JSON | `main.rs:5419-5431` |
| `resolvePc` / `resolvePcs` (WS) | yes — routine/label/segment/source | `main.rs:7119-7135` |
| `m` / `mem` | **no** — zero lookups | `main.rs:3400-3450` |
| `chis` | **no** — calls the *unlabeled* `disasm_line_ts` | `format_chis_from_ring`, `main.rs:3029` |
| `bt` | **no** — raw `$XXXX` return addresses | `main.rs:4592` |
| `whowrote` / `rstep` | **no** — raw writer PCs and target addresses | `main.rs` reverse-debug arms |
| `map` | **no** on the monitor path | `MapOptions.static_ranges`: *"via the monitor `map` verb the list is always empty"* |
| `swimlane` / `taint` / `follow_path` / `profile_loader` / `query_events` | **no** — cannot | `trx64-traceindex` depends only on `trx64-trace` + `duckdb`; it has no path to project knowledge |
| `monitorMemory` (JSON) | **no** — returns a bare `[u64]` with no addresses at all | `main.rs:6708` |
| `monitorDisasm` (JSON) | **no** — `disasm_one`, unlabeled | `main.rs:6719` |
| trace rows (JSON) | **no** | `rows.rs:93-101` → `{"addr":1024,"value":65,"op":"write","pc":49155,"side":"c64","oldValue":32,"cycle_c64":555}` |

Three defects fall out of that table, each verified in code:

1. **`sym` and `d` disagree about what a symbol is.** `d` reads the user-label store; `sym` scans
   `*_analysis.json` effective segments and never opens `labels.user.json`. So `label $c000 foo`
   followed by `sym foo` answers *"no symbol named \"foo\" in the project analysis"*.
2. **C64 labels leak into 1541 disassembly.** `d` picks `device drive8` at `main.rs:3484`, *after*
   loading the index at `:3482`, and hands the same C64-space names to the drive's address space.
   The VICE `.sym` format actually carries this dimension — `C:` is a memspace prefix (`C:` =
   computer, `8:` = drive 8) — and `parse_sym_line` **discards it** (`ci <= 1` → skip).
3. **A loaded build symbol file becomes indistinguishable from hand-typed work.** `load_labels`
   upserts every parsed line as a plain `label-override` record with no origin, so after loading a
   build's `.sym` nothing can tell a generated name from one a person reasoned their way to.

**The build side.** C64RE's `assemble_source` runs KickAssembler (`java -jar KickAss.jar <src> -o
<out>`) and 64tass (`64tass -a -B -o <out> <src>`) — `src/assemble-source.ts:90-106` — and
**neither is invoked with a symbol flag today**, so the build currently produces no address map at
all. Both can, one flag away, and both formats were measured rather than assumed:

```
$ java -jar KickAss.jar t.asm -o t.prg -vicesymbols     # single dash; writes t.vs
al C:816 .print_string
$ 64tass -a -B -o u.prg --vice-labels -l u.vs u.tass    # hyphenated; needs -l
al 810 .start
```

Both parse with the **existing** `parse_sym_line`, unmodified — the `C:`-prefixed and bare forms
alike. KickAssembler additionally writes `<stem>.sym` in its own `.label print_string=$816` form,
which that parser also already reads. Two notes worth carrying: the VICE file extension is `.vs`,
not `.sym`; and in the measurement **neither assembler exported `=` constants** (a `screen = $0400`
equate appeared in no output file) — build symbols name *code and data the build placed*, not
hardware registers.

## 2. Problem

The names exist and reach one verb. Everywhere else a person and a program both get `$C1A3`.

That is two separate costs. For a human at the monitor it is re-derivation: the memory dump, the
backtrace, the CPU history and the trace lanes all show the address that the disassembly two lines
up was willing to call `party.hp`. For a program it is worse, because the one surface that *does*
symbolize puts the name **inside a formatted text line**. A consumer that wants the symbol has to
parse `$C1A3` and `; → party.hp` out of a text column whose width is set by
`format!("{:<30}")`-style padding. That consumer breaks the day a column moves.

Text output is made for eyes. `{"addr": 49571, "symbol": "party.hp", "value": 5}` is made for
programs. This is the difference between a test suite that survives a formatting change and one
that does not — and it is why "symbolize the surfaces" and "expose symbols as a field" are one
spec and not two: doing only the first would put the names in the one place a machine cannot
safely read them.

## 3. Goals

1. **Every rendered surface that prints an address can print its name**, from one resolver, with
   the numeric address always still there.
2. **Every structured response that carries an address carries its symbol as a field**, not only
   inside a text line.
3. **Every symbol says where it came from**, and conflicts resolve by a stated rule rather than by
   whichever layer happened to load last.
4. **Nothing is worse without symbols.** With no symbol file and an empty label store, output is
   what it is today.
5. **The build's `.sym` is usable as-is** — no new format, no new parser, no new store.

### Non-goals

- **Structure.** `party[0].hp` — types, widths, arrays, records, endianness, "this 40-byte block
  is five 8-byte party members" — is a *schema over* symbols and belongs with whatever consumes
  it. 804 is flat address → name, full stop. The cut is deliberate: **804 has to be worth building
  even if nothing downstream ever happens.** A memory dump that shows `party.hp` instead of
  `$C1A3` is better for a person at the monitor on day one, with no consumer, no schema and no
  test harness. A spec whose only value is as a stepping stone rots while it waits for the thing
  it was a step toward.
- **No new symbol FILE format.** VICE `.sym` is what both assemblers emit and what
  `parse_sym_line` already round-trips (§1, measured).
- **No new store and no migration.** `knowledge/labels.user.json` stays exactly as it is —
  see §4, where the build layer is deliberately *not* persisted at all.
- **Not the snippet assembler, not the test harness.** See §7.
- **Not notes.** `note` text stays where it is; annotating a dump with prose is a different thing
  from naming an address.
- **Not bank-qualified symbols** in v1 — open question 4.

## 4. Where a symbol comes from, and who wins

A name at an address has one of three origins, and the difference matters at the moment somebody
reads it:

| origin | layer | authority | lifetime |
|---|---|---|---|
| `build` | the assembler's `.sym`/`.vs` for the binary now running | **authoritative for ADDRESSES** — only the build knows where a name landed after relocation and bank layout | regenerated by every build |
| `user` | `knowledge/labels.user.json` (+ entities/findings) | **authoritative for MEANING** — a person or an LLM reasoned about this address | durable; survives rebuilds, which is exactly the risk |
| `derived` | `*_analysis.json` effective segments, heuristic names | a guess | recomputed |

When `$C1A3` renders as `party.hp`, the reader must be able to see **why** — otherwise they trust
a name left over from an analysis that a rebuild silently invalidated. So:

- **The three layers stay physically separate.** `user` is the existing durable store, untouched.
  `build` is loaded from a file into daemon session state, keyed by its path + mtime, and **never
  written back** — it is a fact about the running binary, not knowledge, and persisting hundreds of
  generated names into a curated store would both pollute it and let it drift from the build.
  `derived` is computed on demand, as today. This is why "no change to how symbols are stored" is
  literally true: nothing new is stored.
- **Precedence for the single rendered name: `user` > `build` > `derived`.** A user label is a
  deliberate act at a specific address; a build name is correct-by-construction but anonymous
  intent; a derived name is a guess. Nothing is discarded — `sym` reports every candidate with its
  origin, so an override is inspectable rather than a disappearance.
- **The origin is visible at the point of reading**, not only on request: a one-character tag
  after the name in text (`party.hp[u]`, `print_string[b]`, `sub_c1a3[?]`), and an `origin` field
  in structured responses.
- **Residual risk, stated rather than glossed:** precedence alone cannot detect a *stale* user
  label. If a rebuild moved `party.hp` and the user label stays at the old address, `user > build`
  renders a confidently wrong name. The origin tag makes it *visible* (`[u]` on an address the
  build calls something else) but does not make it *safe*. The invalidation policy is open
  question 1 — deliberately not decided here, because the cheap answer (auto-demote on load)
  destroys RE work on foreign binaries that have no build at all.

**Address space is part of the key, already, in v1.** `(space, addr)` with `space ∈ {c64, drive8}`
— not because it is elegant but because the leak in §1(2) is live today and the VICE format
already carries the memspace letter our parser throws away. Fixing the parser to keep it costs
less than the bug does. Cart banks are the same class of problem one size up, and are open
question 4.

## 5. Scope

### 5.1 One resolver, called from everywhere

A single `symbols` module in the daemon: `resolve(space, addr) -> Option<Symbol { name, origin }>`
and its batch form, layering user → build → derived per §4. Every surface below calls it; no
surface keeps a private lookup. `user_label_index()` becomes an input to that resolver and stops
being called directly.

**Rendered surfaces to close:** `m`/`mem`, `chis`, `bt`, `whowrote`, `rstep`/`reverse`, `map`
(feed the `static` column, which is `-` on the monitor path today), and the trace renderers
`swimlane` / `taint` / `follow_path` / `profile_loader`. `d` keeps working and gains the build
layer + origin tag. `sym` gains the user and build layers so it stops contradicting `d` (§1.1).

Two rules that make this safe to land:

- **Symbolization never moves an existing column and never removes the numeric address.** Names
  arrive as trailing annotation. A 32-byte `m` row keeps its grid and gains a trailing
  `; +03 party.hp[u]  +0b party.mp[b]` for the symbols falling inside it; monitor transcripts
  keep their shape.
- **The join happens in the daemon, never in `trx64-traceindex`.** That crate is format + query
  (Spec 802) and must not learn about projects, labels or meaning. Trace reads return addresses;
  the daemon symbolizes them on the way out.

### 5.2 The `symbol` field

Every structured response object that carries a machine address gains a sibling key with
`_symbol` appended: `addr` → `addr_symbol`, `pc` → `pc_symbol`. Absent when nothing resolves —
absent, not `null`, matching the existing house convention (`resolvePc` omits absent layers).

The suffix rule rather than a bare `symbol` is a deliberate choice: a trace row carries **two**
addresses (`{"addr":1024,…,"pc":49155,…}`, `rows.rs:93-101`), so a single `symbol` key would be
ambiguous exactly where the data is densest. One mechanical rule — *for every key holding an
address, a sibling key with `_symbol`* — lets a consumer symbolize generically without a table of
per-op special cases.

Origin does **not** go on every row: a per-response `symbols` map carries it once,
`{"party.hp": {"addr": 49571, "space": "c64", "origin": "build"}}`, so a firehose row pays one
string and provenance is still in the same payload.

Surfaces: `monitorDisasm`, the trace/read structured results (`query_events`, `swimlane`, `taint`,
`follow_path`, `profile_loader`), `resolvePc`/`resolvePcs`, and the reverse-debug results.

`monitorMemory` is the exception and is left alone: it returns a bare `[u64]` with no addresses in
it at all (`main.rs:6716`), so there is nowhere to hang a symbol without changing its shape and
C64RE's `runtime_monitor_memory` consumer with it. Instead, a batch op —
`symbols/resolve {space, addrs[]} -> {symbols: {…}}` — serves it and any other consumer that wants
to symbolize addresses it already holds. That op is also what a UI needs: it can name anything it
displays without every op growing a field.

### 5.3 Loading the build's symbols

`load_labels` gains an optional origin so a build file lands in the build layer rather than the
durable store; with no origin argument it behaves exactly as today (durable, `user`), so existing
scripts and transcripts are unaffected. Reload is idempotent and replaces the layer wholesale — a
build map is a snapshot, not an accumulation.

Auto-discovery (find the `.vs`/`.sym` next to the loaded binary and load it on attach) is a
convenience with a defined search order, and whether it should happen at all is open question 2.

### 5.4 The inverse direction: symbolic operands in `a`

The inline assembler (`assembler.rs`) accepts `$hex`, decimal, binary and bare hex only, so
`a jsr print_string` fails with `bad operand 'print_string'`. Resolving a *single* operand through
the same table is the smallest useful form of name→address and makes the day-one value concrete.

One hazard, named because it is easy to get wrong: `abc` is valid bare hex *and* a plausible
symbol name. **Numeric parse wins; the symbol table is consulted only when the operand does not
parse as a number.** `a lda abc` therefore still assembles `$0ABC` even with a symbol named `abc`
loaded, and every existing `a` transcript stays byte-identical.

This is *not* the snippet assembler (§7).

## 6. The Leitregel seam

Addresses are capability; names are meaning. This spec sits exactly on that seam, so it says
explicitly which side does what:

- **TRX64 performs the join and owns the `build` layer.** Only the runtime knows what is mapped at
  `$C1A3` right now — which bank, which PLA config, which CPU — and only the build knows where a
  name landed after relocation. Both are facts about the machine. TRX64 also *has* to do the join:
  it is the process rendering the line, and Spec 802 forbids spawning out to have someone else do
  it.
- **C64RE owns the `user` layer and stays the authority on which names are true.** TRX64 reads and
  writes it only through the existing explicit verbs and the existing `project_knowledge.rs`
  bridge; it never invents, curates or garbage-collects meaning. And per 802 §4.3, C64RE does not
  grow a second renderer — it consumes the `_symbol` fields rather than re-deriving names beside
  them.
- **Provenance is what keeps the seam legible.** `[b]` is the build's fact, `[u]` is C64RE's
  claim, `[?]` is a heuristic. A reader can always tell which authority they are trusting, which
  is the whole reason the origin is on the surface and not in a log.

## 7. Why this matters beyond nicer output

Two things become possible once a name table exists for the running binary. **Neither is in this
spec** — they are stated because they are the reason the field in §5.2 is not a cosmetic
preference:

1. **Snippet-level iteration.** A patch that replaces one routine has to resolve calls into
   existing code: `jsr print_string` must become `jsr $0816` using the address the *last full
   build* put it at. Without the build's symbol table you can only inject code that touches
   nothing — self-contained bytes that call nobody. The overlay → candidate → delta chain (Specs
   795/796/797) today takes pre-assembled bytes or a standalone source file for exactly this
   reason. **Out of scope here:** the snippet assembler itself — multi-line input, forward
   references, local scopes, `org`/relocation, linking against the build.
2. **Tests that assert on named state.** `party.hp == 5` instead of `peek(0xC1A3) == 5` — an
   assertion that survives a rebuild moving the address, and that reads as a claim about the game
   rather than about a number. **Out of scope here:** the assertion language and harness, which is
   a schema over symbols (§3 non-goals).

Both are downstream. 804's own justification is §3: the monitor gets better with nothing
downstream at all.

## 8. Deliberate changes to existing output

Named up front so nobody "restores parity" later:

- `d` output changes **where a label already exists** — the name gains an origin tag, and the
  build layer may name addresses that were bare before. The unlabeled case is unchanged.
- `sym` starts answering for user labels and build symbols, so a query that previously errored
  now succeeds. That is the §1(1) defect being fixed, not a regression.
- `d` on `device drive8` **stops** showing C64-space labels (§1(2)). Anyone relying on that was
  reading a wrong name.

## 9. Acceptance

1. **One resolver, no second path.** `grep -rn "user_label_index" crates/trx64-daemon/src/` finds
   it only inside the `symbols` module; every surface in §5.1 has a call site to the resolver.
2. **`grep -rn "label\|symbol\|project" crates/trx64-traceindex/src/`** shows no symbol lookup
   (the MARK label and `static_label` fields are the pre-existing, unrelated hits) — the join did
   not leak into the reader crate.
3. **The memory dump names things.** With a label at `$C1A3`, `m $c1a0 $c1bf` shows it; the byte
   columns and the `>C:XXXX` prefix are byte-identical to the pre-804 row.
4. **The JSON field is there and typed.** A trace-row read over a fixture store returns
   `addr_symbol` and `pc_symbol` where symbols resolve, omits both keys where none do, and the
   response's `symbols` map carries `origin` for each name.
5. **Provenance is visible.** The same address carrying a `user` and a `build` name renders the
   user name with `[u]`; `sym <name>` lists both candidates with their origins and addresses.
6. **Absent symbols change nothing.** With no build file loaded and no
   `knowledge/labels.user.json`, `scripts/gate.sh` is green and the monitor transcript is
   byte-identical to the pre-804 build; no `_symbol` key appears anywhere (absent, not `null`).
7. **Real assembler output loads unmodified.** Fixtures captured from KickAssembler
   `-vicesymbols` (`al C:816 .print_string`) and 64tass `--vice-labels` (`al 810 .start`) both
   load, and the KickAssembler `C:` memspace routes to the `c64` space.
8. **The drive8 leak is closed.** A regression test: a `c64`-space label at `$C000` does not
   appear in `d` output under `device drive8`; an `8:`-prefixed label does.
9. **Numeric-first in `a`.** `a jsr print_string` assembles when the symbol resolves;
   `a lda abc` still assembles `$0ABC` with a symbol named `abc` loaded.
10. **The batch op answers a mixed request.** `symbols/resolve` over a batch containing a user
    address, a build address, a derived address and an unnamed one returns three entries with
    distinct origins and no entry for the fourth.

## 10. Open questions

1. **Build `.sym` vs a stored label at the same address.** §4 gives the user label the render, but
   not a staleness check. Should loading a build map *flag* (or demote) user labels at addresses
   the build now assigns elsewhere? Auto-demotion is cheap and wrong for foreign-binary RE, where
   there is no build at all and every name is a user label.
2. **Per-session or per-project?** The durable layer is per-project by construction. The build
   layer is a property of the binary currently running — per-session — but under one-machine-
   per-process, sessions *attach* to the same machine. So an LLM loading a `.sym` would change
   what the human co-driving the same session sees. Is that right (one machine, one truth) or
   surprising (my monitor renamed itself)?
3. **Stale `.sym` warning.** Path + mtime come for free with the build layer, so warning when the
   symbol file predates the binary is nearly free — except that the "binary" may be a `.crt`, a
   `.d64` or an undumped snapshot with no build relationship at all. What is the reference point
   when there is no build?
4. **Bank-aware symbols.** With 8 MiB banked cartridges (Spec 803), `$8000` in bank 3 and `$8000`
   in bank 200 are different code, so an address alone is not unique. Does the key become
   `(space, bank, addr)` — and does the build even *know* the bank? A KickAssembler `.vs` has no
   bank column, and VICE's `C:` prefix is a memspace, not a bank. v1 keys on `(space, addr)` and
   leaves this open rather than inventing a format extension nothing emits.
