#!/usr/bin/env bash
# Real-proof sweep of ONLY the private (shielded) operations, one network at a time,
# with a wall-clock time for each.
#
# Why this is separate from scripts/test-all-cli.sh: every operation here mints a
# real recursive STARK, and under LEZ v0.2.4 that is brutally expensive. The
# cost is flat: the WALLET (not the circuit) pads every privacy transaction to
# `MAX_PRIVATE_ACCOUNTS = 7` via `dummy_inputs_default()`
# (lez/wallet/src/account_manager.rs) - the circuit takes a `Vec<DummyInput>` and
# accepts any count. Upstream's stated reason is that 7 is the largest per-tx
# account count currently supported ("it is 7 for AMM") and that all users share
# the value for a larger anonymity set.
#
# Do NOT reach for the padding when this feels slow: each dummy is one nullifier
# hash plus one commitment hash, roughly a dozen hashes against four recursive
# STARKs. Removing it entirely saves ~1% of transaction size and nothing
# measurable in time. The cost is the recursion, and most of a saga's WALL CLOCK
# is our own poll budget, not proving at all.
#
# Consequences you must plan around:
#   * One prover is ~8-9 GB resident and saturates every core it can get. Two
#     concurrent provers on a 16-core box already oversubscribe it; three drove
#     load to ~57 and filled swap. NEVER run these in parallel, and check for
#     foreign r0vm processes first (see the guard below) - an OOM kill would take
#     out somebody else's hours-old proof, not just yours.
#   * RISC0_DEV_MODE is not an escape hatch. A real sequencer VERIFIES proofs, so
#     a dev-mode receipt is rejected on chain. This script refuses to run with it.
#
# Usage:
#   LPAD_NETWORK=testnet bash scripts/private-ops.sh            # all private ops
#   LPAD_NETWORK=paradox bash scripts/private-ops.sh shield     # just one
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
NETWORK="${LPAD_NETWORK:-testnet}"
# `logos` is an alias for `testnet`. Canonicalise it before it reaches the
# ENVFILE default below, which would otherwise look for a bootstrap.logos.env
# that bootstrap.sh never writes.
[ "$NETWORK" = "logos" ] && NETWORK=testnet
case "$NETWORK" in
  testnet) NET_HOME="$HOME/.lpad";         NET_ADDR="https://testnet.lez.logos.co" ;;
  paradox) NET_HOME="$HOME/.lpad-paradox"; NET_ADDR="https://seq-testnet.paradox.computer" ;;
  *) echo "unknown LPAD_NETWORK '$NETWORK'" >&2; exit 2 ;;
esac
ENVFILE="${LPAD_TEST_ENV:-$REPO/scripts/bootstrap.$NETWORK.env}"
[ -f "$ENVFILE" ] || { echo "no env file $ENVFILE - bootstrap $NETWORK first" >&2; exit 2; }
# shellcheck disable=SC1090
source "$ENVFILE"
export LEE_WALLET_HOME_DIR="${LPAD_WALLET_HOME:-$NET_HOME}"
export PATH="$HOME/.cargo/bin:$HOME/.risc0/bin:$PATH"
L="$REPO/cli/target/release/lpad"

if [ "${RISC0_DEV_MODE:-0}" = "1" ]; then
  echo "✗ RISC0_DEV_MODE=1 but $NET_ADDR verifies proofs - a dev receipt is rejected." >&2
  exit 2
fi

# Refuse to pile onto provers we do not own. Each is ~8-9 GB; the OOM killer
# picks the biggest RSS, which is somebody else's in-flight proof.
#
# Match on the EXECUTABLE NAME (`ps -eo comm=`), never `pgrep -f`. `pgrep -f`
# matches the whole command line, so it also matches any shell, editor or grep
# whose arguments merely mention r0vm - including this script's own guard while it
# runs. That false positive made the guard refuse to start with zero real provers
# on the box.
FOREIGN=$(ps -eo comm= | grep -cx 'r0vm' || true)
if [ "${FOREIGN:-0}" -gt 0 ] && [ "${LPAD_FORCE_CONCURRENT_PROVER:-0}" != "1" ]; then
  echo "✗ $FOREIGN r0vm prover(s) already running - refusing to start another." >&2
  ps -eo pid,etimes,rss,comm | awk '$4=="r0vm" {printf "    pid=%s elapsed=%ss rss=%dMB\n",$1,$2,$3/1024}' >&2
  echo "  Wait for them, or set LPAD_FORCE_CONCURRENT_PROVER=1 if you accept the OOM risk." >&2
  exit 3
fi

: "${LPAD_PRIV_COLL:=}"
: "${LPAD_PRIV_PROJ:=}"
N=(--network "$NETWORK")
PASS=0; FAIL=0; declare -a FAILED

run() { # run <name> <cmd...>
  local n="$1"; shift
  local t0 rc; t0=$(date +%s)
  echo "── START $n  ($(date -u +%H:%M:%S))"
  "$@" >"/tmp/priv_$n.out" 2>&1; rc=$?
  local el=$(( $(date +%s) - t0 ))
  if [ $rc -eq 0 ]; then
    echo "   ✓ $n  ${el}s"; PASS=$((PASS+1)); tail -2 "/tmp/priv_$n.out" | sed 's/^/       /'
  else
    echo "   ✗ $n  ${el}s"; FAIL=$((FAIL+1)); FAILED+=("$n"); tail -5 "/tmp/priv_$n.out" | sed 's/^/       /'
  fi
}

WANT="${1:-all}"
want() { [ "$WANT" = "all" ] || [ "$WANT" = "$1" ]; }

echo "== real-proof PRIVATE ops on $NETWORK ($NET_ADDR) =="
echo "   wallet: $LEE_WALLET_HOME_DIR   env: $ENVFILE   cores: $(nproc)"
echo

# shield/deshield are the cheapest private ops (no inner program executions to
# recursively verify), so they run first: if the padding cost alone is
# prohibitive, there is no point queueing the buys behind them.
want shield       && run shield       "$L" "${N[@]}" shield       --token-def "$LPAD_COLL_DEF" --amount 50
want deshield     && run deshield     "$L" "${N[@]}" deshield     --token-def "$LPAD_COLL_DEF" --amount 50
want shield-lez   && run shield-lez   "$L" "${N[@]}" shield-lez   --amount 100
want deshield-lez && run deshield-lez "$L" "${N[@]}" deshield-lez --amount 100

# The disposable private buys additionally verify the inner program executions
# inside the privacy STARK, which historically was ~40-50% of total cycles.
if [ -n "$LPAD_PRIV_COLL" ] && [ -n "$LPAD_PRIV_PROJ" ]; then
  want bc-buy-private   && run bc-buy-private   "$L" "${N[@]}" bc  buy-private  --program "$LPAD_BC_PROGRAM_ID"  --sale "$LPAD_SALE_ID"     --user-collateral "$LPAD_PRIV_COLL" --user-token "$LPAD_PRIV_PROJ" --in 1000 --min-out 0
  want bc-sell-private  && run bc-sell-private  "$L" "${N[@]}" bc  sell-private --program "$LPAD_BC_PROGRAM_ID"  --sale "$LPAD_SALE_ID"     --user-token "$LPAD_PRIV_PROJ" --user-collateral "$LPAD_PRIV_COLL" --tokens 500 --min-out 0
  want lbp-buy-private  && run lbp-buy-private  "$L" "${N[@]}" lbp buy-private  --program "$LPAD_LBP_PROGRAM_ID" --pool "$LPAD_LBP_POOL_ID" --user-collateral "$LPAD_PRIV_COLL" --user-token "$LPAD_PRIV_PROJ" --in 1000 --min-out 0
else
  echo "── skipping the private buys: no LPAD_PRIV_COLL/LPAD_PRIV_PROJ in $ENVFILE"
  echo "   (they need a shielded holding, which the shield above creates - re-run after it lands)"
fi

echo
echo "=== $PASS passed, $FAIL failed ==="
[ "$FAIL" -eq 0 ] || { printf 'FAILED: %s\n' "${FAILED[@]}"; exit 1; }
