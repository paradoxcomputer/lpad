#!/usr/bin/env bash
# Bootstrap an LPAD end-to-end run against a REAL LEZ sequencer.
#
# There is no bundled local sequencer any more, and lpad ships none: it targets the
# Logos testnet, the Paradox Computer testnet, or any other real sequencer you point
# it at - including a LEZ node you install and run yourself, which the CLI calls
# `local` and which this script takes as its plain http(s) URL. The two public ones
# run a LEZ build whose built-in program image ids match v0.2.4, which is what the
# crates are pinned to - see the `chain_parity` tests in sdk/src/lib.rs, which assert
# that equality. A node of your own must be v0.2.4 for the same reason.
#
# What it does: configures a wallet against the chosen network, self-funds a
# creator from the pinata faucet (or an explicit funder key), mints a project +
# collateral token, deploys lpad's three programs, creates a bonding-curve sale
# and an LBP pool, and funds a public buyer plus shielded holdings for the private
# path. Emits scripts/bootstrap.env.
#
# Because the chain is real, PROOFS ARE REAL - a real sequencer verifies them, so
# RISC0_DEV_MODE is not usable here and every private op costs a full STARK
# (minutes). Dev-mode proving only applies to the in-process integration tests.
#
# Env:
#   LPAD_NETWORK         testnet (default) | paradox | an http(s) sequencer URL
#   LPAD_SEQUENCER_ADDR  explicit URL; overrides LPAD_NETWORK
#   LPAD_WALLET_HOME     default ~/.lpad
#   LPAD_WALLET_PW       default lpaddev
#   LPAD_FUNDER          existing funded account ("Public/..."); skips the faucet
#   LPAD_FUNDER_KEY      private key to import for LPAD_FUNDER. NOTE: the LEZ
#                        wallet only accepts this as `--private-key <hex>`, i.e.
#                        on argv, where it is readable in the process table by any
#                        local user for the length of that call. The wallet
#                        password goes over stdin, but there is no equivalent for
#                        the key. Use a testnet-only key here; the default path
#                        (the pinata faucet) needs no key at all.
set -euo pipefail
export PATH="$HOME/.cargo/bin:$HOME/.risc0/bin:$PATH"

REPO="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
LEZ="${LPAD_LEZ_DIR:-$HOME/lez}"
WALLET="${LPAD_WALLET_BIN:-$LEZ/target/release/wallet}"
LPAD="${LPAD_BIN:-$REPO/cli/target/release/lpad}"
PROG="$REPO/programs"
HOME_DIR="${LPAD_WALLET_HOME:-$HOME/.lpad}"
PW="${LPAD_WALLET_PW:-lpaddev}"
OUT="${LPAD_BOOTSTRAP_OUT:-$REPO/scripts/bootstrap.env}"

# Network aliases. `lpad_sdk::Network` additionally has `local` / `local=<url>` for
# a sequencer the user runs; there is deliberately no alias for it HERE, because an
# alias would also have to name a wallet home and bootstrap has none for it - give
# the URL instead, which the http(s) arm below already accepts, and set
# LPAD_WALLET_HOME so a local run does not land in testnet's ~/.lpad.
NETWORK="${LPAD_NETWORK:-testnet}"
case "$NETWORK" in
  testnet|logos) SEQ_ADDR="https://testnet.lez.logos.co" ;;
  paradox)       SEQ_ADDR="https://seq-testnet.paradox.computer" ;;
  http://*|https://*) SEQ_ADDR="$NETWORK" ;;
  *) echo "unknown LPAD_NETWORK '$NETWORK' - use testnet, paradox, or an http(s) URL" >&2; exit 2 ;;
esac
SEQ_ADDR="${LPAD_SEQUENCER_ADDR:-$SEQ_ADDR}"

# Funding: the pinata program is the testnet faucet. An explicit funded account
# takes precedence (a chain may not run pinata, or may have drained it).
FUNDER="${LPAD_FUNDER:-}"
FUNDER_KEY="${LPAD_FUNDER_KEY:-}"

# sale parameters
D=1000000; R=200000; VT=2000000; VC=50000; FEE_BPS=100; NONCE=0
PROJ_SUPPLY=10000000; COLL_SUPPLY=10000000
BUYER_FUND=100000; PRIV_FUND=50000; INIT_DUST=1

# bootstrap is the ONE part of lpad that needs a LEZ checkout. Everything else
# builds off the published git tag; this script drives the upstream `wallet`
# binary (key management, deploys, transfers) and seeds its config from the
# checkout, so both must exist. Say so concretely - "wallet CLI not built" alone
# sent people looking for an lpad target that was never going to appear.
#
# Deploying is no longer what makes that true, and the claim above is narrower than
# it used to be: `lpad network` deploys the three guests through the SDK, with no
# wallet binary and no checkout, so putting lpad on a chain of your own needs
# neither. This script keeps driving the wallet binary because everything ELSE it
# does has no lpad equivalent - creating keypairs, minting tokens, claiming the
# pinata faucet, sending native LEZ - and because it must run unattended, while the
# CLI's deploy sits behind an interactive confirmation. The wallet_config.json
# template below lives only in the checkout too. So: unchanged for bootstrap, gone
# for anyone who only wants the programs deployed.
if [ ! -x "$WALLET" ]; then
  echo "✗ LEZ wallet binary not found: $WALLET" >&2
  echo "  scripts/bootstrap.sh drives the upstream wallet; building lpad does not produce it." >&2
  echo "  Clone LEZ at the tag lpad pins and build it:" >&2
  echo "    git clone --branch v0.2.4 https://github.com/logos-blockchain/logos-execution-zone.git ~/lez" >&2
  echo "    (cd ~/lez && cargo build --release -p wallet)" >&2
  echo "  Then re-run, or set LPAD_LEZ_DIR / LPAD_WALLET_BIN." >&2
  exit 1
fi
[ -x "$LPAD" ]   || { echo "lpad CLI not built: $LPAD (run: cd cli && cargo build --release)" >&2; exit 1; }
# checkHealth over JSON-RPC: works for https (no explicit port) and actually
# proves the sequencer is serving, not just that a socket is open.
timeout 30 curl -sf -X POST "$SEQ_ADDR" -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"checkHealth","params":[]}' >/dev/null \
  || { echo "sequencer not reachable / unhealthy at $SEQ_ADDR" >&2; exit 1; }

export LEE_WALLET_HOME_DIR="$HOME_DIR"
mkdir -p "$HOME_DIR"
# Separate from the binary check above: LPAD_WALLET_BIN can relocate the wallet
# without there being a checkout at all, and this template only lives in one.
WCFG="$LEZ/lez/wallet/configs/debug/wallet_config.json"
[ -f "$WCFG" ] || {
  echo "✗ LEZ wallet config template not found: $WCFG" >&2
  echo "  bootstrap seeds its wallet config from the LEZ checkout, then rewrites the" >&2
  echo "  sequencer list. Point LPAD_LEZ_DIR at a v0.2.4 checkout." >&2
  exit 1
}
cp -f "$WCFG" "$HOME_DIR/wallet_config.json"
python3 - "$HOME_DIR/wallet_config.json" "$SEQ_ADDR" <<'PY'
import json,sys
p,addr=sys.argv[1],sys.argv[2]
d=json.load(open(p))
# v0.2.4 schema: a `sequencers` array (v0.2.0 had a single `sequencer_addr`).
# `initial_accounts` was an rc4 field.
d.pop("initial_accounts", None)
# v0.2.4 replaced the single `sequencer_addr` with a list.
d.pop("sequencer_addr", None)
d["sequencers"] = [{"sequencer_addr": addr}]
# Real networks make blocks slower than a local dev chain did, and a round trip
# is a WAN hop - give polling room.
# Budget = seq_poll_max_retries x seq_poll_timeout. Blocks are ~46-50s on the
# public sequencers, so the old 10 x 20s gave under 4 blocks of patience and a
# slow inclusion was indistinguishable from a rejection. 60 x 30s ~= 30 min.
# Budget per tx = seq_tx_poll_max_blocks x seq_poll_timeout = 30 x 10s = 5 min,
# ~6 blocks. Polling returns as soon as the tx is seen (~1 block), so this is
# only ever spent on something that did NOT land - and spending 30 minutes on
# that, per transaction, was the biggest source of dead time in a real run.
# seq_poll_max_retries bounds an INNER loop with no sleep between calls (60 meant
# 61 RPC round trips per round), not the wall clock.
d["seq_poll_timeout"] = "10s"
d["seq_tx_poll_max_blocks"] = 30
d["seq_poll_max_retries"] = 2
# On open the wallet probes every sequencer absent from statistics.json this many
# times (default 100). The CLI opens a fresh wallet per command, and over a WAN
# hop that default is punishing.
d.setdefault("multi_sequencer_client_config", {})
d["multi_sequencer_client_config"]["distribution_limit"] = 1
d["multi_sequencer_client_config"]["calibration_limit"] = 3
json.dump(d,open(p,"w"),indent=4)
PY
# A real sequencer verifies proofs, so dev-mode proving can never work against
# one. Record `real` unconditionally and refuse a misleading RISC0_DEV_MODE.
if [ "${RISC0_DEV_MODE:-}" = "1" ]; then
  echo "✗ RISC0_DEV_MODE=1 is set, but $SEQ_ADDR is a real sequencer and verifies proofs." >&2
  echo "  Unset it and re-run; private ops will take minutes each." >&2
  exit 2
fi
echo real > "$HOME_DIR/proof_mode"
echo ">> network: $NETWORK ($SEQ_ADDR)   wallet home: $HOME_DIR   proofs: REAL"

w() { printf '%s\n' "$PW" | "$WALLET" "$@"; }

# Current sequencer height.
# MUST NOT fail. `set -euo pipefail` is in force and this is a `curl | python3`
# pipeline, so any transient RPC hiccup - a 502 page, a truncated body, a timeout -
# makes python raise, which fails the pipeline. Every caller writes
# `h=$(seq_height)`, and a command substitution that exits non-zero fails the
# ASSIGNMENT, which `set -e` turns into an immediate abort of the whole bootstrap.
# Both streams are redirected to /dev/null, so there is not even an error message:
# the run simply stops with exit 1 in the middle, having already spent 10+ minutes
# and a faucet claim. That silently killed two consecutive paradox bootstraps
# immediately after a successful transfer, and cost a traced re-run to find.
# Callers all already handle an empty answer (`[ -n "$h" ]`), so degrade to empty
# rather than exploding.
seq_height() {
  timeout 60 curl -s -X POST "$SEQ_ADDR" -H 'content-type: application/json' \
    -d '{"jsonrpc":"2.0","id":1,"method":"getLastBlockId","params":[]}' 2>/dev/null \
    | python3 -c 'import sys,json; print(json.load(sys.stdin)["result"])' 2>/dev/null \
    || true
}

# Block until the chain advances at least one block, so the next step sees the
# previous transaction's state.
#
# This replaces the fixed `wait_block/16/20` this script used against a local dev
# chain. The public sequencers produce a block roughly every 45-50s, so every one
# of those sleeps was SHORTER than a single block - dependent steps would read
# pre-transaction state. Waiting on real height is also self-tuning if the
# operators change the cadence.
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
new_pub() { w account new public 2>&1 | grep -oE 'Public/[1-9A-HJ-NP-Za-km-z]{32,44}' | head -1; }
new_priv() { w account new private --label "$1" 2>&1 | grep -oE 'Private/[1-9A-HJ-NP-Za-km-z]{32,44}' | head -1; }
lpad() { "$LPAD" --config "$HOME_DIR/wallet_config.json" --storage "$HOME_DIR/storage.json" "$@"; }

echo ">> creating accounts"
CREATOR=$(new_pub);   echo "   creator      = $CREATOR"
PROJ_DEF=$(new_pub);  PROJ_HOLD=$(new_pub)
COLL_DEF=$(new_pub);  COLL_HOLD=$(new_pub)
# NB: treasury/buyer holdings are created later, by `lpad init-holding`, once the
# token definitions exist - a plain `account new public` id cannot receive a token
# transfer (see the funding step below).

# v0.2.4 requires an account to be initialized under the authenticated-transfer
# program before it can RECEIVE native tokens (the pinata claim and any
# auth-transfer send both refuse an uninitialized recipient).
#
# This also happens to be what keeps the creator clear of `validate_execution`
# rule 7: initializing sets `program_owner` to the authenticated-transfer program,
# so echoing the creator in a program's post-states is legal. A never-initialized
# keypair that has merely signed is DEFAULT-owned with a bumped nonce, which rule
# 7 rejects.
echo ">> initializing the creator under authenticated-transfer"
w auth-transfer init --account-id "$CREATOR" >&2 2>&1 \
  || echo "   (already initialized)" >&2
wait_block

# On a chain we do not own there is no genesis key to import. Either the caller
# supplies a funded account, or we self-fund the creator from the pinata faucet.
if [ -n "$FUNDER" ]; then
  echo ">> funding creator from the supplied funder $FUNDER"
  [ -n "$FUNDER_KEY" ] && { w account import public --private-key "$FUNDER_KEY" >&2 2>&1 || echo "   (already imported)" >&2; sleep 8; }
  w auth-transfer send --from "$FUNDER" --to "$CREATOR" --amount 8000 >&2 \
    || { echo "✗ transfer from $FUNDER failed - is it funded?" >&2; exit 1; }
else
  echo ">> claiming the pinata faucet into the creator (no LPAD_FUNDER given)"
  # Retried, because the common failure here is not a broken chain: the pinata
  # re-seeds its challenge on every successful claim, so a solution mined while
  # somebody else claimed first scores against a challenge that no longer exists.
  # Nothing is consumed and there is no cooldown - the answer is simply to mine
  # again. Unretried, that lost race aborts a bootstrap that is otherwise fine,
  # and on a fresh wallet it aborts it before anything else has been built.
  claimed=0
  for attempt in 1 2 3 4 5; do
    if w pinata claim --to "$CREATOR" >&2 2>&1; then claimed=1; break; fi
    echo "   pinata claim attempt $attempt failed - re-mining against the new challenge" >&2
    sleep 15
  done
  [ "$claimed" = "1" ] \
    || { echo "✗ pinata claim failed 5 times. This chain may not run pinata, or it is drained." >&2
         echo "  Supply a funded account instead: LPAD_FUNDER=Public/... LPAD_FUNDER_KEY=<hex>" >&2
         exit 1; }
fi
wait_block

echo ">> minting PROJECT (supply $PROJ_SUPPLY) + COLLATERAL (supply $COLL_SUPPLY)"
w token new --name PROJECT --total-supply "$PROJ_SUPPLY" \
  --definition-account-id "$PROJ_DEF" --supply-account-id "$PROJ_HOLD" >&2
wait_block
w token new --name COLLAT --total-supply "$COLL_SUPPLY" \
  --definition-account-id "$COLL_DEF" --supply-account-id "$COLL_HOLD" >&2
wait_block

if [ "${LPAD_SKIP_DEPLOY:-0}" = "1" ]; then echo ">> skipping deploys (LPAD_SKIP_DEPLOY=1; programs already on chain)"; fi

# Deploy a guest and PROVE the bytes are on chain.
#
# `deploy-program` exiting 0 is not proof, and neither is the block id it prints:
#
#   - The deploy tx hash is derived from the ELF alone, so re-deploying an
#     unchanged guest resolves `getTransaction` to the ORIGINAL deploy and
#     reports that old block. wlez re-deployed in 5s "into block 1259" while the
#     chain head was 1408.
#   - A deploy that never lands surfaces only as the generic "All pollers
#     failed", indistinguishable from a slow chain. This used to be tolerated
#     with the rationale that create-sale would "fail loudly with 'Unknown
#     program'". It does not: a tx against a missing program is dropped from the
#     mempool and also reports "All pollers failed". lbp reported inclusion in
#     block 1161, was never actually deployed, and every later `lbp create-sale`
#     was silently discarded - which read as poll flakiness for a whole session.
#
# So check the one thing that cannot lie: are the ELF's own bytes in a block?
#
# Two-stage, because a re-deploy legitimately may never report an inclusion at
# all. The deploy tx hash is a pure function of the bytecode, so a duplicate is
# not re-included; once the ORIGINAL inclusion ages out of `getTransaction`'s
# bounded lookback, the wallet polls to exhaustion and reports nothing - even
# though the program is deployed and working. (Live example: after the Logos
# testnet reset kept its block store but wiped account state, block 1138 still
# held bonding_curve, so re-deploying it could only ever time out.) Failing there
# would be a false alarm, and it would make this stricter than the tolerant code
# it replaced - trading a silent miss for a spurious abort.
#
#   stage 1  the block the wallet named (cheap, the common case)
#   stage 2  scan back from head for the bytes (only if stage 1 found nothing)
#
# Stage 2's range is bounded and always reported, so a "deployed" verdict can
# never come from a scan that quietly gave up early.
#
# The SDK's own deploy path (`LaunchpadClient::deploy_program`, which `lpad network`
# drives) reuses this discipline rather than reinventing it: same prior check, same
# bytes-in-a-block verification, same bounded-and-reported fallback scan, and the
# same LPAD_DEPLOY_SCAN_BLOCKS knob. Keep the two in step - a silent non-deploy is
# the failure this project has actually paid for, twice.
# Which guests are ALREADY on chain, as "<name> <block>" lines. Filled by
# prescan_deployed(); consulted by deploy_verified() to skip a pointless deploy.
DEPLOY_PRESCAN=""

# Scan the recent chain ONCE for all three guests before deploying anything.
#
# Without this the order is deploy-then-verify, and re-deploying a guest that is
# already on chain is the worst case: the duplicate is never re-included, so the
# wallet polls its entire budget (seq_poll_max_retries x seq_poll_timeout = 30 min
# here) to prove nothing, per program. Measured: testnet's bonding_curve sat at
# block 1138 after a reset kept the block store, and re-deploying it burned 30 min
# before the fallback scan found it.
#
# One pass fetches each block once and tests all three probes together - 400 reads
# instead of 1200 - so knowing up front is cheaper than one futile poll.
prescan_deployed() {
  [ "${LPAD_SKIP_DEPLOY:-0}" = "1" ] && return 0
  echo ">> pre-scanning the chain for already-deployed guests" >&2
  DEPLOY_PRESCAN=$(python3 - "$SEQ_ADDR" "${LPAD_DEPLOY_SCAN_BLOCKS:-400}" "$PROG/artifacts/lpad" <<'PY'
import base64, json, os, sys, urllib.request
seq, span, artdir = sys.argv[1], int(sys.argv[2]), sys.argv[3]

def rpc(method, params):
    req = urllib.request.Request(
        seq, json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode(),
        {"content-type": "application/json"})
    return json.load(urllib.request.urlopen(req, timeout=120)).get("result")

probes = {}
for n in ("bonding_curve", "lbp", "wlez"):
    p = os.path.join(artdir, f"{n}.bin")
    if os.path.exists(p):
        elf = open(p, "rb").read()
        probes[n] = elf[len(elf) // 2 : len(elf) // 2 + 64]

try:
    head = rpc("getLastBlockId", [])
except Exception:
    head = None
if not isinstance(head, int):
    # Unknown head: say nothing, so every guest is treated as not-yet-deployed and
    # the normal deploy+verify path runs. Never guess "deployed".
    print("# head unreadable - no prescan", file=sys.stderr)
    sys.exit(0)

low = max(0, head - span)
found = {}
for b in range(head, low - 1, -1):
    if len(found) == len(probes):
        break
    try:
        r = rpc("getBlock", [b])
    except Exception:
        continue
    if not r:
        continue
    raw = base64.b64decode(r)
    if b"\x7fELF" not in raw:      # cheap reject: only deploy blocks carry an ELF
        continue
    for n, pr in probes.items():
        if n not in found and pr in raw:
            found[n] = b
print(f"# scanned blocks {low}..{head}: "
      + (", ".join(f"{n}@{b}" for n, b in sorted(found.items())) or "none of ours"),
      file=sys.stderr)
for n, b in found.items():
    print(f"{n} {b}")
PY
) || DEPLOY_PRESCAN=""
}

deploy_verified() {
  local bin="$1" name="$2" out blk pre
  [ -f "$bin" ] || { echo "missing $bin (build: bash scripts/build-guests.sh)" >&2; return 1; }
  if [ "${LPAD_SKIP_DEPLOY:-0}" = "1" ]; then echo "   (skipping $name deploy)"; return 0; fi
  # Already on chain? Then a deploy can only time out - skip straight to verified.
  pre=$(printf '%s\n' "$DEPLOY_PRESCAN" | awk -v n="$name" '$1 == n { print $2 }' | head -1)
  if [ -n "$pre" ]; then
    echo "   ✓ $name already on chain in block $pre - deploy skipped (a duplicate is never re-included)"
    return 0
  fi
  out=$(w deploy-program "$bin" 2>&1) || true
  printf '%s\n' "$out" >&2
  blk=$(printf '%s\n' "$out" | sed -n 's/.*included in block \([0-9]\{1,\}\).*/\1/p' | tail -1)
  python3 - "$SEQ_ADDR" "${blk:-0}" "$bin" "$name" "${LPAD_DEPLOY_SCAN_BLOCKS:-400}" <<'PY'
import base64, json, sys, urllib.request
seq, blk, bin_path, name, span = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4], int(sys.argv[5])
elf = open(bin_path, "rb").read()
# A chunk from deep inside the ELF: distinctive enough to identify this guest
# (two unrelated guests can share a size - testnet 1107 and paradox 210 both hold
# a 447135-byte non-lpad ELF close to wlez.bin's 447820), and far cheaper than
# comparing the whole 0.5 MB payload.
probe = elf[len(elf) // 2 : len(elf) // 2 + 64]

def rpc(method, params):
    req = urllib.request.Request(
        seq, json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode(),
        {"content-type": "application/json"})
    return json.load(urllib.request.urlopen(req, timeout=120)).get("result")

def has_elf(b):
    try:
        r = rpc("getBlock", [b])
    except Exception:
        return False
    return bool(r) and probe in base64.b64decode(r)

if blk and has_elf(blk):
    print(f"   ✓ {name} verified on chain in block {blk} ({len(elf)} bytes)")
    sys.exit(0)

why = (f"the wallet named block {blk}, but its bytes are not there"
       if blk else "the deploy never reported an inclusion block")
head = rpc("getLastBlockId", [])
if not isinstance(head, int):
    sys.exit(f"✗ {name}: {why}, and the head is unreadable - cannot verify")
low = max(0, head - span)
print(f"   .. {why}; scanning blocks {low}..{head} for {name}'s bytes")
for b in range(head, low - 1, -1):
    if has_elf(b):
        print(f"   ✓ {name} already on chain in block {b} ({len(elf)} bytes) - duplicate deploy "
              f"was correctly not re-included")
        sys.exit(0)
sys.exit(
    f"✗ {name}: {why}, and its bytes are in NO block in {low}..{head} "
    f"({span} scanned).\n"
    f"  Treat it as NOT deployed: every transaction against this program will be\n"
    f"  silently dropped from the mempool and read as a timeout. Re-run the deploy.\n"
    f"  (If it was deployed longer ago than {span} blocks, raise LPAD_DEPLOY_SCAN_BLOCKS.)"
)
PY
}

prescan_deployed

echo ">> deploying bonding_curve program (own block)"
BC_BIN="$PROG/artifacts/lpad/bonding_curve.bin"
deploy_verified "$BC_BIN" bonding_curve || exit 1
wait_block
BC_ID=$(lpad program-id bc --json | python3 -c 'import json,sys;print(json.load(sys.stdin)["program_id"])')
echo "   bonding_curve program id = $BC_ID"

# A token transfer to a never-initialised holding is REJECTED (silently dropped
# from the mempool, which surfaces only as "All pollers failed"): the token
# program claims the recipient with `Claim::Authorized`, and the runtime grants
# that only for an account the transaction signed for - the wallet does not
# co-sign transfer recipients. The LEZ wallet has no token-account-init command,
# so use lpad's, which creates AND initialises the holding and prints its id.
echo ">> creating initialised token holdings (treasury, buyer collateral, buyer token)"
init_holding() {
  lpad init-holding --token-def "$1" --json \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["account"])'
}
TREASURY=$(init_holding "$COLL_DEF");    echo "   treasury     = $TREASURY";   wait_block
BUYER_COLL=$(init_holding "$COLL_DEF");  echo "   buyer collat = $BUYER_COLL"; wait_block
BUYER_TOK=$(init_holding "$PROJ_DEF");   echo "   buyer token  = $BUYER_TOK";  wait_block
for v in TREASURY BUYER_COLL BUYER_TOK; do
  [ -n "${!v}" ] || { echo "✗ failed to initialise $v holding" >&2; exit 1; }
done

echo ">> funding treasury + public buyer holdings"

# Send, then PROVE the recipient's balance is there.
#
# These used to be `|| true; wait_block`, which silently tolerated a lost
# transfer. That is much worse than it looks: an unfunded buyer holding does not
# fail at the point of the transfer, it fails much later inside `bc buy`, where
# the program panics on insufficient balance, the sequencer drops the tx, and the
# CLI reports the usual indistinguishable "All pollers failed". Observed exactly
# once on paradox: of these three sends the two 1-unit dust transfers landed and
# the 100000 buyer-collateral one vanished, so BUYER_COLL sat at 0 and every buy
# in the sweep hung for ~15 min each against a chain that was perfectly healthy.
#
# `|| true` is still right for the SEND (a duplicate/late inclusion is not fatal),
# but the balance check afterwards is what turns a lost transfer into a loud,
# immediate failure instead of a mystery 40 minutes downstream.
fund_verified() {
  local from="$1" to="$2" amount="$3" label="$4" got
  w token send --from "$from" --to "$to" --amount "$amount" >&2 || true
  wait_block
  got=$(python3 - "$SEQ_ADDR" "$to" <<'PY'
import json, sys, urllib.request
addr, acct = sys.argv[1], sys.argv[2].split("/", 1)[-1]
try:
    req = urllib.request.Request(
        addr, json.dumps({"jsonrpc": "2.0", "id": 1, "method": "getAccount", "params": [acct]}).encode(),
        {"content-type": "application/json"})
    d = bytes((json.load(urllib.request.urlopen(req, timeout=60)).get("result") or {}).get("data") or b"")
    # TokenHolding::Fungible == [1-byte borsh tag][32-byte definition id][u128 LE balance]
    print(int.from_bytes(d[33:49], "little") if len(d) >= 49 else -1)
except Exception:
    print(-1)
PY
)
  if [ "${got:--1}" -lt "$amount" ]; then
    echo "✗ $label: expected >= $amount in $to after funding, chain says ${got}." >&2
    echo "  The transfer was lost. Left unchecked this surfaces much later as a hung" >&2
    echo "  'bc buy'/'lbp buy' (the program panics on insufficient balance, the tx is" >&2
    echo "  dropped, and the CLI only ever reports 'All pollers failed')." >&2
    return 1
  fi
  echo "   ✓ $label funded ($got in $to)" >&2
}
fund_verified "$COLL_HOLD" "$TREASURY"   "$INIT_DUST"  "treasury"         || exit 1
fund_verified "$COLL_HOLD" "$BUYER_COLL" "$BUYER_FUND" "buyer collateral" || exit 1
fund_verified "$PROJ_HOLD" "$BUYER_TOK"  "$INIT_DUST"  "buyer token"      || exit 1

if [ "${LPAD_SKIP_PRIVATE:-0}" = "1" ]; then
  echo ">> skipping shielded holdings (LPAD_SKIP_PRIVATE=1)"
  echo "   the private buy/sell paths will have nothing to spend; everything public is unaffected"
  PRIV_COLL=""; PRIV_PROJ=""
else
PRIV_COLL=$(new_priv "lpad-priv-coll-$$")
PRIV_PROJ=$(new_priv "lpad-priv-proj-$$")
echo "   priv collateral = $PRIV_COLL"
echo "   priv project    = $PRIV_PROJ"
w token send --from "$COLL_HOLD" --to "$PRIV_COLL" --amount "$PRIV_FUND" >&2 || true; wait_block
w token send --from "$PROJ_HOLD" --to "$PRIV_PROJ" --amount "$INIT_DUST" >&2 || true; wait_block
w account sync-private >/dev/null 2>&1 || true
fi

echo ">> creating bonding-curve sale (D=$D R=$R Vt=$VT Vc=$VC fee=${FEE_BPS}bps)"
lpad bc create-sale --program "$BC_ID" \
  --collateral-def "$COLL_DEF" --treasury "$TREASURY" \
  --creator-token-holding "$PROJ_HOLD" --creator "$CREATOR" \
  --sale-quantity "$D" --dex-seed "$R" --vt "$VT" --vc "$VC" \
  --fee-bps "$FEE_BPS" --nonce "$NONCE" >&2
wait_block

SALE_ID=$(lpad bc ids --program "$BC_ID" --token-def "$PROJ_DEF" \
  --collateral-def "$COLL_DEF" --creator "$CREATOR" --nonce "$NONCE" --json \
  | python3 -c 'import json,sys;print(json.load(sys.stdin)["sale"])')
echo "   sale id = $SALE_ID"

# --- LBP (RFP-016): deploy the program + create a time-driven sale ---------
echo ">> deploying lbp program (own block)"
LBP_BIN="$PROG/artifacts/lpad/lbp.bin"
deploy_verified "$LBP_BIN" lbp || exit 1
wait_block
LBP_ID=$(lpad program-id lbp --json | python3 -c 'import json,sys;print(json.load(sys.stdin)["program_id"])')

# --- wlez (native-LEZ collateral): deploy so `wrap`/`create-token-sale` work ---
echo ">> deploying wlez program (own block)"
WLEZ_BIN="$PROG/artifacts/lpad/wlez.bin"
if [ -f "$WLEZ_BIN" ]; then
  deploy_verified "$WLEZ_BIN" wlez || exit 1
  wait_block
else
  echo "   (wlez.bin missing - skip; build: bash scripts/build-guests.sh)"
fi

# --- ata (Associated Token Accounts) -----------------------------------------
# Nothing to deploy: lpad used to ship a hardened fork of the ATA program, but
# v0.2.1 pre-deploys the canonical one at genesis (see
# testnet_initial_state::initial_programs), so its id is already live and is a
# compile-time constant on our side.
ATA_ID=$(lpad program-id ata --json | python3 -c 'import json,sys;print(json.load(sys.stdin)["program_id"])')
echo ">> ata program (canonical, pre-deployed at genesis)"
echo "   ata program id = $ATA_ID"
echo "   lbp program id = $LBP_ID"

# Weights shift 0.9 -> 0.1 over a 1-year window STARTING NOW so the sale is
# active throughout the test run; collateral_seed anchors the opening price.
# t_start must be ~now (not epoch 0): create_sale caps (t_end - t_start) at
# ~10 years, and `now` itself is already >10y past epoch 0, so a 0 start has no
# valid end.
LBP_DEPOSIT=500000; LBP_SEED=10000
LBP_TSTART=$(date +%s%3N); LBP_TEND=$(( LBP_TSTART + 31536000000 ))  # now .. now+1yr
echo ">> creating LBP sale (deposit=$LBP_DEPOSIT seed=$LBP_SEED w 0.9->0.1)"
lpad lbp create-sale --program "$LBP_ID" \
  --collateral-def "$COLL_DEF" --treasury "$TREASURY" \
  --creator-token-holding "$PROJ_HOLD" --creator-collateral-holding "$COLL_HOLD" \
  --creator "$CREATOR" --token-deposit "$LBP_DEPOSIT" --collateral-seed "$LBP_SEED" \
  --w-start 0.9 --w-end 0.1 --t-start "$LBP_TSTART" --t-end "$LBP_TEND" \
  --fee-bps "$FEE_BPS" --nonce "$NONCE" >&2
wait_block
LBP_POOL_ID=$(lpad lbp ids --program "$LBP_ID" --token-def "$PROJ_DEF" \
  --collateral-def "$COLL_DEF" --creator "$CREATOR" --nonce "$NONCE" --json \
  | python3 -c 'import json,sys;print(json.load(sys.stdin)["pool"])')
echo "   lbp pool id = $LBP_POOL_ID"

{
  echo "# LPAD bootstrap output ($(date -u +%FT%TZ))"
  echo "export LEE_WALLET_HOME_DIR=\"$HOME_DIR\""
  echo "export LPAD_WALLET_CONFIG=\"$HOME_DIR/wallet_config.json\""
  echo "export LPAD_WALLET_STORAGE=\"$HOME_DIR/storage.json\""
  echo "export LPAD_SEQUENCER_ADDR=\"$SEQ_ADDR\""
  echo "export LPAD_BC_PROGRAM_ID=\"$BC_ID\""
  echo "export LPAD_SALE_ID=\"$SALE_ID\""
  echo "export LPAD_LBP_PROGRAM_ID=\"$LBP_ID\""
  echo "export LPAD_LBP_POOL_ID=\"$LBP_POOL_ID\""
  echo "export LPAD_ATA_PROGRAM_ID=\"$ATA_ID\""
  echo "export LPAD_PROJ_DEF=\"$PROJ_DEF\""
  echo "export LPAD_COLL_DEF=\"$COLL_DEF\""
  echo "export LPAD_CREATOR=\"$CREATOR\""
  echo "export LPAD_PROJ_HOLD=\"$PROJ_HOLD\""
  echo "export LPAD_COLL_HOLD=\"$COLL_HOLD\""
  echo "export LPAD_TREASURY=\"$TREASURY\""
  echo "export LPAD_BUYER_COLL=\"$BUYER_COLL\""
  echo "export LPAD_BUYER_TOK=\"$BUYER_TOK\""
  echo "export LPAD_PRIV_COLL=\"$PRIV_COLL\""
  echo "export LPAD_PRIV_PROJ=\"$PRIV_PROJ\""
} > "$OUT"
echo ">> wrote $OUT"; cat "$OUT"
