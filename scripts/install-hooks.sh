#!/bin/sh
# ─────────────────────────────────────────────────────────────────────────────
# scripts/install-hooks.sh — Spec 783.2. Idempotent installer: points git at the
# committed `hooks/` dir so the pre-push quality gate is enforced on this clone.
# Safe to run repeatedly (a fresh clone just re-runs it).
# ─────────────────────────────────────────────────────────────────────────────
set -u

REPO_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd) || exit 2
cd "$REPO_ROOT" || exit 2

chmod +x hooks/pre-push scripts/gate.sh scripts/install-hooks.sh 2>/dev/null || true

current=$(git config --get core.hooksPath 2>/dev/null || true)
if [ "$current" = "hooks" ]; then
  printf 'core.hooksPath already = hooks (idempotent — nothing changed)\n'
else
  git config core.hooksPath hooks
  printf 'set core.hooksPath = hooks%s\n' "${current:+ (was: $current)}"
fi

printf 'installed hooks:\n'
ls -1 hooks/ 2>/dev/null | grep -vE '\.sample$' | sed 's/^/  /'
printf '\nSpec 783 gate is now enforced on `git push`.\n'
printf 'Bypass a single deliberate push with:  GATE_SKIP=1 git push\n'
