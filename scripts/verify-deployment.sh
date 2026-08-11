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
# Env file: scripts/bootstrap.<network>.env when it exists, else
# scripts/bootstrap.env. LPAD_BOOTSTRAP_OUT overrides both. The per-network
# default matters - the ids inside are PDAs on one specific chain.
#
#   bash scripts/verify-deployment.sh
#   LPAD_NETWORK=paradox bash scripts/verify-deployment.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
LPAD="${LPAD_BIN:-$REPO/cli/target/release/lpad}"

# Capture the caller's network BEFORE sourcing: the env file re-exports the
# LPAD_NETWORK / LPAD_SEQUENCER_ADDR of the run that produced it, so sourcing it
# first would silently override the network being asked about.
NETWORK="${LPAD_NETWORK:-testnet}"
# `logos` is an alias for `testnet`; canonicalise it so it cannot name a
# bootstrap.logos.env that nothing ever writes.
[ "$NETWORK" = "logos" ] && NETWORK=testnet
case "$NETWORK" in
  testnet) SEQ="https://testnet.lez.logos.co" ;;
  paradox) SEQ="https://seq-testnet.paradox.computer" ;;
  http*)   SEQ="$NETWORK" ;;
  *) echo "unknown LPAD_NETWORK '$NETWORK'" >&2; exit 2 ;;
esac

# Default to the PER-NETWORK env file when one exists. Without this,
# `LPAD_NETWORK=paradox bash scripts/verify-deployment.sh` - the invocation this
# script's own header documents - read scripts/bootstrap.env, i.e. the testnet
# ids, and then checked them against the paradox RPC. Every PDA reads as absent,
# which is indistinguishable from "the deployment did not land".
ENVF="${LPAD_BOOTSTRAP_OUT:-}"
if [ -z "$ENVF" ]; then
  ENVF="$REPO/scripts/bootstrap.env"
  [ -f "$REPO/scripts/bootstrap.$NETWORK.env" ] && ENVF="$REPO/scripts/bootstrap.$NETWORK.env"
fi
[ -f "$ENVF" ] || { echo "✗ no $ENVF - run scripts/bootstrap.sh first" >&2; exit 2; }
# shellcheck disable=SC1090
source "$ENVF"

# Re-assert the caller's choice over the env file's snapshot, then let an
# explicit LPAD_SEQUENCER_ADDR from the environment (not the file) win.
SEQ="${LPAD_SEQUENCER_ADDR:-$SEQ}"
export LPAD_NETWORK="$NETWORK"
NET=(--network "$NETWORK")

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
  # An all-zero owner is not a mismatch, it is a DEFAULT account: the id exists in
  # the address space but nothing has claimed it. That is what a chain reset looks
  # like from here - the ids in the env file are still valid PDAs, the state they
  # pointed at is gone. Reporting it as "OWNER MISMATCH" sent the reader looking
  # for an artifact drift that had not happened.
  if [ "$got" = "[0, 0, 0, 0, 0, 0, 0, 0]" ]; then
    no "$label: $acct is UNCLAIMED (owner all-zero) - never created, or the chain was reset. Re-run scripts/bootstrap.sh"
    return
  fi
  want=$(hex_to_words "$want_hex")
  if [ "$got" = "$want" ]; then
    ok "$label: on chain and owned by this build's program id"
  else
    no "$label: OWNER MISMATCH - chain has $got, this build embeds $want (artifacts changed since deployment; redeploy)"
  fi
}

# `program-id` is offline (it hashes the embedded ELF), so it needs no --network.
BC_ID=$("$LPAD" program-id bc   --json | python3 -c 'import json,sys;print(json.load(sys.stdin)["program_id"])')
LBP_ID=$("$LPAD" program-id lbp --json | python3 -c 'import json,sys;print(json.load(sys.stdin)["program_id"])')
WLEZ_ID=$("$LPAD" program-id wlez --json | python3 -c 'import json,sys;print(json.load(sys.stdin)["program_id"])')

[ -n "${LPAD_SALE_ID:-}" ]     && check_pda "bonding-curve sale" "${LPAD_SALE_ID}"     "$BC_ID"  || no "LPAD_SALE_ID not in $ENVF"
[ -n "${LPAD_LBP_POOL_ID:-}" ] && check_pda "LBP pool"           "${LPAD_LBP_POOL_ID}" "$LBP_ID" || no "LPAD_LBP_POOL_ID not in $ENVF"

# wlez owns no account that bootstrap records, which is why this check used to
# cover 2 of the 3 deployed programs. Its two PDAs are pure derivations from the
# program id, so `program-id wlez --json` can hand them over offline. The vault
# is claimed by Initialize, which every `wrap` path runs idempotently - so if it
# is on chain and owned by this build's wlez, wlez is deployed and current.
WLEZ_VAULT=$("$LPAD" program-id wlez --json | python3 -c 'import json,sys;print(json.load(sys.stdin).get("vault",""))')
if [ -n "$WLEZ_VAULT" ]; then
  check_pda "WLEZ vault" "$WLEZ_VAULT" "$WLEZ_ID"
else
  no "could not derive the WLEZ vault id from 'lpad program-id wlez --json'"
fi

# And the high-level reads must work through the CLI - against the SAME network
# the RPC assertions above used. Without --network these inherit the wallet
# config, so they could report a healthy deployment on a different chain.
echo "== CLI reads =="
"$LPAD" "${NET[@]}" bc sale-info  --sale "${LPAD_SALE_ID:-}"     --json >/dev/null 2>&1 && ok "bc sale-info"  || no "bc sale-info"
"$LPAD" "${NET[@]}" lbp pool-info --pool "${LPAD_LBP_POOL_ID:-}" --json >/dev/null 2>&1 && ok "lbp pool-info" || no "lbp pool-info"

echo
echo "passed $PASS, failed $FAIL"
[ "$FAIL" -eq 0 ]
