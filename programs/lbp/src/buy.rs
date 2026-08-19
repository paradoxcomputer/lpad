//! LBP buy instructions. Four paths, all sharing [`apply_buy`] for pricing and
//! the state transition:
//!
//! * [`buy`] - public, priced at the on-chain clock time.
//! * [`buy_gated`] - public, plus a Merkle inclusion proof of the buyer's own
//!   leaf against the committed allowlist root.
//! * `ata::buy_ata` - public, with the buyer side in Associated Token Accounts.
//! * [`buy_disposable`] - private (`Instruction::BuyDisposable`).
//!
//! No per-swap fee - the LBP protocol fee is collected at close.
//!
//! # What "private" means on this path
//!
//! There is no ephemeral account, no deshield leg and no re-shield leg. Privacy
//! on LEZ is a per-slot label (`InputAccountIdentity`) positionally aligned 1:1
//! with the guest's `pre_states`, so the buyer's collateral holding and token
//! holding are simply PRIVATE slots of an otherwise ordinary buy: the same two
//! chained `token::Transfer`s debit and credit them, and both the debit and the
//! credit are private notes inside one proof. **"Disposable" names the
//! single-use NOTE, not an account** - the older design documents in `docs/`
//! describe a 5-leg ephemeral-account router that this deliberately does not
//! build. Because the label is applied outside the guest, the post-state shape
//! is identical to the public path (see `finalize_buy`).
//!
//! The existing 3-transaction `buy-private` saga in the SDK is untouched and
//! stays: it is the path validated against a real sequencer today, and this one
//! is not yet.
//!
//! # Why the LBP one is the hard one: the price depends on time
//!
//! A privacy transaction cannot carry the Clock account. Every public account in
//! one is pinned byte-for-byte at proving time and re-verified against live
//! state at inclusion, and `CLOCK_01`'s data is rewritten every block, so a
//! private buy that declared the clock would be rejected by construction. The
//! price is therefore taken at the caller-supplied `t_buy_ms`, and the output's
//! timestamp validity window - `t_buy_ms .. private_window_hi(..)`, emitted by
//! the guest, enforced by the sequencer - is what binds that argument to real
//! time and to the sale's end.
//!
//! That is only safe because `buy_tokens_out` is non-decreasing in time at
//! fixed reserves: as `t` rises the token weight falls, the exponent
//! `w_collateral/w_token` rises, `base^x` falls (the base `R_c/(R_c+C)` is in
//! `(0,1)`), so `tokens_out` rises. Pricing at an earlier `t_buy_ms` hands the
//! buyer FEWER tokens than the fair amount at admission - the pool is never
//! underpaid.
//!
//! **Precondition: the pool PDA is pinned.** The argument above compares two
//! times at *identical reserves*, which holds only because the pinned-and-
//! re-verified pool account guarantees the reserves at proving time are the
//! reserves at inclusion. Any future change that relaxes that pin - a pool
//! account that may drift between proving and inclusion - breaks pricing safety
//! outright, not just precision. `lbp_core`'s
//! `proptests::tokens_out_non_decreasing_in_time` pins the monotonicity half;
//! nothing but this comment pins the other half.

use std::ops::Range;

use lbp_core::{
    buy_tokens_out, compute_collateral_vault_pda_seed, compute_token_vault_pda_seed,
    allowlist_leaf, fixed_price_tokens_out, is_open_allowlist, merkle_verify, private_window_hi,
    read_fungible, PoolState, SaleStatus, MAX_ALLOWLIST_PROOF_DEPTH,
};
use lee_core::{
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
///
/// `record_obs` appends `t_ms` to the pool's on-chain price-observation ring.
/// The public paths pass `true`; [`buy_disposable`] passes `false`, and must: on
/// that path `t_ms` is a timestamp the BUYER chose, so recording it would let
/// anyone write arbitrary times into an oracle other contracts may read - and a
/// conspicuously stale observation is itself a fingerprint marking that trade as
/// the private one.
#[must_use]
pub fn apply_buy(
    mut state: PoolState,
    collateral_in: u128,
    min_tokens_out: u128,
    t_ms: u64,
    block_id: u64,
    enforce_block: bool,
    record_obs: bool,
) -> BuyOutcome {
    assert!(matches!(state.status, SaleStatus::Open), "sale is not open");
    assert!(!state.paused, "sale is paused");
    assert!(collateral_in > 0, "collateral_in must be > 0");
    assert!(t_ms >= state.t_start_ms, "sale has not started");
    assert!(t_ms < state.t_end_ms, "sale has ended");

    let tokens_out = if state.fixed_price {
        fixed_price_tokens_out(collateral_in, state.fixed_price_q64)
    } else {
        let weight = state.weight_token_q64(t_ms);
        buy_tokens_out(state.reserve_token, state.reserve_collateral, weight, collateral_in)
    };
    // Zero means `collateral_in` was below the pool's price resolution.
    assert!(tokens_out >= 1, "tokens_out is zero");
    assert!(tokens_out <= state.reserve_token, "tokens_out > reserve");
    assert!(tokens_out >= min_tokens_out, "slippage: below minimum");

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
            "block ceiling exceeded"
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
    if record_obs {
        state.record_observation(t_ms);
    }

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
        "pool not owned by LBP"
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
    pool_id: lee_core::account::AccountId,
    collateral_in: u128,
    tokens_out: u128,
) -> Vec<ChainedCall> {
    // Anchored to the VAULTS, never to the submitter-supplied payer. LEZ
    // deployment is permissionless, so `payer.account.program_owner` is
    // attacker-chosen: a holding owned by a no-op program whose `Transfer`
    // echoes its pre-states makes both legs move nothing while `apply_buy` still
    // debits `reserve_token`. Repeated, that drives the reserve to zero for free
    // and every honest buy then reverts on `tokens_out <= reserve_token` - the
    // pool is unbuyable for the rest of its window at no cost to the attacker.
    // The vaults are seeded at `create_sale`, so they are the trustworthy
    // anchor. Mirrors the ATA path's pin.
    let collateral_program_id = collateral_vault.account.program_owner;
    let token_program_id = token_vault.account.program_owner;
    vec![
        ChainedCall::new(
            collateral_program_id,
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

/// Post-states + chained transfers shared by every buy path over the five-account
/// list `[pool, token_vault, collateral_vault, buyer_collateral_holding,
/// buyer_token_holding]`.
///
/// The private path reuses this verbatim: privacy is a per-slot label applied to
/// the transaction outside the guest, so the guest emits exactly the same post
/// states either way. A separate "private" finalizer would be two copies of one
/// contract, and the copy that drifted would be the one nobody tested.
fn finalize_buy(
    pool: AccountWithMetadata,
    token_vault: AccountWithMetadata,
    collateral_vault: AccountWithMetadata,
    buyer_collateral_holding: AccountWithMetadata,
    buyer_token_holding: AccountWithMetadata,
    outcome: BuyOutcome,
    collateral_in: u128,
) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
    assert!(buyer_collateral_holding.is_authorized, "buyer must authorize");
    // Each caller has already checked the holding's DEFINITION, but that reads
    // data a buyer-controlled program can write freely. Bind the holding to the
    // token program that owns the vault it pays into, so a no-op substitute
    // cannot pass as collateral. Here rather than in each caller: this finalizer
    // is the one path every buy variant goes through.
    assert_eq!(
        buyer_collateral_holding.account.program_owner,
        collateral_vault.account.program_owner,
        "buyer collateral: wrong program"
    );
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
    let state = PoolState::try_from(&pool.account.data).expect("bad pool state");
    check_accounts(&state, &pool, &token_vault, &collateral_vault, self_program_id);
    let (collateral_def, _) = read_fungible(&buyer_collateral_holding, "buyer collateral");
    assert_eq!(
        collateral_def, state.collateral_definition_id,
        "buyer collateral: wrong definition"
    );
    // SECURITY: an allowlist-gated pool may ONLY be bought through `buy_gated`
    // (which proves Merkle inclusion). Without this, the submitter could pick the
    // ungated `Buy` instruction against the same pool and defeat the allowlist
    // entirely (RFP-016 Func #7). Gated pools must route through `buy_gated`.
    assert!(
        is_open_allowlist(&state.allowlist_root),
        "pool is allowlist-gated; use BuyGated"
    );
    let t_ms = u64::try_from(clock_ts.max(0)).expect("clock must be non-negative");
    let outcome = apply_buy(state, collateral_in, min_tokens_out, t_ms, clock_block_id, true, true);
    finalize_buy(
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
    let state = PoolState::try_from(&pool.account.data).expect("bad pool state");
    check_accounts(&state, &pool, &token_vault, &collateral_vault, self_program_id);
    let (collateral_def, _) = read_fungible(&buyer_collateral_holding, "buyer collateral");
    assert_eq!(
        collateral_def, state.collateral_definition_id,
        "buyer collateral: wrong definition"
    );
    assert!(
        !is_open_allowlist(&state.allowlist_root),
        "no allowlist; use Buy"
    );
    // Bind the leaf to THIS buyer: an attacker cannot replay a published
    // (leaf, proof) because the leaf must be the buyer's own canonical leaf and
    // the buyer must authorize the tx.
    assert!(buyer_collateral_holding.is_authorized, "buyer must authorize");
    assert_eq!(
        leaf,
        allowlist_leaf(&buyer_collateral_holding.account_id),
        "wrong allowlist leaf"
    );
    // Bound the caller-supplied proof before hashing it: each entry costs one
    // SHA-256 in the guest, so an unbounded proof is a proving-cost DoS vector.
    assert!(
        proof.len() <= MAX_ALLOWLIST_PROOF_DEPTH,
        "proof too deep"
    );
    assert!(
        merkle_verify(leaf, &proof, state.allowlist_root),
        "allowlist proof invalid"
    );
    let t_ms = u64::try_from(clock_ts.max(0)).expect("clock must be non-negative");
    let outcome = apply_buy(state, collateral_in, min_tokens_out, t_ms, clock_block_id, true, true);
    finalize_buy(
        pool,
        token_vault,
        collateral_vault,
        buyer_collateral_holding,
        buyer_token_holding,
        outcome,
        collateral_in,
    )
}

/// The half-open timestamp validity window a `BuyDisposable` output must carry:
/// `t_buy_ms .. private_window_hi(deadline, t_buy_ms, t_end_ms)`.
///
/// The lower bound is the price timestamp itself, so the sequencer will not
/// admit the transaction before the moment it was priced at; the upper bound is
/// the earliest of the caller's deadline, the one-hour staleness cap, and the
/// sale's own end. Only the top-level output may carry a window: the privacy
/// circuit INTERSECTS the windows of every program output and panics on an empty
/// intersection, so the chained `token::Transfer`s must stay unbounded.
///
/// This lives in the library rather than inline in the guest's dispatch arm so
/// the empty-window revert is reachable from the host test suite - `src/main.rs`
/// is a guest `[[bin]]` and `cargo test` never builds it.
#[must_use]
pub fn disposable_window(deadline: u64, t_buy_ms: u64, t_end_ms: u64) -> Range<u64> {
    let hi = private_window_hi(deadline, t_buy_ms, t_end_ms);
    // `Range` is half-open and `TryFrom<Range>` rejects `from >= to`, so an empty
    // window would be a plain `Err` the guest could only unwrap into a vaguer
    // panic. Fail here with something the submitter can act on.
    // `hi` is the earliest of the caller's deadline, `t_buy_ms + 1h` (the
    // staleness cap) and the sale's own `t_end_ms`, so an empty window means
    // `t_buy_ms` is at or past one of those. Remedy: re-quote at a `t_buy_ms`
    // before the sale ends, and pass a deadline after it.
    assert!(t_buy_ms < hi, "BuyDisposable: empty validity window");
    t_buy_ms..hi
}

/// `BuyDisposable` - private buy. The buyer's collateral holding and token
/// holding are PRIVATE account slots; see the module doc for what that does and
/// does not mean.
///
/// Account order: `[pool, token_vault, collateral_vault,
/// buyer_collateral_holding, buyer_token_holding]` - no clock, because a privacy
/// transaction cannot carry one.
///
/// Returns the pool's `t_end_ms` alongside the usual pair: the caller needs it to
/// build the validity window with [`disposable_window`], and reading it here
/// takes it from the PINNED pool account rather than from a caller argument that
/// could name a later end than the pool has.
#[expect(clippy::too_many_arguments, reason = "fixed protocol account list")]
#[must_use]
pub fn buy_disposable(
    pool: AccountWithMetadata,
    token_vault: AccountWithMetadata,
    collateral_vault: AccountWithMetadata,
    buyer_collateral_holding: AccountWithMetadata,
    buyer_token_holding: AccountWithMetadata,
    collateral_in: u128,
    min_tokens_out: u128,
    t_buy_ms: u64,
    self_program_id: ProgramId,
) -> (Vec<AccountPostState>, Vec<ChainedCall>, u64) {
    let state = PoolState::try_from(&pool.account.data).expect("bad pool state");
    check_accounts(&state, &pool, &token_vault, &collateral_vault, self_program_id);
    let (collateral_def, _) = read_fungible(&buyer_collateral_holding, "buyer collateral");
    assert_eq!(
        collateral_def, state.collateral_definition_id,
        "buyer collateral: wrong definition"
    );

    // SECURITY: an allowlist-gated pool may only be bought through `buy_gated`,
    // which binds the inclusion proof to the buyer's OWN leaf. There is no
    // private equivalent of that binding here, and letting the ungated private
    // path run against a gated pool would defeat the allowlist entirely - the
    // same hole `buy` and `buy_ata` close for the public paths.
    assert!(
        is_open_allowlist(&state.allowlist_root),
        "pool is allowlist-gated and cannot be bought privately; use BuyGated"
    );
    // The per-block ceiling is keyed on the clock's block id, and a proof has no
    // block id to key on. Rather than silently bypass a control the creator
    // configured, refuse the pool outright: a silent bypass is the same bug as no
    // ceiling at all, only harder to notice.
    assert!(
        state.block_token_ceiling == 0,
        "per-block ceiling; buy publicly"
    );
    // Monotonicity of `buy_tokens_out` in time is what makes a caller-chosen
    // `t_buy_ms` safe, and it needs a DECLINING weight schedule. `create_sale`
    // asserts that today, but only at creation - assert it again here so this
    // instruction's safety does not rest on a check in a different function that
    // ran at a different time.
    assert!(
        state.fixed_price || state.w_start_q64 > state.w_end_q64,
        "weight does not decline; buy publicly"
    );

    let t_end_ms = state.t_end_ms;
    // `apply_buy` supplies the remaining guards this path needs: sale open, not
    // paused, and `t_start_ms <= t_buy_ms < t_end_ms`. The block id is unused
    // (`enforce_block = false`) because the ceiling is asserted zero above, and
    // `record_obs = false` keeps the buyer's chosen timestamp out of the on-chain
    // observation ring.
    let outcome = apply_buy(state, collateral_in, min_tokens_out, t_buy_ms, 0, false, false);
    let (post_states, chained_calls) = finalize_buy(
        pool,
        token_vault,
        collateral_vault,
        buyer_collateral_holding,
        buyer_token_holding,
        outcome,
        collateral_in,
    );
    (post_states, chained_calls, t_end_ms)
}
