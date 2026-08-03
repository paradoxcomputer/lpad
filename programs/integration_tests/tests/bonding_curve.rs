//! End-to-end integration tests for the bonding-curve program against an
//! in-process LEZ state machine (`V03State`). Covers the public path (`Buy`)
//! and the private path (`BuyDisposable` - deshield → buy → re-shield through a
//! fresh ephemeral account A).
//!
//! The private path is driven as a public transaction whose ephemeral A
//! holdings are real signer keypairs; this validates the program's chained-call
//! machinery (unlinkability itself comes from running the same instruction
//! inside the LEZ privacy circuit, which the SDK does). Run with
//! `RISC0_DEV_MODE=1`.

use bonding_curve_core::{
    buy_tokens_out, compute_collateral_vault_pda, compute_sale_pda, compute_token_vault_pda,
    Instruction, SaleState, SaleStatus, CLOCK_01,
};
use lee::{
    program_deployment_transaction::{self, ProgramDeploymentTransaction},
    public_transaction, PrivateKey, PublicKey, PublicTransaction, V03State,
};
use lee_core::account::{Account, AccountId, Data, Nonce};
use token_core::{TokenDefinition, TokenHolding};

// sale parameters shared by the tests
const VT: u128 = 2_000_000;
const VC: u128 = 50_000;
const D: u128 = 1_000_000;
const R: u128 = 200_000;
const FEE_BPS: u128 = 100; // 1%
const NONCE: u64 = 0;

fn bc() -> lee_core::program::ProgramId {
    lpad_guests::bonding_curve().id()
}
fn token() -> lee_core::program::ProgramId {
    programs::token().id()
}
fn id_of(key: &PrivateKey) -> AccountId {
    AccountId::from(&PublicKey::new_from_private_key(key))
}
fn fungible(def: AccountId, balance: u128) -> Account {
    Account {
        program_owner: token(),
        balance: 0,
        data: Data::from(&TokenHolding::Fungible { definition_id: def, balance }),
        nonce: Nonce(0),
    }
}
/// A real Fungible token definition account (needed wherever a chained
/// `token::InitializeAccount` runs - e.g. pre-seeding the collateral vault).
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
fn deploy(state: &mut V03State) {
    // The built-in programs (token, ATA, clock, authenticated_transfer, ...) are
    // already registered by `testnet_initial_state::initial_state()`, so
    // re-deploying them now fails with `ProgramAlreadyExists`. Only lpad's own
    // program needs deploying.
    {
        let elf = lpad_guests::bonding_curve().elf().to_vec();
        let msg = program_deployment_transaction::Message::new(elf);
        state
            .transition_from_program_deployment_transaction(&ProgramDeploymentTransaction::new(msg))
            .expect("program deployment must succeed");
    }
}

/// Common ids derived from fixed token/collateral/creator/treasury values.
struct Ids {
    token_def: AccountId,
    collateral_def: AccountId,
    treasury: AccountId,
    creator: AccountId,
    sale: AccountId,
    token_vault: AccountId,
    collateral_vault: AccountId,
}
fn ids(creator: AccountId) -> Ids {
    let token_def = AccountId::new([7; 32]);
    let collateral_def = AccountId::new([8; 32]);
    let treasury = AccountId::new([9; 32]);
    let sale = compute_sale_pda(bc(), token_def, collateral_def, creator, NONCE);
    Ids {
        token_def,
        collateral_def,
        treasury,
        creator,
        sale,
        token_vault: compute_token_vault_pda(bc(), sale),
        collateral_vault: compute_collateral_vault_pda(bc(), sale),
    }
}

/// Build an already-open sale state owned by the bonding-curve program.
fn open_sale(i: &Ids, one_directional: bool) -> SaleState {
    SaleState {
        creator: i.creator,
        token_definition_id: i.token_def,
        collateral_definition_id: i.collateral_def,
        token_vault_id: i.token_vault,
        collateral_vault_id: i.collateral_vault,
        treasury_id: i.treasury,
        ata_program_id: programs::ata().id(),
        fee_bps: FEE_BPS,
        one_directional,
        end_timestamp_ms: 0,
        min_duration_ms: 0,
        nonce: NONCE,
        created_ts_ms: 0,
        virt_token: VT,
        virt_collateral: VC,
        k: VT * VC,
        sale_reserve_initial: D,
        dex_seed_reserve: R,
        sale_reserve: D,
        real_collateral: 0,
        status: SaleStatus::Open,
        cum_collateral_in: 0,
        cum_fees: 0,
        buy_count: 0,
        sell_count: 0,
        obs: Vec::new(),
        token_name: String::new(),
        token_symbol: String::new(),
    }
}

/// Seed an open sale: sale state (owned by BC), token vault (holding D+R), and
/// an empty treasury collateral holding.
fn seed_open_sale(state: &mut V03State, i: &Ids) {
    let sale_acc = Account {
        program_owner: bc(),
        data: Data::from(&open_sale(i, false)),
        ..Default::default()
    };
    state.force_insert_account(i.sale, sale_acc);
    state.force_insert_account(i.token_vault, fungible(i.token_def, D + R));
    state.force_insert_account(i.treasury, fungible(i.collateral_def, 0));
}

#[test]
fn create_sale_deposits_and_initializes_state() {
    let creator_token_key = PrivateKey::try_new([41; 32]).expect("key");
    let creator_key = PrivateKey::try_new([42; 32]).expect("key");
    let creator_token_id = id_of(&creator_token_key);
    let i = ids(id_of(&creator_key));

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    state.force_insert_account(creator_token_id, fungible(i.token_def, D + R));
    state.force_insert_account(i.creator, fungible(i.collateral_def, 0));
    // The collateral vault is pre-seeded at creation via token::InitializeAccount,
    // which needs a real collateral token definition on-chain.
    state.force_insert_account(i.collateral_def, token_def_account("Collateral"));

    let instruction = Instruction::CreateSale {
        collateral_definition_id: i.collateral_def,
        treasury_id: i.treasury,
        token_name: String::new(),
        token_symbol: String::new(),
        sale_quantity: D,
        dex_seed_quantity: R,
        virt_token: VT,
        virt_collateral: VC,
        fee_bps: FEE_BPS,
        one_directional: false,
        end_timestamp_ms: 0,
        min_duration_ms: 0,
        nonce: NONCE,
        ata_program_id: programs::ata().id(),
        deadline: u64::MAX,
    };
    let message = public_transaction::Message::try_new(
        bc(),
        vec![
            i.sale,
            i.token_vault,
            i.collateral_vault,
            i.collateral_def,
            creator_token_id,
            i.creator,
            CLOCK_01,
        ],
        vec![Nonce(0), Nonce(0)],
        instruction,
    )
    .expect("message");
    let witness =
        public_transaction::WitnessSet::for_message(&message, &[&creator_token_key, &creator_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(message, witness), 0, 0)
        .expect("create_sale must succeed");

    let sale = SaleState::try_from(&state.get_account_by_id(i.sale).data).expect("sale");
    assert_eq!(state.get_account_by_id(i.sale).program_owner, bc());
    assert_eq!(sale.sale_reserve, D);
    assert_eq!(sale.k, VT * VC);
    assert_eq!(sale.ata_program_id, programs::ata().id(), "ATA program pinned at creation");
    assert!(matches!(sale.status, SaleStatus::Open));
    assert_eq!(bal(&state, i.token_vault), D + R);
    assert_eq!(bal(&state, creator_token_id), 0);
    // The collateral vault is pre-seeded (initialized empty, typed to collateral).
    assert_eq!(bal(&state, i.collateral_vault), 0, "collateral vault initialized empty at creation");
}

#[test]
fn public_buy_moves_tokens_collateral_and_fee() {
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_open_sale(&mut state, &i);

    let buyer_key = PrivateKey::try_new([60; 32]).unwrap();
    let buyer_coll = id_of(&buyer_key);
    let buyer_tok = AccountId::new([61; 32]); // pre-initialised recipient (no signature needed)
    let buyer_start: u128 = 100_000;
    let collateral_in: u128 = 5_000;
    state.force_insert_account(buyer_coll, fungible(i.collateral_def, buyer_start));
    state.force_insert_account(buyer_tok, fungible(i.token_def, 0));

    let (tokens_out, fee, c_eff) = buy_tokens_out(VT, VC, FEE_BPS, collateral_in);
    assert!(tokens_out > 0 && fee > 0);

    let instruction = Instruction::Buy { collateral_in, min_tokens_out: 0, deadline: u64::MAX };
    let message = public_transaction::Message::try_new(
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
        instruction,
    )
    .unwrap();
    let witness = public_transaction::WitnessSet::for_message(&message, &[&buyer_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(message, witness), 0, 0)
        .expect("public buy must succeed");

    assert_eq!(bal(&state, buyer_tok), tokens_out, "buyer received tokens_out");
    assert_eq!(bal(&state, buyer_coll), buyer_start - collateral_in, "buyer paid C_in");
    assert_eq!(bal(&state, i.collateral_vault), c_eff, "vault holds effective collateral");
    assert_eq!(bal(&state, i.treasury), fee, "treasury holds the fee");
    assert_eq!(bal(&state, i.token_vault), D + R - tokens_out, "token vault drained by tokens_out");

    let sale = SaleState::try_from(&state.get_account_by_id(i.sale).data).unwrap();
    assert_eq!(sale.sale_reserve, D - tokens_out);
    assert_eq!(sale.virt_token, VT - tokens_out);
    assert_eq!(sale.virt_collateral, VC + c_eff);
    assert_eq!(sale.real_collateral, c_eff);
    assert_eq!(sale.cum_fees, fee);
    assert_eq!(sale.buy_count, 1);
}

/// The `AtomicDisposable` (3-tx) private buy's middle leg is a PUBLIC `Buy`,
/// which is what makes the 3-tx decomposition drift-free and the in-proof
/// `BuyDisposable` not: the public leg re-prices against LIVE sale state, so a
/// competing buy that lands first does NOT invalidate it (whereas an in-proof
/// buy commits the sale PDA pre-state and would be rejected stale). Also
/// confirms a fresh account-A token holding is initialised by the buy.
#[test]
fn atomic_disposable_public_buy_leg_is_drift_free() {
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_open_sale(&mut state, &i);

    let collateral_in: u128 = 5_000;
    // What A would get against the INITIAL reserves - the snapshot an in-proof
    // buy commits to and then drifts off of.
    let (stale_out, _, _) = buy_tokens_out(VT, VC, FEE_BPS, collateral_in);

    // A competing public buy lands first (after A "deshielded", before A's buy)
    // and moves the curve. Also lazily initialises the collateral vault.
    let comp_key = PrivateKey::try_new([80; 32]).unwrap();
    let comp_coll = id_of(&comp_key);
    let comp_tok = AccountId::new([81; 32]);
    state.force_insert_account(comp_coll, fungible(i.collateral_def, 100_000));
    state.force_insert_account(comp_tok, fungible(i.token_def, 0));
    let m = public_transaction::Message::try_new(
        bc(),
        vec![i.sale, i.token_vault, i.collateral_vault, i.treasury, comp_coll, comp_tok, CLOCK_01],
        vec![Nonce(0)],
        Instruction::Buy { collateral_in: 7_000, min_tokens_out: 0, deadline: u64::MAX },
    )
    .unwrap();
    let w = public_transaction::WitnessSet::for_message(&m, &[&comp_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, w), 0, 0)
        .expect("competing buy must succeed");

    let sale_after = SaleState::try_from(&state.get_account_by_id(i.sale).data).unwrap();
    assert!(sale_after.virt_token < VT && sale_after.virt_collateral > VC, "competing buy moved the curve");

    // Output priced against the LIVE (moved) reserves - what the public leg yields.
    let (live_out, _, _) =
        buy_tokens_out(sale_after.virt_token, sale_after.virt_collateral, FEE_BPS, collateral_in);
    assert_ne!(live_out, stale_out, "live vs stale output must differ so drift is exercised");

    // After the deshield leg, A's collateral is funded. The token-out holding is
    // a FRESH account that CO-SIGNS the buy, so its
    // new_claimed_if_default(Authorized) is satisfied without a separate init -
    // exactly what the SDK's AtomicDisposable path does (wallet owns a_token's
    // key and adds it as a signer).
    let a_key = PrivateKey::try_new([82; 32]).unwrap();
    let a_tok_key = PrivateKey::try_new([83; 32]).unwrap();
    let a_coll = id_of(&a_key);
    let a_tok = id_of(&a_tok_key);
    state.force_insert_account(a_coll, fungible(i.collateral_def, collateral_in));

    // A's drift-free public buy leg against the moved sale; a_coll + a_tok sign.
    let m = public_transaction::Message::try_new(
        bc(),
        vec![i.sale, i.token_vault, i.collateral_vault, i.treasury, a_coll, a_tok, CLOCK_01],
        vec![Nonce(0), Nonce(0)],
        Instruction::Buy { collateral_in, min_tokens_out: 0, deadline: u64::MAX },
    )
    .unwrap();
    let w = public_transaction::WitnessSet::for_message(&m, &[&a_key, &a_tok_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, w), 0, 0)
        .expect("public buy leg must succeed against the live (moved) sale");

    // a_tok credited at the LIVE price - the public leg re-priced against the
    // moved sale, not a stale snapshot. This is the essence of drift-freedom.
    assert_eq!(bal(&state, a_tok), live_out, "output holding credited at the live price");
    assert!(live_out > 0);
    assert_eq!(bal(&state, a_coll), 0, "all deshielded collateral consumed by the public buy");
}
