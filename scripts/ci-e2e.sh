#!/usr/bin/env bash
# LPAD CI gate: unit tests (no zkVM) + IDL drift + standalone E2E.
# Mirrors the ldex ci-e2e.sh. Run from the repo root.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
PROG="$REPO/programs"
export PATH="$HOME/.cargo/bin:$HOME/.risc0/bin:$PATH"
. "$HOME/.cargo/env" 2>/dev/null || true

fail() { echo "✗ $*" >&2; exit 1; }

echo "== 1. Unit tests (core + host, no zkVM) =="
( cd "$PROG" && RISC0_SKIP_BUILD=1 cargo test -q \
    -p bonding_curve_core -p bonding_curve_program \
    -p lbp_core -p lbp_program ) || fail "unit tests"

echo "== 1b. CLI tests (fmt unit + clap + offline/error e2e against the binary) =="
( cd "$REPO/cli" && RISC0_SKIP_BUILD=1 cargo test -q ) || fail "cli tests"

echo "== 2. IDL drift check (order-insensitive) =="
# idl-gen collects helper account-types in hash order, so compare normalized
# (sorted) forms - see scripts/normalize-idl.py.
norm="$REPO/scripts/normalize-idl.py"
for g in bonding_curve lbp; do
  src="$PROG/$g/methods/guest/src/bin/$g.rs"
  cur="$PROG/artifacts/${g}-idl.json"
  [ -f "$cur" ] || fail "missing artifacts/${g}-idl.json"
  ( cd "$PROG" && RISC0_SKIP_BUILD=1 cargo run -q -p idl-gen -- "$src" ) > /tmp/lpad-idl.json 2>/dev/null \
    || fail "idl-gen failed for $g"
  diff <(python3 "$norm" "$cur") <(python3 "$norm" /tmp/lpad-idl.json) >/dev/null 2>&1 \
    || fail "IDL drift for $g (regenerate programs/artifacts/${g}-idl.json)"
done

echo "== 3. E2E vs in-process LEZ state (RISC0_DEV_MODE=1) =="
( cd "$PROG" && RISC0_DEV_MODE=1 cargo test -q -p integration_tests ) || fail "e2e"

echo "✓ CI gate passed"
