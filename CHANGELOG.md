# Changelog

All notable changes to LPAD. Versions follow [semver](https://semver.org); while
the project is pre-1.0 a minor bump may still break interfaces.

Each release names the **LEZ tag** it is built against and the **Logos L1 node**
revision that tag transitively pins. Those are not cosmetic: a program's RISC0
image id *is* its on-chain address, so a LEZ bump that the operators have not
adopted makes every transaction against a built-in program fail - silently, as a
timeout rather than an error.

## [Unreleased]

Needs a version number before it ships, and it is not a patch: all three program
ids move and four existing instructions change arity, so **0.3.0**.

### Added

- **`lpad init-wallet`: lpad can now create a wallet.** Until this, it could only
  *open* one - and `Storage::from_path` fails outright on a missing file, so a
  user who installed the CLI and nothing else got `Failed to load storage from
  …` and no way forward. The only thing that could write that file was the LEZ
  `wallet` binary, which means a LEZ checkout, which is precisely what packaging
  lpad for npm was supposed to make unnecessary. Every quickstart in this repo
  quietly assumed a wallet somebody else had already made. The `wallet` crate is
  already an SDK dependency, so the capability was linked in the whole time and
  simply unreachable.

  It refuses to overwrite an existing wallet (fresh keys over live ones strands
  every balance permanently), prints the 24-word recovery phrase once, remembers
  the chain so the picker never fires, and writes the polling numbers
  `scripts/bootstrap.sh` uses.

  **That last part is not tidiness.** LEZ's defaults are `seq_poll_timeout` 12s x
  `seq_poll_max_retries` 5 - 60 seconds of patience against a block every ~46-50s
  - so a transaction that lands in the very next block is reported as
  `All pollers failed`. The first `lpad faucet` on a wallet created with the
  defaults failed exactly that way; with 30s x 60 it claimed 150 native LEZ in
  block 13835. A new user would have hit that on their first command.

  **It asks for no password, deliberately.** LEZ v0.2.4 accepts one and discards
  it: `Storage::new` is `// TODO: Use password for storage encryption` followed by
  `let _ = password;`, `Storage::from_path` takes no password at all, and
  `save_to_path` writes plain `serde_json` - so `storage.json` holds raw signing
  and spending keys in cleartext. Prompting would tell a user their keys are
  protected when nothing protects them. lpad creates the file `0600` (the default
  is 0644 minus umask, which on a stock Ubuntu is group-readable **0664**) and
  says all of this in the output.

- **`lpad network --check`: a non-interactive deployment check.** The SDK has
  always been able to answer "are this build's three programs on this chain?"
  (`lpad_deployment_status`), but nothing could reach it without a terminal - the
  only caller was the code that runs *after* an interactive network pick. So a
  script had two options, and both were bad: skip the check, or reimplement the
  block scan in bash. `scripts/bootstrap.sh` did the second.

  It exits non-zero when a program has no record, because the failure it prevents
  is silent rather than loud: a transaction against a program that is not
  deployed is dropped from the mempool and reads as a *timeout*, not an error, so
  a caller that carries on spends its entire poll budget - 30 minutes per
  transaction, on the bootstrap's own config - to learn nothing. Under `--json`
  it reports per-program `block`/`deployed` plus a `missing` list.

  "No record" is not proof of absence and the message says so: a chain that was
  reset can have forgotten the deploy transaction while still running the
  program. `scripts/verify-deployment.sh` asks the stronger question - is lpad's
  on-chain state still there, and does its owner match this build's id - and is
  the one to believe on a disagreement.

- **`scripts/demo.sh`: the public path, start to finish.** `faucet -> wrap ->
  create sale -> quote -> public buy -> sell`, sized off the balance the faucet
  actually paid rather than a constant, resolving both holdings by id because the
  sell leg requires them and no command output names them. It states its real
  wall clock (6-9 minutes: 7-11 transactions at a ~46-50s block) instead of
  implying a shorter one, and `LPAD_DEMO_PREFLIGHT_ONLY=1` rehearses every check
  without spending a faucet claim.

  It contains no private operation, and that is enforced rather than promised:
  the script never calls the binary directly, and the wrapper it calls instead
  refuses `shield`/`deshield`/`buy-private`/`sell-private`/`buy-disposable`
  outright. A comment saying "no private ops here" is one well-meaning edit from
  being false; a guard is not.

- **A per-creator sale index, on chain, on both launchpads.** One account per
  `(program, creator)`, which `CreateSale` claims on that creator's first sale
  and appends an id to on every sale after it. Discovery is now one read per
  creator. It used to be a re-derivation of every PDA the wallet could possibly
  have made - a product over (creator account, token definition, collateral
  definition, nonce), each arm a network round-trip - which measured at ~4,800
  sequential reads and tens of minutes on a wallet with any history.

  It is the second-to-last account of `CreateSale`, immediately before the clock,
  on both programs, and it is keyed on the **creator alone** - not the sale, the
  token or the nonce. That is what makes discovery one read, and what makes the
  address stable for the life of the account:

  ```
  creator_index = AccountId::for_public_pda(program_id, sha256(creator || tag))
  creator = the creator account id, 32 bytes
  tag     = b"bc/creator_idx\0\0"  (bonding curve) | b"lbp/creator_idx\0"  (LBP)
  ```

  `compute_creator_index_pda(program_id, creator)`, in each program's `core`
  crate, is the reference implementation.

  The account holds an 8-byte magic - `lpad/bci` on the bonding curve,
  `lpad/lbi` on the LBP - a `u16` layout version (currently `1`), the creator id,
  and the ids, oldest first. The magic and the version are there because this PDA
  is read *speculatively*: a client derives it for an account that may never have
  created anything, so it has to be able to answer "this is not one of mine"
  instead of decoding some unrelated account's bytes into a plausible-looking
  list. A reader that does not recognise the version refuses the account rather
  than guessing at it.

  **Ids only, deliberately.** Sale state changes on every trade and would be
  stale the moment it was written; an id is permanent. Readers resolve the ids
  against the sale accounts themselves. Ids are never removed either - a closed
  sale is still one the creator created, and still worth listing.

  The cap is **3,000 ids per creator**: a full index encodes to 96,046 bytes,
  under LEZ's 100 KiB `DATA_MAX_LENGTH`, with room for a future field. It is
  asserted on append, so the failure names the account and the remedy (create
  further sales from a different creator account) instead of surfacing as a
  `DataTooBigError` from inside the encode.

  `lpad my-sales` and `lpad my-pools` read the index by default and are the
  intended consumers. Their new `--refresh` re-derives every candidate id the old
  way, and `--deep` widens the creator set to every public account rather than
  just the identity accounts; both only ever *add* to what is listed, and both
  exist solely for sales created by an lpad build from before the index did,
  because nothing on chain recorded those.

- **`BuyDisposable`** on both launchpads - a private buy that is one atomic
  transaction rather than three. The buyer's collateral and token holdings are
  private slots of a single proof, so the debit and the credit happen together:
  there is no intermediate transaction to omit and no block in which the proceeds
  are publicly held. The existing `buy-private` saga stays and is still the
  default; this is a second mode, not a replacement, because the saga is the one
  that has been run against a real sequencer.

  It carries no clock account, and cannot: every public account in a privacy
  transaction is pinned byte-for-byte at proving time and re-verified at
  inclusion, while `CLOCK_01` is rewritten every block. Time is enforced instead
  by the transaction's own validity window, which the *guest* computes from the
  pinned sale or pool state - the caller's `deadline` is clamped, never echoed.
  The bonding curve's `not_before_ms` only bounds that window; the LBP's
  `t_buy_ms` also prices the trade, which is safe because `buy_tokens_out` is
  non-decreasing in time at fixed reserves and the pool PDA is pinned, so the
  buyer can only ever receive less than the fair amount at admission. That
  monotonicity is the whole safety argument and is pinned by a proptest.

  The trade-off against the saga is drift: the saga re-prices its public leg
  against live state, while a disposable buy is invalidated by any competing
  trade that lands first. Both modes are shipped for that reason.

- **`SweepTreasury`, on both launchpads**: hands the protocol fee escrowed in the
  collateral vault over to the sale's or pool's pinned treasury (see the escrow
  entries under Changed - the bonding curve escrows on every `BuyDisposable`, the
  LBP at withdrawal). `[sale|pool, collateral_vault, treasury]`, no signer,
  callable whether the sale is open or closed, and it reverts when nothing is
  owed. Anyone may submit it, which is why it takes its chained call's program
  from the collateral vault rather than from the treasury account it was handed:
  the most a stranger can accomplish is delivering the fee to its rightful owner
  while paying the gas.

  **No signature is accepted either**, not even the treasury's own. An earlier
  cut of this release let the treasury sign for itself so the fee leg could
  create a treasury account that had never been initialised. That bootstrap is
  gone: a claim needs the claimed account's pre-state to be `Account::default()`
  WHOLE, so any stranger could kill it forever with a dust transfer, and the
  escrow it was meant to rescue is the one balance on either program that can be
  lost for good. `CreateSale` now REQUIRES the treasury to already be an
  initialised `Fungible` holding of the collateral definition under that
  definition's own token program, which is checked at the one moment the creator
  can still fix it for free; `sweep_treasury` on both programs then `assert_ne!`s
  a DEFAULT-owned treasury unconditionally, and the SDK errors before signing
  anything. A treasury that reaches a sweep unowned means chain state disagrees
  with what the sale pinned - unfixable by any signature, and worth reporting.

  Appended last in each instruction enum: wire discriminant 8 on the bonding
  curve, 10 on the LBP, and no existing variant's discriminant moves.

- `bc buy-disposable`, `lbp buy-disposable`, `bc sweep-treasury` and
  `lbp sweep-treasury` - four of the **51** commands this release ships (17 top
  level, 17 under `bc`, 17 under `lbp`; `faucet` and `network`, both below, are
  the two the top level gained). `cli/tests/cli.rs` asserts that count and asserts
  that `sweep-treasury` exists on **both** groups - dropping either one as a
  duplicate of the other strands that program's escrow with no instruction that
  can reach the treasury.

- **`lpad faucet`**: mine the pinata proof-of-work faucet and claim its fixed
  prize into one of the wallet's accounts, so a fresh wallet on a testnet that
  runs pinata can fund itself without an operator. It reads the challenge, mines
  against it locally, submits, and then *verifies the balance moved* rather than
  trusting the claim's own report.

  It also initialises the recipient under the authenticated-transfer program when
  it can, because under LEZ v0.2.4 that is a precondition for holding native LEZ
  and not a nicety: rule 5 only lets the owning program decrease a balance, so
  LEZ paid into an account nobody initialised can never be moved out again. That
  makes the failure modes matter more than the happy path, and each is reported
  from evidence rather than by pattern-matching the rejection - a drained faucet,
  somebody else's claim re-seeding the challenge mid-mine (not a fault at all:
  re-run), an account that cannot receive LEZ, and, distinctly, an account lpad
  could not re-read to tell which of those it was.

- **`lpad network`, and a network chosen once instead of on every command.** The CLI
  now offers three sequencers - the Logos testnet, Paradox, and a **local** LEZ node
  the user installs and runs - asks which on the first run against a wallet that has
  never been asked, and remembers the answer in a `network` file beside the wallet
  config. `lpad network` re-opens that picker; `lpad network --json` reports the
  current selection (`network`, `url`, `display_name`, `source`, `marker`,
  `wallet_config`) and makes no RPC, so it stays answerable while the chain is down.
  Resolution order is `--network` → the remembered choice → the first-run picker →
  the sequencer in the wallet's own config, which is the pre-existing behaviour and
  is what a run with no terminal still gets.

  Kept per **wallet**, not global, and beside the wallet config exactly like the
  `proof_mode` marker: a wallet's accounts, shielded notes and sale ids are PDAs on
  one chain, so a single global setting would be wrong for every wallet but one, and
  `LEE_WALLET_HOME_DIR=~/.lpad-paradox` would silently keep aiming at testnet. Stored
  in `--network`'s own vocabulary, so `local=<url>` round-trips and an edited local
  URL survives a restart.

  It **cannot** prompt where nobody can answer: the picker requires a terminal on
  stdin *and* stderr - a menu on a redirected stderr is a hang nobody can see - and
  is off under `--json`; all of its output goes to stderr, so a `--json` stdout stays
  parseable. `scripts/test-all-cli.sh`, `scripts/ci-e2e.sh` and CI therefore behave
  exactly as before, and a run with no TTY that has no choice recorded gets the same
  actionable error as always, now naming the file to write.

  `--network` is unchanged in meaning - one invocation, never written back, and the
  sequencer only, never the wallet home - and gained the `local` and `local=<url>`
  aliases. lpad bundles **no** sequencer (0.2.0 removed the local one); `local` is a
  node the user runs, so the URL is offered as `http://127.0.0.1:3040` and is theirs
  to edit.

- **The CLI can deploy the launchpad programs itself.** Previously only
  `scripts/bootstrap.sh` could, by shelling out to the upstream LEZ `wallet` binary -
  so putting lpad on a chain of your own meant a LEZ checkout. Picking `local` now
  checks whether `bonding_curve`, `lbp` and `wlez` are on that chain and offers to
  deploy them through the SDK: no wallet binary, no checkout. It is idempotent (a
  deploy transaction's hash is a pure function of its bytecode, so an unchanged guest
  is reported as already deployed and nothing is submitted - a duplicate is never
  re-included, and submitting anyway burns the wallet's whole ~30 min poll budget per
  program proving nothing) and it verifies the guest's own ELF bytes are in a block
  rather than trusting the wallet's report, with the same bounded, always-reported
  fallback scan and the same `LPAD_DEPLOY_SCAN_BLOCKS` knob bootstrap uses. "No
  record" is reported as exactly that, not as proof of absence: a reset or restored
  sequencer can forget the deploy transaction while still running the program.

  The offer is **local-only**. On the two named chains, and on a bare URL, missing
  programs are reported with `scripts/verify-deployment.sh` as the next step and no
  prompt - those are other people's chains, and lpad cannot tell whose a bare URL is.

  None of this makes program ids per-network. A program's id *is* the RISC0 image id
  of its committed ELF, so all three ids are identical on every chain and there is no
  id table to maintain; the only thing a network decides is where those programs are
  deployed. `bootstrap.sh` still needs a LEZ checkout, for everything else it drives
  (keypairs, minting, the faucet, native transfers) and because it runs unattended
  while the CLI's deploy sits behind a confirmation.

### Changed

- **Every program id changed**, wlez included. An image id covers a program's
  whole dependency closure, so the lockfile alignment in `9a25825` moved all
  three on its own; wlez's `unwrap` then changed as well (below). Every sale,
  pool, vault and ATA PDA on both testnets is derived from the old ids and is
  orphaned; unwrap any wrapped native LEZ *before* redeploying, or it is stranded
  at an address the new wlez cannot claim.

  **Where that stands as measured, not as argued.** On `testnet.lez.logos.co` all
  three are already deployed under this release's ids - `bonding_curve` in block
  12609, `lbp` in 12610, `wlez` in 12611 - and `scripts/verify-deployment.sh`
  confirms the bootstrap's sale and pool are on chain *and owned by those ids*,
  so nothing there needs redeploying. `seq-testnet.paradox.computer` has no
  record of any of the three and every lpad PDA on it reads as unclaimed, so it
  needs a deploy and a fresh bootstrap. Neither line is worth trusting once an
  operator has touched a chain: `lpad --network <net> network --check` asks it
  directly.
- **Four instructions changed arity.** None of it is caught by a discriminant
  check, so an external client submitting an 0.2.0 account list gets a guest
  panic rather than a decode error.

  | instruction | 0.2.0 | now | what moved |
  | --- | --- | --- | --- |
  | `CreateSale` (bc) | 7 | 10 | the project token's definition, the treasury, the creator index |
  | `CreateSale` (LBP) | 7 | 11 | both token definitions, the treasury, the creator index |
  | `Poke` (LBP) | 2 | 3 | the weight observation PDA |
  | `Withdraw` (LBP) | 7 | 6 | the treasury slot is **gone** |

  Both `CreateSale` lists are given in full below, because their orders are *not*
  the same shape - the treasury is fourth on the bonding curve and ninth on the
  LBP - and because getting the length right is only half of it: a list of the
  right length in the wrong order clears the arity check and is then rejected
  further in, by a PDA or owner assert that names the account rather than the
  ordering:

  * bc: `[sale, token_vault, collateral_vault, treasury, token_definition,
    collateral_definition, creator_token_holding, creator, creator_index, clock]`
  * LBP: `[pool, token_vault, collateral_vault, token_definition,
    collateral_definition, creator_token_holding, creator_collateral_holding,
    creator, treasury, creator_index, clock]`

  The treasury now occupies a slot as well as the `treasury_id` field it has
  always had. An id alone is a bare `AccountId` nothing on chain can type-check;
  the account is what lets `CreateSale` reject a treasury the fee could never
  reach - uninitialised, holding a different definition, or owned by a token
  program that is not the collateral's - at the last moment that is free, rather
  than at settlement, when the money is already escrowed behind it. See
  `creator_index` in Added for the other new slot and its derivation.

  Bonding-curve `Withdraw` is *unchanged* at 6 accounts, which is a correction:
  an earlier cut of this release put a treasury slot in it and this file said so.
  That slot is gone again - the escrow is settled by `SweepTreasury` instead - so
  a client written against 0.2.0's `Withdraw` keeps working. The LBP's is the
  opposite case: its treasury slot was real, and removing it is what unblocks the
  creator's payout, so an 0.2.0 client's `lbp withdraw` panics until it drops
  that account.
- Bonding-curve private buys escrow the protocol fee in the collateral vault
  (`SaleState.treasury_owed`) instead of sweeping it to the treasury per trade.
  Pinning a treasury that is shared across sales would mean any fee anywhere
  invalidated every in-flight proof. Settlement is the new permissionless
  `SweepTreasury` and deliberately *not* `Withdraw`: the treasury arrives at
  `CreateSale` as a bare `AccountId` that nothing on chain can type-check, so
  folding the fee transfer into the withdrawal would put the entire raise behind
  an account that may be unable to receive. `Withdraw` now pays
  `vault_balance - treasury_owed` and leaves the escrow, and the collateral
  backing it, in the vault. The economics are unchanged; only the settlement
  timing is.
- **The LBP escrows its at-close fee the same way**, in `PoolState.treasury_owed`,
  and `Withdraw` no longer names a treasury account at all. The LBP takes no
  per-swap fee, so nothing is owed until the creator withdraws; at that point the
  fee is charged against the creator's own share rather than the raw vault
  balance - which is what makes a second withdrawal credit the bucket zero, since
  that share is already gone - and stays in the vault for `SweepTreasury`. The
  invariant both programs now hold is
  `collateral_vault_balance == reserve_collateral + treasury_owed`.
- **The LBP's collateral-vault owner is chain-determined, not creator-supplied.**
  `CreateSale` now takes the collateral token's definition account - one of the two
  definition slots the account list gained - and dispatches the seed leg to
  whichever program owns *that*.
  That leg is the one whose claim types the vault, so reading its program off the
  creator's own holding let a creator name a real, valuable token in
  `collateral_definition_id` while the vault belonged to a program they had
  deployed. Buyers were never at risk (the payer/vault bind rejects every honest
  holder, so such a pool is merely unbuyable), but `pool.collateral_definition_id`
  stopped being evidence of what the pool accepts - which is exactly how the SDK
  and every indexer read it.
- The LBP's interpolated weight moved out of the pool account into its own PDA,
  because `Poke` is permissionless: writing it into the pool let anyone invalidate
  every in-flight private buy for the price of one transaction that changes
  nothing economically.
- wLEZ's `unwrap` asserts that the holding it burns from is owned by the token
  program that owns the WLEZ definition, matching the pin `wrap` already had.
  This is defence in depth and not a closed hole: the substitution it refuses is
  already rejected by the framework, because the chained `token::Burn` would be
  writing an account the token program does not own. What it buys is that wLEZ's
  solvency stops resting on an invariant of *another* program's source, and that
  the failure names the real mistake instead of surfacing as an opaque framework
  rejection attributed to the token program.

### Fixed

- **Every private command could hang forever, and one did - for 5h33m.** Each of
  the seven private entry points (`shield`, `deshield`, both launchpads'
  `buy-private`/`buy-disposable`, `bc sell-private`) begins by bringing the
  wallet's shielded note view to chain head, because spending against a stale
  view re-derives an output commitment that is already on chain and the
  sequencer rejects it as seen - after the proof has been paid for. That sync
  was a bare `block_on` with no deadline of any kind, and a live run sat inside
  it for five and a half hours at 0% CPU - indistinguishable from a hung
  process.

  **Which await hung was never established, and the fix does not depend on
  knowing.** The bound is on the whole call rather than on any one await inside
  it. The obvious suspect - a stalled block-stream request - does not actually
  survive scrutiny: those requests go through a jsonrpsee HTTP client whose
  builder default is a 60-second request timeout, so a dead socket surfaces as
  an error rather than a wedge. A bound placed on the await we guessed at would
  have been a bound resting on a guess.

  **What it does not bound, stated rather than glossed:** synchronous work
  inside the future. A `tokio` timeout fires only when the task yields, and the
  wallet writes its entire storage to disk synchronously, inside the async task,
  once per block. A write that never returns would still hang here and would
  present exactly the observed symptom - 0% CPU. Fixing that means moving the
  sync off the runtime thread, which this wrapper cannot do while it holds
  `&mut wallet`.

  The sync now runs in windows, and the bound is a **stall** bound rather than a
  timeout: the clock restarts every time the wallet's last-synced block
  advances, so a slow-but-healthy first scan of a long chain is never killed for
  being slow. Only a window that scans *no block at all*
  (`$LPAD_SYNC_STALL_SECS`, default 300) fails, plus a backstop for the one case
  progress cannot terminate - a chain producing blocks faster than the wallet
  scans them (`$LPAD_SYNC_MAX_SECS`, default 3600).

  Cutting a window short is safe by construction, not by assumption: the only
  `.await` in the wallet's per-block loop is the one that pulls the next block,
  and both `set_last_synced_block` and `store_persistent_data` run synchronously
  before it, so a cancelled window is always cancelled *between* blocks with
  every scanned block already persisted. The next window resumes from
  `last_synced_block + 1` and rescans nothing. Dropping the stream is in fact
  the repair - a fresh window opens a fresh connection, which is exactly what a
  wedged one needs. Failing is clean on chain too: every caller syncs as its
  first act, before anything is quoted, proved, signed or submitted.

- **A private launch left its operator unable to close or withdraw.**
  `bc create-token-sale --private` mints a fresh, unlinkable creator account and
  a token holding for it inside the command, and printed neither: output was the
  sale id, the token definition and the tx hash. But `bc close --creator <id>`
  and `bc withdraw --creator <id> --creator-token <id> --creator-collateral <id>`
  accept no other accounts, and no lpad output anywhere rendered a sale's
  creator - so the wallet held the keys while the ids were, in the strict sense,
  unknowable. The raise was unwithdrawable and the sale unclosable, permanently,
  by design rather than by accident.

  The launch now reports `creator` and `creator token` in both renderings (as
  `creator` / `creator_token_holding` under `--json`), `bc sale-info` reports the
  sale's creator, and `lbp pool-info` - which had the same hole against `lbp
  close --creator` / `lbp withdraw --creator` - reports the pool's.

  On the privacy question, since these ids exist to be unlinkable: a creator is
  plain, unencrypted state on the sale, readable on chain by anyone who reads the
  sale, so `sale-info` publishes nothing new. What `--private` buys is that the
  creator cannot be tied to the wallet that funded it, and showing an id to its
  own owner in their own terminal does not touch that - unlinkability is a
  property of what was published, not of what the launching process knows. The
  one act that does undo it is the operator pasting the creator next to the sale
  id somewhere public, which is exactly the pair the shield/deshield round trip
  removed, so the private arm says so on the line after and the help and README
  say it again.
- **`--json` listings could be short with nothing to say so.** `my-sales` and
  `my-pools` warn on stderr when the chain's creator index names a sale this run
  could not read. A `--json` consumer sees no stderr: it got a well-formed
  listing, missing that row, and exit 0 - which reads as "that sale does not
  exist", the precise harm the warning was added for. The payload now carries
  `partial` (always present, `false` when complete) and `unreadable` (each id
  with its read error). The exit code stays 0 deliberately: the rows that are
  there are real, and failing hard would discard them along with the warning -
  so the marker rides with the data and scripts branch on `partial`.
- **A false justification in the output sanitiser.** `ui::safe`'s doc said
  `--json` needs no escaping because "serde_json escapes control characters
  itself". serde_json escapes C0 only, so U+009B (the single-character C1 CSI),
  U+2028/2029 and the bidi overrides U+202A-202E reach a terminal raw out of a
  `--json` payload. Low severity - it takes a `jq -r` into a live terminal, and
  `jq` would un-escape anything lpad escaped anyway - but the reason was untrue,
  which is the part that rots. `--json` stays byte-faithful, because the payload
  is contracted to carry chain state exactly as stored, and the doc now says that
  instead. A test (`serde_json_escapes_only_c0_so_the_json_note_cannot_go_stale`)
  pins what serde_json does and does not escape, so the note fails loudly rather
  than quietly becoming false again.
- **A buy leg was dispatched to a program the buyer chose.** Both launchpads took
  the chained `token::Transfer`'s program id from the buyer's own collateral
  holding. Deployment on LEZ is permissionless, so a buyer could pay with a
  holding owned by a no-op program whose `Transfer` simply echoes its pre-states:
  the collateral legs then move nothing while the curve still consumes sale
  reserve and credits `real_collateral`. That is a free sale-kill on any sale,
  and on a two-way one a drain, because the inflated `real_collateral` bounds a
  later `Sell` that pays out of the vault's real token program. Dispatch is
  anchored to the vaults now - `create_sale` initialises them, so their owner is
  not attacker-chosen - and the payer must be owned by that same program.
- **Two ways to lock the protocol fee, one of which took the whole raise with
  it - on BOTH launchpads.** Settling the escrow inside `Withdraw` put the
  creator's collateral *and* the unsold project tokens behind a `token::Transfer`
  to an account `CreateSale` cannot type-check; splitting `SweepTreasury` out
  bounds that failure to the fee bucket, which is the accepted residual risk
  recorded on `SaleState::treasury_owed` / `PoolState::treasury_owed`. The LBP is
  where that bug was still live when this release opened: its at-close fee was a
  leg of `Withdraw`, so an unusable treasury - uninitialised, wrong definition,
  wrong token program - reverted the whole withdrawal, and a closed pool has no
  other drain. Not a stranded fee: the entire raise and every unsold project
  token, permanently. Separately, a `treasury_id` aliasing the creator,
  the sale or pool PDA, or either vault could never be settled at all: LEZ
  rejects a message with a repeated account id before the program runs, so the
  one instruction that could move those funds can never be submitted. Both
  launchpads reject such a treasury at creation, which is the last point it is
  free to catch.

  Both programs now carry the same pair of end-to-end regression tests, in
  `programs/integration_tests/`: `withdraw_leaves_the_escrow_and_sweep_treasury_settles_it`
  walks the whole path (raise, close, withdraw, sweep, re-sweep-reverts), and
  `an_unusable_treasury_cannot_block_the_creators_withdrawal` gives the pool a
  treasury of the wrong token definition and asserts the creator is still paid in
  full while the sweep - and only the sweep - fails.
- **The project-token deposit was dispatched to a program the creator chose.**
  `CreateSale` read `token_definition_id` out of the creator's own holding and
  ran the deposit leg on that holding's owner. Deployment on LEZ is
  permissionless, so a creator could advertise a real, valuable definition id
  while handing over a holding owned by a token program they had deployed: that
  leg is what CLAIMS the token-vault PDA, so the vault came out owned by the
  impostor and every later buy dispatched its token leg straight back into it -
  buyers paying real collateral for a holding that merely *reads* as the valuable
  token. Both programs now take the definition as an account, pin it by
  `account_id` to the id the holding declares, and dispatch on **its** owner,
  with the holding required to be owned by that same program. This is the
  project-token twin of the collateral-side fix above; the LBP already had it on
  the collateral leg and neither program had it on the project leg.

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
- **Deployment verification.** `scripts/verify-deployment.sh` asserts that the
  sale, pool and wLEZ vault PDAs exist *and* are owned by the program ids this
  build embeds, and that the canonical ATA this build pins is a live token
  holding - so all four programs a trade touches are covered, not just the two
  that a sale and a pool make visible. `bootstrap.sh` proves each guest's own ELF
  bytes are in a block rather than trusting the wallet's report.
- **`scripts/private-ops.sh`**, a serialised real-proof sweep of the shielded
  operations with per-op wall-clock timings. It refuses to run under
  `RISC0_DEV_MODE=1` (a real sequencer verifies proofs) or alongside another
  prover (each is ~8-9 GB resident).
- **`docs/UPGRADE-v0.2.4.md`**, the migration record, including the failure modes
  that only appear against a real network. Kept local rather than published, like
  the rest of `docs/`; `docs/PRIVACY.md` is the one file published, because both
  RFPs require a privacy and anonymisation properties document by name.
- **`scripts/audit.sh`** and `.cargo/audit.toml`. All three lockfiles pass
  `cargo audit --deny warnings` with the ignore list in `.cargo/audit.toml`
  applied - that list is what makes them pass, and each entry names the advisory
  and where it comes from. CI runs the same script on every push to `main` and
  every pull request, against an advisory database fetched fresh on the runner,
  so a newly published advisory fails the build; a local run against a database
  that has not been fetched in months proves nothing.
- **`lpad program-id wlez`** additionally prints the definition and vault PDAs it
  owns. They are pure derivations from the program id, and they are the only
  handle a deployment check has on wlez - unlike a sale or a pool, nothing records
  a wlez account id during bootstrap, which is why the standing check had silently
  covered two of the deployed programs. It now covers all four.

### Gates

Two guards existed but were never executed by CI, and two more are new. Each of
these fails in a way that is invisible on chain: a wrong program id is not an
error, it is a transaction that never lands.

- **SDK chain-parity**: the pinned LEZ build's built-in `token`,
  `authenticated_transfer` and `clock` image ids must equal constants read off
  both public sequencers while this release was prepared. It compares committed
  constants and makes no RPC call, so it catches lpad drifting off that snapshot
  but not the operators upgrading underneath it. `sdk` is its own workspace, so
  neither the `programs` nor the `cli` test step ran it.
- **`pinned_ids_match_artifacts`**: the committed guest ELFs must still hash to
  the ids in `lpad_guests::deployed`, which every sale and pool PDA is derived
  from. Needed `-p lpad_guests --features artifacts`, which the gate did not pass.
- **Cross-workspace id parity** (new): the same assertion, made in `sdk` - the id
  that actually reaches a sequencer is the one the CLI embeds, and the CLI is
  built from a different workspace with a different lockfile, which is exactly
  where a divergence would hide.
- **ABI drift**: `scripts/ci-e2e.sh` regenerates the ABIs and fails on a diff.
  The comment promising this check had been there without the check.

All four now run on `.github/workflows/ci.yml`, on a stock GitHub-hosted runner,
on every push to `main` and every pull request - as do the CLI suite and the 22
in-process E2E tests. None of that needs the risc0 toolchain, Docker, a sequencer
or a secret, and it had all been gated only by a self-hosted workflow that no
runner has ever picked up. What genuinely needs a bigger machine - rebuilding the
guests byte-reproducibly, and the live smoke test - stays in `e2e.yml` and is
inert until someone registers that runner.

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
- **A private buy is three transactions, not one.** It is deshield (proof) →
  public buy → re-shield (proof), sequenced in memory by the SDK with no durable
  journal. The middle leg is a public transaction by construction, and a crash
  between legs leaves the funds sitting in the public ephemeral account: they are
  recoverable and nothing is lost, but the privacy of that one trade is. A
  crash-safe journal is planned, not shipped.
- **Testnet state is perishable.** Both public testnets were reset without warning
  during development, so any sale, pool or account id quoted in a release - or
  left in an uncommitted `scripts/bootstrap.*.env` - is good only until the next
  reset.
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
