//! `BuyAta` - LBP buy where the **buyer side uses Associated Token Accounts**
//! (RFP-016 Func #9: ATAs for all token interactions, per LP-0014 / RFP-008).
//!
//! Same idiom as the bonding curve's `ata::buy_ata`: the buyer's collateral leg
//! is dispatched through the ATA program (which internally chains a
//! `token::Transfer` from the ATA-PDA into the collateral vault), and the token
//! leg is the vault-PDA-authorized `token::Transfer` used by `buy::buy`. The
//! keypair path (`buy::buy`) stays intact. LBP has no per-swap fee (the protocol
//! fee is taken at close), so there is no fee leg. Both vaults are pre-seeded at
//! `CreateSale`, so the `ata::Transfer` collateral leg always has an initialized
//! recipient - which `util::assert_ata_recipient` requires, since the canonical
//! upstream ATA program would otherwise auto-create a default one.

use lbp_core::{compute_token_vault_pda_seed, is_open_allowlist, read_fungible, PoolState};
use lee_core::{
    account::{AccountWithMetadata, Data},
    program::{AccountPostState, ChainedCall, ProgramId},
};

use crate::buy::{apply_buy, check_accounts};
use crate::util::{assert_ata_recipient, authorized};

/// `BuyAta` - public LBP buy, priced at the on-chain clock time, with the
/// buyer's collateral and tokens using ATAs.
///
/// Account order: `[pool, token_vault, collateral_vault, owner,
/// buyer_collateral_ata, buyer_token_ata, clock]`.
///
/// Chained legs:
///   1. `ata::Transfer`  owner: `buyer_collateral_ata → collateral_vault` (`collateral_in`)
///   2. `token::Transfer` `token_vault`(PDA) → `buyer_token_ata` (`tokens_out`)
#[expect(clippy::too_many_arguments, reason = "fixed protocol account list")]
#[must_use]
pub fn buy_ata(
    pool: AccountWithMetadata,
    token_vault: AccountWithMetadata,
    collateral_vault: AccountWithMetadata,
    owner: AccountWithMetadata,
    buyer_collateral_ata: AccountWithMetadata,
    buyer_token_ata: AccountWithMetadata,
    collateral_in: u128,
    min_tokens_out: u128,
    ata_program_id: ProgramId,
    self_program_id: ProgramId,
    clock_ts: i64,
    clock_block_id: u64,
) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
    let state = PoolState::try_from(&pool.account.data).expect("invalid pool state account");
    check_accounts(&state, &pool, &token_vault, &collateral_vault, self_program_id);
    // SECURITY: the ATA program is submitter-chosen, so it must match the one
    // pinned at creation. A substitute (e.g. no-op) program would skip the real
    // collateral `token::Transfer` while leg 2 still delivers tokens - draining
    // the pool. LBP has no fee leg, so nothing else cross-checks the deposit.
    assert_eq!(
        ata_program_id, state.ata_program_id,
        "ata_program_id does not match the program pinned at pool creation"
    );
    // SECURITY: a gated pool may only be bought via BuyGated (see `buy::buy`).
    assert!(
        is_open_allowlist(&state.allowlist_root),
        "this pool is allowlist-gated; use BuyGated with an inclusion proof"
    );

    assert!(owner.is_authorized, "buyer (ATA owner) must authorize the buy");
    let (collateral_def, _) = read_fungible(&buyer_collateral_ata, "BuyAta: buyer collateral ATA");
    assert_eq!(
        collateral_def, state.collateral_definition_id,
        "buyer collateral ATA token does not match the pool's collateral definition"
    );
    // The recipient of leg 1. The canonical upstream ATA program does not police
    // its recipient (it auto-creates a default one), so this is the only place the
    // contract is enforced - see `util::assert_ata_recipient`.
    assert_ata_recipient(
        &collateral_vault,
        &buyer_collateral_ata,
        collateral_def,
        "BuyAta collateral vault",
    );

    let t_ms = u64::try_from(clock_ts.max(0)).expect("clock must be non-negative");
    let outcome = apply_buy(state, collateral_in, min_tokens_out, t_ms, clock_block_id, true);
    let pool_id = pool.account_id;

    let mut owner_auth = owner.clone();
    owner_auth.is_authorized = true;
    // The token leg is owned by the token vault's program, which may differ from
    // the collateral vault's program if the two tokens live under distinct programs.
    let token_vault_program_id = token_vault.account.program_owner;
    let calls = vec![
        // 1. buyer collateral ATA -> collateral vault (via the ATA program).
        ChainedCall::new(
            ata_program_id,
            vec![owner_auth, buyer_collateral_ata.clone(), collateral_vault.clone()],
            &ata_core::Instruction::Transfer {
                ata_program_id,
                amount: collateral_in,
            },
        ),
        // 2. token vault (PDA) -> buyer token ATA.
        ChainedCall::new(
            token_vault_program_id,
            vec![authorized(&token_vault), buyer_token_ata.clone()],
            &token_core::Instruction::Transfer { amount_to_transfer: outcome.tokens_out },
        )
        .with_pda_seeds(vec![compute_token_vault_pda_seed(pool_id)]),
    ];

    let mut pool_post = pool.account.clone();
    pool_post.data = Data::from(&outcome.state);
    let post_states = vec![
        AccountPostState::new(pool_post),
        AccountPostState::new(token_vault.account),
        AccountPostState::new(collateral_vault.account),
        AccountPostState::new(owner.account),
        AccountPostState::new(buyer_collateral_ata.account),
        AccountPostState::new(buyer_token_ata.account),
    ];
    (post_states, calls)
}
