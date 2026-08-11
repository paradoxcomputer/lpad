#!/usr/bin/env bash
# Build lpad's three zkVM guests reproducibly and stage them as committed
# artifacts.
#
# Since LEZ v0.2.1 guests are cross-compiled inside a pinned Docker container
# (`cargo risczero build`), so the resulting ELF - and therefore its RISC0 image
# id, which IS the on-chain program id - is byte-identical on every machine.
# That is what lets the ids be compile-time constants (see programs/src/lib.rs)
# instead of being read off disk at run time.
#
# Rebuild and commit `programs/artifacts/lpad/*.bin` whenever guest-visible code
# changes: the program crates, their `core` crates, `src/main.rs`, or the LEZ pin.
# Changing an image id means the program must be re-deployed and any sale/pool
# PDA derived from it moves, so treat a diff here as a consensus-level change.
#
# Requires: Docker running, `cargo-risczero` on PATH, and network egress from
# inside the container (it runs `cargo fetch --locked`).
#
# If the container cannot reach crates.io reliably, vendor the dependencies on the
# host - where they are already cached - so the in-container build is offline:
#
#   cargo vendor --manifest-path programs/Cargo.toml --versioned-dirs vendor \
#       > /tmp/vendor-config.toml
#   mkdir -p .cargo && cp /tmp/vendor-config.toml .cargo/config.toml
#   bash scripts/build-guests.sh
#   rm -rf .cargo/config.toml          # keep it out of host builds: the vendor dir
#                                      # only covers the programs workspace, so
#                                      # leaving it in place breaks sdk/ and cli/
#
# `vendor/` is gitignored. The config must live at the REPO ROOT (the container's
# working directory), not in programs/.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
PROG="$REPO/programs"
OUT="$PROG/artifacts/lpad"

# Must match the tag upstream builds its own artifacts with, or lpad's guests
# would be built by a different toolchain than the LEZ programs they compose
# with. Kept in sync with `RISC0_DOCKER_CONTAINER_TAG` in the LEZ Justfile.
RISC0_DOCKER_CONTAINER_TAG="${RISC0_DOCKER_CONTAINER_TAG:-r0.1.91.1}"

if ! docker info >/dev/null 2>&1; then
    echo "✗ Docker is not available - the reproducible guest build needs it." >&2
    echo "  Start Docker and retry." >&2
    exit 2
fi
if ! command -v cargo-risczero >/dev/null 2>&1; then
    echo "✗ cargo-risczero not found on PATH." >&2
    echo "  Install with: cargo install cargo-risczero --version 3.0.5" >&2
    exit 2
fi

echo "🔨 Building lpad guests (reproducible, container $RISC0_DOCKER_CONTAINER_TAG)"

# `artifacts` must stay OFF here: with it on, the library target would try to
# embed the very .bin files this command produces.
RISC0_DOCKER_CONTAINER_TAG="$RISC0_DOCKER_CONTAINER_TAG" \
CARGO_TARGET_DIR="$PROG/target/guests" \
    cargo risczero build \
        --no-default-features \
        --features programs \
        --manifest-path "$PROG/Cargo.toml"

BUILT="$PROG/target/guests/riscv32im-risc0-zkvm-elf/docker"
mkdir -p "$OUT"
for name in bonding_curve lbp wlez; do
    src="$BUILT/$name.bin"
    [ -f "$src" ] || { echo "✗ expected $src to exist" >&2; exit 1; }
    if [ -f "$OUT/$name.bin" ] && cmp -s "$src" "$OUT/$name.bin"; then
        echo "   $name.bin unchanged"
    else
        cp "$src" "$OUT/$name.bin"
        echo "   $name.bin UPDATED (program id changed - redeploy required)"
    fi
done

echo
echo "✓ Guests staged in programs/artifacts/lpad/"

# The ABIs embed the program ids, so a guest rebuild that changes an ELF leaves
# programs/artifacts/zonescan-program-schemas.json pointing at ids nobody
# deployed - and an indexer keyed on those ids just sees no transactions, with no
# error anywhere. Regenerate here so the two can never be committed out of step.
LPAD_BIN_PATH="${LPAD_BIN:-$REPO/cli/target/release/lpad}"
if [ -x "$LPAD_BIN_PATH" ]; then
    echo "  regenerating the ABIs against the new ids..."
    # The CLI embeds the ELFs at compile time, so it has to be rebuilt before it
    # can report the NEW ids - otherwise the ABIs would carry the old ones.
    ( cd "$REPO/cli" && RISC0_SKIP_BUILD=1 cargo build --release --bin lpad ) \
        || { echo "✗ could not rebuild the CLI; ABIs left untouched" >&2; exit 1; }
    bash "$REPO/scripts/build-abi.sh" || exit 1
else
    echo "  !! cli/target/release/lpad not built - ABIs NOT regenerated." >&2
    echo "     If any .bin changed, run this before committing:" >&2
    echo "       (cd cli && cargo build --release --bin lpad) && bash scripts/build-abi.sh" >&2
fi

echo
# Was `cargo run --bin lpad-program-ids`, a target that has never existed in this
# workspace - so the "Image ids" line only ever printed its own fallback.
echo "  Image ids:"
for g in bc lbp wlez; do
    if [ -x "$LPAD_BIN_PATH" ]; then
        printf '    %-5s %s\n' "$g" "$("$LPAD_BIN_PATH" program-id "$g" --json \
            | python3 -c 'import json,sys;print(json.load(sys.stdin)["program_id"])')"
    else
        echo "    (build the CLI, then: lpad program-id $g)"
    fi
done
