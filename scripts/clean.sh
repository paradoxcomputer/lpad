#!/usr/bin/env bash
# Reclaim build-cache disk. Deletes only regenerable output.
#
# Why this gets big fast: `programs/`, `sdk/` and `cli/` are three separate
# workspaces, so each compiles the whole LEZ + risc0 + ark dependency tree
# independently, in both debug and release. Left alone across a version bump
# (which doubles the dependency sets), the three target/ dirs reached 86 GB
# against ~2 MB of source.
#
# Nothing here is precious:
#   * the deployed guest ELFs live in programs/artifacts/lpad/ and are COMMITTED
#     (target/guests only holds a redundant copy of them)
#   * ~/.cargo/{registry,git} are NOT touched, so a rebuild recompiles but
#     re-downloads nothing - which matters on a flaky connection
#
#   bash scripts/clean.sh          # target/ dirs + vendor/
#   bash scripts/clean.sh --dry-run
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
DRY=0
for a in "$@"; do case "$a" in
  --dry-run|-n) DRY=1;;
  *) echo "unknown arg: $a (use --dry-run)" >&2; exit 2;;
esac; done

# `vendor/` is only needed to build guests without network egress; it is
# regenerable with `cargo vendor` (see scripts/build-guests.sh).
TARGETS=(
  "$REPO/programs/target"
  "$REPO/sdk/target"
  "$REPO/cli/target"
  "$REPO/vendor"
)

before=$(du -sm "$REPO" 2>/dev/null | cut -f1)
total=0
for t in "${TARGETS[@]}"; do
  [ -e "$t" ] || continue
  sz=$(du -sm "$t" 2>/dev/null | cut -f1)
  total=$((total + sz))
  printf '  %-46s %6s MB\n' "${t#"$REPO"/}" "$sz"
  [ "$DRY" = 0 ] && rm -rf "$t"
done

if [ "$DRY" = 1 ]; then
  echo
  echo "would free ~$((total / 1024)) GB (dry run; nothing deleted)"
  exit 0
fi

after=$(du -sm "$REPO" 2>/dev/null | cut -f1)
echo
echo "✓ freed $(((before - after) / 1024)) GB   ($before MB -> $after MB)"
echo
echo "Rebuild when needed (no downloads required - the cargo caches are intact):"
echo "  cd $REPO/programs && cargo test --workspace"
echo "Guests only need rebuilding if guest code changed:"
echo "  bash $REPO/scripts/build-guests.sh"
