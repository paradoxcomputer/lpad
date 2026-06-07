//! WLEZ::Wrap - lock `amount` native LEZ into the vault, mint `amount`
//! WLEZ to the user's holding.
//!
//! Authorisations:
//!   - `user_native` - signs the parent tx (user has the keypair).
//!   - `vault` - destination of the native transfer; receiver does not
//!     need to authorise on `authenticated_transfer_program::transfer`.
//!   - `definition` - the mint authority; flipped is_authorized=true and
//!     backed by `with_pda_seeds(wlez_definition_seed)` so the token
//!     program's `assert!(definition_account.is_authorized)` passes.
//!
//! Chained call order: native transfer first (user must have the funds
//! before we mint anything), then mint.

use nssa_core::{
    account::AccountWithMetadata,
    program::{AccountPostState, ChainedCall, ProgramId},
};
use wlez_core::{
    compute_wlez_definition_seed, decode_program_id, get_wlez_definition_id, get_wlez_vault_id,
};

pub fn wrap(
    user_native: AccountWithMetadata,
    vault: AccountWithMetadata,
    definition: AccountWithMetadata,
    user_holding: AccountWithMetadata,
    amount: u128,
    wlez_program_id: ProgramId,
) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
    assert!(amount != 0, "Wrap amount must be non-zero");

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

    // User must have authorised this op (regular signer authority on
    // their native account). Without this the native transfer's
    // `assert!(sender.is_authorized)` would fire.
    assert!(
        user_native.is_authorized,
        "User authorization is missing on the native source account"
    );

    // SECURITY: pin the native-transfer leg to the program id recorded in
    // the vault at Initialize, NOT to `user_native.program_owner` (which the
    // submitter chooses). A no-op program owning the user's "native" account
    // would otherwise leave the vault uncredited while the mint below still
    // runs, minting unbacked WLEZ - and because such a leg leaves both
    // accounts unchanged, total native balance is conserved, so the
    // framework's MismatchedTotalBalance rule never fires. Requiring
    // user_native to be owned by the pinned native program forces the escrow
    // through the real authenticated-transfer program.
    let native_program_id = decode_program_id(vault.account.data.as_ref())
        .expect("WLEZ vault is missing its pinned native program id; re-run Initialize");
    assert_eq!(
        user_native.account.program_owner, native_program_id,
        "user_native must be owned by the pinned native (authenticated-transfer) program"
    );
    let token_program_id = definition.account.program_owner;

    // Sanity: user_holding must be initialised for the WLEZ definition.
    let holding_def = token_core::TokenHolding::try_from(&user_holding.account.data)
        .expect("user_holding must hold a valid TokenHolding for the WLEZ definition")
        .definition_id();
    assert_eq!(
        holding_def, definition.account_id,
        "user_holding must point at the WLEZ definition"
    );

    // Post-states echo the pre-states; the chained calls mutate them.
    let post_states = vec![
        AccountPostState::new(user_native.account.clone()),
        AccountPostState::new(vault.account.clone()),
        AccountPostState::new(definition.account.clone()),
        AccountPostState::new(user_holding.account.clone()),
    ];

    // 1) Native transfer: user_native -> vault, amount LEZ.
    //    `authenticated_transfer_program::transfer` instruction data is
    //    just the u128 amount (see lez/program_methods/.../authenticated_transfer.rs).
    let call_native = ChainedCall::new(
        native_program_id,
        vec![user_native.clone(), vault.clone()],
        &amount,
    );

    // 2) Mint: definition (PDA-auth) -> user_holding.
    let mut definition_auth = definition.clone();
    definition_auth.is_authorized = true;
    let call_mint = ChainedCall::new(
        token_program_id,
        vec![definition_auth, user_holding.clone()],
        &token_core::Instruction::Mint {
            amount_to_mint: amount,
        },
    )
    .with_pda_seeds(vec![compute_wlez_definition_seed()]);

    (post_states, vec![call_native, call_mint])
}
