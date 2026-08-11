#!/usr/bin/env bash
# Verify lpad is actually deployed and live on a network.
#
# Do NOT use the sequencer's `getProgramIds` for this: it returns a hardcoded
# five-entry list with a `// TODO: Get programs from state` upstream, so it never
# mentions lpad's programs no matter what is deployed.
#
# Instead this checks state that only exists if a deployment worked:
#   1. the sale / pool PDAs exist on chain, and
#   2. their `program_owner` equals the program id this build embeds.
#
# (2) is the real assertion - it proves the chain is running the same guest ELF
# this checkout would submit against. A mismatch means the artifacts changed since
# deployment and every PDA derived from the old id is orphaned.
#
#   bash scripts/verify-deployment.sh                    # uses scripts/bootstrap.env
#   LPAD_NETWORK=paradox bash scripts/verify-deployment.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
ENVF="${LPAD_BOOTSTRAP_OUT:-$REPO/scripts/bootstrap.env}"
LPAD="${LPAD_BIN:-$REPO/cli/target/release/lpad}"

[ -f "$ENVF" ] || { echo "✗ no $ENVF - run scripts/bootstrap.sh first" >&2; exit 2; }
# shellcheck disable=SC1090
source "$ENVF"

NETWORK="${LPAD_NETWORK:-testnet}"
case "$NETWORK" in
  testnet|logos) SEQ="https://testnet.lez.logos.co" ;;
  paradox)       SEQ="https://seq-testnet.paradox.computer" ;;
  http*)         SEQ="$NETWORK" ;;
  *) echo "unknown LPAD_NETWORK '$NETWORK'" >&2; exit 2 ;;
esac
SEQ="${LPAD_SEQUENCER_ADDR:-$SEQ}"

PASS=0; FAIL=0
ok(){ printf '  \033[32m✓\033[0m %s\n' "$*"; PASS=$((PASS+1)); }
no(){ printf '  \033[31m✗\033[0m %s\n' "$*"; FAIL=$((FAIL+1)); }

rpc() {
  timeout 90 curl -s -X POST "$SEQ" -H 'content-type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\",\"params\":$2}" 2>/dev/null
}

# Account ids reach this script in whichever form produced them: the lpad CLI
# prints `Public/<base58>`, bootstrap has recorded sale/pool ids as raw hex, and
# `getAccount` accepts ONLY bare base58 (a `Public/` prefix returns
# `invalid base58: InvalidBase58Character('l', 3)`, and hex contains '0', which
# is not in the base58 alphabet at all). Normalize before every RPC call -
# otherwise a live account reports as ABSENT and a green run means nothing.
norm_acct() {
  python3 -c '
import sys
B58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
s = sys.argv[1].split("/", 1)[-1]
if len(s) == 64 and all(c in "0123456789abcdefABCDEF" for c in s):
    b = bytes.fromhex(s)
    n = int.from_bytes(b, "big")
    o = ""
    while n:
        n, r = divmod(n, 58)
        o = B58[r] + o
    s = "1" * (len(b) - len(b.lstrip(b"\0"))) + o
print(s)' "$1"
}

# hex program id -> the [u32;8] the RPC returns, so they can be compared.
hex_to_words() {
  python3 -c '
import sys
h=sys.argv[1]
b=bytes.fromhex(h)
print([int.from_bytes(b[i:i+4],"little") for i in range(0,32,4)])' "$1"
}

echo "== lpad deployment check: $NETWORK ($SEQ) =="

h=$(rpc getLastBlockId '[]' | python3 -c 'import sys,json;print(json.load(sys.stdin)["result"])' 2>/dev/null)
[ -n "$h" ] && ok "sequencer reachable, block $h" || { no "sequencer unreachable"; exit 1; }

# Each PDA must exist AND be owned by the program id this build embeds.
check_pda() {
  local label="$1" acct="$2" want_hex="$3"
  local got want
  acct=$(norm_acct "$acct")
  got=$(rpc getAccount "[\"$acct\"]" \
    | python3 -c 'import sys,json
try:
    r=json.load(sys.stdin)["result"]
    print(r["program_owner"] if r else "ABSENT")
except Exception: print("ABSENT")' 2>/dev/null)
  if [ "$got" = "ABSENT" ] || [ -z "$got" ]; then
    no "$label: account $acct not on chain (deployment or creation did not land)"
    return
  fi
  want=$(hex_to_words "$want_hex")
  if [ "$got" = "$want" ]; then
    ok "$label: on chain and owned by this build's program id"
  else
    no "$label: OWNER MISMATCH - chain has $got, this build embeds $want"
  fi
}

BC_ID=$("$LPAD" program-id bc   --json | python3 -c 'import json,sys;print(json.load(sys.stdin)["program_id"])')
LBP_ID=$("$LPAD" program-id lbp --json | python3 -c 'import json,sys;print(json.load(sys.stdin)["program_id"])')

[ -n "${LPAD_SALE_ID:-}" ]     && check_pda "bonding-curve sale" "${LPAD_SALE_ID}"     "$BC_ID"  || no "LPAD_SALE_ID not in $ENVF"
[ -n "${LPAD_LBP_POOL_ID:-}" ] && check_pda "LBP pool"           "${LPAD_LBP_POOL_ID}" "$LBP_ID" || no "LPAD_LBP_POOL_ID not in $ENVF"

# And the high-level reads must work through the CLI.
echo "== CLI reads =="
"$LPAD" bc sale-info  --sale "${LPAD_SALE_ID:-}"     --json >/dev/null 2>&1 && ok "bc sale-info"  || no "bc sale-info"
"$LPAD" lbp pool-info --pool "${LPAD_LBP_POOL_ID:-}" --json >/dev/null 2>&1 && ok "lbp pool-info" || no "lbp pool-info"

echo
echo "passed $PASS, failed $FAIL"
[ "$FAIL" -eq 0 ]
