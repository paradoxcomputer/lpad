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
- [Plain token holdings](#plain-token-holdings)
- [Offline math & address derivation](#offline-math--address-derivation)
- [Bonding curve (RFP-015)](#bonding-curve-rfp-015)
- [LBP (RFP-016)](#lbp-rfp-016)
- [JSON output](#json-output)

---

## Setup

```bash
bash setup.sh                 # verify the toolchain (Docker + cargo-risczero, libpcsclite)
bash scripts/install-cli.sh   # install `lpad` to ~/.local/bin
bash scripts/bootstrap.sh     # deploy programs + create the ~/.lpad wallet + a demo sale
```

`bootstrap.sh` is the one step that needs a LEZ checkout. It does not build against
LEZ, it drives it: it shells out to the upstream `wallet` binary for key management,
deploys and transfers, and seeds its wallet config from
`lez/wallet/configs/debug/wallet_config.json`. So clone LEZ at tag `v0.2.4` to
`$HOME/lez` (or point `$LPAD_LEZ_DIR` at it) and build the wallet there first:

```bash
git clone --branch v0.2.4 https://github.com/logos-blockchain/logos-execution-zone.git ~/lez
(cd ~/lez && cargo build --release -p wallet)
```

`$LPAD_WALLET_BIN` relocates the binary alone; the checkout is still needed for the
config.

Deploying is not what makes that true any more: `lpad` deploys the three launchpad
programs itself, through the SDK, with no wallet binary and no checkout - see
[`lpad network`](#lpad-network--online). Bootstrap keeps driving the wallet binary
for everything that has no `lpad` equivalent (create keypairs, mint tokens, claim the
faucet, send native LEZ) and because it must run unattended, while the CLI's deploy
sits behind a prompt.

After `bootstrap.sh`, the wallet lives at `~/.lpad` and `lpad` auto-detects it
(program ids, your holdings, dev/real proof mode) - **no environment variables
are required** for normal use. The first command that opens that wallet also asks,
once, which sequencer it talks to, and remembers the answer.

---

## Global flags & environment

These flags work on **every** command:

| Flag | Default | Meaning |
|---|---|---|
| `--json` | off | Emit machine-readable JSON instead of human-readable text. |
| `--config <PATH>` | `$LEE_WALLET_HOME_DIR/wallet_config.json` | Wallet config file. |
| `--storage <PATH>` | `$LEE_WALLET_HOME_DIR/storage.json` | Wallet storage file. |
| `--network <NET>` | the choice remembered for this wallet (see below) | Sequencer to target **for this invocation only**: `testnet` (or `logos`), `paradox`, `local`, `local=<url>`, or an `http(s)://…` URL. lpad bundles no sequencer - `local` is a LEZ node you run. |

Wallet location is resolved as: **`--config`/`--storage` → `$LEE_WALLET_HOME_DIR`
→ `~/.lpad`** (the default home).

### Which sequencer, and where the answer is kept

The sequencer is resolved in this order:

1. `--network` on the command line - used, and deliberately **never written back**;
2. the choice remembered for this wallet, in a `network` file beside the wallet
   config (the same directory as the `proof_mode` marker below);
3. on a first run with a terminal, the picker - one question, then remembered;
4. otherwise the sequencer the wallet's own config already names, which is what
   `lpad` has always done.

The picker offers three choices, and **lpad ships no sequencer**:

| # | Choice | Sequencer | Who runs it |
|---|---|---|---|
| 1 | Logos testnet | `https://testnet.lez.logos.co` | Logos |
| 2 | Paradox | `https://seq-testnet.paradox.computer` | Paradox Computer |
| 3 | Local | `http://127.0.0.1:3040`, **editable at the prompt** | **you** - a LEZ node you install and run |

It is stored in the form `--network` reads back - `testnet`, `paradox`,
`local=<url>`, or a bare URL - so an edited local URL survives, and you can set it
without a terminal by writing the file yourself:

```bash
printf 'testnet\n' > ~/.lpad/network
printf 'local=http://127.0.0.1:3040\n' > ~/.lpad/network
```

The choice lives **per wallet**, not globally, for the same reason `bootstrap.sh`
gives each network its own home: a wallet's accounts, shielded notes and sale ids
are PDAs on one chain, so one global setting would be wrong for every wallet but
one, and `$LEE_WALLET_HOME_DIR=~/.lpad-paradox` would silently keep aiming at
testnet. An explicit `--config` therefore carries its own selection, exactly as it
carries its own `proof_mode`.

The picker **never** fires without a terminal on *both* stdin and stderr, and never
under `--json`: a menu written to a redirected stderr is a program waiting on a
question nobody saw, which is precisely the hang that `scripts/test-all-cli.sh` and
the CI gate must not hit. Without a terminal the resolution falls through to (4)
above, unchanged from before this flag existed. Everything the picker prints goes to
stderr, so a `--json` stdout stays parseable.

A `network` file that does not parse is a hard error rather than a silent fallback -
believing a typo would aim the next transaction at a chain you did not pick, and
that does not fail loudly, it fails as accounts that read as absent. `lpad network`
is the one exception: it warns and re-asks, because failing there would lock you out
of the only command that repairs the file.

`--network` is applied as a wallet-config override, so it repoints the sequencer for
that one invocation and never rewrites either the config file or the `network`
marker. It does **not** move the wallet
with it: the wallet home is resolved independently, by the rule just above. A wallet
holds per-chain accounts and shielded notes, so `lpad --network paradox status` prints a
Paradox block height beside the sync height of whatever wallet `$LEE_WALLET_HOME_DIR`
(or `~/.lpad`) points at - typically the testnet one, which makes the two numbers
unrelated. Switch both: the bootstrap keeps `~/.lpad` for `testnet` and `~/.lpad-paradox`
for `paradox`.

Program ids are the **same on every network**: a program's id *is* the RISC0 image id
of its committed ELF, so `lpad program-id bc` prints one answer everywhere and there
is no per-network id table. What a network decides is only whether those programs are
*deployed* there - which is why picking `local` also offers to deploy them (see
[`lpad network`](#lpad-network--online)).

| Environment variable | Purpose |
|---|---|
| `LEE_WALLET_HOME_DIR` | Wallet home directory (overrides the `~/.lpad` default). |
| `NSSA_WALLET_HOME_DIR` | The pre-rename name for the same thing, still accepted; `LEE_WALLET_HOME_DIR` wins if both are set. |
| `RISC0_DEV_MODE` | **Real proofs by default.** Set `1` for dev/fake proofs (fast). If unset, `lpad` honors a `proof_mode` marker (`dev`/`real`) next to the wallet (the bootstrap records the sequencer's mode); absent a marker it uses real proofs. |
| `LPAD_TX_TTL_MS` | Transaction deadline TTL in ms (default `120000`). |
| `LPAD_PRIVATE_TX_TTL_MS` | The same deadline for a transaction whose proof is generated *here* - the `buy-disposable` paths (default `3600000`, one hour). The public 120s is useless against a real recursive STARK: proving alone measured 38-40 minutes on Logos testnet, so a 120s deadline expires before the transaction can be submitted. Both guests clamp the window they emit to their own cap regardless, so a long TTL cannot buy a longer free option than the guest allows. |
| `LPAD_SYNC_STALL_SECS` | How long a private (shielded) note sync may scan **no block at all** before `lpad` calls the sequencer's block stream wedged rather than slow (default `300`). The clock restarts every time the sync advances a block, so this never kills a slow-but-healthy first scan. Raise it only for a sequencer that genuinely takes minutes per block. |
| `LPAD_SYNC_MAX_SECS` | Ceiling on one private sync, however healthy it looks (default `3600`). The backstop for a chain that produces blocks faster than this wallet scans them, where every window makes progress and the sync would otherwise never reach head. Giving up is cheap: every scanned block is persisted, so the next command resumes where this one stopped. |
| `LPAD_FAUCET_MAX_ATTEMPTS` | Hash budget for the faucet's proof of work (default `268435456`). Each extra difficulty byte costs 256x more work, so an unreachable challenge ends as an error naming this knob rather than a command that looks hung. |
| `NO_COLOR`, `CLICOLOR_FORCE`, `CLICOLOR` | Colour switches, in that order of precedence. `NO_COLOR` non-empty wins outright (per no-color.org) and is not overridable; `CLICOLOR_FORCE` non-empty and not `0` colours even off a TTY, for `... 2>&1 \| less -R`; `CLICOLOR=0` turns colour off. Otherwise colour follows the stream. |
| `COLUMNS` | Width `lpad` wraps prose to when the terminal has not said otherwise (default `80`). |
| `LPAD_DEPLOY_SCAN_BLOCKS` | How far back to scan for a guest's ELF bytes when deciding whether it is already deployed, and when verifying a deploy that reported no block (default `400`). The one variable `lpad` and `scripts/bootstrap.sh` genuinely share, deliberately: they run the same check, and a deployment older than the window reads as "no record" in both. |

Two **marker files** sit beside the wallet config and are read the same way, so a
wallet carries its own settings wherever it is copied or pointed at:

| File | Written by | Meaning |
|---|---|---|
| `proof_mode` | `bootstrap.sh` | `dev` or `real`; overridden by `RISC0_DEV_MODE`. |
| `network` | the picker, or `lpad network`, or you | Which sequencer this wallet talks to, in `--network`'s own vocabulary; overridden for one invocation by `--network`. |

That is the whole list, plus `HOME` to locate the default `~/.lpad`. In particular
there are no `LPAD_*_ELF` paths and no `LPAD_REPO`: the guest ELFs are compiled into
the binary from the committed artifacts, so there is nothing to point at and
`scripts/install-cli.sh` sets nothing. The scripts under `scripts/` read more of their
own (`LPAD_NETWORK`, `LPAD_WALLET_HOME`, `LPAD_LEZ_DIR`, …), and apart from
`LPAD_DEPLOY_SCAN_BLOCKS` above none of those reach `lpad` under those names - the
scripts translate them into `--network` and `LEE_WALLET_HOME_DIR` first.

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

### `lpad init-wallet` · `[online]`
Create a wallet from nothing. **The first command a bare install can run** - every
other command opens a wallet that must already exist, and until this landed nothing
in lpad could produce one: only the LEZ `wallet` binary could write `storage.json`,
which meant a whole LEZ checkout for the one step that has to come first.

```bash
lpad --network testnet init-wallet
lpad --network testnet --json init-wallet   # {"ok":true,"mnemonic":"…","storage_encrypted":false,…}
```

It **refuses to overwrite** an existing wallet. Fresh keys written over live ones
strand every balance behind the old ones permanently, with no undo.

It writes three things: `storage.json` (the keys), `wallet_config.json`, and the
`network` marker, so the picker never fires for this wallet. The config gets the
polling numbers `scripts/bootstrap.sh` uses - `seq_poll_timeout` 30s,
`seq_poll_max_retries` 60, `seq_tx_poll_max_blocks` 40, `calibration_limit` 3 -
rather than LEZ's defaults. That is load-bearing, not cosmetic: the defaults are
12s x 5 = **60 seconds of patience against a block every ~46-50s**, so a
transaction that lands in the very next block comes back as `All pollers failed`.

> **Your keys are not encrypted, and there is no password.**
> LEZ v0.2.4 accepts a wallet password and throws it away - `Storage::new` is a
> `// TODO: Use password for storage encryption` followed by `let _ = password;`,
> `Storage::from_path` takes no password at all, and `save_to_path` writes plain
> `serde_json`. So `storage.json` holds raw signing and spending keys as cleartext
> JSON. lpad does not prompt for a password rather than imply a protection that is
> not there. It creates the file **`0600`** (the default is 0644 minus umask -
> group-readable `0664` on a stock Ubuntu), and that file mode is the only thing
> protecting the keys. **Back it up as you would a private key**, and keep the
> 24-word recovery phrase: it is printed once, stored nowhere, and is the only way
> back to those keys.

Next: `lpad faucet`, then `lpad wrap --amount 100`.

### `lpad status` · `[online]`
Sequencer block height + wallet sync status.
```bash
lpad status
```

### `lpad network` · `[online]`
Choose which sequencer this wallet talks to, and remember it. Re-opens the same
picker the first run shows (see
[Which sequencer](#which-sequencer-and-where-the-answer-is-kept)) and writes the
answer to the `network` file beside the wallet config, so later commands are silent.

| Form | What it does |
|---|---|
| `lpad network` | asks, remembers, and - on a local pick - checks/deploys (below). |
| `lpad network --json` | **reports** the current selection; asks nothing, contacts nothing. |
| `lpad --network <NET> network` | prints that override and says plainly it was **not** stored. |
| `lpad --network <NET> network --check` | asks the **chain** whether this build's three programs are on it. No picking, no terminal needed; exits 1 if any is missing. |

Needs a terminal on stdin *and* stderr. Without one it does not wait on input that
will never come: it exits with the exact `printf … > <path>` line that sets the
choice non-interactively.

`--json` is the machine-readable surface, so it reports rather than prompts:

```bash
lpad network --json
# {"network":"paradox","url":"https://seq-testnet.paradox.computer","display_name":"Paradox",
#  "source":"stored","marker":"/home/you/.lpad/network","wallet_config":"/home/you/.lpad/wallet_config.json"}
```

`source` is `stored` (the marker), `flag` (`--network` this invocation) or
`wallet-config` (nothing recorded - the sequencer in the wallet's own config, read
straight out of the JSON file). That last read is deliberately not "open the wallet
and ask it": opening calibrates sequencers over the network, which is far too much
work for one line, and this command has to stay answerable while the chain is down -
`network --json` makes no RPC at all.

`--check` is that same question asked deliberately, from a script:

```bash
lpad --network testnet network --check
# lpad programs on https://testnet.lez.logos.co
#   bonding_curve         block 12609  1fe89b0f1888e6554a27c35b2c35ae5a6c5879d8fe0cb784c31ce3331fb93d33
#   lbp                   block 12610  153a85520d6023c9d8adeb45e58b93027aa26d8a3a30fca51c81988170fbd928
#   wlez                  block 12611  d37f6d4de769f06975f7eccfa494a0592ac4846b5a260fb17c44232771353084
# ✓ all three programs are on this chain
```

It **exits 1** when a program has no record, and that is the point: a transaction
against a program that is not deployed is dropped from the mempool and reads as a
*timeout*, not an error, so a script that carries on spends its whole poll budget -
30 minutes per transaction on the bootstrap's config - to learn nothing. Under
`--json` it reports `deployed`, a `missing` list, and per-program `block`.

"No record" is **not** proof of absence: a chain that was reset or restored can have
forgotten the deploy transaction while still running the program. `bash
scripts/verify-deployment.sh` asks the stronger question - is lpad's on-chain state
still there, and does its owner match this build's id - and is the one to believe if
the two disagree.

**After a local pick**, `lpad network` checks whether lpad's three programs
(`bonding_curve`, `lbp`, `wlez`) are on that chain and offers to deploy them, because
a chain you just started has none, and a transaction against a program that is not
deployed is dropped from the mempool and reads as a *timeout* rather than an error.
Deploying goes through the SDK - no `wallet` binary, no LEZ checkout - and:

- is **idempotent**. A deploy transaction's hash is a pure function of its bytecode,
  so a program already on chain is reported as `(was already on chain)` and nothing
  is submitted. That is not tidiness: a duplicate is never re-included, so submitting
  anyway would burn the wallet's entire poll budget (~30 min) per program proving
  nothing.
- **verifies the guest's own ELF bytes are in a block** rather than trusting the
  wallet's report, with a bounded, always-reported backward scan as the fallback
  (`$LPAD_DEPLOY_SCAN_BLOCKS`, default 400 - the same knob `scripts/bootstrap.sh`
  uses). A failure is an error naming the remedy, never a warning.

"No record" is not proof of absence - a sequencer that was reset or restored can have
forgotten the deploy transaction while still running the program - so the CLI says so
and re-deploying is a no-op either way.

On the two named chains, and on a bare URL, missing programs are **reported and never
offered**: those are other people's chains, an accidental deploy to one should not be
one keystroke away, and `local=<url>` is exactly how you tell lpad a chain is yours.
What it prints there is the standing check:

```bash
LPAD_NETWORK=paradox bash scripts/verify-deployment.sh
```

```bash
lpad network                 # pick / re-pick, and deploy to a local chain if asked
lpad network --json          # what am I pointed at, and where is that recorded?
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
lpad my-sales --refresh      # re-derive every candidate id, ignoring the cache
lpad my-sales --deep         # ...and treat every account as a possible creator
```
With no event indexer (LP-0012) there is nothing to ask "what sales exist?", so
ids are re-derived from your accounts and the token definitions you hold. Two
things keep that from being a coffee break:

* only **identity** accounts are tried as creators - accounts owned by the token
  or ATA program are token definitions, holdings and metadata, never a sale's
  creator, and they are the bulk of a wallet that has launched a few tokens;
* the ids that resolve are remembered in `lpad_discovery.json` **next to your
  wallet** (alongside `statistics.json` and the `proof_mode` marker), keyed by
  program id, and a later run re-reads just those. Only ids are cached - every
  status, reserve and raised figure printed is read live, so nothing here can go
  stale. Rebuild the guests and the program id changes, which is a cache miss and
  a full rescan, never a wrong answer.

Sales this wallet creates are remembered as they are made. Use `--refresh` for
one created somewhere else - another machine holding the same key, or a wallet
home restored from backup. `--deep` additionally drops the creator prune, and is
only needed for a sale created with `--creator <a token holding>` rather than an
identity account. A corrupt or unreadable cache warns on stderr and rescans.

**A listing can be short, and it says so.** When the chain's creator index names
a sale this run could not read - a lagging or flaky sequencer - that sale is
neither dropped nor guessed at: the human output warns per id and again under the
rows, and `--json` carries the fact in the payload:

```json
{ "sales": [ ... ], "partial": true,
  "unreadable": [ { "sale": "…", "error": "…" } ] }
```

`partial` is always present, `false` on a healthy run, so a script can test it
without checking whether the key exists. The exit code stays **0**: this is a
partial success, the rows that are there are real, and failing hard would throw
them away too. Scripts must branch on `partial`, not on the exit code -
`jq -e 'if .partial then error("incomplete") else .sales end'`. A missing row in
a `partial` listing means "could not read", never "that sale is gone".

### `lpad my-pools` · `[online]`
LBP pools your wallet created. Same discovery, cache, flags and
`partial`/`unreadable` contract as `my-sales`.
```bash
lpad my-pools
lpad my-pools --refresh
```

### `lpad program-id` · `[offline]`
Print a program's deployed id (RISC0 image id) from its guest ELF. The id is a
compile-time constant - the ELFs are committed and embedded - so this needs no
wallet and no chain.

`wlez` additionally prints the two PDAs it owns (the WLEZ definition and the
vault), derived from the program id alone. They are what a deployment check has to
work with: unlike a sale or a pool, nothing records a wlez account id anywhere.

| Arg | Req | Meaning |
|---|---|---|
| `<which>` | yes | `bc`, `lbp`, `ata`, or `wlez`. |
```bash
lpad program-id bc
lpad program-id lbp
lpad program-id ata
lpad program-id wlez --json   # {"program":"wlez","program_id":"…","definition":"…","vault":"…"}
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

Shared ATA flags:

| Flag | Req | Default | Meaning | On |
|---|---|---|---|---|
| `--owner <ID>` | no | wallet default signer | The account that owns the ATAs (and signs). | all |
| `--ata-program <ID>` | no | ata image id | The deployed ATA program id. | all |
| `--fund-from <ID>` | no | - | Move the input amount from this keypair holding into the owner's input ATA first (so you don't pre-fund it). | the `*-ata` trades only |

`--fund-from` is on `bc buy-ata`, `bc sell-ata` and `lbp buy-ata`. `create-ata`
does not take it: it only claims the ATA, and there is no input amount to move.

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

## Plain token holdings

### `lpad init-holding` · `[online]`
Create **and initialise** a fresh public token holding for a token definition, and
print its id.

This exists because a token transfer to a never-initialised holding is *rejected*:
the token program claims the recipient with `Claim::Authorized`, and the runtime
grants that only for an account the transaction signed for - which the wallet does
not do for transfer recipients. The LEZ wallet has no equivalent subcommand, so
this is how you make a recipient before sending to it. The rejection is silent
(the transaction is dropped from the mempool), so skipping this step surfaces much
later as an unexplained timeout.

Prefer an ATA (`create-ata`) when the holding is meant to be discoverable from an
owner + token definition; use `init-holding` for a bare keypair holding.

| Flag | Req | Default | Meaning |
|---|---|---|---|
| `--token-def <ID>` | yes | - | Token definition the holding is for. |
```bash
lpad init-holding --token-def <COLL_DEF>
lpad init-holding --token-def <COLL_DEF> --json   # {"holding": "..."}
```

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

### `lpad lbp allowlist-leaf` · `[offline]`
Compute the allowlist Merkle leaf for an account's collateral holding. For a
single-member allowlist this hex is the `--allowlist-root` to pass to `lbp
create-sale`; larger allowlists hash sorted leaf pairs to build the root. `lbp
buy-gated` derives the same leaf from the buyer automatically.
```bash
lpad lbp allowlist-leaf --account <BUYER_COLLATERAL_HOLDING>
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

**Output - write down the last two.** The command prints `sale id`, `token def`,
`creator` and `creator token` (and all four under `--json`, as `sale`,
`token_definition`, `creator`, `creator_token_holding`). `bc close --creator` and
`bc withdraw --creator --creator-token` accept no other accounts.

> **`--private`: this output is the only record.** The unlinkable creator and its
> token holding are minted inside this command. Your wallet holds their keys
> without knowing what they are for, and nothing on chain connects them to you -
> that is the point of the flag, and it is also why losing the two ids strands
> the raise. `bc sale-info --sale <id>` recovers the **creator** later (it is
> plain state on the sale, readable by anyone); the creator's token holding it
> cannot.
>
> Showing these ids to their owner in their own terminal does not weaken the
> launch: unlinkability is a property of what was published on chain, not of what
> your own process knows. Publishing them **beside the sale id** does weaken it -
> that pair is precisely the link the shield/deshield round trip removed. Keep
> them somewhere private (a password manager, not a paste bin, not an issue).

### `lpad bc create-sale` · `[online]`
Lower-level: open a sale against an existing collateral token, depositing `D + R`
project tokens from an existing holding.

| Flag | Req | Default | Meaning |
|---|---|---|---|
| `--program <ID>` | no | bc image id | Program id. |
| `--collateral-def <ID>` | yes | - | Collateral token definition. |
| `--treasury <ID>` | yes | - | Fee treasury account. Settled by `bc sweep-treasury`, never by `withdraw`. |
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

> **Pick the treasury carefully, and make it first.** The treasury is pinned at
> creation and nothing ever rewrites it, and `sweep-treasury` is the only
> instruction that can pay the escrow out - so a treasury that cannot receive
> strands that money permanently. Creation is therefore where every unreceivable
> shape is rejected: the guest requires an **already-initialised Fungible holding
> of the collateral definition, owned by that definition's own token program**,
> and the SDK checks the same thing client-side before it signs. A fresh key is
> NOT enough, and a sweep cannot bootstrap one - nothing signs for the treasury.
> Make it with `lpad init-holding --token-def <collateral-def>` and pass the id
> it prints.

### `lpad bc sale-info` · `[online]`
Read a sale's on-chain state (reserves, price, raise, name/symbol, status), plus
the **creator** - the account `bc close --creator` and `bc withdraw --creator`
require, and the only way back to it once a `create-token-sale --private` launch
has scrolled off screen. It is plain on-chain state that any reader of the sale
can read, so reporting it discloses nothing new; what `--private` protects is the
tie between that creator and the wallet that funded it, and that is untouched.
```bash
lpad bc sale-info --sale <SALE_ID>
lpad --json bc sale-info --sale <SALE_ID> | jq -r .creator
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

### `lpad bc buy-disposable` · `[online]`
Private buy in **one** atomic transaction. Both of your holdings - the collateral
you pay with and the tokens you receive - are private account slots of a single
proof, so the debit and the credit happen together: no ephemeral account, no
deshield leg, no re-shield leg, and no block in which the trade is publicly
visible. `buy-private` remains the default; this is a second mode, and it is the
saga that has been run against a real sequencer.

Its protocol fee is **escrowed** in the sale's collateral vault rather than paid
to the treasury during the buy - a privacy proof pins every public account it
touches byte-for-byte, and a treasury shared across sales changes constantly.
`bc sweep-treasury` settles it.

Slower than any public path: this proves the buy locally as a real recursive
STARK, which takes minutes rather than seconds.

| Flag | Req | Default | Meaning |
|---|---|---|---|
| `--program <ID>` | no | bc image id | Program id. |
| `--sale <ID>` | yes | - | Sale id. |
| `--user-collateral <ID>` | no | auto | Private collateral note to spend. |
| `--user-token <ID>` | no | auto | Private token note to credit. |
| `--in <U128>` | yes | - | Collateral to spend. |
| `--min-out <U128>` | no | from slippage | Minimum tokens. |
| `--slippage-bps <U128>` | no | `100` | Slippage tolerance. |
```bash
lpad bc buy-disposable --sale <SALE_ID> --in 1000
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
Withdraw raised collateral + remaining tokens (creator only). It names **no
treasury account**: any protocol fee escrowed by a private buy stays in the
collateral vault, and the creator is paid the rest. Settle the escrow separately
with `bc sweep-treasury`.

| Flag | Req | Meaning |
|---|---|---|
| `--sale <ID>` | yes | Sale id. |
| `--creator-collateral <ID>` | yes | Collateral destination. |
| `--creator-token <ID>` | yes | Token destination. |
| `--creator <ID>` | yes | Creator account. |
```bash
lpad bc withdraw --sale <SALE_ID> --creator-collateral <COLL> --creator-token <TOK> --creator <CREATOR>
```

### `lpad bc sweep-treasury` · `[online]`
Hand the escrowed protocol fee over to the treasury pinned into the sale at
creation. `bc sale-info` reports the outstanding amount.

**Permissionless - there is no `--creator` flag**, because there is no authority
to prove: the payer, the recipient and the amount all come from the sale's own
state, so the most a stranger can accomplish is delivering the fee to its
rightful owner while paying the gas. Works whether the sale is open or closed,
and errors when nothing is owed.

**No exceptions, and no signer at all.** Older lpad builds documented one - the
first sweep to a never-initialised treasury signing for itself so the fee
transfer could create it - and that is gone. `create-sale` refuses a treasury
that is not already an initialised holding of the collateral definition, and the
guest rejects a DEFAULT-owned treasury before it looks at anything else,
whatever signs the transaction. So "the treasury is unowned" is not a
wallet-side problem: it means the sale's pinned treasury disagrees with chain
state. Retrying cannot help, and it wants reporting.

| Flag | Req | Default | Meaning |
|---|---|---|---|
| `--program <ID>` | no | bc image id | Program id. |
| `--sale <ID>` | yes | - | Sale id. |
```bash
lpad bc sweep-treasury --sale <SALE_ID>
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
| `--treasury <ID>` | yes | - | Fee treasury account. Settled by `lbp sweep-treasury`, never by `withdraw`. |
| `--creator-token-holding <ID>` | yes | - | Source of the token deposit. |
| `--creator-collateral-holding <ID>` | yes | - | Source of the collateral seed. |
| `--creator <ID>` | yes | - | Creator account. |
| `--token-deposit <U128>` | yes | - | Project tokens deposited. |
| `--collateral-seed <U128>` | no | `1` | Initial collateral reserve. |
| `--w-start <W>` / `--w-end <W>` | yes | - | Start/end token weight. |
| `--t-start <U64>` / `--t-end <U64>` | yes | - | Window start/end (ms). |
| `--fee-bps <U128>` | no | `0` | Protocol fee, charged at withdrawal on raised collateral only (never on the creator's own seed), then escrowed for `lbp sweep-treasury`. |
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

> **Pick the treasury carefully, and make it first.** The treasury is pinned at
> creation and nothing ever rewrites it, and `sweep-treasury` is the only
> instruction that can pay the escrow out - so a treasury that cannot receive
> strands that money permanently. Creation is therefore where every unreceivable
> shape is rejected: the guest requires an **already-initialised Fungible holding
> of the collateral definition, owned by that definition's own token program**,
> and the SDK checks the same thing client-side before it signs. A fresh key is
> NOT enough, and a sweep cannot bootstrap one - nothing signs for the treasury.
> Make it with `lpad init-holding --token-def <collateral-def>` and pass the id
> it prints.

### `lpad lbp pool-info` · `[online]`
Read a pool's on-chain state, evaluated at time `--at` (ms). Reports the
**creator** too, for the same reason `bc sale-info` does: `lbp close` and `lbp
withdraw` both take `--creator`, and nothing else renders a pool's.

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

### `lpad lbp buy-gated` · `[online]`
Buy from an **allowlist-gated** pool (one created with `--allowlist-root`). Same
flags as `lbp buy`, plus `--proof`: a comma-separated list of 32-byte hex Merkle
sibling hashes proving the buyer is on the allowlist (omit for a single-member
tree). The allowlist leaf is derived from the buyer's collateral holding
automatically, so you never pass it; the ungated `lbp buy` is rejected on a gated
pool.
```bash
lpad lbp buy-gated --pool <POOL_ID> --in 1000 --proof <HASH>,<HASH>
```

### `lpad lbp buy-disposable` · `[online]`
Private buy in **one** atomic transaction, exactly as `bc buy-disposable`: both
buyer-side holdings are private slots of a single proof, with no ephemeral
account and no deshield/re-shield legs. Same flags, with `--pool` instead of
`--sale`.

The LBP is the harder of the two to make private, because its price depends on
time and a privacy transaction cannot carry the on-chain clock. The buy is
therefore priced at the timestamp the CLI built it at, and its slippage floor is
quoted at that same timestamp; the pool's own end time still binds it, because
the guest clamps the transaction's validity window to it. A price taken this way
is never better than the price at admission.

```bash
lpad lbp buy-disposable --pool <POOL_ID> --in 1000
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
Withdraw raised collateral + unsold tokens, minus the at-close fee (creator). It
names **no treasury account**: the fee is escrowed in the collateral vault and the
creator is paid the rest, then `lbp sweep-treasury` settles it.

That split is not cosmetic. Paying both in one transaction meant a treasury that
cannot receive - uninitialised, wrong token definition, wrong token program -
reverted the creator's payout along with the fee, and a closed pool has no other
drain: the entire raise and every unsold token, locked for good. Now the worst
case is a stranded fee.

| Flag | Req | Meaning |
|---|---|---|
| `--pool <ID>` | yes | Pool id. |
| `--creator-collateral <ID>` | yes | Collateral destination. |
| `--creator-token <ID>` | yes | Token destination. |
| `--creator <ID>` | yes | Creator account. |
```bash
lpad lbp withdraw --pool <POOL_ID> --creator-collateral <COLL> --creator-token <TOK> --creator <CREATOR>
```

### `lpad lbp sweep-treasury` · `[online]`
Hand the escrowed at-close fee over to the treasury pinned into the pool at
creation. The mirror of `bc sweep-treasury`, including the permissionless account
list and the absence of any signer; `lbp pool-info` reports the outstanding
amount.

The fee accrues at **withdrawal**, not per swap, so this errors with "nothing
owed" until the creator has closed and withdrawn.

| Flag | Req | Default | Meaning |
|---|---|---|---|
| `--program <ID>` | no | lbp image id | Program id. |
| `--pool <ID>` | yes | - | Pool id. |
```bash
lpad lbp sweep-treasury --pool <POOL_ID>
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

Two things a `--json` consumer has to know:

* **Listings can be incomplete.** `my-sales`/`my-pools` carry `partial` (always
  present) and `unreadable` (see [`my-sales`](#lpad-my-sales--online)). Exit 0
  does not mean the array is exhaustive.
* **Strings are byte-faithful, not terminal-safe.** The human renderer escapes
  everything that steers a terminal in creator-supplied `token_name` /
  `token_symbol` - ESC, CR, the C1 CSI at U+009B, U+2028/2029, the bidi
  overrides. `--json` deliberately does **not**: the payload is contracted to
  carry chain state exactly as stored, so a consumer can compare, re-serialise or
  hash it, and `serde_json` escapes only the C0 controls, so the rest reaches you
  raw. `lpad ... --json | jq -r .token_name` therefore prints attacker-controlled
  bytes to your terminal - as it would for any JSON from anywhere. Sanitise at
  the point you render.
