#!/usr/bin/env node
'use strict';

// lpad launcher.
//
// `@paradoxcomputer/lpad` ships no binary of its own. Every platform's binary
// lives in its own package (`@paradoxcomputer/lpad-linux-x64`, ...) listed in
// this package's `optionalDependencies`. Each of those declares `os`/`cpu`, so
// npm installs exactly the one that matches the machine and silently skips the
// rest - which is why there is no postinstall script here. Nothing is
// downloaded, decompressed or chmod'ed at install time, so `npm i -g` works
// offline, behind a corporate proxy, with `--ignore-scripts`, and inside an
// image built on one machine and run on another.
//
// All this file does is find that package, run the binary inside it, and stand
// in for it faithfully: same stdio, same argv, same exit status, same signals.

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawn } = require('node:child_process');

const SCOPE = '@paradoxcomputer';
const PLATFORM_KEY = `${process.platform}-${process.arch}`;
const PLATFORM_PKG = `${SCOPE}/lpad-${PLATFORM_KEY}`;

// Kept in sync with the directories under npm/ and with the release job's
// build matrix. Listed here only so the failure message can tell the user what
// *is* supported.
// Intel macOS is deliberately absent. GitHub is retiring the macos-13 image and
// the runner never became available, so no Intel binary has ever been built -
// and release.yml's own rule is to drop the entry rather than publish a package
// that does not run. An Intel mac therefore falls into the "no prebuilt binary"
// branch below, which tells the truth and points at building from source.
const SUPPORTED = {
  'linux-x64': 'Linux x86_64, glibc >= 2.34 (Ubuntu 22.04+, Debian 12+)',
  'darwin-arm64': 'macOS on Apple silicon',
};

function die(lines) {
  process.stderr.write(`${lines.join('\n')}\n`);
  process.exit(1);
}

// --- 1. locate the platform package ----------------------------------------
// require.resolve() rather than a hand-built '../lpad-linux-x64' path: it
// follows whatever layout the package manager produced - hoisted node_modules,
// a nested one, a pnpm store symlink, a global prefix - instead of assuming
// npm's flat default. The platform packages deliberately declare no "exports"
// field so that this subpath stays resolvable.
let platformPkgJson;
try {
  platformPkgJson = require.resolve(`${PLATFORM_PKG}/package.json`);
} catch {
  // Two very different situations reach this point, and conflating them sends
  // people down the wrong path: either the platform has no build at all, or it
  // has one and the install skipped it.
  const lines = [`lpad: no prebuilt binary for ${PLATFORM_KEY}.`, '', `  ${PLATFORM_PKG} is not installed.`, ''];
  if (SUPPORTED[PLATFORM_KEY]) {
    lines.push(
      `  There IS a build for ${PLATFORM_KEY} (${SUPPORTED[PLATFORM_KEY]}),`,
      '  so the package was skipped rather than missing. The usual causes are',
      '  installing with --omit=optional / --no-optional, and lockfiles generated',
      '  on another OS.',
      '',
      '  Reinstall with optional dependencies enabled:',
      '    npm i -g @paradoxcomputer/lpad',
      '  or install the platform package directly:',
      `    npm i -g ${PLATFORM_PKG}`,
    );
  } else {
    lines.push('  There is no prebuilt binary for this platform. Builds exist for:');
    for (const [key, what] of Object.entries(SUPPORTED)) lines.push(`    ${key.padEnd(13)} ${what}`);
    lines.push('');
    if (process.platform === 'win32') {
      lines.push('  Windows has no native build - use WSL2 with a glibc distribution.', '');
    }
    if (process.platform === 'linux' && !process.report?.getReport()?.header?.glibcVersionRuntime) {
      lines.push(
        '  This looks like a musl system (Alpine). The Linux build is glibc-only:',
        '  use a glibc image (debian:12-slim, ubuntu:22.04) or build from source.',
        '',
      );
    }
    lines.push(
      '  Build from source instead (Rust toolchain required):',
      '    git clone https://github.com/paradoxcomputer/lpad.git',
      '    cd lpad && bash setup.sh && bash scripts/install-cli.sh',
      '  See https://github.com/paradoxcomputer/lpad#readme',
    );
  }
  die(lines);
}

const platformRoot = path.dirname(platformPkgJson);
const binary = path.join(platformRoot, 'bin', 'lpad');

if (!fs.existsSync(binary)) {
  die([
    `lpad: ${PLATFORM_PKG} is installed but contains no binary at`,
    `  ${binary}`,
    '',
    '  That is a broken package rather than an unsupported platform - please',
    '  report it at https://github.com/paradoxcomputer/lpad/issues',
  ]);
}

// --- 2. make the bundled libpcsclite discoverable (Linux only) --------------
// The binary links libpcsclite.so.1 whether or not you own a smartcard: the
// upstream LEZ wallet crate pulls in keycard support unconditionally, with no
// feature gate to turn off. lpad itself never talks to a card reader, so this
// is a link-time fact, not a runtime dependency you can satisfy by installing
// pcscd. Don't waste time trying to drop it - it has to come from upstream.
//
// Rather than make `npm i -g` fail on any machine without libpcsclite1, the
// linux-x64 package ships its own copy next to the binary and we point the
// loader at it. LD_LIBRARY_PATH (not a patchelf'd RPATH) because it is visible
// in `env`, needs no extra tooling in the release job, and can be overridden or
// reproduced by hand when someone is debugging a load failure.
//
// It is set on the CHILD's environment only - the user's shell is untouched -
// and it PREPENDS to any existing value, so an LD_LIBRARY_PATH the user set for
// their own reasons still applies to everything else the binary loads.
const env = { ...process.env };
if (process.platform === 'linux') {
  const libDir = path.join(platformRoot, 'lib');
  if (fs.existsSync(path.join(libDir, 'libpcsclite.so.1'))) {
    env.LD_LIBRARY_PATH = env.LD_LIBRARY_PATH ? `${libDir}:${env.LD_LIBRARY_PATH}` : libDir;
  }
}

// --- 3. run it --------------------------------------------------------------
// A CLI that loses an exit code or eats Ctrl-C is worse than useless in a
// script, and Node has no execve(), so this process has to stand in for the
// binary faithfully.
//
// stdio: 'inherit' hands over the real file descriptors, so lpad's TTY
// detection (progress bars, colour) sees a terminal when there is one and pipes
// stay pipes. argv0 makes the binary's own usage and error text say `lpad`
// rather than a path inside node_modules.
const child = spawn(binary, process.argv.slice(2), {
  stdio: 'inherit',
  env,
  argv0: 'lpad',
});

// Forward signals rather than dying on them.
//
// From a terminal the child is in our foreground process group and already gets
// Ctrl-C directly - but a supervisor, `timeout`, or a CI runner signals this
// process alone, and then only an explicit forward reaches the binary. Dying
// here instead would orphan a running transaction and hand the shell back a
// prompt while the binary is still writing to the terminal.
const FORWARDED = ['SIGINT', 'SIGTERM', 'SIGHUP', 'SIGQUIT'];
for (const sig of FORWARDED) {
  try {
    process.on(sig, () => {
      try {
        child.kill(sig);
      } catch {
        // Already gone: the exit handler below is about to run.
      }
    });
  } catch {
    // Not every signal exists on every platform; failing to hook one is never a
    // reason to refuse to run.
  }
}

child.on('error', (err) => {
  const extra =
    err.code === 'EACCES'
      ? [
          '  The binary is not executable. That usually means the package was',
          '  unpacked by something that dropped file modes.',
        ]
      : [];
  die([`lpad: could not run ${binary}`, `  ${err.message}`, ...extra]);
});

child.on('exit', (code, signal) => {
  // Killed by a signal: re-raise it on ourselves so our own wait status is the
  // one the binary produced. Then `$?` is 130 for Ctrl-C rather than a bare 1,
  // `timeout` sees the kill it expects, and the shell prints its usual
  // "Interrupt". Dropping the handlers first restores the default disposition -
  // otherwise the forwarder above would swallow the re-raise.
  if (signal) {
    for (const sig of FORWARDED) process.removeAllListeners(sig);
    process.kill(process.pid, signal);
    // Only reached if the signal is blocked or ignored further up; fall back to
    // the shell's 128+n convention.
    process.exit(128 + (os.constants.signals[signal] || 0));
  }
  process.exit(code === null ? 1 : code);
});
