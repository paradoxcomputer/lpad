#!/usr/bin/env bash
# Fill in npm/lpad-linux-x64 from a locally built binary, so the package can be
# packed and run without a release.
#
# This is the local stand-in for what the release job does: the job builds the
# binary on a glibc 2.34 machine (or in a container of one) and copies it plus
# libpcsclite.so.1 into the same two places. Doing it with a script rather than
# by hand keeps the local test honest - if the packaging is wrong, it is wrong
# the same way here as it would be in a release.
#
# Usage:
#   bash npm/scripts/assemble-linux-x64.sh
#   LPAD_BINARY=/path/to/lpad LPAD_LIBPCSCLITE=/path/to/libpcsclite.so.1 \
#     bash npm/scripts/assemble-linux-x64.sh
set -euo pipefail

NPM_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
REPO="$(cd "$NPM_DIR/.." && pwd)"
PKG="$NPM_DIR/lpad-linux-x64"
BINARY="${LPAD_BINARY:-$REPO/cli/target/release/lpad}"

[ -x "$BINARY" ] || {
  echo "!! no lpad binary at $BINARY" >&2
  echo "   build one first:  bash scripts/install-cli.sh   (or cargo build --release --bin lpad in cli/)" >&2
  exit 1
}

# Find libpcsclite by asking the loader what this exact binary resolves it to,
# rather than guessing a multiarch path. If it comes back unresolved the binary
# would not run here either, so bail with the useful message instead of copying
# nothing and failing much later at `lpad --version`.
LIB="${LPAD_LIBPCSCLITE:-$(ldd "$BINARY" | awk '/libpcsclite\.so\.1/ {print $3; exit}')}"
[ -n "$LIB" ] && [ -e "$LIB" ] || {
  echo "!! could not find libpcsclite.so.1 for $BINARY" >&2
  echo "   ldd says:" >&2; ldd "$BINARY" | sed 's/^/     /' >&2
  echo "   install it (apt install libpcsclite1) or set LPAD_LIBPCSCLITE=/path/to/libpcsclite.so.1" >&2
  exit 1
}

mkdir -p "$PKG/bin" "$PKG/lib"
install -m 0755 "$BINARY" "$PKG/bin/lpad"
# -L: the system path is normally a symlink to libpcsclite.so.1.0.0, and a
# dangling symlink is all that would survive being packed into a tarball.
cp -L "$LIB" "$PKG/lib/libpcsclite.so.1"
chmod 0755 "$PKG/lib/libpcsclite.so.1"

echo ">> bin/lpad             <- $BINARY"
echo ">> lib/libpcsclite.so.1 <- $LIB"
ls -l "$PKG/bin/lpad" "$PKG/lib/libpcsclite.so.1" | sed 's/^/   /'

# The glibc floor is a property of the build machine, not of this script, so
# report it rather than enforce it: a binary assembled on a newer distro will
# install fine from npm and then fail to start for half the users.
FLOOR="$(objdump -T "$PKG/bin/lpad" 2>/dev/null | sed -n 's/.*GLIBC_\([0-9.]*\).*/\1/p' \
         | sort -t. -k1,1n -k2,2n -k3,3n | tail -1 || true)"
[ -n "$FLOOR" ] && echo ">> highest glibc symbol required by this binary: GLIBC_$FLOOR (packages advertise >= 2.34)"
