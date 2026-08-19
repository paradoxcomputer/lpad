//! F14, end to end: a sale whose creator is a fresh, never-initialised account
//! can be CREATED and then never settled - and the fix closes that for good.
//!
//! # The finding
//!
//! `close_sale` and `withdraw` (both programs) echo the creator's own account in
//! their post-states, and neither may drop it: v0.2.4 rejects a declared account
//! that is missing from the output (`DeclaredAccountMissingFromOutput`). So the
//! account that signs a `CreateSale` has to remain an account a program is
//! ALLOWED to echo, for the whole life of the sale.
//!
//! `validate_execution` rule 7 (`NonDefaultAccountWithDefaultOwner`,
//! `lee/state_machine/core/src/program/mod.rs`) forbids exactly one shape: a
//! post state owned by the DEFAULT program whose PRE state is not the
//! whole-default account. `State::apply_state_diff` bumps every signer's nonce
//! AFTER the diff is applied, so an account that was never initialised and signs
//! one transaction lands precisely there - `{DEFAULT, nonce 1}`. The create
//! itself succeeds, because during it the creator is still whole-default. Every
//! close and every withdraw afterwards fails. The deposit and the entire raise
//! are locked forever.
//!
//! # Why the rest of the suite cannot see it
//!
//! Every other fixture in `programs/integration_tests` hands the creator an
//! account that is already owned by the token program -
//! `bonding_curve.rs:284`'s `state.force_insert_account(i.creator,
//! fungible(i.collateral_def, 0))`, and `lbp.rs`'s identical line. A
//! token-program-owned account can never trip rule 7, whatever it goes on to
//! sign, so those tests agree with themselves by construction and the trap is
//! invisible to them. The creators here are NOT force-inserted at all: they are
//! whole-default, which is exactly what `wallet::create_new_account_public`
//! leaves behind and what `lpad launch` was handed in the field.
//!
//! # What is proved here
//!
//! Four end-to-end walks against the real `V03State`, two per program:
//!
//!   * the TRAP - a create from a whole-default creator succeeds, moves the
//!     deposit, and then every settlement path is refused with rule 7, naming
//!     that creator. Nor can the creator be repaired afterwards.
//!   * the FIX - the same walk with one extra transaction first
//!     (`authenticated_transfer::Initialize`, which is what `scripts/bootstrap.sh`
//!     and the SDK's `initialize_native_account` submit) creates, closes AND
//!     withdraws, and the money actually arrives.
//!
//! The SDK-side guard (`sdk::ensure_creator_settleable_for`) is a pure predicate
//! with its own unit tests; those prove the SDK says no. This file is the only
//! place that proves the RUNTIME says no - i.e. that the predicate is guarding a
//! real chain behaviour and not a misdiagnosis - and that the remedy it
//! prescribes actually works.

use authenticated_transfer_core::Instruction as AuthTransferInstruction;
use bonding_curve_core::{
    buy_cost_for_tokens, compute_collateral_vault_pda as bc_collateral_vault_pda,
    compute_creator_index_pda as bc_creator_index_pda, compute_sale_pda,
    compute_token_vault_pda as bc_token_vault_pda, Instruction as BcInstruction, SaleState,
    SaleStatus, CLOCK_01,
};
use lbp_core::{
    close_fee, compute_collateral_vault_pda as lbp_collateral_vault_pda,
    compute_creator_index_pda as lbp_creator_index_pda, compute_pool_pda,
    compute_token_vault_pda as lbp_token_vault_pda, Instruction as LbpInstruction, PoolState,
    SaleStatus as LbpSaleStatus,
};
use lee::{
    error::{InvalidProgramBehaviorError, LeeError},
    program_deployment_transaction::{self, ProgramDeploymentTransaction},
    public_transaction, PrivateKey, PublicKey, PublicTransaction, V03State,
};
use lee_core::{
    account::{Account, AccountId, Data, Nonce},
    program::{ExecutionValidationError, ProgramId, DEFAULT_PROGRAM_ID},
};
use token_core::{TokenDefinition, TokenHolding};

// --- bonding-curve sale parameters, copied from the suite's standard fixture --
const VT: u128 = 2_000_000;
const VC: u128 = 50_000;
const D: u128 = 1_000_000;
const R: u128 = 200_000;
const BC_FEE_BPS: u128 = 100; // 1%

/// `bonding_curve_program::lifecycle::RECOVERY_FLOOR_MS`, restated because it is
/// private to the guest crate: 7 days in ms, after which the creator of a sale
/// with no end timestamp may close it.
///
/// The trap walk deliberately creates a sale with `end_timestamp_ms == 0` and
/// closes it on this floor, because that is the only shape whose BUYS are not
/// also time-gated - so one state can carry a close attempt AND a later buy, and
/// the withdraw trap can be reached the way a real sale reaches it (auto-close
/// on the supply target) rather than by hand-editing the sale account.
const BC_RECOVERY_FLOOR_MS: u64 = 7 * 24 * 60 * 60 * 1_000;

// --- LBP pool parameters, copied from the suite's standard fixture -----------
const RESERVE_TOKEN: u128 = 1_000_000;
const RESERVE_COLLATERAL: u128 = 50_000;
const T_START: u64 = 0;
const T_END: u64 = 1_000_000;
const LBP_FEE_BPS: u128 = 500; // 5% at close

fn bc() -> ProgramId {
    lpad_guests::bonding_curve().id()
}
fn lbp() -> ProgramId {
    lpad_guests::lbp().id()
}
fn token() -> ProgramId {
    programs::token().id()
}
fn ata_prog() -> ProgramId {
    programs::ata().id()
}
/// The native-LEZ program whose `Initialize` is the whole remedy: it is the one
/// instruction that can give a whole-default account an owner, and it is
/// registered at genesis by `testnet_initial_state::initial_state()`.
fn auth_transfer() -> ProgramId {
    programs::authenticated_transfer().id()
}
fn id_of(key: &PrivateKey) -> AccountId {
    AccountId::from(&PublicKey::new_from_private_key(key))
}
fn w(num: u128, den: u128) -> u128 {
    (num << 64) / den
}
fn fungible(def: AccountId, balance: u128) -> Account {
    Account {
        program_owner: token(),
        balance: 0,
        data: Data::from(&TokenHolding::Fungible { definition_id: def, balance }),
        nonce: Nonce(0),
    }
}
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
fn bal(state: &V03State, id: AccountId) -> u128 {
    match TokenHolding::try_from(&state.get_account_by_id(id).data) {
        Ok(TokenHolding::Fungible { balance, .. }) => balance,
        _ => panic!("account {id:?} is not a fungible holding"),
    }
}
/// Deploy one lpad guest. The built-ins (token, ATA, clock,
/// authenticated_transfer, ...) are already registered by
/// `initial_state()`, so re-deploying them would fail with
/// `ProgramAlreadyExists`.
fn deploy(state: &mut V03State, elf: Vec<u8>) {
    let msg = program_deployment_transaction::Message::new(elf);
    state
        .transition_from_program_deployment_transaction(&ProgramDeploymentTransaction::new(msg))
        .expect("program deployment must succeed");
}
/// Overwrite the canonical clock account, as `bonding_curve.rs::set_clock` does:
/// a `u64` block id then an `i64` millisecond timestamp, both little-endian.
fn set_clock(state: &mut V03State, block_id: u64, ts_ms: u64) {
    let mut clock = state.get_account_by_id(CLOCK_01);
    let mut bytes = Vec::with_capacity(16);
    bytes.extend_from_slice(&block_id.to_le_bytes());
    bytes.extend_from_slice(&i64::try_from(ts_ms).expect("timestamp fits i64").to_le_bytes());
    clock.data = Data::try_from(bytes).expect("16 bytes is well within the account data cap");
    state.force_insert_account(CLOCK_01, clock);
}

// ---------------------------------------------------------------------------
// The two states a creator can be in, and the transaction between them
// ---------------------------------------------------------------------------

/// Assert the creator is in the state `wallet account new public` leaves it in:
/// present in no account map at all, so LEZ reads it back as the whole-default
/// account. Not a fixture shortcut - the ABSENCE of a fixture line.
#[track_caller]
fn assert_whole_default(state: &V03State, creator: AccountId, when: &str) {
    assert_eq!(
        state.get_account_by_id(creator),
        Account::default(),
        "{when}: the creator must be genuinely whole-default. If a fixture ever \
         force-inserts it, this whole file stops testing anything"
    );
}

/// Assert the creator is in the TRAPPED state: unowned, and no longer pristine.
/// This is the shape rule 7 refuses to let any program echo, and the shape
/// `authenticated_transfer::Initialize` refuses to claim.
#[track_caller]
fn assert_trapped(state: &V03State, creator: AccountId) {
    let acc = state.get_account_by_id(creator);
    assert_eq!(
        acc.program_owner, DEFAULT_PROGRAM_ID,
        "signing a create does not give the creator an owner - nothing in the create claims it"
    );
    assert_eq!(
        acc.nonce,
        Nonce(1),
        "apply_state_diff bumps every signer's nonce AFTER applying the diff, so the create \
         leaves the creator at nonce 1"
    );
    assert_ne!(
        acc,
        Account::default(),
        "and that nonce is what makes it non-default: from here rule 7 rejects every \
         transaction that echoes it"
    );
}

/// The remedy: claim the creator under the native program BEFORE it signs
/// anything, which is what `scripts/bootstrap.sh` and
/// `LaunchpadClient::initialize_native_account` submit. One account, and one
/// signer: the account itself, because `Claim::Authorized` is only granted to a
/// pre-state the signer authorized.
fn initialize_creator(state: &mut V03State, key: &PrivateKey, block_id: u64) {
    let creator = id_of(key);
    let message = public_transaction::Message::try_new(
        auth_transfer(),
        vec![creator],
        vec![Nonce(0)],
        AuthTransferInstruction::Initialize,
    )
    .expect("message");
    let witness = public_transaction::WitnessSet::for_message(&message, &[key]);
    state
        .transition_from_public_transaction(
            &PublicTransaction::new(message, witness),
            block_id,
            0,
        )
        .expect("initialising a pristine creator under authenticated_transfer must succeed");

    let acc = state.get_account_by_id(creator);
    assert_eq!(
        acc.program_owner,
        auth_transfer(),
        "the claim must actually have landed - an owner is the entire point"
    );
    assert_eq!(acc.nonce, Nonce(1), "the initialise itself bumps the creator's nonce");
}

/// The failure the CRITICAL is about, matched by SHAPE and by ACCOUNT.
///
/// "some error" would pass just as happily if the transaction had reverted for
/// an unrelated reason - a guest assert, a bad nonce, a missing account - and
/// then this file would be proving nothing. Rule 7, naming the creator, is the
/// only outcome that means what the finding says.
#[track_caller]
fn expect_rule_7(result: Result<(), LeeError>, creator: AccountId, what: &str) {
    match result {
        Err(LeeError::InvalidProgramBehavior(
            InvalidProgramBehaviorError::ExecutionValidationFailed(
                ExecutionValidationError::NonDefaultAccountWithDefaultOwner { account_id },
            ),
        )) => assert_eq!(
            account_id, creator,
            "{what} must be refused because of the CREATOR, not some other account"
        ),
        other => panic!(
            "{what} must fail with validate_execution rule 7 \
             (NonDefaultAccountWithDefaultOwner) naming the creator. Got: {other:?}. \
             If this reverted for another reason - or worse, SUCCEEDED - then F14 is \
             misdiagnosed and the SDK guard is guarding nothing"
        ),
    }
}

/// Once trapped, a creator cannot be rescued either: `Initialize` asserts the
/// account it claims is still whole-default, and the nonce bump has already
/// destroyed that. This is what makes the fund-lock permanent rather than
/// merely inconvenient, and it is why the remedy has to come FIRST.
#[track_caller]
fn assert_cannot_be_repaired(state: &mut V03State, key: &PrivateKey, block_id: u64) {
    let creator = id_of(key);
    let message = public_transaction::Message::try_new(
        auth_transfer(),
        vec![creator],
        vec![Nonce(1)], // the create already bumped it
        AuthTransferInstruction::Initialize,
    )
    .expect("message");
    let witness = public_transaction::WitnessSet::for_message(&message, &[key]);
    let result = state.transition_from_public_transaction(
        &PublicTransaction::new(message, witness),
        block_id,
        0,
    );
    let Err(LeeError::ProgramExecutionFailed(err)) = result else {
        panic!(
            "a trapped creator must not be initialisable after the fact - if it were, the \
             fund-lock would be recoverable and the SDK's \"use a fresh account\" advice \
             would be wrong. Got: {result:?}"
        );
    };
    assert!(
        err.contains("uninitialized"),
        "it must fail on authenticated_transfer's own whole-default assert, not on something \
         incidental: {err}"
    );
    assert_eq!(
        state.get_account_by_id(creator).program_owner,
        DEFAULT_PROGRAM_ID,
        "and the creator is still unowned afterwards"
    );
}

// ===========================================================================
// Bonding curve
// ===========================================================================

struct BcIds {
    token_def: AccountId,
    collateral_def: AccountId,
    treasury: AccountId,
    creator: AccountId,
    sale: AccountId,
    token_vault: AccountId,
    collateral_vault: AccountId,
    creator_index: AccountId,
}
fn bc_ids(creator: AccountId) -> BcIds {
    let token_def = AccountId::new([7; 32]);
    let collateral_def = AccountId::new([8; 32]);
    let sale = compute_sale_pda(bc(), token_def, collateral_def, creator, 0);
    BcIds {
        token_def,
        collateral_def,
        treasury: AccountId::new([9; 32]),
        creator,
        sale,
        token_vault: bc_token_vault_pda(bc(), sale),
        collateral_vault: bc_collateral_vault_pda(bc(), sale),
        creator_index: bc_creator_index_pda(bc(), creator),
    }
}

/// Everything a `CreateSale` needs on chain - and NOT the creator identity
/// account, which is the whole point of this file. Compare
/// `bonding_curve.rs::seed_create_fixture`, which force-inserts a
/// token-program-owned creator and thereby hides the bug.
fn bc_seed_fixture(state: &mut V03State, i: &BcIds, creator_token_id: AccountId) {
    state.force_insert_account(creator_token_id, fungible(i.token_def, D + R));
    state.force_insert_account(i.token_def, token_def_account("Project"));
    state.force_insert_account(i.collateral_def, token_def_account("Collateral"));
    state.force_insert_account(i.treasury, fungible(i.collateral_def, 0));
}

/// The standard create over this file's sale parameters, with
/// `end_timestamp_ms == 0` (see [`BC_RECOVERY_FLOOR_MS`]). Account order is the
/// program's; the two signers are the creator's token holding and the creator
/// identity, and they are given separate nonces because the fix walk initialises
/// the creator first and so signs at a nonce the holding has not reached.
fn bc_create_msg(
    i: &BcIds,
    creator_token_id: AccountId,
    holding_nonce: u128,
    creator_nonce: u128,
) -> public_transaction::Message {
    public_transaction::Message::try_new(
        bc(),
        vec![
            i.sale,
            i.token_vault,
            i.collateral_vault,
            i.treasury,
            i.token_def,
            i.collateral_def,
            creator_token_id,
            i.creator,
            i.creator_index,
            CLOCK_01,
        ],
        vec![Nonce(holding_nonce), Nonce(creator_nonce)],
        BcInstruction::CreateSale {
            collateral_definition_id: i.collateral_def,
            treasury_id: i.treasury,
            token_name: String::new(),
            token_symbol: String::new(),
            sale_quantity: D,
            dex_seed_quantity: R,
            virt_token: VT,
            virt_collateral: VC,
            fee_bps: BC_FEE_BPS,
            one_directional: false,
            end_timestamp_ms: 0,
            min_duration_ms: 0,
            nonce: 0,
            ata_program_id: ata_prog(),
            deadline: u64::MAX,
        },
    )
    .expect("message")
}

fn bc_close_msg(i: &BcIds, creator_nonce: u128) -> public_transaction::Message {
    public_transaction::Message::try_new(
        bc(),
        vec![i.sale, i.creator, CLOCK_01],
        vec![Nonce(creator_nonce)],
        BcInstruction::CloseSale { deadline: u64::MAX },
    )
    .expect("message")
}

fn bc_withdraw_msg(
    i: &BcIds,
    creator_coll: AccountId,
    creator_tok: AccountId,
    creator_nonce: u128,
) -> public_transaction::Message {
    public_transaction::Message::try_new(
        bc(),
        vec![
            i.sale,
            i.token_vault,
            i.collateral_vault,
            creator_coll,
            creator_tok,
            i.creator,
        ],
        vec![Nonce(creator_nonce)],
        BcInstruction::Withdraw { deadline: u64::MAX },
    )
    .expect("message")
}

/// One public buy, funded from a fresh buyer. Returns the collateral spent.
fn bc_buy(
    state: &mut V03State,
    i: &BcIds,
    collateral_in: u128,
    block_id: u64,
    ts_ms: u64,
) -> u128 {
    let buyer_key = PrivateKey::try_new([60; 32]).expect("key");
    let buyer_coll = id_of(&buyer_key);
    let buyer_tok = AccountId::new([61; 32]);
    state.force_insert_account(buyer_coll, fungible(i.collateral_def, collateral_in));
    state.force_insert_account(buyer_tok, fungible(i.token_def, 0));

    let buy = public_transaction::Message::try_new(
        bc(),
        vec![
            i.sale,
            i.token_vault,
            i.collateral_vault,
            i.treasury,
            buyer_coll,
            buyer_tok,
            CLOCK_01,
        ],
        vec![Nonce(0)],
        BcInstruction::Buy { collateral_in, min_tokens_out: 0, deadline: u64::MAX },
    )
    .expect("message");
    let witness = public_transaction::WitnessSet::for_message(&buy, &[&buyer_key]);
    state
        .transition_from_public_transaction(
            &PublicTransaction::new(buy, witness),
            block_id,
            ts_ms,
        )
        .expect("the buy must be included");
    collateral_in
}

/// THE TRAP, on the real state machine, with nothing hand-edited.
///
/// One creator, one sale, and then every way out of it refused - each time by
/// rule 7, naming that creator. If this test ever goes green on the `.expect`s
/// instead of the `expect_rule_7`s, F14 was misdiagnosed and the SDK guard is
/// guarding nothing; that is why the failure is matched by shape rather than by
/// `is_err()`.
#[test]
fn bonding_curve_a_whole_default_creator_can_never_close_or_withdraw() {
    let creator_token_key = PrivateKey::try_new([41; 32]).expect("key");
    let creator_key = PrivateKey::try_new([42; 32]).expect("key");
    let creator_token_id = id_of(&creator_token_key);
    let i = bc_ids(id_of(&creator_key));

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state, lpad_guests::bonding_curve().elf().to_vec());
    bc_seed_fixture(&mut state, &i, creator_token_id);
    assert_whole_default(&state, i.creator, "before the create");

    // 1. The create SUCCEEDS. That is what makes this a trap rather than a
    //    rejection: the guests see a whole-default creator and are right to
    //    accept it, the deposit really moves, and nothing anywhere says no.
    let create = bc_create_msg(&i, creator_token_id, 0, 0);
    let witness =
        public_transaction::WitnessSet::for_message(&create, &[&creator_token_key, &creator_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(create, witness), 0, 0)
        .expect("a create from a whole-default creator must succeed - the trap springs later");
    assert_eq!(bal(&state, i.token_vault), D + R, "the creator's whole deposit is now on chain");
    assert_eq!(bal(&state, creator_token_id), 0, "and it has left their hands");
    assert_trapped(&state, i.creator);

    // 2. CloseSale. The sale has no end timestamp, so the recovery floor is the
    //    creator's route out; put the clock past it so the guest's own guards all
    //    pass and rule 7 is the ONLY thing that can refuse this.
    set_clock(&mut state, 1, BC_RECOVERY_FLOOR_MS);
    let close = bc_close_msg(&i, 1);
    let witness = public_transaction::WitnessSet::for_message(&close, &[&creator_key]);
    expect_rule_7(
        state.transition_from_public_transaction(
            &PublicTransaction::new(close, witness),
            1,
            BC_RECOVERY_FLOOR_MS,
        ),
        i.creator,
        "CloseSale by a whole-default creator",
    );
    let sale = SaleState::try_from(&state.get_account_by_id(i.sale).data).expect("sale");
    assert!(
        matches!(sale.status, SaleStatus::Open),
        "the rejected close changed nothing: the sale is still Open, and always will be"
    );

    // 3. Withdraw. It needs a CLOSED sale, and the creator cannot deliver one -
    //    so reach the closed state the only other way a real sale can, by
    //    exhausting the supply target. `apply_buy` auto-closes on
    //    `sale_reserve == 0`, and that path names no creator at all. This is what
    //    makes the withdraw refusal below a genuine end-to-end observation rather
    //    than a hand-edited sale account.
    let buyout = buy_cost_for_tokens(VT, VC, BC_FEE_BPS, D);
    bc_buy(&mut state, &i, buyout, 2, BC_RECOVERY_FLOOR_MS);
    let sale = SaleState::try_from(&state.get_account_by_id(i.sale).data).expect("sale");
    assert_eq!(sale.sale_reserve, 0, "the buyout must really exhaust the sale reserve");
    assert!(
        matches!(sale.status, SaleStatus::Closed),
        "and the sale must have auto-closed on the supply target"
    );
    let stranded_collateral = bal(&state, i.collateral_vault);
    let stranded_tokens = bal(&state, i.token_vault);
    assert!(
        stranded_collateral > 0 && stranded_tokens > 0,
        "there must be real money to withdraw, or the refusal below proves nothing"
    );

    let creator_coll = AccountId::new([90; 32]);
    let creator_tok = AccountId::new([91; 32]);
    state.force_insert_account(creator_coll, fungible(i.collateral_def, 0));
    state.force_insert_account(creator_tok, fungible(i.token_def, 0));
    let withdraw = bc_withdraw_msg(&i, creator_coll, creator_tok, 1);
    let witness = public_transaction::WitnessSet::for_message(&withdraw, &[&creator_key]);
    expect_rule_7(
        state.transition_from_public_transaction(
            &PublicTransaction::new(withdraw, witness),
            3,
            BC_RECOVERY_FLOOR_MS,
        ),
        i.creator,
        "Withdraw by a whole-default creator, from a sale that auto-closed",
    );

    // 4. The money is exactly where it was, and there is no other drain: the
    //    vaults are PDAs of this program, and every instruction that can move
    //    them names the creator.
    assert_eq!(
        bal(&state, i.collateral_vault),
        stranded_collateral,
        "the entire raise is still in the vault"
    );
    assert_eq!(
        bal(&state, i.token_vault),
        stranded_tokens,
        "and so is the unsold reserve plus the DEX seed"
    );
    assert_eq!(bal(&state, creator_coll), 0, "the creator received nothing");
    assert_eq!(bal(&state, creator_tok), 0, "the creator received nothing");

    // 5. And it cannot be undone.
    assert_cannot_be_repaired(&mut state, &creator_key, 4);
}

/// THE FIX: one extra transaction, before the creator ever signs, and the same
/// walk settles in full.
#[test]
fn bonding_curve_a_creator_initialised_first_can_create_close_and_withdraw() {
    let creator_token_key = PrivateKey::try_new([41; 32]).expect("key");
    let creator_key = PrivateKey::try_new([42; 32]).expect("key");
    let creator_token_id = id_of(&creator_token_key);
    let i = bc_ids(id_of(&creator_key));

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state, lpad_guests::bonding_curve().elf().to_vec());
    bc_seed_fixture(&mut state, &i, creator_token_id);
    assert_whole_default(&state, i.creator, "before the initialise");

    // 0. The remedy, first, exactly as the SDK orders it.
    initialize_creator(&mut state, &creator_key, 0);

    // 1. Create. The creator signs at nonce 1 now (the initialise bumped it);
    //    the token holding has signed nothing and is still at 0.
    let create = bc_create_msg(&i, creator_token_id, 0, 1);
    let witness =
        public_transaction::WitnessSet::for_message(&create, &[&creator_token_key, &creator_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(create, witness), 1, 0)
        .expect("create_sale must succeed with an initialised creator");
    assert_eq!(bal(&state, i.token_vault), D + R);
    assert_eq!(
        state.get_account_by_id(i.creator).program_owner,
        auth_transfer(),
        "the create echoes the creator unchanged, owner and all"
    );

    // 2. A real buy, so the withdrawal below has a raise to return and not just
    //    the creator's own deposit.
    let collateral_in = 5_000_u128;
    bc_buy(&mut state, &i, collateral_in, 2, 0);
    let raised = bal(&state, i.collateral_vault);
    let unsold = bal(&state, i.token_vault);
    let fee_paid = bal(&state, i.treasury);
    assert!(raised > 0 && unsold > 0 && fee_paid > 0, "the buy must move all three");

    // 3. Close - the transaction the trapped creator could never submit.
    set_clock(&mut state, 3, BC_RECOVERY_FLOOR_MS);
    let close = bc_close_msg(&i, 2);
    let witness = public_transaction::WitnessSet::for_message(&close, &[&creator_key]);
    state
        .transition_from_public_transaction(
            &PublicTransaction::new(close, witness),
            3,
            BC_RECOVERY_FLOOR_MS,
        )
        .expect("an initialised creator must be able to close");
    let sale = SaleState::try_from(&state.get_account_by_id(i.sale).data).expect("sale");
    assert!(matches!(sale.status, SaleStatus::Closed), "the sale really closed");
    assert_eq!(
        sale.treasury_owed, 0,
        "a public buy pays its fee at once, so the whole vault balance is the creator's"
    );

    // 4. Withdraw, and the money actually arrives.
    let creator_coll = AccountId::new([90; 32]);
    let creator_tok = AccountId::new([91; 32]);
    state.force_insert_account(creator_coll, fungible(i.collateral_def, 0));
    state.force_insert_account(creator_tok, fungible(i.token_def, 0));
    let withdraw = bc_withdraw_msg(&i, creator_coll, creator_tok, 3);
    let witness = public_transaction::WitnessSet::for_message(&withdraw, &[&creator_key]);
    state
        .transition_from_public_transaction(
            &PublicTransaction::new(withdraw, witness),
            4,
            BC_RECOVERY_FLOOR_MS,
        )
        .expect("an initialised creator must be able to withdraw");

    assert_eq!(bal(&state, creator_coll), raised, "the creator is paid the whole raise");
    assert_eq!(bal(&state, creator_tok), unsold, "and gets every unsold token back");
    assert_eq!(bal(&state, i.collateral_vault), 0, "collateral vault drained");
    assert_eq!(bal(&state, i.token_vault), 0, "token vault drained");
    assert_eq!(
        state.get_account_by_id(i.creator).nonce,
        Nonce(4),
        "initialise, create, close, withdraw - four signatures, and none of them stranded it"
    );
}

// ===========================================================================
// LBP
// ===========================================================================

struct LbpIds {
    token_def: AccountId,
    collateral_def: AccountId,
    treasury: AccountId,
    creator: AccountId,
    pool: AccountId,
    token_vault: AccountId,
    collateral_vault: AccountId,
    creator_index: AccountId,
}
fn lbp_ids(creator: AccountId) -> LbpIds {
    let token_def = AccountId::new([7; 32]);
    let collateral_def = AccountId::new([8; 32]);
    let pool = compute_pool_pda(lbp(), token_def, collateral_def, creator, 0);
    LbpIds {
        token_def,
        collateral_def,
        treasury: AccountId::new([9; 32]),
        creator,
        pool,
        token_vault: lbp_token_vault_pda(lbp(), pool),
        collateral_vault: lbp_collateral_vault_pda(lbp(), pool),
        creator_index: lbp_creator_index_pda(lbp(), creator),
    }
}

/// As `bc_seed_fixture`: everything the create needs EXCEPT the creator identity
/// account. Compare `lbp.rs::seed_create_fixture`, which force-inserts one.
fn lbp_seed_fixture(
    state: &mut V03State,
    i: &LbpIds,
    creator_token_id: AccountId,
    creator_coll_id: AccountId,
) {
    state.force_insert_account(creator_token_id, fungible(i.token_def, RESERVE_TOKEN));
    state.force_insert_account(creator_coll_id, fungible(i.collateral_def, RESERVE_COLLATERAL));
    state.force_insert_account(i.token_def, token_def_account("Project"));
    state.force_insert_account(i.collateral_def, token_def_account("Collateral"));
    state.force_insert_account(i.treasury, fungible(i.collateral_def, 0));
}

/// Three signers: both creator holdings (the deposit legs' senders) and the
/// creator identity, whose nonce is separate for the same reason as in
/// [`bc_create_msg`].
fn lbp_create_msg(
    i: &LbpIds,
    creator_token_id: AccountId,
    creator_coll_id: AccountId,
    holding_nonce: u128,
    creator_nonce: u128,
) -> public_transaction::Message {
    public_transaction::Message::try_new(
        lbp(),
        vec![
            i.pool,
            i.token_vault,
            i.collateral_vault,
            i.token_def,
            i.collateral_def,
            creator_token_id,
            creator_coll_id,
            i.creator,
            i.treasury,
            i.creator_index,
            CLOCK_01,
        ],
        vec![Nonce(holding_nonce), Nonce(holding_nonce), Nonce(creator_nonce)],
        LbpInstruction::CreateSale {
            collateral_definition_id: i.collateral_def,
            treasury_id: i.treasury,
            token_name: String::new(),
            token_symbol: String::new(),
            token_deposit: RESERVE_TOKEN,
            collateral_seed: RESERVE_COLLATERAL,
            w_start_q64: w(99, 100),
            w_end_q64: w(1, 100),
            t_start_ms: T_START,
            t_end_ms: T_END,
            fee_bps: LBP_FEE_BPS,
            block_token_ceiling: 0,
            allowlist_root: [0; 32],
            fixed_price: false,
            min_duration_ms: 0,
            nonce: 0,
            ata_program_id: ata_prog(),
            deadline: u64::MAX,
        },
    )
    .expect("message")
}

fn lbp_close_msg(i: &LbpIds, creator_nonce: u128) -> public_transaction::Message {
    public_transaction::Message::try_new(
        lbp(),
        vec![i.pool, i.creator, CLOCK_01],
        vec![Nonce(creator_nonce)],
        LbpInstruction::CloseSale { deadline: u64::MAX },
    )
    .expect("message")
}

fn lbp_withdraw_msg(
    i: &LbpIds,
    creator_coll: AccountId,
    creator_tok: AccountId,
    creator_nonce: u128,
) -> public_transaction::Message {
    public_transaction::Message::try_new(
        lbp(),
        vec![
            i.pool,
            i.token_vault,
            i.collateral_vault,
            creator_coll,
            creator_tok,
            i.creator,
        ],
        vec![Nonce(creator_nonce)],
        LbpInstruction::Withdraw { deadline: u64::MAX },
    )
    .expect("message")
}

fn lbp_buy(state: &mut V03State, i: &LbpIds, collateral_in: u128, block_id: u64) {
    let buyer_key = PrivateKey::try_new([60; 32]).expect("key");
    let buyer_coll = id_of(&buyer_key);
    let buyer_tok = AccountId::new([61; 32]);
    state.force_insert_account(buyer_coll, fungible(i.collateral_def, collateral_in));
    state.force_insert_account(buyer_tok, fungible(i.token_def, 0));

    let buy = public_transaction::Message::try_new(
        lbp(),
        vec![i.pool, i.token_vault, i.collateral_vault, buyer_coll, buyer_tok, CLOCK_01],
        vec![Nonce(0)],
        LbpInstruction::Buy { collateral_in, min_tokens_out: 0, deadline: u64::MAX },
    )
    .expect("message");
    let witness = public_transaction::WitnessSet::for_message(&buy, &[&buyer_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(buy, witness), block_id, 0)
        .expect("the public buy must be included");
}

/// The same trap, same shape, in the LBP - `close_sale` and `withdraw` echo the
/// creator there too.
///
/// It bites one step EARLIER than in the bonding curve, and that is worth being
/// precise about: the LBP has no auto-close (nothing in `buy.rs` ever sets
/// `SaleStatus::Closed`), so `CloseSale` is the only way a pool can reach the
/// state `Withdraw` requires. With a whole-default creator that close is refused
/// by rule 7 forever, so the pool is pinned Open and the withdraw never even
/// reaches the echo - it reverts on the guest's own "sale must be closed first".
/// Both refusals are asserted below, each by its own shape, because "the funds
/// are locked" is only proved by naming the thing that locks them.
#[test]
fn lbp_a_whole_default_creator_can_never_close_or_withdraw() {
    let creator_token_key = PrivateKey::try_new([41; 32]).expect("key");
    let creator_coll_key = PrivateKey::try_new([43; 32]).expect("key");
    let creator_key = PrivateKey::try_new([42; 32]).expect("key");
    let creator_token_id = id_of(&creator_token_key);
    let creator_coll_id = id_of(&creator_coll_key);
    let i = lbp_ids(id_of(&creator_key));

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state, lpad_guests::lbp().elf().to_vec());
    lbp_seed_fixture(&mut state, &i, creator_token_id, creator_coll_id);
    assert_whole_default(&state, i.creator, "before the create");

    // 1. The create succeeds and both deposits move.
    let create = lbp_create_msg(&i, creator_token_id, creator_coll_id, 0, 0);
    let witness = public_transaction::WitnessSet::for_message(
        &create,
        &[&creator_token_key, &creator_coll_key, &creator_key],
    );
    state
        .transition_from_public_transaction(&PublicTransaction::new(create, witness), 0, 0)
        .expect("a create from a whole-default creator must succeed - the trap springs later");
    assert_eq!(bal(&state, i.token_vault), RESERVE_TOKEN, "the project tokens are on chain");
    assert_eq!(
        bal(&state, i.collateral_vault),
        RESERVE_COLLATERAL,
        "and so is the creator's collateral seed"
    );
    assert_trapped(&state, i.creator);

    // 2. A real buy, so what gets locked is a raise and not only the seed.
    lbp_buy(&mut state, &i, 20_000, 1);
    let stranded_collateral = bal(&state, i.collateral_vault);
    let stranded_tokens = bal(&state, i.token_vault);
    assert!(
        stranded_collateral > RESERVE_COLLATERAL && stranded_tokens > 0,
        "the buy must really have raised something"
    );

    // 3. CloseSale at the pool's own end timestamp: every guard in the guest
    //    passes, so rule 7 is the only thing left that can refuse it.
    set_clock(&mut state, 2, T_END);
    let close = lbp_close_msg(&i, 1);
    let witness = public_transaction::WitnessSet::for_message(&close, &[&creator_key]);
    expect_rule_7(
        state.transition_from_public_transaction(
            &PublicTransaction::new(close, witness),
            2,
            T_END,
        ),
        i.creator,
        "CloseSale by a whole-default creator",
    );
    let pool = PoolState::try_from(&state.get_account_by_id(i.pool).data).expect("pool");
    assert!(
        matches!(pool.status, LbpSaleStatus::Open),
        "the rejected close changed nothing: the pool is pinned Open for good"
    );

    // 4. ...and therefore the withdrawal can never even be attempted for real.
    let creator_coll = AccountId::new([90; 32]);
    let creator_tok = AccountId::new([91; 32]);
    state.force_insert_account(creator_coll, fungible(i.collateral_def, 0));
    state.force_insert_account(creator_tok, fungible(i.token_def, 0));
    let withdraw = lbp_withdraw_msg(&i, creator_coll, creator_tok, 1);
    let witness = public_transaction::WitnessSet::for_message(&withdraw, &[&creator_key]);
    let result = state.transition_from_public_transaction(
        &PublicTransaction::new(withdraw, witness),
        3,
        T_END,
    );
    let Err(LeeError::ProgramExecutionFailed(err)) = result else {
        panic!("a withdraw against an Open pool must revert, got {result:?}");
    };
    assert!(
        err.contains("sale must be closed first"),
        "it must revert on the closed-pool guard - the pool can never get past it, which is \
         the second half of the lock: {err}"
    );

    // 5. Nothing moved, and nothing can.
    assert_eq!(bal(&state, i.collateral_vault), stranded_collateral, "seed + raise still locked");
    assert_eq!(bal(&state, i.token_vault), stranded_tokens, "unsold tokens still locked");
    assert_eq!(bal(&state, creator_coll), 0);
    assert_eq!(bal(&state, creator_tok), 0);
    assert_cannot_be_repaired(&mut state, &creator_key, 4);
}

/// And the LBP fix, end to end: initialise, create, buy, close, withdraw.
#[test]
fn lbp_a_creator_initialised_first_can_create_close_and_withdraw() {
    let creator_token_key = PrivateKey::try_new([41; 32]).expect("key");
    let creator_coll_key = PrivateKey::try_new([43; 32]).expect("key");
    let creator_key = PrivateKey::try_new([42; 32]).expect("key");
    let creator_token_id = id_of(&creator_token_key);
    let creator_coll_id = id_of(&creator_coll_key);
    let i = lbp_ids(id_of(&creator_key));

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state, lpad_guests::lbp().elf().to_vec());
    lbp_seed_fixture(&mut state, &i, creator_token_id, creator_coll_id);
    assert_whole_default(&state, i.creator, "before the initialise");

    initialize_creator(&mut state, &creator_key, 0);

    let create = lbp_create_msg(&i, creator_token_id, creator_coll_id, 0, 1);
    let witness = public_transaction::WitnessSet::for_message(
        &create,
        &[&creator_token_key, &creator_coll_key, &creator_key],
    );
    state
        .transition_from_public_transaction(&PublicTransaction::new(create, witness), 1, 0)
        .expect("create_sale must succeed with an initialised creator");
    assert_eq!(
        state.get_account_by_id(i.creator).program_owner,
        auth_transfer(),
        "the create echoes the creator unchanged, owner and all"
    );

    let raised = 20_000_u128;
    lbp_buy(&mut state, &i, raised, 2);
    let vault_collateral = bal(&state, i.collateral_vault);
    let unsold = bal(&state, i.token_vault);
    assert_eq!(
        vault_collateral,
        RESERVE_COLLATERAL + raised,
        "the LBP takes no per-swap fee, so the whole C_in is in the vault"
    );

    set_clock(&mut state, 3, T_END);
    let close = lbp_close_msg(&i, 2);
    let witness = public_transaction::WitnessSet::for_message(&close, &[&creator_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(close, witness), 3, T_END)
        .expect("an initialised creator must be able to close");
    let pool = PoolState::try_from(&state.get_account_by_id(i.pool).data).expect("pool");
    assert!(matches!(pool.status, LbpSaleStatus::Closed), "the pool really closed");

    let creator_coll = AccountId::new([90; 32]);
    let creator_tok = AccountId::new([91; 32]);
    state.force_insert_account(creator_coll, fungible(i.collateral_def, 0));
    state.force_insert_account(creator_tok, fungible(i.token_def, 0));
    let withdraw = lbp_withdraw_msg(&i, creator_coll, creator_tok, 3);
    let witness = public_transaction::WitnessSet::for_message(&withdraw, &[&creator_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(withdraw, witness), 4, T_END)
        .expect("an initialised creator must be able to withdraw");

    // The at-close fee taxes only the raised collateral and stays escrowed in the
    // vault for `SweepTreasury`; everything else is the creator's.
    let fee = close_fee(raised.min(vault_collateral), LBP_FEE_BPS);
    assert!(fee > 0, "the walk must exercise a non-zero at-close fee");
    assert_eq!(
        bal(&state, creator_coll),
        vault_collateral - fee,
        "the creator is paid their seed plus the raise, less the at-close fee"
    );
    assert_eq!(bal(&state, creator_tok), unsold, "and gets every unsold token back");
    assert_eq!(bal(&state, i.token_vault), 0, "token vault drained");
    assert_eq!(bal(&state, i.collateral_vault), fee, "only the escrowed fee is left behind");
    assert_eq!(
        state.get_account_by_id(i.creator).nonce,
        Nonce(4),
        "initialise, create, close, withdraw - four signatures, and none of them stranded it"
    );
}
