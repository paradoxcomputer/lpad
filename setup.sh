#!/usr/bin/env bash
# Per-user setup check for LPAD. Run this once after `git clone`.
#
# lpad is pinned to LEZ **v0.2.4**, pulled from the published git tag, so there is
# nothing to wire up: the old `_lez` symlink and the [patch] sections that used it
# are gone. (The change landed in v0.2.1, which upstreamed the one local
# modification lpad needed - the bonsai-free risc0 config, without which the zkVM
# guests cannot cross-compile. The v0.2.x attributions below are likewise the
# release that introduced each requirement, not a claim about the current pin.)
#
# This script verifies the toolchain can do the things that are not plain
# `cargo build`: cross-compile the guests reproducibly (Docker + cargo-risczero)
# and link the wallet (libpcsclite, via keycard support, which is a non-optional
# dependency). It also reports whether the LEZ checkout that only
# scripts/bootstrap.sh needs is present.
#
# Only what blocks a build fails: a missing cargo or libpcsclite, or a cached
# pcsc link path that would break every relink. Docker and cargo-risczero are
# warnings, because the guest ELFs they would produce are committed under
# programs/artifacts/lpad/ - a machine without either still builds, installs and
# runs everything except a guest rebuild.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
export PATH="$HOME/.cargo/bin:$HOME/.risc0/bin:$PATH"
. "$HOME/.cargo/env" 2>/dev/null || true

fail=0
warned=0
note() { printf '  %s\n' "$1"; }
ok()   { printf '\033[32m✓\033[0m %s\n' "$1"; }
# A warning is a real gap that costs you something specific and nothing else;
# it must say what. Only `bad` sets the exit status.
warn() { printf '\033[33m!\033[0m %s\n' "$1"; warned=1; }
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
# on-chain program ids - are byte-identical on every machine. Both tools are
# needed by exactly two callers: scripts/build-guests.sh, and scripts/ci-e2e.sh
# when run with LPAD_VERIFY_GUESTS=1 (which is that same rebuild, compared
# against the committed bytes). Neither is on the path from `git clone` to a
# working CLI, so neither fails this script.
if command -v cargo-risczero >/dev/null 2>&1; then
    ok "cargo-risczero present ($(cargo risczero --version 2>/dev/null || echo '?'))"
else
    warn "cargo-risczero not found - you cannot rebuild the zkVM guests"
    note "needed by scripts/build-guests.sh and by LPAD_VERIFY_GUESTS=1; without"
    note "it the committed ELFs are what the host crates embed, which is what"
    note "every deployed sale was created against anyway"
    note "cargo install cargo-risczero --version 3.0.5"
fi
if docker info >/dev/null 2>&1; then
    ok "docker running (reproducible guest builds available)"
else
    warn "docker not available - the reproducible guest build needs it"
    note "same two callers: scripts/build-guests.sh and LPAD_VERIFY_GUESTS=1"
    note "you can still build the host crates; committed artifacts under"
    note "programs/artifacts/lpad/ are used as-is unless you change guest code"
fi

# --- 4. libpcsclite -----------------------------------------------------------
# The v0.2.1 wallet depends on keycard_wallet unconditionally, which links
# libpcsclite. Only the -dev package (headers/.pc/unversioned .so) is missing on
# most systems; pcsc-sys also honours PCSC_LIB_DIR as an escape hatch.
SHIM="$HOME/.local/lib/pcsc-shim"
if pkg-config --exists libpcsclite 2>/dev/null; then
    ok "libpcsclite found via pkg-config"
elif [ -n "${PCSC_LIB_DIR:-}" ] && [ -e "$PCSC_LIB_DIR/libpcsclite.so" ]; then
    ok "libpcsclite provided via \$PCSC_LIB_DIR"
else
    # Very common case: the distro ships the RUNTIME library (libpcsclite.so.1)
    # but not the -dev package, so there is no unversioned .so for the linker and
    # no .pc file for pkg-config. pcsc-sys accepts PCSC_LIB_DIR as an escape
    # hatch, so a one-file symlink farm is enough and needs no root.
    # No early `exit` in awk: it would close the pipe, SIGPIPE ldconfig, and
    # `set -o pipefail` would abort the whole script (exit 141).
    # `|| true`: on a distro with no ldconfig at all the pipeline fails, and under
    # `set -o pipefail` that would abort the script with a bare exit 1 instead of
    # reaching the diagnosis below.
    RUNTIME=$(ldconfig -p 2>/dev/null | awk '/libpcsclite\.so\.1 /{ if (!seen++) print $NF }' || true)
    if [ -n "$RUNTIME" ] && [ -e "$RUNTIME" ]; then
        mkdir -p "$SHIM" && ln -sfn "$RUNTIME" "$SHIM/libpcsclite.so"
        ok "created a libpcsclite shim at $SHIM (-> $RUNTIME)"
        note "add this to your shell profile, or the SDK/CLI will not link:"
        note "  export PCSC_LIB_DIR=$SHIM"
        # Not a hard failure: the shim exists, the user just has to export it.
    else
        bad "libpcsclite not found - the wallet (and so the SDK/CLI) will not link"
        note "Debian/Ubuntu: sudo apt install pkgconf libpcsclite-dev"
        note "Arch:          sudo pacman -S pkgconf pcsclite"
        note "Fedora:        sudo dnf install pkgconf pcsc-lite-devel"
        note "Or, without root, point PCSC_LIB_DIR at a directory containing a"
        note "libpcsclite.so symlink to your existing libpcsclite.so.1"
    fi
fi
# PCSC_LIB_DIR is captured by pcsc-sys's build script and BAKED INTO cargo's
# cached output, so it survives the variable being unset. If it ever pointed at a
# directory that later disappeared (a temp dir, another checkout), every relink
# fails with `unable to find library -lpcsclite` while a stale binary keeps
# working - which reads as a broken toolchain. The fix is to force the build
# script to re-run:
#     cargo clean -p pcsc-sys      # in each of programs/, cli/, sdk/
for w in programs cli sdk; do
    for out in "$REPO/$w"/target/*/build/pcsc-sys-*/output; do
        [ -e "$out" ] || continue
        dir=$(sed -n 's/^cargo:rustc-link-search=native=//p' "$out" | head -1)
        if [ -n "$dir" ] && [ ! -d "$dir" ]; then
            # `cargo clean -p` only touches the dev profile, so name the profile
            # the stale entry actually lives in - otherwise the release binary
            # keeps a dead link path and the next relink fails again.
            profile=$(basename "$(dirname "$(dirname "$(dirname "$out")")")")
            [ "$profile" = "release" ] && rel=" --release" || rel=""
            bad "$w/target/$profile has a cached pcsc link path that no longer exists: $dir"
            note "run: (cd $REPO/$w && cargo clean$rel -p pcsc-sys)"
        fi
    done
done

# --- 5. LEZ checkout - needed ONLY by scripts/bootstrap.sh --------------------
# Informational, never fatal: building lpad and running scripts/ci-e2e.sh need no
# checkout at all. Only bootstrap does, because it drives the upstream `wallet`
# binary and seeds its config from the checkout - and it used to discover that
# with a bare "wallet CLI not built", which reads as an lpad build problem.
LEZ_DIR="${LPAD_LEZ_DIR:-$HOME/lez}"
LEZ_WALLET="${LPAD_WALLET_BIN:-$LEZ_DIR/target/release/wallet}"
if [ -x "$LEZ_WALLET" ] && [ -f "$LEZ_DIR/lez/wallet/configs/debug/wallet_config.json" ]; then
    ok "LEZ wallet available for scripts/bootstrap.sh ($LEZ_WALLET)"
else
    ok "LEZ checkout not required to build (only scripts/bootstrap.sh needs one)"
    note "to bootstrap against a testnet later:"
    note "  git clone --branch v0.2.4 https://github.com/logos-blockchain/logos-execution-zone.git $LEZ_DIR"
    note "  (cd $LEZ_DIR && cargo build --release -p wallet)"
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

if [ "$warned" -ne 0 ]; then
    echo
    echo "The ! items above cost you step 2 and nothing else - every other step,"
    echo "including deploying and running against a live chain, works without them."
fi
