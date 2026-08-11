#!/usr/bin/env bash
# LPAD CI gate: unit tests (no zkVM) + guest-artifact freshness + in-process E2E.
# No sequencer involved: step 3 drives the LEZ state machine directly.
# Mirrors the ldex ci-e2e.sh. Run from the repo root.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
PROG="$REPO/programs"
export PATH="$HOME/.cargo/bin:$HOME/.risc0/bin:$PATH"
. "$HOME/.cargo/env" 2>/dev/null || true
# Pick up the libpcsclite shim setup.sh creates on distros that ship only the
# runtime library, so the gate works straight after setup.sh without the user
# having to edit a shell profile first. An explicit PCSC_LIB_DIR always wins.
if [ -z "${PCSC_LIB_DIR:-}" ] && [ -e "$HOME/.local/lib/pcsc-shim/libpcsclite.so" ]; then
  export PCSC_LIB_DIR="$HOME/.local/lib/pcsc-shim"
fi

fail() { echo "✗ $*" >&2; exit 1; }

echo "== 1. Unit tests (core + host, no zkVM) =="
# `lpad_guests --features artifacts` is what runs `pinned_ids_match_artifacts`:
# the committed ELFs must still hash to the ids in programs/src/lib.rs::deployed.
# Those ids are baked into every sale and pool PDA on chain, so an accidental
# artifact change orphans all existing state and points the CLI at a program
# nobody deployed. The guard existed but was never in the gate.
( cd "$PROG" && RISC0_SKIP_BUILD=1 cargo test -q \
    -p bonding_curve_core -p bonding_curve_program \
    -p lbp_core -p lbp_program -p wlez_core -p wlez_program \
    -p lpad_guests --features artifacts ) || fail "unit tests"

echo "== 1b. CLI tests (fmt unit + clap + offline/error e2e against the binary) =="
( cd "$REPO/cli" && RISC0_SKIP_BUILD=1 cargo test -q ) || fail "cli tests"

echo "== 1c. SDK chain-parity (the LEZ pin vs what the networks actually run) =="
# The one check that decides whether this build can transact at all: the SDK
# asserts the pinned LEZ version's built-in token / authenticated_transfer /
# clock image ids equal the ones deployed on the target sequencers. If they
# disagree, every path that touches a built-in program is rejected on chain -
# and the symptom is an indistinguishable "All pollers failed", not an error.
#
# It lives in `sdk`, which is its own workspace, so neither the `programs` nor
# the `cli` step above runs it: `cargo test` in cli/ builds lpad-sdk but only
# runs lpad-cli's own tests. It was therefore ungated until now.
#
# Linking needs libpcsclite (the LEZ wallet pulls keycard support in
# unconditionally); setup.sh checks for it and can create the shim.
( cd "$REPO/sdk" && RISC0_SKIP_BUILD=1 cargo test -q ) || fail "sdk chain-parity tests"

echo "== 2. Guest artifacts present =="
# The committed ELFs under artifacts/lpad/ ARE the deployed programs: their RISC0
# image ids are the on-chain program ids, baked into the SDK at compile time.
#
for g in bonding_curve lbp wlez; do
  [ -f "$PROG/artifacts/lpad/${g}.bin" ] || \
    fail "missing artifacts/lpad/${g}.bin - run scripts/build-guests.sh"
done

# The per-program ABIs are generated from source by scripts/build-abi.sh, so a
# stale one means the committed JSON no longer describes the deployed programs -
# and an indexer decoding against it would mis-read every instruction, silently.
# This comment used to promise the check without performing it; now it does.
#
# Needs the release CLI (build-abi.sh reads the program ids from it), so it is
# conditional rather than a hard requirement of the unit-test gate.
if [ -x "$REPO/cli/target/release/lpad" ]; then
  echo "   checking the committed ABIs still match the sources..."
  bash "$REPO/scripts/build-abi.sh" >/tmp/lpad-abi.log 2>&1 \
    || fail "ABI generation failed (see /tmp/lpad-abi.log)"
  git -C "$REPO" diff --quiet -- programs/artifacts/'*.json' \
    || fail "committed ABIs are stale - commit the regenerated programs/artifacts/*.json"
else
  echo "   (skipping the ABI drift check - no cli/target/release/lpad; build it to enable)"
fi
# A reproducible rebuild must not change the committed bytes. Only checked when
# Docker is available, since that is what makes the build deterministic.
if [ "${LPAD_VERIFY_GUESTS:-0}" = "1" ] && docker info >/dev/null 2>&1; then
  echo "   verifying the committed guests are reproducible..."
  bash "$REPO/scripts/build-guests.sh" >/tmp/lpad-guestbuild.log 2>&1 \
    || fail "guest rebuild failed (see /tmp/lpad-guestbuild.log)"
  git -C "$REPO" diff --quiet -- programs/artifacts/lpad \
    || fail "guest artifacts are stale - commit the rebuilt programs/artifacts/lpad/*.bin"
fi

echo "== 3. E2E vs in-process LEZ state (RISC0_DEV_MODE=1) =="
( cd "$PROG" && RISC0_DEV_MODE=1 cargo test -q -p integration_tests ) || fail "e2e"

echo "✓ CI gate passed"
