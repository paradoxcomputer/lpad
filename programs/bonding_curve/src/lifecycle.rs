//! Close and withdraw - end of the sale lifecycle.

use bonding_curve_core::{
    compute_collateral_vault_pda_seed, compute_token_vault_pda_seed, read_fungible, SaleState,
    SaleStatus,
};
use nssa_core::{
    account::{AccountWithMetadata, Data},
    program::{AccountPostState, ChainedCall, ProgramId},
};

use crate::util::authorized;

/// `CloseSale` - manual close by the creator. Permitted once the configured end
/// timestamp has passed, or if the supply target is already exhausted
/// (idempotent with the auto-close that fires on the final buy).
///
/// Account order: `[sale, creator, clock]`.
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
    assert!(
        timed_out || supply_done,
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

/// `Withdraw` - creator withdraws the full real collateral reserve (fees already
/// taken per-swap; no extra deduction, RFP-015 Func §5) and the remaining
/// project tokens in the vault (DEX seed `R` plus any unsold sale reserve).
///
/// Account order: `[sale, token_vault, collateral_vault, creator_collateral_holding,
/// creator_token_holding, creator, clock]`.
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
    assert!(
        collateral_balance > 0 || token_balance > 0,
        "nothing to withdraw (already drained)"
    );

    let sale_id = sale.account_id;
    let token_program_id = collateral_vault.account.program_owner;
    let mut calls = Vec::with_capacity(2);
    if collateral_balance > 0 {
        calls.push(
            ChainedCall::new(
                token_program_id,
                vec![authorized(&collateral_vault), creator_collateral_holding.clone()],
                &token_core::Instruction::Transfer { amount_to_transfer: collateral_balance },
            )
            .with_pda_seeds(vec![compute_collateral_vault_pda_seed(sale_id)]),
        );
    }
    if token_balance > 0 {
        calls.push(
            ChainedCall::new(
                token_program_id,
                vec![authorized(&token_vault), creator_token_holding.clone()],
                &token_core::Instruction::Transfer { amount_to_transfer: token_balance },
            )
            .with_pda_seeds(vec![compute_token_vault_pda_seed(sale_id)]),
        );
    }

    // Zero the accounting buckets for a clean closed-sale state. (A second
    // withdraw is actually prevented by the drained-vault balance check above,
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
