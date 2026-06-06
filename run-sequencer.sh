#!/usr/bin/env bash
# Run a local LEZ sequencer in standalone mode (no Bedrock/Indexer).
# Listens on 127.0.0.1:3040 - the address the mini-app's wallet config and
# the CLI use. Genesis accounts are pre-funded (no faucet needed locally).
#
# Leave this running in its own terminal while you use the launchpad
# (bootstrap, CLI, mini-app).
set -euo pipefail

# LEZ source-tree location. Defaults to the `_lez` symlink that setup.sh creates
# in the repo root (pointing at your LEZ checkout); override with $LPAD_LEZ_DIR.
LEZ="${LPAD_LEZ_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)/_lez}"
CFG="${LPAD_SEQUENCER_CONFIG:-$LEZ/sequencer/service/configs/debug/sequencer_config.json}"

export PATH="$HOME/.cargo/bin:$HOME/.risc0/bin:$PATH"
. "$HOME/.cargo/env" 2>/dev/null || true
export LOGOS_BLOCKCHAIN_CIRCUITS="${LOGOS_BLOCKCHAIN_CIRCUITS:-$HOME/.logos-blockchain-circuits}"
unset RISC0_DEV_MODE   # real proofs

cd "$LEZ"
BIN="$LEZ/target/release/sequencer_service"
if [ ! -x "$BIN" ]; then
  echo ">> Building standalone sequencer (first run; slow)..."
  cargo build --features standalone -p sequencer_service --release
fi

echo ">> Starting sequencer (standalone) on 127.0.0.1:3040 - Ctrl-C to stop"
exec "$BIN" "$CFG"
