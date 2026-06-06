//! WLEZ::Unwrap - burn `amount` WLEZ from the user's holding, release
//! `amount` native LEZ from the vault to the user's native account.
//!
//! Why this is NOT symmetric with Wrap:
//!
//! Wrap chains `authenticated_transfer::transfer(user_native → vault)`,
//! and that works because `user_native` is *owned by* the auth-transfer
//! program - which is also the executing program for the chained call,
//! so `validate_execution`'s "only the owning program can decrease an
//! account's balance" rule is satisfied (executing == owning).
//!
//! Unwrap can't mirror that. The vault is owned by **this** (WLEZ)
//! program. If we chained `authenticated_transfer::transfer(vault →
//! user_native)`, the executing program would be `auth_transfer`, the
//! vault's owner is WLEZ, and the framework would reject with
//! `UnauthorizedBalanceDecrease` regardless of any
//! `is_authorized` flag or `with_pda_seeds` value - that flag only
//! satisfies the *guest's* `assert!(sender.is_authorized)`, not the
//! sequencer's ownership check.
//!
//! Solution: WLEZ mutates both `vault` and `user_native` *directly*
//! in its own post-states. WLEZ is the executing program AND the
//! vault's owner → decreasing vault is allowed. `user_native`'s
//! balance only INCREASES, which is unconstrained. The burn side
//! still chains `token::Burn` because the holding is owned by the
//! token program.
//!
//! End-state invariant: `vault.balance == definition.total_supply`
//! preserved by construction - both shrink by `amount`.

use nssa_core::{
    account::AccountWithMetadata,
    program::{AccountPostState, ChainedCall, ProgramId},
};
use wlez_core::{get_wlez_definition_id, get_wlez_vault_id};

pub fn unwrap(
    user_holding: AccountWithMetadata,
    definition: AccountWithMetadata,
    vault: AccountWithMetadata,
    user_native: AccountWithMetadata,
    amount: u128,
    wlez_program_id: ProgramId,
) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
    assert!(amount != 0, "Unwrap amount must be non-zero");

    // PDAs match.
    assert_eq!(
        vault.account_id,
        get_wlez_vault_id(&wlez_program_id),
        "vault account_id does not match WLEZ vault PDA"
    );
    assert_eq!(
        definition.account_id,
        get_wlez_definition_id(&wlez_program_id),
        "definition account_id does not match WLEZ definition PDA"
    );

    // User must have authorised this op - they're burning their WLEZ.
    assert!(
        user_holding.is_authorized,
        "User authorization is missing on the WLEZ holding (cannot burn)"
    );

    // The user's WLEZ holding must point at the WLEZ definition.
    let holding_def = token_core::TokenHolding::try_from(&user_holding.account.data)
        .expect("user_holding must hold a valid TokenHolding for the WLEZ definition")
        .definition_id();
    assert_eq!(
        holding_def, definition.account_id,
        "user_holding must point at the WLEZ definition"
    );

    // Vault must hold at least `amount`.
    assert!(
        vault.account.balance >= amount,
        "Vault balance is below the requested unwrap amount"
    );

    let token_program_id = definition.account.program_owner;

    // Mutate vault + user_native directly. WLEZ is the executing program
    // AND the vault's owner, so decreasing vault is allowed under the
    // sequencer's `validate_execution` ownership rule. user_native's
    // balance only increases, which is unrestricted.
    let mut vault_post = vault.account.clone();
    vault_post.balance = vault_post
        .balance
        .checked_sub(amount)
        .expect("vault balance must cover amount (asserted above)");
    let mut user_native_post = user_native.account.clone();
    user_native_post.balance = user_native_post
        .balance
        .checked_add(amount)
        .expect("user_native balance overflow on unwrap");

    let post_states = vec![
        // user_holding passes through; the chained burn writes the
        // burnt-balance post-state.
        AccountPostState::new(user_holding.account.clone()),
        // definition passes through; the chained burn writes the
        // shrunk-supply post-state.
        AccountPostState::new(definition.account.clone()),
        // vault is mutated directly by THIS program (allowed: we own it).
        AccountPostState::new(vault_post),
        // user_native gets the released native.
        AccountPostState::new(user_native_post),
    ];

    // Chained burn - token program is the executing program, holding is
    // owned by token program, so the burn is allowed under the ownership
    // rule. The user's tx signature authorises the holding.
    let call_burn = ChainedCall::new(
        token_program_id,
        vec![definition.clone(), user_holding.clone()],
        &token_core::Instruction::Burn {
            amount_to_burn: amount,
        },
    );

    (post_states, vec![call_burn])
}
