#!/usr/bin/env bash
# Stamp a version onto every npm package, and refresh their licence copies.
#
# The version is DERIVED, not typed: cli/Cargo.toml is the single source of
# truth for what "lpad 0.2.0" means, and `lpad --version` prints that string, so
# the npm packages must not be able to disagree with it. Pass an explicit
# version only when a release deliberately publishes something other than the
# crate version (a re-publish of the same code as 0.2.0-1, say) - and expect to
# justify it.
#
# All four packages move together and the wrapper pins its optionalDependencies
# to the EXACT version: a launcher and a binary from different releases would
# still install happily and then lie about what they are.
#
# Usage:
#   bash npm/scripts/prepare-packages.sh            # version from cli/Cargo.toml
#   bash npm/scripts/prepare-packages.sh 0.3.0-rc.1 # explicit override
set -euo pipefail

NPM_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
REPO="$(cd "$NPM_DIR/.." && pwd)"
WRAPPER="$NPM_DIR/lpad"
PLATFORMS=(lpad-linux-x64 lpad-darwin-arm64 lpad-darwin-x64)

# Read the [package] version out of cli/Cargo.toml - the first `version =` after
# the [package] header, so a dependency's version string can never be picked up.
crate_version() {
  awk '/^\[package\]/{p=1;next} /^\[/{p=0}
       p && /^version[[:space:]]*=/ && match($0, /"[^"]*"/) {
         print substr($0, RSTART + 1, RLENGTH - 2); exit }' "$REPO/cli/Cargo.toml"
}

VERSION="${1:-$(crate_version)}"
[ -n "$VERSION" ] || { echo "!! could not read a version from $REPO/cli/Cargo.toml" >&2; exit 1; }
echo ">> version: $VERSION $([ $# -gt 0 ] && echo '(explicit override)' || echo '(from cli/Cargo.toml)')"

node - "$VERSION" "$WRAPPER" "${PLATFORMS[@]/#/$NPM_DIR/}" <<'NODE'
const fs = require('node:fs');
const path = require('node:path');
const [version, ...dirs] = process.argv.slice(2);
if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version)) {
  console.error(`!! ${version} is not a semver version npm will accept`);
  process.exit(1);
}
for (const dir of dirs) {
  const file = path.join(dir, 'package.json');
  const pkg = JSON.parse(fs.readFileSync(file, 'utf8'));
  pkg.version = version;
  if (pkg.optionalDependencies) {
    for (const dep of Object.keys(pkg.optionalDependencies)) pkg.optionalDependencies[dep] = version;
  }
  fs.writeFileSync(file, `${JSON.stringify(pkg, null, 2)}\n`);
  console.log(`   ${pkg.name} -> ${version}`);
}
NODE

# Every published package carries the licence it claims in its "license" field.
# Copied rather than symlinked because npm follows neither symlinks nor `../`
# out of the package directory when packing.
for d in "$WRAPPER" "${PLATFORMS[@]/#/$NPM_DIR/}"; do
  for f in LICENSE LICENSE-MIT LICENSE-APACHE; do
    cp "$REPO/$f" "$d/$f"
  done
done
echo ">> refreshed LICENSE, LICENSE-MIT, LICENSE-APACHE in all packages"
