#!/usr/bin/env bash
# Exercises EVERY lpad CLI command against a live (dev-mode) sequencer.
# Prereq: a running dev sequencer + `bash scripts/bootstrap.sh` (BC + LBP sales)
# emitting the env file below. Run: bash scripts/test-all-cli.sh
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
ENVFILE="${LPAD_TEST_ENV:-$REPO/scripts/bootstrap.rc4.env}"
# shellcheck disable=SC1090
source "$ENVFILE"
export PATH="$HOME/.cargo/bin:$HOME/.risc0/bin:$PATH"
export LOGOS_BLOCKCHAIN_CIRCUITS="${LOGOS_BLOCKCHAIN_CIRCUITS:-$HOME/.logos-blockchain-circuits}"
# Default to REAL proofs (RISC0_DEV_MODE=0) to match how the launchpad runs;
# set RISC0_DEV_MODE=1 for a fast dev/fake-proof pass against a dev sequencer.
export RISC0_DEV_MODE="${RISC0_DEV_MODE:-0}"
L="$REPO/cli/target/release/lpad"

PASS=0; FAIL=0; declare -a FAILED
chk() { local n="$1"; shift
  if "$@" >/tmp/cli_t.out 2>&1; then echo "  ✓ $n"; PASS=$((PASS+1))
  else echo "  ✗ $n"; FAIL=$((FAIL+1)); FAILED+=("$n"); sed 's/^/        /' /tmp/cli_t.out | tail -4; fi; }
sale_id() { "$L" bc ids --program "$LPAD_BC_PROGRAM_ID" --token-def "$LPAD_PROJ_DEF" --collateral-def "$LPAD_COLL_DEF" --creator "$LPAD_CREATOR" --nonce "$1" --json | python3 -c 'import json,sys;print(json.load(sys.stdin)["sale"])'; }
pool_id() { "$L" lbp ids --program "$LPAD_LBP_PROGRAM_ID" --token-def "$LPAD_PROJ_DEF" --collateral-def "$LPAD_COLL_DEF" --creator "$LPAD_CREATOR" --nonce "$1" --json | python3 -c 'import json,sys;print(json.load(sys.stdin)["pool"])'; }

echo "## OFFLINE (pure compute, no chain)"
chk "bc quote"       "$L" bc quote --vt 2000000 --vc 50000 --fee-bps 100 --in 1000
chk "bc cost"        "$L" bc cost --vt 2000000 --vc 50000 --fee-bps 100 --tokens 1000
chk "bc sell-quote"  "$L" bc sell-quote --vt 2000000 --vc 50000 --fee-bps 100 --tokens 1000
chk "bc ids"         "$L" bc ids --program "$LPAD_BC_PROGRAM_ID" --token-def "$LPAD_PROJ_DEF" --collateral-def "$LPAD_COLL_DEF" --creator "$LPAD_CREATOR"
chk "lbp weight"     "$L" lbp weight --w-start 0.9 --w-end 0.1 --t-start 0 --t-end 1000 --at 500
chk "lbp quote"      "$L" lbp quote --reserve-token 500000 --reserve-collateral 10000 --w-start 0.9 --w-end 0.1 --t-start 0 --t-end 1000 --at 500 --in 1000
chk "lbp ids"        "$L" lbp ids --program "$LPAD_LBP_PROGRAM_ID" --token-def "$LPAD_PROJ_DEF" --collateral-def "$LPAD_COLL_DEF" --creator "$LPAD_CREATOR"
chk "program-id bc"  "$L" program-id bc
chk "program-id lbp" "$L" program-id lbp
chk "program-id ata" "$L" program-id ata

echo "## ONLINE - reads"
chk "status"         "$L" status
chk "balance"        "$L" balance --account "$LPAD_BUYER_COLL"
chk "bc sale-info"   "$L" bc sale-info --sale "$LPAD_SALE_ID"
chk "lbp pool-info"  "$L" lbp pool-info --pool "$LPAD_LBP_POOL_ID" --at "$(date +%s%3N)"

echo "## ONLINE - bonding-curve writes"
chk "bc buy"                  "$L" bc buy --program "$LPAD_BC_PROGRAM_ID" --sale "$LPAD_SALE_ID" --buyer-collateral "$LPAD_BUYER_COLL" --buyer-token "$LPAD_BUYER_TOK" --in 1000 --min-out 0
chk "bc sell"                 "$L" bc sell --program "$LPAD_BC_PROGRAM_ID" --sale "$LPAD_SALE_ID" --seller-token "$LPAD_BUYER_TOK" --seller-collateral "$LPAD_BUYER_COLL" --tokens 500 --min-out 0
chk "bc buy-private" "$L" bc buy-private --program "$LPAD_BC_PROGRAM_ID" --sale "$LPAD_SALE_ID" --user-collateral "$LPAD_PRIV_COLL" --user-token "$LPAD_PRIV_PROJ" --in 1000 --min-out 0

echo "## ONLINE - LBP writes"
chk "lbp buy"                  "$L" lbp buy --program "$LPAD_LBP_PROGRAM_ID" --pool "$LPAD_LBP_POOL_ID" --buyer-collateral "$LPAD_BUYER_COLL" --buyer-token "$LPAD_BUYER_TOK" --in 1000 --min-out 0
chk "lbp buy-private" "$L" lbp buy-private --program "$LPAD_LBP_PROGRAM_ID" --pool "$LPAD_LBP_POOL_ID" --user-collateral "$LPAD_PRIV_COLL" --user-token "$LPAD_PRIV_PROJ" --in 1000 --min-out 0
chk "lbp poke"   "$L" lbp poke   --program "$LPAD_LBP_PROGRAM_ID" --pool "$LPAD_LBP_POOL_ID"
chk "lbp pause"  "$L" lbp pause  --program "$LPAD_LBP_PROGRAM_ID" --pool "$LPAD_LBP_POOL_ID" --creator "$LPAD_CREATOR"
chk "lbp resume" "$L" lbp resume --program "$LPAD_LBP_PROGRAM_ID" --pool "$LPAD_LBP_POOL_ID" --creator "$LPAD_CREATOR"

echo "## ONLINE - ATAs (RFP Func: ATAs for all token interactions)"
# owner = the wallet's creator account; --fund-from seeds the owner's input ATA
# from a keypair holding before the ATA buy/sell.
chk "create-ata"  "$L" create-ata --token-def "$LPAD_COLL_DEF" --owner "$LPAD_CREATOR"
chk "bc buy-ata"  "$L" bc buy-ata  --program "$LPAD_BC_PROGRAM_ID" --sale "$LPAD_SALE_ID" --owner "$LPAD_CREATOR" --fund-from "$LPAD_BUYER_COLL" --in 1000 --min-out 0
chk "bc sell-ata" "$L" bc sell-ata --program "$LPAD_BC_PROGRAM_ID" --sale "$LPAD_SALE_ID" --owner "$LPAD_CREATOR" --fund-from "$LPAD_PROJ_HOLD" --tokens 500 --min-out 0
chk "lbp buy-ata" "$L" lbp buy-ata --program "$LPAD_LBP_PROGRAM_ID" --pool "$LPAD_LBP_POOL_ID" --owner "$LPAD_CREATOR" --fund-from "$LPAD_BUYER_COLL" --in 1000 --min-out 0

echo "## ONLINE - creator lifecycle (dedicated closeable sales)"
N7="${LPAD_LC_NONCE:-$(date +%s)}"   # unique nonce per run so PDAs never collide
# BC manual close needs the end timestamp to have passed, but BC *buys* are
# rejected once it has. So: open a wide window, buy early (a small buy initialises
# the collateral vault that withdraw needs, without exhausting the sale), wait the
# window out, then advance the clock with a tx on the open-ended main sale so the
# close is permitted.
BC_END=$(( $(date +%s%3N) + 60000 ))
chk "bc create-sale" "$L" bc create-sale --program "$LPAD_BC_PROGRAM_ID" --collateral-def "$LPAD_COLL_DEF" --treasury "$LPAD_TREASURY" --creator-token-holding "$LPAD_PROJ_HOLD" --creator "$LPAD_CREATOR" --sale-quantity 100000 --dex-seed 100 --vt 2000000 --vc 50000 --fee-bps 100 --end-ts "$BC_END" --nonce "$N7"
BC7=$(sale_id "$N7")
chk "bc buy (close-test)" "$L" bc buy --program "$LPAD_BC_PROGRAM_ID" --sale "$BC7" --buyer-collateral "$LPAD_BUYER_COLL" --buyer-token "$LPAD_BUYER_TOK" --in 100 --min-out 0
while [ "$(date +%s%3N)" -lt "$BC_END" ]; do sleep 2; done
"$L" bc buy --program "$LPAD_BC_PROGRAM_ID" --sale "$LPAD_SALE_ID" --buyer-collateral "$LPAD_BUYER_COLL" --buyer-token "$LPAD_BUYER_TOK" --in 100 --min-out 0 >/dev/null 2>&1  # fresh block, clock past end_ts
chk "bc close"    "$L" bc close    --program "$LPAD_BC_PROGRAM_ID" --sale "$BC7" --creator "$LPAD_CREATOR"
chk "bc withdraw" "$L" bc withdraw --program "$LPAD_BC_PROGRAM_ID" --sale "$BC7" --creator-collateral "$LPAD_COLL_HOLD" --creator-token "$LPAD_PROJ_HOLD" --creator "$LPAD_CREATOR"

# LBP: t_end=1 → already ended → immediately closeable.
chk "lbp create-sale" "$L" lbp create-sale --program "$LPAD_LBP_PROGRAM_ID" --collateral-def "$LPAD_COLL_DEF" --treasury "$LPAD_TREASURY" --creator-token-holding "$LPAD_PROJ_HOLD" --creator-collateral-holding "$LPAD_COLL_HOLD" --creator "$LPAD_CREATOR" --token-deposit 1000 --collateral-seed 100 --w-start 0.9 --w-end 0.1 --t-start 0 --t-end 1 --nonce "$N7"
LBP7=$(pool_id "$N7")
chk "lbp close"    "$L" lbp close    --program "$LPAD_LBP_PROGRAM_ID" --pool "$LBP7" --creator "$LPAD_CREATOR"
chk "lbp withdraw" "$L" lbp withdraw --program "$LPAD_LBP_PROGRAM_ID" --pool "$LBP7" --creator-collateral "$LPAD_COLL_HOLD" --creator-token "$LPAD_PROJ_HOLD" --creator "$LPAD_CREATOR"

echo
echo "=== $PASS passed, $FAIL failed ==="
if [ "$FAIL" -gt 0 ]; then printf 'FAILED: %s\n' "${FAILED[@]}"; exit 1; fi
