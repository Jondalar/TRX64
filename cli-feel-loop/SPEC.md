# CLI-Feel Spec — trx64cli cockpit = "bash for the emulator"

Owner: Alex (TRX/TREX). Authored 2026-07-01. Scope: `crates/trx64-cli/**` +
the daemon monitor/media routing in `crates/trx64-daemon/src/main.rs`. **Never**
touch C64RE UI/MCP. Ground-truth map: `cli-feel-understand` workflow (this dir's
`MAP.md` if dumped; else the file:line anchors below are verified current).

## Vision (user, verbatim intent)

The cockpit should feel like **bash for the emulator**. Three command namespaces:

- `/…` — **emulator control**: mount, umount/eject, dump/undump, power, reset,
  joy, warp, window, settings, run, pause, step.
- `!…` — **filesystem** (the existing monitor FS verbs, just re-prefixed):
  `!pwd !cd !ls !mkdir !rmdir !load !save !bload !bsave`. Feels like a coding
  tool's `!`-shell escape.
- bare — **monitor** commands (`d m r g bk obs trace whowrote diff …`).

Always-on, regardless of namespace:
1. **Tab autocomplete** everywhere (verbs in all 3 namespaces; paths for path args).
2. **Cursor + Backspace/Insert** through the whole line (done — e5a6f21; keep).
3. **Path completion** everywhere a path is an argument — through quotes + spaces,
   multi-candidate → colored list, single → fill, common prefix → fill.
4. **LS_COLORS-lite** — `!ls` output + Tab candidate lists colored by filetype.

### Media semantics (CRT ≠ Disk) — mostly already correct, verify + polish

- **Disk mount** = swap the medium only. No reset, no power-cycle. Floppy state +
  running program survive. (`media/mount` disk branch already does this via
  `mount_disk_media`, main.rs:8891 — NO reset.)
- **CRT mount** = power off → insert → power on cold boot. Atomic, no pause.
  (`media/mount` CRT branch already `fill_power_on_ram`+`cold_reset`, main.rs:8863.)
- **CRT eject** = power off → eject → power on. Cart fully out, like a real C64.
  (`media/unmount` cart branch already persist+detach+`fill_power_on_ram`+
  `cold_reset`, main.rs:8763.)

## Ground truth (verified file:line)

**Dispatch** — `crates/trx64-cli/src/engine.rs`
- `exec_line(&self,line)->CmdResult` :200. `strip_prefix('/')` :208 → VM verb match
  :220-251; else `verb_monitor(line)` :210 → rpc `monitor/exec` :465. **Only `/`
  routing exists; NO `!`.**
- 17 VM verbs :278-list: power reset run pause step mount eject load warp joystick
  window dump restore ringdump ringload help quit. PATH verbs: mount(:332)
  load(:349) run<prg>(:359) dump(:421) restore(:431) ringdump(:443) ringload(:453).
- `/mount`→`media/mount{path}` :332. `/eject`→`media/unmount{}` :342. Unknown `/`
  verb → error, no monitor fallthrough :250. `help_text()` :592.

**TUI** — `crates/trx64-cli/src/tui.rs`
- `Cockpit` :66 { input, cursor(char-idx :70), log, history(Vec :72 in-mem, no
  dedup, no persist), hist_idx, snap, scroll }.
- Char-safe editor helpers: line_char_len :105, byte_at :108, insert_char :115,
  backspace :120, delete_at :127, set_line :134. **Line-editing DONE.**
- Keys in `run_loop` :153: Ctrl-C/D=quit :185 (only Ctrl combos); Tab :194→
  `autocomplete`; Char :200; Backspace :204; Delete :207; Left/Right/Home/End
  :210-219; Up/Down history :222/233; Enter :244. Press-only :181. Raw mode +
  mouse capture, **no bracketed paste** :52.
- `autocomplete` :277 = `/`-verbs ONLY (17-verb const :278); bails on non-slash
  :282 and on space present :283. `longest_common_prefix` :306. `draw_input` :465
  reverse-video block cursor.
- MISSING: Ctrl-A/E/K/U/W/L/R; history persist; dedup; path/monitor/`!` complete.
- GOTCHA: Ctrl+letter (≠c/d) falls through to Char arm → inserts the letter
  (:200 ignores modifiers). Backspace/Delete don't reset hist_idx.

**Monitor FS** — `crates/trx64-daemon/src/main.rs`
- `run_monitor(st,command)` :2778; op=lowercase first token; `match op` :2919;
  unknown → err :5529. Case-insensitive verbs.
- FS verbs: pwd :5379, cd :5380 (sets `st.mon.fs_cwd` :5387), ls/dir :5392
  (per-entry format :5410 = `"  {d|-} {name}"`, header `"{dir}:"` :5413, empty
  `"  (empty)"` :5408), mkdir :5415, rmdir :5423, load :5432 (host→RAM), save
  :5457 (RAM→host), bload :5478, bsave :5505.
- `fs_cwd` field :499; `fs_cwd_now` :2856; `resolve_fs_path` closure :2857;
  `resolve_fs_path_with_state` :11475. `quoted_first` :5698 (double-quote only,
  first pair, no escapes). **NO `!` handling.**

**Media/power** — `main.rs` + `crates/trx64-core/src/lib.rs`
- media/mount :8815 (ext route: .c64re err :8834 / .crt :8837 / else disk :8891).
  CRT branch power-cycle+coldboot :8854-8879; disk branch NO reset (mount_disk_media
  :8931). media/swap :8970 identical. media/unmount :8731 (cart eject power-cycle
  :8763; disk eject no reset :8779). media/ingress :8568 (kind crt HONORS
  resetPolicy :8647; kind eject cart keepRam :8611 — **divergent RAM semantics**).
- Primitives: fill_power_on_ram (RAM wipe = power off) lib.rs:750; cold_reset
  (boots into cart, keeps RAM = power on) lib.rs:868; warm_reset :949;
  attach_cart_from_bytes :846; detach_cart :862; drive attach_disk/detach_disk
  drive.rs:804/826. **No cartridge freeze-button primitive** — compose from
  pause + reset + attach/detach.
- Dual run-state: cli Engine has its own host `running` flag driving `pump_frame`
  (engine.rs:150); daemon has `session.running`. After a control rpc these must
  reconcile or the cockpit view looks frozen. **Suspected root of "CRT mount →
  C64 läuft weiter" — verify.**

**Input routing** — `keymap.rs`/`window.rs`: cockpit (crossterm, tui.rs) and
emulator window (winit, window.rs) are SEPARATE loops; routing = OS window focus.
`keymap.rs` is emulator-only → NO collision with cockpit editor keys. Only shared
input state = `joystick_mode` AtomicU8. **Cockpit editor changes stay in tui.rs.**

## Slices (dependency-ordered; each MUST compile + test + commit before next)

Each commit message: `feat(cli): <slice> [CLI-FEEL Sx]`. Never push.

### S1 — `!` namespace + verb aliases + help (engine.rs)
- In `exec_line` add a `!` branch (before the `/` branch or as a sibling):
  `line.strip_prefix('!')` → `verb_monitor(rest.trim())` (FS verbs live in the
  monitor; `!ls`→monitor `ls`). Empty `!` → short FS help.
- Bare line whose FIRST token ∈ {pwd,cd,ls,dir,mkdir,rmdir,load,save,bload,bsave}
  → return a one-line hint `"filesystem commands live behind '!' — try !ls"`
  (cockpit nudge; do NOT change the shared monitor). Exception: keep `load` too
  ambiguous? No — `/load` is the VM load; bare `load` → hint to `!load`.
- Add VM aliases: `umount`→eject, `undump`→restore, `settings`→new `verb_settings`
  (prints pacing/warp/joystick/mounted-disk/cart summary via existing snapshot +
  a `session/status`-style rpc; read-only).
- Update `help_text()` :592 to show the three namespaces (`/` `!` bare) + aliases.
- Tests: `crates/trx64-cli/tests/` — `!ls` routes to monitor ls; bare `ls` returns
  the hint; `/umount` == `/eject` path; `/settings` returns non-empty.

### S2 — filetype color module (new `crates/trx64-cli/src/ftcolor.rs`, pure)
- `pub fn style_for(name: &str, is_dir: bool) -> ratatui::style::Style` per the
  agreed palette: dir=blue+bold; `.crt`=yellow; `.d64/.g64/.p64`=cyan;
  `.prg/.bin`=green; `.c64re/.c64retrace/.c64rering`=magenta;
  `.asm/.tass/.md/.json`=gray; else default. Case-insensitive extension.
- `pub fn ext_bucket(name)->Bucket` helper for testing.
- Unit tests covering each bucket + uppercase ext + no-ext + dotfile.
- Register `mod ftcolor;` in `lib.rs`/`main.rs` of the crate as needed.

### S3 — `fs/complete` daemon rpc (main.rs)
- New JSON-RPC arm `"fs/complete"` near the FS verbs. params `{ partial: string }`.
  Resolve `partial` against `fs_cwd` (reuse `resolve_fs_path_with_state`): split
  into (dir, stem); `read_dir(dir)`; return `{ entries: [{name, is_dir}],
  common: <longest-common-prefix of matches>, dir: <resolved dir> }` for entries
  whose name starts with `stem` (case-insensitive). Cap 500. Errors soft → empty.
- Handles a trailing `/` (list dir contents), no-arg (list cwd), and a bare stem.
- Test: `crates/trx64-daemon/tests/` or an embedded test — mkdir temp with
  `a.crt`,`a2.crt`,`sub/` → complete `"a"` returns both + common `"a"`; complete
  `"sub/"` lists inside.

### S4 — `!ls` coloring in cockpit (tui.rs, needs S2)
- When the executed line is `!ls`/`!dir` (or any `!` line whose output matches the
  `"{dir}:"` + `"  {d|-} name"` shape), render each entry line with `ftcolor`
  colors instead of pushing raw text. Parse the `d|-` flag col (:5410 format) for
  is_dir; keep the header + `(empty)` sentinel plain.
- Keep non-`!ls` output untouched. Log lines gain optional per-line Style (extend
  `log: Vec<String>` to a styled representation OR post-color on draw — pick the
  lower-churn option; a parallel `Vec<Option<Style>>` or a `LogLine{text,style}`).

### S5 — Tab autocomplete rewrite (tui.rs, needs S1+S2+S3)
- Replace `autocomplete` with a namespace-aware completer:
  - `/` + no space → complete VM verbs (full list incl aliases).
  - `/` + space + PATH-verb → path-complete the last token via `fs/complete` rpc.
  - `!` + no space → complete FS verbs.
  - `!` + space + path-taking FS verb (cd/ls/dir/load/save/bload/bsave/mkdir/rmdir)
    → path-complete.
  - bare + no space → complete monitor verbs (curated const list from MONITOR.md).
  - bare + space → no-op (or address/symbol later — out of scope now).
- Path-complete: take the last whitespace token (respect a leading `"` quoted
  token with spaces — find the quote, treat rest as the path), call `fs/complete`,
  fill common prefix (re-quoting if the result contains a space), and on multiple
  candidates push a COLORED candidate list (ftcolor) into the log. Single dir
  candidate → append `/`; single file → append trailing space.
- Cursor to end after completion (as today, :198).
- Tests: pure-logic parts (token extraction, quote handling, common-prefix fill)
  as unit tests on a helper that does NOT need the rpc; the rpc-backed path tested
  via the engine against a temp dir.

### S6 — readline muscles + history persistence (tui.rs)
- Ctrl-A=home, Ctrl-E=end, Ctrl-K=kill-to-end, Ctrl-U=kill-to-start, Ctrl-W=
  delete-word-before, Ctrl-L=clear log/redraw. Add to the `KeyModifiers::CONTROL`
  check (:185 area) so they don't fall to the Char insert arm.
- Ctrl-C: if line non-empty → clear the line (bash convention); if empty → quit.
  Ctrl-D: quit only when line empty (else delete-at). Keep both quitting when empty.
- History: persist to `~/.trx64/history` (create dir; append on Enter; load on
  boot; cap ~2000). Dedup consecutive duplicates (skip push if == last).
- Backspace/Delete should reset hist_idx=None (fix the noted gotcha) so editing a
  recalled line detaches it.
- Tests: line-editor unit tests for each kill/word op + dedup; history round-trip
  (write temp file, reload).

### S7 — mount-resume verify + eject targeting + settings polish (engine.rs+main.rs)
- Verify (runtime): `/mount <crt>` in the cockpit visibly cold-boots (screen
  changes, pump resumes). If the cli host `running` flag is not set true after a
  power-cycle mount, set it in `verb_mount` after the rpc (reconcile dual run-state,
  engine.rs). Same for `/eject`.
- `/eject` smart target: if a cart is mounted → eject cart; else if a disk →
  eject disk. Pass an explicit role/slot to `media/unmount` so it is unambiguous
  (currently is_cart = role=='cartridge' OR slot==0, main.rs:8741 — verb sends
  `{}`). Add a `session/status`-ish read so `verb_eject` knows what's mounted, or
  send `{ role:"auto" }` and resolve in the daemon.
- Unify eject RAM semantics: make `media/ingress` kind:eject cart branch match
  `media/unmount` (power-cycle, RAM wiped) OR document why they differ. User's
  model = full power-cycle (RAM wiped).
- Tests: `/mount <crt>` then assert cart attached + a cold-boot marker; `/eject`
  after a disk mount ejects the disk not a (absent) cart.

### S8 — docs (MONITOR.md + cockpit doc + help)
- `MONITOR.md`: add a short "trx64cli cockpit namespaces" note (`/`=machine,
  `!`=filesystem, bare=monitor) + that Tab completes verbs and paths.
- New `crates/trx64-cli/README.md` (or `docs/cockpit.md`): the namespace model,
  autocomplete, colors, media semantics, readline keys.
- Ensure `help_text()` (S1) and MONITOR.md agree.

### S9 — final gate (no code, just prove)
- `rtk cargo build --release` (updates the symlinked `/usr/local/bin/trx64cli`).
- `rtk cargo test -p trx64-cli -p trx64-daemon` green.
- Write `cli-feel-loop/TEST-CHECKLIST.md` — the 60-second manual test the user runs
  this afternoon (each vision bullet → one keystroke sequence + expected result).
- Final commit; do NOT push. PushNotification one-line summary.

## Gate tiers (per "scale gates to change")
- Per slice: `rtk cargo check -p <crate>` + `rtk cargo test -p <crate> <scoped>`.
- S9 only: `rtk cargo build --release` + full `-p trx64-cli -p trx64-daemon` tests.
- No C64RE build, no probe-single-path, no 7-game gate (this is CLI-only).

## Guardrails
- NEVER push (commit only; user pushes).
- NEVER touch C64RE (`../C64ReverseEngineeringMCP`), UI, or MCP.
- Shared monitor (`run_monitor`) FS verbs stay bare-callable (C64RE depends on
  them via `runtime_monitor`); the `!` namespace is a COCKPIT routing layer only.
- Each slice: compile + test + self-review before commit. No half-slices.
- If a slice is genuinely blocked, mark it `blocked` in state.json with the reason
  and continue to the next unblocked slice; surface blockers in the final summary.
