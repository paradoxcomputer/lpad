//! `CreateSale` - open a new LBP sale.
//!
//! Account order: `[pool, token_vault, collateral_vault, creator_token_holding,
//! creator_collateral_holding, creator, clock]`.
//! - `pool`                 - uninitialized PDA, claimed by this program.
//! - `token_vault`          - uninitialized PDA; receives the project-token deposit.
//! - `collateral_vault`     - uninitialized PDA; receives the collateral seed.
//! - `creator_token_holding`/`creator_collateral_holding` - creator's holdings (signer).
//! - `creator`              - creator identity (signer); stored for auth.

use lbp_core::{
    compute_collateral_vault_pda, compute_collateral_vault_pda_seed, compute_pool_pda,
    compute_pool_pda_seed, compute_token_vault_pda, compute_token_vault_pda_seed, fixed::ONE,
    read_fungible, spot_price_q64, PoolState, SaleStatus, MAX_DURATION_MS, MAX_FEE_BPS, MAX_RESERVE,
};
use nssa_core::{
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
    creator_token_holding: AccountWithMetadata,
    creator_collateral_holding: AccountWithMetadata,
    creator: AccountWithMetadata,
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
    assert!(fee_bps <= MAX_FEE_BPS, "fee_bps exceeds the maximum allowed");
    assert!(token_deposit > 0, "token deposit must be positive");
    assert!(collateral_seed > 0, "collateral seed must be positive to anchor the price");
    // The Balancer price math left-shifts both reserves by 64 (Q64.64), so any
    // reserve >= 2^64 overflows the u128 domain. The fixed-price branch catches
    // collateral_seed via spot_price_q64 -> div_to_q64, but a non-fixed pool never
    // touches that path at creation, so a >= 2^64 seed/deposit would create a pool
    // whose every buy reverts in buy_tokens_out (deposit-accepting, un-buyable).
    // Reject it here to honor MAX_RESERVE's "creates that would exceed it revert"
    // invariant. Mirrors bonding_curve::create_sale's MAX_VIRT_COLLATERAL bound.
    assert!(
        collateral_seed < MAX_RESERVE && token_deposit < MAX_RESERVE,
        "reserves exceed the 64-bit Q64.64 domain (>= 2^64)"
    );
    assert!(creator.is_authorized, "creator must authorize sale creation");
    // SECURITY (mirrors bonding_curve::create_sale): a default/zero ata_program_id
    // can never name a real ATA program, so BuyAta would dispatch the buyer's
    // collateral leg to a no-op that skips the real token::Transfer. Reject it
    // on-chain; the SDK additionally refuses a non-canonical pin participant-side.
    assert!(
        ata_program_id != DEFAULT_PROGRAM_ID,
        "ata_program_id must be a real ATA program (not the default/zero id)"
    );
    assert!(w_start_q64 > 0 && w_start_q64 < ONE, "w_start must be in (0,1)");
    assert!(w_end_q64 > 0 && w_end_q64 < ONE, "w_end must be in (0,1)");
    assert!(t_end_ms > t_start_ms, "t_end must be after t_start");
    assert!(
        (t_end_ms - t_start_ms) >= min_duration_ms,
        "sale duration must be at least min_duration_ms (privacy fairness)"
    );
    // Bound the interpolation domain so weight_token_q64's `delta * elapsed`
    // (delta up to ~2^64, elapsed up to the span) can never approach i128::MAX
    // and wrap under release overflow-checks. ~10 years is far beyond any real
    // sale; an unbounded creator-set span would otherwise make buys revert in
    // the overflowing window (self-inflicted DoS) and break the in-range weight
    // invariant. Mirrors MAX_RESERVE's "creates that would exceed it revert".
    assert!(
        (t_end_ms - t_start_ms) <= MAX_DURATION_MS,
        "sale duration exceeds the maximum allowed (~10 years)"
    );
    if !fixed_price {
        assert!(
            w_start_q64 > w_end_q64,
            "LBP token weight must decline (w_start > w_end); use fixed_price for a flat sale"
        );
    }

    let (token_definition_id, creator_token_balance) =
        read_fungible(&creator_token_holding, "CreateSale: creator token holding");
    let (collateral_def, creator_collateral_balance) =
        read_fungible(&creator_collateral_holding, "CreateSale: creator collateral holding");
    assert_eq!(
        collateral_def, collateral_definition_id,
        "creator collateral holding does not match the declared collateral definition"
    );
    assert!(creator_token_balance >= token_deposit, "insufficient project tokens");
    assert!(creator_collateral_balance >= collateral_seed, "insufficient collateral seed");
    assert!(
        token_definition_id != collateral_definition_id,
        "project and collateral tokens must differ"
    );

    // --- PDA checks -----------------------------------------------------------
    let pool_id = compute_pool_pda(
        self_program_id,
        token_definition_id,
        collateral_definition_id,
        creator.account_id,
        nonce,
    );
    assert_eq!(pool.account_id, pool_id, "pool account id does not match PDA");
    assert_eq!(pool.account, Account::default(), "pool account must be uninitialized");
    let token_vault_id = compute_token_vault_pda(self_program_id, pool_id);
    let collateral_vault_id = compute_collateral_vault_pda(self_program_id, pool_id);
    assert_eq!(token_vault.account_id, token_vault_id, "token vault id mismatch");
    assert_eq!(collateral_vault.account_id, collateral_vault_id, "collateral vault id mismatch");
    // The at-close fee must not sweep into the pool's own vaults.
    assert!(
        treasury_id != collateral_vault_id && treasury_id != token_vault_id,
        "treasury must differ from the pool's token and collateral vaults"
    );

    // --- build state ----------------------------------------------------------
    let fixed_price_q64 = if fixed_price {
        let price = spot_price_q64(token_deposit, collateral_seed, w_start_q64);
        // spot_price_q64 floors to 0 when collateral_seed is tiny relative to
        // token_deposit (below Q64.64 resolution); the collateral_seed > 0 check
        // above guards the raw amount, not the resulting price. A zero price would
        // make every buy revert (fixed_price_tokens_out asserts price_q64 > 0),
        // leaving a deposit-accepting but permanently un-buyable pool.
        assert!(price > 0, "fixed price rounds to zero; increase collateral_seed or decrease token_deposit");
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
        stored_w_token_q64: w_start_q64,
        stored_w_ts_ms: t_start_ms,
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

    let token_program_id = creator_token_holding.account.program_owner;
    let deposit_tokens = ChainedCall::new(
        token_program_id,
        vec![creator_token_holding.clone(), authorized(&token_vault)],
        &token_core::Instruction::Transfer { amount_to_transfer: token_deposit },
    )
    .with_pda_seeds(vec![compute_token_vault_pda_seed(pool_id)]);
    let deposit_collateral = ChainedCall::new(
        token_program_id,
        vec![creator_collateral_holding.clone(), authorized(&collateral_vault)],
        &token_core::Instruction::Transfer { amount_to_transfer: collateral_seed },
    )
    .with_pda_seeds(vec![compute_collateral_vault_pda_seed(pool_id)]);

    let post_states = vec![
        pool_post,
        AccountPostState::new(token_vault.account),
        AccountPostState::new(collateral_vault.account),
        AccountPostState::new(creator_token_holding.account),
        AccountPostState::new(creator_collateral_holding.account),
        AccountPostState::new(creator.account),
    ];
    (post_states, vec![deposit_tokens, deposit_collateral])
}
