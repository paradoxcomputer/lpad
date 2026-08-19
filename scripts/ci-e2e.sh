#!/usr/bin/env bash
# LPAD CI gate: unit tests (no zkVM) + chain parity + artifact/ABI drift +
# in-process E2E. No sequencer involved: step 3 drives the LEZ state machine
# directly, and the parity checks compare committed constants, not live RPC.
# Mirrors the ldex ci-e2e.sh. Run from the repo root.
#
# None of it needs the risc0 toolchain, so .github/workflows/ci.yml now runs the
# same tests on GitHub-hosted runners and is what actually gates a PR. This stays
# the one command to run locally, and the only place the guest rebuild is wired
# up (LPAD_VERIFY_GUESTS=1, which needs Docker).
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

echo "== 1c. SDK chain-parity (the LEZ pin vs the ids read off the networks) =="
# The one check that decides whether this build can transact at all: the SDK
# asserts the pinned LEZ version's built-in token / authenticated_transfer /
# clock image ids equal constants read off the target sequencers when this
# release was prepared. It is a comparison of committed constants, not a live
# query. If they disagree, every path that touches a built-in program is
# rejected on chain - and the symptom is an indistinguishable "All pollers
# failed", not an error.
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
for g in bonding_curve lbp wlez; do
  [ -f "$PROG/artifacts/lpad/${g}.bin" ] || \
    fail "missing artifacts/lpad/${g}.bin - run scripts/build-guests.sh"
done

# The per-program ABIs are generated from source by scripts/build-abi.sh, so a
# stale one means the committed JSON no longer describes the deployed programs -
# and an indexer decoding against it would mis-read every instruction, silently.
# This comment used to promise the check without performing it. Then it promised
# it and skipped: build-abi.sh reads the program ids out of a built CLI, so the
# check was conditional on cli/target/release/lpad existing - which on a clean
# checkout it never does, because nothing here builds it. On a runner it took the
# skip branch and exited 0 in exactly the situation it was written for.
#
# It is unconditional now. Step 1b above cannot run tests/cli.rs without first
# building cli/target/debug/lpad (the test drives CARGO_BIN_EXE_lpad), and the
# program ids are compile-time constants, so the debug binary prints the same
# ones as the release build. A missing binary here means step 1b did not do what
# it says.
ABI_BIN=""
for cand in "${LPAD_BIN:-}" "$REPO/cli/target/release/lpad" "$REPO/cli/target/debug/lpad"; do
  if [ -n "$cand" ] && [ -x "$cand" ]; then ABI_BIN="$cand"; break; fi
done
[ -n "$ABI_BIN" ] || fail "no lpad binary to read program ids from - step 1b should have\
 left one at cli/target/debug/lpad; set LPAD_BIN if yours is elsewhere (CARGO_TARGET_DIR)"
echo "   checking the committed ABIs still match the sources (${ABI_BIN#"$REPO/"})..."
LPAD_BIN="$ABI_BIN" bash "$REPO/scripts/build-abi.sh" >/tmp/lpad-abi.log 2>&1 \
  || fail "ABI generation failed (see /tmp/lpad-abi.log)"
git -C "$REPO" diff --quiet -- programs/artifacts/'*.json' \
  || fail "committed ABIs are stale - commit the regenerated programs/artifacts/*.json"
# A reproducible rebuild must not change the committed bytes. Opt-in, because it
# needs the pinned Docker toolchain that is what makes the build deterministic -
# but asking for it and not getting it is a failure, not a skip. The condition
# used to be `LPAD_VERIFY_GUESTS=1 && docker info`, which meant the e2e job whose
# only purpose is this check passed green, and silently, on any machine where
# Docker was down.
if [ "${LPAD_VERIFY_GUESTS:-0}" = "1" ]; then
  docker info >/dev/null 2>&1 || fail "LPAD_VERIFY_GUESTS=1 but Docker is not usable -\
 the guest rebuild is only byte-reproducible inside the pinned container, so there is\
 nothing this can check; start Docker, or unset LPAD_VERIFY_GUESTS to skip on purpose"
  echo "   verifying the committed guests are reproducible..."
  bash "$REPO/scripts/build-guests.sh" >/tmp/lpad-guestbuild.log 2>&1 \
    || fail "guest rebuild failed (see /tmp/lpad-guestbuild.log)"
  git -C "$REPO" diff --quiet -- programs/artifacts/lpad \
    || fail "guest artifacts are stale - commit the rebuilt programs/artifacts/lpad/*.bin"
else
  echo "   (guest reproducibility not checked - set LPAD_VERIFY_GUESTS=1, Docker running)"
fi

echo "== 3. E2E vs in-process LEZ state (RISC0_DEV_MODE=1) =="
( cd "$PROG" && RISC0_DEV_MODE=1 cargo test -q -p integration_tests ) || fail "e2e"

echo "✓ CI gate passed"
