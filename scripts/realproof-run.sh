#!/usr/bin/env bash
# Real-proof verification of the private paths against a real LEZ v0.2.4 sequencer.
# Waits for the real-proof bootstrap, then runs every private buy variant with a
# real recursive STARK and times each. Public ops carry no client proof, so they
# are a quick sanity that the real-proof sequencer accepts our txs at all.
set -uo pipefail
cd "$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
export PATH="$HOME/.cargo/bin:$HOME/.risc0/bin:$PATH"
unset RISC0_DEV_MODE
L=cli/target/release/lpad
ENVF="${LPAD_TEST_ENV:-scripts/bootstrap.env}"
LOG=/tmp/lpad-realproof.log
: > "$LOG"
say() { echo "[$(date +%T)] $*" | tee -a "$LOG"; }

say "waiting for real-proof bootstrap to finish (env: $ENVF)"
for _ in $(seq 1 160); do            # up to ~40 min (bootstrap incl. 2 shield STARKs)
  if [ -f "$ENVF" ] && grep -q LPAD_LBP_POOL_ID "$ENVF"; then break; fi
  if ! pgrep -f "scripts/bootstrap.sh" >/dev/null; then sleep 5; break; fi
  sleep 15
done
if ! { [ -f "$ENVF" ] && grep -q LPAD_LBP_POOL_ID "$ENVF"; }; then
  say "BOOTSTRAP DID NOT COMPLETE - last lines:"; tail -25 /tmp/lpad-v020-real-bootstrap.log | tee -a "$LOG"; exit 1
fi
# shellcheck disable=SC1090
source "$ENVF"
say "bootstrap OK  bc=$LPAD_BC_PROGRAM_ID sale=${LPAD_SALE_ID:0:16}.. lbp_pool=${LPAD_LBP_POOL_ID:0:16}.."

run_timed() {                        # run_timed <name> <cmd...>
  local name="$1"; shift; local t0; t0=$(date +%s)
  say "START $name"
  if "$@" >"/tmp/rp_$name.out" 2>&1; then
    say "PASS  $name  ($(( $(date +%s)-t0 ))s)"
    grep -iE "tx|hash|submitted|out|balance" "/tmp/rp_$name.out" | tail -3 | sed 's/^/        /' | tee -a "$LOG"
  else
    say "FAIL  $name  ($(( $(date +%s)-t0 ))s)"; tail -8 "/tmp/rp_$name.out" | sed 's/^/        /' | tee -a "$LOG"
  fi
}

say "sale state before:"; $L bc sale-info --sale "$LPAD_SALE_ID" 2>/dev/null | sed 's/^/        /' | tee -a "$LOG"

# public sanity - no client proof, just confirms the real-proof sequencer accepts our txs
run_timed public-bc-buy   $L bc buy  --program "$LPAD_BC_PROGRAM_ID"  --sale "$LPAD_SALE_ID"  --buyer-collateral "$LPAD_BUYER_COLL" --buyer-token "$LPAD_BUYER_TOK" --in 1000 --min-out 0
run_timed public-lbp-buy  $L lbp buy --program "$LPAD_LBP_PROGRAM_ID" --pool "$LPAD_LBP_POOL_ID" --buyer-collateral "$LPAD_BUYER_COLL" --buyer-token "$LPAD_BUYER_TOK" --in 1000 --min-out 0

# REAL private proofs - drift-free deshield → public buy → re-shield; each leg a real STARK
run_timed bc-private   $L bc buy-private  --program "$LPAD_BC_PROGRAM_ID"  --sale "$LPAD_SALE_ID"  --user-collateral "$LPAD_PRIV_COLL" --user-token "$LPAD_PRIV_PROJ" --in 1000 --min-out 0
run_timed lbp-private  $L lbp buy-private --program "$LPAD_LBP_PROGRAM_ID" --pool "$LPAD_LBP_POOL_ID" --user-collateral "$LPAD_PRIV_COLL" --user-token "$LPAD_PRIV_PROJ" --in 1000 --min-out 0

say "sale state after:"; $L bc sale-info --sale "$LPAD_SALE_ID" 2>/dev/null | sed 's/^/        /' | tee -a "$LOG"
say "DONE real-proof sweep"
