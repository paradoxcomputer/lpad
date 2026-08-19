# npm packaging for the `lpad` CLI

This directory publishes the Rust binary built from `cli/` to npm, so that

```sh
npm i -g @paradoxcomputer/lpad
lpad --version
```

is a complete install on a machine with no Rust toolchain. It is an alternative
to `scripts/install-cli.sh` (which builds from source), not a replacement for
it: both end up running the same `lpad` binary, and both assume nothing else has
to be downloaded afterwards, because since the LEZ v0.2.1 upgrade the zkVM guest
ELFs are compiled into the binary and the proving circuits are no longer a
separate artifact.

Nothing here publishes anything. `npm publish` is the release job's business.

## Layout

    npm/
      lpad/                  @paradoxcomputer/lpad          - wrapper, the only
                                                              package users install
        bin/lpad.js          the launcher (this is the whole implementation)
      lpad-linux-x64/        @paradoxcomputer/lpad-linux-x64
        bin/lpad             filled in at release time
        lib/libpcsclite.so.1 filled in at release time
        lib/LICENSE-libpcsclite.txt
      lpad-darwin-arm64/     @paradoxcomputer/lpad-darwin-arm64
      lpad-darwin-x64/       @paradoxcomputer/lpad-darwin-x64
      scripts/prepare-packages.sh    stamp the version, refresh licences
      scripts/assemble-linux-x64.sh  fill in the Linux package locally
      smoke-test.sh                  end-to-end test, publishes nothing

## How it works, and why this way

**One package per platform, selected by npm.** This is the pattern esbuild and
swc use. The wrapper lists every platform package in `optionalDependencies`;
each platform package declares `os` and `cpu`, so npm installs the one that
matches and quietly skips the rest. A user downloads one platform tarball, not all
of them.

**No postinstall script.** The alternative pattern - one package plus a
postinstall that downloads a binary from GitHub Releases - fails exactly where
it hurts: offline installs, corporate proxies, `npm ci --ignore-scripts` in
hardened CI, and images built on one machine to run on another. Because
everything arrives as ordinary package content, this layout works in all of
those, and there is no window where a half-installed package has no binary.

**The launcher spawns rather than re-implements.** `bin/lpad.js` resolves the
platform package with `require.resolve` (so it works with hoisted
`node_modules`, nested ones, pnpm symlinks and global prefixes alike), then runs
the binary with the real stdio, forwards argv untouched, forwards SIGINT /
SIGTERM / SIGHUP / SIGQUIT to the child, and reproduces the child's exit status
- re-raising the signal on itself when the binary died from one, so `$?` is 130
after a Ctrl-C rather than a bare 1. A launcher that swallows an exit code or a
signal breaks every script that wraps the CLI, which is most of them.

**Exact version pins.** `optionalDependencies` pin `0.2.0`, not `^0.2.0`: a
launcher and a binary from different releases would install happily and then
disagree about what `lpad --version` means.

**The Linux package bundles libpcsclite.** See below.

## Runtime requirements

- **Node 18+**, used only to launch the binary.
- **Linux x86_64 with glibc >= 2.34** - Ubuntu 22.04+, Debian 12+, Fedora 35+,
  RHEL 9+. That floor is the highest versioned glibc symbol the binary needs
  (`assemble-linux-x64.sh` prints it, so a build from a newer distro that would
  raise the floor is visible before publishing, not after). musl/Alpine is not
  supported; build from source there.
- **macOS**, Apple silicon or Intel.
- **Windows**: no native build. WSL2 with a glibc distribution.

## The bundled libpcsclite

`ldd` on the Linux binary lists exactly four libraries:

    libpcsclite.so.1  libgcc_s.so.1  libm.so.6  libc.so.6

The last three exist on every glibc system. `libpcsclite.so.1` does not - it is
a separate package (`libpcsclite1`) that plenty of machines and most container
images lack. It is there because the upstream LEZ wallet crate links Keycard
smartcard support unconditionally (`keycard_wallet.workspace = true`, with no
feature gate). lpad never opens a card reader, so this is purely a link-time
fact; don't spend time trying to drop it from this side, it has to change
upstream.

Requiring `apt install libpcsclite1` before `npm i -g` would defeat the point of
publishing to npm, so `lpad-linux-x64` ships its own copy in `lib/` and the
launcher prepends that directory to `LD_LIBRARY_PATH` **for the child process
only**, appending any value the user already had rather than replacing it. An
`LD_LIBRARY_PATH` set for other reasons keeps working, and the user's shell is
never modified.

`LD_LIBRARY_PATH` rather than a patched `RPATH`/`$ORIGIN`: it needs no patchelf
in the release job, survives npm unpacking the file anywhere, and - when a load
does go wrong - it is visible in `env` and reproducible by hand. The smoke test
proves the bundled copy is what makes the CLI run by hiding the system one.

**Licence.** The shipped `libpcsclite.so.1` is BSD-3-clause, plus ISC for
`src/simclist.[ch]`. Both are permissive and both require the attribution notice
to travel with the binary, which is why `lib/LICENSE-libpcsclite.txt` is part of
the package's `files` allowlist. (pcsc-lite's GPL-3+ files cover `debian/*` and
`src/spy/*` - the packaging and the separate spy shim - and none of that code is
in the `.so`.) macOS needs no bundled library at all: the smartcard dependency
is satisfied by the system PCSC framework.

## Versioning

`cli/Cargo.toml` is the source of truth. `lpad --version` prints the crate
version, so the npm version is derived from it rather than typed:

```sh
bash npm/scripts/prepare-packages.sh          # reads cli/Cargo.toml (0.2.0 today)
bash npm/scripts/prepare-packages.sh 0.3.0    # explicit override
```

It stamps all four `package.json` files and re-pins the wrapper's
`optionalDependencies` in one go, and refreshes each package's copy of
`LICENSE`, `LICENSE-MIT` and `LICENSE-APACHE` from the repository root (npm
cannot pack files from outside a package directory, so each package carries its
own copies).

A release therefore bumps the crate version first and runs the script second; it
never edits an npm `version` field by hand. If a package ever has to be
re-published without a code change (a bad tarball, a metadata fix), that is what
the explicit override is for - use a prerelease or build suffix, and expect npm
to refuse a version that already exists.

## Binaries are not committed

`lpad-*/bin/lpad` and `lpad-linux-x64/lib/*.so*` are build outputs and are
`.gitignore`d. A fresh checkout of this directory has no binaries in it; that is
correct, not a broken clone. They are filled in immediately before publishing.

One trap worth knowing about: `npm pack` falls back to a package's `.gitignore`
when there is no `.npmignore`, which would exclude the very binary the package
exists to ship. Each platform package therefore contains an (almost) empty
`.npmignore`, and `smoke-test.sh` asserts that the packed tarball really
contains `bin/lpad` - a hollow package is not something to discover after
publishing.

## Testing it locally

`cli/target/release/lpad` must already be built (`bash scripts/install-cli.sh`
does that). Then:

```sh
bash npm/smoke-test.sh
```

It assembles the Linux package from that binary plus the system libpcsclite,
`npm pack`s both packages, installs the tarballs into a throwaway prefix with
`--offline --ignore-scripts`, and then checks that:

- the tarballs contain the binary, the bundled library and the licences;
- `lpad --version` through the installed launcher prints the crate version, and
  a real command (`lpad bc quote`) produces output;
- **with the system libpcsclite hidden** (an overlay mount inside a user
  namespace - no root, nothing on the machine is changed) the raw binary fails
  with `cannot open shared object file` while the launcher still runs, and
  `LD_DEBUG=libs` confirms the library that loaded is the one inside the
  installed package;
- exit codes, argv, SIGINT/SIGTERM and the `LD_LIBRARY_PATH` prepend behave, via
  a stub platform package (lpad itself has no command that blocks long enough to
  signal);
- both "no build for this platform" messages are the ones a user would want.

The hidden-library test skips itself, loudly, where unprivileged user namespaces
or overlayfs are unavailable. Nothing in the script publishes, links globally or
touches the user's npm configuration - the throwaway prefix is deleted on exit.

To try the packages by hand, `npm pack` them and install the tarballs into a
prefix the same way, or `npm link` the wrapper while the platform package sits
in its `node_modules`.

## What a release does

1. Bump `cli/Cargo.toml` (the owner's decision - the CHANGELOG argues the next
   release is a minor bump; this directory has no opinion).
2. Build `lpad` for each platform on a machine or container with the oldest
   glibc still supported (a build on a newer distro raises the floor and breaks
   users who were fine yesterday), and for macOS on the matching architecture.
3. Drop each binary into `npm/lpad-<platform>/bin/lpad`, and the Linux
   libpcsclite into `npm/lpad-linux-x64/lib/libpcsclite.so.1`
   (`assemble-linux-x64.sh` is exactly this step for the local case).
4. `bash npm/scripts/prepare-packages.sh`.
5. `bash npm/smoke-test.sh` on the Linux runner.
6. Publish the platform packages **first**, then the wrapper. Publishing the
   wrapper first leaves a window where `npm i -g @paradoxcomputer/lpad` resolves
   nothing installable and every install prints the launcher's error.

## Licence

lpad is dual-licensed MIT OR Apache-2.0. The bundled libpcsclite is covered by
`lpad-linux-x64/lib/LICENSE-libpcsclite.txt`.
