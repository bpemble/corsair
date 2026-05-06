#!/usr/bin/env bash
# Lint: ban `*.underlying_price` as a positional argument to
# svi_implied_vol / sabr_implied_vol / compute_theo.
#
# Both §16 (HXEK6 P560 picked-off-every-time, 2026-05-01) and §19
# (calendar-carry-as-drift, 2026-05-06) were instances of "wrong f64
# passed to a positional float arg" — specifically, current spot
# (`state.scalars.lock().underlying_price`) substituted for fit-time
# forward at the pricing-function call site. This lint codifies the
# rule as a syntactic ban — anyone re-introducing the bug class on
# the same-line form gets a CI failure on the PR.
#
# Heuristic: same-line calls only (function name and bad arg on the
# same line). Multi-line calls require human review. This is by
# design — the lint is a backstop, not a substitute for review.
#
# Per audit §6 #4 (audits/sections-16-19-audit.md). See also CLAUDE.md
# §16 and §19 for incident background, and the audit's §4 for the
# class-of-error analysis that motivates this lint.
#
# Exit codes: 0 = clean, 1 = violation found.

set -euo pipefail

if [ "${1:-}" != "" ]; then
  ROOT="$1"
else
  ROOT="$(git rev-parse --show-toplevel 2>/dev/null || echo .)"
fi
BANNED_FNS="svi_implied_vol|sabr_implied_vol|compute_theo"
BAD_ARG="\.underlying_price"

# Lint the trader and broker hot-path crates only — corsair_pricing
# itself defines these functions and (intentionally) tests them with
# parameter sweeps, so it's excluded.
CRATES=(
  "rust/corsair_trader/src"
  "rust/corsair_broker/src"
)

found=0
for dir in "${CRATES[@]}"; do
  full="$ROOT/$dir"
  [ -d "$full" ] || continue
  # grep -E for extended regex; -rn for recursive + line numbers;
  # -- to terminate option parsing in case the path starts with a dash.
  # Regex: function name + open paren + any chars + .underlying_price.
  # `.*` instead of `[^)]*` because real argument expressions can
  # contain nested parens (e.g., `state.scalars.lock().underlying_price`).
  # False positives possible if .underlying_price appears AFTER an
  # unrelated function call closes on the same line — review-only
  # backstop, not a substitute for review.
  matches=$(grep -rEn "($BANNED_FNS)[[:space:]]*\(.*$BAD_ARG" "$full" 2>/dev/null || true)
  if [ -n "$matches" ]; then
    echo "ERROR: pricing function called with current underlying_price as a positional arg:"
    echo "$matches"
    echo
    echo "RULE: svi_implied_vol / sabr_implied_vol / compute_theo must receive"
    echo "the FIT-TIME forward (e.g., vp.forward / surface.forward) — NOT the"
    echo "current underlying spot. See audits/sections-16-19-audit.md §1 and"
    echo "CLAUDE.md §16. Bug class re-introduction blocked at lint level."
    found=1
  fi
done

if [ "$found" -eq 1 ]; then
  exit 1
fi

echo "OK: pricing-arg lint clean (no banned same-line forms found)."
