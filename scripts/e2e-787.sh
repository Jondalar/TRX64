#!/bin/sh
# ─────────────────────────────────────────────────────────────────────────────
# scripts/e2e-787.sh — Spec 787 (Scoped TRX64 Instances) acceptance gate.
#
# v1 model (DECIDED): a "scratch" TRX64 instance = a short-lived `trx64cli`
# process (`sandbox` / `boot`). There is NO daemon `spawn-scratch` verb — the OS
# gives process isolation for free; the daemon stays attach-only (one-live). This
# gate prints GREEN/RED for each of the 7 acceptance points in Spec 787 §5.
#
# Exit: 0 = all blocking points GREEN. 1 = a point RED. 2 = environment error.
#
# Env knobs:
#   E2E787_SKIP_BASELINE=1   skip point #7 (scripts/gate.sh is heavy: release
#                            build + 7-game behavioural gate). Reported as SKIP,
#                            NOT a false green.
#   C64RE_ROOT=              C64RE checkout (for the TS single-path probe + ROMs).
# ─────────────────────────────────────────────────────────────────────────────
set -u

REPO_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd) || exit 2
cd "$REPO_ROOT" || exit 2
C64RE_ROOT="${C64RE_ROOT:-/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP}"
CLI="$REPO_ROOT/target/release/trx64cli"

# Per-point verdicts (space-separated "N:VERDICT:detail" — detail uses '_' for spaces).
RESULTS=""
RED=0
record() { RESULTS="$RESULTS $1:$2:$3"; [ "$2" = "RED" ] && RED=1; }
say() { printf '%s\n' "$*"; }

TMPD=$(mktemp -d "${TMPDIR:-/tmp}/e2e787.XXXXXX") || exit 2
cleanup() { rm -rf "$TMPD" 2>/dev/null; }
trap cleanup EXIT INT TERM

say "=== Spec 787 acceptance gate (e2e:787) — $REPO_ROOT ==="
command -v cargo >/dev/null 2>&1 || { say "FATAL: cargo not on PATH"; exit 2; }

# ── Build the CLI (the scratch surface) ──────────────────────────────────────
say "[build] cargo build -p trx64-cli --release"
if ! cargo build -p trx64-cli --release >"$TMPD/build.log" 2>&1; then
  say "FATAL: trx64-cli build failed:"; tail -20 "$TMPD/build.log"; exit 2
fi
[ -x "$CLI" ] || { say "FATAL: $CLI missing after build"; exit 2; }

ROMS="$C64RE_ROOT/resources/roms"
if [ ! -f "$ROMS/kernal-901227-03.bin" ]; then
  say "FATAL: ROMs absent at $ROMS (set C64RE_ROOT)"; exit 2
fi

# The tiny depacker fixture: writes $00..$0f to $4000 then RTS; load addr $c000.
PRG="$TMPD/tiny_depack.prg"
python3 - "$PRG" <<'PY'
import sys
open(sys.argv[1],'wb').write(bytes([0x00,0xc0,0xa2,0x00,0x8a,0x9d,0x00,0x40,0xe8,0xe0,0x10,0xd0,0xf7,0x60]))
PY
EXPECT="000102030405060708090a0b0c0d0e0f"

# ── #1 Spawn + dispose ───────────────────────────────────────────────────────
# The CLI process IS the scratch instance: it spawns, reaches a usable booted
# state (real routine runs to sentinel_rts on the real core), and disposes on exit.
# (Daemon `spawn-scratch`/`dispose` verb — N/A, v1 = the CLI process is the scratch.)
say "[#1] spawn + dispose (trx64cli sandbox one-shot)"
OUT1=$("$CLI" --rom-dir "$ROMS" sandbox --load "$PRG" --entry '$c000' --harvest '$4000:16' 2>"$TMPD/s1.err")
RC1=$?
if [ "$RC1" = 0 ] && printf '%s' "$OUT1" | grep -q "stop=sentinel_rts" && printf '%s' "$OUT1" | grep -q "$EXPECT"; then
  # dispose = the process returned; no lingering child from this shot.
  LEAK=$(pgrep -f "trx64cli .*sandbox --load $PRG" 2>/dev/null | wc -l | tr -d ' ')
  if [ "${LEAK:-0}" = 0 ]; then
    record 1 GREEN "sandbox_booted→sentinel_rts,_process_exited_(no_leak)"
  else
    record 1 RED "scratch_process_leaked_($LEAK)"
  fi
else
  record 1 RED "sandbox_did_not_reach_usable_state_(rc=$RC1)"
fi

# ── #5 CLI: spawns, runs, returns --json, self-disposes; Bash-invocable e2e ───
say "[#5] CLI --json contract"
J=$("$CLI" --rom-dir "$ROMS" sandbox --load "$PRG" --entry '$c000' --harvest '$4000:16' --json 2>/dev/null)
RC5=$?
if [ "$RC5" = 0 ] && printf '%s' "$J" | grep -q '"stopReason":"sentinel_rts"' && printf '%s' "$J" | grep -q "\"hex\":\"$EXPECT\""; then
  record 5 GREEN "--json_returned_(stopReason+harvest),_process_self-disposed"
else
  record 5 RED "json_contract_failed_(rc=$RC5)"
fi

# ── #6 TRX64-only; with TRX64 absent → fail cleanly, no silent TS fallback ────
# The CLI is a pure-Rust binary linking trx64-core/daemon; there is NO TS second
# path in it. Point a scratch at an absent ROM dir → clean non-zero failure with a
# diagnostic, never a silent "success via some fallback".
say "[#6] TRX64-only / fails cleanly"
BADOUT=$("$CLI" --rom-dir "$TMPD/no-such-roms" sandbox --load "$PRG" --entry '$c000' --harvest '$4000:16' 2>&1)
RC6=$?
NO_TS=1
grep -rq "TypeScript\|integrated-session\|fallback" "$REPO_ROOT/crates/trx64-cli/src/sandbox_cmd.rs" "$REPO_ROOT/crates/trx64-cli/src/boot_cmd.rs" 2>/dev/null && NO_TS=0
if [ "$RC6" != 0 ] && printf '%s' "$BADOUT" | grep -qi "boot" && [ "$NO_TS" = 1 ]; then
  record 6 GREEN "absent-ROM_scratch_failed_cleanly_(rc=$RC6),_no_TS_fallback_path"
else
  record 6 RED "did_not_fail_cleanly_or_TS_fallback_present_(rc=$RC6,no_ts=$NO_TS)"
fi

# ── #2 + #3 One-live invariant + isolation both ways ─────────────────────────
# TRX64-side asserting gate: a real live Machine in-process vs a real separate-OS
# `trx64cli sandbox` child; live RAM+regs+drive byte-identical, scratch deterministic
# under concurrent live mutation.
say "[#2+#3] scratch↔live isolation (cargo test e2e_787_isolation)"
if cargo test --release -p trx64-cli --test e2e_787_isolation >"$TMPD/iso.log" 2>&1 \
   && grep -q "test result: ok" "$TMPD/iso.log"; then
  if grep -q "1 passed" "$TMPD/iso.log"; then
    record 2 GREEN "live_RAM+regs+half_track_byte-identical_across_scratch_spawn/run/dispose"
    record 3 GREEN "scratch_cannot_mutate_live_+_concurrent_live_ops_do_not_perturb_scratch"
  else
    record 2 RED "isolation_test_skipped_(ROMs?)"; record 3 RED "isolation_test_skipped"
  fi
else
  record 2 RED "isolation_test_FAILED"; record 3 RED "isolation_test_FAILED"
  tail -20 "$TMPD/iso.log"
fi

# ── #4 Single-Path preserved; no new mode flag ───────────────────────────────
say "[#4] single-path preserved (no new mode flag)"
# (a) TRX64-side structural: the scratch constructs the ONE pipeline (Machine::new()
#     + boot_from_dir) — no alternate CPU/VIC/drive selector, no mode toggle among
#     the sandbox/boot CLI args.
S4A=1
grep -q "Machine::new()" "$REPO_ROOT/crates/trx64-cli/src/sandbox_cmd.rs" || S4A=0
FORBID='useMicrocodedCpu|useCycleLockstep|useLiteralPort|CycleLockstep|--mode|scheduler|fast-trap|real-kernal'
if grep -Eq "$FORBID" "$REPO_ROOT/crates/trx64-cli/src/sandbox_cmd.rs" \
      "$REPO_ROOT/crates/trx64-cli/src/boot_cmd.rs"; then S4A=0; fi
# Also: the CLI subcommand arg surface (main.rs sandbox/boot) introduces no path toggle.
if grep -Eq "$FORBID" "$REPO_ROOT/crates/trx64-cli/src/main.rs"; then S4A=0; fi
# (b) TS runtime untouched: the C64RE single-path probe (reachability-gated).
S4B="skip"
PROBE="$C64RE_ROOT/scripts/probe-single-path.mjs"
if command -v node >/dev/null 2>&1 && [ -f "$PROBE" ] \
   && [ -f "$C64RE_ROOT/dist/runtime/headless/integrated-session-manager.js" ]; then
  if ( cd "$C64RE_ROOT" && node "$PROBE" ) >"$TMPD/sp.log" 2>&1 && grep -q "^GREEN single-path" "$TMPD/sp.log"; then
    S4B="green"
  else
    S4B="red"
  fi
fi
if [ "$S4A" = 1 ] && [ "$S4B" != "red" ]; then
  record 4 GREEN "TRX64_scratch=one_pipeline_(Machine::new,no_toggle);_TS_single-path_probe=$S4B"
else
  record 4 RED "single-path_violated_(trx64_struct=$S4A,ts_probe=$S4B)"
fi

# ── #7 Runtime product proof baseline stays green ────────────────────────────
say "[#7] runtime product proof baseline (scripts/gate.sh)"
if [ "${E2E787_SKIP_BASELINE:-0}" = 1 ]; then
  record 7 SKIP "baseline_opt-out_(E2E787_SKIP_BASELINE=1);_run_scripts/gate.sh_separately"
elif [ -x "$REPO_ROOT/scripts/gate.sh" ]; then
  if "$REPO_ROOT/scripts/gate.sh" >"$TMPD/gate.log" 2>&1 && grep -q "GATE GREEN" "$TMPD/gate.log"; then
    record 7 GREEN "scripts/gate.sh_GREEN_(unit_gates_+_7-game)"
  else
    record 7 RED "scripts/gate.sh_not_green_(see_log)"
    tail -25 "$TMPD/gate.log"
  fi
else
  record 7 SKIP "scripts/gate.sh_absent"
fi

# ── Summary table ────────────────────────────────────────────────────────────
say ""
say "──────────────────────────────────────────────────────────────────────────"
say "Spec 787 §5 acceptance — verdict per point:"
titles_1="Spawn+dispose"
titles_2="One-live_invariant_(byte-identical)"
titles_3="Isolation_both_ways"
titles_4="Single-Path_preserved_(no_new_mode)"
titles_5="CLI_--json_self-disposes"
titles_6="TRX64-only_(clean_fail,_no_TS_fallback)"
titles_7="Runtime_product_proof_baseline"
for n in 1 2 3 4 5 6 7; do
  eval "t=\$titles_$n"
  v=""; d=""
  for r in $RESULTS; do
    case "$r" in
      "$n:"*) v=$(printf '%s' "$r" | cut -d: -f2); d=$(printf '%s' "$r" | cut -d: -f3- | tr '_' ' ');;
    esac
  done
  printf '  #%s  %-6s  %-40s  %s\n' "$n" "$v" "$(printf '%s' "$t" | tr '_' ' ')" "$d"
done
# The daemon spawn-scratch/dispose verb from §6 build order is N/A in v1:
say "  ·   N/A     daemon spawn-scratch verb                  v1 = CLI process is the scratch (no daemon verb)"
say "──────────────────────────────────────────────────────────────────────────"
if [ "$RED" = 0 ]; then
  say "e2e:787 GREEN"; exit 0
else
  say "e2e:787 RED"; exit 1
fi
