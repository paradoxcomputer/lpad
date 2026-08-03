#!/usr/bin/env bash
# Per-user setup check for LPAD. Run this once after `git clone`.
#
# Since the LEZ v0.2.1 upgrade there is nothing to wire up: every crate depends
# on the published git tag, so the old `_lez` symlink (and the [patch] sections
# that used it) are gone. LEZ v0.2.1 upstreamed the one local modification lpad
# needed - the bonsai-free risc0 config, without which the zkVM guests cannot
# cross-compile - so a local checkout is no longer required to build.
#
# This script now just verifies the toolchain can do the two things that are not
# plain `cargo build`: cross-compile the guests reproducibly (Docker +
# cargo-risczero) and link the wallet (libpcsclite, via the keycard support that
# v0.2.1 made a non-optional dependency).
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
export PATH="$HOME/.cargo/bin:$HOME/.risc0/bin:$PATH"
. "$HOME/.cargo/env" 2>/dev/null || true

fail=0
note() { printf '  %s\n' "$1"; }
ok()   { printf '\033[32m✓\033[0m %s\n' "$1"; }
bad()  { printf '\033[31m✗\033[0m %s\n' "$1"; fail=1; }

# --- 1. A stale _lez symlink from before the upgrade is now dead weight. -------
if [ -L "$REPO/_lez" ]; then
    rm "$REPO/_lez"
    ok "removed the obsolete _lez symlink (no longer used)"
fi

# --- 2. Rust toolchain --------------------------------------------------------
if command -v cargo >/dev/null 2>&1; then
    ok "cargo present ($(cargo --version))"
else
    bad "cargo not found - install Rust (https://rustup.rs)"
fi

# --- 3. Guest cross-compilation ----------------------------------------------
# Guests are built inside a pinned container so their image ids - which ARE the
# on-chain program ids - are byte-identical on every machine.
if command -v cargo-risczero >/dev/null 2>&1; then
    ok "cargo-risczero present ($(cargo risczero --version 2>/dev/null || echo '?'))"
else
    bad "cargo-risczero not found - needed to build the zkVM guests"
    note "cargo install cargo-risczero --version 3.0.5"
fi
if docker info >/dev/null 2>&1; then
    ok "docker running (reproducible guest builds available)"
else
    bad "docker not available - the reproducible guest build needs it"
    note "you can still build the host crates; committed artifacts under"
    note "programs/artifacts/lpad/ are used as-is unless you change guest code"
fi

# --- 4. libpcsclite -----------------------------------------------------------
# The v0.2.1 wallet depends on keycard_wallet unconditionally, which links
# libpcsclite. Only the -dev package (headers/.pc/unversioned .so) is missing on
# most systems; pcsc-sys also honours PCSC_LIB_DIR as an escape hatch.
if pkg-config --exists libpcsclite 2>/dev/null; then
    ok "libpcsclite found via pkg-config"
elif [ -n "${PCSC_LIB_DIR:-}" ] && [ -e "$PCSC_LIB_DIR/libpcsclite.so" ]; then
    ok "libpcsclite provided via \$PCSC_LIB_DIR"
else
    bad "libpcsclite not found - the wallet (and so the SDK/CLI) will not link"
    note "Debian/Ubuntu: sudo apt install pkgconf libpcsclite-dev"
    note "Arch:          sudo pacman -S pkgconf pcsclite"
    note "Fedora:        sudo dnf install pkgconf pcsc-lite-devel"
    note "Or, without root, point PCSC_LIB_DIR at a directory containing a"
    note "libpcsclite.so symlink to your existing libpcsclite.so.1"
fi

echo
if [ "$fail" -ne 0 ]; then
    echo "Setup incomplete - resolve the ✗ items above." >&2
    exit 2
fi

cat <<EOF
Setup complete. Next:
  1. cd $REPO/programs && cargo build --release       # host crates
  2. bash $REPO/scripts/build-guests.sh               # only if guest code changed
  3. bash $REPO/scripts/bootstrap.sh                  # deploy + fund on the testnet
  4. bash $REPO/scripts/install-cli.sh                # install lpad on PATH
  5. lpad status                                      # smoke test (against the Logos testnet)
EOF
