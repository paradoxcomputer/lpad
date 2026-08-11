#!/usr/bin/env bash
# Exercises EVERY lpad CLI command against a live sequencer (44 commands: 14
# top-level, 15 `bc`, 15 `lbp`; `help` excluded).
# Prereq: `bash scripts/bootstrap.sh` against the target network (BC + LBP sales)
# emitting the env file below. Run: bash scripts/test-all-cli.sh
#
# Env:
#   LPAD_NETWORK        testnet (default) | paradox | an http(s) sequencer URL.
#                       Selects BOTH the wallet home and the --network passed to
#                       every online invocation. See "network selection" below.
#   LPAD_WALLET_HOME    override the wallet home the network alias would pick
#   LPAD_TEST_ENV       bootstrap env file (default: scripts/bootstrap.<net>.env
#                       if present, else scripts/bootstrap.env)
#   LPAD_SKIP_PRIVATE   1 = skip only the real-STARK private ops, so the fast
#                       public sweep can run first. Skips are REPORTED, and a
#                       skip is never counted as a pass.
#   LPAD_OFFLINE_ONLY   1 = stop after the offline section. Nothing touches the
#                       chain, so it is the safe way to smoke-test this script.
#   LPAD_ALLOW_ENV_MISMATCH  1 = don't refuse an env file bootstrapped against a
#                       different network (see the guard below)
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"

# ---------------------------------------------------------------------------
# Network selection.
#
# The env file is a snapshot of ONE bootstrap run and re-exports that run's
# LPAD_NETWORK / LEE_WALLET_HOME_DIR / LPAD_SEQUENCER_ADDR. Sourcing it would
# therefore clobber the caller's choice and silently drag the sweep back to the
# network that produced the file - the exact failure this script used to have
# (no --network plumbing at all, so it just inherited whatever the wallet config
# happened to point at and could never target paradox). So: capture every
# caller-supplied knob FIRST, source the file, then re-assert the knobs.
# ---------------------------------------------------------------------------
NETWORK="${LPAD_NETWORK:-testnet}"
# `logos` is an alias for `testnet`; canonicalise it so the per-network env-file
# default below cannot name a bootstrap.logos.env that nothing writes.
[ "$NETWORK" = "logos" ] && NETWORK=testnet
SKIP_PRIVATE="${LPAD_SKIP_PRIVATE:-0}"
WALLET_HOME_OVERRIDE="${LPAD_WALLET_HOME:-}"
ENVFILE_OVERRIDE="${LPAD_TEST_ENV:-}"

# Network aliases, kept in step with `lpad_sdk::Network` and scripts/bootstrap.sh.
# Each alias also names the wallet home bootstrap.sh created for it: the wallet
# holds per-chain accounts and shielded notes, so pointing a testnet wallet at
# paradox reads as "every account is absent".
case "$NETWORK" in
  testnet|logos)      NET_HOME="$HOME/.lpad";         NET_ADDR="https://testnet.lez.logos.co" ;;
  paradox)            NET_HOME="$HOME/.lpad-paradox"; NET_ADDR="https://seq-testnet.paradox.computer" ;;
  http://*|https://*) NET_HOME="$HOME/.lpad";         NET_ADDR="$NETWORK" ;;
  *) echo "unknown LPAD_NETWORK '$NETWORK' - use testnet, paradox, or an http(s) URL" >&2; exit 2 ;;
esac

# Default env file is per-network when one exists (bootstrap.sh writes wherever
# LPAD_BOOTSTRAP_OUT says), falling back to the historical single-file default so
# an unset LPAD_NETWORK behaves exactly as before.
ENVFILE="$ENVFILE_OVERRIDE"
if [ -z "$ENVFILE" ]; then
  ENVFILE="$REPO/scripts/bootstrap.env"
  [ -f "$REPO/scripts/bootstrap.$NETWORK.env" ] && ENVFILE="$REPO/scripts/bootstrap.$NETWORK.env"
fi
[ -f "$ENVFILE" ] || { echo "no bootstrap env file at $ENVFILE - run scripts/bootstrap.sh" >&2; exit 2; }
# shellcheck disable=SC1090
source "$ENVFILE"

# Re-assert the caller's choices over the env file's snapshot.
LPAD_NETWORK="$NETWORK"; export LPAD_NETWORK
export LEE_WALLET_HOME_DIR="${WALLET_HOME_OVERRIDE:-$NET_HOME}"

# The account/sale/pool ids in the env file are PDAs on one specific chain. Using
# testnet ids against paradox would not error usefully: a rejected tx and a slow
# chain are indistinguishable ("All pollers failed"), and getAccount reports an
# absent account rather than a mismatch. Fail loudly up front instead.
ENV_ADDR="${LPAD_SEQUENCER_ADDR:-}"
if [ -n "$ENV_ADDR" ] && [ "$ENV_ADDR" != "$NET_ADDR" ] && [ "${LPAD_ALLOW_ENV_MISMATCH:-0}" != "1" ]; then
  echo "env file $ENVFILE was bootstrapped against $ENV_ADDR, but LPAD_NETWORK=$NETWORK is $NET_ADDR." >&2
  echo "Its ids are PDAs on the other chain. Bootstrap that network first:" >&2
  echo "  LPAD_NETWORK=$NETWORK LPAD_BOOTSTRAP_OUT=$REPO/scripts/bootstrap.$NETWORK.env bash scripts/bootstrap.sh" >&2
  echo "(or set LPAD_ALLOW_ENV_MISMATCH=1 if you really mean it)" >&2
  exit 2
fi

# The shielded holdings only exist if bootstrap.sh itself ran the private path;
# with LPAD_SKIP_PRIVATE=1 it emits them empty, and older env files omit them
# entirely - which under `set -u` used to abort the whole sweep at `bc
# buy-private`. Default them so a missing shielded holding degrades to a reported
# skip instead of killing the run.
: "${LPAD_PRIV_COLL:=}"
: "${LPAD_PRIV_PROJ:=}"

export PATH="$HOME/.cargo/bin:$HOME/.risc0/bin:$PATH"
# Default to REAL proofs (RISC0_DEV_MODE=0) to match how the launchpad runs;
# dev/fake proofs are NOT usable against a real sequencer - it verifies them.
export RISC0_DEV_MODE="${RISC0_DEV_MODE:-0}"
L="$REPO/cli/target/release/lpad"
# --network is a clap global, so it is accepted before the subcommand and
# propagates into `bc`/`lbp`. It overrides the wallet config for that invocation
# only, which is what makes one wallet home usable against several sequencers.
# Offline commands never resolve a sequencer, so they are left untouched.
NET=(--network "$NETWORK")

echo "## target: $NETWORK ($NET_ADDR)"
echo "##   wallet home: $LEE_WALLET_HOME_DIR"
echo "##   env file:    $ENVFILE"
[ "$SKIP_PRIVATE" = "1" ] && echo "##   LPAD_SKIP_PRIVATE=1 - real-STARK private ops will be skipped"
echo

PASS=0; FAIL=0; SKIP=0; declare -a FAILED; declare -a SKIPPED
chk() { local n="$1"; shift
  if "$@" >/tmp/cli_t.out 2>&1; then echo "  ✓ $n"; PASS=$((PASS+1))
  else echo "  ✗ $n"; FAIL=$((FAIL+1)); FAILED+=("$n"); sed 's/^/        /' /tmp/cli_t.out | tail -4; fi; }
# A skip is neither a pass nor a fail: it is recorded and printed in the tally so
# a partial sweep can never be mistaken for full coverage.
skip() { echo "  - $1 (skipped: $2)"; SKIP=$((SKIP+1)); SKIPPED+=("$1 [$2]"); }
# chkp: a private op. Under v0.2.4's 7-note privacy padding every one of these
# needs a real recursive STARK (hours of CPU), which is why the fast public sweep
# wants them out of the way.
chkp() { local n="$1"; shift
  if [ "$SKIP_PRIVATE" = "1" ]; then skip "$n" "LPAD_SKIP_PRIVATE=1"; return 0; fi
  chk "$n" "$@"; }
# chkps: a private op that also SPENDS a shielded holding, so it additionally
# needs LPAD_PRIV_* from the env file. LPAD_SKIP_PRIVATE wins as the reason.
chkps() { local n="$1"
  if [ "$SKIP_PRIVATE" != "1" ] && { [ -z "$LPAD_PRIV_COLL" ] || [ -z "$LPAD_PRIV_PROJ" ]; }; then
    skip "$n" "no shielded holdings (LPAD_PRIV_*) in $ENVFILE"; return 0
  fi
  chkp "$@"; }
# `bc ids` / `lbp ids` are pure PDA derivation (offline), hence no --network.
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
chk "program-id bc"   "$L" program-id bc
chk "program-id lbp"  "$L" program-id lbp
chk "program-id ata"  "$L" program-id ata
chk "program-id wlez" "$L" program-id wlez
chk "lbp allowlist-leaf" "$L" lbp allowlist-leaf --account "$LPAD_BUYER_COLL"

# Everything below is ONLINE and therefore carries "${NET[@]}".
[ "${LPAD_OFFLINE_ONLY:-0}" = "1" ] && { echo; echo "=== $PASS passed, $FAIL failed, $SKIP skipped (offline only) ==="
  if [ "$FAIL" -gt 0 ]; then printf 'FAILED: %s\n' "${FAILED[@]}"; exit 1; fi; exit 0; }

# ---------------------------------------------------------------------------
# Preflight: are the env file's ids still ALIVE on this chain?
#
# The mismatch guard above only proves the env file names the same SEQUENCER. It
# cannot notice that the chain was RESET underneath it - and these public
# testnets do reset. Observed 2026-08-07: the Logos sequencer 502'd mid-sweep and
# came back with head 1424 -> 1208, the creator's nonce 3 -> 0, and every
# definition/holding/sale/pool reading empty. The sweep kept going and reported
# ~40 consecutive command "failures" that were nothing of the kind, which is
# expensive to debug and actively misleading: a reset is indistinguishable from a
# broken build if you only look at the tally.
#
# So assert up front that the sale PDA still exists AND is still owned by this
# build's program id. Cheap (two reads), and it converts a reset into one clear
# message instead of a wall of false failures.
# ---------------------------------------------------------------------------
preflight_state_alive() {
  python3 - "$NET_ADDR" "$LPAD_SALE_ID" "$LPAD_BC_PROGRAM_ID" "$LPAD_LBP_POOL_ID" "$LPAD_LBP_PROGRAM_ID" <<'PY'
import json, sys, urllib.request
addr, sale, bc_hex, pool, lbp_hex = sys.argv[1:6]
B58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"

def bare(s):
    # getAccount takes ONLY bare base58: a 'Public/' prefix or a hex id returns
    # "invalid base58" and would read as ABSENT - a false reset signal.
    s = s.split("/", 1)[-1]
    if len(s) == 64 and all(c in "0123456789abcdefABCDEF" for c in s):
        b = bytes.fromhex(s); n = int.from_bytes(b, "big"); o = ""
        while n:
            n, r = divmod(n, 58); o = B58[r] + o
        s = "1" * (len(b) - len(b.lstrip(b"\0"))) + o
    return s

def rpc(method, params):
    req = urllib.request.Request(
        addr, json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode(),
        {"content-type": "application/json"})
    return json.load(urllib.request.urlopen(req, timeout=60)).get("result")

def words(h):
    b = bytes.fromhex(h)
    return [int.from_bytes(b[i:i + 4], "little") for i in range(0, 32, 4)]

bad = []
for label, acct, prog_hex in (("bonding-curve sale", sale, bc_hex), ("LBP pool", pool, lbp_hex)):
    if not acct:
        continue
    try:
        r = rpc("getAccount", [bare(acct)])
    except Exception as e:
        bad.append(f"{label}: RPC failed ({e})"); continue
    data = bytes((r or {}).get("data") or b"")
    if not data:
        bad.append(f"{label} {acct} has NO state on chain (account empty)")
    elif r.get("program_owner") != words(prog_hex):
        bad.append(f"{label} {acct} owner {r.get('program_owner')} != this build's {words(prog_hex)}")

if bad:
    print("✗ preflight: the env file's on-chain state is gone or foreign:", file=sys.stderr)
    for b in bad:
        print(f"    - {b}", file=sys.stderr)
    print("  The chain was most likely RESET (these testnets do). Redeploy and", file=sys.stderr)
    print("  re-bootstrap before sweeping, or every write below will 'fail' for", file=sys.stderr)
    print("  reasons that have nothing to do with lpad:", file=sys.stderr)
    print("    bash scripts/verify-deployment.sh", file=sys.stderr)
    print("    bash scripts/bootstrap.sh", file=sys.stderr)
    sys.exit(1)
print("  ✓ preflight: sale + pool still on chain and owned by this build")
PY
}
preflight_state_alive || exit 2

echo "## ONLINE - reads"
chk "status"         "$L" "${NET[@]}" status
chk "balance"        "$L" "${NET[@]}" balance --account "$LPAD_BUYER_COLL"
chk "bc sale-info"   "$L" "${NET[@]}" bc sale-info --sale "$LPAD_SALE_ID"
chk "lbp pool-info"  "$L" "${NET[@]}" lbp pool-info --pool "$LPAD_LBP_POOL_ID" --at "$(date +%s%3N)"

echo "## ONLINE - discovery"
chk "my-balance" "$L" "${NET[@]}" my-balance
chk "my-sales"   "$L" "${NET[@]}" my-sales
chk "my-pools"   "$L" "${NET[@]}" my-pools

echo "## ONLINE - holdings"
# A token transfer to a never-initialised holding is REJECTED (the token program
# claims the recipient as Authorized, which the runtime only grants to a signer)
# and the LEZ wallet has no equivalent command - so `init-holding` is how you
# make a recipient before sending to it. Creates a fresh holding each run.
chk "init-holding" "$L" "${NET[@]}" init-holding --token-def "$LPAD_COLL_DEF"

echo "## ONLINE - bonding-curve writes"
chk "bc buy"                  "$L" "${NET[@]}" bc buy --program "$LPAD_BC_PROGRAM_ID" --sale "$LPAD_SALE_ID" --buyer-collateral "$LPAD_BUYER_COLL" --buyer-token "$LPAD_BUYER_TOK" --in 1000 --min-out 0
chk "bc sell"                 "$L" "${NET[@]}" bc sell --program "$LPAD_BC_PROGRAM_ID" --sale "$LPAD_SALE_ID" --seller-token "$LPAD_BUYER_TOK" --seller-collateral "$LPAD_BUYER_COLL" --tokens 500 --min-out 0
chkps "bc buy-private" "$L" "${NET[@]}" bc buy-private --program "$LPAD_BC_PROGRAM_ID" --sale "$LPAD_SALE_ID" --user-collateral "$LPAD_PRIV_COLL" --user-token "$LPAD_PRIV_PROJ" --in 1000 --min-out 0
# sell-private is the mirror of buy-private (deshield tokens -> public sell ->
# re-shield collateral) and must run AFTER it: the shielded project holding is
# only seeded with dust by bootstrap, so the tokens sold here are the ones the
# buy-private above delivered.
chkps "bc sell-private" "$L" "${NET[@]}" bc sell-private --program "$LPAD_BC_PROGRAM_ID" --sale "$LPAD_SALE_ID" --user-token "$LPAD_PRIV_PROJ" --user-collateral "$LPAD_PRIV_COLL" --tokens 500 --min-out 0

echo "## ONLINE - LBP writes"
chk "lbp buy"                  "$L" "${NET[@]}" lbp buy --program "$LPAD_LBP_PROGRAM_ID" --pool "$LPAD_LBP_POOL_ID" --buyer-collateral "$LPAD_BUYER_COLL" --buyer-token "$LPAD_BUYER_TOK" --in 1000 --min-out 0
chkps "lbp buy-private" "$L" "${NET[@]}" lbp buy-private --program "$LPAD_LBP_PROGRAM_ID" --pool "$LPAD_LBP_POOL_ID" --user-collateral "$LPAD_PRIV_COLL" --user-token "$LPAD_PRIV_PROJ" --in 1000 --min-out 0
chk "lbp poke"   "$L" "${NET[@]}" lbp poke   --program "$LPAD_LBP_PROGRAM_ID" --pool "$LPAD_LBP_POOL_ID"
chk "lbp pause"  "$L" "${NET[@]}" lbp pause  --program "$LPAD_LBP_PROGRAM_ID" --pool "$LPAD_LBP_POOL_ID" --creator "$LPAD_CREATOR"
chk "lbp resume" "$L" "${NET[@]}" lbp resume --program "$LPAD_LBP_PROGRAM_ID" --pool "$LPAD_LBP_POOL_ID" --creator "$LPAD_CREATOR"

echo "## ONLINE - native LEZ (wrap / unwrap; idempotently initializes wlez)"
# Size these against the creator's ACTUAL native balance rather than hardcoding.
# bootstrap.sh funds via a single `pinata claim`, which pays PRIZE = 150 - so the
# old fixed `--amount 2000` could not possibly succeed on a freshly bootstrapped
# chain, and reported a faucet limit as a wrap/unwrap defect. (It also cascades:
# `unwrap` then fails with "no WLEZ holding to unwrap", a second false failure.)
# Wrap ~40% of the balance so fees and the later shield-lez still have room, and
# unwrap half of what we wrapped so the WLEZ holding keeps a non-zero remainder.
NATIVE=$(python3 - "$NET_ADDR" "$LPAD_CREATOR" <<'PY'
import json, sys, urllib.request
addr, acct = sys.argv[1], sys.argv[2].split("/", 1)[-1]
try:
    req = urllib.request.Request(
        addr, json.dumps({"jsonrpc": "2.0", "id": 1, "method": "getAccount", "params": [acct]}).encode(),
        {"content-type": "application/json"})
    print(int((json.load(urllib.request.urlopen(req, timeout=60)).get("result") or {}).get("balance") or 0))
except Exception:
    print(0)
PY
)
WRAP_AMT=$(( NATIVE * 2 / 5 ))
UNWRAP_AMT=$(( WRAP_AMT / 2 ))
echo "   creator native balance = $NATIVE  ->  wrap $WRAP_AMT, unwrap $UNWRAP_AMT"
if [ "$WRAP_AMT" -lt 10 ]; then
  skip "wrap"   "creator native balance $NATIVE too low (need >=25; fund via more pinata claims)"
  skip "unwrap" "wrap was skipped, so there is no WLEZ holding"
else
  chk "wrap"   "$L" "${NET[@]}" wrap   --amount "$WRAP_AMT"
  chk "unwrap" "$L" "${NET[@]}" unwrap --amount "$UNWRAP_AMT"
fi

echo "## ONLINE - privacy (shield / deshield, public<->shielded + native-LEZ)"
# These four create their own shielded holding, so they need no LPAD_PRIV_*; they
# are still real STARKs and so still honour LPAD_SKIP_PRIVATE.
chkp "shield"       "$L" "${NET[@]}" shield       --token-def "$LPAD_COLL_DEF" --amount 50
chkp "deshield"     "$L" "${NET[@]}" deshield     --token-def "$LPAD_COLL_DEF" --amount 50
# Same faucet-sized reality as wrap/unwrap above: shield-lez wraps native LEZ
# before shielding it, so a fixed 500 cannot work off a 150-unit pinata claim.
# Budget ~20% of the original balance, which the wrap/unwrap pair above leaves
# intact (it wraps 40% and unwraps half of that back).
LEZ_PRIV_AMT=$(( NATIVE / 5 ))
if [ "$LEZ_PRIV_AMT" -lt 10 ]; then
  skip "shield-lez"   "creator native balance $NATIVE too low (need >=50)"
  skip "deshield-lez" "shield-lez was skipped, so there is no shielded WLEZ"
else
  chkp "shield-lez"   "$L" "${NET[@]}" shield-lez   --amount "$LEZ_PRIV_AMT"
  chkp "deshield-lez" "$L" "${NET[@]}" deshield-lez --amount "$LEZ_PRIV_AMT"
fi

echo "## ONLINE - native-LEZ sale (create-token-sale: mints token+metadata, WLEZ collateral)"
chk "bc create-token-sale" "$L" "${NET[@]}" bc create-token-sale --name "NativeTok" --symbol "NTK" --supply 1000000 --sale-quantity 100000 --vt 2000000 --vc 50000 --nonce "$(date +%s)"

echo "## ONLINE - ATAs (RFP Func: ATAs for all token interactions)"
# owner = the wallet's creator account; --fund-from seeds the owner's input ATA
# from a keypair holding before the ATA buy/sell.
chk "create-ata"  "$L" "${NET[@]}" create-ata --token-def "$LPAD_COLL_DEF" --owner "$LPAD_CREATOR"
chk "bc buy-ata"  "$L" "${NET[@]}" bc buy-ata  --program "$LPAD_BC_PROGRAM_ID" --sale "$LPAD_SALE_ID" --owner "$LPAD_CREATOR" --fund-from "$LPAD_BUYER_COLL" --in 1000 --min-out 0
chk "bc sell-ata" "$L" "${NET[@]}" bc sell-ata --program "$LPAD_BC_PROGRAM_ID" --sale "$LPAD_SALE_ID" --owner "$LPAD_CREATOR" --fund-from "$LPAD_PROJ_HOLD" --tokens 500 --min-out 0
chk "lbp buy-ata" "$L" "${NET[@]}" lbp buy-ata --program "$LPAD_LBP_PROGRAM_ID" --pool "$LPAD_LBP_POOL_ID" --owner "$LPAD_CREATOR" --fund-from "$LPAD_BUYER_COLL" --in 1000 --min-out 0

echo "## ONLINE - creator lifecycle (dedicated closeable sales)"
N7="${LPAD_LC_NONCE:-$(date +%s)}"   # unique nonce per run so PDAs never collide
# BC manual close needs the end timestamp to have passed, but BC *buys* are
# rejected once it has. So: open a wide window, buy early (a small buy initialises
# the collateral vault that withdraw needs, without exhausting the sale), wait the
# window out, then advance the clock with a tx on the open-ended main sale so the
# close is permitted.
BC_END=$(( $(date +%s%3N) + 60000 ))
chk "bc create-sale" "$L" "${NET[@]}" bc create-sale --program "$LPAD_BC_PROGRAM_ID" --collateral-def "$LPAD_COLL_DEF" --treasury "$LPAD_TREASURY" --creator-token-holding "$LPAD_PROJ_HOLD" --creator "$LPAD_CREATOR" --sale-quantity 100000 --dex-seed 100 --vt 2000000 --vc 50000 --fee-bps 100 --end-ts "$BC_END" --nonce "$N7"
BC7=$(sale_id "$N7")
chk "bc buy (close-test)" "$L" "${NET[@]}" bc buy --program "$LPAD_BC_PROGRAM_ID" --sale "$BC7" --buyer-collateral "$LPAD_BUYER_COLL" --buyer-token "$LPAD_BUYER_TOK" --in 100 --min-out 0
while [ "$(date +%s%3N)" -lt "$BC_END" ]; do sleep 2; done
"$L" "${NET[@]}" bc buy --program "$LPAD_BC_PROGRAM_ID" --sale "$LPAD_SALE_ID" --buyer-collateral "$LPAD_BUYER_COLL" --buyer-token "$LPAD_BUYER_TOK" --in 100 --min-out 0 >/dev/null 2>&1  # fresh block, clock past end_ts
chk "bc close"    "$L" "${NET[@]}" bc close    --program "$LPAD_BC_PROGRAM_ID" --sale "$BC7" --creator "$LPAD_CREATOR"
chk "bc withdraw" "$L" "${NET[@]}" bc withdraw --program "$LPAD_BC_PROGRAM_ID" --sale "$BC7" --creator-collateral "$LPAD_COLL_HOLD" --creator-token "$LPAD_PROJ_HOLD" --creator "$LPAD_CREATOR"

# LBP: t_end=1 → already ended → immediately closeable.
chk "lbp create-sale" "$L" "${NET[@]}" lbp create-sale --program "$LPAD_LBP_PROGRAM_ID" --collateral-def "$LPAD_COLL_DEF" --treasury "$LPAD_TREASURY" --creator-token-holding "$LPAD_PROJ_HOLD" --creator-collateral-holding "$LPAD_COLL_HOLD" --creator "$LPAD_CREATOR" --token-deposit 1000 --collateral-seed 100 --w-start 0.9 --w-end 0.1 --t-start 0 --t-end 1 --nonce "$N7"
LBP7=$(pool_id "$N7")
chk "lbp close"    "$L" "${NET[@]}" lbp close    --program "$LPAD_LBP_PROGRAM_ID" --pool "$LBP7" --creator "$LPAD_CREATOR"
chk "lbp withdraw" "$L" "${NET[@]}" lbp withdraw --program "$LPAD_LBP_PROGRAM_ID" --pool "$LBP7" --creator-collateral "$LPAD_COLL_HOLD" --creator-token "$LPAD_PROJ_HOLD" --creator "$LPAD_CREATOR"

echo "## ONLINE - allowlist-gated LBP (single-member tree: root == leaf(buyer), empty proof)"
GN="$(( N7 + 1 ))"   # numeric, distinct from the lifecycle LBP sale's nonce (N7)
GROOT=$("$L" lbp allowlist-leaf --account "$LPAD_BUYER_COLL" --json | python3 -c 'import json,sys;print(json.load(sys.stdin)["leaf"])')
GTS=$(date +%s%3N); GTE=$(( GTS + 31536000000 ))   # now .. now+1yr (under the ~10yr cap)
chk "lbp create-sale (gated)" "$L" "${NET[@]}" lbp create-sale --program "$LPAD_LBP_PROGRAM_ID" --collateral-def "$LPAD_COLL_DEF" --treasury "$LPAD_TREASURY" --creator-token-holding "$LPAD_PROJ_HOLD" --creator-collateral-holding "$LPAD_COLL_HOLD" --creator "$LPAD_CREATOR" --token-deposit 1000 --collateral-seed 100 --w-start 0.9 --w-end 0.1 --t-start "$GTS" --t-end "$GTE" --allowlist-root "$GROOT" --nonce "$GN"
GPOOL=$(pool_id "$GN")
chk "lbp buy-gated" "$L" "${NET[@]}" lbp buy-gated --program "$LPAD_LBP_PROGRAM_ID" --pool "$GPOOL" --buyer-collateral "$LPAD_BUYER_COLL" --buyer-token "$LPAD_BUYER_TOK" --in 50 --min-out 0

echo
echo "=== $PASS passed, $FAIL failed, $SKIP skipped ==="
# Skips are listed explicitly: with LPAD_SKIP_PRIVATE=1 a green run is NOT full
# coverage, and the follow-up private pass still owes these.
if [ "$SKIP" -gt 0 ]; then printf 'SKIPPED: %s\n' "${SKIPPED[@]}"; fi
if [ "$FAIL" -gt 0 ]; then printf 'FAILED: %s\n' "${FAILED[@]}"; exit 1; fi
