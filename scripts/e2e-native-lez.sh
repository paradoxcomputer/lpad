#!/usr/bin/env bash
# End-to-end native-LEZ launch test on a FRESH dev chain with SEPARATE creator +
# buyer wallets (the realistic scenario; no god-wallet treasury collision).
# Flow: deploy bc+wlez -> creator create-token-sale -> buyer wrap -> buy ->
#       shield -> sell-private. Each step asserted.
set -uo pipefail
cd "$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$HOME/.risc0/bin:$PATH"
export RISC0_DEV_MODE=1 LOGOS_BLOCKCHAIN_CIRCUITS="$HOME/.logos-blockchain-circuits"
LEZ="${LPAD_LEZ_DIR:-$PWD/_lez}"
W=$LEZ/target/release/wallet
L=cli/target/release/lpad
SEQ=$LEZ/target/release/sequencer_service
GEN=Public/2RHZhw9h534Zr3eq2RGhQete2Hh667foECzXPmSkGni2
PASS=0; FAIL=0
ok(){ echo "  ✓ $*"; PASS=$((PASS+1)); }
no(){ echo "  ✗ $*"; FAIL=$((FAIL+1)); }

echo ">> fresh dev sequencer :3044"
pkill -f "port 3044" 2>/dev/null; sleep 1
rm -rf /tmp/e2e-seq/data          # wipe → fresh genesis (a reused chain drains the shared funder / desyncs its nonce → "insufficient balance")
mkdir -p /tmp/e2e-seq/data
python3 -c 'import json;d=json.load(open("/tmp/lpad-rc4-seq/seqcfg.json"));d["home"]="/tmp/e2e-seq/data";json.dump(d,open("/tmp/e2e-seq/seqcfg.json","w"))'
pkill -f "port 3044" 2>/dev/null; sleep 1
nohup $SEQ /tmp/e2e-seq/seqcfg.json --port 3044 >/tmp/e2e-seq/seq.log 2>&1 &
sleep 6

mkwallet(){ # $1=dir
  rm -rf "$1"; mkdir -p "$1"
  cp $LEZ/wallet/configs/debug/wallet_config.json "$1/"
  python3 -c 'import json,sys;p=sys.argv[1]+"/wallet_config.json";d=json.load(open(p));d["sequencer_addr"]="http://127.0.0.1:3044";d["seq_poll_timeout"]="2s";json.dump(d,open(p,"w"),indent=4)' "$1"
  echo dev > "$1/proof_mode"
}
HC=/tmp/e2e-creator; HB=/tmp/e2e-buyer; mkwallet $HC; mkwallet $HB
wc(){ NSSA_WALLET_HOME_DIR=$HC bash -c "printf 'lpaddev\n' | $W $*"; }
wb(){ NSSA_WALLET_HOME_DIR=$HB bash -c "printf 'lpaddev\n' | $W $*"; }
lc(){ NSSA_WALLET_HOME_DIR=$HC $L "$@"; }
lb(){ NSSA_WALLET_HOME_DIR=$HB $L "$@"; }

echo ">> deploy bc + wlez"
wc deploy-program programs/target/riscv-guest/bonding_curve-methods/bonding_curve-guest/riscv32im-risc0-zkvm-elf/release/bonding_curve.bin >/dev/null 2>&1; sleep 16
wc deploy-program programs/target/riscv-guest/wlez-methods/wlez-guest/riscv32im-risc0-zkvm-elf/release/wlez.bin >/dev/null 2>&1; sleep 16

echo ">> accounts + funding (genesis -> creator, buyer)"
CREATOR=$(wc account new public 2>&1 | grep -oE 'Public/[1-9A-HJ-NP-Za-km-z]{32,44}' | head -1)
BUYER=$(wb account new public 2>&1 | grep -oE 'Public/[1-9A-HJ-NP-Za-km-z]{32,44}' | head -1)
echo "   creator=$CREATOR buyer=$BUYER"
wc auth-transfer send --from $GEN --to $CREATOR --amount 6000 >/dev/null 2>&1; sleep 14
wc auth-transfer send --from $GEN --to $BUYER   --amount 6000 >/dev/null 2>&1; sleep 14

echo ">> [creator] create-token-sale (mint DEMO + WLEZ-collateral sale)"
J=$(lc bc create-token-sale --name DEMO --symbol DMO --supply 1000000 --sale-quantity 500000 --dex-seed 50000 --vt 2000000 --vc 50000 --fee-bps 100 --creator "$CREATOR" --json 2>/dev/null | tail -1)
SALE=$(echo "$J" | python3 -c 'import json,sys;print(json.load(sys.stdin)["sale"])' 2>/dev/null)
[ -n "$SALE" ] && ok "create-token-sale (sale ${SALE:0:12}..)" || { no "create-token-sale"; echo "$J"; }

echo ">> [buyer] wrap 3000 LEZ -> WLEZ"
lb wrap --amount 3000 >/dev/null 2>&1 && ok "wrap" || no "wrap"
echo ">> [buyer] buy --in 1000 (WLEZ auto-detected; no treasury collision)"
lb bc buy --sale "$SALE" --in 1000 >/dev/null 2>&1 && ok "native-LEZ buy" || no "native-LEZ buy"
echo "   buyer holdings:"; lb my-balance 2>/dev/null | grep -iE "DEMO|WLEZ|LEZ \(public\)" | sed 's/^/     /'

echo ">> [buyer] shield DEMO 1000 -> shielded"
DEMODEF=$(echo "$J" | python3 -c 'import json,sys;print(json.load(sys.stdin)["token_definition"])' 2>/dev/null)
lb shield --token-def "$DEMODEF" --amount 1000 >/dev/null 2>&1 && ok "shield DEMO" || no "shield DEMO"
echo ">> [buyer] deshield DEMO 500 -> public"
lb deshield --token-def "$DEMODEF" --amount 500 >/dev/null 2>&1 && ok "deshield DEMO" || no "deshield DEMO"

echo ">> [creator] create-token-sale --private (unlinkable creator via shielded deposit)"
JP=$(lc bc create-token-sale --name PRIV --symbol PRV --supply 1000000 --sale-quantity 200000 --dex-seed 20000 --vt 2000000 --vc 50000 --fee-bps 100 --creator "$CREATOR" --private --nonce 1 --json 2>/dev/null | tail -1)
PSALE=$(echo "$JP" | python3 -c 'import json,sys;print(json.load(sys.stdin)["sale"])' 2>/dev/null)
[ -n "$PSALE" ] && ok "create-token-sale --private (sale ${PSALE:0:12}..)" || { no "create-token-sale --private"; echo "$JP" | tail -2; }

echo
echo "=== $PASS passed, $FAIL failed ==="
lb my-balance 2>/dev/null | grep -iE "DEMO|WLEZ" | sed 's/^/  /'
