#!/usr/bin/env bash
# Local end-to-end test of the npm packaging. Publishes NOTHING.
#
# It assembles the linux-x64 package from an already-built
# cli/target/release/lpad plus the system libpcsclite, packs both packages with
# `npm pack`, installs the resulting tarballs into a throwaway prefix, and then
# runs the launcher the way a user would.
#
# Two things are worth testing beyond "it starts":
#
#   * the packed tarball really contains the binary. `files` allowlists and
#     ignore rules interact in ways that quietly produce a hollow package, and
#     the failure would only show up after publishing.
#   * the bundled libpcsclite is what makes it run. That is the entire reason
#     the Linux package is bigger than one file, so the test hides the system
#     copy (overlayfs in a user namespace) and shows the raw binary failing
#     where the launcher still works.
#
# Usage:  bash npm/smoke-test.sh
set -uo pipefail

NPM_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
REPO="$(cd "$NPM_DIR/.." && pwd)"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/lpad-npm-smoke.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

PASS=0; FAIL=0; SKIP=0
ok()   { PASS=$((PASS+1)); printf '   PASS  %s\n' "$1"; }
bad()  { FAIL=$((FAIL+1)); printf '   FAIL  %s\n' "$1"; [ $# -gt 1 ] && printf '         %s\n' "$2"; }
skip() { SKIP=$((SKIP+1)); printf '   SKIP  %s\n' "$1"; }
step() { printf '\n== %s\n' "$1"; }
check(){ # check <name> <expected> <actual>
  if [ "$2" = "$3" ]; then ok "$1"; else bad "$1" "expected [$2], got [$3]"; fi
}

# --- 1. assemble -------------------------------------------------------------
step "assembling packages"
bash "$NPM_DIR/scripts/prepare-packages.sh"   | sed 's/^/   /' || exit 1
bash "$NPM_DIR/scripts/assemble-linux-x64.sh" | sed 's/^/   /' || exit 1
VERSION="$(node -p "require('$NPM_DIR/lpad/package.json').version")"

# --- 2. pack -----------------------------------------------------------------
# `npm pack` applies exactly the rules a publish would, so what lands in these
# tarballs is what users would get.
step "npm pack (no publish)"
npm pack --pack-destination "$WORK" "$NPM_DIR/lpad" "$NPM_DIR/lpad-linux-x64" >/dev/null 2>"$WORK/pack.err" || {
  echo "npm pack failed:"; cat "$WORK/pack.err"; exit 1; }
WRAP_TGZ="$WORK/paradoxcomputer-lpad-$VERSION.tgz"
PLAT_TGZ="$WORK/paradoxcomputer-lpad-linux-x64-$VERSION.tgz"
ls -l "$WRAP_TGZ" "$PLAT_TGZ" | sed 's/^/   /'
# Listed to files rather than re-read per assertion: `tar | grep -q` makes tar
# die of SIGPIPE the moment grep matches, which pipefail then reports as a
# failed check for every file that is NOT last in the archive.
tar tzf "$PLAT_TGZ" > "$WORK/plat.list"; tar tzf "$WRAP_TGZ" > "$WORK/wrap.list"
sed 's/^/   /' "$WORK/plat.list"

step "tarball contents"
for want in package/bin/lpad package/lib/libpcsclite.so.1 package/lib/LICENSE-libpcsclite.txt package/LICENSE-MIT package/LICENSE-APACHE; do
  if grep -qx -- "$want" "$WORK/plat.list"; then ok "platform tarball ships $want"
  else bad "platform tarball ships $want" "missing - check the files allowlist and .npmignore"; fi
done
for want in package/bin/lpad.js package/README.md package/LICENSE-MIT; do
  if grep -qx -- "$want" "$WORK/wrap.list"; then ok "wrapper tarball ships $want"; else bad "wrapper tarball ships $want"; fi
done

# --- 3. install --------------------------------------------------------------
# --offline proves the point of the design: with both tarballs on disk the
# install never reaches the network, so it also works behind a proxy or in a
# sealed build. --ignore-scripts proves there is no postinstall doing the work.
step "offline install into a throwaway prefix"
PREFIX="$WORK/prefix"
npm install --global --prefix "$PREFIX" --offline --ignore-scripts --no-audit --no-fund \
  "$PLAT_TGZ" "$WRAP_TGZ" 2>&1 | sed 's/^/   /' || { bad "npm install"; }
LPAD="$PREFIX/bin/lpad"
if [ -x "$LPAD" ]; then ok "installed launcher on PATH: $LPAD"; else bad "installed launcher on PATH" "$LPAD missing"; exit 1; fi

# --- 4. does it run ----------------------------------------------------------
step "running the CLI through the launcher"
OUT="$("$LPAD" --version 2>&1)"; RC=$?
check "lpad --version exit status" "0" "$RC"
check "lpad --version output" "lpad $VERSION" "$OUT"
[ "$VERSION" = "$(awk '/^\[package\]/{p=1;next} /^\[/{p=0} p && /^version/ && match($0,/"[^"]*"/){print substr($0,RSTART+1,RLENGTH-2);exit}' "$REPO/cli/Cargo.toml")" ] \
  && ok "npm version matches the crate version in cli/Cargo.toml" \
  || bad "npm version matches the crate version in cli/Cargo.toml"

# A real command, not just --version: this one is pure local math, so it needs
# no wallet or sequencer, and it proves the binary's own code ran.
QUOTE="$("$LPAD" bc quote --vt 2000000 --vc 50000 --fee-bps 100 --in 1000 2>&1)"; RC=$?
check "lpad bc quote exit status" "0" "$RC"
printf '   %s\n' "$QUOTE" | head -8
"$LPAD" definitely-not-a-command >/dev/null 2>&1; RC=$?
if [ "$RC" -ne 0 ]; then ok "a failing command exits non-zero (got $RC)"; else bad "a failing command exits non-zero"; fi

# --- 5. the bundled libpcsclite ---------------------------------------------
# Hide the system copy with an overlay mount inside a user namespace: the
# overlay exists only for the processes in that namespace, so nothing on the
# machine is modified and no root is needed. If the kernel or the container this
# runs in forbids that, skip rather than fail - the packaging is not what broke.
step "with the system libpcsclite hidden"
OVL="$WORK/ovl"; mkdir -p "$OVL/up" "$OVL/work"
SYSLIBDIR="$(dirname "$(ldd "$REPO/cli/target/release/lpad" | awk '/libpcsclite/{print $3;exit}')")"
HIDE="mount -t overlay lpad-smoke -o lowerdir=$SYSLIBDIR,upperdir=$OVL/up,workdir=$OVL/work,userxattr $SYSLIBDIR && rm -f $SYSLIBDIR/libpcsclite.so*"
if ! unshare --map-root-user --mount -- sh -c "$HIDE" >"$WORK/ns.log" 2>&1; then
  skip "hiding $SYSLIBDIR/libpcsclite.so.1 (no unprivileged overlay here: $(tail -1 "$WORK/ns.log"))"
else
  RAW="$(unshare --map-root-user --mount -- sh -c "$HIDE >/dev/null 2>&1; $REPO/cli/target/release/lpad --version 2>&1; echo rc=\$?" 2>&1)"
  case "$RAW" in
    *"libpcsclite.so.1: cannot open shared object file"*)
      ok "control: the raw binary really does fail without libpcsclite"
      printf '   %s\n' "$RAW" ;;
    *) bad "control: the raw binary really does fail without libpcsclite" "got: $RAW" ;;
  esac
  BUNDLED="$(unshare --map-root-user --mount -- sh -c "$HIDE >/dev/null 2>&1; $LPAD --version 2>&1" 2>&1)"
  check "the launcher still runs (bundled copy is used)" "lpad $VERSION" "$BUNDLED"
  # ...and prove which file satisfied the dependency rather than inferring it.
  LOADED="$(unshare --map-root-user --mount -- sh -c "$HIDE >/dev/null 2>&1; LD_DEBUG=libs $LPAD --version 2>&1" 2>&1 \
            | sed -n 's/.*calling init: \(.*libpcsclite[^ ]*\)/\1/p' | head -1)"
  case "$LOADED" in
    "$PREFIX"/lib/node_modules/@paradoxcomputer/lpad-linux-x64/lib/libpcsclite.so.1)
      ok "loaded libpcsclite came from the installed package"
      printf '   %s\n' "$LOADED" ;;
    *) bad "loaded libpcsclite came from the installed package" "got: ${LOADED:-<none>}" ;;
  esac
fi

# --- 6. launcher behaviour ---------------------------------------------------
# Exit codes, signals and argv are properties of the launcher, not of lpad, and
# lpad has no command that blocks long enough to signal reliably. So point the
# launcher at a stub platform package: same layout, scriptable behaviour.
step "launcher semantics (stub platform package)"
STUB="$WORK/stub"
mkdir -p "$STUB/node_modules/@paradoxcomputer/lpad-linux-x64/bin" "$STUB/node_modules/@paradoxcomputer/lpad-linux-x64/lib"
cp -r "$NPM_DIR/lpad" "$STUB/node_modules/@paradoxcomputer/"
node -e "const f='$STUB/node_modules/@paradoxcomputer/lpad-linux-x64/package.json';
  require('fs').writeFileSync(f, JSON.stringify({name:'@paradoxcomputer/lpad-linux-x64',version:'$VERSION'}))"
: > "$STUB/node_modules/@paradoxcomputer/lpad-linux-x64/lib/libpcsclite.so.1"
cat > "$STUB/node_modules/@paradoxcomputer/lpad-linux-x64/bin/lpad" <<'STUBEOF'
#!/usr/bin/env bash
case "${1:-}" in
  ldpath) printf '%s\n' "${LD_LIBRARY_PATH:-<unset>}" ;;
  argv)   shift; for a in "$@"; do printf '[%s]' "$a"; done; printf '\n' ;;
  code)   exit "$2" ;;
  block)  trap 'exit 99' TERM; sleep 30 ;;
  # exec, so the process the launcher signals is the sleep itself. A bash that
  # is waiting on a child defers an untrapped signal until that child returns,
  # which would test bash's semantics instead of the launcher's.
  wait)   exec sleep 30 ;;
esac
STUBEOF
chmod +x "$STUB/node_modules/@paradoxcomputer/lpad-linux-x64/bin/lpad"
STUB_LAUNCHER="$STUB/node_modules/@paradoxcomputer/lpad/bin/lpad.js"
STUB_LIB="$STUB/node_modules/@paradoxcomputer/lpad-linux-x64/lib"

check "argv is forwarded verbatim (spaces, --, empty)" \
  "[a b][--][][-x]" "$(node "$STUB_LAUNCHER" argv 'a b' -- '' -x)"
node "$STUB_LAUNCHER" code 42 >/dev/null 2>&1
check "the child's exit code is propagated" "42" "$?"
check "LD_LIBRARY_PATH gets the bundled lib dir" "$STUB_LIB" "$(node "$STUB_LAUNCHER" ldpath)"
check "an existing LD_LIBRARY_PATH is prepended to, not clobbered" \
  "$STUB_LIB:/opt/sentinel" "$(LD_LIBRARY_PATH=/opt/sentinel node "$STUB_LAUNCHER" ldpath)"

for SIG in INT TERM; do
  node "$STUB_LAUNCHER" wait & LP=$!
  sleep 0.7; kill -"$SIG" "$LP" 2>/dev/null; wait "$LP" 2>/dev/null; RC=$?
  EXPECT=$((128 + $(kill -l "$SIG")))
  check "SIG$SIG reaches the binary and comes back as $EXPECT" "$EXPECT" "$RC"
done
# A binary that handles the signal itself must keep its own exit code.
node "$STUB_LAUNCHER" block & LP=$!
sleep 0.7; kill -TERM "$LP" 2>/dev/null; wait "$LP" 2>/dev/null
check "a binary that traps SIGTERM keeps its own exit code" "99" "$?"

# The message an unsupported user actually sees. Faking the arch is the only way
# to reach it on a supported machine.
step "unsupported platform message"
MSG="$(node -e "Object.defineProperty(process,'arch',{value:'mips64el'}); require('$STUB_LAUNCHER')" 2>&1)"; RC=$?
if [ "$RC" -ne 0 ] && printf '%s' "$MSG" | grep -q 'linux-mips64el'; then
  ok "names the unmatched platform and exits non-zero"
  printf '%s\n' "$MSG" | sed 's/^/   | /'
else
  bad "names the unmatched platform and exits non-zero" "rc=$RC: $MSG"
fi

# The other half of that message: a supported platform whose package is simply
# absent, which is what `--omit=optional` leaves behind.
LONELY="$WORK/lonely"; mkdir -p "$LONELY/node_modules/@paradoxcomputer"
cp -r "$NPM_DIR/lpad" "$LONELY/node_modules/@paradoxcomputer/"
MSG2="$(node "$LONELY/node_modules/@paradoxcomputer/lpad/bin/lpad.js" 2>&1)"; RC=$?
if [ "$RC" -ne 0 ] && printf '%s' "$MSG2" | grep -q -- '--omit=optional'; then
  ok "a supported platform with the package missing is diagnosed differently"
  printf '%s\n' "$MSG2" | sed 's/^/   | /'
else
  bad "a supported platform with the package missing is diagnosed differently" "rc=$RC: $MSG2"
fi

# --- 7. verdict --------------------------------------------------------------
printf '\n== %d passed, %d failed, %d skipped\n' "$PASS" "$FAIL" "$SKIP"
[ "$FAIL" -eq 0 ] || exit 1
printf 'Nothing was published; the tarballs lived in %s and are gone now.\n' "$WORK"
