#!/usr/bin/env bash
# The public demo path, start to finish, with nothing in it that can mint a STARK.
#
#   pick network -> faucet -> wrap -> create sale -> quote -> public buy -> sell
#
# WHAT IT COSTS, honestly. This is not a five-minute script from a cold wallet and
# no arrangement of the steps makes it one: the flow is 7-11 on-chain transactions
# and both public sequencers produce a block roughly every 46-50 seconds, so the
# happy path is about **6-9 minutes**. The five-minute version is a *rehearsed*
# one - fund and wrap beforehand, then run with:
#
#   LPAD_DEMO_SKIP_FAUCET=1 LPAD_DEMO_SKIP_WRAP=1 bash scripts/demo.sh
#
# which leaves create -> quote -> buy -> sell, about 4-5 minutes. Skips are
# REPORTED, never counted as passes, so a shortened run cannot read as a full one.
#
# WHY NO PRIVATE OPERATION. Every shielded command (`shield`, `deshield`,
# `shield-lez`, `deshield-lez`, `buy-private`, `sell-private`, `buy-disposable`)
# mints a real recursive STARK: ~8-9 GB resident and 38-40 minutes for one proof,
# measured on Logos testnet. That is not a demo, it is an intermission. The
# exclusion is enforced rather than promised - see `lpad()` below, which refuses
# to run one - because a comment saying "no private ops here" is one well-meaning
# edit away from being false. `scripts/private-ops.sh` is where those belong.
#
# Prereqs: an lpad binary built from THIS tree (`bash scripts/install-cli.sh` or
# `cargo build --release` in cli/), and a wallet home that already exists. The
# demo cannot conjure one: `lpad` refuses to start without a `wallet_config.json`,
# and only the LEZ `wallet` binary can create the `storage.json` beside it, so a
# virgin machine needs `bash scripts/bootstrap.sh` once. An already-bootstrapped
# `~/.lpad` needs nothing else - this script creates its own token, its own
# treasury and its own sale.
#
# Env:
#   LPAD_NETWORK        testnet (default) | paradox | local | local=<url> | an
#                       http(s) sequencer URL
#   LPAD_WALLET_HOME    override the wallet home the network alias would pick
#   LPAD_BIN            override the lpad binary
#   LPAD_DEMO_SKIP_FAUCET / LPAD_DEMO_SKIP_WRAP   1 = assume the wallet is
#                       already funded / already holds WLEZ (the rehearsed run)
#   LPAD_DEMO_NAME / LPAD_DEMO_SYMBOL             the token's name and ticker
#   LPAD_DEMO_PREFLIGHT_ONLY  1 = run the checks and stop. Nothing is submitted
#                       and no faucet claim is spent, so it is the safe way to
#                       rehearse this script (and the only way to test it)
set -uo pipefail

# NON-INTERACTIVE, and it has to stay that way: `lpad` asks once which sequencer a
# wallet talks to when nothing is recorded for it, and that question fires only
# when stdin AND stderr are terminals. A live demo is exactly the situation where
# both are. Detaching stdin for the whole run is the first of two guards; every
# online invocation below also passes an explicit `--network`, which is consumed
# before the picker is ever consulted. Either alone is one edit from being lost.
exec </dev/null

REPO="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
export PATH="$HOME/.cargo/bin:$HOME/.risc0/bin:$PATH"
L="${LPAD_BIN:-$REPO/cli/target/release/lpad}"

# ---------------------------------------------------------------------------
# Network selection. Same vocabulary as scripts/test-all-cli.sh and
# lpad_sdk::Network, and per-network wallet homes for the same reason: every id
# in a wallet is a PDA on exactly one chain.
# ---------------------------------------------------------------------------
NETWORK="${LPAD_NETWORK:-testnet}"
[ "$NETWORK" = "logos" ] && NETWORK=testnet
case "$NETWORK" in
  testnet)            NET_HOME="$HOME/.lpad";         NET_ADDR="https://testnet.lez.logos.co" ;;
  paradox)            NET_HOME="$HOME/.lpad-paradox"; NET_ADDR="https://seq-testnet.paradox.computer" ;;
  local)              NET_HOME="$HOME/.lpad";         NET_ADDR="http://127.0.0.1:3040" ;;
  local=*)            NET_HOME="$HOME/.lpad";         NET_ADDR="${NETWORK#local=}" ;;
  http://*|https://*) NET_HOME="$HOME/.lpad";         NET_ADDR="$NETWORK" ;;
  *) echo "unknown LPAD_NETWORK=$NETWORK (testnet|paradox|local|local=<url>|<url>)" >&2; exit 2 ;;
esac
export LEE_WALLET_HOME_DIR="${LPAD_WALLET_HOME:-$NET_HOME}"
NET=(--network "$NETWORK")

# ---------------------------------------------------------------------------
# Reporting. Counters and a name list, as in test-all-cli.sh; a skip is neither a
# pass nor a failure and is always listed, so a partial run cannot read as full.
# ---------------------------------------------------------------------------
PASS=0; FAIL=0; SKIP=0; FAILED=(); SKIPPED=()
STARTED=$(date +%s)
ok()   { printf '  \033[32m✓\033[0m %s\n' "$*"; PASS=$((PASS + 1)); }
no()   { printf '  \033[31m✗\033[0m %s\n' "$*"; FAIL=$((FAIL + 1)); FAILED+=("$1"); }
skip() { printf '  \033[33m!\033[0m %s - %s\n' "$1" "$2"; SKIP=$((SKIP + 1)); SKIPPED+=("$1"); }
step() { printf '\n\033[1m%s\033[0m\n' "$*"; }
die()  { printf '\n\033[31m%s\033[0m\n' "$*" >&2; exit 2; }

# Elapsed wall clock for one step, because "is it hung or is it a block?" is the
# question every audience asks and a block is 46-50s.
since() { local t=$1; printf '    (%ss)\n' "$(( $(date +%s) - t ))"; }

# ---------------------------------------------------------------------------
# The private-operation guard. Structural, not a promise in a comment: the demo
# never calls "$L" directly, and this refuses the subcommands that prove.
# ---------------------------------------------------------------------------
lpad() {
  local a
  for a in "$@"; do
    case "$a" in
      shield|deshield|shield-lez|deshield-lez|buy-private|sell-private|buy-disposable)
        die "demo.sh refused to run \`lpad $a\`: it mints a real recursive STARK (~8-9 GB, 38-40 min on testnet). Private operations belong in scripts/private-ops.sh." ;;
    esac
  done
  "$L" "$@"
}

# Run one lpad command ONCE, keeping its output for a failure report.
#
# The obvious shape - run it quietly, and re-run it to show the error - is wrong
# here in a way that costs money: every step below submits a transaction, so a
# re-run mints a second token, sends a second buy, or claims the faucet twice.
# `try` therefore captures the first attempt and prints its tail if it failed.
# `$OUT` holds the stdout of the last attempt, for the JSON steps to parse.
OUT=""
try() {
  local name="$1"; shift
  local err; err=$(mktemp)
  if OUT=$("$@" 2>"$err"); then
    rm -f "$err"; return 0
  fi
  sed 's/^/    /' "$err" | tail -8
  rm -f "$err"; OUT=""; return 1
}

# Read one field out of an lpad --json payload. Degrades to empty rather than
# exploding, so a caller can decide what a missing field means.
jget() { python3 -c 'import json,sys
try: print(json.load(sys.stdin).get(sys.argv[1], "") or "")
except Exception: print("")' "$1" 2>/dev/null; }

echo "## lpad public demo - $NETWORK ($NET_ADDR)"
echo "## wallet home: $LEE_WALLET_HOME_DIR"

# ---------------------------------------------------------------------------
# Preflight. Everything that can be known before a single transaction is spent,
# checked here, because each failure below this line costs a block to discover.
# Exit 2 = "the run never started"; exit 1 = "it ran and something failed".
# ---------------------------------------------------------------------------
step "0. preflight"

[ -x "$L" ] || die "no lpad binary at $L - build one with \`cargo build --release\` in cli/, or set LPAD_BIN."
ok "binary $($L --version 2>/dev/null || echo "$L")"

[ -f "$LEE_WALLET_HOME_DIR/wallet_config.json" ] \
  || die "no wallet at $LEE_WALLET_HOME_DIR - run \`bash scripts/bootstrap.sh\` once to create one. lpad cannot create a wallet itself; only the LEZ wallet binary can write the storage.json beside the config."
ok "wallet $LEE_WALLET_HOME_DIR"

HEIGHT=$(timeout 30 curl -s -X POST "$NET_ADDR" -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"getLastBlockId","params":[]}' 2>/dev/null \
  | python3 -c 'import sys,json; print(json.load(sys.stdin)["result"])' 2>/dev/null || true)
[ -n "$HEIGHT" ] || die "$NET_ADDR did not answer getLastBlockId - nothing here can work until it does."
ok "sequencer alive at block $HEIGHT"

# The check that saves the most time. A transaction against a program that is not
# deployed is DROPPED from the mempool and reads as a timeout, not an error, so a
# run that skips this spends its whole poll budget - 30 minutes per transaction on
# the bootstrap's config - to learn nothing. Tolerated as a skip only when the
# binary predates `network --check`, since an old binary is a rebuild away.
DEPLOY_OUT=$(lpad "${NET[@]}" network --check 2>&1)
DEPLOY_RC=$?
if [ "$DEPLOY_RC" -eq 0 ]; then
  ok "all three lpad programs are on this chain"
elif echo "$DEPLOY_OUT" | grep -q -- "--check"; then
  skip "deployment check" "this lpad binary has no \`network --check\` - rebuild from this tree to get it"
else
  printf '%s\n' "$DEPLOY_OUT" | sed 's/^/    /'
  die "lpad's programs are not deployed on $NET_ADDR. Deploy them with \`bash scripts/bootstrap.sh\` first - every step below would be dropped from the mempool and look like a slow chain."
fi

if [ "${LPAD_DEMO_PREFLIGHT_ONLY:-0}" = "1" ]; then
  printf '\n=== preflight only: %s passed, %s failed, %s skipped ===\n' "$PASS" "$FAIL" "$SKIP"
  [ "$SKIP" -gt 0 ] && printf 'SKIPPED: %s\n' "${SKIPPED[@]}"
  [ "$FAIL" -eq 0 ] || { printf 'FAILED: %s\n' "${FAILED[@]}"; exit 1; }
  exit 0
fi

# ---------------------------------------------------------------------------
# 1. Faucet. 150 native LEZ per claim, fixed, no cooldown. It is the one command
#    whose whole effect is to change that balance, and it REPORTS the resulting
#    balance - so the wrap below is sized off a measurement rather than a
#    constant. A hardcoded amount is exactly how this flow broke before: `--amount
#    2000` against a 150-unit prize reported a faucet limit as a wrap defect.
# ---------------------------------------------------------------------------
step "1. faucet"
NATIVE=""
if [ "${LPAD_DEMO_SKIP_FAUCET:-0}" = "1" ]; then
  skip "faucet" "LPAD_DEMO_SKIP_FAUCET=1 (rehearsed run: the wallet is already funded)"
else
  t=$(date +%s)
  if try faucet lpad "${NET[@]}" --json faucet; then
    FAUCET_JSON="$OUT"
    NATIVE=$(printf '%s' "$FAUCET_JSON" | jget balance)
    ok "claimed $(printf '%s' "$FAUCET_JSON" | jget amount) native LEZ into $(printf '%s' "$FAUCET_JSON" | jget account) (balance now $NATIVE)"
    LEFT=$(printf '%s' "$FAUCET_JSON" | jget faucet_claims_left)
    if [ -n "$LEFT" ] && [ "$LEFT" -le 3 ] 2>/dev/null; then
      printf '    ! the faucet has about %s claim(s) left - a rehearsal may find it drained\n' "$LEFT"
    fi
  else
    no "faucet"
  fi
  since "$t"
fi

# ---------------------------------------------------------------------------
# 2. Wrap. WLEZ is the collateral `bc create-token-sale` raises in, and wrapping
#    is the only way to get it. Sized off the measured balance: a claim pays 150,
#    so any fixed amount above that reports a faucet limit as a wrap defect.
# ---------------------------------------------------------------------------
step "2. wrap native LEZ -> WLEZ"
WLEZ_DEF=$(lpad --json program-id wlez 2>/dev/null | jget definition)
[ -n "$WLEZ_DEF" ] || die "could not derive the WLEZ token definition offline - that is a broken binary, not a chain problem."

if [ "${LPAD_DEMO_SKIP_WRAP:-0}" = "1" ]; then
  skip "wrap" "LPAD_DEMO_SKIP_WRAP=1 (rehearsed run: the wallet already holds WLEZ)"
else
  t=$(date +%s)
  # 40% of the measured balance, leaving room for fees and a second run. With no
  # measurement (the faucet was skipped) fall back to a figure that is under one
  # claim rather than guessing upward - being wrong low costs a smaller demo,
  # being wrong high costs the step.
  if [ -n "$NATIVE" ] && [ "$NATIVE" -gt 0 ] 2>/dev/null; then
    WRAP_AMT=$(( NATIVE * 2 / 5 ))
  else
    WRAP_AMT="${LPAD_DEMO_WRAP_AMOUNT:-60}"
    echo "    (no measured balance this run - wrapping $WRAP_AMT; set LPAD_DEMO_WRAP_AMOUNT to change)"
  fi
  if [ "$WRAP_AMT" -lt 10 ]; then
    skip "wrap" "only $NATIVE native LEZ - too little to wrap meaningfully (re-run \`lpad faucet\`)"
  elif try wrap lpad "${NET[@]}" wrap --amount "$WRAP_AMT"; then
    ok "wrapped $WRAP_AMT native LEZ into WLEZ"
  else
    no "wrap"
  fi
  since "$t"
fi

# The buyer's collateral holding: the wallet's WLEZ holding, matched by the
# definition derived offline above. Resolved explicitly rather than left to the
# CLI's default, because the sell leg REQUIRES both holdings by id and nothing in
# the buy output names them.
COLL_HOLD=$(lpad "${NET[@]}" --json my-balance 2>/dev/null | python3 -c 'import json,sys
try: hs = json.load(sys.stdin).get("holdings", [])
except Exception: hs = []
d = sys.argv[1].lower().removeprefix("0x")
c = [h for h in hs if not h.get("private") and (h.get("definition") or "").lower().removeprefix("0x") == d]
c.sort(key=lambda h: int(h.get("balance") or 0), reverse=True)
print(c[0]["account"] if c else "")' "$WLEZ_DEF" 2>/dev/null)
[ -n "$COLL_HOLD" ] || die "the wallet holds no WLEZ - wrap some first (this run either skipped the wrap or it failed)."
echo "    collateral holding: $COLL_HOLD"

# ---------------------------------------------------------------------------
# 3. Create the sale. Self-sufficient: it mints the project token with metadata,
#    bootstraps wlez if this chain has never seen it, creates its OWN treasury
#    holding, and opens the sale. No bootstrap env file, no pre-made accounts.
# ---------------------------------------------------------------------------
VT=2000000; VC=50000; FEE_BPS=100
NAME="${LPAD_DEMO_NAME:-DEMO}"; SYMBOL="${LPAD_DEMO_SYMBOL:-DMO}"

step "3. create the sale ($NAME/$SYMBOL)"
t=$(date +%s)
try create-token-sale lpad "${NET[@]}" --json bc create-token-sale \
  --name "$NAME" --symbol "$SYMBOL" --supply 1000000 \
  --sale-quantity 500000 --dex-seed 50000 \
  --vt "$VT" --vc "$VC" --fee-bps "$FEE_BPS"
SALE=$(printf '%s' "$OUT" | jget sale)
TOK_HOLD=$(printf '%s' "$OUT" | jget creator_token_holding)
if [ -z "$SALE" ] || [ -z "$TOK_HOLD" ]; then
  no "create-token-sale"
  die "the sale was not created, so there is nothing to quote, buy or sell. Note that a partial create may still have minted the token - check \`lpad my-balance\` before re-running."
fi
ok "sale $SALE"
echo "    token holding: $TOK_HOLD"
since "$t"

# ---------------------------------------------------------------------------
# 4. Quote. OFFLINE - no wallet, no sequencer, no block. It is the same curve
#    math the guest runs, which is what makes it worth showing: the number here
#    is the number the chain will produce, and step 5 pins that by passing it as
#    the slippage floor.
# ---------------------------------------------------------------------------
BUY_IN=20
step "4. quote $BUY_IN WLEZ (offline)"
t=$(date +%s)
try quote lpad --json bc quote --vt "$VT" --vc "$VC" --fee-bps "$FEE_BPS" --in "$BUY_IN"
QUOTE="$OUT"
TOKENS_OUT=$(printf '%s' "$QUOTE" | jget tokens_out)
if [ -z "$TOKENS_OUT" ] || [ "$TOKENS_OUT" = "0" ]; then
  no "quote"
  die "no quote, so there is no floor to buy against."
fi
ok "$BUY_IN WLEZ -> $TOKENS_OUT $SYMBOL (fee $(printf '%s' "$QUOTE" | jget fee), impact $(printf '%s' "$QUOTE" | jget price_impact_pct)%)"
since "$t"

# ---------------------------------------------------------------------------
# 5. Public buy, against the quote. `--min-out` at 99% of the quoted amount is a
#    real assertion, not decoration: on a fresh sale with no competing trade the
#    chain must produce exactly what the offline quote said, and anything less
#    means the two implementations have drifted.
# ---------------------------------------------------------------------------
MIN_OUT=$(( TOKENS_OUT * 99 / 100 ))
step "5. public buy"
t=$(date +%s)
if try buy lpad "${NET[@]}" bc buy --sale "$SALE" --in "$BUY_IN" \
     --buyer-collateral "$COLL_HOLD" --buyer-token "$TOK_HOLD" --min-out "$MIN_OUT"; then
  ok "bought with $BUY_IN WLEZ (floor $MIN_OUT $SYMBOL)"
else
  no "buy"
fi
since "$t"

step "5b. the price after the buy"
lpad "${NET[@]}" bc sale-info --sale "$SALE" 2>&1 | sed 's/^/    /'

# ---------------------------------------------------------------------------
# 6. Public sell, back into the same curve. Half of what the buy bought, because
#    a sell is bounded by the sale's REAL collateral (`c_out_raw <=
#    state.real_collateral`) and this buy is the only thing that put any there.
# ---------------------------------------------------------------------------
SELL_TOKENS=$(( TOKENS_OUT / 2 ))
step "6. public sell ($SELL_TOKENS $SYMBOL)"
t=$(date +%s)
if try sell lpad "${NET[@]}" bc sell --sale "$SALE" --tokens "$SELL_TOKENS" \
     --seller-token "$TOK_HOLD" --seller-collateral "$COLL_HOLD" --min-out 0; then
  ok "sold $SELL_TOKENS $SYMBOL back into the curve"
else
  no "sell"
fi
since "$t"

# ---------------------------------------------------------------------------
printf '\n=== %s passed, %s failed, %s skipped in %ss ===\n' \
  "$PASS" "$FAIL" "$SKIP" "$(( $(date +%s) - STARTED ))"
echo "sale: $SALE"
[ "$SKIP" -gt 0 ] && printf 'SKIPPED: %s\n' "${SKIPPED[@]}"
if [ "$FAIL" -gt 0 ]; then
  printf 'FAILED: %s\n' "${FAILED[@]}"
  exit 1
fi
exit 0
