#!/usr/bin/env bash
# LPAD CI gate: unit tests (no zkVM) + guest-artifact freshness + in-process E2E.
# No sequencer involved: step 3 drives the LEZ state machine directly.
# Mirrors the ldex ci-e2e.sh. Run from the repo root.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
PROG="$REPO/programs"
export PATH="$HOME/.cargo/bin:$HOME/.risc0/bin:$PATH"
. "$HOME/.cargo/env" 2>/dev/null || true

fail() { echo "✗ $*" >&2; exit 1; }

echo "== 1. Unit tests (core + host, no zkVM) =="
( cd "$PROG" && RISC0_SKIP_BUILD=1 cargo test -q \
    -p bonding_curve_core -p bonding_curve_program \
    -p lbp_core -p lbp_program -p wlez_core -p wlez_program ) || fail "unit tests"

echo "== 1b. CLI tests (fmt unit + clap + offline/error e2e against the binary) =="
( cd "$REPO/cli" && RISC0_SKIP_BUILD=1 cargo test -q ) || fail "cli tests"

echo "== 2. Guest artifacts present =="
# The committed ELFs under artifacts/lpad/ ARE the deployed programs: their RISC0
# image ids are the on-chain program ids, baked into the SDK at compile time.
#
# This replaces the old IDL drift check. IDL *generation* went away with the LEZ
# v0.2.1 upgrade (it was a SPEL feature, and SPEL no longer exists), so
# artifacts/*-idl.json are now hand-maintained; see docs/UPGRADE-v0.2.0.md.
for g in bonding_curve lbp wlez; do
  [ -f "$PROG/artifacts/lpad/${g}.bin" ] || \
    fail "missing artifacts/lpad/${g}.bin - run scripts/build-guests.sh"
done
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
