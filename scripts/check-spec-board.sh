#!/bin/bash
# Spec-board drift gate.
#
# TRX64 had no board until 2026-08-12: status lived only inside each spec, nothing
# compared it to anything, and two specs sat wrong for weeks. 791 said PROPOSED with a
# shipped CLI command, a round-trip test and a parity probe behind it. 790 said PROPOSED
# while its own header recorded both slices done. Nobody was lying — there was simply no
# second place where the claim had to hold.
#
# So this checks the cheap, mechanical half, which is the half that decays:
#
#   1. every docs/NNN-*.md has a row on the board
#   2. every board row points at a file that exists
#   3. every spec carries a `**Status:**` line at all
#   4. the board's status word and the spec's own status word agree
#
# It cannot check a status against the CODE — that needs someone to read both. What it
# can do is make the two written claims agree, so a stale one has to be stale in two
# places at once.
#
#   scripts/check-spec-board.sh

set -uo pipefail
cd "$(dirname "$0")/.."

BOARD="docs/README.md"
fails=0
note() { printf '  %s\n' "$1"; }
fail() { printf '  FAIL  %s\n' "$1"; fails=$((fails + 1)); }

[ -f "$BOARD" ] || { echo "  FAIL  no $BOARD — the board IS the check's subject"; exit 1; }

specs=$(ls docs/[0-9]*.md 2>/dev/null | xargs -n1 basename 2>/dev/null | sort)
[ -n "$specs" ] || { echo "  no numbered specs under docs/ — nothing to check"; exit 0; }

count=0
for f in $specs; do
  count=$((count + 1))
  num="${f%%-*}"

  # 1 + 2: the spec is on the board, and the board's link resolves.
  if ! grep -q "($f)" "$BOARD"; then
    fail "$f has no row on the board — a spec nobody lists is a spec nobody revisits"
    continue
  fi

  # 3: the spec says what it is.
  status_line=$(grep -m1 -E '^\*\*Status:?\*\*' "docs/$f" || true)
  if [ -z "$status_line" ]; then
    fail "$f carries no \`**Status:**\` line"
    continue
  fi

  # 4: the two claims agree on the WORD. The board may say more than the spec (it
  # carries what is left); it may not say something different.
  # Longest-first: `grep -o` would otherwise take `BUILT` out of `HALF BUILT` and
  # report a contradiction that is not there. Found by this check going red on itself.
  spec_word=$(printf '%s' "$status_line" \
    | grep -oiE 'PARTLY BUILT|HALF BUILT|PROPOSED|BUILT|RESOLVED|SUPERSEDED|SHIPPED|SCOPED|DRAFT|READY|CLOSED' \
    | head -1 | tr '[:lower:]' '[:upper:]')
  board_row=$(grep -m1 "($f)" "$BOARD")
  board_word=$(printf '%s' "$board_row" \
    | grep -oE '\*\*[A-Z ]+\*\*' | head -1 | tr -d '*' | sed 's/ *$//')

  if [ -z "$board_word" ]; then
    fail "$f: the board row states no status in bold"
    continue
  fi

  # PARTLY BUILT contains BUILT; compare the board's word against the spec's, allowing
  # the board to be the MORE precise one (the spec says BUILT, the board says PARTLY
  # BUILT after someone measured). The reverse — the board claiming more than the spec —
  # is the drift worth catching.
  # They must MATCH. An earlier version let the board be "the measured one" and merely
  # noted a disagreement — which passed the exact case this gate exists for: a spec
  # saying PROPOSED with a shipped command behind it. A tolerance that admits the
  # defect is not a tolerance, it is the defect with a nicer message. Board and spec
  # both measured, both written down, or the gate is red.
  if [ "$board_word" != "$spec_word" ]; then
    fail "$f: board says '$board_word', spec says '$spec_word' — fix whichever is stale"
  fi
done

# Every board row must point at a real file.
while read -r link; do
  [ -f "docs/$link" ] || fail "the board links $link, which does not exist"
done < <(grep -oE '\(([0-9]+-[a-z0-9-]+\.md)\)' "$BOARD" | tr -d '()' | sort -u)

note "$count spec(s) checked against $BOARD"
if [ "$fails" -gt 0 ]; then
  printf '\nRED  spec board: %s fail.\n' "$fails"
  exit 1
fi
printf '\nGREEN  spec board: 0 fail.\n'
