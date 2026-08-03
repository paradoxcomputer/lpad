# Upgrading LPAD from LEZ `v0.2.0-rc4` to `v0.2.1`

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
| `WalletCore::new_update_chain(config, storage, overrides)` | now **async**, plus a `statistics_path` argument |
| `wallet.sequencer_client` (field) | `wallet.helm_owned()` / `wallet.get_last_block_id()` |
| `wallet.last_synced_block` (field) | `wallet.storage().last_synced_block()` |
| `store_persistent_data()` | no longer async |
| `get_accounts_nonces(Vec<AccountId>)` | `&[AccountId]` |
| `poll_native_token_transfer(hash) -> Tx` | `poll_transaction(hash) -> (Tx, BlockId)` |
| `storage().user_data.*` maps | `storage().key_chain().public_account_ids()` / `private_account_ids()` |
| `storage().labels` (id → label) | `storage().labels_for_account(AccountIdWithPrivacy)` — the map is inverted and private |
| `WalletChainStore` | `Storage`, all fields private |

Two behavioural items worth knowing:

* **Startup calibration.** Opening a wallet probes every sequencer missing from
  `statistics.json` ~100 times. Because the CLI opens a fresh wallet per command,
  that cost would be paid on *every* invocation. `LaunchpadClient::open` now takes
  a statistics path, `record_sequencer_statistics()` persists the samples after a
  successful command, and `bootstrap.sh` writes a low `calibration_limit`.
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
`--listen-address` now defaults to `0.0.0.0` — `run-sequencer.sh` passes
`127.0.0.1` explicitly since the RPC has no caller auth.

## 9. Capability lost: IDL generation

`programs/tools/idl-gen` depended on `spel-framework-core`'s `idl-gen` feature and
has been deleted; v0.2.1 has no IDL generator. The committed
`programs/artifacts/{bonding_curve,lbp}-idl.json` are still accurate (neither
`Instruction` enum changed) but are now **hand-maintained**, and the CI drift
check has been replaced by a guest-artifact presence/reproducibility check.

If client-facing IDLs matter going forward, the options are to hand-maintain
them, write a small generator over the `Instruction` enums in the `*_core`
crates, or drop them.

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
