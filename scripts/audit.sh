#!/usr/bin/env bash
# Run cargo-audit over all three lockfiles with the ignore list actually applied.
#
# Why a script: cargo-audit reads its config from `.cargo/audit.toml` relative to
# the CURRENT DIRECTORY, and lpad has three separate workspaces. Running
# `cargo audit` from inside cli/ would look for cli/.cargo/audit.toml, miss it,
# and report every ignored advisory as a failure. So this stays at the repo root
# and points `-f` at each lockfile in turn.
#
# There is no `--config` flag; `-c` is `--color`. The invocation this repo used to
# document (`cargo audit -c audit.toml -f ...`) errors out, which is why the
# ignore list had never once been applied.
#
#   bash scripts/audit.sh            # fetch the advisory DB first (default)
#   LPAD_AUDIT_OFFLINE=1 bash scripts/audit.sh
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"
cd "$REPO"

command -v cargo-audit >/dev/null 2>&1 || {
  echo "cargo-audit not installed:  cargo install cargo-audit --locked" >&2; exit 2; }

ARGS=(--deny warnings)
[ "${LPAD_AUDIT_OFFLINE:-0}" = "1" ] && ARGS+=(--no-fetch --stale)

fail=0
for w in programs cli sdk; do
  echo "== $w/Cargo.lock =="
  cargo audit "${ARGS[@]}" -f "$w/Cargo.lock" || fail=1
  echo
done

if [ "$fail" -ne 0 ]; then
  echo "✗ cargo audit found something not covered by .cargo/audit.toml." >&2
  echo "  Check provenance before adding it there:  cargo tree -i <crate>" >&2
  exit 1
fi
echo "✓ all three lockfiles clean against .cargo/audit.toml"
