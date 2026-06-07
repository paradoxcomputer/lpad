//! LBP buy instructions: public (clock-priced), allowlist-gated, and private
//! (disposable, priced at an argument timestamp bounded by the validity window).
//!
//! No per-swap fee - the LBP protocol fee is collected at close.

use lbp_core::{
    buy_tokens_out, compute_collateral_vault_pda_seed, compute_token_vault_pda_seed,
    allowlist_leaf, fixed_price_tokens_out, is_open_allowlist, merkle_verify, read_fungible,
    PoolState, SaleStatus, MAX_ALLOWLIST_PROOF_DEPTH,
};
use nssa_core::{
    account::{AccountWithMetadata, Data},
    program::{AccountPostState, ChainedCall, ProgramId},
};

use crate::util::authorized;

pub struct BuyOutcome {
    pub state: PoolState,
    pub tokens_out: u128,
}

/// Validate, price, and apply a buy at time `t_ms`. `block_id`/`enforce_block`
/// drive the optional per-block token ceiling (public path only).
#[must_use]
pub fn apply_buy(
    mut state: PoolState,
    collateral_in: u128,
    min_tokens_out: u128,
    t_ms: u64,
    block_id: u64,
    enforce_block: bool,
) -> BuyOutcome {
    assert!(matches!(state.status, SaleStatus::Open), "sale is not open");
    assert!(!state.paused, "sale is paused");
    assert!(collateral_in > 0, "collateral_in must be positive");
    assert!(t_ms >= state.t_start_ms, "sale has not started");
    assert!(t_ms < state.t_end_ms, "sale has ended");

    let tokens_out = if state.fixed_price {
        fixed_price_tokens_out(collateral_in, state.fixed_price_q64)
    } else {
        let weight = state.weight_token_q64(t_ms);
        buy_tokens_out(state.reserve_token, state.reserve_collateral, weight, collateral_in)
    };
    assert!(tokens_out >= 1, "computed tokens_out is zero (input too small)");
    assert!(tokens_out <= state.reserve_token, "tokens_out exceeds the token reserve");
    assert!(tokens_out >= min_tokens_out, "slippage: tokens_out below minimum");

    // Optional per-block token allocation ceiling (RFP-016 Func §2.5).
    if enforce_block && state.block_token_ceiling > 0 {
        if block_id != state.block_window_id {
            state.block_window_id = block_id;
            state.block_sold = 0;
        }
        state.block_sold = state
            .block_sold
            .checked_add(tokens_out)
            .expect("block_sold overflow");
        assert!(
            state.block_sold <= state.block_token_ceiling,
            "per-block token ceiling exceeded"
        );
    }

    state.reserve_token -= tokens_out;
    state.reserve_collateral = state
        .reserve_collateral
        .checked_add(collateral_in)
        .expect("reserve_collateral overflow");
    state.cum_collateral_in = state.cum_collateral_in.saturating_add(collateral_in);
    state.cum_tokens_out = state.cum_tokens_out.saturating_add(tokens_out);
    state.buy_count = state.buy_count.saturating_add(1);
    state.record_observation(t_ms);

    BuyOutcome { state, tokens_out }
}

pub(crate) fn check_accounts(
    state: &PoolState,
    pool: &AccountWithMetadata,
    token_vault: &AccountWithMetadata,
    collateral_vault: &AccountWithMetadata,
    self_program_id: ProgramId,
) {
    assert_eq!(
        pool.account.program_owner, self_program_id,
        "pool account must be owned by the LBP program"
    );
    assert_eq!(token_vault.account_id, state.token_vault_id, "wrong token vault");
    assert_eq!(collateral_vault.account_id, state.collateral_vault_id, "wrong collateral vault");
}

/// Collateral `payer → collateral_vault`, then `token_vault → recipient`.
fn buy_transfers(
    payer: &AccountWithMetadata,
    collateral_vault: &AccountWithMetadata,
    token_vault: &AccountWithMetadata,
    recipient: &AccountWithMetadata,
    pool_id: nssa_core::account::AccountId,
    collateral_in: u128,
    tokens_out: u128,
) -> Vec<ChainedCall> {
    let token_program_id = payer.account.program_owner;
    vec![
        ChainedCall::new(
            token_program_id,
            vec![payer.clone(), authorized(collateral_vault)],
            &token_core::Instruction::Transfer { amount_to_transfer: collateral_in },
        )
        .with_pda_seeds(vec![compute_collateral_vault_pda_seed(pool_id)]),
        ChainedCall::new(
            token_program_id,
            vec![authorized(token_vault), recipient.clone()],
            &token_core::Instruction::Transfer { amount_to_transfer: tokens_out },
        )
        .with_pda_seeds(vec![compute_token_vault_pda_seed(pool_id)]),
    ]
}

fn finalize_public(
    pool: AccountWithMetadata,
    token_vault: AccountWithMetadata,
    collateral_vault: AccountWithMetadata,
    buyer_collateral_holding: AccountWithMetadata,
    buyer_token_holding: AccountWithMetadata,
    outcome: BuyOutcome,
    collateral_in: u128,
) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
    assert!(buyer_collateral_holding.is_authorized, "buyer collateral holding must be authorized");
    let calls = buy_transfers(
        &buyer_collateral_holding,
        &collateral_vault,
        &token_vault,
        &buyer_token_holding,
        pool.account_id,
        collateral_in,
        outcome.tokens_out,
    );
    let mut pool_post = pool.account.clone();
    pool_post.data = Data::from(&outcome.state);
    let post_states = vec![
        AccountPostState::new(pool_post),
        AccountPostState::new(token_vault.account),
        AccountPostState::new(collateral_vault.account),
        AccountPostState::new(buyer_collateral_holding.account),
        AccountPostState::new(buyer_token_holding.account),
    ];
    (post_states, calls)
}

/// `Buy` - public buy, priced at the on-chain clock time.
///
/// Account order: `[pool, token_vault, collateral_vault, buyer_collateral_holding,
/// buyer_token_holding, clock]`.
#[expect(clippy::too_many_arguments, reason = "fixed protocol account list")]
#[must_use]
pub fn buy(
    pool: AccountWithMetadata,
    token_vault: AccountWithMetadata,
    collateral_vault: AccountWithMetadata,
    buyer_collateral_holding: AccountWithMetadata,
    buyer_token_holding: AccountWithMetadata,
    collateral_in: u128,
    min_tokens_out: u128,
    self_program_id: ProgramId,
    clock_ts: i64,
    clock_block_id: u64,
) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
    let state = PoolState::try_from(&pool.account.data).expect("invalid pool state account");
    check_accounts(&state, &pool, &token_vault, &collateral_vault, self_program_id);
    let (collateral_def, _) = read_fungible(&buyer_collateral_holding, "Buy: buyer collateral holding");
    assert_eq!(
        collateral_def, state.collateral_definition_id,
        "buyer collateral does not match the pool's collateral definition"
    );
    // SECURITY: an allowlist-gated pool may ONLY be bought through `buy_gated`
    // (which proves Merkle inclusion). Without this, the submitter could pick the
    // ungated `Buy` instruction against the same pool and defeat the allowlist
    // entirely (RFP-016 Func #7). Gated pools must route through `buy_gated`.
    assert!(
        is_open_allowlist(&state.allowlist_root),
        "this pool is allowlist-gated; use BuyGated with an inclusion proof"
    );
    let t_ms = u64::try_from(clock_ts.max(0)).expect("clock must be non-negative");
    let outcome = apply_buy(state, collateral_in, min_tokens_out, t_ms, clock_block_id, true);
    finalize_public(
        pool,
        token_vault,
        collateral_vault,
        buyer_collateral_holding,
        buyer_token_holding,
        outcome,
        collateral_in,
    )
}

/// `BuyGated` - public buy requiring a Merkle inclusion proof against the
/// committed allowlist root. Account order matches `buy`.
#[expect(clippy::too_many_arguments, reason = "fixed protocol account list")]
#[must_use]
pub fn buy_gated(
    pool: AccountWithMetadata,
    token_vault: AccountWithMetadata,
    collateral_vault: AccountWithMetadata,
    buyer_collateral_holding: AccountWithMetadata,
    buyer_token_holding: AccountWithMetadata,
    collateral_in: u128,
    min_tokens_out: u128,
    leaf: [u8; 32],
    proof: Vec<[u8; 32]>,
    self_program_id: ProgramId,
    clock_ts: i64,
    clock_block_id: u64,
) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
    let state = PoolState::try_from(&pool.account.data).expect("invalid pool state account");
    check_accounts(&state, &pool, &token_vault, &collateral_vault, self_program_id);
    let (collateral_def, _) = read_fungible(&buyer_collateral_holding, "BuyGated: buyer collateral holding");
    assert_eq!(
        collateral_def, state.collateral_definition_id,
        "buyer collateral does not match the pool's collateral definition"
    );
    assert!(
        !is_open_allowlist(&state.allowlist_root),
        "this sale has no allowlist; use Buy"
    );
    // Bind the leaf to THIS buyer: an attacker cannot replay a published
    // (leaf, proof) because the leaf must be the buyer's own canonical leaf and
    // the buyer must authorize the tx.
    assert!(buyer_collateral_holding.is_authorized, "buyer must authorize a gated buy");
    assert_eq!(
        leaf,
        allowlist_leaf(&buyer_collateral_holding.account_id),
        "allowlist leaf must be the buyer's own leaf"
    );
    // Bound the caller-supplied proof before hashing it: each entry costs one
    // SHA-256 in the guest, so an unbounded proof is a proving-cost DoS vector.
    assert!(
        proof.len() <= MAX_ALLOWLIST_PROOF_DEPTH,
        "allowlist proof exceeds maximum depth"
    );
    assert!(
        merkle_verify(leaf, &proof, state.allowlist_root),
        "allowlist gate rejected: inclusion proof invalid"
    );
    let t_ms = u64::try_from(clock_ts.max(0)).expect("clock must be non-negative");
    let outcome = apply_buy(state, collateral_in, min_tokens_out, t_ms, clock_block_id, true);
    finalize_public(
        pool,
        token_vault,
        collateral_vault,
        buyer_collateral_holding,
        buyer_token_holding,
        outcome,
        collateral_in,
    )
}

