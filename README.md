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

## Quick install and test

Install the CLI from npm - no Rust toolchain, no LEZ checkout, no `bootstrap.sh`:

```bash
npm i -g @paradoxcomputer/lpad
lpad --version
```

One platform binary is fetched, not all of them; on Linux the smartcard library the LEZ
wallet links is bundled alongside it, so there is nothing to `apt install`. Requires
glibc 2.34+ on Linux (Ubuntu 22.04+, Debian 12+).

Then go from an empty wallet to a token you can trade, entirely through `lpad`:

```bash
lpad network                                  # pick 1) Logos testnet
lpad faucet                                   # 150 native LEZ (solves a small proof-of-work)
lpad wrap --amount 100                        # native LEZ -> WLEZ, the collateral token
lpad bc create-token-sale --name DEMO --symbol DMO --supply 1000000 \
     --sale-quantity 500000 --dex-seed 50000 --vt 2000000 --vc 50000 --fee-bps 100
lpad my-sales                                 # the sale id you just created
lpad bc buy --sale <id> --in 50               # buy from your own curve
lpad bc sale-info --sale <id>                 # watch the price move
```

Those amounts are sized against the faucet, not picked for looking round: one claim
pays a fixed **150** native LEZ (`FAUCET_PRIZE`, a constant of the pinata guest), so
`wrap` has to stay under it and `--in` under what was wrapped. Nothing else in the
flow costs collateral - `--supply`, `--sale-quantity` and `--dex-seed` are all project
tokens the sale mints for itself, and `--vt`/`--vc` are *virtual* reserves that price
the curve without anyone funding them. Only the buy spends WLEZ. Re-run `lpad faucet`
for another 150; there is no cooldown.

**On Logos testnet you do not bootstrap anything.** lpad's three programs are deployed
there under exactly the ids this build carries - `bonding_curve` in block 12609, `lbp` in
12610, `wlez` in 12611 - and a program's id *is* its RISC0 image id, so the same build
addresses the same programs on every chain. **Paradox is the exception right now**: that
sequencer has no record of any of the three, and every lpad PDA on it reads as unclaimed,
so it needs a deploy and a bootstrap before any of this works. Ask a given network yourself
rather than trusting this paragraph - the ids move whenever the guests are rebuilt, and
this is exactly the sentence that goes stale:

```bash
lpad --network testnet network --check      # are this build's programs on that chain?
bash scripts/verify-deployment.sh           # the stronger question: is lpad's state still there?
```

On a chain where they are up you need no funded account to start from: `lpad faucet` gets you native LEZ, `wrap` turns it
into collateral, and `bc create-token-sale` mints the project token *and* opens the sale in
one step. `scripts/bootstrap.sh` is the other half of the story - it deploys the three
programs and stands up a demo sale, an LBP pool and a funded buyer in one shot - and it is
the only part of lpad that needs a LEZ checkout.

**Picking local is the case that needs more of you.** You install and run the node
yourself; lpad ships none. A fresh chain carries none of lpad's programs, so the picker
offers to deploy all three for you. Funding is yours to arrange too - whether `lpad faucet`
works there depends on whether your genesis includes the pinata faucet program.

Want the private path? It is the same shape, and slower by design, because each one is a
real recursive STARK:

```bash
lpad shield-lez --amount 2000                 # native LEZ -> WLEZ -> shielded
lpad bc buy-disposable --sale <id> --in 1000  # private buy in ONE transaction
```

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
and it fails as a timeout rather than as an error. `sdk/src/lib.rs::chain_parity` guards
that: this build's `token`, `authenticated_transfer` and `clock` image ids must equal
constants that were read off both public sequencers, and they do match both live chains
as of this release. It is a comparison of committed constants, not a live query - it runs
in CI and in `scripts/ci-e2e.sh` and touches no network - so it catches lpad drifting away
from that snapshot, but not the operators upgrading underneath it. Confirming the latter
means reading the ids off the chains again. See
[`CHANGELOG.md`](CHANGELOG.md) for what changed per release. (The blow-by-blow
migration record, `docs/UPGRADE-v0.2.4.md`, is kept local rather than published -
`docs/` is a working directory; `docs/PRIVACY.md` is the exception, because both
RFPs require it as a deliverable.)

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

Deploying is no longer what makes that true. `lpad` deploys the three launchpad
programs itself, through the SDK, with no wallet binary and no checkout - see
[Which chain](#which-chain). Bootstrap still drives the wallet binary because
everything *else* it does has no `lpad` equivalent (create keypairs, mint tokens,
claim the faucet, send native LEZ) and because it has to run unattended, while the
CLI's deploy sits behind a prompt. So the checkout requirement is unchanged for
bootstrap, and gone for anyone who only wants lpad on a chain they run.

```bash
bash setup.sh                                                 # verify the toolchain
bash scripts/ci-e2e.sh                                        # full gate: unit + CLI + SDK chain-parity + ABI/artifact drift + e2e
bash scripts/audit.sh                                         # cargo-audit over all three lockfiles
RISC0_SKIP_BUILD=1 cargo test --manifest-path cli/Cargo.toml  # CLI tests (fast)
cd programs && RISC0_DEV_MODE=1 cargo test --workspace --features artifacts   # unit + in-process e2e; dev/fake proofs for speed, real everywhere else
bash scripts/bootstrap.sh                                     # deploy programs + fund a demo sale (Logos testnet)
bash scripts/demo.sh                                          # the public path end to end: faucet -> wrap -> create -> quote -> buy -> sell
LPAD_DEMO_PREFLIGHT_ONLY=1 bash scripts/demo.sh               # ...rehearse its checks without spending a faucet claim
```

`scripts/demo.sh` is the one to run in front of people. It is **6-9 minutes**, not
five: the flow is 7-11 on-chain transactions and a block is ~46-50s, and the script
says so rather than implying otherwise. Skip the faucet and wrap on an already-funded
wallet (`LPAD_DEMO_SKIP_FAUCET=1 LPAD_DEMO_SKIP_WRAP=1`) and it is 4-5. It contains no
private operation and cannot be edited into containing one by accident - the wrapper
every step calls refuses the shielded subcommands outright, because each of those is a
real recursive STARK at 38-40 minutes. Those live in `scripts/private-ops.sh`.

`--features artifacts` is not decoration: it is what compiles the `lpad_guests` library,
and with it `pinned_ids_match_artifacts`, the test that the committed guest ELFs still
hash to the program ids every sale and pool PDA is derived from. Leave it off and that
guard runs only because `integration_tests` happens to pull the feature in - narrow the
run to anything that excludes those tests and it silently disappears. `scripts/ci-e2e.sh`
passes it explicitly for the same reason.

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

If you installed from npm, skip to the commands below - [Quick install and
test](#quick-install-and-test) covers getting there. To install from a source checkout
instead:

```bash
bash scripts/install-cli.sh                   # builds and puts `lpad` on your PATH
```

`lpad` **auto-detects** its wallet - program ids, your holdings, and the dev/real proof
mode - so no env vars are needed. The wallet lives at `~/.lpad` (one per network) and is
created on first use; `scripts/bootstrap.sh` also writes one, but you only need it if you
want a whole demo environment seeded at once. Then, e.g.:

```bash
lpad status                                   # chain + wallet snapshot
lpad my-balance                               # tokens, native LEZ, WLEZ, shielded - with ids
lpad bc create-token-sale --name DEMO --symbol DMO --supply 1000000 \
     --sale-quantity 500000 --dex-seed 50000 --vt 2000000 --vc 50000 --fee-bps 100
lpad my-sales                                 # discover your sales (and `my-pools`)
lpad wrap --amount 3000                       # native LEZ -> WLEZ (collateral)
lpad bc buy --sale <id> --in 1000             # public buy (1% default slippage; --min-out to override)
lpad bc buy-private --sale <id> --in 1000     # private buy: deshield -> public buy -> re-shield
lpad bc buy-disposable --sale <id> --in 1000  # private buy in ONE transaction (private slots, no ephemeral account)
lpad bc sale-info --sale <id>                 # state + the creator `close`/`withdraw` need
lpad bc close --sale <id> --creator <id>      # then withdraw...
lpad bc sweep-treasury --sale <id>            # ...and settle the escrowed fee (permissionless, no --creator)
```

`--program` and holdings auto-detect; pass `--config`/`--storage` or set
`$LEE_WALLET_HOME_DIR` to target a non-default wallet.

### Which chain

Three choices, and **lpad ships no sequencer**:

| # | Choice | Sequencer | Who runs it |
|---|---|---|---|
| 1 | Logos testnet | `https://testnet.lez.logos.co` | Logos |
| 2 | Paradox | `https://seq-testnet.paradox.computer` | Paradox Computer |
| 3 | Local | `http://127.0.0.1:3040` (**editable at the prompt**) | **you** - a LEZ node you install and run |

The first command that opens a wallet with no choice recorded - and no `--network`
of its own - asks once, on stderr, and writes the answer to a `network` file next to
the wallet; every later command uses it silently. `lpad network` re-opens the picker.
`--network` overrides one invocation and is never written back.

```bash
lpad network                                  # pick, or re-pick, and remember
lpad network --json                           # what this wallet is set to, and where that is stored
lpad --network paradox status                 # this invocation only
```

Local mode targets a node **you** install and run - lpad no longer bundles one - so
the URL is yours to set: the picker offers `http://127.0.0.1:3040` and takes whatever
you type instead. That node must be LEZ v0.2.4, for the same reason the pin above is
load-bearing. Because a fresh chain of your own carries none of lpad's programs, and
a transaction against a program that is not deployed is dropped from the mempool and
reads as a *timeout* rather than an error, picking local also checks what is on that
chain and offers to deploy the three programs for you. That offer is local-only: the
two named chains are run by other people, and a bare URL is treated the same way,
because lpad cannot tell whose chain it is.

Program ids do **not** vary by network. A program's id *is* its RISC0 image id,
computed from the committed ELF, so the ids are identical on all three; the only
thing "which network" decides is where those programs are deployed and where your
accounts live.

Nothing here prompts without a terminal: with stdin or stderr redirected - CI,
`scripts/test-all-cli.sh`, a pipeline - a wallet with no recorded choice falls back
to the sequencer in its own config exactly as before, and `lpad network` prints the
file to write instead of waiting on an answer nobody can see. `bootstrap.sh` writes
the sequencer into the wallet's *config* but records no choice of its own, so the
first interactive command after it still asks once; answer with the network you
bootstrapped.

The choice is per **wallet**, not global, because a wallet's accounts, shielded
notes and sale ids are PDAs on one chain: `~/.lpad` for testnet, `~/.lpad-paradox`
for paradox. `--network` moves the sequencer only - the wallet home stays wherever
`--config`/`$LEE_WALLET_HOME_DIR` put it, so mixing them prints one chain's height
beside another chain's wallet. See
[cli/README.md](cli/README.md#global-flags--environment).

> 📖 **Full command reference** - every command, flag, and a runnable example:
> **[cli/README.md](cli/README.md)**.

## CLI - what works

All 51 commands below are implemented, and one `scripts/test-all-cli.sh` sweep exercises
all 51 against a live sequencer - the offline ones (quotes, PDA derivation, `program-id`,
`network --json`) make no RPC by design, and are checked in the same run.
A skip there is neither a pass nor a fail: it is counted apart
and listed at the end, so a partial sweep cannot read as full coverage. Nine of the 51
are real recursive STARKs - `buy-private` and `buy-disposable` on both programs,
`bc sell-private`, plus the four shield/deshield commands - and `LPAD_SKIP_PRIVATE=1`
skips exactly those.
Skipping them cascades honestly: a skipped `bc buy-disposable` leaves the curve with no
escrowed fee, so `bc sweep-treasury` is then reported as a skip too, never as a pass.
Both bootstrap environments this repo was last driven against were created with
`LPAD_SKIP_PRIVATE=1`. Those environments are per-run output and are not committed, and
no run log is either, so this is a sweep to re-run rather than a result to read off. `scripts/private-ops.sh` runs the shielded
subset on its own, because each of those needs a real recursive STARK. There is no
dev-mode sequencer to test against - a real one verifies proofs, so `RISC0_DEV_MODE`
applies only to the in-process integration tests.

| Area | Commands | What it does |
|---|---|---|
| Wallet creation | `init-wallet` | create a wallet from nothing: fresh keys, the recovery phrase printed once, sane polling written into the config, and the chain remembered. The first command a bare install can run - everything else needs a wallet to already exist |
| Wallet / discovery | `status`, `balance`, `my-balance`, `my-sales`, `my-pools` | chain+wallet snapshot; one account's balance or all of yours (native LEZ + WLEZ + shielded); auto-discover your sales/pools |
| Network | `network` | pick which sequencer this wallet talks to and remember it; on a local pick, check whether lpad's programs are on that chain and offer to deploy them. `--json` reports the current choice without asking |
| Offline calc (no wallet) | `bc quote\|cost\|sell-quote\|ids`, `lbp weight\|quote\|ids\|allowlist-leaf`, `program-id` | quotes / PDA derivation / allowlist leaves straight from the on-chain libraries |
| Collateral & shielding | `wrap` / `unwrap` (native LEZ ↔ WLEZ), `shield` / `deshield`, `shield-lez` / `deshield-lez` | wrap native LEZ into the WLEZ token; move tokens public ↔ shielded (one-shot LEZ↔shielded-WLEZ) |
| One-shot launch | `bc create-token-sale [--private]` | mint a token (on-chain name/symbol) **and** open a native-LEZ-collateral sale in one go; `--private` = unlinkable creator via a shielded deposit. Prints the creator and creator token holding it minted - `bc close`/`bc withdraw` take no other accounts, and under `--private` this output is the only record of them |
| Bonding curve | `bc create-sale`, `bc buy`, `bc buy-ata`, `bc buy-private`, `bc buy-disposable`, `bc sell`, `bc sell-ata`, `bc sell-private`, `bc close`, `bc withdraw`, `bc sweep-treasury`, `bc sale-info` | full BC lifecycle - public / private / **ATA** paths, plus fee settlement |
| LBP | `lbp create-sale`, `lbp buy`, `lbp buy-gated`, `lbp buy-ata`, `lbp buy-private`, `lbp buy-disposable`, `lbp pause\|resume\|poke`, `lbp close`, `lbp withdraw`, `lbp sweep-treasury`, `lbp pool-info` | full LBP lifecycle - public / private / **ATA** / allowlist-gated paths, plus fee settlement |
| Fee settlement | `bc sweep-treasury`, `lbp sweep-treasury` | pay the escrowed protocol fee out to the treasury the sale/pool pinned at creation. **Permissionless** - no creator flag - and the ONLY instruction on either program that moves that money |
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
treasury - paid inside a public buy, but **escrowed** in the collateral vault by a
private one (a privacy proof pins every public account it touches, and a shared
treasury changes constantly) and settled afterwards by `bc sweep-treasury`. Optional
one-directional mode (no sell-back), end timestamp, and a `DEX_seed`
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
per-block token ceiling and an optional **Merkle allowlist gate** (only the root is
on-chain - the eligibility set never is; the leaf is bound to the buyer so a published
proof can't be replayed). It is a plain sorted-pair SHA-256 inclusion proof, not a ZK
one: `BuyGated` is a public transaction and the leaf and path are cleartext, so a gated
buy's anonymity set is the allowlist rather than the zone, and there is no gated private
path (`docs/PRIVACY.md §2`). There is **no per-swap fee at all**: one fee is charged at
withdrawal, on the raised collateral only (never on the creator's own seed), escrowed
in the collateral vault and settled by `lbp sweep-treasury`. `withdraw` names no
treasury account, which is the point - paying both in one transaction meant a treasury
that could not receive locked the creator's entire raise, not just the fee.

## Private buys

Two modes ship, and both stay - they make opposite trade-offs, so neither is a strict
improvement on the other.

**`buy-private` (the saga, and the default).** Collateral is deshielded to a **fresh,
single-use public account A**, A buys against the public curve/pool, and the tokens are
re-shielded to the buyer's private account. Observers see only an ephemeral account with
no history. It is **drift-free**: deshield (proof) → **public** buy (atomic vs live
state, slippage-bounded with `--min-out`) → re-shield (proof). Only the proofless public
buy touches the pool, so a competing trade can't invalidate it. It is **SDK-sequenced**
with best-effort recovery - a failed buy rolls the deshield back; the re-shield is
retried and, if it ultimately fails, the tokens remain recoverable in account A (no
funds lost). A fully durable crash-safe journal is planned. Real-proof verified
end-to-end; `lpad bc buy-private --sale <id> --in <amount>` runs it.

**`buy-disposable` (one transaction).** Privacy on LEZ is a per-slot label, so the
buyer's collateral holding and token holding are simply declared as **private slots of
one ordinary buy**: the debit and the credit are private notes inside a single proof.
There is no ephemeral account, no deshield leg and no re-shield leg - either the whole
buy lands or nothing happened, so no crash can leave a trade publicly visible. What it
gives up is drift-freeness: the sale or pool PDA is pinned for the length of the proof,
so a competing trade that lands first invalidates it and the minutes of local proving
are wasted. It also cannot carry the on-chain clock (every public account in a privacy
transaction is pinned byte-for-byte and re-verified at inclusion, and the clock is
rewritten every block), so the LBP prices it at a caller-supplied `t_buy_ms` bounded by
a validity window the *guest* computes - never better than the price at admission.
`lpad bc buy-disposable` / `lpad lbp buy-disposable` run it.

Its cost is structural rather than proportional to the trade, but **not because of the note
padding** - that is a red herring worth naming, because it is the first thing everyone
reaches for. The *wallet* (not the circuit) pads every privacy transaction to
`MAX_PRIVATE_ACCOUNTS = 7` via `dummy_inputs_default()`; the circuit itself takes a
`Vec<DummyInput>` and accepts any count. Each dummy costs a nullifier hash and a commitment
hash - about a dozen hashes in total, against four recursive STARKs - so removing the
padding would save roughly 1% of transaction size and nothing measurable in proving time.
The cost is the recursion itself. Budget minutes to hours of CPU per buy, and run private operations one at a
time. The **~8 min** quoted here through v0.1.0 was measured before that padding landed
and no longer holds; nothing has been re-measured since, so treat the figure as withdrawn
rather than revised.

Under the saga the only pool touch is a public buy off the live on-chain clock, so the
LBP's time dependency needs no in-proof clock at all. Under `buy-disposable` it cannot
have one - every public account in a privacy transaction is pinned byte-for-byte at
proving time and re-verified at inclusion, and the clock account is rewritten every
block - so the guest emits a timestamp validity window bounding the caller's `t_buy_ms`
to `min(deadline, t_buy_ms + 1h, t_end_ms)` instead. Because the LBP price only falls,
the price at admission is never better than the one proved.

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
