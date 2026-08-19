//! Poke, pause/resume, close, and withdraw for the LBP program.

use lbp_core::{
    close_fee, compute_collateral_vault_pda_seed, compute_token_vault_pda_seed,
    compute_weight_obs_pda, compute_weight_obs_pda_seed, read_fungible, PoolState, SaleStatus,
    WeightObs,
};
use lee_core::{
    account::{Account, AccountWithMetadata, Data},
    program::{AccountPostState, ChainedCall, Claim, ProgramId, DEFAULT_PROGRAM_ID},
};

use crate::util::authorized;

/// `Poke` - advance the stored weight for off-chain consumers. Pricing is lazy,
/// so this is purely a convenience; idempotent within a block.
/// Account order: `[pool, weight_obs, clock]`.
///
/// **The pool account is echoed byte-identical.** `Poke` is permissionless and
/// economically inert, but a `BuyDisposable` proof pins the pool account
/// byte-for-byte at proving time and the sequencer re-verifies it against live
/// state at inclusion. When this wrote the advanced weight into the pool, anyone
/// could invalidate every in-flight private buy for the price of one cheap
/// transaction that changed nothing. The weight now lives in its own per-pool
/// PDA (`compute_weight_obs_pda_seed`), which a private buy never declares.
///
/// That PDA is claimed lazily, the way the vault PDAs are: it does not exist
/// until the first poke, and `new_claimed_if_default` claims it exactly then.
#[must_use]
pub fn poke(
    pool: AccountWithMetadata,
    weight_obs: AccountWithMetadata,
    self_program_id: ProgramId,
    clock_ts: i64,
) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
    let state = PoolState::try_from(&pool.account.data).expect("bad pool state");
    assert_eq!(pool.account.program_owner, self_program_id, "pool not owned by LBP");
    // The submitter chooses non-signer account ids, so pin the observation slot to
    // this pool's own PDA - otherwise a poke could be aimed at another pool's
    // observation account and write a weight from the wrong schedule into it.
    assert_eq!(
        weight_obs.account_id,
        compute_weight_obs_pda(self_program_id, pool.account_id),
        "weight_obs: wrong PDA"
    );
    assert!(
        weight_obs.account == Account::default()
            || weight_obs.account.program_owner == self_program_id,
        "weight_obs not owned by LBP"
    );
    let t_ms = u64::try_from(clock_ts.max(0)).unwrap_or(0);
    let obs = WeightObs { w_token_q64: state.weight_token_q64(t_ms), ts_ms: t_ms };
    let mut obs_post = weight_obs.account.clone();
    obs_post.data = Data::from(&obs);
    (
        vec![
            // Echoed unmodified - see the note above; do not rebuild it from
            // `state`, a re-serialize is not guaranteed to be byte-identical.
            AccountPostState::new(pool.account),
            AccountPostState::new_claimed_if_default(
                obs_post,
                Claim::Pda(compute_weight_obs_pda_seed(pool.account_id)),
            ),
        ],
        vec![],
    )
}

/// `Pause` / `Resume` - emergency stop toggled by the creator. Weight
/// progression is unaffected (it is computed lazily from the clock).
/// Account order: `[pool, creator]`.
#[must_use]
pub fn set_paused(
    pool: AccountWithMetadata,
    creator: AccountWithMetadata,
    paused: bool,
    self_program_id: ProgramId,
) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
    let mut state = PoolState::try_from(&pool.account.data).expect("bad pool state");
    assert_eq!(pool.account.program_owner, self_program_id, "pool not owned by LBP");
    assert!(creator.is_authorized, "creator must authorize");
    assert_eq!(creator.account_id, state.creator, "not the pool creator");
    state.paused = paused;
    let mut pool_post = pool.account.clone();
    pool_post.data = Data::from(&state);
    (
        vec![AccountPostState::new(pool_post), AccountPostState::new(creator.account)],
        vec![],
    )
}

/// `CloseSale` - close after the end timestamp. Account order: `[pool, creator, clock]`.
#[must_use]
pub fn close_sale(
    pool: AccountWithMetadata,
    creator: AccountWithMetadata,
    self_program_id: ProgramId,
    clock_ts: i64,
) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
    let mut state = PoolState::try_from(&pool.account.data).expect("bad pool state");
    assert_eq!(pool.account.program_owner, self_program_id, "pool not owned by LBP");
    assert!(creator.is_authorized, "creator must authorize");
    assert_eq!(creator.account_id, state.creator, "not the pool creator");
    assert!(matches!(state.status, SaleStatus::Open), "already closed");
    let now = u64::try_from(clock_ts.max(0)).unwrap_or(0);
    assert!(now >= state.t_end_ms, "not yet t_end_ms");
    state.status = SaleStatus::Closed;
    let mut pool_post = pool.account.clone();
    pool_post.data = Data::from(&state);
    (
        vec![AccountPostState::new(pool_post), AccountPostState::new(creator.account)],
        vec![],
    )
}

/// `Withdraw` - creator withdraws the collateral that is genuinely theirs (their
/// own seed plus the raise, less the at-close protocol fee - RFP-016 Func §5)
/// and every unsold project token.
///
/// The fee is ESCROWED, not paid: it is credited to `state.treasury_owed` and
/// the collateral behind it is left in the vault, which is what upholds
/// `collateral_vault_balance == reserve_collateral + treasury_owed` across a
/// withdrawal (see [`lbp_core::PoolState::treasury_owed`]).
///
/// Settling that escrow is [`sweep_treasury`], a SEPARATE instruction, and not a
/// leg here. `CreateSale` now takes the treasury as an ACCOUNT and pins it to an
/// initialised `Fungible` holding of the collateral definition under that
/// definition's own token program, so a treasury that cannot receive is no
/// longer constructible - but the split stays, because it is what bounds the
/// blast radius if that pin is ever wrong. As a leg of THIS instruction, one
/// dead treasury reverts the creator's payout forever, and a closed pool has no
/// other drain: the entire raise AND every unsold token go with it. Settled on
/// its own, the same failure strands the fee and nothing else. Naming no
/// treasury account here also keeps the `treasury_id == creator` collision out:
/// LEZ dedups `message.account_ids` and rejects a duplicate before execution, so
/// a treasury that aliased a slot in this list would brick withdrawal outright.
///
/// Account order: `[pool, token_vault, collateral_vault,
/// creator_collateral_holding, creator_token_holding, creator]`.
#[must_use]
pub fn withdraw(
    pool: AccountWithMetadata,
    token_vault: AccountWithMetadata,
    collateral_vault: AccountWithMetadata,
    creator_collateral_holding: AccountWithMetadata,
    creator_token_holding: AccountWithMetadata,
    creator: AccountWithMetadata,
    self_program_id: ProgramId,
) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
    let mut state = PoolState::try_from(&pool.account.data).expect("bad pool state");
    assert_eq!(pool.account.program_owner, self_program_id, "pool not owned by LBP");
    assert!(creator.is_authorized, "creator must authorize");
    assert_eq!(creator.account_id, state.creator, "not the pool creator");
    assert!(matches!(state.status, SaleStatus::Closed), "sale must be closed first");
    assert_eq!(token_vault.account_id, state.token_vault_id, "wrong token vault");
    assert_eq!(collateral_vault.account_id, state.collateral_vault_id, "wrong collateral vault");

    // Withdraw the actual on-chain balances (robust against rounding dust).
    let (_, collateral_balance) = read_fungible(&collateral_vault, "collateral vault");
    let (_, token_balance) = read_fungible(&token_vault, "token vault");

    // Anything already escrowed sits inside `collateral_balance` but is not the
    // creator's, so it comes off the top and stays in the vault for
    // `sweep_treasury`. `saturating_sub` gives the treasury priority: if a
    // rounding shortfall ever broke the invariant, the creator takes the loss
    // (possibly down to nothing) rather than the escrow being dipped into or the
    // subtraction underflowing.
    let creator_gross = collateral_balance.saturating_sub(state.treasury_owed);
    // Tax only buyer-raised collateral, not the creator's own seed deposit.
    // `cum_collateral_in` tracks exactly the raised amount; `.min` guards edges.
    // Charging it against the CREATOR's share (rather than the raw balance) is
    // also what stops a second withdrawal from charging the fee twice: after the
    // first one that share is zero, so `close_fee` returns zero.
    let fee = close_fee(state.cum_collateral_in.min(creator_gross), state.fee_bps);
    let net = creator_gross - fee;
    // Already drained. Any collateral still sitting in the vault is the
    // treasury's escrowed at-close fee, not the creator's - it leaves via
    // `SweepTreasury`, not here.
    assert!(net > 0 || token_balance > 0, "nothing to withdraw");

    let pool_id = pool.account_id;
    let token_program_id = collateral_vault.account.program_owner;
    let mut calls = Vec::with_capacity(2);

    // 1. the creator's collateral -> creator (the fee stays behind in the vault).
    // Built against the FULL vault balance: no earlier leg moves it any more, so
    // there is nothing to shift.
    if net > 0 {
        calls.push(
            ChainedCall::new(
                token_program_id,
                vec![authorized(&collateral_vault), creator_collateral_holding.clone()],
                &token_core::Instruction::Transfer { amount_to_transfer: net },
            )
            .with_pda_seeds(vec![compute_collateral_vault_pda_seed(pool_id)]),
        );
    }
    // 2. unsold project tokens -> creator.
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
            .with_pda_seeds(vec![compute_token_vault_pda_seed(pool_id)]),
        );
    }

    // Escrow the fee instead of transferring it, and zero the CREATOR's buckets.
    // The collateral behind `treasury_owed` is still in the vault, so this is
    // exactly what keeps `vault_balance == reserve_collateral + treasury_owed`
    // true once `reserve_collateral` goes to zero. (A second withdraw is
    // prevented by the creator-share check above, not by these fields - they are
    // not consumed by that guard.)
    state.treasury_owed = state
        .treasury_owed
        .checked_add(fee)
        .expect("treasury_owed overflow");
    state.reserve_collateral = 0;
    state.reserve_token = 0;
    let mut pool_post = pool.account.clone();
    pool_post.data = Data::from(&state);

    let post_states = vec![
        AccountPostState::new(pool_post),
        AccountPostState::new(token_vault.account),
        AccountPostState::new(collateral_vault.account),
        AccountPostState::new(creator_collateral_holding.account),
        AccountPostState::new(creator_token_holding.account),
        AccountPostState::new(creator.account),
    ];
    (post_states, calls)
}

/// `SweepTreasury` - pay the at-close protocol fee that [`withdraw`] escrowed in
/// the collateral vault (`state.treasury_owed`) over to the pool's pinned
/// treasury, and clear the bucket by exactly what moved.
///
/// Account order: `[pool, collateral_vault, treasury]`.
///
/// PERMISSIONLESS, and that is the point: there is no signer slot because there
/// is nothing here for a submitter to choose. The payer is
/// `state.collateral_vault_id`, the recipient is `state.treasury_id`, the amount
/// is `state.treasury_owed`, and the transfer is dispatched on the vault's own
/// `program_owner` - so the most a stranger can accomplish is delivering the fee
/// to its rightful owner while paying the gas. A creator or admin signature would
/// buy no safety and would add a party able to withhold the treasury's money.
///
/// No signature at all, not even the treasury's. The bootstrap path that used to
/// accept one - so the fee leg could CLAIM a never-initialised treasury - is gone
/// with the state it existed for: `CreateSale` pins an already-initialised
/// holding, so there is nothing left to bootstrap. That path was also fragile in
/// a way no signature could repair: a claim needs the pre-state to be
/// `Account::default()` WHOLE, and LEZ bumps a signer's nonce even when its
/// account is unowned, so the treasury's OWN key bricked the id by signing
/// anything before the sweep. Never a stranger's grief - LEZ lets nobody modify a
/// DEFAULT-owned account they cannot claim - always the treasury's own. See the
/// assert below.
///
/// No status gate: the bucket is only ever credited at withdrawal, so a pool that
/// owes anything is already closed, and asserting it again would only add a way
/// for the check to disagree with the books.
///
/// Kept out of `Withdraw` because this is the one leg that can fail for good -
/// see the note there and on [`lbp_core::PoolState::treasury_owed`].
#[must_use]
pub fn sweep_treasury(
    pool: AccountWithMetadata,
    collateral_vault: AccountWithMetadata,
    treasury: AccountWithMetadata,
    self_program_id: ProgramId,
) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
    let mut state = PoolState::try_from(&pool.account.data).expect("bad pool state");
    assert_eq!(pool.account.program_owner, self_program_id, "pool not owned by LBP");
    assert_eq!(collateral_vault.account_id, state.collateral_vault_id, "wrong collateral vault");
    assert_eq!(treasury.account_id, state.treasury_id, "wrong treasury account");
    // UNREACHABLE by construction, and asserted anyway because the cost of being
    // wrong here is a permanently unsettleable escrow.
    //
    // `create_sale` takes the treasury as an account and requires it to ALREADY
    // be an initialised `Fungible` holding of the pool's collateral definition.
    // An initialised holding is owned by the token program, and LEZ has no
    // instruction that un-initialises one (`burn` only reduces the balance), so a
    // treasury that passed creation cannot regress to DEFAULT-owned for the
    // pool's whole life.
    //
    // What stood here was a bootstrap path that let the treasury's own signature
    // CLAIM an uninitialised treasury, and it carried a hazard that signature
    // could not clear: a claim needs the pre-state to be `Account::default()`
    // WHOLE, and `State::apply_state_diff` bumps a signer's nonce even when its
    // account is DEFAULT-owned, so the treasury's own key killed the only
    // instruction that can ever pay this escrow out - forever - by signing
    // anything at all first. A stranger could NOT: LEZ rejects any transaction
    // that leaves a DEFAULT-owned account modified without a claim, balance
    // included, so there is no dust to send and no third party in this story.
    // Pinning an initialised holding at creation removes the whole state instead
    // of documenting it. The sweep needs no signature at all now, and stays
    // permissionless.
    //
    // If it ever does fire, this pool's pinned treasury disagrees with chain
    // state: retrying cannot help, and it wants reporting rather than a retry.
    //
    // Mirrors `bonding_curve::lifecycle::sweep_treasury` - and that claim is
    // checked: both programs reject here identically. The two have silently
    // drifted apart twice before, each time behind a comment asserting they
    // had not.
    assert_ne!(
        treasury.account.program_owner, DEFAULT_PROGRAM_ID,
        "pool treasury is unowned"
    );
    // No at-close fee has accrued since the last sweep. The fee is escrowed by
    // `Withdraw`, so the pool has to be closed and withdrawn from first.
    assert!(state.treasury_owed > 0, "nothing owed to the treasury");

    // Pay what the vault can actually cover. The invariant says it holds at least
    // `treasury_owed`; if a rounding shortfall ever broke that, emit the leg for
    // the remainder instead of one that is guaranteed to revert.
    let (_, collateral_balance) = read_fungible(&collateral_vault, "collateral vault");
    let amount = state.treasury_owed.min(collateral_balance);
    // The vault balance and `treasury_owed` have diverged, which the invariant
    // says cannot happen. Retrying cannot help - this wants reporting.
    assert!(amount > 0, "vault empty but treasury_owed > 0");

    // Dispatched on the VAULT's program_owner, never on an id read off a
    // submitter-supplied account: this instruction is permissionless, so trusting
    // the treasury's owner would let anyone route the vault PDA's authority into a
    // program of their choosing. The vault is claimed and typed by `create_sale`'s
    // collateral-seed leg, so its owner is the trustworthy anchor. Mirrors
    // `buy::buy_transfers`.
    let calls = vec![
        ChainedCall::new(
            collateral_vault.account.program_owner,
            vec![authorized(&collateral_vault), treasury.clone()],
            &token_core::Instruction::Transfer { amount_to_transfer: amount },
        )
        .with_pda_seeds(vec![compute_collateral_vault_pda_seed(pool.account_id)]),
    ];

    // Clear only what actually moved: a clamped payout must leave the remainder
    // claimable by a later sweep rather than silently forfeiting it.
    state.treasury_owed -= amount;
    let mut pool_post = pool.account.clone();
    pool_post.data = Data::from(&state);

    let post_states = vec![
        AccountPostState::new(pool_post),
        AccountPostState::new(collateral_vault.account),
        AccountPostState::new(treasury.account),
    ];
    (post_states, calls)
}
