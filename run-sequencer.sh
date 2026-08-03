#!/usr/bin/env bash
# Run a local LEZ sequencer in standalone mode (no Bedrock/Indexer).
# Listens on 127.0.0.1:3040 - the address the mini-app's wallet config and
# the CLI use.
#
# Leave this running in its own terminal while you use the launchpad
# (bootstrap, CLI, mini-app).
#
# NOTE on genesis funding: since LEZ v0.2.1, a `supply_account` genesis action
# credits the recipient's VAULT PDA, not the account's own balance. So genesis
# accounts start with a zero spendable balance and the funds must be claimed
# (`wallet vault claim`) before they can be transferred - scripts/bootstrap.sh
# does this. Only `supply_bridge_account` credits a balance directly.
set -euo pipefail

# LEZ source-tree location. Only needed to build/run the sequencer binary - the
# launchpad itself builds against the published git tag and needs no checkout.
# Override with $LPAD_LEZ_DIR.
LEZ="${LPAD_LEZ_DIR:-$HOME/lez}"
# v0.2.1 moved the sequencer under a `lez/` top-level directory.
CFG="${LPAD_SEQUENCER_CONFIG:-$LEZ/lez/sequencer/service/configs/debug/sequencer_config.json}"
PORT="${LPAD_SEQUENCER_PORT:-3040}"

if [ ! -d "$LEZ" ]; then
    echo "✗ LEZ source tree not found at $LEZ" >&2
    echo "  Clone it (only needed to run a local sequencer):" >&2
    echo "    git clone --branch v0.2.1 https://github.com/logos-blockchain/logos-execution-zone.git ~/lez" >&2
    echo "  or point LPAD_LEZ_DIR at an existing checkout." >&2
    exit 2
fi
if [ ! -f "$CFG" ]; then
    echo "✗ No sequencer config at $CFG" >&2
    echo "  Is $LEZ checked out at v0.2.1? (v0.2.0-rc4 had no lez/ prefix.)" >&2
    exit 2
fi

export PATH="$HOME/.cargo/bin:$HOME/.risc0/bin:$PATH"
. "$HOME/.cargo/env" 2>/dev/null || true
unset RISC0_DEV_MODE   # real proofs

cd "$LEZ"
BIN="$LEZ/target/release/sequencer_service"
if [ ! -x "$BIN" ]; then
  echo ">> Building standalone sequencer (first run; slow)..."
  cargo build --features standalone -p sequencer_service --release
fi

# v0.2.1 defaults --listen-address to 0.0.0.0; the RPC has no caller auth, so
# bind loopback explicitly.
echo ">> Starting sequencer (standalone) on 127.0.0.1:$PORT - Ctrl-C to stop"
exec "$BIN" "$CFG" --port "$PORT" --listen-address 127.0.0.1
