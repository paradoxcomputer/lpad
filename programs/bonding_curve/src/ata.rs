//! `BuyAta` / `SellAta` - buy and sell where the **user side uses Associated
//! Token Accounts** (RFP-015 Func #7: ATAs for all token interactions, per
//! LP-0014 / RFP-008).
//!
//! ATAs are deterministic per `(owner, definition)` (see `ata_core`); they are
//! token holdings owned by the *token* program, but the *ATA program* authorizes
//! spends from them via PDA seeds when the account's named **owner** signs the
//! outer transaction. So the user's collateral/token leg is dispatched through
//! the ATA program (`ata::Transfer`, which internally chains a
//! `token::Transfer` from the ATA-PDA); the vault legs are the same
//! vault-PDA-authorized `token::Transfer`s used by the keypair path.
//!
//! Chained calls execute depth-first (the ATA program's internal transfer fully
//! applies to the running state-diff before the next sibling call), so the
//! fee-sweep / pre-state shifts below match the running diff exactly.
//!
//! The keypair-side paths (`buy::buy`, `sell::sell`) stay intact; this only adds
//! an ATA-routed variant. Both vaults are pre-seeded at `CreateSale`, so the
//! `ata::Transfer` legs always have an initialized recipient - which
//! [`crate::util::assert_ata_recipient`] requires, since the canonical upstream
//! ATA program would otherwise auto-create a default one.

use bonding_curve_core::{
    compute_collateral_vault_pda_seed, compute_token_vault_pda_seed, read_fungible, SaleState,
};
use lee_core::{
    account::{AccountWithMetadata, Data},
    program::{AccountPostState, ChainedCall, ProgramId},
};

use crate::buy::{apply_buy, check_accounts};
use crate::sell::{apply_sell, sell_collateral_payout};
use crate::util::{assert_ata_recipient, authorized, shift_balance};

/// `BuyAta` - public buy where the buyer's collateral and tokens use ATAs.
///
/// Account order: `[sale, token_vault, collateral_vault, treasury, owner,
/// buyer_collateral_ata, buyer_token_ata, clock]`.
///
/// Chained legs:
///   1. `ata::Transfer`  owner: `buyer_collateral_ata → collateral_vault` (full `collateral_in`)
///   2. `token::Transfer` `collateral_vault`(PDA) → `treasury` (`fee`, if any)
///   3. `token::Transfer` `token_vault`(PDA) → `buyer_token_ata` (`tokens_out`)
#[expect(clippy::too_many_arguments, reason = "fixed protocol account list")]
#[must_use]
pub fn buy_ata(
    sale: AccountWithMetadata,
    token_vault: AccountWithMetadata,
    collateral_vault: AccountWithMetadata,
    treasury: AccountWithMetadata,
    owner: AccountWithMetadata,
    buyer_collateral_ata: AccountWithMetadata,
    buyer_token_ata: AccountWithMetadata,
    collateral_in: u128,
    min_tokens_out: u128,
    ata_program_id: ProgramId,
    self_program_id: ProgramId,
    clock_ts: i64,
) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
    let state = SaleState::try_from(&sale.account.data).expect("invalid sale state account");
    check_accounts(&state, &sale, &token_vault, &collateral_vault, &treasury, self_program_id);
    // SECURITY: the ATA program is submitter-chosen, so it must match the one
    // pinned at creation. Otherwise leg 1 could be dispatched to a no-op program
    // that never moves collateral into the vault, while legs 2/3 still pay out -
    // draining the vault. (On fee==0 sales nothing else cross-checks the deposit.)
    assert_eq!(
        ata_program_id, state.ata_program_id,
        "ata_program_id does not match the program pinned at sale creation"
    );

    assert!(owner.is_authorized, "buyer (ATA owner) must authorize the buy");
    // The buyer's collateral ATA must hold the sale's collateral token.
    let (collateral_def, _) = read_fungible(&buyer_collateral_ata, "BuyAta: buyer collateral ATA");
    assert_eq!(
        collateral_def, state.collateral_definition_id,
        "buyer collateral ATA token does not match the sale's collateral definition"
    );
    // The recipient of leg 1. Upstream's ATA program does not police its
    // recipient (it auto-creates a default one), so this is the only place the
    // contract is enforced - see `util::assert_ata_recipient`.
    assert_ata_recipient(
        &collateral_vault,
        &buyer_collateral_ata,
        collateral_def,
        "BuyAta collateral vault",
    );

    let token_program_id = collateral_vault.account.program_owner;
    let outcome = apply_buy(state, collateral_in, min_tokens_out, Some(clock_ts));
    let sale_id = sale.account_id;

    let mut calls = Vec::with_capacity(3);

    // 1. buyer collateral ATA -> collateral vault (full gross `collateral_in`),
    //    dispatched through the ATA program (owner-authorized PDA spend; the ATA
    //    program internally chains the token::Transfer).
    let mut owner_auth = owner.clone();
    owner_auth.is_authorized = true;
    calls.push(ChainedCall::new(
        ata_program_id,
        vec![owner_auth, buyer_collateral_ata.clone(), collateral_vault.clone()],
        &ata_core::Instruction::Transfer {
            ata_program_id,
            amount: collateral_in,
        },
    ));

    // 2. collateral vault (PDA, now holding +collateral_in) -> treasury (fee).
    if outcome.fee > 0 {
        calls.push(
            ChainedCall::new(
                token_program_id,
                vec![
                    shift_balance(&authorized(&collateral_vault), collateral_in, true),
                    treasury.clone(),
                ],
                &token_core::Instruction::Transfer { amount_to_transfer: outcome.fee },
            )
            .with_pda_seeds(vec![compute_collateral_vault_pda_seed(sale_id)]),
        );
    }

    // 3. token vault (PDA) -> buyer token ATA (tokens_out). The token leg is owned
    //    by the token vault's program, which may differ from the collateral one.
    let token_vault_program_id = token_vault.account.program_owner;
    calls.push(
        ChainedCall::new(
            token_vault_program_id,
            vec![authorized(&token_vault), buyer_token_ata.clone()],
            &token_core::Instruction::Transfer { amount_to_transfer: outcome.tokens_out },
        )
        .with_pda_seeds(vec![compute_token_vault_pda_seed(sale_id)]),
    );

    let mut sale_post = sale.account.clone();
    sale_post.data = Data::from(&outcome.state);

    let post_states = vec![
        AccountPostState::new(sale_post),
        AccountPostState::new(token_vault.account),
        AccountPostState::new(collateral_vault.account),
        AccountPostState::new(treasury.account),
        AccountPostState::new(owner.account),
        AccountPostState::new(buyer_collateral_ata.account),
        AccountPostState::new(buyer_token_ata.account),
    ];
    (post_states, calls)
}

/// `SellAta` - public sell where the seller's tokens and collateral use ATAs.
/// Reverts if the sale is one-directional. Mirrors `sell::sell` but routes the
/// seller's token return through the ATA program and pays out into the seller's
/// collateral ATA.
///
/// Account order: `[sale, token_vault, collateral_vault, treasury, owner,
/// seller_token_ata, seller_collateral_ata, clock]`.
///
/// Chained legs:
///   1. `ata::Transfer`  owner: `seller_token_ata → token_vault` (`tokens_in`)
///   2. `token::Transfer` `collateral_vault`(PDA) → `seller_collateral_ata` (`to_seller`)
///   3. `token::Transfer` `collateral_vault`(PDA) → `treasury` (`fee`, if any)
#[expect(clippy::too_many_arguments, reason = "fixed protocol account list")]
#[must_use]
pub fn sell_ata(
    sale: AccountWithMetadata,
    token_vault: AccountWithMetadata,
    collateral_vault: AccountWithMetadata,
    treasury: AccountWithMetadata,
    owner: AccountWithMetadata,
    seller_token_ata: AccountWithMetadata,
    seller_collateral_ata: AccountWithMetadata,
    tokens_in: u128,
    min_collateral_out: u128,
    ata_program_id: ProgramId,
    self_program_id: ProgramId,
    clock_ts: i64,
) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
    let state = SaleState::try_from(&sale.account.data).expect("invalid sale state account");
    check_accounts(&state, &sale, &token_vault, &collateral_vault, &treasury, self_program_id);
    // SECURITY: see `buy_ata` - a substitute ATA program would let leg 1 no-op
    // (seller never returns tokens) while the collateral payout (leg 2) still runs.
    assert_eq!(
        ata_program_id, state.ata_program_id,
        "ata_program_id does not match the program pinned at sale creation"
    );
    assert!(owner.is_authorized, "seller (ATA owner) must authorize the sell");

    // The seller's token ATA must hold the sale's project token.
    let (token_def, _) = read_fungible(&seller_token_ata, "SellAta: seller token ATA");
    assert_eq!(
        token_def, state.token_definition_id,
        "seller token ATA does not match the sale's project token definition"
    );
    // The recipient of leg 1 - see the note in `buy_ata`.
    assert_ata_recipient(
        &token_vault,
        &seller_token_ata,
        token_def,
        "SellAta token vault",
    );

    let outcome = apply_sell(state, tokens_in, min_collateral_out, clock_ts);
    let sale_id = sale.account_id;
    let mut calls = Vec::with_capacity(3);

    // 1. seller token ATA -> token vault (return tokens), via the ATA program.
    let mut owner_auth = owner.clone();
    owner_auth.is_authorized = true;
    calls.push(ChainedCall::new(
        ata_program_id,
        vec![owner_auth, seller_token_ata.clone(), token_vault.clone()],
        &ata_core::Instruction::Transfer {
            ata_program_id,
            amount: tokens_in,
        },
    ));
    // 2 + 3. collateral payout to the seller's collateral ATA, then the fee sweep
    //        to the treasury (shared with the keypair `sell` path).
    calls.extend(sell_collateral_payout(
        &collateral_vault,
        &seller_collateral_ata,
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
        AccountPostState::new(owner.account),
        AccountPostState::new(seller_token_ata.account),
        AccountPostState::new(seller_collateral_ata.account),
    ];
    (post_states, calls)
}
