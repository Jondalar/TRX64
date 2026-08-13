#!/usr/bin/env bash
# Architecture gate — the daemon owns ALL machine state.
#
# TRX64 is a daemon with N clients (the TRX64 TUI, the C64RE web UI) that must have
# feature parity. Parity is FREE when every client renders the same daemon state, and is
# rebuilt-by-hand-forever the moment a client owns something.
#
# This is not a style rule. Spec 808 produced five bugs in one day out of exactly one
# violation — a client-side `running: AtomicBool` whose own comment called itself "the
# AUTHORITY ... distinct from the controller's session.running":
#
#   F11 needed three presses      two run-flags, each press cleared one
#   header said PAUSE mid-run     client flag said paused, daemon was running
#   `play back` did nothing       pump gated on the client flag
#   recording 4x too dense        the client's 5 ms pump set the capture cadence
#   F10 still paused              two key paths, no daemon state between them
#
# So: no client-side atomic may name a piece of MACHINE state. Client-owned lifecycle
# (quit), display caches keyed off an explicit daemon reply, and UI-only concerns are
# fine — the list below is what must never come back.
#
#   scripts/check-client-owns-no-state.sh

set -uo pipefail
cd "$(dirname "$0")/.."

FILE="crates/trx64-cli/src/engine.rs"
fails=0

# Machine state that belongs to the daemon, as a client-side field.
for name in running warp playing direction paused transport_playing cadence cursor; do
  if grep -qE "^\s+${name}: Arc<Atomic" "$FILE"; then
    echo "  FAIL  ${FILE} declares client-side machine state \`${name}\`"
    echo "        The daemon owns it. Send an event, render the reply."
    fails=$((fails + 1))
  fi
done

# The give-away phrasing from the bug this gate exists for.
if grep -qiE 'AUTHORITY.*distinct from|reconcile the dual' "$FILE"; then
  echo "  FAIL  ${FILE} still describes two authorities for one fact"
  echo "        A 'reconcile' comment means the seam is back."
  fails=$((fails + 1))
fi

n=$(grep -cE '^\s+[a-z_]+: Arc<Atomic' "$FILE" || true)
echo "  ${FILE}: ${n} client-side atomic field(s)"

if [ "$fails" -eq 0 ]; then
  echo "GREEN  client owns no machine state: 0 fail."
else
  echo "RED    client owns no machine state: ${fails} fail."
fi
exit "$fails"
