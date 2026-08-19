//! Buy instructions - the public ones (keypair holdings, clock-checked) and the
//! **disposable** one, whose buyer-side holdings are private account slots.
//!
//! "Disposable" names the single-use private NOTE, not an account. Privacy on
//! LEZ is a per-slot label on an otherwise ordinary transaction, so
//! [`buy_disposable`] is the same buy as [`buy`] with the buyer's two holdings
//! marked private: the chained `token::Transfer`s debit and credit them
//! in-circuit. There is no ephemeral account, no deshield leg and no re-shield
//! leg - the 5-leg ephemeral-account router that the older design notes under
//! `docs/` describe is deliberately not what this builds.
//!
//! It does not replace anything either: the SDK's `buy-private` saga (deshield,
//! public buy, re-shield, sequenced across three transactions on the host) stays
//! as-is. This path is additive and has not yet been exercised against a real
//! sequencer.
//!
//! Pricing and state transition are shared by [`apply_buy`]; the paths differ
//! only in how collateral arrives, where the tokens go, and how time is proven -
//! the public paths read the on-chain clock, the disposable path cannot carry
//! one and is bounded by its transaction validity window instead.

use bonding_curve_core::{
    buy_tokens_out, compute_collateral_vault_pda_seed, compute_token_vault_pda_seed, read_fungible,
    SaleState, SaleStatus,
};
use lee_core::{
    account::{AccountId, AccountWithMetadata, Data},
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
/// disposable path (supply-driven pricing needs no time; the end-timestamp guard
/// is enforced instead by the emitted timestamp validity window, see
/// [`buy_disposable`]).
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
    sale_id: AccountId,
    outcome: &BuyOutcome,
) -> Vec<ChainedCall> {
    // Dispatch is anchored to the VAULTS, never to the submitter-supplied payer.
    // `payer.account.program_owner` is attacker-chosen: LEZ deployment is
    // permissionless, so a buyer can hand us a holding owned by a no-op program
    // whose `Transfer` echoes its pre-states. Both legs would then move nothing
    // while `apply_buy` still consumed the reserve and credited the curve - a
    // free sale-kill, and on a two-way sale a drain, since the inflated
    // `real_collateral` bounds a later `Sell` payout that runs on the vault's
    // real token program. The vaults are initialised at `create_sale`, so their
    // `program_owner` is the trustworthy anchor. Mirrors `sell.rs` and `ata.rs`.
    let collateral_program_id = collateral_vault.account.program_owner;
    let token_program_id = token_vault.account.program_owner;
    let mut calls = Vec::with_capacity(3);

    // 1. payer -> collateral vault. This leg does NOT create the vault: it is
    // initialized (and typed to the collateral definition) by `create_sale`'s
    // `token::InitializeAccount`, and the dispatch id above is read off the vault
    // account itself - which only resolves to the real token program because the
    // vault already exists. A buy against a vault that somehow did not exist
    // therefore fails at dispatch instead of quietly materializing one.
    calls.push(
        ChainedCall::new(
            collateral_program_id,
            vec![payer.clone(), authorized(collateral_vault)],
            &token_core::Instruction::Transfer { amount_to_transfer: outcome.c_eff },
        )
        .with_pda_seeds(vec![compute_collateral_vault_pda_seed(sale_id)]),
    );

    // 2. payer (post step 1) -> treasury (protocol fee), if any.
    if outcome.fee > 0 {
        calls.push(ChainedCall::new(
            collateral_program_id,
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

/// Validate the sale account and its two vaults against the recorded sale state.
/// Split out of [`check_accounts`] because the disposable path has no treasury
/// account to check - it escrows its fee in the collateral vault instead.
pub(crate) fn check_sale_and_vaults(
    state: &SaleState,
    sale: &AccountWithMetadata,
    token_vault: &AccountWithMetadata,
    collateral_vault: &AccountWithMetadata,
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
    check_sale_and_vaults(state, sale, token_vault, collateral_vault, self_program_id);
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

    check_buyer_collateral(
        &buyer_collateral_holding,
        &collateral_vault,
        &state,
        "Buy: buyer collateral holding",
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

/// SECURITY: require the buyer to pay the sale's declared collateral token.
///
/// The vault is not type-blind: `create_sale` initializes it as a holding of the
/// collateral definition, so the deposit leg's `token::Transfer` would reject a
/// foreign token on its own ("Sender and recipient definition id mismatch"). This
/// check is what turns that into an up-front revert naming the offending account
/// rather than a failure deep inside a chained call, and it is the only type
/// check that does not itself depend on the vault having been initialized as
/// intended. Load-bearing on both keypair paths, and doubly so on the disposable
/// one: a zero-fee sale emits no fee transfer to force a match, and the
/// disposable path has no separate fee transfer at all.
fn check_buyer_collateral(
    buyer_collateral_holding: &AccountWithMetadata,
    collateral_vault: &AccountWithMetadata,
    state: &SaleState,
    context: &str,
) {
    let (buyer_collateral_def, _) = read_fungible(buyer_collateral_holding, context);
    assert_eq!(
        buyer_collateral_def, state.collateral_definition_id,
        "buyer collateral token does not match the sale's collateral definition"
    );
    // The definition check above reads the holding's DATA, which a program the
    // buyer controls can write freely. Bind the holding to the same token
    // program that owns the vault it pays into, so a no-op substitute cannot
    // masquerade as collateral. Mirrors `sell.rs`'s seller-holding anchor.
    assert_eq!(
        buyer_collateral_holding.account.program_owner,
        collateral_vault.account.program_owner,
        "buyer collateral holding is not owned by the sale's collateral token program"
    );
}

/// `BuyDisposable` - buy whose buyer-side holdings are **private** account slots,
/// so the collateral debit and the token credit are private notes inside one
/// atomic proof. Same pricing, same state transition as [`buy`]; what differs is
/// the two accounts it does *not* take.
///
/// Account order: `[sale, token_vault, collateral_vault, buyer_collateral_holding,
/// buyer_token_holding]`.
///
/// **No clock.** Every public account in a privacy transaction is pinned
/// byte-for-byte at proving time and re-verified against live state at
/// inclusion, and the clock account's data is rewritten every block - so a
/// private buy that carried one could never be included. `apply_buy` is
/// therefore called with `None`, and the sale's `end_timestamp_ms` is enforced
/// by the timestamp validity window the guest emits around this call
/// (`bonding_curve_core::private_window_hi`).
///
/// **No treasury.** It is a `CreateSale` argument, shared across sales in lpad's
/// own bootstrap, so pinning it into a minutes-long proof would let any
/// fee-bearing activity anywhere invalidate every in-flight private buy. The fee
/// stays in the collateral vault as `state.treasury_owed` and is handed over
/// later by [`crate::lifecycle::sweep_treasury`], a permissionless instruction
/// anyone may trigger; the economics are identical and it costs one fewer chained
/// call here. That is why the collateral leg below moves the FULL gross
/// `collateral_in`, and why the vault holds `real_collateral + treasury_owed`.
#[expect(clippy::too_many_arguments, reason = "fixed protocol account list")]
#[must_use]
pub fn buy_disposable(
    sale: AccountWithMetadata,
    token_vault: AccountWithMetadata,
    collateral_vault: AccountWithMetadata,
    buyer_collateral_holding: AccountWithMetadata,
    buyer_token_holding: AccountWithMetadata,
    collateral_in: u128,
    min_tokens_out: u128,
    self_program_id: ProgramId,
) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
    let state = SaleState::try_from(&sale.account.data).expect("invalid sale state account");
    check_sale_and_vaults(&state, &sale, &token_vault, &collateral_vault, self_program_id);
    check_buyer_collateral(
        &buyer_collateral_holding,
        &collateral_vault,
        &state,
        "BuyDisposable: buyer collateral holding",
    );
    let payer = buy_payer(&buyer_collateral_holding);

    let mut outcome = apply_buy(state, collateral_in, min_tokens_out, None);
    // The fee is escrowed rather than swept: it is already inside the vault (the
    // collateral leg moves the gross amount), it is just not the creator's.
    outcome.state.treasury_owed = outcome
        .state
        .treasury_owed
        .checked_add(outcome.fee)
        .expect("treasury_owed overflow");

    let calls = disposable_transfers(
        &payer,
        &collateral_vault,
        &token_vault,
        &buyer_token_holding,
        sale.account_id,
        collateral_in,
        &outcome,
    );

    let mut sale_post = sale.account.clone();
    sale_post.data = Data::from(&outcome.state);

    let post_states = vec![
        AccountPostState::new(sale_post),
        AccountPostState::new(token_vault.account),
        AccountPostState::new(collateral_vault.account),
        AccountPostState::new(buyer_collateral_holding.account),
        AccountPostState::new(buyer_token_holding.account),
    ];
    (post_states, calls)
}

/// The two chained `token::Transfer`s of a disposable buy: `payer →
/// collateral_vault` for the full gross `collateral_in` (c_eff and fee together,
/// one leg - the fee is escrowed, not swept), then `token_vault → recipient`
/// for `tokens_out`. Exactly two, because each additional public account pinned
/// into the proof is another way for it to be invalidated before inclusion.
fn disposable_transfers(
    payer: &AccountWithMetadata,
    collateral_vault: &AccountWithMetadata,
    token_vault: &AccountWithMetadata,
    recipient: &AccountWithMetadata,
    sale_id: AccountId,
    collateral_in: u128,
    outcome: &BuyOutcome,
) -> Vec<ChainedCall> {
    // Anchored to the vaults, not to the payer - see `buy_transfers` for why
    // trusting `payer.account.program_owner` is exploitable.
    let collateral_program_id = collateral_vault.account.program_owner;
    let token_program_id = token_vault.account.program_owner;
    vec![
        // 1. payer -> collateral vault, gross. Like the public path, this leg does
        // not create the vault - `create_sale` initializes it, and the dispatch id
        // is read off the vault account.
        ChainedCall::new(
            collateral_program_id,
            vec![payer.clone(), authorized(collateral_vault)],
            &token_core::Instruction::Transfer { amount_to_transfer: collateral_in },
        )
        .with_pda_seeds(vec![compute_collateral_vault_pda_seed(sale_id)]),
        // 2. token vault (PDA) -> recipient (tokens_out); inits+claims it if default.
        ChainedCall::new(
            token_program_id,
            vec![authorized(token_vault), recipient.clone()],
            &token_core::Instruction::Transfer { amount_to_transfer: outcome.tokens_out },
        )
        .with_pda_seeds(vec![compute_token_vault_pda_seed(sale_id)]),
    ]
}
