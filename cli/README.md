# `lpad` - CLI Command Reference

Complete reference for the `lpad` command-line client: **every command, its
flags, and a runnable example.** `lpad` is a thin client over the LPAD SDK and the
two on-chain programs - the **bonding curve** (RFP-015) and the **LBP** (RFP-016) -
plus the shared collateral (`wlez`), shielding, and **Associated Token Account
(ATA)** plumbing.

> Work in progress - interfaces may still change. See the top-level
> [README](../README.md) for the high-level overview.

## Contents

- [Setup](#setup)
- [Global flags & environment](#global-flags--environment)
- [Conventions](#conventions)
- [Wallet & discovery](#wallet--discovery)
- [Collateral & shielding](#collateral--shielding)
- [Associated Token Accounts (ATAs)](#associated-token-accounts-atas)
- [Offline math & address derivation](#offline-math--address-derivation)
- [Bonding curve (RFP-015)](#bonding-curve-rfp-015)
- [LBP (RFP-016)](#lbp-rfp-016)
- [JSON output](#json-output)

---

## Setup

```bash
bash setup.sh                 # create the _lez symlink to your LEZ checkout
bash scripts/install-cli.sh   # install `lpad` to ~/.local/bin (bakes in guest ELF paths)
bash run-sequencer.sh         # local sequencer on 127.0.0.1:3040  (separate terminal)
bash scripts/bootstrap.sh     # deploy programs + create the ~/.lpad wallet + a demo sale
```

After `bootstrap.sh`, the wallet lives at `~/.lpad` and `lpad` auto-detects it
(program ids, your holdings, dev/real proof mode) - **no environment variables
are required** for normal use.

---

## Global flags & environment

These flags work on **every** command:

| Flag | Default | Meaning |
|---|---|---|
| `--json` | off | Emit machine-readable JSON instead of human-readable text. |
| `--config <PATH>` | `$NSSA_WALLET_HOME_DIR/wallet_config.json` | Wallet config file. |
| `--storage <PATH>` | `$NSSA_WALLET_HOME_DIR/storage.json` | Wallet storage file. |

Wallet location is resolved as: **`--config`/`--storage` → `$NSSA_WALLET_HOME_DIR`
→ `~/.lpad`** (the default home).

| Environment variable | Purpose |
|---|---|
| `NSSA_WALLET_HOME_DIR` | Wallet home directory (overrides the `~/.lpad` default). |
| `RISC0_DEV_MODE` | **Real proofs by default.** Set `1` for dev/fake proofs (fast). If unset, `lpad` honors a `proof_mode` marker (`dev`/`real`) next to the wallet (the bootstrap records the sequencer's mode); absent a marker it uses real proofs. |
| `LPAD_TX_TTL_MS` | Transaction deadline TTL in ms (default `120000`). |
| `LPAD_BC_ELF`, `LPAD_LBP_ELF`, `LPAD_WLEZ_ELF`, `LPAD_ATA_ELF` | Guest ELF paths; set automatically by the install-cli launcher. |
| `LPAD_REPO` | Repo root (used to locate program artifacts when not installed). |

---

## Conventions

- **`[offline]` vs `[online]`** - offline commands compute quotes / addresses
  purely from the on-chain pricing libraries and need **no wallet or sequencer**.
  Online commands open the wallet and read/submit transactions.
- **Account ids** (`--sale`, `--token-def`, `--creator`, …) are passed as the
  ids printed by the wallet / `lpad` (`Public/…` base58, or 32-byte hex).
- **Amounts** (`--in`, `--amount`, `--supply`, …) are unsigned integers (`u128`),
  in the token's base units.
- **Weights** (`--w-start`, `--w-end`) accept a decimal `0.99` or a fraction
  `99/100`; they must be strictly inside `(0, 1)`.
- **Timestamps** (`--t-start`, `--t-end`, `--at`, `--end-ts`) are Unix
  milliseconds.
- **Auto-detection** - `--program` defaults to the baked-in guest image id;
  `--buyer-collateral`/`--buyer-token` (and the private `--user-*` variants) are
  auto-discovered from your wallet holdings when omitted.
- **ATAs** - the `*-ata` variants route token movements through the on-chain ATA
  program. `--owner` defaults to your wallet's default signing account;
  `--ata-program` defaults to the deployed ATA guest image id; `--fund-from`
  seeds the owner's input ATA from a keypair holding before the trade.
- **Slippage** - buys/sells default to `--slippage-bps 100` (1%). Pass an explicit
  `--min-out` to override the computed floor.

---

## Wallet & discovery

### `lpad status` · `[online]`
Sequencer block height + wallet sync status.
```bash
lpad status
```

### `lpad balance` · `[online]`
Token balance of a single account.

| Flag | Req | Meaning |
|---|---|---|
| `--account <ID>` | yes | Account to inspect. |
```bash
lpad balance --account Public/2RHZhw9h534Zr3eq2RGhQete2Hh667foECzXPmSkGni2
```

### `lpad my-balance` · `[online]`
Your balances across every wallet account - project tokens, native LEZ, WLEZ, and
shielded holdings - each with its id.
```bash
lpad my-balance
```

### `lpad my-sales` · `[online]`
Bonding-curve sales your wallet created (derived locally; no indexer needed).
```bash
lpad my-sales
```

### `lpad my-pools` · `[online]`
LBP pools your wallet created.
```bash
lpad my-pools
```

### `lpad program-id` · `[offline]`
Print a program's deployed id (RISC0 image id) from its guest ELF.

| Arg | Req | Meaning |
|---|---|---|
| `<which>` | yes | `bc`, `lbp`, `ata`, or `wlez`. |
```bash
lpad program-id bc
lpad program-id lbp
lpad program-id ata
```

---

## Collateral & shielding

The native gas token (LEZ) is wrapped into **WLEZ** to serve as sale collateral.
Shielding moves a public holding into an unlinkable, private one.

### `lpad wrap` · `[online]`
Wrap native LEZ into WLEZ.

| Flag | Req | Default | Meaning |
|---|---|---|---|
| `--amount <U128>` | yes | - | Amount of LEZ to wrap. |
| `--from <ID>` | no | auto | Source account (auto-detected if omitted). |
```bash
lpad wrap --amount 3000
```

### `lpad unwrap` · `[online]`
Unwrap WLEZ back into native LEZ.
```bash
lpad unwrap --amount 1000
```

### `lpad shield` · `[online]`
Shield a public token holding into a private (shielded) holding.

| Flag | Req | Meaning |
|---|---|---|
| `--token-def <ID>` | yes | Token definition id of the holding. |
| `--amount <U128>` | yes | Amount to shield. |
```bash
lpad shield --token-def <TOKEN_DEF> --amount 1000
```

### `lpad deshield` · `[online]`
Deshield a shielded holding back into a public one.
```bash
lpad deshield --token-def <TOKEN_DEF> --amount 500
```

### `lpad shield-lez` · `[online]`
One shot: wrap native LEZ → WLEZ, then shield it.

| Flag | Req | Default | Meaning |
|---|---|---|---|
| `--amount <U128>` | yes | - | Amount of LEZ. |
| `--from <ID>` | no | auto | Source account. |
```bash
lpad shield-lez --amount 2000
```

### `lpad deshield-lez` · `[online]`
One shot: deshield WLEZ, then unwrap it back to native LEZ.
```bash
lpad deshield-lez --amount 2000
```

---

## Associated Token Accounts (ATAs)

ATAs are **deterministic token accounts per `(owner, token definition)`** - like
Solana's associated token accounts. The `bc buy-ata` / `bc sell-ata` / `lbp
buy-ata` variants satisfy the RFP requirement to **use ATAs for all token
interactions** (RFP-015 #7 / RFP-016 #9, per LP-0014 / RFP-008): the user's token
legs are routed through the on-chain `ata` program (which internally authorizes
the spend via the ATA's PDA when the named owner signs), while the program's
vaults stay program-owned PDAs.

Shared ATA flags (on every `*-ata` command):

| Flag | Req | Default | Meaning |
|---|---|---|---|
| `--owner <ID>` | no | wallet default signer | The account that owns the ATAs (and signs). |
| `--ata-program <ID>` | no | ata image id | The deployed ATA program id. |
| `--fund-from <ID>` | no | - | Move the input amount from this keypair holding into the owner's input ATA first (so you don't pre-fund it). |

### `lpad create-ata` · `[online]`
Create your Associated Token Account for a token definition (idempotent - a no-op
if it already exists). Buys/sells create the ATAs they need automatically, so this
is mainly for pre-creating or funding an ATA.

| Flag | Req | Default | Meaning |
|---|---|---|---|
| `--token-def <ID>` | yes | - | Token definition to create the ATA for. |
| `--owner <ID>` | no | wallet default | ATA owner. |
| `--ata-program <ID>` | no | ata image id | ATA program id. |
```bash
lpad create-ata --token-def <WLEZ_DEF>
lpad create-ata --token-def <WLEZ_DEF> --owner <OWNER>
```

> `bc buy-ata`, `bc sell-ata`, and `lbp buy-ata` are documented under their
> respective program sections below.

---

## Offline math & address derivation

No wallet or sequencer required - these compute results straight from the on-chain
libraries (byte-identical to what the programs enforce).

### `lpad bc quote` · `[offline]`
Tokens received for a collateral input on the bonding curve, plus price impact.

| Flag | Req | Default | Meaning |
|---|---|---|---|
| `--vt <U128>` | yes | - | Virtual token reserve. |
| `--vc <U128>` | yes | - | Virtual collateral reserve. |
| `--fee-bps <U128>` | no | `0` | Per-swap fee, basis points. |
| `--in <U128>` | yes | - | Collateral input. |
```bash
lpad bc quote --vt 2000000 --vc 50000 --fee-bps 100 --in 1000
```

### `lpad bc cost` · `[offline]`
Inverse quote: exact collateral cost to buy a target token quantity.

| Flag | Req | Default | Meaning |
|---|---|---|---|
| `--vt <U128>` / `--vc <U128>` | yes | - | Virtual reserves. |
| `--fee-bps <U128>` | no | `0` | Fee, bps. |
| `--tokens <U128>` | yes | - | Target tokens to buy (must be `< vt`). |
```bash
lpad bc cost --vt 2000000 --vc 50000 --fee-bps 100 --tokens 5000
```

### `lpad bc sell-quote` · `[offline]`
Collateral received for selling tokens back into the curve.
```bash
lpad bc sell-quote --vt 2000000 --vc 50000 --fee-bps 100 --tokens 5000
```

### `lpad bc ids` · `[offline]`
Derive the sale + token-vault + collateral-vault PDA addresses.

| Flag | Req | Default | Meaning |
|---|---|---|---|
| `--program <ID>` | no | bc image id | Program id. |
| `--token-def <ID>` | yes | - | Project token definition. |
| `--collateral-def <ID>` | yes | - | Collateral token definition. |
| `--creator <ID>` | yes | - | Sale creator account. |
| `--nonce <U64>` | no | `0` | Sale nonce (for multiple sales per creator). |
```bash
lpad bc ids --token-def <TOKEN_DEF> --collateral-def <WLEZ_DEF> --creator <CREATOR> --nonce 0
```

### `lpad lbp weight` · `[offline]`
Token (and collateral) weight at a point in time.

| Flag | Req | Meaning |
|---|---|---|
| `--w-start <W>` / `--w-end <W>` | yes | Start/end token weight (`0.99` or `99/100`). |
| `--t-start <U64>` / `--t-end <U64>` | yes | Window start/end (ms). |
| `--at <U64>` | yes | Time to evaluate (ms). |
```bash
lpad lbp weight --w-start 0.99 --w-end 0.01 --t-start 1000 --t-end 2000 --at 1500
```

### `lpad lbp quote` · `[offline]`
Buy quote at a point in time, including the weight-shifted spot price.

| Flag | Req | Meaning |
|---|---|---|
| `--reserve-token <U128>` / `--reserve-collateral <U128>` | yes | Pool reserves. |
| `--w-start` / `--w-end` / `--t-start` / `--t-end` / `--at` | yes | Weight schedule + eval time. |
| `--in <U128>` | yes | Collateral input. |
```bash
lpad lbp quote --reserve-token 1000000 --reserve-collateral 50000 \
  --w-start 0.99 --w-end 0.01 --t-start 1000 --t-end 2000 --at 1500 --in 1000
```

### `lpad lbp ids` · `[offline]`
Derive the pool + vault PDA addresses (same flags as `bc ids`).
```bash
lpad lbp ids --token-def <TOKEN_DEF> --collateral-def <WLEZ_DEF> --creator <CREATOR>
```

---

## Bonding curve (RFP-015)

Supply-driven constant-product curve over virtual reserves. Price rises with each
buy. Public, private (unlinkable), and ATA paths.

### `lpad bc create-token-sale` · `[online]`
One-shot launch: mint a token (on-chain name + symbol) **and** open a sale that
raises in native LEZ (WLEZ collateral).

| Flag | Req | Default | Meaning |
|---|---|---|---|
| `--name <S>` / `--symbol <S>` | yes | - | Token metadata (stored on-chain). |
| `--supply <U128>` | yes | - | Total minted supply. |
| `--sale-quantity <U128>` | yes | - | Tokens offered on the curve (`D`). |
| `--dex-seed <U128>` | no | `0` | Tokens held back for post-graduation liquidity (`R`). |
| `--vt <U128>` / `--vc <U128>` | yes | - | Virtual reserves (set the starting price). |
| `--fee-bps <U128>` | no | `0` | Per-swap fee, bps. |
| `--creator <ID>` | no | wallet default | Creator account. |
| `--private` | no | off | Fund the deposit from a shielded holding via an unlinkable creator account. |
| `--nonce <U64>` | no | `0` | Sale nonce. |
```bash
lpad bc create-token-sale --name DEMO --symbol DMO --supply 1000000 \
  --sale-quantity 500000 --dex-seed 50000 --vt 2000000 --vc 50000 --fee-bps 100
# unlinkable creator:
lpad bc create-token-sale --name PRIV --symbol PRV --supply 1000000 \
  --sale-quantity 200000 --vt 2000000 --vc 50000 --fee-bps 100 --private --nonce 1
```

### `lpad bc create-sale` · `[online]`
Lower-level: open a sale against an existing collateral token, depositing `D + R`
project tokens from an existing holding.

| Flag | Req | Default | Meaning |
|---|---|---|---|
| `--program <ID>` | no | bc image id | Program id. |
| `--collateral-def <ID>` | yes | - | Collateral token definition. |
| `--treasury <ID>` | yes | - | Fee treasury account. |
| `--creator-token-holding <ID>` | yes | - | Holding to source the deposit from. |
| `--creator <ID>` | yes | - | Creator account. |
| `--sale-quantity <U128>` | yes | - | Tokens offered (`D`). |
| `--dex-seed <U128>` | no | `0` | Reserve held back (`R`). |
| `--vt <U128>` / `--vc <U128>` | yes | - | Virtual reserves. |
| `--fee-bps <U128>` | no | `0` | Fee, bps. |
| `--one-directional` | no | off | Disable sell-back (buys only). |
| `--end-ts <U64>` | no | `0` | Optional end timestamp (ms; `0` = none). |
| `--min-duration <U64>` | no | `0` | Minimum sale duration (ms). |
| `--nonce <U64>` | no | `0` | Sale nonce. |
```bash
lpad bc create-sale --collateral-def <WLEZ_DEF> --treasury <TREASURY> \
  --creator-token-holding <HOLDING> --creator <CREATOR> \
  --sale-quantity 500000 --dex-seed 50000 --vt 2000000 --vc 50000 --fee-bps 100
```

### `lpad bc sale-info` · `[online]`
Read a sale's on-chain state (reserves, price, raise, name/symbol, status).
```bash
lpad bc sale-info --sale <SALE_ID>
```

### `lpad bc buy` · `[online]`
Buy from a sale (public path, keypair holdings).

| Flag | Req | Default | Meaning |
|---|---|---|---|
| `--program <ID>` | no | bc image id | Program id. |
| `--sale <ID>` | yes | - | Sale id. |
| `--buyer-collateral <ID>` | no | auto | Collateral holding (auto-detected). |
| `--buyer-token <ID>` | no | auto | Destination token holding (auto-detected). |
| `--in <U128>` | yes | - | Collateral to spend. |
| `--min-out <U128>` | no | from slippage | Minimum tokens to accept. |
| `--slippage-bps <U128>` | no | `100` | Slippage tolerance (1%). |
```bash
lpad bc buy --sale <SALE_ID> --in 1000
lpad bc buy --sale <SALE_ID> --in 1000 --min-out 19000   # explicit floor
```

### `lpad bc buy-ata` · `[online]`
Buy where the buyer's collateral and tokens use **ATAs** (RFP Func: ATAs). The
collateral and token ATAs are created automatically if absent; `--fund-from`
seeds the collateral ATA first.

| Flag | Req | Default | Meaning |
|---|---|---|---|
| `--sale <ID>` | yes | - | Sale id. |
| `--in <U128>` | yes | - | Collateral to spend. |
| `--owner <ID>` | no | wallet default | ATA owner / signer. |
| `--fund-from <ID>` | no | - | Keypair holding to seed the collateral ATA from. |
| `--program <ID>` / `--ata-program <ID>` | no | image ids | Program ids. |
| `--min-out <U128>` / `--slippage-bps <U128>` | no | 1% floor | Slippage. |
```bash
# seed the owner's WLEZ ATA from a keypair holding, then buy via ATAs:
lpad bc buy-ata --sale <SALE_ID> --in 1000 --fund-from <WLEZ_HOLDING>
```

### `lpad bc buy-private` · `[online]`
Private buy: deshield → public buy → re-shield through a fresh ephemeral
account `A`, so the purchase never links to your private account.

| Flag | Req | Default | Meaning |
|---|---|---|---|
| `--program <ID>` | no | bc image id | Program id. |
| `--sale <ID>` | yes | - | Sale id. |
| `--user-collateral <ID>` | no | auto | Shielded collateral source. |
| `--user-token <ID>` | no | auto | Shielded token destination. |
| `--in <U128>` | yes | - | Collateral to spend. |
| `--min-out <U128>` | no | from slippage | Minimum tokens. |
| `--slippage-bps <U128>` | no | `100` | Slippage tolerance. |
```bash
lpad bc buy-private --sale <SALE_ID> --in 1000
```

### `lpad bc sell` · `[online]`
Sell tokens back into a sale (public path).

| Flag | Req | Default | Meaning |
|---|---|---|---|
| `--program <ID>` | no | bc image id | Program id. |
| `--sale <ID>` | yes | - | Sale id. |
| `--seller-token <ID>` | yes | - | Token holding to sell from. |
| `--seller-collateral <ID>` | yes | - | Collateral destination holding. |
| `--tokens <U128>` | yes | - | Tokens to sell. |
| `--min-out <U128>` | no | from slippage | Minimum collateral. |
| `--slippage-bps <U128>` | no | `100` | Slippage tolerance. |
```bash
lpad bc sell --sale <SALE_ID> --seller-token <TOK> --seller-collateral <COLL> --tokens 5000
```

### `lpad bc sell-ata` · `[online]`
Sell where the seller's tokens and collateral use **ATAs**. `--fund-from` seeds the
token ATA first.

| Flag | Req | Default | Meaning |
|---|---|---|---|
| `--sale <ID>` | yes | - | Sale id. |
| `--tokens <U128>` | yes | - | Tokens to sell. |
| `--owner <ID>` | no | wallet default | ATA owner / signer. |
| `--fund-from <ID>` | no | - | Keypair holding to seed the token ATA from. |
| `--program <ID>` / `--ata-program <ID>` | no | image ids | Program ids. |
| `--min-out <U128>` / `--slippage-bps <U128>` | no | 1% floor | Slippage. |
```bash
lpad bc sell-ata --sale <SALE_ID> --tokens 500 --fund-from <TOKEN_HOLDING>
```

### `lpad bc sell-private` · `[online]`
Private sell: deshield tokens → public sell → re-shield collateral.
```bash
lpad bc sell-private --sale <SALE_ID> --tokens 5000
```

### `lpad bc close` · `[online]`
Close a sale (creator only).
```bash
lpad bc close --sale <SALE_ID> --creator <CREATOR>
```

### `lpad bc withdraw` · `[online]`
Withdraw raised collateral + remaining tokens (creator only).

| Flag | Req | Meaning |
|---|---|---|
| `--sale <ID>` | yes | Sale id. |
| `--creator-collateral <ID>` | yes | Collateral destination. |
| `--creator-token <ID>` | yes | Token destination. |
| `--creator <ID>` | yes | Creator account. |
```bash
lpad bc withdraw --sale <SALE_ID> --creator-collateral <COLL> --creator-token <TOK> --creator <CREATOR>
```

---

## LBP (RFP-016)

Time-driven Balancer weight-shifting pool: the token weight declines over the
window, so the price falls unless buying pressure counteracts it.

### `lpad lbp create-sale` · `[online]`
Open an LBP pool, depositing project tokens. Weights given in `[0,1]` as `0.99` or
`99/100`.

| Flag | Req | Default | Meaning |
|---|---|---|---|
| `--program <ID>` | no | lbp image id | Program id. |
| `--collateral-def <ID>` | yes | - | Collateral token definition. |
| `--treasury <ID>` | yes | - | Fee treasury account. |
| `--creator-token-holding <ID>` | yes | - | Source of the token deposit. |
| `--creator-collateral-holding <ID>` | yes | - | Source of the collateral seed. |
| `--creator <ID>` | yes | - | Creator account. |
| `--token-deposit <U128>` | yes | - | Project tokens deposited. |
| `--collateral-seed <U128>` | no | `1` | Initial collateral reserve. |
| `--w-start <W>` / `--w-end <W>` | yes | - | Start/end token weight. |
| `--t-start <U64>` / `--t-end <U64>` | yes | - | Window start/end (ms). |
| `--fee-bps <U128>` | no | `0` | Protocol fee (collected at close, on raised collateral). |
| `--block-ceiling <U128>` | no | `0` | Optional per-block token-out cap (`0` = none). |
| `--allowlist-root <HEX>` | no | `""` | Merkle root of an eligibility allowlist (`""` = open). |
| `--fixed-price` | no | off | Hold the price fixed (no weight shift). |
| `--min-duration <U64>` | no | `0` | Minimum pool duration (ms). |
| `--nonce <U64>` | no | `0` | Pool nonce. |
```bash
lpad lbp create-sale --collateral-def <WLEZ_DEF> --treasury <TREASURY> \
  --creator-token-holding <TOK> --creator-collateral-holding <COLL> --creator <CREATOR> \
  --token-deposit 1000000 --collateral-seed 50000 \
  --w-start 0.99 --w-end 0.01 --t-start 1700000000000 --t-end 1700086400000 --fee-bps 100
```

### `lpad lbp pool-info` · `[online]`
Read a pool's on-chain state, evaluated at time `--at` (ms).

| Flag | Req | Default | Meaning |
|---|---|---|---|
| `--pool <ID>` | yes | - | Pool id. |
| `--at <U64>` | no | `0` | Time to evaluate the weight/price at. |
```bash
lpad lbp pool-info --pool <POOL_ID> --at 1700043200000
```

### `lpad lbp buy` · `[online]`
Buy from a pool (public path). Same flags as `bc buy`, with `--pool` instead of
`--sale`.
```bash
lpad lbp buy --pool <POOL_ID> --in 1000
```

### `lpad lbp buy-ata` · `[online]`
Buy from a pool where the buyer side uses **ATAs**. Same flags as `bc buy-ata`,
with `--pool` instead of `--sale`.
```bash
lpad lbp buy-ata --pool <POOL_ID> --in 1000 --fund-from <WLEZ_HOLDING>
```

### `lpad lbp buy-private` · `[online]`
Private buy via a fresh ephemeral account; the public buy leg is priced at the
live on-chain clock (no in-proof clock to drift). Same flags as `bc buy-private`,
with `--pool`.
```bash
lpad lbp buy-private --pool <POOL_ID> --in 1000
```

### `lpad lbp pause` · `[online]`
Pause a pool (creator). Note: this does **not** halt weight progression.
```bash
lpad lbp pause --pool <POOL_ID> --creator <CREATOR>
```

### `lpad lbp resume` · `[online]`
Resume a paused pool (creator).
```bash
lpad lbp resume --pool <POOL_ID> --creator <CREATOR>
```

### `lpad lbp poke` · `[online]`
Refresh the stored weight for off-chain readers (pricing is computed lazily, so
this is purely cosmetic for indexers).
```bash
lpad lbp poke --pool <POOL_ID>
```

### `lpad lbp close` · `[online]`
Close a pool (creator).
```bash
lpad lbp close --pool <POOL_ID> --creator <CREATOR>
```

### `lpad lbp withdraw` · `[online]`
Withdraw raised collateral + unsold tokens, minus the at-close fee (creator).

| Flag | Req | Meaning |
|---|---|---|
| `--pool <ID>` | yes | Pool id. |
| `--creator-collateral <ID>` | yes | Collateral destination. |
| `--creator-token <ID>` | yes | Token destination. |
| `--creator <ID>` | yes | Creator account. |
```bash
lpad lbp withdraw --pool <POOL_ID> --creator-collateral <COLL> --creator-token <TOK> --creator <CREATOR>
```

---

## JSON output

Add `--json` to any command for machine-readable output (useful for scripting /
piping into `jq`):

```bash
lpad bc quote --vt 2000000 --vc 50000 --fee-bps 100 --in 1000 --json
lpad bc create-token-sale --name DEMO --symbol DMO --supply 1000000 \
  --sale-quantity 500000 --vt 2000000 --vc 50000 --json | jq -r .sale
```
