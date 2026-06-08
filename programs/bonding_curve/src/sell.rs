//! Sell instruction - sell project tokens back into the curve at the current
//! AMM price (two-way curve, RFP-015 Func §Two-way curve). Reverts if the sale
//! was created in one-directional mode. Output is bounded by the real collateral
//! reserve so the pool is always solvent.

use bonding_curve_core::{
    compute_collateral_vault_pda_seed, read_fungible, sell_collateral_out, SaleState, SaleStatus,
};
use nssa_core::{
    account::{AccountId, AccountWithMetadata, Data},
    program::{AccountPostState, ChainedCall, ProgramId},
};

use crate::util::{authorized, shift_balance};

/// Result of pricing + applying a sell to the in-memory sale state. Shared by the
/// keypair `sell` and the ATA `sell_ata` paths (mirrors `buy::BuyOutcome`).
pub struct SellOutcome {
    pub state: SaleState,
    pub to_seller: u128,
    pub fee: u128,
    pub c_out_raw: u128,
}

/// Validate, price, and apply a sell to `state`. Panics (reverts) on a closed or
/// one-directional sale, a too-small input, an over-the-real-reserve withdrawal,
/// or slippage; the output is bounded by the real collateral reserve so the pool
/// stays solvent. Mirrors [`crate::buy::apply_buy`]. The caller validates the
/// vault/treasury accounts before calling this.
#[must_use]
pub(crate) fn apply_sell(
    mut state: SaleState,
    tokens_in: u128,
    min_collateral_out: u128,
    clock_ts: i64,
) -> SellOutcome {
    assert!(matches!(state.status, SaleStatus::Open), "sale is not open");
    assert!(
        !state.one_directional,
        "selling is disabled for this sale (one-directional mode)"
    );
    assert!(tokens_in > 0, "tokens_in must be positive");

    let (to_seller, fee, c_out_raw) =
        sell_collateral_out(state.virt_token, state.virt_collateral, state.fee_bps, tokens_in);
    assert!(c_out_raw > 0, "computed collateral out is zero (input too small)");
    assert!(
        c_out_raw <= state.real_collateral,
        "sell would withdraw more collateral than the real reserve holds"
    );
    assert!(to_seller >= min_collateral_out, "slippage: collateral out below minimum");

    state.virt_token = state
        .virt_token
        .checked_add(tokens_in)
        .expect("Vt + tokens_in overflows u128");
    state.virt_collateral -= c_out_raw;
    state.sale_reserve = state
        .sale_reserve
        .checked_add(tokens_in)
        .expect("sale_reserve overflow");
    state.real_collateral -= c_out_raw;
    state.cum_fees = state.cum_fees.saturating_add(fee);
    state.sell_count = state.sell_count.saturating_add(1);
    state.record_observation(u64::try_from(clock_ts.max(0)).unwrap_or(0));

    SellOutcome { state, to_seller, fee, c_out_raw }
}

/// The collateral-vault payout legs of a sell, shared by both paths: leg
/// `collateral_vault → recipient` (net of fee) then leg `collateral_vault →
/// treasury` (fee, if any). `recipient` is the seller's keypair collateral
/// holding (public path) or collateral ATA (ATA path). The leg-1 token *return*
/// (keypair vs ATA) differs and is built by the caller.
#[must_use]
pub(crate) fn sell_collateral_payout(
    collateral_vault: &AccountWithMetadata,
    recipient: &AccountWithMetadata,
    treasury: &AccountWithMetadata,
    sale_id: AccountId,
    outcome: &SellOutcome,
) -> Vec<ChainedCall> {
    let token_program_id = collateral_vault.account.program_owner;
    let mut calls = Vec::with_capacity(2);
    // collateral vault (PDA) -> recipient (net of fee).
    calls.push(
        ChainedCall::new(
            token_program_id,
            vec![authorized(collateral_vault), recipient.clone()],
            &token_core::Instruction::Transfer { amount_to_transfer: outcome.to_seller },
        )
        .with_pda_seeds(vec![compute_collateral_vault_pda_seed(sale_id)]),
    );
    // collateral vault (PDA, post-payout) -> treasury (protocol fee), if any.
    if outcome.fee > 0 {
        calls.push(
            ChainedCall::new(
                token_program_id,
                vec![
                    shift_balance(&authorized(collateral_vault), outcome.to_seller, false),
                    treasury.clone(),
                ],
                &token_core::Instruction::Transfer { amount_to_transfer: outcome.fee },
            )
            .with_pda_seeds(vec![compute_collateral_vault_pda_seed(sale_id)]),
        );
    }
    calls
}

/// `Sell` - public sell with keypair token holdings.
///
/// Account order: `[sale, token_vault, collateral_vault, treasury,
/// seller_token_holding, seller_collateral_holding, clock]`.
#[expect(clippy::too_many_arguments, reason = "fixed protocol account list")]
#[must_use]
pub fn sell(
    sale: AccountWithMetadata,
    token_vault: AccountWithMetadata,
    collateral_vault: AccountWithMetadata,
    treasury: AccountWithMetadata,
    seller_token_holding: AccountWithMetadata,
    seller_collateral_holding: AccountWithMetadata,
    tokens_in: u128,
    min_collateral_out: u128,
    self_program_id: ProgramId,
    clock_ts: i64,
) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
    let state = SaleState::try_from(&sale.account.data).expect("invalid sale state account");
    assert_eq!(
        sale.account.program_owner, self_program_id,
        "sale account must be owned by the bonding-curve program"
    );
    assert_eq!(token_vault.account_id, state.token_vault_id, "wrong token vault");
    assert_eq!(
        collateral_vault.account_id, state.collateral_vault_id,
        "wrong collateral vault"
    );
    assert_eq!(treasury.account_id, state.treasury_id, "wrong treasury account");
    assert!(
        seller_token_holding.is_authorized,
        "seller token holding must be authorized"
    );
    // Parity with `sell_ata`: reject a wrong-token return early with a clear error
    // (the token program would also reject the leg-1 transfer on a definition
    // mismatch, but only with a generic message).
    let (token_def, _) = read_fungible(&seller_token_holding, "Sell: seller token holding");
    assert_eq!(
        token_def, state.token_definition_id,
        "seller token holding does not match the sale's project token definition"
    );
    // SECURITY: leg 1 returns the seller's tokens to the PDA-anchored token vault,
    // so it must run on the vault's real token program - not whatever program owns
    // the submitter-chosen seller holding. Otherwise a holding owned by a no-op
    // EVIL program would let leg 1 silently skip the token return while the
    // collateral payout (legs 2/3) still drains the vault. Mirrors the buy-path
    // collateral-type defense and the ATA program pin.
    assert_eq!(
        seller_token_holding.account.program_owner, token_vault.account.program_owner,
        "seller token holding is not owned by the sale's token program"
    );

    let outcome = apply_sell(state, tokens_in, min_collateral_out, clock_ts);
    let token_program_id = token_vault.account.program_owner;
    let sale_id = sale.account_id;
    let mut calls = Vec::with_capacity(3);

    // 1. seller returns tokens to the (already-initialized) token vault.
    calls.push(ChainedCall::new(
        token_program_id,
        vec![seller_token_holding.clone(), token_vault.clone()],
        &token_core::Instruction::Transfer { amount_to_transfer: tokens_in },
    ));
    // 2 + 3. collateral payout to the seller, then the fee sweep to the treasury.
    calls.extend(sell_collateral_payout(
        &collateral_vault,
        &seller_collateral_holding,
        &treasury,
        sale_id,
        &outcome,
    ));

    let mut sale_post = sale.account.clone();
    sale_post.data = Data::from(&outcome.state);

    let post_states = vec![
        AccountPostState::new(sale_post),
        AccountPostState::new(token_vault.account),
        AccountPostState::new(collateral_vault.account),
        AccountPostState::new(treasury.account),
        AccountPostState::new(seller_token_holding.account),
        AccountPostState::new(seller_collateral_holding.account),
    ];
    (post_states, calls)
}
