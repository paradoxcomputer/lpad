//! End-to-end integration tests for the **WLEZ** wrap/unwrap path - the
//! native-LEZ ⇄ token bridge that lets the AMM take native LEZ as a pool side.
//!
//! Unlike the pure-function unit tests in `programs/wlez/src/tests.rs` (which
//! only check the *shape* of each instruction's post-states + chained calls),
//! these drive the real framework machinery via `transition_from_public_transaction`:
//! the chained `token::Mint` / native `authenticated_transfer::transfer` in Wrap,
//! the chained `token::Burn` in Unwrap, the tx-signer authorization that backs
//! each, and - most importantly - Unwrap's *direct* vault-balance mutation, which
//! relies on `validate_execution` rule 5 (a decrease is permitted only because the
//! WLEZ program is both the executing program and the vault's owner) plus rule 8
//! (total balance preserved: vault `-amount`, user_native `+amount`). A refactor
//! that broke any of those would still pass the unit tests but is caught here.
//!
//! The native (`authenticated_transfer`) program is registered automatically by
//! `V03State::new_with_genesis_accounts`; native accounts carry its program id as
//! their owner. Run with `RISC0_DEV_MODE=1`.

use nssa::program::Program;
use nssa::{
    program_deployment_transaction::{self, ProgramDeploymentTransaction},
    public_transaction, PrivateKey, PublicKey, PublicTransaction, V03State,
};
use nssa_core::account::{Account, AccountId, Data, Nonce};
use nssa_core::program::ProgramId;
use token_core::{TokenDefinition, TokenHolding};
use wlez_core::{
    get_wlez_definition_id, get_wlez_init_holding_id, get_wlez_vault_id, Instruction, WLEZ_NAME,
};

fn token() -> ProgramId {
    token_methods::TOKEN_ID
}
fn wlez() -> ProgramId {
    wlez_methods::WLEZ_ID
}
/// The native-LEZ (`authenticated_transfer`) program, registered at genesis.
fn native() -> ProgramId {
    Program::authenticated_transfer_program().id()
}
fn id_of(key: &PrivateKey) -> AccountId {
    AccountId::from(&PublicKey::new_from_private_key(key))
}

/// A token-program-owned fungible holding for `def` with the given balance.
fn fungible(def: AccountId, balance: u128) -> Account {
    Account {
        program_owner: token(),
        balance: 0,
        data: Data::from(&TokenHolding::Fungible { definition_id: def, balance }),
        nonce: Nonce(0),
    }
}
/// A native (authenticated_transfer-owned) account holding `balance` LEZ.
fn native_account(balance: u128) -> Account {
    Account {
        program_owner: native(),
        balance,
        data: Data::default(),
        nonce: Nonce(0),
    }
}
/// A real token definition account - used as the `reference_token_def` whose
/// `program_owner` tells `Initialize` which token program to chain into.
fn token_def_account(name: &str) -> Account {
    Account {
        program_owner: token(),
        balance: 0,
        data: Data::from(&TokenDefinition::Fungible {
            name: name.to_owned(),
            total_supply: 1_000_000_000,
            metadata_id: None,
        }),
        nonce: Nonce(0),
    }
}
/// WLEZ token-holding balance for `id`.
fn bal(state: &V03State, id: AccountId) -> u128 {
    match TokenHolding::try_from(&state.get_account_by_id(id).data) {
        Ok(TokenHolding::Fungible { balance, .. }) => balance,
        _ => panic!("account {id:?} is not a fungible holding"),
    }
}
/// Native (LEZ) balance for `id`.
fn native_bal(state: &V03State, id: AccountId) -> u128 {
    state.get_account_by_id(id).balance
}
/// `definition.total_supply` for the WLEZ definition.
fn total_supply(state: &V03State, def: AccountId) -> u128 {
    match TokenDefinition::try_from(&state.get_account_by_id(def).data) {
        Ok(TokenDefinition::Fungible { total_supply, .. }) => total_supply,
        _ => panic!("account {def:?} is not a fungible definition"),
    }
}

fn deploy(state: &mut V03State) {
    for elf in [token_methods::TOKEN_ELF.to_vec(), wlez_methods::WLEZ_ELF.to_vec()] {
        let msg = program_deployment_transaction::Message::new(elf);
        state
            .transition_from_program_deployment_transaction(&ProgramDeploymentTransaction::new(msg))
            .expect("program deployment must succeed");
    }
}

/// Deploy token+wlez and run the real `Initialize`: claims the vault PDA and
/// creates the WLEZ token definition (chained `token::NewFungibleDefinition`,
/// total_supply 0). `payer_key` signs the bootstrap tx.
fn deploy_and_initialize(state: &mut V03State, payer_key: &PrivateKey) {
    deploy(state);

    let vault = get_wlez_vault_id(&wlez());
    let definition = get_wlez_definition_id(&wlez());
    let init_holding = get_wlez_init_holding_id(&wlez());
    // A pre-existing token-program definition, used only as the `token_program_id`
    // source inside Initialize.
    let reference = AccountId::new([0xCC; 32]);
    state.force_insert_account(reference, token_def_account("REF"));
    let payer = id_of(payer_key);

    let message = public_transaction::Message::try_new(
        wlez(),
        vec![vault, definition, init_holding, reference, payer],
        vec![Nonce(0)],
        Instruction::Initialize { token_program_id: token(), native_program_id: native() },
    )
    .unwrap();
    let witness = public_transaction::WitnessSet::for_message(&message, &[payer_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(message, witness), 0, 0)
        .expect("Initialize must succeed");

    // Post-conditions: vault claimed by WLEZ at 0 LEZ, definition created by the
    // token program at total_supply 0, matching the invariant vault == supply.
    assert_eq!(state.get_account_by_id(vault).program_owner, wlez(), "vault claimed by WLEZ");
    assert_eq!(native_bal(state, vault), 0, "fresh vault holds no LEZ");
    assert_eq!(state.get_account_by_id(definition).program_owner, token(), "definition owned by token program");
    assert_eq!(total_supply(state, definition), 0, "WLEZ supply starts at 0");
    match TokenDefinition::try_from(&state.get_account_by_id(definition).data) {
        Ok(TokenDefinition::Fungible { name, .. }) => assert_eq!(name, WLEZ_NAME),
        _ => panic!("WLEZ definition is not a fungible definition"),
    }
}

#[test]
fn wlez_wrap_locks_native_and_mints_wlez() {
    let payer_key = PrivateKey::try_new([10; 32]).unwrap();
    let mut state = V03State::new_with_genesis_accounts(&[], vec![], 0);
    deploy_and_initialize(&mut state, &payer_key);

    let vault = get_wlez_vault_id(&wlez());
    let definition = get_wlez_definition_id(&wlez());

    // user_native signs the wrap (the native transfer's `assert!(sender.is_authorized)`).
    let user_native_key = PrivateKey::try_new([80; 32]).unwrap();
    let user_native = id_of(&user_native_key);
    // user_holding is the mint destination; need not sign for wrap.
    let user_holding = id_of(&PrivateKey::try_new([81; 32]).unwrap());
    let native_start: u128 = 100_000;
    let amount: u128 = 30_000;
    state.force_insert_account(user_native, native_account(native_start));
    state.force_insert_account(user_holding, fungible(definition, 0));

    let message = public_transaction::Message::try_new(
        wlez(),
        vec![user_native, vault, definition, user_holding],
        vec![Nonce(0)],
        Instruction::Wrap { amount },
    )
    .unwrap();
    let witness = public_transaction::WitnessSet::for_message(&message, &[&user_native_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(message, witness), 0, 0)
        .expect("Wrap must succeed");

    assert_eq!(native_bal(&state, user_native), native_start - amount, "user_native paid `amount` LEZ");
    assert_eq!(native_bal(&state, vault), amount, "vault escrowed `amount` LEZ");
    assert_eq!(bal(&state, user_holding), amount, "user_holding minted `amount` WLEZ");
    assert_eq!(total_supply(&state, definition), amount, "WLEZ supply grew by `amount`");
    // Core invariant: vault native balance == WLEZ total supply.
    assert_eq!(native_bal(&state, vault), total_supply(&state, definition), "vault == supply after Wrap");
}

#[test]
fn wlez_unwrap_burns_wlez_and_releases_native() {
    let payer_key = PrivateKey::try_new([11; 32]).unwrap();
    let mut state = V03State::new_with_genesis_accounts(&[], vec![], 0);
    deploy_and_initialize(&mut state, &payer_key);

    let vault = get_wlez_vault_id(&wlez());
    let definition = get_wlez_definition_id(&wlez());

    // Wrap first so there is supply to burn and LEZ in the vault to release.
    let user_native_key = PrivateKey::try_new([82; 32]).unwrap();
    let user_native = id_of(&user_native_key);
    let user_holding_key = PrivateKey::try_new([83; 32]).unwrap();
    let user_holding = id_of(&user_holding_key);
    let native_start: u128 = 100_000;
    let wrapped: u128 = 40_000;
    state.force_insert_account(user_native, native_account(native_start));
    state.force_insert_account(user_holding, fungible(definition, 0));

    let wrap_msg = public_transaction::Message::try_new(
        wlez(),
        vec![user_native, vault, definition, user_holding],
        vec![Nonce(0)],
        Instruction::Wrap { amount: wrapped },
    )
    .unwrap();
    let wrap_w = public_transaction::WitnessSet::for_message(&wrap_msg, &[&user_native_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(wrap_msg, wrap_w), 0, 0)
        .expect("setup Wrap must succeed");

    // Now Unwrap a portion. user_holding signs (the burn's authorization).
    let unwrap_amount: u128 = 25_000;
    let unwrap_msg = public_transaction::Message::try_new(
        wlez(),
        vec![user_holding, definition, vault, user_native],
        vec![Nonce(0)],
        Instruction::Unwrap { amount: unwrap_amount },
    )
    .unwrap();
    let unwrap_w = public_transaction::WitnessSet::for_message(&unwrap_msg, &[&user_holding_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(unwrap_msg, unwrap_w), 0, 0)
        .expect("Unwrap must succeed");

    assert_eq!(bal(&state, user_holding), wrapped - unwrap_amount, "user_holding WLEZ burnt by `amount`");
    assert_eq!(total_supply(&state, definition), wrapped - unwrap_amount, "WLEZ supply shrank by `amount`");
    assert_eq!(native_bal(&state, vault), wrapped - unwrap_amount, "vault released `amount` LEZ");
    assert_eq!(
        native_bal(&state, user_native),
        native_start - wrapped + unwrap_amount,
        "user_native credited the released LEZ"
    );
    // Invariant holds after Unwrap's direct vault decrease (validate_execution rule 5 + 8).
    assert_eq!(native_bal(&state, vault), total_supply(&state, definition), "vault == supply after Unwrap");
}

/// NEGATIVE: an Unwrap for more than the vault holds must revert - the guest's
/// `assert!(vault.balance >= amount)` fires, and (defence in depth) the direct
/// vault decrease would otherwise underflow rule 8's balance accounting.
#[test]
fn wlez_unwrap_above_vault_balance_reverts() {
    let payer_key = PrivateKey::try_new([12; 32]).unwrap();
    let mut state = V03State::new_with_genesis_accounts(&[], vec![], 0);
    deploy_and_initialize(&mut state, &payer_key);

    let vault = get_wlez_vault_id(&wlez());
    let definition = get_wlez_definition_id(&wlez());

    let user_native_key = PrivateKey::try_new([84; 32]).unwrap();
    let user_native = id_of(&user_native_key);
    let user_holding_key = PrivateKey::try_new([85; 32]).unwrap();
    let user_holding = id_of(&user_holding_key);
    let wrapped: u128 = 20_000;
    state.force_insert_account(user_native, native_account(100_000));
    state.force_insert_account(user_holding, fungible(definition, 0));

    let wrap_msg = public_transaction::Message::try_new(
        wlez(),
        vec![user_native, vault, definition, user_holding],
        vec![Nonce(0)],
        Instruction::Wrap { amount: wrapped },
    )
    .unwrap();
    let wrap_w = public_transaction::WitnessSet::for_message(&wrap_msg, &[&user_native_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(wrap_msg, wrap_w), 0, 0)
        .expect("setup Wrap must succeed");

    // Ask for more than is in the vault.
    let unwrap_msg = public_transaction::Message::try_new(
        wlez(),
        vec![user_holding, definition, vault, user_native],
        vec![Nonce(0)],
        Instruction::Unwrap { amount: wrapped + 1 },
    )
    .unwrap();
    let unwrap_w = public_transaction::WitnessSet::for_message(&unwrap_msg, &[&user_holding_key]);
    let result =
        state.transition_from_public_transaction(&PublicTransaction::new(unwrap_msg, unwrap_w), 0, 0);
    assert!(result.is_err(), "Unwrap above vault balance must revert");
    // Vault and supply untouched after the revert.
    assert_eq!(native_bal(&state, vault), wrapped, "vault unchanged after revert");
    assert_eq!(total_supply(&state, definition), wrapped, "supply unchanged after revert");
}

/// NEGATIVE: a Wrap whose `user_holding` points at a *non-WLEZ* definition must
/// revert - the guest's `assert_eq!(holding_def, definition.account_id)` guards
/// against minting WLEZ into a holding the program does not control.
#[test]
fn wlez_wrap_with_foreign_holding_definition_reverts() {
    let payer_key = PrivateKey::try_new([13; 32]).unwrap();
    let mut state = V03State::new_with_genesis_accounts(&[], vec![], 0);
    deploy_and_initialize(&mut state, &payer_key);

    let vault = get_wlez_vault_id(&wlez());
    let definition = get_wlez_definition_id(&wlez());

    let user_native_key = PrivateKey::try_new([86; 32]).unwrap();
    let user_native = id_of(&user_native_key);
    let user_holding = id_of(&PrivateKey::try_new([87; 32]).unwrap());
    // Holding points at some OTHER token definition, not the WLEZ one.
    let foreign_def = AccountId::new([0xEE; 32]);
    state.force_insert_account(user_native, native_account(100_000));
    state.force_insert_account(user_holding, fungible(foreign_def, 0));

    let message = public_transaction::Message::try_new(
        wlez(),
        vec![user_native, vault, definition, user_holding],
        vec![Nonce(0)],
        Instruction::Wrap { amount: 5_000 },
    )
    .unwrap();
    let witness = public_transaction::WitnessSet::for_message(&message, &[&user_native_key]);
    let result =
        state.transition_from_public_transaction(&PublicTransaction::new(message, witness), 0, 0);
    assert!(result.is_err(), "Wrap into a foreign-definition holding must revert");
    // Nothing moved: vault still empty, supply still 0.
    assert_eq!(native_bal(&state, vault), 0, "vault unchanged after revert");
    assert_eq!(total_supply(&state, definition), 0, "supply unchanged after revert");
}
