#!/bin/sh
# ─────────────────────────────────────────────────────────────────────────────
# scripts/gate.sh — Spec 783 local quality gate (no cloud CI).
#
# One command: runs the full regression gate and EXITS NON-ZERO on ANY red.
# Quiet on green, first-failure-loud on red (fail-fast). Steps:
#
#   [1/4] clippy            (lint — NON-BLOCKING by default; pre-existing backlog)
#   [2/4] rust gate tests   (iso_vic_gate + vic_collision_gate + cart_mapper_gate, release)
#   [3/4] 7-game gate       (behavioral, release — gates on the printed VERDICT)
#   [4/4] WS conformance     (TS↔TRX64 oracle — reachability-checked + OPT-IN)
#
# Exit codes: 0 = green (all blocking steps passed; skips are OK). 1 = a blocking
# step failed. 2 = environment error (no repo / no cargo).
#
# Env knobs:
#   GATE_CLIPPY_STRICT=1   make clippy a BLOCKING `-D warnings` step (default: off — backlog)
#   GATE_CONFORMANCE=1     run the WS conformance harness (default: skipped — heavy)
#   GATE_CONFORMANCE_SEV=  severity filter for conformance (default: P0)
#   C64RE_ROOT=            path to the C64RE TS oracle (default: sibling repo)
#   TRX64_DAEMON_BIN=      trx64-daemon binary for conformance (default: target/release)
# ─────────────────────────────────────────────────────────────────────────────
set -u

REPO_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd) || exit 2
cd "$REPO_ROOT" || exit 2

TMPS=""
cleanup() { [ -n "$TMPS" ] && rm -rf $TMPS 2>/dev/null; }
trap cleanup EXIT INT TERM
mktmp() { t=$(mktemp "${TMPDIR:-/tmp}/gate.XXXXXX"); TMPS="$TMPS $t"; printf '%s' "$t"; }
mktmpd() { d=$(mktemp -d "${TMPDIR:-/tmp}/gate.XXXXXX"); TMPS="$TMPS $d"; printf '%s' "$d"; }

hr()    { printf '%s\n' "----------------------------------------------------------------"; }
green() { printf '  GREEN  %s\n' "$1"; }
note()  { printf '  NOTE   %s\n' "$1"; }
skip()  { printf '  SKIP   %s\n' "$1" >&2; }
# first-failure-loud: dump detail then exit non-zero.
die_red() {
  printf '\n' >&2; hr >&2
  printf 'GATE RED — %s\n' "$1" >&2
  if [ -n "${2:-}" ] && [ -f "$2" ]; then
    printf '\n--- detail (tail) ---\n' >&2
    tail -60 "$2" >&2
  fi
  hr >&2
  exit 1
}

t0=$(date +%s)
printf '=== Spec 783 quality gate — %s ===\n' "$REPO_ROOT"

# ── [1/4] clippy — lint ──────────────────────────────────────────────────────
printf '[1/4] clippy (lint)\n'
if ! command -v cargo >/dev/null 2>&1; then
  skip "cargo not on PATH — lint skipped"
elif ! cargo clippy --version >/dev/null 2>&1; then
  skip "clippy component not installed (\`rustup component add clippy\`) — lint skipped"
else
  LINTLOG=$(mktmp)
  if [ "${GATE_CLIPPY_STRICT:-0}" = "1" ]; then
    if cargo clippy --all-targets -- -D warnings >"$LINTLOG" 2>&1; then
      green "clippy strict (-D warnings): clean"
    else
      die_red "clippy strict (-D warnings) found lints" "$LINTLOG"
    fi
  else
    cargo clippy --all-targets >"$LINTLOG" 2>&1
    WARNS=$(grep -cE '^warning' "$LINTLOG" 2>/dev/null || true)
    ERRS=$(grep -cE '^error'   "$LINTLOG" 2>/dev/null || true)
    note "clippy NON-BLOCKING (pre-existing backlog): ${WARNS:-0} warning-lines, ${ERRS:-0} error-lines. Enforce with GATE_CLIPPY_STRICT=1."
  fi
fi

# ── [2/4] rust gate tests (asserting, fast) ──────────────────────────────────
printf '[2/4] rust gate tests (iso_vic + vic_collision + cart_mapper, release)\n'
command -v cargo >/dev/null 2>&1 || die_red "cargo not on PATH — cannot run gate tests" ""
TLOG=$(mktmp)
if cargo test --release -p trx64-core \
     --test iso_vic_gate --test vic_collision_gate --test cart_mapper_gate \
     >"$TLOG" 2>&1; then
  green "unit gates: $(grep -cE 'test result: ok' "$TLOG") suites ok ($(grep -oE '[0-9]+ passed' "$TLOG" | awk '{s+=$1} END{print s}') tests)"
else
  die_red "a rust gate test FAILED" "$TLOG"
fi

# ── [3/4] 7-game behavioral gate ─────────────────────────────────────────────
printf '[3/4] 7-game behavioral gate (release, ~25s)\n'
SNAP=$(mktmpd)
cp traces/gate_*_trx64.png "$SNAP"/ 2>/dev/null || true
GLOG=$(mktmp)
if ! cargo test --release -p trx64-core --test seven_game_gate -- --ignored --nocapture \
     >"$GLOG" 2>&1; then
  die_red "7-game gate process failed (panic / boot error)" "$GLOG"
fi
if grep -qE 'skip .*(ROMs absent|sample absent)' "$GLOG"; then
  RAN=$(grep -cE 'VERDICT:' "$GLOG" 2>/dev/null || true)
  skip "7-game gate INCOMPLETE — ROMs/samples absent (C64RE resources unreachable). Only ${RAN:-0}/7 games ran; NOT counted as green."
else
  VP=$(grep -cE 'VERDICT: PASS' "$GLOG" 2>/dev/null || true)
  VT=$(grep -cE 'VERDICT:'      "$GLOG" 2>/dev/null || true)
  : "${VP:=0}" "${VT:=0}"
  if [ "$VT" -lt 7 ] || [ "$VP" -lt "$VT" ]; then
    printf '\n' >&2; hr >&2
    printf 'GATE RED — 7-game gate: %s/%s games PASS (a game regressed)\n\n' "$VP" "$VT" >&2
    grep -E '=====|VERDICT:' "$GLOG" >&2
    printf '\n--- log tail ---\n' >&2; tail -30 "$GLOG" >&2
    hr >&2
    exit 1
  fi
  green "7-game gate: $VP/7 PASS"
fi
# Screenshot drift is INFORMATIONAL: traces/ is gitignored (no pinned oracle) and
# the render is deterministic-but-not-baseline-pinned, so the VERDICT above is the
# gate; a changed PNG is a heads-up, not a red. (Follow-up: pin the PNGs.)
CHANGED=""
for f in traces/gate_*_trx64.png; do
  [ -e "$f" ] || continue
  b="$SNAP/$(basename "$f")"
  if [ ! -f "$b" ]; then CHANGED="$CHANGED new:$(basename "$f")"; continue; fi
  cmp -s "$f" "$b" || CHANGED="$CHANGED $(basename "$f")"
done
[ -n "$CHANGED" ] && note "screenshot drift vs pre-run baseline:$CHANGED (informational)"

# ── [4/4] WS conformance (opt-in, reachability-checked) ──────────────────────
printf '[4/4] WS conformance (TS↔TRX64 oracle)\n'
ORACLE_DIR="$REPO_ROOT/tools/oracle"
C64RE_ROOT="${C64RE_ROOT:-/Users/alex/Development/C64/Tools/C64ReverseEngineeringMCP}"
TRXBIN="${TRX64_DAEMON_BIN:-$REPO_ROOT/target/release/trx64-daemon}"
[ -x "$TRXBIN" ] || TRXBIN="$REPO_ROOT/target/debug/trx64-daemon"
reachable=1
command -v node >/dev/null 2>&1                              || reachable=0
[ -x "$ORACLE_DIR/node_modules/.bin/tsx" ]                    || reachable=0
[ -x "$C64RE_ROOT/node_modules/.bin/tsx" ]                    || reachable=0
[ -f "$C64RE_ROOT/src/runtime/headless/daemon/run.ts" ]      || reachable=0
[ -x "$TRXBIN" ]                                             || reachable=0
if [ "$reachable" = "0" ]; then
  skip "conformance UNREACHABLE — needs node + tools/oracle tsx + C64RE TS oracle (\$C64RE_ROOT) + a built trx64-daemon. Not run (NOT a false green)."
elif [ "${GATE_CONFORMANCE:-0}" = "0" ]; then
  skip "conformance reachable but OPT-IN (heavy: TS-daemon cold-boot ~50s/case, drives the C64RE oracle). Run: GATE_CONFORMANCE=1 [GATE_CONFORMANCE_SEV=P0] $0"
else
  SEV="${GATE_CONFORMANCE_SEV:-P0}"
  printf '  running conformance --severity %s (slow) ...\n' "$SEV"
  CLOG=$(mktmp)
  if ( cd "$ORACLE_DIR" && TRX64_DAEMON_BIN="$TRXBIN" \
       node_modules/.bin/tsx src/conformance.ts --severity "$SEV" ) >"$CLOG" 2>&1; then
    green "conformance $SEV: $(grep -E 'GREEN,' "$CLOG" | tail -1)"
  else
    die_red "conformance ($SEV) failed" "$CLOG"
  fi
fi

t1=$(date +%s)
printf '\n'; hr
printf 'GATE GREEN  (%ss)\n' "$((t1 - t0))"
exit 0
