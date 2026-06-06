//! Buy instructions - public (keypair holdings) and private (disposable
//! deshield→buy→re-shield through a fresh ephemeral account A).
//!
//! Pricing and state transition are shared by [`apply_buy`]; the variants differ
//! only in how collateral arrives and where the tokens go.

use bonding_curve_core::{
    buy_tokens_out, compute_collateral_vault_pda_seed, compute_token_vault_pda_seed, read_fungible,
    SaleState, SaleStatus,
};
use nssa_core::{
    account::{AccountWithMetadata, Data},
    program::{AccountPostState, ChainedCall, ProgramId},
};

use crate::util::{authorized, shift_balance};

/// Result of pricing + applying a buy to the in-memory sale state.
pub struct BuyOutcome {
    pub state: SaleState,
    pub tokens_out: u128,
    pub fee: u128,
    pub c_eff: u128,
}

/// Validate, price, and apply a buy to `state`. Panics (reverts) on a closed
/// sale, an exhausted/insufficient reserve, slippage, or an elapsed end
/// timestamp. `now_ms` is the on-chain clock for public buys, or `None` for the
/// private path (supply-driven pricing needs no time; the end-timestamp guard is
/// enforced instead by the SDK's timestamp validity window).
#[must_use]
pub fn apply_buy(
    mut state: SaleState,
    collateral_in: u128,
    min_tokens_out: u128,
    now_ms: Option<i64>,
) -> BuyOutcome {
    assert!(matches!(state.status, SaleStatus::Open), "sale is not open");
    if let Some(now) = now_ms
        && state.end_timestamp_ms != 0
    {
        let now_u = u64::try_from(now.max(0)).expect("clock must be non-negative");
        assert!(now_u < state.end_timestamp_ms, "sale has ended (end timestamp passed)");
    }

    let (tokens_out, fee, c_eff) =
        buy_tokens_out(state.virt_token, state.virt_collateral, state.fee_bps, collateral_in);
    assert!(tokens_out >= 1, "computed tokens_out is zero (input too small)");
    assert!(
        tokens_out <= state.sale_reserve,
        "tokens_out exceeds the remaining sale reserve"
    );
    assert!(tokens_out >= min_tokens_out, "slippage: tokens_out below minimum");

    state.virt_token -= tokens_out;
    state.virt_collateral = state
        .virt_collateral
        .checked_add(c_eff)
        .expect("Vc + c_eff overflows u128");
    state.sale_reserve -= tokens_out;
    state.real_collateral = state
        .real_collateral
        .checked_add(c_eff)
        .expect("real_collateral overflow");
    state.cum_collateral_in = state.cum_collateral_in.saturating_add(collateral_in);
    state.cum_fees = state.cum_fees.saturating_add(fee);
    state.buy_count = state.buy_count.saturating_add(1);
    if let Some(now) = now_ms {
        state.record_observation(u64::try_from(now.max(0)).unwrap_or(0));
    }
    // Auto-close on supply target (RFP-015 Reliability #3): atomic, same tx.
    if state.sale_reserve == 0 {
        state.status = SaleStatus::Closed;
    }

    BuyOutcome { state, tokens_out, fee, c_eff }
}

/// Build the chained `token::Transfer`s for a buy: `payer → collateral_vault
/// (c_eff)`, `payer → treasury (fee)` (skipped when fee==0), `token_vault →
/// recipient (tokens_out)`. `payer` is an already-authorized collateral holding.
fn buy_transfers(
    payer: &AccountWithMetadata,
    collateral_vault: &AccountWithMetadata,
    treasury: &AccountWithMetadata,
    token_vault: &AccountWithMetadata,
    recipient: &AccountWithMetadata,
    sale_id: nssa_core::account::AccountId,
    outcome: &BuyOutcome,
) -> Vec<ChainedCall> {
    let token_program_id = payer.account.program_owner;
    let mut calls = Vec::with_capacity(3);

    // 1. payer -> collateral vault; inits+claims the vault PDA.
    calls.push(
        ChainedCall::new(
            token_program_id,
            vec![payer.clone(), authorized(collateral_vault)],
            &token_core::Instruction::Transfer { amount_to_transfer: outcome.c_eff },
        )
        .with_pda_seeds(vec![compute_collateral_vault_pda_seed(sale_id)]),
    );

    // 2. payer (post step 1) -> treasury (protocol fee), if any.
    if outcome.fee > 0 {
        calls.push(ChainedCall::new(
            token_program_id,
            vec![shift_balance(payer, outcome.c_eff, false), treasury.clone()],
            &token_core::Instruction::Transfer { amount_to_transfer: outcome.fee },
        ));
    }

    // 3. token vault (PDA) -> recipient (tokens_out); inits+claims recipient if default.
    calls.push(
        ChainedCall::new(
            token_program_id,
            vec![authorized(token_vault), recipient.clone()],
            &token_core::Instruction::Transfer { amount_to_transfer: outcome.tokens_out },
        )
        .with_pda_seeds(vec![compute_token_vault_pda_seed(sale_id)]),
    );

    calls
}

/// Validate the vault/treasury accounts against the recorded sale state.
pub(crate) fn check_accounts(
    state: &SaleState,
    sale: &AccountWithMetadata,
    token_vault: &AccountWithMetadata,
    collateral_vault: &AccountWithMetadata,
    treasury: &AccountWithMetadata,
    self_program_id: ProgramId,
) {
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
}

/// `Buy` - public buy with keypair token holdings.
///
/// Account order: `[sale, token_vault, collateral_vault, treasury,
/// buyer_collateral_holding, buyer_token_holding, clock]`.
#[expect(clippy::too_many_arguments, reason = "fixed protocol account list")]
#[must_use]
pub fn buy(
    sale: AccountWithMetadata,
    token_vault: AccountWithMetadata,
    collateral_vault: AccountWithMetadata,
    treasury: AccountWithMetadata,
    buyer_collateral_holding: AccountWithMetadata,
    buyer_token_holding: AccountWithMetadata,
    collateral_in: u128,
    min_tokens_out: u128,
    self_program_id: ProgramId,
    clock_ts: i64,
) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
    let state = SaleState::try_from(&sale.account.data).expect("invalid sale state account");
    check_accounts(&state, &sale, &token_vault, &collateral_vault, &treasury, self_program_id);

    // SECURITY: the collateral vault is created lazily on the first buy, so it
    // would otherwise inherit whatever token the first buyer pays with. Require
    // the buyer to pay the sale's declared collateral token - without this, the
    // first buyer of a zero-fee sale could pay a worthless token (the fee
    // transfer that would force a type match is skipped when fee_bps == 0) and
    // drain the project-token vault.
    let (buyer_collateral_def, _) = read_fungible(&buyer_collateral_holding, "Buy: buyer collateral holding");
    assert_eq!(
        buyer_collateral_def, state.collateral_definition_id,
        "buyer collateral token does not match the sale's collateral definition"
    );

    let outcome = apply_buy(state, collateral_in, min_tokens_out, Some(clock_ts));

    let calls = buy_transfers(
        &buy_payer(&buyer_collateral_holding),
        &collateral_vault,
        &treasury,
        &token_vault,
        &buyer_token_holding,
        sale.account_id,
        &outcome,
    );

    let mut sale_post = sale.account.clone();
    sale_post.data = Data::from(&outcome.state);

    let post_states = vec![
        AccountPostState::new(sale_post),
        AccountPostState::new(token_vault.account),
        AccountPostState::new(collateral_vault.account),
        AccountPostState::new(treasury.account),
        AccountPostState::new(buyer_collateral_holding.account),
        AccountPostState::new(buyer_token_holding.account),
    ];
    (post_states, calls)
}

/// The buyer's collateral holding must be authorized (the buyer signs the tx).
fn buy_payer(buyer_collateral_holding: &AccountWithMetadata) -> AccountWithMetadata {
    assert!(
        buyer_collateral_holding.is_authorized,
        "buyer collateral holding must be authorized"
    );
    buyer_collateral_holding.clone()
}
