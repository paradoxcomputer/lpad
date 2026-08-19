# lpad

Command-line launchpad for the **Logos Execution Zone (LEZ)**: create and trade
bonding-curve tokens and Liquidity Bootstrapping Pools, publicly or with
shielded (zero-knowledge) buys.

```sh
npm i -g @paradoxcomputer/lpad
lpad --version
lpad bc quote --vt 2000000 --vc 50000 --fee-bps 100 --in 1000
```

Full command reference, wallet/sequencer setup and the on-chain program
documentation live in the repository:
<https://github.com/paradoxcomputer/lpad>.

## What gets installed

No compiler, no postinstall script, no download at install time. This package
is a small launcher; the actual binary comes from one of these, installed by npm
as an optional dependency and selected by its `os`/`cpu` fields:

| package | platform |
| --- | --- |
| `@paradoxcomputer/lpad-linux-x64` | Linux x86_64, glibc >= 2.34 |
| `@paradoxcomputer/lpad-darwin-arm64` | macOS, Apple silicon |
| `@paradoxcomputer/lpad-darwin-x64` | macOS, Intel |

npm downloads only the one that matches your machine. Because nothing is fetched
outside of the normal package install, this works offline, behind a proxy, with
`--ignore-scripts`, and in a container image built on another machine.

The binary is self-contained: the zkVM guest ELFs are compiled in from the
repository's committed reproducible artifacts, and the proving circuits are part
of the binary rather than a separate download. There is nothing else to install
after `npm i -g`.

## Requirements

- **Node 18+** - used only to launch the binary.
- **Linux:** x86_64 with **glibc >= 2.34** (Ubuntu 22.04+, Debian 12+,
  Fedora 35+, RHEL 9+). Alpine/musl is not supported; build from source there.
- **macOS:** Apple silicon or Intel.
- **Windows:** no native build. Use WSL2 with a glibc distribution.

Online commands additionally need a funded wallet and a sequencer endpoint - see
the repository README.

## Unsupported platform?

The launcher tells you so by name and points here. Build from source instead
(Rust toolchain required):

```sh
git clone https://github.com/paradoxcomputer/lpad.git
cd lpad && bash setup.sh && bash scripts/install-cli.sh
```

## Third-party notice

The Linux package bundles `libpcsclite.so.1` (pcsc-lite), redistributed
unmodified under the BSD-3-clause licence with one ISC-licensed file. It is
present because the upstream LEZ wallet links Keycard smartcard support
unconditionally - lpad never uses a smartcard - and bundling it is what lets
`npm i -g` work without `apt install libpcsclite1`. The full attribution notice
ships with that package as `lib/LICENSE-libpcsclite.txt`.

## Licence

lpad is dual-licensed **MIT OR Apache-2.0** at your option - see `LICENSE-MIT`
and `LICENSE-APACHE`.
