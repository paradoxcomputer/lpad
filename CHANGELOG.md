# Changelog

All notable changes to LPAD. Versions follow [semver](https://semver.org); while
the project is pre-1.0 a minor bump may still break interfaces.

Each release names the **LEZ tag** it is built against and the **Logos L1 node**
revision that tag transitively pins. Those are not cosmetic: a program's RISC0
image id *is* its on-chain address, so a LEZ bump that the operators have not
adopted makes every transaction against a built-in program fail - silently, as a
timeout rather than an error.

## [0.2.0] - 2026-08-11

Built against **LEZ [`v0.2.4`](https://github.com/logos-blockchain/logos-execution-zone/releases/tag/v0.2.4)**
(2026-08-07, the latest release), which pins the **Logos L1 node** at rev
`e2a1c3b7`. That revision sits 19 commits *ahead* of the latest L1 node release,
[`0.2.1`](https://github.com/logos-blockchain/logos-blockchain/releases/tag/0.2.1)
(2026-08-05), and 8 behind - those 8 being only genesis-ceremony, deployment-config
and version-bump commits on the release branch, no code changes. lpad does not
select the L1 revision itself; it inherits whatever the pinned LEZ selects, which
is what keeps it byte-compatible with the running sequencers.

Verified against both public testnets: the pinned LEZ build's `token`,
`authenticated_transfer` and `clock` image ids equal the ones live on
`testnet.lez.logos.co` and `seq-testnet.paradox.computer`.

### Changed

- **Retargeted from LEZ v0.2.0-rc4 to v0.2.4**, via v0.2.0 and v0.2.1. Every crate
  now depends on the published git tag, so there is no LEZ checkout to wire up and
  no path patching; the `_lez` symlink and its `[patch]` sections are gone.
  v0.2.1 upstreamed the bonsai-free risc0 configuration that the guests need in
  order to cross-compile at all.
- **No bundled local sequencer.** lpad targets the two live testnets. Selecting a
  network is now explicit (`--network`, `LPAD_NETWORK`) across the CLI, the
  bootstrap and the test harnesses, and each network keeps its own wallet home,
  because a wallet holds per-chain accounts and shielded notes.
- **Dropped the vendored token and ATA forks.** v0.2.1 ships both upstream as
  committed artifacts with machine-independent image ids, so the recipient
  contract those forks enforced is re-asserted by lpad's own callers instead.
- **Program ids are compile-time constants.** The guest ELFs are committed under
  `programs/artifacts/lpad/` and embedded into the SDK, replacing the old runtime
  ELF lookup - the CLI can no longer submit against a program id that disagrees
  with what was deployed.

### Added

- **wLEZ**, so native LEZ can be used as sale collateral (`wrap`, `unwrap`,
  `shield-lez`, `deshield-lez`, `bc create-token-sale`). The canonical native
  program is pinned at `Initialize` and re-checked on every `Wrap`, which closes
  an unbacked-mint front-run.
- **Associated Token Accounts** on every token interaction (`create-ata`,
  `bc buy-ata` / `sell-ata`, `lbp buy-ata`), with the ATA program pinned at sale
  creation so a substituted one is rejected.
- **Allowlist-gated LBP sales** (`lbp buy-gated`, `lbp allowlist-leaf`), with a
  bounded Merkle inclusion proof checked on chain.
- **Generated ABIs.** `scripts/build-abi.sh` derives per-program ABIs from source
  in zonescan's type-descriptor format, plus a consolidated bundle for indexers.
  The hand-maintained IDLs are gone - they could drift, and did.
- **Deployment verification.** `scripts/verify-deployment.sh` asserts the sale and
  pool PDAs exist *and* are owned by the program id this build embeds, and
  `bootstrap.sh` proves each guest's own ELF bytes are in a block rather than
  trusting the wallet's report.
- **`scripts/private-ops.sh`**, a serialised real-proof sweep of the shielded
  operations with per-op wall-clock timings. It refuses to run under
  `RISC0_DEV_MODE=1` (a real sequencer verifies proofs) or alongside another
  prover (each is ~8-9 GB resident).
- **`docs/UPGRADE-v0.2.4.md`**, the migration record, including the failure modes
  that only appear against a real network.
- **`scripts/audit.sh`** and `.cargo/audit.toml`. All three lockfiles are clean
  under `cargo audit --deny warnings`.
- **`lpad program-id wlez`** additionally prints the definition and vault PDAs it
  owns. They are pure derivations from the program id, and they are the only
  handle a deployment check has on wlez - unlike a sale or a pool, nothing records
  a wlez account id during bootstrap, which is why the standing check silently
  covered two of the three deployed programs.

### Gates

Two guards existed but were never executed by CI, and two more are new. Each of
these fails in a way that is invisible on chain: a wrong program id is not an
error, it is a transaction that never lands.

- **SDK chain-parity**: the pinned LEZ build's built-in `token`,
  `authenticated_transfer` and `clock` image ids must equal the live ones. `sdk`
  is its own workspace, so neither the `programs` nor the `cli` test step ran it.
- **`pinned_ids_match_artifacts`**: the committed guest ELFs must still hash to
  the ids in `lpad_guests::deployed`, which every sale and pool PDA is derived
  from. Needed `-p lpad_guests --features artifacts`, which the gate did not pass.
- **Cross-workspace id parity** (new): the same assertion, made in `sdk` - the id
  that actually reaches a sequencer is the one the CLI embeds, and the CLI is
  built from a different workspace with a different lockfile, which is exactly
  where a divergence would hide.
- **ABI drift**: `scripts/ci-e2e.sh` regenerates the ABIs and fails on a diff.
  The comment promising this check had been there without the check.

### Fixed

- **Silent deploy failures.** A deploy transaction's hash is a pure function of
  its bytecode, so re-deploying an unchanged guest resolves to the *original*
  deploy and reports its block; and a deploy that never lands is indistinguishable
  from a slow chain. One program reported inclusion and was never deployed, after
  which every call against it was dropped from the mempool with no error. Deploys
  are now verified by reading the block, with a bounded and always-reported
  fallback scan.
- **Silent funding failures.** The bootstrap's token transfers tolerated loss, so
  an unfunded buyer holding surfaced ~40 minutes later as a hung `bc buy`. Each
  transfer's effect on the recipient's balance is now asserted.
- **Uninitialised recipients.** A token transfer to a never-initialised holding is
  rejected, because the token program claims the recipient as `Authorized` and the
  runtime grants that only to a signer. `lpad init-holding` creates and
  initialises in one step; the LEZ wallet has no equivalent.
- **A too-short poll budget.** 10 × 20s was under four blocks of patience against
  ~46-50s block times, so a slow inclusion and an outright rejection produced the
  same `All pollers failed`. Now 60 × 30s.
- **`getAccount` id normalisation.** It accepts only bare base58; a `Public/`
  prefix or a hex id made a live account read as absent, so a green verification
  run meant nothing.
- **An inert `cargo audit` config.** `audit.toml` documented
  `cargo audit -c audit.toml`, but cargo-audit has no `--config`; `-c` is
  `--color`, so the command errored and the ignore list had never once been
  applied. Moved to `.cargo/audit.toml`, where cargo-audit actually reads it, and
  corrected: two advisories that fire today were missing, and the claim that no
  ignored advisory comes from lpad's own crates was false (`number_prefix` arrives
  through the CLI's `indicatif`).
- **Version skew between the three lockfiles.** `risc0-binfmt` - the crate that
  turns a guest ELF into its on-chain program id - was the one risc0 dependency
  left on a caret range, and had already resolved to 3.0.5 in `programs/` and
  3.0.4 in `cli/` and `sdk/`. The ids agreed, but only by luck. Now pinned `=3.0.5`
  everywhere. `borsh` likewise differed between the lockfile that builds the
  guests (1.8.0) and the ones that build the clients encoding for them (1.6.1);
  and `cli`/`sdk` carried yanked `bitcoin_hashes` and `spin` releases that
  `programs` had already moved past.
- **`.gitignore` ignored itself** and so had never been committed: a fresh clone
  arrived with no ignore rules at all, one `git add -A` away from committing
  `target/` or a live `bootstrap.env`.
- **`verify-deployment.sh` could check the wrong chain.** It read
  `scripts/bootstrap.env` regardless of network - so its own documented
  `LPAD_NETWORK=paradox` invocation compared testnet PDAs against the paradox RPC,
  where every account reads as absent, indistinguishable from a failed deploy. It
  now defaults to the per-network env file, passes `--network` to its CLI reads,
  covers wlez, and tells an unclaimed account (a chain reset) apart from an owner
  mismatch (artifact drift).
- **The build's one undocumented prerequisite.** `scripts/bootstrap.sh` drives the
  upstream LEZ `wallet` binary and seeds its config from a LEZ checkout - the one
  thing "no LEZ checkout to wire up" did not cover. README says so now, `setup.sh`
  reports it, and the script fails with the two commands that fix it.
- **A build that could not link.** On distros shipping only the runtime
  `libpcsclite.so.1`, `setup.sh` now creates the shim `pcsc-sys` needs; it also
  detects a cached `PCSC_LIB_DIR` pointing at a directory that no longer exists,
  which fails every relink with `unable to find library -lpcsclite` while stale
  binaries keep working.
- Security and quality fixes across four review passes: token-vault program
  dispatch, the LBP withdraw token leg, the bonding-curve sell anchor,
  private-transfer idempotency, and clone-safe program-id pins.

### Known limitations

- **Not externally audited.** Do not use with real funds.
- **Private operations are expensive.** Since LEZ v0.2.1 the privacy circuit pads
  its note set to `MAX_PRIVATE_ACCOUNTS = 7` with no opt-out, so a one-note shield
  proves the same seven slots as a full one. The cost is flat and structural, not
  proportional to what is being moved: budget minutes to hours of CPU per shielded
  operation, and run them one at a time (`scripts/private-ops.sh` enforces this).
- `cargo fmt --check` is not part of the gate: `programs/rustfmt.toml` selects
  nightly-only options that the pinned stable toolchain ignores, so stable rustfmt
  would reformat against the repo's own style.

## [0.1.0] - 2026-06-07

Initial testnet release, built against LEZ `v0.2.0-rc4`.

Two on-chain launchpad programs for the Logos Execution Zone - `bonding_curve`
(RFP-015, a constant-product virtual-reserve AMM) and `lbp` (RFP-016, a
Balancer-style weight-shifting AMM) - plus a Rust SDK, a CLI, and the private
deshield → public buy → re-shield path.

[0.2.0]: https://github.com/paradoxcomputer/lpad/releases/tag/v0.2.0
[0.1.0]: https://github.com/paradoxcomputer/lpad/releases/tag/v0.1.0
