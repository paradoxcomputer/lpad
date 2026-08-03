#!/usr/bin/env bash
# Bootstrap an LPAD end-to-end run against a REAL LEZ sequencer.
#
# There is no bundled local sequencer any more: lpad targets the Logos testnet or
# the Paradox Computer testnet. Both currently run LEZ v0.2.0, which is what the
# crates are pinned to.
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
#   LPAD_FUNDER_KEY      private key to import for LPAD_FUNDER
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

# Network aliases, kept in step with `lpad_sdk::Network`.
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

[ -x "$WALLET" ] || { echo "wallet CLI not built: $WALLET" >&2; exit 1; }
[ -x "$LPAD" ]   || { echo "lpad CLI not built: $LPAD (run: cd cli && cargo build --release)" >&2; exit 1; }
# checkHealth over JSON-RPC: works for https (no explicit port) and actually
# proves the sequencer is serving, not just that a socket is open.
timeout 30 curl -sf -X POST "$SEQ_ADDR" -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"checkHealth","params":[]}' >/dev/null \
  || { echo "sequencer not reachable / unhealthy at $SEQ_ADDR" >&2; exit 1; }

export LEE_WALLET_HOME_DIR="$HOME_DIR"
mkdir -p "$HOME_DIR"
cp -f "$LEZ/lez/wallet/configs/debug/wallet_config.json" "$HOME_DIR/wallet_config.json"
python3 - "$HOME_DIR/wallet_config.json" "$SEQ_ADDR" <<'PY'
import json,sys
p,addr=sys.argv[1],sys.argv[2]
d=json.load(open(p))
# v0.2.0 schema: one `sequencer_addr`. (The `sequencers` array + multi-sequencer
# client only arrive in v0.2.1.) `initial_accounts` was an rc4 field.
d.pop("initial_accounts", None)
d["sequencer_addr"] = addr
# Real networks make blocks slower than a local dev chain did, and a round trip
# is a WAN hop - give polling room.
d["seq_poll_timeout"] = "20s"
d["seq_tx_poll_max_blocks"] = 20
d["seq_poll_max_retries"] = 10
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
new_pub() { w account new public 2>&1 | grep -oE 'Public/[1-9A-HJ-NP-Za-km-z]{32,44}' | head -1; }
new_priv() { w account new private --label "$1" 2>&1 | grep -oE 'Private/[1-9A-HJ-NP-Za-km-z]{32,44}' | head -1; }
lpad() { "$LPAD" --config "$HOME_DIR/wallet_config.json" --storage "$HOME_DIR/storage.json" "$@"; }

echo ">> creating accounts"
CREATOR=$(new_pub);   echo "   creator      = $CREATOR"
PROJ_DEF=$(new_pub);  PROJ_HOLD=$(new_pub)
COLL_DEF=$(new_pub);  COLL_HOLD=$(new_pub)
TREASURY=$(new_pub);  BUYER_COLL=$(new_pub); BUYER_TOK=$(new_pub)

# On a chain we do not own there is no genesis key to import. Either the caller
# supplies a funded account, or we self-fund the creator from the pinata faucet.
if [ -n "$FUNDER" ]; then
  echo ">> funding creator from the supplied funder $FUNDER"
  [ -n "$FUNDER_KEY" ] && { w account import public --private-key "$FUNDER_KEY" >&2 2>&1 || echo "   (already imported)" >&2; sleep 8; }
  w auth-transfer send --from "$FUNDER" --to "$CREATOR" --amount 8000 >&2 \
    || { echo "✗ transfer from $FUNDER failed - is it funded?" >&2; exit 1; }
else
  echo ">> claiming the pinata faucet into the creator (no LPAD_FUNDER given)"
  w pinata claim --to "$CREATOR" >&2 2>&1 \
    || { echo "✗ pinata claim failed. This chain may not run pinata, or it is drained." >&2
         echo "  Supply a funded account instead: LPAD_FUNDER=Public/... LPAD_FUNDER_KEY=<hex>" >&2
         exit 1; }
fi
sleep 20

echo ">> minting PROJECT (supply $PROJ_SUPPLY) + COLLATERAL (supply $COLL_SUPPLY)"
w token new --name PROJECT --total-supply "$PROJ_SUPPLY" \
  --definition-account-id "$PROJ_DEF" --supply-account-id "$PROJ_HOLD" >&2
sleep 14
w token new --name COLLAT --total-supply "$COLL_SUPPLY" \
  --definition-account-id "$COLL_DEF" --supply-account-id "$COLL_HOLD" >&2
sleep 14

echo ">> deploying bonding_curve program (own block)"
BC_BIN="$PROG/artifacts/lpad/bonding_curve.bin"
[ -f "$BC_BIN" ] || { echo "missing $BC_BIN (build: bash scripts/build-guests.sh)" >&2; exit 1; }
w deploy-program "$BC_BIN" >&2 2>&1 || true
sleep 20
BC_ID=$(lpad program-id bc --json | python3 -c 'import json,sys;print(json.load(sys.stdin)["program_id"])')
echo "   bonding_curve program id = $BC_ID"

echo ">> funding treasury (init), public buyer holdings, shielded private holdings"
w token send --from "$COLL_HOLD" --to "$TREASURY"   --amount "$INIT_DUST"  >&2 || true; sleep 14
w token send --from "$COLL_HOLD" --to "$BUYER_COLL"  --amount "$BUYER_FUND" >&2 || true; sleep 14
w token send --from "$PROJ_HOLD" --to "$BUYER_TOK"   --amount "$INIT_DUST"  >&2 || true; sleep 14

PRIV_COLL=$(new_priv "lpad-priv-coll-$$")
PRIV_PROJ=$(new_priv "lpad-priv-proj-$$")
echo "   priv collateral = $PRIV_COLL"
echo "   priv project    = $PRIV_PROJ"
w token send --from "$COLL_HOLD" --to "$PRIV_COLL" --amount "$PRIV_FUND" >&2 || true; sleep 16
w token send --from "$PROJ_HOLD" --to "$PRIV_PROJ" --amount "$INIT_DUST" >&2 || true; sleep 16
w account sync-private >/dev/null 2>&1 || true

echo ">> creating bonding-curve sale (D=$D R=$R Vt=$VT Vc=$VC fee=${FEE_BPS}bps)"
lpad bc create-sale --program "$BC_ID" \
  --collateral-def "$COLL_DEF" --treasury "$TREASURY" \
  --creator-token-holding "$PROJ_HOLD" --creator "$CREATOR" \
  --sale-quantity "$D" --dex-seed "$R" --vt "$VT" --vc "$VC" \
  --fee-bps "$FEE_BPS" --nonce "$NONCE" >&2
sleep 16

SALE_ID=$(lpad bc ids --program "$BC_ID" --token-def "$PROJ_DEF" \
  --collateral-def "$COLL_DEF" --creator "$CREATOR" --nonce "$NONCE" --json \
  | python3 -c 'import json,sys;print(json.load(sys.stdin)["sale"])')
echo "   sale id = $SALE_ID"

# --- LBP (RFP-016): deploy the program + create a time-driven sale ---------
echo ">> deploying lbp program (own block)"
LBP_BIN="$PROG/artifacts/lpad/lbp.bin"
[ -f "$LBP_BIN" ] || { echo "missing $LBP_BIN (build: bash scripts/build-guests.sh)" >&2; exit 1; }
w deploy-program "$LBP_BIN" >&2 2>&1 || true
sleep 20
LBP_ID=$(lpad program-id lbp --json | python3 -c 'import json,sys;print(json.load(sys.stdin)["program_id"])')

# --- wlez (native-LEZ collateral): deploy so `wrap`/`create-token-sale` work ---
echo ">> deploying wlez program (own block)"
WLEZ_BIN="$PROG/artifacts/lpad/wlez.bin"
[ -f "$WLEZ_BIN" ] && { w deploy-program "$WLEZ_BIN" >&2 2>&1 || true; sleep 20; echo "   wlez deployed"; } \
  || echo "   (wlez.bin missing - skip; build: bash scripts/build-guests.sh)"

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
sleep 16
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
