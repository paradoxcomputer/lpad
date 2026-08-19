//! Close and withdraw - end of the sale lifecycle.

use bonding_curve_core::{
    compute_collateral_vault_pda_seed, compute_token_vault_pda_seed, read_fungible, SaleState,
    SaleStatus,
};
use lee_core::{
    account::{AccountWithMetadata, Data},
    program::{AccountPostState, ChainedCall, ProgramId, DEFAULT_PROGRAM_ID},
};

use crate::util::authorized;

/// Recovery floor for a no-timeout sale (`end_timestamp_ms == 0`). After this
/// much wall-clock time has elapsed since creation, the creator may close a
/// stalled sale that never reached its supply target, so the raised collateral
/// and DEX-seed tokens are recoverable rather than locked forever. 7 days in ms.
const RECOVERY_FLOOR_MS: u64 = 7 * 24 * 60 * 60 * 1_000;

/// `CloseSale` - manual close by the creator. Permitted once the configured end
/// timestamp has passed, or if the supply target is already exhausted
/// (idempotent with the auto-close that fires on the final buy), or - for a
/// no-timeout sale (`end_timestamp_ms == 0`) - once the recovery floor has
/// elapsed since creation so a stalled sale's funds are recoverable.
///
/// Account order: `[sale, creator, clock]`. No `creator_index` slot, and that is
/// deliberate: closing does NOT remove the sale from the creator's
/// [`bonding_curve_core::CreatorIndex`]. A closed sale is still a sale that
/// creator created - it still has a state account, a withdrawable balance and a
/// history worth listing - so `my-sales` must keep showing it, with its status
/// read from the sale account as it always was. Removing entries would also make
/// the index a shrinking, rewritable list rather than an append-only one, which
/// is what keeps its size bounded and its writes conflict-free. Same for
/// `Withdraw`. Please do not "fix" this.
#[must_use]
pub fn close_sale(
    sale: AccountWithMetadata,
    creator: AccountWithMetadata,
    self_program_id: ProgramId,
    clock_ts: i64,
) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
    let mut state = SaleState::try_from(&sale.account.data).expect("invalid sale state account");
    assert_eq!(
        sale.account.program_owner, self_program_id,
        "sale account must be owned by the bonding-curve program"
    );
    assert!(creator.is_authorized, "creator must authorize close");
    assert_eq!(creator.account_id, state.creator, "only the creator may close");
    assert!(matches!(state.status, SaleStatus::Open), "sale already closed");

    let now = u64::try_from(clock_ts.max(0)).unwrap_or(0);
    let timed_out = state.end_timestamp_ms != 0 && now >= state.end_timestamp_ms;
    let supply_done = state.sale_reserve == 0;
    // Recovery path for a no-timeout sale that stalls below the supply target:
    // without this it could never close (and the creator could never withdraw),
    // locking the raised collateral. The floor (>= min_duration_ms) keeps an
    // active sale from being closed early.
    let recoverable = state.end_timestamp_ms == 0 && {
        let created = u64::try_from(state.created_ts_ms.max(0)).unwrap_or(0);
        let elapsed_floor = state.min_duration_ms.max(RECOVERY_FLOOR_MS);
        now >= created.saturating_add(elapsed_floor)
    };
    assert!(
        timed_out || supply_done || recoverable,
        "cannot close: supply target not reached and end timestamp not passed (or unset)"
    );

    state.status = SaleStatus::Closed;
    let mut sale_post = sale.account.clone();
    sale_post.data = Data::from(&state);

    (
        vec![
            AccountPostState::new(sale_post),
            AccountPostState::new(creator.account),
        ],
        vec![],
    )
}

/// `Withdraw` - creator withdraws the collateral that is genuinely theirs
/// (public-path fees were already taken per-swap; no extra deduction, RFP-015
/// Func §5) and the remaining project tokens in the vault (DEX seed `R` plus any
/// unsold sale reserve).
///
/// It pays out `collateral_vault_balance - treasury_owed` and does NOT touch
/// `treasury_owed`: the fee `BuyDisposable` escrowed stays in the vault and stays
/// on the books, which is what upholds
/// `collateral_vault_balance == real_collateral + treasury_owed` across a
/// withdrawal (see [`bonding_curve_core::SaleState::treasury_owed`]).
///
/// Settling that escrow is [`sweep_treasury`], a SEPARATE instruction, and not a
/// leg here. `CreateSale` now takes the treasury as an account and pins it to an
/// initialised Fungible holding of the collateral definition under that
/// definition's own token program, so the shapes that could never receive the fee
/// are no longer constructible - but the split stays, because it is what bounds
/// the loss if one ever were: as a leg of this instruction, a treasury that
/// reverts would revert the creator's payout with it and take the entire raise
/// plus every unsold token, forever, instead of stranding only the accrued fee
/// (see [`bonding_curve_core::SaleState::treasury_owed`]). Naming no treasury
/// account here also removes the `treasury_id == creator` collision: LEZ dedups
/// `message.account_ids` and rejects a duplicate before execution, so a treasury
/// that aliased a slot in this list would have bricked withdrawal outright.
///
/// Account order: `[sale, token_vault, collateral_vault,
/// creator_collateral_holding, creator_token_holding, creator]`.
#[must_use]
pub fn withdraw(
    sale: AccountWithMetadata,
    token_vault: AccountWithMetadata,
    collateral_vault: AccountWithMetadata,
    creator_collateral_holding: AccountWithMetadata,
    creator_token_holding: AccountWithMetadata,
    creator: AccountWithMetadata,
    self_program_id: ProgramId,
) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
    let mut state = SaleState::try_from(&sale.account.data).expect("invalid sale state account");
    assert_eq!(
        sale.account.program_owner, self_program_id,
        "sale account must be owned by the bonding-curve program"
    );
    assert!(creator.is_authorized, "creator must authorize withdrawal");
    assert_eq!(creator.account_id, state.creator, "only the creator may withdraw");
    assert!(matches!(state.status, SaleStatus::Closed), "sale must be closed before withdrawal");
    assert_eq!(token_vault.account_id, state.token_vault_id, "wrong token vault");
    assert_eq!(
        collateral_vault.account_id, state.collateral_vault_id,
        "wrong collateral vault"
    );

    // Withdraw the actual on-chain balances (robust against rounding dust).
    let (_, collateral_balance) =
        read_fungible(&collateral_vault, "Withdraw: collateral vault");
    let (_, token_balance) = read_fungible(&token_vault, "Withdraw: token vault");

    // The escrowed fees sit inside `collateral_balance` but are not the
    // creator's, so they are subtracted here and LEFT in the vault for
    // `sweep_treasury`. `saturating_sub` gives the treasury priority: if a
    // rounding shortfall ever broke the invariant, the creator takes the loss
    // (possibly down to nothing) rather than the escrow being dipped into or the
    // subtraction underflowing.
    let creator_collateral = collateral_balance.saturating_sub(state.treasury_owed);
    assert!(
        creator_collateral > 0 || token_balance > 0,
        "nothing to withdraw (already drained). Collateral still sitting in the vault is the \
         treasury's escrowed disposable-buy fee, not yours - hand it over with SweepTreasury."
    );

    let sale_id = sale.account_id;
    let token_program_id = collateral_vault.account.program_owner;
    let mut calls = Vec::with_capacity(2);

    // 1. the creator's collateral -> creator (escrow stays behind in the vault).
    if creator_collateral > 0 {
        calls.push(
            ChainedCall::new(
                token_program_id,
                vec![authorized(&collateral_vault), creator_collateral_holding.clone()],
                &token_core::Instruction::Transfer { amount_to_transfer: creator_collateral },
            )
            .with_pda_seeds(vec![compute_collateral_vault_pda_seed(sale_id)]),
        );
    }
    // 2. unsold project tokens + DEX seed reserve -> creator.
    if token_balance > 0 {
        // The project-token leg is owned by the token vault's program, which may
        // differ from the collateral vault's program if the two tokens live under
        // distinct token programs.
        let token_vault_program_id = token_vault.account.program_owner;
        calls.push(
            ChainedCall::new(
                token_vault_program_id,
                vec![authorized(&token_vault), creator_token_holding.clone()],
                &token_core::Instruction::Transfer { amount_to_transfer: token_balance },
            )
            .with_pda_seeds(vec![compute_token_vault_pda_seed(sale_id)]),
        );
    }

    // Zero the CREATOR's accounting buckets for a clean closed-sale state.
    // `treasury_owed` is deliberately left alone: the collateral behind it is
    // still in the vault, so zeroing it would forfeit the treasury's claim and
    // break `vault_balance == real_collateral + treasury_owed`. (A second
    // withdraw is actually prevented by the creator-share balance check above,
    // not by these fields - they are not consumed by that guard.)
    state.real_collateral = 0;
    state.dex_seed_reserve = 0;
    state.sale_reserve = 0;
    let mut sale_post = sale.account.clone();
    sale_post.data = Data::from(&state);

    let post_states = vec![
        AccountPostState::new(sale_post),
        AccountPostState::new(token_vault.account),
        AccountPostState::new(collateral_vault.account),
        AccountPostState::new(creator_collateral_holding.account),
        AccountPostState::new(creator_token_holding.account),
        AccountPostState::new(creator.account),
    ];
    (post_states, calls)
}

/// `SweepTreasury` - pay the protocol fees that `BuyDisposable` escrowed in the
/// collateral vault (`state.treasury_owed`) over to the sale's pinned treasury,
/// and clear the bucket by exactly what moved.
///
/// Account order: `[sale, collateral_vault, treasury]`.
///
/// PERMISSIONLESS, and that is the point: no signature is required because there
/// is nothing here for a submitter to choose. The payer is
/// `state.collateral_vault_id`, the recipient is `state.treasury_id`, the amount
/// is `state.treasury_owed`, and the transfer is dispatched on the vault's own
/// `program_owner` - so the most a stranger can accomplish is delivering the fee
/// to its rightful owner while paying the gas. A creator or admin signature would
/// buy no safety and would add a party able to withhold the treasury's money.
///
/// No signature at all, not even the treasury's. The bootstrap path that used to
/// accept one - so the fee leg could CLAIM a never-initialised treasury - is gone
/// with the state it existed for: `CreateSale` now pins an already-initialised
/// holding, so there is nothing left to bootstrap. The slot is still forwarded to
/// the fee leg exactly as it arrived, never forced, because marking an account the
/// signer set does not contain is `InvalidAccountAuthorization`. This wording is
/// deliberately identical in intent to `lbp::lifecycle::sweep_treasury`: the two
/// have silently drifted apart three times, each time behind a comment asserting
/// they had not.
///
/// Valid while the sale is Open as well as Closed: the escrow accrues during the
/// sale, settling it early moves only `treasury_owed` (the creator's
/// `real_collateral` is untouched), and the treasury should not have to wait on
/// the creator to close.
///
/// Kept out of `Withdraw` because this is the one leg that can fail for good -
/// see the note there and on [`bonding_curve_core::SaleState::treasury_owed`].
#[must_use]
pub fn sweep_treasury(
    sale: AccountWithMetadata,
    collateral_vault: AccountWithMetadata,
    treasury: AccountWithMetadata,
    self_program_id: ProgramId,
) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
    let mut state = SaleState::try_from(&sale.account.data).expect("invalid sale state account");
    assert_eq!(
        sale.account.program_owner, self_program_id,
        "sale account must be owned by the bonding-curve program"
    );
    assert_eq!(
        collateral_vault.account_id, state.collateral_vault_id,
        "wrong collateral vault"
    );
    assert_eq!(treasury.account_id, state.treasury_id, "wrong treasury account");
    // An uninitialised treasury has to be CREATED by the fee leg, and
    // `token::transfer` creates its recipient with `new_claimed_if_default(..,
    // Claim::Authorized)` - a claim LEZ validates against the authorization of the
    // account BEING CLAIMED, i.e. the treasury's own. Adding a signer slot to this
    // instruction would therefore buy nothing: no third party's signature can
    // authorise it. The treasury's can, so the treasury slot is forwarded to that
    // leg with whatever authorization it arrived with - forwarded, never forced,
    // because marking an account the signer set does not contain is
    // `InvalidAccountAuthorization` and would break every ordinary sweep. The
    // instruction stays permissionless, and needs no signature at all now that
    // `CreateSale` pins an initialised treasury - there is no bootstrap case left.
    //
    // Both cases are caught here so the operator gets an answer, instead of paying
    // for a proof that dies as `ClaimedUnauthorizedAccount` (or, worse, as
    // `NonDefaultAccountWithDefaultOwner`) naming no remedy.
    //
    // UNREACHABLE for any sale this build can create - and KEPT ANYWAY, on
    // purpose. `create_sale` requires the treasury to already be an initialised
    // Fungible holding of the collateral definition, and that state cannot
    // regress: an initialised holding is owned by the token program, and LEZ's
    // token program has no instruction that un-initialises one (`burn` only lowers
    // a balance). A sale created by an older ELF cannot arrive here either - a
    // different guest is a different program id, and the assert above pins
    // `sale.account.program_owner` to ours.
    //
    // It stays because it is free and one-sided: it can only reject a DEFAULT-
    // owned treasury, which `create_sale` has already made impossible, so it will
    // never fail a legitimate sweep, and the day the reasoning above stops holding
    // (a token program with a close/unwind instruction, a future creation path
    // that relaxes the pin) it is the difference between a named error and an
    // opaque framework rejection of an unsettleable escrow. The escrow is the one
    // thing in this program that can be lost for good; that is worth two asserts
    // on a cold instruction.
    // `CreateSale` pins the treasury as an initialised Fungible holding of the
    // collateral definition, so it is owned by a token program from the moment
    // the sale exists - and no LEZ instruction un-initialises an account (the
    // token program has only Transfer/NewDefinition/InitializeAccount/Burn/Mint/
    // PrintNft, and `DefaultAccountModifiedWithoutClaim` stops a DEFAULT-owned
    // account from carrying data at all). A DEFAULT-owned treasury here therefore
    // does not mean "not set up yet"; it means chain state disagrees with what
    // this sale pinned, which no retry and no signature can fix.
    //
    // This replaced a two-assert bootstrap path that tried to CLAIM such an
    // account when the treasury signed for itself. That remedy is unreachable now,
    // and its message told operators to do something that can no longer work.
    //
    // Mirrors `lbp::lifecycle::sweep_treasury` - and that claim is checked: both
    // programs reject here identically. The two have silently drifted apart twice
    // before, each time behind a comment asserting they had not.
    assert_ne!(
        treasury.account.program_owner, DEFAULT_PROGRAM_ID,
        "the sale's treasury account is unowned, which CreateSale rejects and no LEZ instruction \
         can produce afterwards: this sale's pinned treasury disagrees with chain state. \
         Retrying cannot help - report this."
    );
    assert!(
        state.treasury_owed > 0,
        "nothing owed to the treasury: no disposable-buy fee has accrued since the last sweep, \
         so there is nothing to settle"
    );

    // Pay what the vault can actually cover. The invariant says it holds at least
    // `treasury_owed`; if a rounding shortfall ever broke that, emit the leg for
    // the remainder instead of one that is guaranteed to revert.
    let (_, collateral_balance) =
        read_fungible(&collateral_vault, "SweepTreasury: collateral vault");
    let amount = state.treasury_owed.min(collateral_balance);
    assert!(
        amount > 0,
        "the collateral vault is empty but the sale still owes the treasury: the vault balance \
         and treasury_owed have diverged. Retrying cannot help - report this."
    );

    // Dispatched on the VAULT's program_owner, never on an id read off a
    // submitter-supplied account: this instruction is permissionless, so trusting
    // the treasury's owner would let anyone route the vault PDA's authority into a
    // program of their choosing. The vault is initialised by `create_sale`, so its
    // owner is the trustworthy anchor. Mirrors `buy::buy_transfers`.
    let calls = vec![
        ChainedCall::new(
            collateral_vault.account.program_owner,
            vec![authorized(&collateral_vault), treasury.clone()],
            &token_core::Instruction::Transfer { amount_to_transfer: amount },
        )
        .with_pda_seeds(vec![compute_collateral_vault_pda_seed(sale.account_id)]),
    ];

    // Clear only what actually moved: a clamped payout must leave the remainder
    // claimable by a later sweep rather than silently forfeiting it.
    state.treasury_owed -= amount;
    let mut sale_post = sale.account.clone();
    sale_post.data = Data::from(&state);

    let post_states = vec![
        AccountPostState::new(sale_post),
        AccountPostState::new(collateral_vault.account),
        AccountPostState::new(treasury.account),
    ];
    (post_states, calls)
}
