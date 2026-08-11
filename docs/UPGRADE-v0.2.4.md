# Upgrading LPAD across LEZ `v0.2.0-rc4` → `v0.2.0` → `v0.2.4`

> **Now pinned at `v0.2.4`.** Both sequencers were reset and upgraded (heights
> dropped from 49192/5364 to ~1085/203), and a new L1 came with them
> (`logos-blockchain` rev `d8711bbc` → `e2a1c3b7`, inherited transitively — lpad
> has no direct L1 dependency and must not acquire one).
>
> For the record, since a bare hash says nothing about currency: `e2a1c3b7` sits
> **19 commits ahead** of the latest L1 node release,
> [`0.2.1`](https://github.com/logos-blockchain/logos-blockchain/releases/tag/0.2.1)
> (2026-08-05, commit `964e08fc`), and 8 behind — and those 8 are genesis-ceremony,
> deployment-config and version-bump commits on the release branch, no code. So
> LEZ `v0.2.4` rides post-`0.2.1` L1 master, while the running networks were
> genesis'd from the `0.2.1` release. Pinning the LEZ *tag* is what keeps lpad on
> the same side of that as the sequencers; overriding the L1 rev independently
> would not.
>
> The chains' built-in ids now match `v0.2.4` exactly:
>
> | program | deployed on both chains | v0.2.4 computes |
> |---|---|---|
> | `token` | `[1047643340, 4291649067, …]` | same |
> | `authenticated_transfer` | `[583309054, 2344528779, …]` | same |
> | `clock` | `[96247601, 2082502477, …]` (from the live clock account's `program_owner`) | same |
>
> Note the guest ELFs are **byte-identical across `v0.2.2`, `v0.2.3` and
> `v0.2.4`**, so the RPC cannot tell which of the three the operators run — and it
> does not matter: any of them satisfies the parity guard. That is why `v0.2.4` is
> safe to pin here, unlike the earlier v0.2.1-vs-v0.2.0 mismatch.
>
> **The v0.2.0 → v0.2.4 delta for lpad was small.** 395 upstream commits, but the
> programs workspace needed *zero* changes and the whole port was: bump the tag,
> re-apply the v0.2.1-shaped wallet layer (async 4-arg `new_update_chain` with a
> statistics path, `poll_transaction`, `helm_owned`, sliced nonces), switch the
> `WalletConfigOverrides` sequencer override from a scalar to the `sequencers`
> list, and delete `filter_output` (§12).
>
> Sections below that say "v0.2.1-only" now DO apply — v0.2.4 is past v0.2.1.

This was not a version bump. Between the two tags upstream landed 691 commits
including a whole-repo rename, the deletion of the program-authoring framework
lpad's guests were written against, and a change to account-id derivation that
invalidates all existing on-chain state.

Read the **Operational consequences** section before deploying or reusing a
wallet.

---

## 1. Why there was no incremental path

The `nssa` → `lee`/`lez` rename landed in `v0.2.0-rc5`, so no tag between rc4 and
v0.2.1 avoids it. The intermediate tags (rc5, rc6, v0.2.0) all carry the full
break.

## 2. Crate renames and moves

| rc4 | v0.2.1 | note |
|---|---|---|
| `nssa` | `lee` | path `lee/state_machine` |
| `nssa_core` | `lee_core` | path `lee/state_machine/core` |
| `common::transaction::NSSATransaction` | `common::transaction::LeeTransaction` | same three variants |
| `nssa::program::Program::token()` etc. | the new **`programs`** crate: `programs::token()`, `programs::ata()`, `programs::authenticated_transfer()`, `programs::clock()` | needs `features = ["artifacts"]`, which is deliberately *not* a default (upstream risc0#3772) |
| `NssaError` | `LeeError` | |
| `wallet`, `common`, `sequencer_service_rpc` | same crate names | moved under `lez/` |
| `ata_core` | `associated_token_account_core` | lpad keeps the short name via a Cargo `package =` rename |
| `NSSA_WALLET_HOME_DIR` | `LEE_WALLET_HOME_DIR` | the CLI still accepts the old name |
| `LOGOS_BLOCKCHAIN_CIRCUITS` | *(gone)* | circuits are no longer a separate download |

## 3. SPEL is deleted; guests are hand-written

The `spel-framework` git dependency, the `#[lez_program]` / `#[instruction]`
macros and `SpelOutput::execute` no longer exist. A program is now one crate with
a `src/main.rs` that does its own dispatch:

```rust
let (ProgramInput { self_program_id, caller_program_id, pre_states, instruction },
     instruction_words) = read_lee_inputs::<Instruction>();     // was read_nssa_inputs
let pre_states_clone = pre_states.clone();
let (post_states, chained_calls, deadline) = match instruction { /* ... */ };
let (pre, post) = filter_output(pre_states_clone, post_states);  // see below
ProgramOutput::new(self_program_id, caller_program_id, instruction_words, pre, post)
    .with_chained_calls(chained_calls)
    .with_timestamp_validity_window(..deadline)
    .write();
```

The underlying `lee_core::program` API (`AccountPostState`, `Claim`,
`ChainedCall`, `ProgramInput`/`ProgramOutput`, validity windows) is otherwise
**unchanged** — SPEL was only a macro layer over it, so the host handler modules
needed nothing but the `nssa_core` → `lee_core` rename.

### 3.1 The load-bearing part: `filter_output`

The `#[lez_program]` macro silently filtered the emitted `(pre, post)` pairs, and
**omitting that filter breaks every instruction that lists a signer account.**
`validate_execution` rule 7 rejects a post-state whose `program_owner` is
`DEFAULT_PROGRAM_ID` when the matching pre-state was not a default account, and
rule 4 forbids changing `program_owner`. A signer whose nonce an earlier
transaction already bumped is exactly that shape, so `create_sale`, `close_sale`
and `withdraw` would fail with `NonDefaultAccountWithDefaultOwner`.

It is ported verbatim into `<program>/src/dispatch.rs` for each of the three
programs. Dropping a pair is safe: the state machine only walks the pre-states a
program declares, so an omitted account is excluded from the diff and untouched.

### 3.2 Layout change

Per program, the `methods/` + `methods/guest/` crate pair is gone. `programs/` is
now both the workspace root and the `lpad_guests` package, mirroring upstream's
`lez/programs`: one `[[bin]]` per program pointing at `<program>/src/main.rs`,
plus a library that embeds the built ELFs. The `artifacts` and `programs` features
are mutually exclusive — with `artifacts` on, a guest build would try to embed the
very `.bin` files it is producing.

## 4. Guests are now reproducible, and their ids are constants

Guests are cross-compiled inside a pinned Docker container
(`cargo risczero build`, container `r0.1.91.1`), which makes the ELF — and hence
its RISC0 image id — byte-identical on every machine. Verified: rebuilding
upstream's 16 programs reproduced all 16 committed artifacts byte-for-byte.

Consequences:

* `programs/artifacts/lpad/*.bin` are **committed** and embedded into the SDK at
  compile time. `bc_program_id()` and friends are now compile-time constants; the
  old runtime ELF lookup (`LPAD_BC_ELF`, `programs/target/riscv-guest/...`) is
  gone, along with the failure mode where a guest rebuild left the CLI submitting
  against an undeployed image id.
* This **retroactively solves the clone-safe program-id problem** that drove
  commits `cf68c55` and `3825bab`. A pinned id no longer breaks a fresh clone on a
  different toolchain, because there is only one possible id.
* A diff in `programs/artifacts/lpad/` is a consensus-level change: the program
  must be redeployed and every PDA derived from its id moves.

## 5. Vendored programs: token and ATA removed

lpad vendored `token` and `ata` as **hardened forks** — 8 on-chain assertions
upstream does not have (upstream's `initialize_account` even carries a TODO
acknowledging the gap). Both forks are now deleted in favour of upstream's
committed artifacts, on the following basis:

* **token** — bootstrap never deployed lpad's fork; the SDK called
  `Program::token()` (the chain's built-in) everywhere. The fork was dead code in
  production, exercised only by integration tests, so its 5 extra assertions were
  never in force on chain. Nothing is lost.
* **ATA** — bootstrap *did* deploy lpad's fork, so its 3 recipient-contract
  assertions were live. Upstream's ATA program deliberately does not police its
  recipient (its doc: *"any account; auto-created if default"*), so those checks
  were **re-added on the caller side** as `util::assert_ata_recipient`, invoked by
  `bonding_curve::{buy_ata, sell_ata}` and `lbp::buy_ata` for the recipient of
  every `ata::Transfer` leg — including the vault-side recipient, which lpad did
  not previously check at all.

  Note the enforcement point moved from the ATA program into each caller: a
  future ATA caller must remember to assert it.

`ata_core::Instruction`'s three variants each gained `ata_program_id` as their
**first** field (3 call sites).

The canonical ATA program is also **pre-deployed at genesis** in v0.2.1
(`testnet_initial_state::initial_programs`), together with token,
authenticated_transfer, clock, vault and faucet — so `bootstrap.sh` no longer
deploys it.

## 6. The one silent runtime break

`authenticated_transfer`'s instruction was a bare `u128` in rc4; v0.2.1 gave it a
typed `Instruction` enum. `wlez::wrap` chained the native-transfer leg with
`&amount`, and `ChainedCall::new` is generic over any `Serialize`, so **this
compiles cleanly and fails only at run time** — the callee reads the amount as an
enum discriminant. Fixed to
`&authenticated_transfer_core::Instruction::Transfer { amount }`; the unit test
`wrap_emits_native_transfer_then_mint` is the drift-guard.

## 7. Wallet API changes

| rc4 | v0.2.1 |
|---|---|
| `PrivacyPreservingAccount` | `AccountIdentity` (lpad's two variants keep their names) |
| `WalletCore::new_update_chain(config, storage, overrides)` | *v0.2.1-only:* becomes **async** + takes a `statistics_path`. On v0.2.0 it is unchanged. |
| `wallet.sequencer_client` (field) | *v0.2.1-only:* replaced by `helm_owned()` / `get_last_block_id()`. On v0.2.0 the public field remains. |
| `wallet.last_synced_block` (field) | `wallet.storage().last_synced_block()` |
| `store_persistent_data()` | no longer async |
| `get_accounts_nonces(Vec<AccountId>)` | *v0.2.1-only:* takes `&[AccountId]`. Still `Vec` on v0.2.0. |
| `poll_native_token_transfer(hash) -> Tx` | *v0.2.1-only:* renamed to `poll_transaction`, returning `(Tx, BlockId)`. Unchanged on v0.2.0. |
| `storage().user_data.*` maps | `storage().key_chain().public_account_ids()` / `private_account_ids()` |
| `storage().labels` (id → label) | `storage().labels_for_account(AccountIdWithPrivacy)` — the map is inverted and private |
| `WalletChainStore` | `Storage`, all fields private |

Two behavioural items worth knowing:

* **Startup calibration** is a v0.2.1 concern only (the multi-sequencer client
  and `statistics.json` do not exist in v0.2.0), so lpad does not carry it.
* **Private proving got more expensive.** `send_privacy_preserving_tx`
  unconditionally pads the private inputs to 7 notes
  (`MAX_PRIVATE_ACCOUNTS = 7`). lpad's deshield/re-shield legs have exactly one
  private account, so the circuit now proves 7 notes instead of 1. There is no
  opt-out — `account_manager` is a private module. Expect the private buy/sell
  timings in `docs/PROOF-PERF.md` to regress.

## 8. Operational consequences

**Every account id changes.** The public-PDA derivation domain moved from
`/NSSA/v0.2/AccountId/PDA/` to `/LEE/v0.2/AccountId/PDA/`, so every sale, pool,
vault, treasury and ATA address is different. Private account ids also changed
(the viewing key is now mixed in, and the viewing key itself changed
representation to ML-KEM-768).

**An existing `~/.lpad` wallet is not upgradable.** `storage.json` moved its
accounts under a new `key_chain` object, `wallet_config.json` replaced
`sequencer_addr` with a `sequencers` array and dropped its genesis keys, and the
note-encryption KDF moved to v0.3 — so previously-decoded notes cannot be read
back. Move the directory aside and re-bootstrap against a fresh chain. The CLI
detects the old layout and says so rather than emitting a raw serde error.

**Genesis funding must be claimed.** A `supply_account` genesis action now credits
the recipient's *vault PDA*, not its own balance, so a genesis account starts with
zero spendable funds. `bootstrap.sh` now does
`wallet account import public --private-key …` followed by
`wallet vault claim --account-id … --amount …` before its first transfer. Only
`supply_bridge_account` credits a balance directly.

**The sequencer config schema changed.** `genesis_id`, `is_genesis_random`,
`initial_public_accounts` and `initial_private_accounts` are gone, replaced by
`genesis: Vec<GenesisAction>` (`supply_account`, `supply_bridge_account`,
`supply_bridge_lock_holding`). `bedrock_config.funding_key` is a new **required**
field even in standalone mode, where it is never read. The config also moved to
`lez/sequencer/service/configs/debug/sequencer_config.json`, and
`--listen-address` now defaults to `0.0.0.0`.

This section is retained as a record of the schema change only. lpad no longer
runs a sequencer of its own: `run-sequencer.sh` was deleted (§11) and lpad targets
the two public testnets, so nothing here is a step you perform.

## 9. IDL generation replaced by source-generated ABIs

`programs/tools/idl-gen` depended on `spel-framework-core`'s `idl-gen` feature and
was deleted with the v0.2.1 migration; v0.2.4 still ships no IDL generator. The
capability was replaced rather than lost: `scripts/build-abi.sh` derives
`programs/artifacts/{bonding_curve,lbp,wlez}-abi.json` — bare zonescan type
descriptors — plus the ready-to-paste
`programs/artifacts/zonescan-program-schemas.json`, straight from the
`Instruction` enums in the `*_core` crates, so an ABI cannot drift from the
program it describes.

The hand-maintained `programs/artifacts/*-idl.json` files are gone, and they were
not accurate when they went: declaration order is the wire discriminant, and the
old files listed some variants out of order. `scripts/ci-e2e.sh` now regenerates
the ABIs and fails on a diff, and `scripts/build-guests.sh` regenerates them after
any guest rebuild — the ABIs carry the program ids, so a rebuilt ELF would
otherwise leave an indexer keyed on ids nobody deployed.

## 10. Build environment

* `libpcsclite` is a **new hard requirement**: v0.2.1's `wallet` depends on
  `keycard_wallet` unconditionally, which links `pcsc`. Install
  `libpcsclite-dev`, or point `PCSC_LIB_DIR` at a directory containing a
  `libpcsclite.so` symlink (`pcsc-sys` honours it and needs no headers).
* Docker + `cargo-risczero` are needed only to rebuild guests.
* The `_lez` symlink, its `[patch]` sections, and the CI clone step are all gone.
  The bonsai-free risc0 configuration lpad used to patch in locally
  (`default-features = false`, without which guests cannot cross-compile) is now
  upstream's own default.
* Rust toolchain is unchanged at 1.94.0.

---

## 11. No bundled sequencer: lpad targets real networks

`run-sequencer.sh` and the standalone dev-sequencer path are gone. lpad talks to
real sequencers, selected with `--network` (or `$LPAD_NETWORK` for the scripts):

| alias | sequencer |
|---|---|
| `testnet` (default) | `https://testnet.lez.logos.co` |
| `paradox` | `https://seq-testnet.paradox.computer` |
| any `http(s)://…` | passed through verbatim |

`--network` is applied as a `WalletConfigOverrides`, so it changes the sequencer
for that invocation only and never rewrites the wallet config.

Consequences:

* **Real proofs, always.** A real sequencer verifies, so `RISC0_DEV_MODE` cannot
  be used against one — `bootstrap.sh` now refuses to run if it is set. Dev-mode
  proving remains available for the in-process integration tests, which drive the
  state machine directly and need no chain. Budget minutes per private op.
* **No genesis control.** The hardcoded genesis funder key and `vault claim` only
  worked on a chain we owned. Bootstrap now claims from the **pinata faucet**, or
  uses `$LPAD_FUNDER` / `$LPAD_FUNDER_KEY` if you supply a funded account.
* **lpad's programs are not built into either chain** — nothing upstream deploys
  them, so `bootstrap.sh` does, and their ids are pinned in
  `lpad_guests::deployed` with a drift-guard test. Rebuilding the artifacts
  changes those ids, which orphans every existing sale and pool, so treat a diff
  there as a consensus change. (As of this release all three *are* deployed and
  live on the Logos testnet — see §13 — and `scripts/verify-deployment.sh` is how
  to check any chain.)
* The CI `e2e` job stays hermetic (in-process, dev proofs). A live smoke test is
  opt-in via `workflow_dispatch`, since it needs a funded account and real proving.

One caveat on reading the chain: the sequencer's `getProgramIds` RPC returns a
**hardcoded five-entry list** with a `// TODO: Get programs from state`, so it is
not a reliable inventory of what is deployed. Query a known account id instead
(the clock account is a fixed literal, so it works across versions).

---

## 12. v0.2.4 removed the need for the SPEL post-state filter

v0.2.4 commit `7bc4c460` added `DeclaredAccountMissingFromOutput`: every account
in `message.account_ids` must appear in the final state diff. Its message names
the motivating case explicitly — *"a program (or a macro-generated dispatcher
wrapping one) that silently drops an account from both sides of its own output"*.
That is exactly what the ported SPEL `filter_output` did (§3.1).

Both constraints are live in v0.2.4 and, for an offending account, they are now
mutually exclusive:

* echo a `DEFAULT`-owned account with a non-default pre-state → rule 7,
  `NonDefaultAccountWithDefaultOwner`
* drop it → `DeclaredAccountMissingFromOutput`

So filtering can no longer rescue a transaction; it only swaps rule 7's precise
error for a vaguer one. **`filter_output` is therefore deleted**, and all three
guests now emit `pre_states_clone` verbatim, matching upstream's own programs.

The real constraint moved to the caller: never *declare* an account that is
`DEFAULT`-owned with non-default state. lpad satisfies this already — every
account it declares is owned by the token program, the authenticated-transfer
program (funded native accounts), or is a PDA the program claims. Rule 7 keys on
`program_owner == DEFAULT`, so a funded creator never trips it; the filter only
ever mattered for a bare keypair that had signed but held nothing.

Verified empirically: the full integration suite (21 tests at the time; 22 now,
after the LBP create-sale path gained end-to-end coverage) passes against the real
v0.2.4 state machine both with the filter neutralised and with it removed. `wlez`
had no other dispatch helpers, so its `dispatch.rs` is gone entirely; the
bonding-curve and LBP modules keep their clock helpers.

---

## 13. Deploying to a real network: what actually bites

Learned while deploying to the Logos and Paradox testnets on v0.2.4.

**Program deployment needs no signer and no funding.** The deploy transaction
carries only bytecode - no nonce, no witness set. Two consequences: a fresh
unfunded wallet can deploy, and because the transaction is a pure function of the
bytecode, its **hash is deterministic**, so re-deploying identical bytecode is
idempotent - the wallet returns the original hash and reports the original block.
Re-running bootstrap against an already-deployed chain is therefore safe.

**Receiving requires initialisation, twice over.** Both of these are rejected for
an uninitialised recipient:

  * a native transfer or a pinata claim - fix with
    `wallet auth-transfer init --account-id <acct>`
  * a **token** transfer - the token program claims the recipient with
    `Claim::Authorized`, which the runtime grants only for an account the
    transaction signed for, and the wallet does not co-sign transfer recipients

The LEZ wallet has an `auth-transfer init` but no token-account equivalent, which
is why lpad added `lpad init-holding --token-def <D>` (creates *and* initialises a
holding, prints its id). Initialising the creator under authenticated-transfer is
also what keeps it clear of `validate_execution` rule 7, since it sets
`program_owner` away from `DEFAULT`.

**A rejected transaction looks like a timeout.** A transaction the sequencer drops
never appears on chain, and the wallet reports only `Error: All pollers failed`.

Do NOT conclude "rejected" from `getTransaction` returning nothing: that RPC has a
bounded lookback, so a transaction that WAS included ages out of the window and
then reads as absent. (Observed directly: the bonding_curve deploy reported
"included in block 1138", and ~68 blocks later `getTransaction` on the same hash
returned nothing, with the chain healthy and never reset.) The reliable signal is
whether the wallet ever printed "Transaction is included in block N" - it polls
until it sees inclusion.

The same lookback explains a confusing deploy behaviour: because a deploy tx is a
pure function of its bytecode, re-deploying is idempotent only while the ORIGINAL
inclusion is still inside the poll window. After that, the duplicate is never
re-included and the deploy appears to fail even though the program is deployed and
working.

**Verify a deploy by reading the block, not by trusting the report.** An earlier
revision of this document concluded from the above that "a deploy step must
tolerate failure and let a subsequent real call (e.g. `create-sale`) be the proof
of deployment". That is wrong, and it cost a session's debugging:

  * a call against a *missing* program is dropped from the mempool and reports
    the same `All pollers failed` as any other non-inclusion, so `create-sale`
    does **not** distinguish "not deployed" from "slow chain". There is no
    "Unknown program" error to wait for.
  * the reported block id is not evidence either. Re-deploying an unchanged guest
    resolves `getTransaction` to the original deploy and prints *its* block:
    `wlez` "deployed into block 1259" in 5 seconds while the head was 1408.

Concretely: `lbp` reported "included in block 1161" on the Logos testnet and was
never actually deployed. Every `lbp create-sale` for the rest of that session was
silently discarded, which read as poll flakiness. Scanning blocks 1000-1397 for
the committed artifact bytes found `bonding_curve` (1138) and `wlez` (1259) but no
`lbp` at all - while all three were present on Paradox (279/280/281), so it was a
single dropped transaction, not a build or id problem.

`bootstrap.sh::deploy_verified` now fetches the block the wallet named and asserts
the guest's own ELF bytes are inside it. That is cheap (one block), positive, and
tells "already deployed" (old block, bytes present) apart from "never landed"
(bytes absent -> hard fail). `scripts/verify-deployment.sh` is the standing check;
note `getAccount` takes only bare base58, so it normalizes ids first - passing a
`Public/`-prefixed or hex id makes a live account read as ABSENT.

**Block time is ~46-50s on both public testnets**, not the 15s a local dev chain
used. Two knock-on effects:

  * fixed `sleep`s tuned for a dev chain are shorter than one block, so dependent
    steps read pre-transaction state. `bootstrap.sh` now waits on real height
    (`wait_block`) instead.
  * the wallet's poll budget is `seq_poll_max_retries x seq_poll_timeout`. The
    defaults this repo used (10 x 20s) gave only ~4 blocks of patience, which is
    what made rejections look like timeouts. Use something like 60 x 30s.

**Real sequencers verify proofs**, so `RISC0_DEV_MODE` is unusable against them -
every private operation is a full STARK (minutes). `bootstrap.sh` refuses to run
with it set. Dev-mode proving remains available only for the in-process
integration tests.
