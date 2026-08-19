//! `CreateSale` - open a new LBP sale.
//!
//! Account order: `[pool, token_vault, collateral_vault, token_definition,
//! collateral_definition, creator_token_holding, creator_collateral_holding,
//! creator, treasury, creator_index, clock]`.
//! - `pool`                  - uninitialized PDA, claimed by this program.
//! - `token_vault`           - uninitialized PDA; receives the project-token deposit.
//! - `collateral_vault`      - uninitialized PDA; receives the collateral seed.
//! - `token_definition`      - the project token's definition, pinned by
//!   `account_id == token_definition_id` (which is read out of the creator's
//!   holding). The project-token deposit leg is dispatched to the program that
//!   owns THIS account, and that leg is what claims and types the token vault -
//!   so a creator cannot name a real token in the pool while a program they
//!   deployed owns the vault handing tokens out.
//! - `collateral_definition` - the sale's collateral token definition, pinned by
//!   `account_id == collateral_definition_id`. The collateral seed leg is
//!   dispatched to the program that owns THIS account, and that leg is what
//!   claims and types the collateral vault - so the vault's owner is fixed by
//!   real chain state instead of by a creator-supplied holding. Mirrors
//!   `bonding_curve::create_sale`, which pins its vault-init leg the same way.
//! - `creator_token_holding`/`creator_collateral_holding` - creator's holdings (signer).
//! - `creator`               - creator identity (signer); stored for auth.
//! - `treasury`              - the sale's fee sink, echoed unchanged. Read-only
//!   here, and required to ALREADY be an initialised `Fungible` holding of the
//!   collateral definition under that definition's own token program - see the
//!   SECURITY note in the body for why nothing weaker survives.
//! - `creator_index`         - the creator's [`lbp_core::CreatorIndex`] PDA:
//!   claimed here on their first pool, appended to on every later one. Derived
//!   from the creator and from nothing else, which is what lets discovery read
//!   it once per creator instead of deriving and probing thousands of
//!   candidates. Sits directly before the clock, as it does in the bonding
//!   curve.
//! - `clock`                 - read-only CLOCK_01, echoed unchanged.

use lbp_core::{
    compute_collateral_vault_pda, compute_collateral_vault_pda_seed, compute_creator_index_pda,
    compute_creator_index_pda_seed, compute_pool_pda, compute_pool_pda_seed,
    compute_token_vault_pda, compute_token_vault_pda_seed, fixed::ONE, read_fungible,
    spot_price_q64, CreatorIndex, PoolState, SaleStatus, MAX_DURATION_MS, MAX_FEE_BPS,
    MAX_METADATA_LEN, MAX_RESERVE,
};
use lee_core::{
    account::{Account, AccountId, AccountWithMetadata, Data},
    program::{AccountPostState, ChainedCall, Claim, ProgramId, DEFAULT_PROGRAM_ID},
};

use crate::util::authorized;

#[expect(clippy::too_many_arguments, reason = "fixed protocol account/param list")]
#[must_use]
pub fn create_sale(
    pool: AccountWithMetadata,
    token_vault: AccountWithMetadata,
    collateral_vault: AccountWithMetadata,
    token_definition: AccountWithMetadata,
    collateral_definition: AccountWithMetadata,
    creator_token_holding: AccountWithMetadata,
    creator_collateral_holding: AccountWithMetadata,
    creator: AccountWithMetadata,
    treasury: AccountWithMetadata,
    creator_index: AccountWithMetadata,
    collateral_definition_id: AccountId,
    treasury_id: AccountId,
    token_name: String,
    token_symbol: String,
    token_deposit: u128,
    collateral_seed: u128,
    w_start_q64: u128,
    w_end_q64: u128,
    t_start_ms: u64,
    t_end_ms: u64,
    fee_bps: u128,
    block_token_ceiling: u128,
    allowlist_root: [u8; 32],
    fixed_price: bool,
    min_duration_ms: u64,
    nonce: u64,
    ata_program_id: ProgramId,
    self_program_id: ProgramId,
    clock_ts: i64,
) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
    // --- validation -----------------------------------------------------------
    assert!(fee_bps <= MAX_FEE_BPS, "fee_bps > MAX_FEE_BPS");
    assert!(token_deposit > 0, "token_deposit must be > 0");
    // Positive so the pool opens with a real collateral side to price against:
    // a zero seed leaves the spot price undefined and every buy reverting.
    assert!(collateral_seed > 0, "collateral_seed must be > 0");
    // The Balancer price math left-shifts both reserves by 64 (Q64.64), so any
    // reserve >= 2^64 overflows the u128 domain. The fixed-price branch catches
    // collateral_seed via spot_price_q64 -> div_to_q64, but a non-fixed pool never
    // touches that path at creation, so a >= 2^64 seed/deposit would create a pool
    // whose every buy reverts in buy_tokens_out (deposit-accepting, un-buyable).
    // Reject it here to honor MAX_RESERVE's "creates that would exceed it revert"
    // invariant. Mirrors bonding_curve::create_sale's MAX_VIRT_COLLATERAL bound.
    assert!(
        collateral_seed < MAX_RESERVE && token_deposit < MAX_RESERVE,
        "reserves must be < 2^64"
    );
    assert!(creator.is_authorized, "creator must authorize");
    // Bound the self-describing metadata so the serialized pool state stays under
    // Data's 100 KiB cap even once the 64-entry observation ring fills; an
    // unbounded name/symbol could otherwise grow the state across that cap
    // mid-life, panicking the PoolState->Data encode on every later buy/sell/close
    // and permanently locking deposited collateral. Mirrors bonding_curve.
    assert!(
        token_name.len() <= MAX_METADATA_LEN && token_symbol.len() <= MAX_METADATA_LEN,
        "name/symbol too long"
    );
    // SECURITY (mirrors bonding_curve::create_sale): a default/zero ata_program_id
    // can never name a real ATA program, so BuyAta would dispatch the buyer's
    // collateral leg to a no-op that skips the real token::Transfer. Reject it
    // on-chain; the SDK additionally refuses a non-canonical pin participant-side.
    assert!(
        ata_program_id != DEFAULT_PROGRAM_ID,
        "ata_program_id must not be default"
    );
    assert!(w_start_q64 > 0 && w_start_q64 < ONE, "w_start must be in (0,1)");
    assert!(w_end_q64 > 0 && w_end_q64 < ONE, "w_end must be in (0,1)");
    assert!(t_end_ms > t_start_ms, "t_end must be after t_start");
    // `min_duration_ms` is the creator's own declared floor on the sale's length.
    // RFP-016 ties it to privacy fairness: a very short window makes a private
    // buy's timestamp validity range narrow enough to be a fingerprint.
    assert!(
        (t_end_ms - t_start_ms) >= min_duration_ms,
        "duration < min_duration_ms"
    );
    // Bound the interpolation domain so weight_token_q64's `delta * elapsed`
    // (delta up to ~2^64, elapsed up to the span) can never approach i128::MAX
    // and wrap under release overflow-checks. ~10 years is far beyond any real
    // sale; an unbounded creator-set span would otherwise make buys revert in
    // the overflowing window (self-inflicted DoS) and break the in-range weight
    // invariant. Mirrors MAX_RESERVE's "creates that would exceed it revert".
    assert!(
        (t_end_ms - t_start_ms) <= MAX_DURATION_MS,
        "duration > MAX_DURATION_MS"
    );
    if !fixed_price {
        // A flat sale is expressed with `fixed_price`, not with w_start == w_end:
        // an LBP's whole point is the declining token weight, and the private
        // path's pricing safety (see `buy::buy_disposable`) depends on it.
        assert!(w_start_q64 > w_end_q64, "w_start must exceed w_end");
    }

    let (token_definition_id, creator_token_balance) =
        read_fungible(&creator_token_holding, "creator token");
    let (collateral_def, creator_collateral_balance) =
        read_fungible(&creator_collateral_holding, "creator collateral");
    assert_eq!(
        collateral_def, collateral_definition_id,
        "creator collateral: wrong definition"
    );
    assert!(creator_token_balance >= token_deposit, "insufficient tokens");
    assert!(creator_collateral_balance >= collateral_seed, "insufficient collateral");
    assert!(
        token_definition_id != collateral_definition_id,
        "token and collateral must differ"
    );

    // --- PDA checks -----------------------------------------------------------
    let pool_id = compute_pool_pda(
        self_program_id,
        token_definition_id,
        collateral_definition_id,
        creator.account_id,
        nonce,
    );
    assert_eq!(pool.account_id, pool_id, "pool id != PDA");
    assert_eq!(pool.account, Account::default(), "pool must be uninit");
    let token_vault_id = compute_token_vault_pda(self_program_id, pool_id);
    let collateral_vault_id = compute_collateral_vault_pda(self_program_id, pool_id);
    assert_eq!(token_vault.account_id, token_vault_id, "wrong token vault");
    assert_eq!(collateral_vault.account_id, collateral_vault_id, "wrong collateral vault");
    // Every vault PDA is claimed by a chained call's `new_claimed_if_default`, so
    // each needs the same precondition as the sale/pool account itself. Without
    // the assert the failure is an opaque framework rejection: the top-level
    // post-state echoes the pre-state, so a DEFAULT-owned non-default vault trips
    // validate_execution rule 7 (`NonDefaultAccountWithDefaultOwner`), naming the
    // PDA and nothing about why.
    //
    // Defence in depth rather than a live hazard, and worth being exact about,
    // because the same question decides the creator index below. A vault PDA is
    // keyless and derived from THIS program's id, so by the LEZ rules set out
    // there nobody but this program can put a byte, a balance or a nonce at one.
    // The only way to reach this assert with a non-default vault is to re-use a
    // (creator, nonce) pair whose pool already exists - and the pool assert above
    // catches that first. If it ever does fire, a bumped `nonce` is still the
    // remedy, and the operator should not have to read the LEZ state machine to
    // find that out.
    assert_eq!(
        token_vault.account,
        Account::default(),
        "token vault must be uninitialized - if this PDA already exists, re-create the pool \
         with a different nonce"
    );
    assert_eq!(
        collateral_vault.account,
        Account::default(),
        "collateral vault must be uninitialized - if this PDA already exists, re-create the \
         pool with a different nonce"
    );
    // SECURITY: pin the project-token definition account to the definition id the
    // creator's holding declares. `token_definition_id` is read out of that
    // holding's DATA, which a program the creator deployed may write freely, and
    // the deposit leg below is what CLAIMS the token vault - so dispatching that
    // leg off the holding let a creator publish a real, valuable
    // `token_definition_id` in the pool while the vault handing tokens out was
    // owned by their own program. A buyer whose token holding already exists is
    // safe (rule 6 refuses a foreign program's write to it), but one whose
    // holding is UNINITIALIZED has it claimed by that program and filled with a
    // holding that reads as the valuable token and is not - real collateral paid
    // for nothing. `account_id` is the one field of this account a submitter
    // cannot forge: it names real chain state. Same fix, same reasoning as the
    // collateral side below.
    assert_eq!(
        token_definition.account_id, token_definition_id,
        "token_definition: wrong definition id"
    );
    // ...and it must really be a FUNGIBLE token definition: the deposit leg is a
    // `Transfer`, which never parses this account, so nothing else here would
    // notice. Without it an uninitialized account (DEFAULT `program_owner`)
    // would surface only as LEZ's opaque "Unknown program" rejection of the
    // chained call. Mirrors the collateral definition check below.
    //
    // Remedy when it fires: pass the project token's DEFINITION account, not a
    // holding of it - and note that an NFT collection cannot be sold here.
    //
    // `Fungible` and not merely `is_ok()`: a pool prices a divisible reserve, and
    // an NFT definition names holdings this program cannot move. The creator's
    // holding is already forced Fungible by `read_fungible` above, so an NFT
    // definition here would only ever describe a pool whose own state contradicts
    // itself - and every real Fungible holding names a Fungible definition, so
    // this rejects nothing a working sale could have used.
    assert!(
        matches!(
            token_core::TokenDefinition::try_from(&token_definition.account.data),
            Ok(token_core::TokenDefinition::Fungible { .. })
        ),
        "token_definition: not Fungible"
    );
    // SECURITY: pin the definition account to the declared collateral definition
    // id. This account is what the collateral seed leg is dispatched to (see
    // below), and `account_id` is the one field of it a submitter cannot forge -
    // it names real chain state. Mirrors `bonding_curve::create_sale`.
    assert_eq!(
        collateral_definition.account_id, collateral_definition_id,
        "collateral_definition: wrong id"
    );
    // ...and it must really be a FUNGIBLE token definition. Nothing else here
    // would notice if it were not: the account is read only for its
    // `program_owner`. The bonding curve gets the parse for free - its vault-init
    // leg is a chained `token::InitializeAccount`, which parses the definition -
    // but the LBP seeds its vault with a `Transfer` that never touches the
    // definition account. Without this, an uninitialized account (whose
    // `program_owner` is the DEFAULT program) would surface only as LEZ's opaque
    // "Unknown program" rejection of the chained call.
    //
    // Remedy when it fires: pass the collateral token's DEFINITION account, not
    // a holding of it - and note that an NFT collection cannot be collateral.
    //
    // `Fungible` and not merely `is_ok()`: collateral is paid in divisible
    // amounts by every buy and swept in divisible amounts by the fee leg. The
    // treasury check just below is what makes NFT collateral unconstructible
    // outright - a `TokenHolding::Fungible` can never name a NonFungible
    // definition - and this assert says the same thing where the definition is
    // read, so the two cannot drift.
    assert!(
        matches!(
            token_core::TokenDefinition::try_from(&collateral_definition.account.data),
            Ok(token_core::TokenDefinition::Fungible { .. })
        ),
        "collateral_definition: not Fungible"
    );
    // The treasury must be a standalone fee sink: not one of the protocol's own
    // accounts, and not the creator's identity account.
    //
    // Vaults - sweeping the at-close fee into one would corrupt the pool's own
    // accounting (and the token vault holds the wrong definition entirely). Pool
    // PDA - a fee transfer would write token-holding bytes over the live pool
    // state.
    //
    // Beyond that, an alias is not merely odd, it is unusable, in two ways.
    // (1) LEZ dedups `message.account_ids` into a HashSet and rejects the message
    // with `InvalidInput("Duplicate account_ids")` BEFORE executing anything, so
    // any instruction that names the treasury alongside the account it aliases can
    // never run at all. `SweepTreasury` is `[pool, collateral_vault, treasury]`
    // and both of those ids are pinned from the pool state, so there is no
    // alternative account list to fall back on: the escrowed fee would be stranded
    // for good.
    // (2) The creator slot is an identity signer, not a Fungible holding of the
    // collateral definition, so the sweep's transfer would revert against it.
    // "Just send the fees to me" is the natural thing for a creator to type, and
    // creation is where it gets caught.
    //
    // The treasury is an account in this list now, so LEZ's own dedup would
    // reject most of these aliases before this program ever runs - but as a bare
    // `Duplicate account_ids` naming neither slot. The assert stays because it is
    // what tells the creator WHICH id to change.
    //
    // Remedy when it fires: pass a separate Fungible holding of the collateral
    // definition, distinct from the pool PDA, both vaults, and the creator.
    //
    // The creator's two HOLDINGS are deliberately not on this list: they are
    // chosen at withdrawal time, and `Withdraw` no longer names the treasury at
    // all, so an alias with one of them costs nothing.
    assert!(
        treasury_id != collateral_vault_id
            && treasury_id != token_vault_id
            && treasury_id != pool_id
            && treasury_id != creator.account_id,
        "treasury must not alias the pool account, its token or collateral vault, or the \
         creator - pass a separate Fungible holding of the collateral definition"
    );
    // SECURITY: the treasury arrives as an ACCOUNT, and it must ALREADY be an
    // initialised `Fungible` holding of this pool's collateral definition.
    //
    // `SweepTreasury` is the only instruction that can ever pay the escrowed
    // at-close fee out, and against a treasury that cannot receive it fails
    // PERMANENTLY - nothing rewrites `treasury_id`, so the collateral behind the
    // escrow just sits in the vault for good. Creation is the last moment the
    // creator can still fix that for free, so every unreceivable shape is
    // rejected here.
    //
    // Demanding an INITIALISED holding, rather than merely a plausible id, is
    // what makes the guarantee permanent instead of probable. The alternative -
    // letting the fee leg CLAIM an uninitialised treasury - fails two ways, and
    // NEITHER of them is a stranger's doing:
    //   * `token::transfer` claims its recipient with `Claim::Authorized`, which
    //     LEZ admits only when the account being claimed is itself authorized.
    //     `SweepTreasury` is permissionless and has no signer slot at all, so
    //     nothing in this program could ever supply that.
    //   * Even a sweep that did take the treasury's signature would be one
    //     signature away from dead: a claim needs the pre-state to be
    //     `Account::default()` WHOLE, and `State::apply_state_diff` bumps every
    //     signer's nonce whether or not its account is owned, so the treasury's
    //     key signing ANY unrelated transaction leaves an id no program may ever
    //     claim.
    // A THIRD PARTY cannot reach either state. LEZ rejects any transaction that
    // leaves a DEFAULT-owned account modified but unclaimed
    // (`DefaultAccountModifiedWithoutClaim`), and that covers balance as well as
    // data, so there is no dust to send; a stranger holds neither the treasury's
    // signature nor a program whose PDA derivation is that id. The hazard is
    // self-inflicted, permanent, and creation is the last place it is free to
    // refuse.
    //
    // An initialised holding has neither problem, and cannot regress either: it
    // is owned by the token program, and LEZ has no instruction that
    // un-initialises a holding (`burn` only reduces the balance). So the
    // DEFAULT-owned branch in `lifecycle::sweep_treasury` is unreachable rather
    // than merely unlikely.
    assert_eq!(
        treasury.account_id, treasury_id,
        "the treasury account passed does not match the treasury_id declared in the instruction \
         - pass the account for the id being pinned into the pool"
    );
    let treasury_definition_id = match token_core::TokenHolding::try_from(&treasury.account.data) {
        Ok(token_core::TokenHolding::Fungible { definition_id, .. }) => definition_id,
        // One arm for three shapes because the remedy is the same for all of
        // them: an unparseable account, an uninitialised one, and an NFT holding
        // are equally unable to receive the fee, and the escrow would be
        // unsweepable forever.
        _ => panic!(
            "the pool's treasury is not an initialised Fungible token holding - initialise it \
             as a Fungible holding of this pool's collateral definition \
             (token::InitializeAccount) and re-run CreateSale. It has to exist now: nothing \
             weaker can ever receive the at-close fee. SweepTreasury is permissionless and \
             cannot claim an account on your behalf, and a treasury's own key locks the id out \
             of that fee for good the moment it signs anything else"
        ),
    };
    assert_eq!(
        treasury_definition_id, collateral_definition_id,
        "the pool's treasury holds a different token than the pool's collateral - pass a \
         Fungible holding of the collateral definition, which is the only token this pool will \
         ever pay it"
    );
    // ...and under the collateral definition's OWN token program. The sweep's fee
    // leg is dispatched on the collateral VAULT's `program_owner`, and the vault
    // is claimed and typed by the seed leg below, which is dispatched on this
    // definition - so the two are the same program by construction. LEZ refuses a
    // foreign program's write to an account it does not own (`validate_execution`
    // rule 6), so a same-definition holding under some other program reverts
    // every sweep just as permanently as the wrong definition would.
    assert_eq!(
        treasury.account.program_owner, collateral_definition.account.program_owner,
        "the pool's treasury is not owned by the collateral definition's own token program - \
         the at-close fee leg is dispatched on the collateral vault's program, so a holding \
         under any other token program can never receive it. Pass a treasury minted from the \
         collateral definition itself"
    );

    // --- build state ----------------------------------------------------------
    let fixed_price_q64 = if fixed_price {
        let price = spot_price_q64(token_deposit, collateral_seed, w_start_q64);
        // spot_price_q64 floors to 0 when collateral_seed is tiny relative to
        // token_deposit (below Q64.64 resolution); the collateral_seed > 0 check
        // above guards the raw amount, not the resulting price. A zero price would
        // make every buy revert (fixed_price_tokens_out asserts price_q64 > 0),
        // leaving a deposit-accepting but permanently un-buyable pool.
        // Remedy: raise `collateral_seed` or lower `token_deposit`.
        assert!(price > 0, "fixed price rounds to zero");
        price
    } else {
        0
    };
    let state = PoolState {
        creator: creator.account_id,
        token_definition_id,
        collateral_definition_id,
        token_vault_id,
        collateral_vault_id,
        treasury_id,
        ata_program_id, // pinned here; BuyAta rejects any other ATA program
        fee_bps,
        w_start_q64,
        w_end_q64,
        t_start_ms,
        t_end_ms,
        min_duration_ms,
        reserve_token: token_deposit,
        reserve_collateral: collateral_seed,
        // Nothing is owed until `Withdraw` escrows the at-close fee.
        treasury_owed: 0,
        // No stored weight here: it lives in the per-pool `WeightObs` PDA, which
        // `Poke` claims lazily on first use (see `lifecycle::poke` for why).
        paused: false,
        block_token_ceiling,
        block_sold: 0,
        block_window_id: 0,
        allowlist_root,
        fixed_price,
        fixed_price_q64,
        nonce,
        created_ts_ms: clock_ts,
        status: SaleStatus::Open,
        cum_collateral_in: 0,
        cum_tokens_out: 0,
        buy_count: 0,
        obs: Vec::new(),
        token_name,
        token_symbol,
    };

    let mut pool_post = pool.account.clone();
    pool_post.data = Data::from(&state);
    let pool_post = AccountPostState::new_claimed(
        pool_post,
        Claim::Pda(compute_pool_pda_seed(
            token_definition_id,
            collateral_definition_id,
            creator.account_id,
            nonce,
        )),
    );

    // --- creator index --------------------------------------------------------
    // The only on-chain record of what a creator has created, and the reason
    // `my-pools` is one read per creator instead of thousands of derived probes
    // (see `lbp_core::CreatorIndex`). Structure, reasoning and revert messages
    // mirror `bonding_curve::create_sale`: these two blocks check the same things
    // about the same shape of account, so they say the same things about them.
    //
    // No same-block race to design around: `ValidatedStateDiff::
    // from_public_transaction` builds every pre-state from `state
    // .get_account_by_id` and applies each transaction's diff before the next one
    // runs, so a second CreateSale from the same creator in the same block reads
    // the index the first one just wrote and takes the append branch. (A
    // proof-pinned path could NOT do that - which is why `BuyDisposable` pins no
    // account whose data moves. CreateSale has no private form.)
    //
    // Pinned to the PDA of (THIS program, the DECLARED creator): the submitter
    // picks non-signer account ids, so without the pin a create could append this
    // pool into somebody else's list. Mirrors the `weight_obs` pin in
    // `lifecycle::poke`.
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
        let mut index = CreatorIndex::new(creator.account_id);
        index.push(pool_id);
        index_account.data = Data::from(&index);
        AccountPostState::new_claimed(
            index_account,
            Claim::Pda(compute_creator_index_pda_seed(creator.account_id)),
        )
    } else {
        // The append path: an index this program already owns, holding the pools
        // this creator opened before.
        //
        // Any OTHER non-default shape here is unreachable on a live chain, for
        // exactly the reasons set out above - which is also why there is no
        // "bump the nonce" escape hatch to offer and, emphatically, why changing
        // creator account is not a remedy. Kept as defence in depth against a
        // future seed change, a derivation collision, or a caller aiming this
        // slot somewhere else; if it ever fires, the state it names should not
        // exist, so the message says that rather than sending the operator off
        // to abandon a working identity.
        assert_eq!(
            index_account.program_owner, self_program_id,
            "creator index account exists but is not owned by this program - unreachable on a \
             live chain, since only this program can claim this PDA and LEZ refuses to modify a \
             DEFAULT-owned account without a claim. Report it; a different creator account is \
             not a fix, it is an abandoned identity"
        );
        let mut index = CreatorIndex::try_from(&index_account.data)
            .expect("creator index account does not hold an LBP creator index");
        // Unreachable while the PDA pin above holds - kept so that a future seed
        // change, or a collision, merges nobody's pool list into anybody else's.
        assert_eq!(
            index.creator, creator.account_id,
            "creator index belongs to a different creator"
        );
        index.push(pool_id);
        index_account.data = Data::from(&index);
        AccountPostState::new(index_account)
    };

    // Derived from the pinned DEFINITION, never from the creator's holding - see
    // the assert above for what reading it off the holding bought an attacker.
    let token_program_id = token_definition.account.program_owner;
    // Consequence of the pin, exactly as on the collateral leg: the holding being
    // debited must be owned by that same program, which every real holding of
    // this definition is. Without this the mismatch still fails, but as an opaque
    // `UnauthorizedDataModification` (rule 6) naming no cause.
    assert_eq!(
        creator_token_holding.account.program_owner, token_program_id,
        "creator token: wrong program"
    );
    let deposit_tokens = ChainedCall::new(
        token_program_id,
        vec![creator_token_holding.clone(), authorized(&token_vault)],
        &token_core::Instruction::Transfer { amount_to_transfer: token_deposit },
    )
    .with_pda_seeds(vec![compute_token_vault_pda_seed(pool_id)]);
    // SECURITY: derive the collateral leg's program from the pinned DEFINITION
    // account, never from `creator_collateral_holding` (mirrors
    // `bonding_curve::create_sale`, which dispatches its vault-init leg the same
    // way). This leg is what claims the collateral vault PDA -
    // `token::transfer`'s `new_claimed_if_default` - so whichever program
    // executes it becomes the vault's owner for the pool's whole life. Reading
    // that program off the creator's holding made the vault's owner entirely
    // creator-determined: LEZ deployment is permissionless, so a creator could
    // point `collateral_definition_id` at a real, valuable token while the vault
    // was owned by a program they deployed. Buyers are safe either way (buy's
    // `finalize_buy` binds the payer to the vault's program, so every honest
    // holder of the real token is rejected and such a pool is merely unbuyable),
    // but `pool.collateral_definition_id` would stop being evidence of what the
    // pool actually accepts - which the SDK and every indexer read it as.
    let collateral_token_program_id = collateral_definition.account.program_owner;
    // Consequence of the pin: the holding being debited must be owned by that
    // same program, which every real holding of this definition is (the token
    // program claims holdings when it initializes them, ATAs included). Without
    // this the mismatch still fails - `validate_execution` rule 6 refuses a data
    // change to an account the executing program does not own - but as an opaque
    // `UnauthorizedDataModification` naming no cause. Mirrors the payer/vault
    // bind in `buy::finalize_buy`.
    assert_eq!(
        creator_collateral_holding.account.program_owner, collateral_token_program_id,
        "creator collateral: wrong program"
    );
    let deposit_collateral = ChainedCall::new(
        collateral_token_program_id,
        vec![creator_collateral_holding.clone(), authorized(&collateral_vault)],
        &token_core::Instruction::Transfer { amount_to_transfer: collateral_seed },
    )
    .with_pda_seeds(vec![compute_collateral_vault_pda_seed(pool_id)]);

    let post_states = vec![
        pool_post,
        AccountPostState::new(token_vault.account),
        AccountPostState::new(collateral_vault.account),
        AccountPostState::new(token_definition.account),
        AccountPostState::new(collateral_definition.account),
        AccountPostState::new(creator_token_holding.account),
        AccountPostState::new(creator_collateral_holding.account),
        AccountPostState::new(creator.account),
        // Read-only: this instruction validates the treasury, it never touches it.
        AccountPostState::new(treasury.account),
        creator_index_post,
    ];
    (post_states, vec![deposit_tokens, deposit_collateral])
}
