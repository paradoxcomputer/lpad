#!/usr/bin/env bash
# Exercises EVERY lpad CLI command against a live sequencer (50 commands: 16
# top-level, 17 `bc`, 17 `lbp`; `help` excluded), including both programs'
# `sweep-treasury` - the only instruction on either that can pay the escrowed
# protocol fee out to the treasury, so an untested one means broken settlement
# ships green.
#
# Nine of the 50 are real recursive STARKs (`buy-private` and `buy-disposable` on
# both programs, `bc sell-private`, shield/deshield x4); LPAD_SKIP_PRIVATE=1 skips those and
# REPORTS the skips. A skipped disposable buy leaves the bonding curve with no
# escrow, so `bc sweep-treasury` is then reported as a skip too, never as a
# pass.
# Prereq: `bash scripts/bootstrap.sh` against the target network (BC + LBP sales)
# emitting the env file below. Run: bash scripts/test-all-cli.sh
#
# Env:
#   LPAD_NETWORK        testnet (default) | paradox | local | local=<url> |
#                       an http(s) sequencer URL. Selects BOTH the wallet home and
#                       the --network passed to every online invocation. See
#                       "network selection" below.
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

# This harness is NON-INTERACTIVE and must stay that way. `lpad` asks, once, which
# sequencer a wallet talks to when nothing is recorded for it, so detach stdin for
# the whole run and every command it spawns: the picker only fires when stdin AND
# stderr are terminals, and /dev/null is not one. Nothing here reads stdin, so
# there is nothing to lose.
#
# That is the second of two independent guards - every online invocation below
# already passes an explicit `--network`, which is consumed before the picker is
# ever consulted. Both exist because either one alone is a single edit away from
# being lost: drop `--network` from one new command and the flag guard is gone;
# run this from a terminal with stdin inherited and only the flag stands between
# the sweep and a menu waiting forever inside `chk`, where its output is captured
# and nobody would see the question.
exec </dev/null

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
#
# `local` / `local=<url>` is a sequencer the USER runs - lpad bundles none - and
# `http://127.0.0.1:3040` mirrors `lpad_sdk::LOCAL_SEQUENCER`, the default the CLI's
# picker offers. It gets the same wallet home as a bare URL (`~/.lpad`, overridable
# with LPAD_WALLET_HOME) rather than a home of its own, because bootstrap.sh creates
# no `.lpad-local` and inventing one here would name a directory nothing writes.
case "$NETWORK" in
  testnet|logos)      NET_HOME="$HOME/.lpad";         NET_ADDR="https://testnet.lez.logos.co" ;;
  paradox)            NET_HOME="$HOME/.lpad-paradox"; NET_ADDR="https://seq-testnet.paradox.computer" ;;
  local)              NET_HOME="$HOME/.lpad";         NET_ADDR="http://127.0.0.1:3040" ;;
  local=*)            NET_HOME="$HOME/.lpad";         NET_ADDR="${NETWORK#local=}" ;;
  http://*|https://*) NET_HOME="$HOME/.lpad";         NET_ADDR="$NETWORK" ;;
  *) echo "unknown LPAD_NETWORK '$NETWORK' - use testnet, paradox, local, local=<url>, or an http(s) URL" >&2; exit 2 ;;
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

PASS=0; FAIL=0; SKIP=0; declare -a FAILED; declare -a SKIPPED; declare -a COVERED

# Set while a pool is deliberately paused, cleared once it is resumed. The trap
# below is what makes `lbp pause` safe to run against the shared bootstrap pool:
# without it, any exit between pause and resume leaves that pool paused for every
# future run, and the sweep would have bricked its own fixture.
LBP_PAUSED=""
resume_on_exit() {
  [ -n "$LBP_PAUSED" ] || return 0
  echo "!! the sweep paused $LBP_PAUSED and did not resume it - resuming now" >&2
  timeout 900 "$L" "${NET[@]}" lbp resume --program "$LPAD_LBP_PROGRAM_ID" \
    --pool "$LBP_PAUSED" --creator "$LPAD_CREATOR" >/dev/null 2>&1 \
    || echo "!! could not resume $LBP_PAUSED - resume it by hand before the next run" >&2
  LBP_PAUSED=""
}
trap resume_on_exit EXIT INT TERM
chk() { local n="$1"; shift
  COVERED+=("$n")
  if "$@" >/tmp/cli_t.out 2>&1; then echo "  ✓ $n"; PASS=$((PASS+1))
  else echo "  ✗ $n"; FAIL=$((FAIL+1)); FAILED+=("$n"); sed 's/^/        /' /tmp/cli_t.out | tail -4; fi; }
# A skip is neither a pass nor a fail: it is recorded and printed in the tally so
# a partial sweep can never be mistaken for full coverage.
skip() { echo "  - $1 (skipped: $2)"; SKIP=$((SKIP+1)); SKIPPED+=("$1 [$2]"); COVERED+=("$1"); }
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

# Every `--json` invocation must emit ONLY parseable JSON on stdout.
#
# It did not: opening a wallet that has never recorded sequencer latencies prints
# "Statistics not found, choosing empty" to STDOUT, so the first `--json` command
# after `lpad init-wallet` returned unparseable output. That is the new-user path,
# and `--json` is contracted to be the machine surface - a caller piping it to
# `python3 -m json.tool` got a syntax error rather than a payload. Checked here
# because it only reproduces against a real wallet + sequencer.
jsonchk() {
  local n="$1"; shift
  COVERED+=("$n")
  local out
  out=$("$@" 2>/dev/null)
  if printf '%s' "$out" | python3 -c 'import json,sys; json.load(sys.stdin)' 2>/dev/null; then
    echo "  ✓ $n (--json is parseable)"; PASS=$((PASS + 1))
  else
    echo "  ✗ $n (--json emitted non-JSON on stdout)"; FAIL=$((FAIL + 1)); FAILED+=("$n")
    printf '%s' "$out" | head -3 | sed 's/^/        /'
  fi
}

# `treasury_owed` straight off the chain: what the two sweep commands settle,
# and the only way to tell "nothing to settle" - a legitimate skip, since both
# guests REVERT when nothing is owed - apart from "settlement is broken", which
# must fail. Prints 0 if the read fails at all, so an unreadable account degrades
# to a reported skip instead of a false failure.
owed() { "$L" "${NET[@]}" --json "$@" 2>/dev/null | python3 -c 'import json,sys;print(json.load(sys.stdin).get("treasury_owed","0"))' 2>/dev/null || echo 0; }

echo "## OFFLINE (no chain: pure compute, plus one wallet-file read)"
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

# `lpad network` answers "which sequencer is this wallet aimed at?". Under --json
# it reports instead of prompting and makes no RPC at all - it reads the wallet's
# own config file - so it belongs in this section; unlike everything else here it
# needs a wallet to report ON, hence the skip rather than a failure when there is
# none.
#
# Checked on what it PRINTS, not merely on its exit status. This is the one command
# whose entire job is that answer, and the network the sweep passes has to be the
# one that comes back: a green exit reporting some other url would mean every
# ONLINE check below had quietly run against a different chain.
network_json_is_this_network() {
  local got
  got=$("$L" "${NET[@]}" network --json \
        | python3 -c 'import json,sys;print(json.load(sys.stdin)["url"])') || return 1
  [ "$got" = "$NET_ADDR" ] && return 0
  echo "network --json reported url=$got, expected $NET_ADDR"
  return 1
}
if [ -f "$LEE_WALLET_HOME_DIR/wallet_config.json" ]; then
  chk "network --json" network_json_is_this_network
else
  skip "network --json" "no wallet config at $LEE_WALLET_HOME_DIR (run scripts/bootstrap.sh)"
fi

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
jsonchk "status --json" "$L" "${NET[@]}" --json status
# `init-wallet` CREATES a wallet, so it cannot be aimed at this sweep's own: it
# refuses to overwrite one, and were it to succeed it would replace the keys
# every other check here depends on. A throwaway home is also the only honest
# test of it - the thing it has to work on is a directory that does not exist.
INIT_HOME="$(mktemp -d -t lpad-initwallet-XXXXXX)"; rmdir "$INIT_HOME"
chk "init-wallet" env LEE_WALLET_HOME_DIR="$INIT_HOME" "$L" "${NET[@]}" init-wallet
rm -rf "$INIT_HOME"
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
# A disposable buy is the ONLY thing that credits the curve's `treasury_owed`: it
# cannot pay the treasury inline (a privacy proof pins every public account it
# touches byte-for-byte, and a shared treasury moves constantly), so its fee is
# escrowed in the collateral vault. Every public path settles its fee in the same
# transaction and leaves nothing behind - so this is what gives the sweep below
# something to settle.
chkps "bc buy-disposable" "$L" "${NET[@]}" bc buy-disposable --program "$LPAD_BC_PROGRAM_ID" --sale "$LPAD_SALE_ID" --user-collateral "$LPAD_PRIV_COLL" --user-token "$LPAD_PRIV_PROJ" --in 1000 --min-out 0
# Settlement: a separate, permissionless instruction, and the only way that
# escrow ever reaches the treasury. Read the escrow first - with the disposable
# buy skipped there is legitimately nothing owed, and `SweepTreasury` reverts on
# an empty escrow, so calling it anyway would paint a working protocol red.
BC_OWED=$(owed bc sale-info --sale "$LPAD_SALE_ID")
if [ "${BC_OWED:-0}" = "0" ]; then
  skip "bc sweep-treasury" "sale owes the treasury nothing (only a disposable buy escrows a fee)"
else
  chk "bc sweep-treasury" "$L" "${NET[@]}" bc sweep-treasury --program "$LPAD_BC_PROGRAM_ID" --sale "$LPAD_SALE_ID"
fi

echo "## ONLINE - LBP writes"
chk "lbp buy"                  "$L" "${NET[@]}" lbp buy --program "$LPAD_LBP_PROGRAM_ID" --pool "$LPAD_LBP_POOL_ID" --buyer-collateral "$LPAD_BUYER_COLL" --buyer-token "$LPAD_BUYER_TOK" --in 1000 --min-out 0
chkps "lbp buy-private" "$L" "${NET[@]}" lbp buy-private --program "$LPAD_LBP_PROGRAM_ID" --pool "$LPAD_LBP_POOL_ID" --user-collateral "$LPAD_PRIV_COLL" --user-token "$LPAD_PRIV_PROJ" --in 1000 --min-out 0
# The LBP's own one-transaction private buy. It escrows nothing (the LBP takes no
# per-swap fee - its only fee is charged at withdrawal), so unlike the curve's it
# is here for command coverage, not for the sweep below.
chkps "lbp buy-disposable" "$L" "${NET[@]}" lbp buy-disposable --program "$LPAD_LBP_PROGRAM_ID" --pool "$LPAD_LBP_POOL_ID" --user-collateral "$LPAD_PRIV_COLL" --user-token "$LPAD_PRIV_PROJ" --in 1000 --min-out 0
chk "lbp poke"   "$L" "${NET[@]}" lbp poke   --program "$LPAD_LBP_PROGRAM_ID" --pool "$LPAD_LBP_POOL_ID"
# pause/resume act on the SHARED bootstrap pool - the one `lbp buy`,
# `lbp buy-private`, `lbp buy-disposable` and `lbp buy-ata` all use. If this run
# dies between the two (Ctrl-C, an OOM during a prover, an operator killing a
# 30-minute poll), or if `resume` simply fails, that pool stays paused and every
# later LBP command fails - in THIS run and in every re-run after it, until
# somebody resumes it by hand. A sweep must not be able to brick its own fixture,
# so the resume is guaranteed by a trap rather than by reaching the next line.
LBP_PAUSED="$LPAD_LBP_POOL_ID"
chk "lbp pause"  "$L" "${NET[@]}" lbp pause  --program "$LPAD_LBP_PROGRAM_ID" --pool "$LPAD_LBP_POOL_ID" --creator "$LPAD_CREATOR"
chk "lbp resume" "$L" "${NET[@]}" lbp resume --program "$LPAD_LBP_PROGRAM_ID" --pool "$LPAD_LBP_POOL_ID" --creator "$LPAD_CREATOR"
LBP_PAUSED=""

echo "## ONLINE - native LEZ (wrap / unwrap; idempotently initializes wlez)"
# Size these against the creator's ACTUAL native balance rather than hardcoding.
# bootstrap.sh funds via a single `pinata claim`, which pays PRIZE = 150 - so the
# old fixed `--amount 2000` could not possibly succeed on a freshly bootstrapped
# chain, and reported a faucet limit as a wrap/unwrap defect. (It also cascades:
# `unwrap` then fails with "no WLEZ holding to unwrap", a second false failure.)
# Wrap ~40% of the balance so fees and the later shield-lez still have room, and
# unwrap half of what we wrapped so the WLEZ holding keeps a non-zero remainder.
# The creator's native balance, read twice: once for the wrap/unwrap
# thresholds and again after `faucet`, which exists to change it.
native_balance() {
python3 - "$NET_ADDR" "$LPAD_CREATOR" <<'PY'
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
}
# Block until the chain advances, so a balance read after a transaction sees the
# state that transaction wrote.
#
# This used to call `wait_block`, which is defined in scripts/bootstrap.sh and
# has never existed in this file - so it always fell through to the `|| sleep 5`
# arm, and `2>/dev/null` hid the "command not found" that said so. Five seconds
# against a ~46-50s block is not a wait at all. Same class of bug as the
# `native_balance` call above, which was also a call to a function that was never
# written; `bash -n` passes both, which is how they shipped.
#
# Deliberately a local copy rather than a source of bootstrap.sh: that script
# runs a full bootstrap on load, and this harness must not.
seq_height() {
  timeout 60 curl -s -X POST "$NET_ADDR" -H 'content-type: application/json' \
    -d '{"jsonrpc":"2.0","id":1,"method":"getLastBlockId","params":[]}' 2>/dev/null \
    | python3 -c 'import sys,json; print(json.load(sys.stdin)["result"])' 2>/dev/null \
    || true
}
wait_block() {
  local want start now waited=0
  start=$(seq_height); [ -n "$start" ] || { sleep 60; return 0; }
  want=$((start + ${1:-1}))
  while [ "$waited" -lt "${LPAD_BLOCK_WAIT_MAX:-240}" ]; do
    sleep 10; waited=$((waited + 10))
    now=$(seq_height)
    [ -n "$now" ] && [ "$now" -ge "$want" ] && { printf '   (block %s, waited %ss)\n' "$now" "$waited" >&2; return 0; }
  done
  echo "   ! still at block $(seq_height) after ${waited}s (wanted >= $want) - continuing" >&2
}

# The ON-CHAIN clock in ms, or 0 if unreadable. This is the clock both guests
# gate on - a bonding-curve sale's `end_timestamp_ms`, an LBP's `t_start`/`t_end`
# - and it is NOT this machine's clock.
chain_ms() {
  "$L" "${NET[@]}" --json status 2>/dev/null \
    | python3 -c 'import sys,json
try: print(int(json.load(sys.stdin).get("chain_time_ms") or 0))
except Exception: print(0)' 2>/dev/null || echo 0
}

# Block until the CHAIN clock passes $1 (ms), nudging it with the command in
# "$@" whenever it is not moving.
#
# Waiting on `date` here is wrong twice over, and both ways cost 30 minutes of
# polling to discover. The clock lives in an ACCOUNT that only advances when a
# block is produced, so on a quiet chain it trails wall clock without bound: a
# local wait returns while the chain still believes the sale is open, and `close`
# is rejected with "sale has not ended". And nothing produces a block unless
# somebody sends a transaction - so the wait has to generate the very traffic it
# is waiting for. The nudge is issued only when the clock did NOT move since the
# last look, because each one is a real transaction and a busy chain needs none.
wait_chain_past() {
  local want="$1"; shift
  local now last="" waited=0 nudges=0
  local max="${LPAD_CHAIN_WAIT_MAX:-2400}" max_nudges="${LPAD_CHAIN_NUDGE_MAX:-10}"
  # An lpad that predates `status --json`'s `chain_time_ms` reports 0 forever.
  # Without this the loop can never satisfy `now >= want` and burns its whole
  # 2400s cap - twice per run - before continuing anyway. Fall back to the local
  # clock in that case: it is what this script did before the chain clock was
  # readable, and it is correct as long as the window is wide enough to survive
  # the drift, which is why the windows above are minutes rather than seconds.
  if [ "$(chain_ms)" = "0" ]; then
    echo "   ! this lpad does not report chain_time_ms - waiting on the local clock instead" >&2
    while [ "$(date +%s%3N)" -lt "$want" ]; do sleep 5; done
    "$@" >/dev/null 2>&1   # one nudge, so a block carries the chain clock past it
    sleep 30
    return 0
  fi
  while [ "$waited" -lt "$max" ]; do
    now=$(chain_ms)
    if [ -n "$now" ] && [ "$now" -ge "$want" ] 2>/dev/null; then
      printf '   (chain clock %s reached %s after %ss, %s nudge(s))\n' "$now" "$want" "$waited" "$nudges" >&2
      return 0
    fi
    if [ "$now" = "$last" ] && [ "$nudges" -lt "$max_nudges" ]; then
      "$@" >/dev/null 2>&1
      nudges=$((nudges + 1))
    fi
    last="$now"
    sleep 20; waited=$((waited + 20))
  done
  printf '   ! chain clock %s still short of %s after %ss and %s nudge(s) - continuing\n' \
    "$(chain_ms)" "$want" "$waited" "$nudges" >&2
}

NATIVE=$(native_balance)
# `faucet` tops the creator up before the thresholds below are measured - which is
# also the only way to exercise it, since it is the one command whose whole effect
# is to change that balance. It spends a shared faucet, so it is deliberately here
# and not in the offline pass.
# `--to` is not optional here even though the flag is. With no recipient the
# faucet claims into the wallet's default faucet recipient - the lowest
# signing-capable, auth-transfer-owned account id - which on a bootstrapped
# wallet is NOT $LPAD_CREATOR. `native_balance` below reads $LPAD_CREATOR, so an
# unaddressed claim tops up one account and is measured on another: the balance
# never moves, and WRAP_AMT / LEZ_PRIV_AMT get sized off a stale figure.
chk "faucet" "$L" "${NET[@]}" faucet --to "$LPAD_CREATOR"
wait_block
NATIVE=$(native_balance)
echo "   creator native balance after faucet = $NATIVE"

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
# The window must outlast INCLUSION of the create plus inclusion of the buy: the
# CLI waits for the create to land before it submits the buy, so the buy is in a
# strictly later block - at least one ~46-50s block of chain time after the create.
# A 60s window could therefore never admit its own buy. The guest asserts
# `now < end_timestamp_ms`, panics, the sequencer DROPS the tx, and the wallet
# then polls its whole 30-minute budget before reporting a timeout that looks
# nothing like the clock problem it is. Ten minutes is ~12 blocks of room.
BC_END=$(( $(date +%s%3N) + 600000 ))
chk "bc create-sale" "$L" "${NET[@]}" bc create-sale --program "$LPAD_BC_PROGRAM_ID" --collateral-def "$LPAD_COLL_DEF" --treasury "$LPAD_TREASURY" --creator-token-holding "$LPAD_PROJ_HOLD" --creator "$LPAD_CREATOR" --sale-quantity 100000 --dex-seed 100 --vt 2000000 --vc 50000 --fee-bps 100 --end-ts "$BC_END" --nonce "$N7"
BC7=$(sale_id "$N7")
chk "bc buy (close-test)" "$L" "${NET[@]}" bc buy --program "$LPAD_BC_PROGRAM_ID" --sale "$BC7" --buyer-collateral "$LPAD_BUYER_COLL" --buyer-token "$LPAD_BUYER_TOK" --in 100 --min-out 0
wait_chain_past "$BC_END" \
  "$L" "${NET[@]}" bc buy --program "$LPAD_BC_PROGRAM_ID" --sale "$LPAD_SALE_ID" --buyer-collateral "$LPAD_BUYER_COLL" --buyer-token "$LPAD_BUYER_TOK" --in 100 --min-out 0
chk "bc close"    "$L" "${NET[@]}" bc close    --program "$LPAD_BC_PROGRAM_ID" --sale "$BC7" --creator "$LPAD_CREATOR"
chk "bc withdraw" "$L" "${NET[@]}" bc withdraw --program "$LPAD_BC_PROGRAM_ID" --sale "$BC7" --creator-collateral "$LPAD_COLL_HOLD" --creator-token "$LPAD_PROJ_HOLD" --creator "$LPAD_CREATOR"

# LBP: this pool has to RAISE something before it closes. The at-close fee that
# `lbp sweep-treasury` settles is charged on `cum_collateral_in`, so a pool that
# never sold a token escrows nothing and the sweep would have nothing to prove -
# which is how settlement could break unnoticed. So it needs a live window (the
# old `--t-end 1`, already ended, admitted no buys at all): start it a minute in
# the past, in case the chain clock trails the local one, and end it a minute
# ahead. --fee-bps is what makes the fee non-zero; close_fee rounds UP, so even a
# small raise leaves something to settle.
# Same arithmetic as the bonding curve above: the buy lands a block after the
# create, so a 120s window (60s of it already in the past) cannot admit it. The
# failure is worse here because it cascades - no buy means `cum_collateral_in`
# stays 0, so `lbp withdraw` escrows no fee and `lbp sweep-treasury` has nothing
# to settle and is reported as a skip. One clock bug, one failure and one skip.
LBP_TS=$(( $(date +%s%3N) - 60000 )); LBP_TE=$(( LBP_TS + 720000 ))
chk "lbp create-sale" "$L" "${NET[@]}" lbp create-sale --program "$LPAD_LBP_PROGRAM_ID" --collateral-def "$LPAD_COLL_DEF" --treasury "$LPAD_TREASURY" --creator-token-holding "$LPAD_PROJ_HOLD" --creator-collateral-holding "$LPAD_COLL_HOLD" --creator "$LPAD_CREATOR" --token-deposit 1000 --collateral-seed 100 --w-start 0.9 --w-end 0.1 --t-start "$LBP_TS" --t-end "$LBP_TE" --fee-bps 100 --nonce "$N7"
LBP7=$(pool_id "$N7")
chk "lbp buy (fee-test)" "$L" "${NET[@]}" lbp buy --program "$LPAD_LBP_PROGRAM_ID" --pool "$LBP7" --buyer-collateral "$LPAD_BUYER_COLL" --buyer-token "$LPAD_BUYER_TOK" --in 100 --min-out 0
# Same shape as the bonding-curve close above: wait the window out locally, then
# push a transaction through the long-running bootstrap pool so a fresh block
# carries the clock past t_end (an idle chain's clock does not move on its own).
wait_chain_past "$LBP_TE" \
  "$L" "${NET[@]}" lbp poke --program "$LPAD_LBP_PROGRAM_ID" --pool "$LPAD_LBP_POOL_ID"
chk "lbp close"    "$L" "${NET[@]}" lbp close    --program "$LPAD_LBP_PROGRAM_ID" --pool "$LBP7" --creator "$LPAD_CREATOR"
chk "lbp withdraw" "$L" "${NET[@]}" lbp withdraw --program "$LPAD_LBP_PROGRAM_ID" --pool "$LBP7" --creator-collateral "$LPAD_COLL_HOLD" --creator-token "$LPAD_PROJ_HOLD" --creator "$LPAD_CREATOR"
# `lbp withdraw` pays the creator and ESCROWS the fee; this is the only
# instruction that hands that escrow to the treasury. Gated on the same read as
# the curve's sweep, so a withdrawal that never landed is reported as a skip with
# its reason rather than as a second, derived failure.
LBP_OWED=$(owed lbp pool-info --pool "$LBP7" --at "$(date +%s%3N)")
if [ "${LBP_OWED:-0}" = "0" ]; then
  skip "lbp sweep-treasury" "pool owes the treasury nothing (the withdrawal above is what escrows the fee)"
else
  chk "lbp sweep-treasury" "$L" "${NET[@]}" lbp sweep-treasury --program "$LPAD_LBP_PROGRAM_ID" --pool "$LBP7"
fi

echo "## ONLINE - allowlist-gated LBP (single-member tree: root == leaf(buyer), empty proof)"
GN="$(( N7 + 1 ))"   # numeric, distinct from the lifecycle LBP sale's nonce (N7)
GROOT=$("$L" lbp allowlist-leaf --account "$LPAD_BUYER_COLL" --json | python3 -c 'import json,sys;print(json.load(sys.stdin)["leaf"])')
GTS=$(date +%s%3N); GTE=$(( GTS + 31536000000 ))   # now .. now+1yr (under the ~10yr cap)
chk "lbp create-sale (gated)" "$L" "${NET[@]}" lbp create-sale --program "$LPAD_LBP_PROGRAM_ID" --collateral-def "$LPAD_COLL_DEF" --treasury "$LPAD_TREASURY" --creator-token-holding "$LPAD_PROJ_HOLD" --creator-collateral-holding "$LPAD_COLL_HOLD" --creator "$LPAD_CREATOR" --token-deposit 1000 --collateral-seed 100 --w-start 0.9 --w-end 0.1 --t-start "$GTS" --t-end "$GTE" --allowlist-root "$GROOT" --nonce "$GN"
GPOOL=$(pool_id "$GN")
chk "lbp buy-gated" "$L" "${NET[@]}" lbp buy-gated --program "$LPAD_LBP_PROGRAM_ID" --pool "$GPOOL" --buyer-collateral "$LPAD_BUYER_COLL" --buyer-token "$LPAD_BUYER_TOK" --in 50 --min-out 0

echo
# Coverage is ASSERTED, not claimed. Every command the binary advertises must have
# been chk'd or explicitly skipped; a command nobody wired up used to read as full
# coverage, because the only check was a count in this header that a human kept in
# sync by hand. `lpad faucet` shipped and this sweep reported 49-of-50 as complete.
ALL_CMDS=$( { "$L" --help 2>/dev/null | awk '/^Commands:/{f=1;next} /^$/{f=0} f&&$1!~/^-/{print $1}'
              for g in bc lbp; do "$L" $g --help 2>/dev/null \
                | awk -v g="$g" '/^Commands:/{f=1;next} /^$/{f=0} f&&$1!~/^-/{print g" "$1}'; done
            } | grep -v ' help$' | grep -vx 'help' \
              | grep -vx 'bc' | grep -vx 'lbp' | sort -u )   # `bc`/`lbp` are GROUPS,
              # not commands - their leaves are enumerated above and counting the
              # group name too would demand a check for something unrunnable.
# `program-id` is exercised four times, once per program (`program-id bc`, ...),
# and `lpad --help` advertises the single leaf `program-id`. Without collapsing
# the family the comm below reports `program-id` as NOT EXERCISED, adds a
# synthetic failure and exits 1 - so every one of the 51 commands could pass and
# the sweep would still report a failure. That is exactly the false negative this
# assertion exists to prevent, pointed at itself.
COVERED_NORM=$(printf '%s\n' "${COVERED[@]:-}" \
  | sed 's/ --.*//' | sed 's/^program-id .*/program-id/' | sed 's/[[:space:]]*$//' | sort -u)
MISSING=$(comm -23 <(printf '%s\n' "$ALL_CMDS") <(printf '%s\n' "$COVERED_NORM"))

echo "=== $PASS passed, $FAIL failed, $SKIP skipped ==="
if [ -n "$MISSING" ]; then
  echo "!! NOT EXERCISED by this sweep - coverage is incomplete:"
  printf '     %s\n' $MISSING
  echo "   Wire each one up, or skip() it with a reason. A command that is merely"
  echo "   absent reads as a pass, which is how this file lied once already."
  FAIL=$((FAIL+1)); FAILED+=("coverage: $(printf '%s ' $MISSING)")
fi
# Skips are listed explicitly: with LPAD_SKIP_PRIVATE=1 a green run is NOT full
# coverage, and the follow-up private pass still owes these.
if [ "$SKIP" -gt 0 ]; then printf 'SKIPPED: %s\n' "${SKIPPED[@]}"; fi
if [ "$FAIL" -gt 0 ]; then printf 'FAILED: %s\n' "${FAILED[@]}"; exit 1; fi
