//! WLEZ::Initialize - one-shot setup at program-deployment time.
//!
//! Claims the WLEZ vault PDA (so we own the escrow account) and creates
//! the WLEZ token definition via a chained `token::NewFungibleDefinition`
//! with `total_supply: 0`. The chained call passes both the definition
//! and an init-holding (also a PDA owned by this program) as PDA-
//! authorised, so the WLEZ program has signed for the creation. After
//! this runs the post-states are:
//!
//!   - vault.program_owner       = wlez_program_id    balance = 0
//!     data = encode_program_id(native_program_id)
//!   - definition.program_owner  = token_program_id   TokenDefinition::Fungible{name:"WLEZ", total_supply:0}
//!   - init_holding.program_owner= token_program_id   TokenHolding::Fungible{..., balance:0}
//!
//! Subsequent Wrap mints inflate `definition.total_supply` and `vault.balance`
//! in lock-step; Unwrap deflates them the same way.

use nssa_core::{
    account::{Account, AccountWithMetadata, Data},
    program::{AccountPostState, ChainedCall, Claim, ProgramId},
};
use token_core::{TokenDefinition, TokenHolding};
use wlez_core::{
    compute_wlez_definition_seed, compute_wlez_init_holding_seed, compute_wlez_vault_seed,
    get_wlez_definition_id, get_wlez_vault_id, WLEZ_NAME,
};

#[expect(clippy::too_many_arguments, reason = "fixed protocol account/param list")]
pub fn initialize(
    vault: AccountWithMetadata,
    definition: AccountWithMetadata,
    init_holding: AccountWithMetadata,
    reference_token_def: AccountWithMetadata,
    payer: AccountWithMetadata,
    wlez_program_id: ProgramId,
    expected_token_program_id: ProgramId,
    native_program_id: ProgramId,
) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
    // 1. Verify the PDAs match the seeds we'll sign with.
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

    // The WLEZ definition PDA is a fixed, claim-once address; the program
    // it is created under is whatever `reference_token_def` is owned by.
    // Pin that to the caller-supplied canonical token program so a
    // malicious reference definition can't redirect the WLEZ definition's
    // owning program at bootstrap (it would then control WLEZ mint
    // accounting for the launchpad's native-LEZ collateral).
    assert_eq!(
        reference_token_def.account.program_owner, expected_token_program_id,
        "reference_token_def must be owned by the expected token program"
    );
    let token_program_id = expected_token_program_id;

    // 2. Idempotency - if the vault is already claimed by this program
    //    AND the definition is already token-program-owned, this call is
    //    a no-op. Lets bootstrap re-run safely.
    if vault.account.program_owner == wlez_program_id
        && definition.account.program_owner == token_program_id
    {
        return (
            vec![
                AccountPostState::new(vault.account.clone()),
                AccountPostState::new(definition.account.clone()),
                AccountPostState::new(init_holding.account.clone()),
                AccountPostState::new(reference_token_def.account.clone()),
                AccountPostState::new(payer.account.clone()),
            ],
            vec![],
        );
    }

    // 3. Fresh-init path - all three must currently be default.
    assert_eq!(
        vault.account,
        Account::default(),
        "vault must be uninitialised on first Initialize"
    );
    assert_eq!(
        definition.account,
        Account::default(),
        "definition must be uninitialised on first Initialize"
    );
    assert_eq!(
        init_holding.account,
        Account::default(),
        "init_holding must be uninitialised on first Initialize"
    );

    // 4. Vault: claim at the WLEZ vault PDA via Claim::Pda(seed). The
    //    framework's validate_execution requires the post-state's
    //    program_owner to *stay* at DEFAULT_PROGRAM_ID - the framework
    //    rewrites it to wlez_program_id after the claim check passes
    //    (see lez/nssa/src/validated_state_diff.rs:211-239). Setting
    //    program_owner eagerly trips rule 4 of validate_execution
    //    (`pre.program_owner != post.program_owner` is forbidden) and
    //    the sequencer rejects the whole tx with ModifiedProgramOwner.
    // Record the trusted native/authenticated-transfer program id in the
    // vault's `data`. Wrap reads it back to pin its native-transfer leg, so
    // a submitter can't route the escrow through a no-op native program and
    // mint unbacked WLEZ. The native transfer only touches `balance`, so this
    // `data` survives every later Wrap (see authenticated_transfer::transfer).
    let mut vault_init = vault.account.clone();
    vault_init.data = Data::try_from(wlez_core::encode_program_id(&native_program_id).to_vec())
        .expect("32-byte native program id always fits within Data");
    let vault_post_claimed =
        AccountPostState::new_claimed(vault_init, Claim::Pda(compute_wlez_vault_seed()));

    // 5. Definition + init-holding will be written by the chained
    //    NewFungibleDefinition call; emit passthrough post-states.
    let post_states = vec![
        vault_post_claimed,
        AccountPostState::new(definition.account.clone()),
        AccountPostState::new(init_holding.account.clone()),
        AccountPostState::new(reference_token_def.account.clone()),
        // Payer signed the tx; echo its state so the framework's
        // account-count check matches the 5 accounts in the message.
        AccountPostState::new(payer.account.clone()),
    ];

    // 6. Chained NewFungibleDefinition with both PDA accounts marked
    //    authorised + matching `with_pda_seeds`. Order MUST line up:
    //    NewFungibleDefinition's account contract is
    //    `[definition, holding]`.
    let mut definition_auth = definition.clone();
    definition_auth.is_authorized = true;
    let mut init_holding_auth = init_holding.clone();
    init_holding_auth.is_authorized = true;

    let chained = ChainedCall::new(
        token_program_id,
        vec![definition_auth, init_holding_auth],
        &token_core::Instruction::NewFungibleDefinition {
            name: WLEZ_NAME.to_string(),
            total_supply: 0,
        },
    )
    .with_pda_seeds(vec![
        compute_wlez_definition_seed(),
        compute_wlez_init_holding_seed(),
    ]);

    (post_states, vec![chained])
}

/// Helper for tests + the dispatcher: build the expected definition
/// post-state after Initialize has run (definition lives at the WLEZ
/// definition PDA, owned by token program, with WLEZ TokenDefinition
/// data and total_supply=0).
pub fn expected_definition_after_init(
    definition: &AccountWithMetadata,
    token_program_id: ProgramId,
) -> Account {
    let mut a = definition.account.clone();
    a.program_owner = token_program_id;
    a.data = Data::from(&TokenDefinition::Fungible {
        name: WLEZ_NAME.to_string(),
        total_supply: 0,
        metadata_id: None,
    });
    a
}

/// Expected init-holding post-state after Initialize - total_supply is
/// 0 so the holding starts at 0 balance.
pub fn expected_init_holding_after_init(
    init_holding: &AccountWithMetadata,
    definition_id: nssa_core::account::AccountId,
    token_program_id: ProgramId,
) -> Account {
    let mut a = init_holding.account.clone();
    a.program_owner = token_program_id;
    a.data = Data::from(&TokenHolding::Fungible {
        definition_id,
        balance: 0,
    });
    a
}
