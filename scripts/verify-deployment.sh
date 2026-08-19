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
# That shape covers bc, lbp and wlez, each of which owns an account to point at.
# The fourth program a deployment needs, the canonical ATA program, owns none -
# nothing deploys it and bootstrap creates no ATA - so it gets its own pair of
# checks near the bottom.
#
# The CLI has its own deployment check now (`lpad network`, via the SDK's
# `lpad_deployment_status`), and it points HERE rather than replacing this script,
# because the two answer different questions and this is the stronger one:
#
#   - the SDK asks "are this ELF's bytes in a recent block?" - it re-derives the
#     deploy transaction hash from the bytecode and looks for the inclusion. That
#     is what you need BEFORE deploying, and its "no" is not proof of absence: the
#     lookback is bounded, and a sequencer that was reset or restored can have
#     forgotten the transaction while still running the program.
#   - this script asks "is the state lpad created still here, and does the chain
#     still run the exact guest this checkout embeds?" That catches artifact drift
#     after a rebuild, which no block-scan can see: the old deploy is still in its
#     block, and every PDA derived from the old id is orphaned anyway.
#
# So neither is redundant, and this one is not rewritten to call the CLI's.
#
# Env file: scripts/bootstrap.<network>.env when it exists, else
# scripts/bootstrap.env. LPAD_BOOTSTRAP_OUT overrides both. The per-network
# default matters - the ids inside are PDAs on one specific chain.
#
#   bash scripts/verify-deployment.sh
#   LPAD_NETWORK=paradox bash scripts/verify-deployment.sh
#   LPAD_NETWORK=local=http://127.0.0.1:3040 bash scripts/verify-deployment.sh
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
# Aliases, kept in step with `lpad_sdk::Network` and the other scripts. `local` /
# `local=<url>` is a sequencer the USER runs - lpad bundles none - and
# `http://127.0.0.1:3040` mirrors `lpad_sdk::LOCAL_SEQUENCER`, the default the
# CLI's picker offers. Bare `local` gets a bootstrap.local.env of its own; a
# `local=<url>` falls back to scripts/bootstrap.env exactly as a bare URL does,
# since a URL cannot be part of a filename.
case "$NETWORK" in
  testnet)  SEQ="https://testnet.lez.logos.co" ;;
  paradox)  SEQ="https://seq-testnet.paradox.computer" ;;
  local)    SEQ="http://127.0.0.1:3040" ;;
  local=*)  SEQ="${NETWORK#local=}" ;;
  http*)    SEQ="$NETWORK" ;;
  *) echo "unknown LPAD_NETWORK '$NETWORK' - use testnet, paradox, local, local=<url>, or an http(s) URL" >&2; exit 2 ;;
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
# Say why the file is needed, not just that it is missing. `lpad network` sends
# people here when a chain has no record of lpad's programs, and someone who has
# never bootstrapped that network arrives with nothing to check: every assertion
# below is about state ONE bootstrap run created, so there is no way to run this
# without that run's record. The URL is used rather than the alias because
# bootstrap.sh takes any http(s) sequencer and has no `local` alias of its own.
[ -f "$ENVF" ] || {
  echo "✗ no $ENVF" >&2
  echo "  This checks the sale, pool, vault and ATA ids one bootstrap run created" >&2
  echo "  on this chain, so it needs that run's record. Create it with:" >&2
  echo "    LPAD_NETWORK=$SEQ LPAD_BOOTSTRAP_OUT=$ENVF bash scripts/bootstrap.sh" >&2
  exit 2
}
# shellcheck disable=SC1090
source "$ENVF"

# Re-assert the caller's choice over the env file's snapshot, then let an
# explicit LPAD_SEQUENCER_ADDR from the environment (not the file) win.
SEQ="${LPAD_SEQUENCER_ADDR:-$SEQ}"
export LPAD_NETWORK="$NETWORK"
NET=(--network "$NETWORK")

PASS=0; FAIL=0; SKIP=0
ok(){ printf '  \033[32m✓\033[0m %s\n' "$*"; PASS=$((PASS+1)); }
no(){ printf '  \033[31m✗\033[0m %s\n' "$*"; FAIL=$((FAIL+1)); }
# A third outcome, counted separately and never fatal: there was nothing on chain
# to assert against, and a healthy deployment can legitimately be in that state.
# Only the ATA check can reach it - see the comment above it.
skip(){ printf '  \033[33m!\033[0m %s\n' "$*"; SKIP=$((SKIP+1)); }

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

# The same normalisation in the other direction: any account form -> 32-byte hex,
# for comparing ids to each other rather than sending them to the RPC.
acct_hex() {
  python3 -c '
import sys
B58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
s = sys.argv[1].split("/", 1)[-1]
if len(s) == 64 and all(c in "0123456789abcdefABCDEF" for c in s):
    out = s.lower()
else:
    n = 0
    for c in s:
        n = n * 58 + B58.index(c)
    out = n.to_bytes(32, "big").hex()
print(out)' "$1"
}

# The Associated Token Account id for (owner, definition) under an ATA program,
# all three given as hex, printed as hex. Mirrors ata_core::compute_ata_seed +
# AccountId::for_public_pda - the pair `sdk/src/lib.rs::ata_id` calls:
#     seed = sha256(owner || definition)
#     id   = sha256("/LEE/v0.2/AccountId/PDA/" + 8 NUL bytes + program_id + seed)
# Mirrored here because the CLI has no offline `ata id` command today:
# `create-ata` is the only ATA derivation it exposes and it SUBMITS when the
# account is absent, which a read-only check must not do. Adding one would turn
# the check below into a `program-id`-style one-liner, like the wlez vault.
ata_id() {
  python3 -c '
import hashlib, sys
pid, owner, definition = sys.argv[1:4]
seed = hashlib.sha256(bytes.fromhex(owner) + bytes.fromhex(definition)).digest()
prefix = b"/LEE/v0.2/AccountId/PDA/" + bytes(8)
print(hashlib.sha256(prefix + bytes.fromhex(pid) + seed).hexdigest())' "$1" "$2" "$3"
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

# The `program_owner` of an account id (base58), or ABSENT when `getAccount`
# returns no account at all - which it also does for an unreachable sequencer, so
# every caller has to treat ABSENT as "nothing to assert against", not as proof.
acct_owner() {
  rpc getAccount "[\"$1\"]" \
    | python3 -c 'import sys,json
try:
    r=json.load(sys.stdin)["result"]
    print(r["program_owner"] if r else "ABSENT")
except Exception: print("ABSENT")' 2>/dev/null
}

# Each PDA must exist AND be owned by the program id this build embeds.
check_pda() {
  local label="$1" acct="$2" want_hex="$3"
  local got want
  acct=$(norm_acct "$acct")
  got=$(acct_owner "$acct")
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

# An ATA is not a PDA this build owns - the token program claims it - so
# check_pda's owner comparison says nothing about it. What identifies it is its
# contents: `balance` parses the account as a token holding and reports the
# definition it holds, which must be the definition the id was derived from.
check_ata() {
  local ata="$1" want_def="$2"
  local acct got def
  acct=$(norm_acct "$ata")
  got=$(acct_owner "$acct")
  # Absent or unclaimed is NOT a failure here, which is the one way this check
  # differs from the three above: bootstrap creates no ATA, so a chain that has
  # only been bootstrapped correctly has none, and the same reading is what an id
  # derived under the wrong ata program would produce. Assertion 1 is the one
  # that fails in that case; there is nothing to conclude from an empty slot.
  if [ "$got" = "ABSENT" ] || [ -z "$got" ] || [ "$got" = "[0, 0, 0, 0, 0, 0, 0, 0]" ]; then
    skip "canonical ATA: nothing at $acct - no *-ata command has run on this chain yet (bootstrap creates none), so there is no ATA to check"
    return
  fi
  def=$("$LPAD" "${NET[@]}" balance --account "$ata" --json 2>/dev/null \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["definition"])' 2>/dev/null)
  if [ "$def" = "$want_def" ]; then
    ok "canonical ATA: $acct is a live token holding for the definition it derives from"
  elif [ -z "$def" ]; then
    no "canonical ATA: $acct is claimed but does not read as a token holding - something other than the ATA program owns the derived id"
  else
    no "canonical ATA: $acct holds definition $def, not the $want_def it was derived from (the derivation and the chain disagree)"
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
  # An UNCLAIMED vault is not a deployment failure, and reporting it as one made
  # this script fail on every freshly bootstrapped chain - testnet and paradox
  # alike - for a reason that has nothing to do with deployment. The vault is
  # claimed by wlez's `Initialize`, which only ever runs as part of a `wrap`, and
  # `scripts/bootstrap.sh` never wraps. So on a chain where lpad is correctly
  # deployed but nobody has wrapped yet, the account legitimately does not exist.
  #
  # The three outcomes are genuinely different and are now reported differently:
  #   unclaimed          -> skip: no wrap has run here; says nothing about wlez
  #   owned by this wlez -> pass: wlez is deployed AND current
  #   owned by another   -> fail: real artifact drift, still a hard failure
  #
  # "Is wlez deployed?" is answered directly and without this proxy by
  # `lpad --network <net> network --check`, which reads the ELF's own bytes out of
  # a block.
  WLEZ_VAULT_OWNER=$(acct_owner "$(norm_acct "$WLEZ_VAULT")")
  if [ "$WLEZ_VAULT_OWNER" = "[0, 0, 0, 0, 0, 0, 0, 0]" ] || [ "$WLEZ_VAULT_OWNER" = "ABSENT" ]; then
    skip "WLEZ vault: no wrap has run on this chain, so the vault was never claimed - use \`lpad network --check\` for whether wlez is deployed"
  else
    check_pda "WLEZ vault" "$WLEZ_VAULT" "$WLEZ_ID"
  fi
else
  no "could not derive the WLEZ vault id from 'lpad program-id wlez --json'"
fi

# The canonical ATA program is the fourth program a live deployment needs, and
# the only one nothing deploys: v0.2.1 pre-deploys it at genesis, so on our side
# its id is a compile-time constant - which is exactly how it can go stale
# unnoticed. It is pinned into every sale and pool at creation (SaleState and
# PoolState both store `ata_program_id`, and the programs reject a call that
# arrives under any other), and every `*-ata` command submits against it. Two
# assertions, because neither covers it alone.
ATA_ID=$("$LPAD" program-id ata --json | python3 -c 'import json,sys;print(json.load(sys.stdin)["program_id"])')

# 1. Offline: the id this build embeds against the one this deployment was
#    created with. No CLI read surfaces a sale's stored ata_program_id, so the
#    bootstrap record is the copy available here. A mismatch is the ATA
#    equivalent of OWNER MISMATCH above - the sales on this chain will not accept
#    calls dispatched to the program this checkout would use.
if [ -z "${LPAD_ATA_PROGRAM_ID:-}" ]; then
  no "LPAD_ATA_PROGRAM_ID not in $ENVF"
elif [ "$ATA_ID" = "$LPAD_ATA_PROGRAM_ID" ]; then
  ok "ATA program: this build embeds the id this deployment was created with"
else
  no "ATA program: ID MISMATCH - $ENVF was bootstrapped against $LPAD_ATA_PROGRAM_ID, this build embeds $ATA_ID (every *-ata command would target a program these sales reject)"
fi

# 2. On chain: an ATA derived under that id must be real state. If the derivation
#    ever drifts from ata_core the derived id simply stops matching anything, and
#    this reports "nothing there" rather than a false alarm.
if [ -n "${LPAD_CREATOR:-}" ] && [ -n "${LPAD_COLL_DEF:-}" ]; then
  ATA_DEF=$(acct_hex "${LPAD_COLL_DEF}")
  check_ata "$(ata_id "$ATA_ID" "$(acct_hex "${LPAD_CREATOR}")" "$ATA_DEF")" "$ATA_DEF"
else
  no "LPAD_CREATOR / LPAD_COLL_DEF not in $ENVF - no (owner, definition) pair to derive an ATA from"
fi

# And the high-level reads must work through the CLI - against the SAME network
# the RPC assertions above used. Without --network these inherit the wallet
# config, so they could report a healthy deployment on a different chain.
echo "== CLI reads =="
"$LPAD" "${NET[@]}" bc sale-info  --sale "${LPAD_SALE_ID:-}"     --json >/dev/null 2>&1 && ok "bc sale-info"  || no "bc sale-info"
"$LPAD" "${NET[@]}" lbp pool-info --pool "${LPAD_LBP_POOL_ID:-}" --json >/dev/null 2>&1 && ok "lbp pool-info" || no "lbp pool-info"

echo
echo "passed $PASS, failed $FAIL"
# Say it out loud rather than letting a clean "failed 0" imply everything was
# asserted: a skip means the state a check needs was not there to look at.
[ "$SKIP" -ne 0 ] && echo "$SKIP check(s) had nothing on chain to assert against - see the ! lines above"
[ "$FAIL" -eq 0 ]
