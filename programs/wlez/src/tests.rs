//! Pure-function unit tests - exercise the WLEZ program without the
//! zkVM. Run with `RISC0_DEV_MODE=1 cargo test -p wlez_program`.
//!
//! Coverage:
//!   - Initialize emits the expected vault claim + chained NewFungibleDefinition
//!   - Initialize is idempotent (second call returns passthrough)
//!   - Wrap emits the expected native transfer + chained mint
//!   - Wrap rejects zero amounts and mismatched holdings
//!   - Unwrap emits the expected burn + PDA-authorised native release
//!   - Unwrap rejects amounts above vault balance
//!
//! The framework's own `validate_execution` (state-preservation,
//! nonce, balance) is exercised by the integration tests; here we
//! only verify the shape of what each instruction produces.

use nssa_core::{
    account::{Account, AccountId, AccountWithMetadata, Data, Nonce},
    program::{ChainedCall, Claim, ProgramId},
};
use token_core::{TokenDefinition, TokenHolding};
use wlez_core::{
    compute_wlez_definition_seed, compute_wlez_vault_seed, get_wlez_definition_id,
    get_wlez_vault_id, Instruction, WLEZ_NAME,
};

const WLEZ_PROGRAM_ID: ProgramId = [3u32; 8];
const TOKEN_PROGRAM_ID: ProgramId = [2u32; 8];
// Must equal the pinned canonical native id - Initialize now asserts it.
const NATIVE_PROGRAM_ID: ProgramId = wlez_core::NATIVE_PROGRAM_ID;

fn user_id() -> AccountId {
    AccountId::new([0xAAu8; 32])
}
fn user_holding_id() -> AccountId {
    AccountId::new([0xBBu8; 32])
}
fn reference_token_def_id() -> AccountId {
    AccountId::new([0xCCu8; 32])
}

fn default_at(id: AccountId) -> AccountWithMetadata {
    AccountWithMetadata {
        account: Account::default(),
        is_authorized: false,
        account_id: id,
    }
}

fn vault_default() -> AccountWithMetadata {
    default_at(get_wlez_vault_id(&WLEZ_PROGRAM_ID))
}
fn definition_default() -> AccountWithMetadata {
    default_at(get_wlez_definition_id(&WLEZ_PROGRAM_ID))
}
fn init_holding_default() -> AccountWithMetadata {
    default_at(wlez_core::get_wlez_init_holding_id(&WLEZ_PROGRAM_ID))
}
fn payer_default() -> AccountWithMetadata {
    AccountWithMetadata {
        account: Account {
            program_owner: NATIVE_PROGRAM_ID,
            balance: 1_000_000,
            data: Data::default(),
            nonce: Nonce(0),
        },
        is_authorized: true,
        account_id: AccountId::new([0xEEu8; 32]),
    }
}

fn reference_token_def() -> AccountWithMetadata {
    AccountWithMetadata {
        account: Account {
            program_owner: TOKEN_PROGRAM_ID,
            balance: 0,
            data: Data::from(&TokenDefinition::Fungible {
                name: "REF".to_string(),
                total_supply: 1_000_000,
                metadata_id: None,
            }),
            nonce: Nonce(0),
        },
        is_authorized: false,
        account_id: reference_token_def_id(),
    }
}

fn user_native_with(balance: u128, authorized: bool) -> AccountWithMetadata {
    AccountWithMetadata {
        account: Account {
            program_owner: NATIVE_PROGRAM_ID,
            balance,
            data: Data::default(),
            nonce: Nonce(0),
        },
        is_authorized: authorized,
        account_id: user_id(),
    }
}

fn vault_with(balance: u128) -> AccountWithMetadata {
    AccountWithMetadata {
        account: Account {
            program_owner: WLEZ_PROGRAM_ID,
            balance,
            // Initialize stores the pinned native program id in the vault's
            // data; Wrap reads it back to authorise the native-transfer leg.
            data: Data::try_from(wlez_core::encode_program_id(&NATIVE_PROGRAM_ID).to_vec())
                .expect("32-byte native id fits in Data"),
            nonce: Nonce(0),
        },
        is_authorized: false,
        account_id: get_wlez_vault_id(&WLEZ_PROGRAM_ID),
    }
}

fn definition_initialized(total_supply: u128) -> AccountWithMetadata {
    AccountWithMetadata {
        account: Account {
            program_owner: TOKEN_PROGRAM_ID,
            balance: 0,
            data: Data::from(&TokenDefinition::Fungible {
                name: WLEZ_NAME.to_string(),
                total_supply,
                metadata_id: None,
            }),
            nonce: Nonce(0),
        },
        is_authorized: false,
        account_id: get_wlez_definition_id(&WLEZ_PROGRAM_ID),
    }
}

fn user_holding_with(balance: u128, authorized: bool) -> AccountWithMetadata {
    AccountWithMetadata {
        account: Account {
            program_owner: TOKEN_PROGRAM_ID,
            balance: 0,
            data: Data::from(&TokenHolding::Fungible {
                definition_id: get_wlez_definition_id(&WLEZ_PROGRAM_ID),
                balance,
            }),
            nonce: Nonce(0),
        },
        is_authorized: authorized,
        account_id: user_holding_id(),
    }
}

// ── Initialize ──────────────────────────────────────────────────────

#[test]
fn initialize_claims_vault_and_chains_new_definition() {
    let (post_states, chained) = crate::initialize::initialize(
        vault_default(),
        definition_default(),
        init_holding_default(),
        reference_token_def(),
        payer_default(),
        WLEZ_PROGRAM_ID,
        TOKEN_PROGRAM_ID,
        NATIVE_PROGRAM_ID,
    );
    // Post-states: vault (claimed), definition (passthrough), init_holding (passthrough), ref-token-def (passthrough), payer (passthrough).
    assert_eq!(post_states.len(), 5);
    assert_eq!(
        post_states[0].required_claim(),
        Some(Claim::Pda(compute_wlez_vault_seed())),
        "vault must be claimed at the WLEZ vault PDA"
    );
    // Vault post-state's program_owner must STAY at DEFAULT_PROGRAM_ID
    // here - the framework rewrites it to wlez_program_id after the
    // Claim::Pda check passes (see lez/nssa/src/validated_state_diff.rs).
    // Setting it eagerly violates validate_execution rule 4 and the
    // sequencer rejects the tx with ModifiedProgramOwner.
    assert_eq!(
        post_states[0].account().program_owner,
        nssa_core::program::DEFAULT_PROGRAM_ID,
        "vault post-state must keep DEFAULT_PROGRAM_ID; framework claims via Claim::Pda"
    );
    // The vault records the pinned native program id in its data so Wrap can
    // authorise its native-transfer leg against it.
    assert_eq!(
        wlez_core::decode_program_id(post_states[0].account().data.as_ref()),
        Some(NATIVE_PROGRAM_ID),
        "vault post-state must record the pinned native program id"
    );

    // One chained call to NewFungibleDefinition on the token program.
    assert_eq!(chained.len(), 1);

    let mut definition_auth = definition_default();
    definition_auth.is_authorized = true;
    let mut init_holding_auth = init_holding_default();
    init_holding_auth.is_authorized = true;
    let expected = ChainedCall::new(
        TOKEN_PROGRAM_ID,
        vec![definition_auth, init_holding_auth],
        &token_core::Instruction::NewFungibleDefinition {
            name: WLEZ_NAME.to_string(),
            total_supply: 0,
        },
    )
    .with_pda_seeds(vec![
        compute_wlez_definition_seed(),
        wlez_core::compute_wlez_init_holding_seed(),
    ]);
    assert_eq!(chained[0], expected);
}

#[test]
fn initialize_is_idempotent_after_first_run() {
    // Pretend Initialize already ran: vault is wlez-owned and definition
    // is token-program-owned. A second call must be a no-op (no chained
    // call) so re-running the bootstrap doesn't double-create.
    let vault_post = vault_with(0);
    let definition_post = definition_initialized(0);
    let mut init_holding_post = init_holding_default();
    init_holding_post.account.program_owner = TOKEN_PROGRAM_ID;
    init_holding_post.account.data = Data::from(&TokenHolding::Fungible {
        definition_id: get_wlez_definition_id(&WLEZ_PROGRAM_ID),
        balance: 0,
    });
    let (_, chained) = crate::initialize::initialize(
        vault_post,
        definition_post,
        init_holding_post,
        reference_token_def(),
        payer_default(),
        WLEZ_PROGRAM_ID,
        TOKEN_PROGRAM_ID,
        NATIVE_PROGRAM_ID,
    );
    assert!(chained.is_empty(), "Second Initialize must emit no chained calls");
}

#[test]
#[should_panic(expected = "vault account_id does not match WLEZ vault PDA")]
fn initialize_rejects_wrong_vault_id() {
    let mut wrong_vault = vault_default();
    wrong_vault.account_id = AccountId::new([0xDEu8; 32]);
    let _ = crate::initialize::initialize(
        wrong_vault,
        definition_default(),
        init_holding_default(),
        reference_token_def(),
        payer_default(),
        WLEZ_PROGRAM_ID,
        TOKEN_PROGRAM_ID,
        NATIVE_PROGRAM_ID,
    );
}

#[test]
#[should_panic(expected = "native_program_id must be the canonical")]
fn initialize_rejects_non_canonical_native_program() {
    // E1: a permissionless / front-run Initialize that pins a no-op "native"
    // program must be rejected, so Wrap can never trust an attacker-chosen
    // native id and mint unbacked WLEZ.
    let _ = crate::initialize::initialize(
        vault_default(),
        definition_default(),
        init_holding_default(),
        reference_token_def(),
        payer_default(),
        WLEZ_PROGRAM_ID,
        TOKEN_PROGRAM_ID,
        [9u32; 8], // attacker's EVIL program id, != the canonical native program
    );
}

// ── Wrap ────────────────────────────────────────────────────────────

#[test]
fn wrap_emits_native_transfer_then_mint() {
    let (post_states, chained) = crate::wrap::wrap(
        user_native_with(/*balance*/ 1000, /*authorized*/ true),
        vault_with(0),
        definition_initialized(0),
        user_holding_with(0, false),
        /*amount*/ 250,
        WLEZ_PROGRAM_ID,
    );
    assert_eq!(post_states.len(), 4);
    assert_eq!(chained.len(), 2);

    // 1st chained call: native transfer (instruction data = u128 amount).
    let native_call = ChainedCall::new(
        NATIVE_PROGRAM_ID,
        vec![
            user_native_with(1000, true),
            vault_with(0),
        ],
        &250u128,
    );
    assert_eq!(chained[0], native_call);

    // 2nd chained call: token::Mint with definition PDA-authorised.
    let mut definition_auth = definition_initialized(0);
    definition_auth.is_authorized = true;
    let mint_call = ChainedCall::new(
        TOKEN_PROGRAM_ID,
        vec![definition_auth, user_holding_with(0, false)],
        &token_core::Instruction::Mint {
            amount_to_mint: 250,
        },
    )
    .with_pda_seeds(vec![compute_wlez_definition_seed()]);
    assert_eq!(chained[1], mint_call);
}

#[test]
#[should_panic(expected = "Wrap amount must be non-zero")]
fn wrap_rejects_zero() {
    let _ = crate::wrap::wrap(
        user_native_with(1000, true),
        vault_with(0),
        definition_initialized(0),
        user_holding_with(0, false),
        0,
        WLEZ_PROGRAM_ID,
    );
}

#[test]
#[should_panic(expected = "User authorization is missing on the native source account")]
fn wrap_rejects_unauth_user() {
    let _ = crate::wrap::wrap(
        user_native_with(1000, /*authorized*/ false),
        vault_with(0),
        definition_initialized(0),
        user_holding_with(0, false),
        250,
        WLEZ_PROGRAM_ID,
    );
}

#[test]
#[should_panic(expected = "user_holding must point at the WLEZ definition")]
fn wrap_rejects_wrong_holding_definition() {
    let mut wrong_holding = user_holding_with(0, false);
    wrong_holding.account.data = Data::from(&TokenHolding::Fungible {
        definition_id: AccountId::new([0xFFu8; 32]), // not the WLEZ def
        balance: 0,
    });
    let _ = crate::wrap::wrap(
        user_native_with(1000, true),
        vault_with(0),
        definition_initialized(0),
        wrong_holding,
        250,
        WLEZ_PROGRAM_ID,
    );
}

#[test]
#[should_panic(expected = "user_native must be owned by the pinned native")]
fn wrap_rejects_foreign_native_program() {
    // The vault pins NATIVE_PROGRAM_ID; a user_native owned by some OTHER
    // program (e.g. an attacker's no-op "native" program that would skip the
    // real escrow) must be rejected before any WLEZ is minted.
    let mut foreign_native = user_native_with(/*balance*/ 1000, /*authorized*/ true);
    foreign_native.account.program_owner = [9u32; 8];
    let _ = crate::wrap::wrap(
        foreign_native,
        vault_with(0),
        definition_initialized(0),
        user_holding_with(0, false),
        250,
        WLEZ_PROGRAM_ID,
    );
}

// ── Unwrap ──────────────────────────────────────────────────────────

#[test]
fn unwrap_emits_burn_and_direct_vault_native_post_states() {
    // Note: the native release is NOT a chained call - see the comment
    // block at the top of unwrap.rs. WLEZ directly mutates vault and
    // user_native in its own post_states because the sequencer's
    // ownership rule blocks `auth_transfer` from decreasing a
    // WLEZ-owned vault. The chained burn is unchanged.
    let (post_states, chained) = crate::unwrap::unwrap(
        user_holding_with(/*balance*/ 250, /*authorized*/ true),
        definition_initialized(/*total_supply*/ 250),
        vault_with(/*balance*/ 250),
        user_native_with(0, false),
        /*amount*/ 250,
        WLEZ_PROGRAM_ID,
    );
    assert_eq!(post_states.len(), 4);
    assert_eq!(chained.len(), 1);

    // Only chained call: Burn.
    let burn_call = ChainedCall::new(
        TOKEN_PROGRAM_ID,
        vec![
            definition_initialized(250),
            user_holding_with(250, true),
        ],
        &token_core::Instruction::Burn { amount_to_burn: 250 },
    );
    assert_eq!(chained[0], burn_call);

    // Direct post-state mutations:
    //   post_states[2] = vault with balance 0 (250 - 250)
    //   post_states[3] = user_native with balance 250 (0 + 250)
    assert_eq!(
        post_states[2].account().balance,
        0,
        "vault must be drained by `amount`"
    );
    assert_eq!(
        post_states[3].account().balance,
        250,
        "user_native must receive `amount`"
    );
    // Sanity - neither account changes program_owner.
    assert_eq!(post_states[2].account().program_owner, WLEZ_PROGRAM_ID);
    assert_eq!(post_states[3].account().program_owner, NATIVE_PROGRAM_ID);
}

#[test]
#[should_panic(expected = "Vault balance is below the requested unwrap amount")]
fn unwrap_rejects_under_collateralised_vault() {
    let _ = crate::unwrap::unwrap(
        user_holding_with(1000, true),
        definition_initialized(1000),
        vault_with(/*balance*/ 100), // <- insufficient
        user_native_with(0, false),
        /*amount*/ 500,
        WLEZ_PROGRAM_ID,
    );
}

#[test]
#[should_panic(expected = "User authorization is missing on the WLEZ holding")]
fn unwrap_rejects_unauth_user_holding() {
    let _ = crate::unwrap::unwrap(
        user_holding_with(250, /*authorized*/ false),
        definition_initialized(250),
        vault_with(250),
        user_native_with(0, false),
        250,
        WLEZ_PROGRAM_ID,
    );
}

#[test]
fn instruction_enum_roundtrips_serde() {
    // Spot-check the serde round-trip for the instruction enum so the
    // dispatcher's `serde_json::from_slice` doesn't silently break on a
    // refactor.
    for ix in [
        Instruction::Initialize {
            token_program_id: TOKEN_PROGRAM_ID,
            native_program_id: NATIVE_PROGRAM_ID,
        },
        Instruction::Wrap { amount: 42 },
        Instruction::Unwrap { amount: u128::MAX / 2 },
    ] {
        let s = serde_json::to_string(&ix).expect("serialize");
        let back: Instruction = serde_json::from_str(&s).expect("roundtrip");
        // We don't impl PartialEq on Instruction; compare via re-serialisation.
        assert_eq!(s, serde_json::to_string(&back).unwrap());
    }
}
