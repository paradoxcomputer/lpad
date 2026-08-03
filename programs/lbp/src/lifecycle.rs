//! Poke, pause/resume, close, and withdraw for the LBP program.

use lbp_core::{
    close_fee, compute_collateral_vault_pda_seed, compute_token_vault_pda_seed, read_fungible,
    PoolState, SaleStatus,
};
use lee_core::{
    account::{AccountWithMetadata, Data},
    program::{AccountPostState, ChainedCall, ProgramId},
};

use crate::util::{authorized, shift_balance};

/// `Poke` - advance the stored weight for off-chain consumers. Pricing is lazy,
/// so this is purely a convenience; idempotent within a block.
/// Account order: `[pool, clock]`.
#[must_use]
pub fn poke(
    pool: AccountWithMetadata,
    self_program_id: ProgramId,
    clock_ts: i64,
) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
    let mut state = PoolState::try_from(&pool.account.data).expect("invalid pool state account");
    assert_eq!(pool.account.program_owner, self_program_id, "pool not owned by LBP program");
    let t_ms = u64::try_from(clock_ts.max(0)).unwrap_or(0);
    state.stored_w_token_q64 = state.weight_token_q64(t_ms);
    state.stored_w_ts_ms = t_ms;
    let mut pool_post = pool.account.clone();
    pool_post.data = Data::from(&state);
    (vec![AccountPostState::new(pool_post)], vec![])
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
    let mut state = PoolState::try_from(&pool.account.data).expect("invalid pool state account");
    assert_eq!(pool.account.program_owner, self_program_id, "pool not owned by LBP program");
    assert!(creator.is_authorized, "creator must authorize pause/resume");
    assert_eq!(creator.account_id, state.creator, "only the creator may pause/resume");
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
    let mut state = PoolState::try_from(&pool.account.data).expect("invalid pool state account");
    assert_eq!(pool.account.program_owner, self_program_id, "pool not owned by LBP program");
    assert!(creator.is_authorized, "creator must authorize close");
    assert_eq!(creator.account_id, state.creator, "only the creator may close");
    assert!(matches!(state.status, SaleStatus::Open), "sale already closed");
    let now = u64::try_from(clock_ts.max(0)).unwrap_or(0);
    assert!(now >= state.t_end_ms, "cannot close before the sale end timestamp");
    state.status = SaleStatus::Closed;
    let mut pool_post = pool.account.clone();
    pool_post.data = Data::from(&state);
    (
        vec![AccountPostState::new(pool_post), AccountPostState::new(creator.account)],
        vec![],
    )
}

/// `Withdraw` - creator withdraws collateral net of the at-close protocol fee
/// (RFP-016 Func §5) plus any unsold project tokens. The fee goes to the
/// treasury atomically.
///
/// Account order: `[pool, token_vault, collateral_vault, treasury,
/// creator_collateral_holding, creator_token_holding, creator]`.
#[expect(clippy::too_many_arguments, reason = "fixed protocol account list")]
#[must_use]
pub fn withdraw(
    pool: AccountWithMetadata,
    token_vault: AccountWithMetadata,
    collateral_vault: AccountWithMetadata,
    treasury: AccountWithMetadata,
    creator_collateral_holding: AccountWithMetadata,
    creator_token_holding: AccountWithMetadata,
    creator: AccountWithMetadata,
    self_program_id: ProgramId,
) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
    let mut state = PoolState::try_from(&pool.account.data).expect("invalid pool state account");
    assert_eq!(pool.account.program_owner, self_program_id, "pool not owned by LBP program");
    assert!(creator.is_authorized, "creator must authorize withdrawal");
    assert_eq!(creator.account_id, state.creator, "only the creator may withdraw");
    assert!(matches!(state.status, SaleStatus::Closed), "sale must be closed before withdrawal");
    assert_eq!(token_vault.account_id, state.token_vault_id, "wrong token vault");
    assert_eq!(collateral_vault.account_id, state.collateral_vault_id, "wrong collateral vault");
    assert_eq!(treasury.account_id, state.treasury_id, "wrong treasury account");

    let (_, collateral_balance) = read_fungible(&collateral_vault, "Withdraw: collateral vault");
    let (_, token_balance) = read_fungible(&token_vault, "Withdraw: token vault");
    assert!(collateral_balance > 0 || token_balance > 0, "nothing to withdraw (already drained)");

    // Tax only buyer-raised collateral, not the creator's own seed deposit.
    // `cum_collateral_in` tracks exactly the raised amount; `.min` guards edges.
    let fee = close_fee(state.cum_collateral_in.min(collateral_balance), state.fee_bps);
    let net = collateral_balance - fee;
    let pool_id = pool.account_id;
    let token_program_id = collateral_vault.account.program_owner;
    let mut calls = Vec::with_capacity(3);

    // 1. at-close fee -> treasury.
    if fee > 0 {
        calls.push(
            ChainedCall::new(
                token_program_id,
                vec![authorized(&collateral_vault), treasury.clone()],
                &token_core::Instruction::Transfer { amount_to_transfer: fee },
            )
            .with_pda_seeds(vec![compute_collateral_vault_pda_seed(pool_id)]),
        );
    }
    // 2. net collateral -> creator.
    if net > 0 {
        let vault_after_fee = shift_balance(&authorized(&collateral_vault), fee, false);
        calls.push(
            ChainedCall::new(
                token_program_id,
                vec![vault_after_fee, creator_collateral_holding.clone()],
                &token_core::Instruction::Transfer { amount_to_transfer: net },
            )
            .with_pda_seeds(vec![compute_collateral_vault_pda_seed(pool_id)]),
        );
    }
    // 3. unsold project tokens -> creator.
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

    state.reserve_collateral = 0;
    state.reserve_token = 0;
    let mut pool_post = pool.account.clone();
    pool_post.data = Data::from(&state);

    let post_states = vec![
        AccountPostState::new(pool_post),
        AccountPostState::new(token_vault.account),
        AccountPostState::new(collateral_vault.account),
        AccountPostState::new(treasury.account),
        AccountPostState::new(creator_collateral_holding.account),
        AccountPostState::new(creator_token_holding.account),
        AccountPostState::new(creator.account),
    ];
    (post_states, calls)
}
