//! `CreateSale` - open a new bonding-curve sale.
//!
//! Account order (top-level): `[sale, token_vault, collateral_vault, treasury,
//! token_definition, collateral_definition, creator_token_holding, creator,
//! creator_index, clock]`.
//! - `sale`                  - uninitialized PDA, claimed by this program.
//! - `token_vault`           - uninitialized PDA; receives `D + R` project tokens.
//! - `collateral_vault`      - uninitialized PDA; initialized empty here so the
//!   ATA buy path (`ata::Transfer`, which rejects a default recipient) works on
//!   the very first buy, and so the vault is typed to the collateral definition
//!   up-front (no lazy-init token-type inheritance). Mirrors the LBP pre-seed.
//! - `treasury`              - the fee sink, ALREADY an initialized Fungible
//!   holding of the sale's collateral definition under that definition's own
//!   token program. Read-only, echoed unchanged; it is in this list purely so the
//!   pin below can type-check it. Sits directly after the collateral vault, the
//!   slot it occupies in `Buy`/`Sell`/`BuyAta`/`SellAta` too.
//! - `token_definition`      - the project token's definition account. Pinned to
//!   the definition id the creator's holding declares, and the token program that
//!   owns it is what the `D + R` deposit is dispatched on (see the SECURITY note
//!   in the body).
//! - `collateral_definition` - the sale's collateral token definition (needed to
//!   initialize the empty collateral vault holding, and the anchor the treasury's
//!   definition and token program are pinned against).
//! - `creator_token_holding` - creator's project-token holding (sender, signer).
//! - `creator`               - creator identity account (signer); stored for withdrawal auth.
//! - `creator_index`         - the creator's `CreatorIndex` PDA: claimed here on
//!   their first sale, appended to on every later one. Sits next to the creator
//!   because it is derived from them and from nothing else.
//! - `clock`                 - read-only CLOCK_01, echoed unchanged.

use bonding_curve_core::{
    compute_collateral_vault_pda, compute_collateral_vault_pda_seed, compute_creator_index_pda,
    compute_creator_index_pda_seed, compute_sale_pda, compute_token_vault_pda,
    compute_token_vault_pda_seed, read_fungible, CreatorIndex, SaleState, SaleStatus, MAX_FEE_BPS,
    MAX_METADATA_LEN, MAX_VIRT_COLLATERAL,
};
use lee_core::{
    account::{Account, AccountId, AccountWithMetadata, Data},
    program::{AccountPostState, ChainedCall, Claim, ProgramId, DEFAULT_PROGRAM_ID},
};

use crate::util::authorized;

#[expect(clippy::too_many_arguments, reason = "fixed protocol account/param list")]
#[must_use]
pub fn create_sale(
    sale: AccountWithMetadata,
    token_vault: AccountWithMetadata,
    collateral_vault: AccountWithMetadata,
    treasury: AccountWithMetadata,
    token_definition: AccountWithMetadata,
    collateral_definition: AccountWithMetadata,
    creator_token_holding: AccountWithMetadata,
    creator: AccountWithMetadata,
    creator_index: AccountWithMetadata,
    collateral_definition_id: AccountId,
    treasury_id: AccountId,
    token_name: String,
    token_symbol: String,
    sale_quantity: u128,
    dex_seed_quantity: u128,
    virt_token: u128,
    virt_collateral: u128,
    fee_bps: u128,
    one_directional: bool,
    end_timestamp_ms: u64,
    min_duration_ms: u64,
    nonce: u64,
    ata_program_id: ProgramId,
    self_program_id: ProgramId,
    clock_ts: i64,
) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
    // --- parameter validation -------------------------------------------------
    assert!(fee_bps <= MAX_FEE_BPS, "fee_bps exceeds the maximum allowed");
    assert!(sale_quantity > 0, "sale quantity D must be positive");
    assert!(virt_collateral > 0, "virtual collateral Vc must be positive");
    assert!(
        virt_token > sale_quantity,
        "virtual token reserve Vt must exceed sale quantity D"
    );
    let k = virt_token
        .checked_mul(virt_collateral)
        .expect("k = Vt * Vc overflows u128 - choose smaller virtual reserves");
    // Vc is synthetic (no real deposit backs it), so a creator can set it freely.
    // Bound it to the Q64.64 spot-price domain here: spot_price_q64 (run on every
    // public buy/sell via record_observation) asserts the same bound, so an
    // out-of-domain Vc would otherwise revert every public swap once the sale is
    // live. Reject it at creation instead. Mirrors the LBP MAX_RESERVE defense.
    //
    // The live Vc *grows* on every buy (apply_buy does Vc += c_eff), heading toward
    // its peak when all D tokens are sold and Vt has fallen to `virt_token -
    // sale_quantity`. `ceil_div(k, virt_token - sale_quantity)` is only the
    // *idealized* peak that assumes the constant product stays exactly at k; in the
    // discrete engine each buy floors tokens_out (buy_tokens_out) while adding the
    // full c_eff, so Vt*Vc grows strictly above k and the *realized* live Vc
    // overshoots that idealized value (empirically by up to ~1.62x, independent of
    // scale). Bounding only the idealized peak would let a sale whose params pass
    // still drive Vc across 2^64 mid-curve, where every further buy/sell/close
    // would revert (and, with end_timestamp_ms == 0, the sale could never close),
    // permanently locking deposited collateral. Reserve 2x headroom - which covers
    // the worst-case ~1.62x rounding overshoot - so the realized Vc stays inside
    // the Q64.64 domain on every reachable trade path. (The spot-price oracle also
    // saturates rather than asserting, so an observation can never wrap; this bound
    // additionally protects the private path, which records no observation.)
    assert!(
        bonding_curve_core::ceil_div(k, virt_token - sale_quantity)
            .checked_mul(2)
            .is_some_and(|v| v < MAX_VIRT_COLLATERAL),
        "max reachable Vc exceeds the Q64.64 spot-price domain (choose larger Vt-D or smaller Vc)"
    );
    assert!(creator.is_authorized, "creator must authorize sale creation");
    // Bound the self-describing metadata so the serialized sale state stays under
    // Data's 100 KiB cap even once the 64-entry observation ring fills; an
    // unbounded name/symbol could otherwise grow the state across that cap
    // mid-life, panicking the SaleState->Data encode on every later buy/sell/close
    // and permanently locking deposited collateral.
    assert!(
        token_name.len() <= MAX_METADATA_LEN && token_symbol.len() <= MAX_METADATA_LEN,
        "token name/symbol exceed the maximum metadata length"
    );
    // SECURITY: BuyAta/SellAta dispatch the user's token/collateral leg through
    // this pinned program (an owner-authorized PDA spend). A zero/default id can
    // never name a real ATA program, so it could only resolve to a no-op leg that
    // skips the actual token::Transfer while the vault legs still pay out -
    // draining the collateral vault. Reject it here. (The canonical-ATA image id
    // is not a compile-time constant in this crate - programs are addressed by
    // guest image id, unlike the sequencer-seeded CLOCK_01 AccountId - so the SDK
    // validates the pinned id against the deployed ATA program for participants.)
    assert!(
        ata_program_id != DEFAULT_PROGRAM_ID,
        "ata_program_id must be a real ATA program (not the default/zero id)"
    );
    if end_timestamp_ms != 0 {
        let now = u64::try_from(clock_ts.max(0)).expect("clock timestamp must be non-negative");
        assert!(
            end_timestamp_ms >= now.saturating_add(min_duration_ms),
            "end timestamp must be at least min_duration_ms in the future"
        );
    }

    // --- identity / definitions ----------------------------------------------
    let (token_definition_id, creator_balance) =
        read_fungible(&creator_token_holding, "CreateSale: creator token holding");
    let total_deposit = sale_quantity
        .checked_add(dex_seed_quantity)
        .expect("D + R overflows u128");
    assert!(
        creator_balance >= total_deposit,
        "creator holding does not cover D + R project tokens"
    );
    assert!(
        token_definition_id != collateral_definition_id,
        "project and collateral tokens must differ"
    );
    // SECURITY: pin the project token the same way the collateral side is pinned
    // below - by an account, not by bytes the creator wrote.
    //
    // `token_definition_id` is read out of `creator_token_holding`'s DATA, and
    // LEZ deployment is permissionless, so a creator can advertise a real,
    // valuable definition id while handing us a holding owned by a token program
    // they deployed themselves. Dispatching the deposit on that holding's owner
    // would run the whole sale on the impostor: the deposit leg is authorized to
    // claim the token-vault PDA (the vault is a default account, so
    // `token::Transfer`'s `new_claimed_if_default(.., Claim::Authorized)` takes
    // it), the vault comes out owned by the impostor, and every later buy
    // dispatches its token leg on `token_vault.account.program_owner` - i.e. back
    // into the impostor. Buyers pay real collateral and receive a holding that
    // merely *reads* as the valuable token.
    //
    // Naming the definition as an account and dispatching on ITS owner ties the
    // sale to whatever token program actually owns the definition it advertises.
    // The creator's holding must then be owned by that same program, or the
    // deposit's sender is foreign to the program executing it (which LEZ would
    // reject deep inside the chained call as an unauthorized data modification).
    assert_eq!(
        token_definition.account_id, token_definition_id,
        "token definition account does not match the definition id the creator's holding declares"
    );
    assert_eq!(
        creator_token_holding.account.program_owner, token_definition.account.program_owner,
        "creator token holding is not owned by the project token definition's own token program"
    );
    // ...and it must really be a token definition. Unlike the collateral side -
    // whose vault-init leg is a chained `token::InitializeAccount` that takes the
    // definition account and therefore parses it - the project-token deposit leg
    // is a `Transfer` that never names this account, so nothing downstream would
    // notice. Without this, an uninitialized account surfaces only as LEZ's
    // opaque "Unknown program" rejection of the chained call, from a program id
    // read off the very account that was wrong. Mirrors `lbp::create_sale`, which
    // asserts this on both of its definitions.
    //
    // Fungible SPECIFICALLY, on both definitions. `token::InitializeAccount`
    // types a holding from the definition it is handed, so a NonFungible one
    // yields an `NftPrintedCopy` holding - and every balance this program ever
    // reads goes through `read_fungible`, which panics on that variant. Naming an
    // NFT collection here is self-inflicted rather than hostile, but it is
    // unrecoverable: the `D + R` deposit lands and then `Withdraw` dies on the
    // vault it cannot read, stranding the creator's whole deposit for good.
    assert!(
        matches!(
            token_core::TokenDefinition::try_from(&token_definition.account.data),
            Ok(token_core::TokenDefinition::Fungible { .. })
        ),
        "project token definition account does not hold a FUNGIBLE token definition - pass the \
         project token's definition account, not a holding of it, and not an NFT collection \
         (a bonding curve can only sell a fungible token)"
    );

    // --- PDA checks -----------------------------------------------------------
    let sale_id = compute_sale_pda(
        self_program_id,
        token_definition_id,
        collateral_definition_id,
        creator.account_id,
        nonce,
    );
    assert_eq!(sale.account_id, sale_id, "sale account id does not match PDA");
    assert_eq!(
        sale.account,
        Account::default(),
        "sale account must be uninitialized"
    );
    let token_vault_id = compute_token_vault_pda(self_program_id, sale_id);
    assert_eq!(
        token_vault.account_id, token_vault_id,
        "token vault id does not match PDA"
    );
    // Every vault PDA is claimed by a chained call's `new_claimed_if_default`, so
    // each needs the same precondition as the sale account itself. Without the
    // assert the failure is an opaque framework rejection: the top-level
    // post-state echoes the pre-state, so a DEFAULT-owned non-default vault trips
    // validate_execution rule 7 (`NonDefaultAccountWithDefaultOwner`), naming the
    // PDA and nothing about why.
    //
    // NOT a griefing surface, whatever this comment used to say. A vault id is
    // `AccountId::for_public_pda(this program, hash(sale_id | tag))` - a keyless
    // derivation of THIS program's id - and no stranger can put data, balance or
    // a nonce there. The full argument is on the creator-index claim below; the
    // index PDA and these two vault PDAs are the same kind of account in exactly
    // this respect, and neither can be dusted.
    //
    // So the pre-state here is pristine or already ours, and a non-default vault
    // means either that a sale with this (token def, collateral def, creator,
    // nonce) already exists - in which case the sale assert above fired first -
    // or a seed collision. Defence in depth, then. The "different nonce" remedy
    // is still the right thing to print: the nonce is the one input the creator
    // controls that moves the sale id, and with it both vault derivations.
    assert_eq!(
        token_vault.account,
        Account::default(),
        "token vault must be uninitialized - if this PDA already exists, re-create the sale \
         with a different nonce"
    );
    let collateral_vault_id = compute_collateral_vault_pda(self_program_id, sale_id);
    assert_eq!(
        collateral_vault.account_id, collateral_vault_id,
        "collateral vault id does not match PDA"
    );
    assert_eq!(
        collateral_vault.account,
        Account::default(),
        "collateral vault must be uninitialized - if this PDA already exists, re-create the \
         sale with a different nonce"
    );
    assert_eq!(
        collateral_definition.account_id, collateral_definition_id,
        "collateral definition account does not match the declared collateral definition id"
    );
    // The `InitializeAccount` leg below parses this account too, so this assert is
    // belt-and-braces rather than the only thing standing between a creator and a
    // bad definition. It is here so the failure is local and legible - a chained
    // call rejected inside the token program names neither this account nor what
    // was wrong with it - and so that the two launchpads, which are documented as
    // identical in shape here, actually are.
    //
    // The treasury pin below already implies the Fungible half of this - a
    // `TokenHolding::Fungible` can never name a NonFungible definition, so an
    // NFT-collateral sale has no valid treasury to pass. Kept anyway, because
    // that rejection would arrive as a message about the TREASURY while the
    // creator's actual mistake was the collateral definition they named. A
    // legible error is worth two lines.
    assert!(
        matches!(
            token_core::TokenDefinition::try_from(&collateral_definition.account.data),
            Ok(token_core::TokenDefinition::Fungible { .. })
        ),
        "collateral definition account does not hold a FUNGIBLE token definition - pass the \
         collateral token's definition account, not a holding of it, and not an NFT collection \
         (a sale can only be denominated in a fungible collateral token)"
    );
    // --- per-creator sale index ----------------------------------------------
    //
    // ONE account per (program, creator) holding the ids of every sale that
    // creator has opened, so listing a wallet's sales is a single read instead of
    // re-deriving every PDA it could conceivably have made (a product over
    // creator x token def x collateral def x nonce - thousands of round-trips,
    // measured at tens of minutes on a wallet with history).
    //
    // Pinned to the PDA of (this program, the creator that SIGNED this
    // instruction), exactly as the vaults are pinned to the sale: the submitter
    // picks every non-signer account id, so without this pin a sale could be
    // appended into a stranger's index and pollute their listing.
    //
    // Two sales from one creator in the SAME BLOCK are two appends to this one
    // account, and they do not collide. A public transaction is executed at
    // inclusion against the running state, not against a snapshot the submitter
    // pinned: `ValidatedStateDiff::from_public_transaction` builds every
    // pre-state from `state.get_account_by_id` and applies each transaction's
    // diff before the next one runs, so the second CreateSale reads the index the
    // first one just wrote and takes the append branch below. (This is exactly
    // what a proof-pinned path could NOT do - which is why `BuyDisposable` pins
    // no account whose data moves. CreateSale has no private form.)
    let creator_index_id = compute_creator_index_pda(self_program_id, creator.account_id);
    assert_eq!(
        creator_index.account_id, creator_index_id,
        "creator index account is not the index PDA of the signing creator"
    );
    let mut index_account = creator_index.account;
    let creator_index_post = if index_account == Account::default() {
        // Lazy claim on first use, like the vault PDAs - except this one is
        // claimed by the top-level program directly rather than by a chained
        // token call, because it holds this program's own data.
        //
        // The vaults take a `nonce` in their derivation and this PDA does not.
        // It needs none - and neither, on the dusting argument, do they: no
        // stranger can put a byte, a balance or a nonce at a keyless PDA derived
        // from THIS program's id, so the pre-state here is either pristine or
        // already ours. Verified against LEZ v0.2.4:
        //   * DATA or BALANCE. Both are account modifications, and the public
        //     path's final sweep in `ValidatedStateDiff::from_public_transaction`
        //     (`validated_state_diff/mod.rs`) rejects the whole transaction with
        //     `DefaultAccountModifiedWithoutClaim` for any DEFAULT-owned account
        //     whose post-state differs from its pre-state and was not CLAIMED.
        //     The privacy circuit enforces the same rule on public accounts
        //     ("Account {id} was modified but not claimed" in
        //     `privacy_preserving_circuit/src/execution_state.rs`). Note that
        //     `validate_execution` rule 5 lets any program RAISE any account's
        //     balance - it is that later sweep, not rule 5, that makes dusting a
        //     foreign PDA impossible rather than merely rude. (A DEFAULT-owned
        //     account can hold a balance only if GENESIS gave it one; after that
        //     no transaction can create the state.)
        //   * The CLAIM. `Claim::Pda(seed)` is admitted only when the account id
        //     equals `AccountId::for_public_pda(<claiming program>, seed)`, so no
        //     other program can claim one of our derivations; `Claim::Authorized`
        //     needs the account authorized, which for a keyless PDA means a
        //     `pda_seeds` delegation - and `compute_public_authorized_pdas` keys
        //     those off the CALLER's program id, so only this program can
        //     delegate ours. It delegates the two vault seeds and nothing else.
        //   * The NONCE. Only signer accounts' nonces are bumped
        //     (`State::apply_state_diff`), and a signer id is the hash of a
        //     public key while this id is a hash under LEZ's PDA domain
        //     separator - signing for it would mean finding a preimage.
        //
        // So the vaults' "re-create with a different nonce" hint is not a dusting
        // escape hatch: it is the remedy for the only shape that can actually
        // reach their assert, a sale that already exists.
        let mut index = CreatorIndex::new(creator.account_id);
        index.push(sale_id);
        index_account.data = Data::from(&index);
        AccountPostState::new_claimed(
            index_account,
            Claim::Pda(compute_creator_index_pda_seed(creator.account_id)),
        )
    } else {
        // The append path: an index this program already owns, holding the sales
        // this creator opened before.
        //
        // Any OTHER non-default shape here is unreachable on a live chain, for
        // exactly the reasons set out above - which is also why there is no
        // "bump the nonce" escape hatch to offer and, emphatically, why changing
        // creator account is not a remedy. Kept as defence in depth against a
        // future seed change, a derivation collision, or a caller aiming this
        // slot somewhere else; if it ever fires, the state it names should not
        // exist, so the message says that rather than sending the operator off
        // to abandon a working identity. Mirrors `lbp::create_sale`.
        assert_eq!(
            index_account.program_owner, self_program_id,
            "creator index account exists but is not owned by this program - unreachable on a \
             live chain, since only this program can claim this PDA and LEZ refuses to modify a \
             DEFAULT-owned account without a claim. Report it; a different creator account is \
             not a fix, it is an abandoned identity"
        );
        let mut index = CreatorIndex::try_from(&index_account.data)
            .expect("creator index account does not hold a bonding-curve creator index");
        // Unreachable while the PDA pin above holds - kept so that a future seed
        // change, or a collision, merges nobody's sale list into anybody else's.
        assert_eq!(
            index.creator, creator.account_id,
            "creator index belongs to a different creator"
        );
        index.push(sale_id);
        index_account.data = Data::from(&index);
        AccountPostState::new(index_account)
    };

    // The treasury must be a standalone fee sink: not one of the protocol's own
    // accounts, and not the creator's identity account.
    //
    // Vaults - sweeping fees into one would corrupt the raised-collateral
    // accounting. Sale PDA - a fee transfer would write token-holding bytes over
    // the live sale state.
    //
    // Beyond that, an alias is not merely odd, it is unusable, in two ways.
    // (1) LEZ dedups `message.account_ids` into a HashSet and rejects the message
    // with `InvalidInput("Duplicate account_ids")` BEFORE executing anything, so
    // any instruction that names the treasury alongside the account it aliases can
    // never run at all. `SweepTreasury` is `[sale, collateral_vault, treasury]`
    // and both of those ids are pinned from the sale state, so there is no
    // alternative account list to fall back on: the escrowed fee would be stranded
    // for good.
    // (2) The creator slot is an identity signer, not a Fungible holding of the
    // collateral definition, so the fee leg of every public buy and the sweep's
    // transfer would revert against it. "Just send the fees to me" is the natural
    // thing for a creator to type, and it is worth its own error: the type pin
    // below would reject it too, but only as "not a token holding", which does not
    // tell the creator that the treasury has to be a *separate* account.
    //
    // The alias rules therefore run BEFORE the type pin, so an alias is diagnosed
    // as an alias.
    assert!(
        treasury_id != collateral_vault_id
            && treasury_id != token_vault_id
            && treasury_id != sale_id
            && treasury_id != creator.account_id
            && treasury_id != creator_index_id,
        "treasury must not alias the sale account, its token or collateral vault, or the \
         creator - pass a separate Fungible holding of the collateral definition"
    );
    // ...and the treasury has to be an account the fee can actually reach. The
    // rules above constrain the ID; these three constrain the ACCOUNT, which is
    // the whole reason the treasury occupies a slot in this instruction's list.
    //
    // SECURITY: `SweepTreasury` is the ONLY instruction that pays out
    // `state.treasury_owed`, and its fee leg is a `token::Transfer` dispatched on
    // the collateral vault's own token program. A treasury that cannot receive
    // that transfer makes the escrow unsweepable FOREVER: `Withdraw` deliberately
    // subtracts the escrow from the creator's payout, and nothing rewrites
    // `treasury_id`. Three shapes get in if the treasury is only an id:
    //   * UNINITIALISED. Settling then has to CLAIM the treasury, and
    //     `token::transfer` claims a recipient with `Claim::Authorized`, which
    //     LEZ admits only when the account BEING CLAIMED is itself authorized. So
    //     nothing but the treasury's own signature could ever settle such a sale
    //     - and that same key is the one thing that can destroy the id, because a
    //     claim also needs the pre-state to be `Account::default()` WHOLE while
    //     `State::apply_state_diff` bumps every signer's nonce, DEFAULT-owned or
    //     not. Let it sign anything else first and the account can never be
    //     claimed by any program again: it can never receive a token at all, and
    //     the escrow is dead. This is NOT a stranger's grief - LEZ rejects any
    //     transaction that leaves a DEFAULT-owned account modified but unclaimed
    //     (`DefaultAccountModifiedWithoutClaim`), balance included, and a third
    //     party has neither the treasury's signature nor a program whose PDA
    //     derivation is that id. The hazard is self-inflicted and permanent,
    //     which is why creation is where it gets refused.
    //   * A holding of the WRONG DEFINITION. The fee leg moves the collateral
    //     vault's token; a recipient holding something else reverts every time.
    //   * A holding of the right definition under a DIFFERENT TOKEN PROGRAM. Just
    //     as stuck, and this one is not hypothetical: LEZ deployment is
    //     permissionless, so "a Fungible holding naming the collateral definition"
    //     is a shape anyone can mint under a program of their own.
    //
    // Requiring an ALREADY-INITIALISED holding closes all three, and closes them
    // permanently rather than probably: an initialised holding is owned by the
    // token program, and LEZ's token program has no instruction that
    // un-initialises one (`burn` only lowers a balance), so the state cannot
    // regress between creation and the last sweep. That is what lets
    // `sweep_treasury`'s DEFAULT-owned branch be dead code for any sale this build
    // creates - see the note there.
    assert_eq!(
        treasury.account_id, treasury_id,
        "the treasury account passed does not match the treasury_id declared in the instruction \
         - pass the account for the id being pinned into the sale"
    );
    // Checked before `read_fungible` so the commonest mistake - naming a treasury
    // that does not exist yet - gets the answer instead of "expected a valid Token
    // Holding account".
    assert_ne!(
        treasury.account,
        Account::default(),
        "the sale's treasury account does not exist yet - initialise it as a Fungible holding of \
         the collateral definition (token::InitializeAccount) and re-run CreateSale. It must \
         exist now: settling a fee into an uninitialised treasury would need that treasury's own \
         signature on the sweep, and that same key locks the id out of the fee for good the \
         moment it signs anything else"
    );
    let (treasury_definition_id, _) = read_fungible(&treasury, "CreateSale: treasury");
    assert_eq!(
        treasury_definition_id, collateral_definition_id,
        "the sale's treasury holds a different token than the sale's collateral - pass a \
         Fungible holding of the collateral definition, which is the only token this sale will \
         ever pay it"
    );
    assert_eq!(
        treasury.account.program_owner, collateral_definition.account.program_owner,
        "the sale's treasury is not owned by the collateral definition's own token program - the \
         fee leg is dispatched on the collateral vault's program, so a holding under any other \
         token program can never receive it. Pass a treasury minted from the collateral \
         definition itself"
    );

    // --- build sale state -----------------------------------------------------
    let state = SaleState {
        creator: creator.account_id,
        token_definition_id,
        collateral_definition_id,
        token_vault_id,
        collateral_vault_id,
        treasury_id, // admin-configured fee sink; every buy/sell validates the passed treasury against this
        ata_program_id, // pinned here (rejected if default); BuyAta/SellAta reject any other ATA program, the SDK rejects a non-canonical pin
        fee_bps,
        one_directional,
        end_timestamp_ms,
        min_duration_ms,
        nonce,
        created_ts_ms: clock_ts,
        virt_token,
        virt_collateral,
        k,
        sale_reserve_initial: sale_quantity,
        dex_seed_reserve: dex_seed_quantity,
        sale_reserve: sale_quantity,
        real_collateral: 0,
        treasury_owed: 0,
        status: SaleStatus::Open,
        cum_collateral_in: 0,
        cum_fees: 0,
        buy_count: 0,
        sell_count: 0,
        obs: Vec::new(),
        token_name,
        token_symbol,
    };

    let mut sale_post = sale.account.clone();
    sale_post.data = Data::from(&state);
    let sale_post = AccountPostState::new_claimed(
        sale_post,
        Claim::Pda(bonding_curve_core::compute_sale_pda_seed(
            token_definition_id,
            collateral_definition_id,
            creator.account_id,
            nonce,
        )),
    );

    // --- chained call: deposit D + R into the token vault --------------------
    // Dispatched on the DEFINITION's owner (pinned above), never on the
    // creator-supplied holding's - see the SECURITY note there.
    let token_program_id = token_definition.account.program_owner;
    let deposit = ChainedCall::new(
        token_program_id,
        vec![creator_token_holding.clone(), authorized(&token_vault)],
        &token_core::Instruction::Transfer {
            amount_to_transfer: total_deposit,
        },
    )
    .with_pda_seeds(vec![compute_token_vault_pda_seed(sale_id)]);

    // --- chained call: initialize the (empty) collateral vault ----------------
    // Claims + initializes the collateral-vault PDA as an empty Fungible holding
    // of the sale's collateral definition (mirrors LBP's pre-seed). This is what
    // lets the ATA buy path - `ata::Transfer`, which rejects a default recipient
    // - deposit into the vault on the very first buy.
    let collateral_token_program_id = collateral_definition.account.program_owner;
    let init_collateral_vault = ChainedCall::new(
        collateral_token_program_id,
        vec![collateral_definition.clone(), authorized(&collateral_vault)],
        &token_core::Instruction::InitializeAccount,
    )
    .with_pda_seeds(vec![compute_collateral_vault_pda_seed(sale_id)]);

    let post_states = vec![
        sale_post,
        AccountPostState::new(token_vault.account),
        AccountPostState::new(collateral_vault.account),
        // Echoed byte-for-byte: the treasury is read-only here, named only so the
        // pin above could type it.
        AccountPostState::new(treasury.account),
        AccountPostState::new(token_definition.account),
        AccountPostState::new(collateral_definition.account),
        AccountPostState::new(creator_token_holding.account),
        AccountPostState::new(creator.account),
        creator_index_post,
    ];

    (post_states, vec![deposit, init_collateral_vault])
}
