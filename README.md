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

Needs a LEZ `v0.2.0-rc4` checkout + the RISC Zero toolchain. Point `setup.sh` at your
LEZ checkout with `$LPAD_LEZ_DIR` (defaults to `~/lez`); it symlinks it as `_lez` in the
repo, and every other script defaults to that symlink - no machine-specific paths.

```bash
bash setup.sh                                                 # create the _lez symlink to your LEZ checkout
bash scripts/ci-e2e.sh                                        # full gate: unit + CLI + IDL drift + e2e
RISC0_SKIP_BUILD=1 cargo test --manifest-path cli/Cargo.toml  # CLI tests (fast, no guest build)
cd programs && RISC0_DEV_MODE=1 cargo test --workspace        # tests only - dev/fake proofs for speed; the launchpad itself defaults to real proofs
bash run-sequencer.sh                                         # local sequencer on :3040
bash scripts/bootstrap.sh                                     # deploy programs + fund a demo sale
```

## Run the CLI

Install `lpad` to `~/.local/bin` (the launcher bakes in the guest ELF paths + circuits):

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
`$NSSA_WALLET_HOME_DIR` to target a non-default wallet.

> 📖 **Full command reference** - every command, flag, and a runnable example:
> **[cli/README.md](cli/README.md)**.

## CLI - what works

All commands below are implemented and live-tested against a dev **and** a real-proof
sequencer.

| Area | Commands | What it does |
|---|---|---|
| Wallet / discovery | `status`, `my-balance`, `my-sales`, `my-pools` | chain+wallet snapshot; balances incl. native LEZ + WLEZ + shielded; auto-discover your sales/pools |
| Offline calc (no wallet) | `bc quote\|cost\|sell-quote\|ids`, `lbp weight\|quote\|ids`, `program-id` | quotes / PDA derivation straight from the on-chain libraries |
| Collateral & shielding | `wrap` / `unwrap` (native LEZ ↔ WLEZ), `shield` / `deshield`, `shield-lez` / `deshield-lez` | wrap native LEZ into the WLEZ token; move tokens public ↔ shielded (one-shot LEZ↔shielded-WLEZ) |
| One-shot launch | `bc create-token-sale [--private]` | mint a token (on-chain name/symbol) **and** open a native-LEZ-collateral sale in one go; `--private` = unlinkable creator via a shielded deposit |
| Bonding curve | `bc create-sale`, `bc buy`, `bc buy-ata`, `bc buy-private`, `bc sell`, `bc sell-ata`, `bc sell-private`, `bc close`, `bc withdraw`, `bc sale-info` | full BC lifecycle - public / private / **ATA** paths |
| LBP | `lbp create-sale`, `lbp buy`, `lbp buy-ata`, `lbp buy-private`, `lbp pause\|resume\|poke`, `lbp close`, `lbp withdraw`, `lbp pool-info` | full LBP lifecycle - public / private / **ATA** paths |
| ATAs (RFP Func) | `create-ata`, `bc buy-ata` / `sell-ata`, `lbp buy-ata` | route token interactions through Associated Token Accounts (RFP-015 #7 / RFP-016 #9, per LP-0014) |

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
programs/   bonding_curve/{core,,methods}, lbp/{core,,methods}  (core math, host logic, SPEL guest)
            token/ ata/ wlez/   vendored LEZ programs (ata also powers the ATA buy/sell path; same image ids → ldex interop)
            integration_tests/  E2E vs in-process V03State
sdk/        lpad-sdk: full lifecycle (discover/quote/buy/sell/create/withdraw - public + private + ATA)
cli/        lpad CLI over the SDK (offline quotes/PDAs + online ops); see cli/README.md for the full command reference
```

Each program follows the LEZ triple: `<name>/core` (types, PDA derivation, pure pricing
math), `<name>` (host logic → `(Vec<AccountPostState>, Vec<ChainedCall>)`),
`<name>/methods` (the SPEL `#[lez_program]` guest compiled to a zkVM ELF).
```
