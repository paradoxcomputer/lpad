# LPAD - Logos Privacy-Preserving Token Launchpad

[![CI](https://github.com/paradoxcomputer/lpad/actions/workflows/ci.yml/badge.svg)](https://github.com/paradoxcomputer/lpad/actions/workflows/ci.yml)
![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)
![Status](https://img.shields.io/badge/status-work%20in%20progress-orange.svg)

A token-launch platform for the Logos Execution Zone (LEZ) with **two complementary
launch mechanisms** behind one shared SDK + CLI (+ mini-app). Pick the curve that fits
the raise; both run on the same chain, share the same privacy pattern, and expose the
same lifecycle.

> ⚠️ **Work in progress.** The on-chain programs, SDK and CLI are implemented and
> live-tested (unit + e2e, public **and** private buys verified on a real-proof
> sequencer). Interfaces may still change. Not yet audited - do not
> use with real funds.

**RFPs:** delivers **[RFP-015 - Bonding Curve](https://github.com/logos-co/rfp)** and
its companion **RFP-016 - Liquidity Bootstrapping Pool (LBP)** as one integrated
platform (shared SDK/CLI/privacy layer). The two programs are independent on-chain but
ship together.

| Program | RFP | Mechanism | Price dynamics | Best for |
|---|---|---|---|---|
| `bonding_curve` | RFP-015 | constant-product AMM, virtual reserves (pump.fun / Meteora style) | **supply-driven** - rises with each buy | deterministic, pre-computable raises; rewarding early buyers |
| `lbp` | RFP-016 | Balancer weight-shifting AMM | **time-driven** - starts high, falls over the window | bot-resistant, market-driven price discovery |

All pool/curve state is **public** on-chain (reserves, weights, price, raise) so pricing
is permissionlessly verifiable and composable; privacy is enforced at the SDK/UX layer
(below), not by hiding pool state. Both programs are integer-only and round against the
trader, so the pool stays solvent. License: **MIT OR Apache-2.0**.

## Build & test

Builds against LEZ **[`v0.2.4`](https://github.com/logos-blockchain/logos-execution-zone/releases/tag/v0.2.4)**,
the latest release, pulled straight from the published git tag - so *building* needs no
LEZ checkout and no path-patching. That tag transitively pins the **Logos L1 node** at
rev `e2a1c3b7`, which is 19 commits ahead of the latest L1 node release,
[`0.2.1`](https://github.com/logos-blockchain/logos-blockchain/releases/tag/0.2.1)
(the 8 commits it lacks are genesis-ceremony and deployment-config only, no code).
lpad never selects the L1 revision itself; it inherits whatever the pinned LEZ picks,
which is what keeps it byte-compatible with the sequencers.

The pin is load-bearing. A program's RISC0 image id *is* its on-chain address, so a LEZ
version the operators have not adopted makes every call against a built-in program fail,
and it fails as a timeout rather than as an error. `sdk/src/lib.rs::chain_parity` asserts
that this build's `token`, `authenticated_transfer` and `clock` image ids equal the ones
live on both public sequencers; it runs in CI and in `scripts/ci-e2e.sh`. See
[`docs/UPGRADE-v0.2.4.md`](docs/UPGRADE-v0.2.4.md) for the full migration record and
[`CHANGELOG.md`](CHANGELOG.md) for what changed per release.

Two things beyond `cargo` are required to build: **Docker** + `cargo-risczero` (only to
rebuild the zkVM guests, whose prebuilt ELFs are committed), and **libpcsclite** (the LEZ
wallet links keycard support unconditionally). `setup.sh` checks both, and on distros that
ship only the runtime `libpcsclite.so.1` it creates the shim `pcsc-sys` needs and prints
the one `export` to add.

`scripts/bootstrap.sh` is the one exception to "no LEZ checkout". It does not *build*
against LEZ, it *drives* it: it shells out to the upstream `wallet` binary for key
management, deploys and transfers, and seeds its wallet config from
`lez/wallet/configs/debug/wallet_config.json`. So before bootstrapping, clone LEZ at tag
`v0.2.4` to `$HOME/lez` (or point `$LPAD_LEZ_DIR` at it) and build the wallet there:

```bash
git clone --branch v0.2.4 https://github.com/logos-blockchain/logos-execution-zone.git ~/lez
(cd ~/lez && cargo build --release -p wallet)
```

`$LPAD_WALLET_BIN` relocates the binary alone; the checkout is still needed for the
config. Nothing else below touches it - `setup.sh`, `scripts/ci-e2e.sh` and the `cargo
test` lines are all self-contained.

```bash
bash setup.sh                                                 # verify the toolchain
bash scripts/ci-e2e.sh                                        # full gate: unit + CLI + SDK chain-parity + ABI/artifact drift + e2e
bash scripts/audit.sh                                         # cargo-audit over all three lockfiles
RISC0_SKIP_BUILD=1 cargo test --manifest-path cli/Cargo.toml  # CLI tests (fast)
cd programs && RISC0_DEV_MODE=1 cargo test --workspace        # tests only - dev/fake proofs for speed; the launchpad itself defaults to real proofs
bash scripts/bootstrap.sh                                     # deploy programs + fund a demo sale (Logos testnet)
```

The three launchpad programs are compiled to riscv32 inside a pinned container, so their
RISC0 image ids - which *are* the on-chain program ids - are byte-identical on every
machine. The resulting ELFs are committed under `programs/artifacts/lpad/` and embedded
into the SDK at compile time. Rebuild them only when guest-visible code changes:

```bash
bash scripts/build-guests.sh    # needs Docker; a changed .bin means a redeploy
```

The three workspaces (`programs/`, `sdk/`, `cli/`) each compile the full LEZ +
risc0 dependency tree, so build caches get large - tens of GB after a version
bump. Reclaim with:

```bash
bash scripts/clean.sh           # --dry-run to see the total first
```

It touches only regenerable output; `~/.cargo` is left alone, so a rebuild
recompiles but downloads nothing.

## Run the CLI

Install `lpad` to `~/.local/bin`:

```bash
bash scripts/install-cli.sh
```

After `bash scripts/bootstrap.sh` the wallet lives at `~/.lpad` and `lpad` **auto-detects**
it - program ids, your holdings, and the dev/real proof mode - so no env vars are needed.
Then, e.g.:

```bash
lpad status                                   # chain + wallet snapshot
lpad my-balance                               # tokens, native LEZ, WLEZ, shielded - with ids
lpad bc create-token-sale --name DEMO --symbol DMO --supply 1000000 \
     --sale-quantity 500000 --dex-seed 50000 --vt 2000000 --vc 50000 --fee-bps 100
lpad my-sales                                 # discover your sales (and `my-pools`)
lpad wrap --amount 3000                       # native LEZ -> WLEZ (collateral)
lpad bc buy --sale <id> --in 1000             # public buy (1% default slippage; --min-out to override)
lpad bc buy-private --sale <id> --in 1000     # private buy: deshield -> public buy -> re-shield
```

`--program` and holdings auto-detect; pass `--config`/`--storage` or set
`$LEE_WALLET_HOME_DIR` to target a non-default wallet.

> 📖 **Full command reference** - every command, flag, and a runnable example:
> **[cli/README.md](cli/README.md)**.

## CLI - what works

All 44 commands below are implemented, and all 44 have been run against a live
sequencer. There is no dev-mode sequencer to test against - a real one verifies
proofs, so `RISC0_DEV_MODE` applies only to the in-process integration tests.
`scripts/test-all-cli.sh` is the sweep; `scripts/private-ops.sh` runs the shielded
subset on its own, because each of those needs a real recursive STARK.

| Area | Commands | What it does |
|---|---|---|
| Wallet / discovery | `status`, `balance`, `my-balance`, `my-sales`, `my-pools` | chain+wallet snapshot; one account's balance or all of yours (native LEZ + WLEZ + shielded); auto-discover your sales/pools |
| Offline calc (no wallet) | `bc quote\|cost\|sell-quote\|ids`, `lbp weight\|quote\|ids\|allowlist-leaf`, `program-id` | quotes / PDA derivation / allowlist leaves straight from the on-chain libraries |
| Collateral & shielding | `wrap` / `unwrap` (native LEZ ↔ WLEZ), `shield` / `deshield`, `shield-lez` / `deshield-lez` | wrap native LEZ into the WLEZ token; move tokens public ↔ shielded (one-shot LEZ↔shielded-WLEZ) |
| One-shot launch | `bc create-token-sale [--private]` | mint a token (on-chain name/symbol) **and** open a native-LEZ-collateral sale in one go; `--private` = unlinkable creator via a shielded deposit |
| Bonding curve | `bc create-sale`, `bc buy`, `bc buy-ata`, `bc buy-private`, `bc sell`, `bc sell-ata`, `bc sell-private`, `bc close`, `bc withdraw`, `bc sale-info` | full BC lifecycle - public / private / **ATA** paths |
| LBP | `lbp create-sale`, `lbp buy`, `lbp buy-gated`, `lbp buy-ata`, `lbp buy-private`, `lbp pause\|resume\|poke`, `lbp close`, `lbp withdraw`, `lbp pool-info` | full LBP lifecycle - public / private / **ATA** / allowlist-gated paths |
| ATAs (RFP Func) | `create-ata`, `bc buy-ata` / `sell-ata`, `lbp buy-ata` | route token interactions through Associated Token Accounts (RFP-015 #7 / RFP-016 #9, per LP-0014) |
| Holdings | `init-holding` | create **and initialise** a token holding. Needed before anyone can send to it: the token program claims the recipient as `Authorized`, which the runtime grants only to a signer, and the LEZ wallet has no equivalent command |

> Not yet wired into the CLI: the Logos mini-app (GUI) - see the WIP note above.

## How the mechanisms work

### Bonding curve (RFP-015) - supply-driven

Constant product over **virtual reserves** `Vt` (token) and `Vc` (collateral); spot price
`p = Vc / Vt` rises as tokens are bought. Integer-only, no exponentiation:

```
c_eff   = C_in − fee                            fee = C_in · fee_bps / 10_000
buy:      tokens_out = Vt · c_eff / (Vc + c_eff)            (floored)
inverse:  C_in to buy exactly q tokens = ceil(Vc · q / (Vt − q)), grossed up for the fee
sell:     c_out      = Vc · tokens_in / (Vt + tokens_in)   (floored, capped at real reserve)
```

After a buy: `Vt -= tokens_out`, `Vc += c_eff`, `sale_reserve -= tokens_out`. The sale
**auto-closes atomically** when the sale reserve `D` is exhausted. Per-swap fee to a
treasury; optional one-directional mode (no sell-back), end timestamp, and a `DEX_seed`
reserve `R` held back for post-graduation liquidity. The token's name/symbol are mirrored
into on-chain sale state, so a sale is self-describing.

### LBP (RFP-016) - time-driven

Balancer weighted pool; the token weight declines linearly over the sale, so the price
falls unless buying pressure counteracts it. The out-given-in power is implemented in
**integer Q64.64** (`pow(b,e) = exp2(e · log2(b))`, verified vs exact references):

```
w_token(t)  = w_start + (w_end − w_start) · (t − t_start) / (t_end − t_start)   (per-tx, lazy)
w_coll      = 1 − w_token
tokens_out  = reserve_token · (1 − (rc / (rc + C_in)) ^ (w_coll / w_token))     (floored)
spot price  = (reserve_coll / w_coll) / (reserve_token / w_token)
```

Weights are computed lazily at each buy (a `poke` only refreshes the stored value for
off-chain readers). Emergency `pause` does **not** halt weight progression. Optional
per-block token ceiling and an optional **ZK/Merkle allowlist gate** (only the root is
on-chain - the eligibility set never is; the leaf is bound to the buyer so a published
proof can't be replayed). Protocol fee is collected **at close**, on the raised
collateral only.

## Private buys

A private buy never links the buyer's private account to the purchase: collateral is
deshielded to a **fresh, single-use public account A**, A buys against the public
curve/pool, and the tokens are re-shielded to the buyer's private account. Observers see
only an ephemeral account with no history.

The buy is **drift-free**: deshield (proof) → **public** buy (atomic vs live state,
slippage-bounded with `--min-out`) → re-shield (proof). Only the proofless public buy
touches the pool, so a competing buy can't invalidate it. It is **SDK-sequenced** with
best-effort recovery - a failed buy rolls the deshield back; the re-shield is retried and,
if it ultimately fails, the tokens remain recoverable in account A (no funds lost). A
fully durable crash-safe journal is planned. Real-proof verified end-to-end at **~8 min**
(the two token-transfer STARKs; the public buy needs no proof). `lpad bc buy-private
--sale <id> --in <amount>` runs it.

Because the only pool touch is a public buy off the live on-chain clock, the LBP's time
dependency needs no in-proof clock (which would drift).

## Layout

```
programs/   bonding_curve/{core,}, lbp/{core,}, wlez/{core,}  (core math, host logic, guest main.rs)
            artifacts/lpad/*.bin   committed reproducible guest ELFs = the program ids
            artifacts/*-abi.json   generated from source by scripts/build-abi.sh
            integration_tests/     E2E vs in-process LEZ state
            (the token and ATA programs are no longer vendored - LEZ v0.2.1 ships
             both upstream as committed artifacts with machine-independent ids)
sdk/        lpad-sdk: full lifecycle (discover/quote/buy/sell/create/withdraw - public + private + ATA)
cli/        lpad CLI over the SDK (offline quotes/PDAs + online ops); see cli/README.md for the full command reference
```

Each program follows the LEZ triple: `<name>/core` (types, PDA derivation, pure pricing
math), `<name>` (host logic → `(Vec<AccountPostState>, Vec<ChainedCall>)`),
`<name>/src/main.rs` (the guest entrypoint, cross-compiled to a zkVM ELF).
```
