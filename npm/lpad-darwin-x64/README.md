# @paradoxcomputer/lpad-darwin-x64

The macOS (Intel, x86_64) build of the
[`lpad`](https://github.com/paradoxcomputer/lpad) CLI - a bonding-curve + LBP
token launchpad for the Logos Execution Zone.

**Do not install this package directly.** Install
[`@paradoxcomputer/lpad`](https://www.npmjs.com/package/@paradoxcomputer/lpad)
instead; it lists this package as an optional dependency and npm picks the right
one for your machine from the `os`/`cpu` fields. This package contains no
launcher and puts nothing on your `PATH`.

## Contents

    bin/lpad    the CLI (self-contained apart from system libraries: the zkVM
                guest ELFs and proving circuits are compiled in)

No bundled libraries: on macOS the smartcard dependency that forces a bundled
`libpcsclite.so.1` into the Linux package is satisfied by the system PCSC
framework, which is part of the OS.

## The binary is not in git

`bin/lpad` is a build output, not source, so it is `.gitignore`d in the
repository and filled in by the release job immediately before publishing. A
checkout of this directory therefore looks empty; see `npm/README.md` in the
repository for how the whole thing fits together.

## Licence

Dual-licensed MIT OR Apache-2.0 - see `LICENSE-MIT` and `LICENSE-APACHE`.
