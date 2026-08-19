# @paradoxcomputer/lpad-linux-x64

The Linux x86_64 build of the [`lpad`](https://github.com/paradoxcomputer/lpad)
CLI - a bonding-curve + LBP token launchpad for the Logos Execution Zone.

**Do not install this package directly.** Install
[`@paradoxcomputer/lpad`](https://www.npmjs.com/package/@paradoxcomputer/lpad)
instead; it lists this package as an optional dependency and npm picks the right
one for your machine from the `os`/`cpu` fields. This package contains no
launcher and puts nothing on your `PATH`.

## Requirements

- Linux, x86_64
- **glibc >= 2.34** (Ubuntu 22.04+, Debian 12+, Fedora 35+, RHEL 9+). The binary
  is built against that floor; on anything older the loader refuses it with a
  `GLIBC_2.34 not found` error. musl systems (Alpine) are not supported by this
  package - build from source there.

## Contents

    bin/lpad                      the CLI (~19 MB, statically self-contained
                                  apart from system libraries: the zkVM guest
                                  ELFs and proving circuits are compiled in)
    lib/libpcsclite.so.1          bundled third-party library, see below
    lib/LICENSE-libpcsclite.txt   its licence and attribution notice

## Why libpcsclite is in here

`ldd bin/lpad` lists exactly `libpcsclite.so.1`, `libgcc_s.so.1`, `libm.so.6`
and `libc.so.6`. The last three are on every glibc system; libpcsclite is not.
It is a link-time dependency of the upstream LEZ wallet crate, which pulls in
Keycard smartcard support unconditionally with no feature gate. lpad never
touches a card reader, so the library is dead weight at runtime - but the loader
still has to find it, and requiring `apt install libpcsclite1` before `npm i -g`
works would defeat the point of shipping on npm.

So the library ships here, and the `@paradoxcomputer/lpad` launcher prepends
this package's `lib/` to `LD_LIBRARY_PATH` for the child process only.

It is BSD-3-clause (plus ISC for one file); redistribution requires the
attribution in `lib/LICENSE-libpcsclite.txt`, which is why that file ships too.

## The binary is not in git

`bin/lpad` and `lib/libpcsclite.so.1` are build outputs, not source, so they are
`.gitignore`d in the repository. They are filled in immediately before
publishing - by the release job for a real release, or by
`npm/scripts/assemble-linux-x64.sh` when testing locally. A checkout of this
directory therefore looks empty; see `npm/README.md` in the repository for how
the whole thing fits together.

## Licence

lpad is dual-licensed MIT OR Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE`).
The bundled libpcsclite is covered separately by `lib/LICENSE-libpcsclite.txt`.
