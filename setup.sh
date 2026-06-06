#!/usr/bin/env bash
# Per-user setup for LPAD. Run this once after `git clone`.
#
# Creates a `_lez` symlink in the repo root pointing at your LEZ source
# checkout. Every Cargo.toml patches the upstream nssa/nssa_core deps via
# this symlink, so the build works on any machine without modifying
# tracked files.
#
# Override the LEZ location:
#   LPAD_LEZ_DIR=/path/to/lez bash setup.sh
#
# Defaults to ~/lez; set LPAD_LEZ_DIR to your own LEZ checkout - see the README.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
LEZ_DIR="${LPAD_LEZ_DIR:-$HOME/lez}"

if [ ! -d "$LEZ_DIR" ]; then
    echo "✗ LEZ source tree not found at $LEZ_DIR" >&2
    echo "  Clone + build the LEZ runtime, then point LPAD_LEZ_DIR at it (see the README):" >&2
    echo "    git clone --branch v0.2.0-rc4 https://github.com/logos-co/lez.git" >&2
    echo "    (cd lez && cargo build --release)" >&2
    echo "    LPAD_LEZ_DIR=\"\$PWD/lez\" bash setup.sh    # or place it at ~/lez" >&2
    exit 2
fi

LEZ_DIR="$(cd "$LEZ_DIR" && pwd)"   # canonicalise
LINK="$REPO/_lez"

if [ -L "$LINK" ]; then
    current="$(readlink -f "$LINK")"
    if [ "$current" = "$LEZ_DIR" ]; then
        echo "✓ _lez already points at $LEZ_DIR"
    else
        echo "→ updating _lez: $current → $LEZ_DIR"
        rm "$LINK"
        ln -s "$LEZ_DIR" "$LINK"
    fi
elif [ -e "$LINK" ]; then
    echo "✗ $LINK exists and is not a symlink - refusing to overwrite" >&2
    exit 2
else
    ln -s "$LEZ_DIR" "$LINK"
    echo "✓ created _lez → $LEZ_DIR"
fi

# Sanity-check key sub-paths exist.
for sub in nssa nssa/core wallet; do
    if [ ! -d "$LINK/$sub" ]; then
        echo "✗ Expected $LINK/$sub does not exist." >&2
        echo "  Your LEZ checkout layout differs from v0.2.0-rc4. Make sure you" >&2
        echo "  cloned the right branch (see the README)." >&2
        exit 2
    fi
done

echo
echo "Setup complete. Next:"
echo "  1. cd $REPO/programs"
echo "  2. cargo build --release                    # programs + IDLs"
echo "  3. bash $REPO/run-sequencer.sh              # start dev sequencer (127.0.0.1:3040)"
echo "  4. bash $REPO/scripts/bootstrap.sh          # deploy + fund (another terminal)"
echo "  5. bash $REPO/scripts/install-cli.sh        # install lpad on PATH"
echo "  6. lpad status                              # smoke test"
