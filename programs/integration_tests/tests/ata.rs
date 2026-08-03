//! End-to-end integration tests for the **ATA** token-interaction path (RFP-015
//! Func #7 / RFP-016 Func #9: "use Associated Token Accounts for all token
//! interactions", per LP-0014 / RFP-008).
//!
//! These exercise the real chained-call machinery: a `BuyAta`/`SellAta` routes
//! the user leg through the ATA program (an `ata::Transfer` that internally
//! chains a `token::Transfer` from the ATA-PDA), while vault legs stay
//! vault-PDA-authorized `token::Transfer`s. Because chained calls execute
//! depth-first, the fee-sweep's pre-state shift must match the running diff -
//! these tests prove it does. Run with `RISC0_DEV_MODE=1`.

use ata_core::{compute_ata_seed, get_associated_token_account_id};
use bonding_curve_core::{
    buy_tokens_out, compute_collateral_vault_pda, compute_sale_pda, compute_token_vault_pda,
    sell_collateral_out, Instruction, SaleState, SaleStatus, CLOCK_01,
};
use lee::{
    program_deployment_transaction::{self, ProgramDeploymentTransaction},
    public_transaction, PrivateKey, PublicKey, PublicTransaction, V03State,
};
use lee_core::account::{Account, AccountId, Data, Nonce};
use lee_core::program::ProgramId;
use token_core::TokenHolding;

const VT: u128 = 2_000_000;
const VC: u128 = 50_000;
const D: u128 = 1_000_000;
const R: u128 = 200_000;
const FEE_BPS: u128 = 100; // 1%
const NONCE: u64 = 0;

fn bc() -> ProgramId {
    lpad_guests::bonding_curve().id()
}
fn token() -> ProgramId {
    programs::token().id()
}
fn ata_prog() -> ProgramId {
    programs::ata().id()
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
fn bal(state: &V03State, id: AccountId) -> u128 {
    match TokenHolding::try_from(&state.get_account_by_id(id).data) {
        Ok(TokenHolding::Fungible { balance, .. }) => balance,
        _ => panic!("account {id:?} is not a fungible holding"),
    }
}
/// Canonical ATA address for `(owner, definition)` under the ATA program.
fn ata_addr(owner: AccountId, def: AccountId) -> AccountId {
    get_associated_token_account_id(&ata_prog(), &compute_ata_seed(owner, def))
}
fn deploy(state: &mut V03State) {
    // The built-in programs (token, ATA, clock, authenticated_transfer, ...) are
    // already registered by `testnet_initial_state::initial_state()`, so
    // re-deploying them now fails with `ProgramAlreadyExists`. Only lpad's own
    // program needs deploying.
    {
        let elf = lpad_guests::bonding_curve().elf().to_vec();
        assert!(
            !elf.is_empty(),
            "guest ELFs not built; run without RISC0_SKIP_BUILD"
        );
        let msg = program_deployment_transaction::Message::new(elf);
        state
            .transition_from_program_deployment_transaction(&ProgramDeploymentTransaction::new(msg))
            .expect("program deployment must succeed");
    }
}

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

/// An open sale with `real_collateral` already raised (so sells can pay out).
fn open_sale(i: &Ids, real_collateral: u128) -> SaleState {
    SaleState {
        creator: i.creator,
        token_definition_id: i.token_def,
        collateral_definition_id: i.collateral_def,
        token_vault_id: i.token_vault,
        collateral_vault_id: i.collateral_vault,
        treasury_id: i.treasury,
        ata_program_id: ata_prog(),
        fee_bps: FEE_BPS,
        one_directional: false,
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
        real_collateral,
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

/// Seed an open sale with the collateral vault PRE-SEEDED (as `CreateSale` now
/// does), since the ATA collateral leg's `ata::Transfer` rejects a default
/// recipient. `vault_collateral` seeds the collateral vault balance.
fn seed_sale(state: &mut V03State, i: &Ids, real_collateral: u128, vault_collateral: u128) {
    let sale_acc = Account {
        program_owner: bc(),
        data: Data::from(&open_sale(i, real_collateral)),
        ..Default::default()
    };
    state.force_insert_account(i.sale, sale_acc);
    state.force_insert_account(i.token_vault, fungible(i.token_def, D + R));
    state.force_insert_account(i.collateral_vault, fungible(i.collateral_def, vault_collateral));
    state.force_insert_account(i.treasury, fungible(i.collateral_def, 0));
}

#[test]
fn bc_buy_ata_routes_collateral_and_tokens_through_atas() {
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_sale(&mut state, &i, 0, 0);

    let owner_key = PrivateKey::try_new([70; 32]).unwrap();
    let owner = id_of(&owner_key);
    let collateral_ata = ata_addr(owner, i.collateral_def);
    let token_ata = ata_addr(owner, i.token_def);
    let buyer_start: u128 = 100_000;
    let collateral_in: u128 = 5_000;
    state.force_insert_account(collateral_ata, fungible(i.collateral_def, buyer_start));
    state.force_insert_account(token_ata, fungible(i.token_def, 0));

    let (tokens_out, fee, c_eff) = buy_tokens_out(VT, VC, FEE_BPS, collateral_in);
    assert!(tokens_out > 0 && fee > 0, "test must exercise a non-zero fee sweep");

    let instruction = Instruction::BuyAta {
        collateral_in,
        min_tokens_out: 0,
        ata_program_id: ata_prog(),
        deadline: u64::MAX,
    };
    let message = public_transaction::Message::try_new(
        bc(),
        vec![
            i.sale,
            i.token_vault,
            i.collateral_vault,
            i.treasury,
            owner,
            collateral_ata,
            token_ata,
            CLOCK_01,
        ],
        vec![Nonce(0)],
        instruction,
    )
    .unwrap();
    let witness = public_transaction::WitnessSet::for_message(&message, &[&owner_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(message, witness), 0, 0)
        .expect("buy_ata must succeed");

    assert_eq!(bal(&state, token_ata), tokens_out, "buyer token ATA received tokens_out");
    assert_eq!(bal(&state, collateral_ata), buyer_start - collateral_in, "buyer collateral ATA paid C_in");
    assert_eq!(bal(&state, i.collateral_vault), c_eff, "collateral vault holds c_eff after fee sweep");
    assert_eq!(bal(&state, i.treasury), fee, "treasury swept the fee");
    assert_eq!(bal(&state, i.token_vault), D + R - tokens_out, "token vault drained by tokens_out");

    let sale = SaleState::try_from(&state.get_account_by_id(i.sale).data).unwrap();
    assert_eq!(sale.real_collateral, c_eff);
    assert_eq!(sale.cum_fees, fee);
    assert_eq!(sale.buy_count, 1);
    assert_eq!(sale.virt_token, VT - tokens_out);
    assert_eq!(sale.virt_collateral, VC + c_eff);
}

#[test]
fn bc_sell_ata_routes_tokens_and_collateral_through_atas() {
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    // Pre-raised sale: 50k real collateral, vault funded with the same.
    let raised: u128 = 50_000;
    seed_sale(&mut state, &i, raised, raised);

    let owner_key = PrivateKey::try_new([71; 32]).unwrap();
    let owner = id_of(&owner_key);
    let token_ata = ata_addr(owner, i.token_def);
    let collateral_ata = ata_addr(owner, i.collateral_def);
    let tokens_in: u128 = 10_000;
    state.force_insert_account(token_ata, fungible(i.token_def, tokens_in));
    state.force_insert_account(collateral_ata, fungible(i.collateral_def, 0));

    let (to_seller, fee, c_out_raw) = sell_collateral_out(VT, VC, FEE_BPS, tokens_in);
    assert!(to_seller > 0 && fee > 0 && c_out_raw <= raised, "sell must pay out within the reserve");

    let instruction = Instruction::SellAta {
        tokens_in,
        min_collateral_out: 0,
        ata_program_id: ata_prog(),
        deadline: u64::MAX,
    };
    let message = public_transaction::Message::try_new(
        bc(),
        vec![
            i.sale,
            i.token_vault,
            i.collateral_vault,
            i.treasury,
            owner,
            token_ata,
            collateral_ata,
            CLOCK_01,
        ],
        vec![Nonce(0)],
        instruction,
    )
    .unwrap();
    let witness = public_transaction::WitnessSet::for_message(&message, &[&owner_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(message, witness), 0, 0)
        .expect("sell_ata must succeed");

    assert_eq!(bal(&state, token_ata), 0, "seller token ATA emptied");
    assert_eq!(bal(&state, i.token_vault), D + R + tokens_in, "token vault received returned tokens");
    assert_eq!(bal(&state, collateral_ata), to_seller, "seller collateral ATA paid net of fee");
    assert_eq!(bal(&state, i.treasury), fee, "treasury swept the sell fee");
    assert_eq!(bal(&state, i.collateral_vault), raised - c_out_raw, "collateral vault released c_out_raw");

    let sale = SaleState::try_from(&state.get_account_by_id(i.sale).data).unwrap();
    assert_eq!(sale.real_collateral, raised - c_out_raw);
    assert_eq!(sale.sell_count, 1);
    assert_eq!(sale.virt_token, VT + tokens_in);
}

/// SECURITY regression: a `BuyAta` that names an ATA program other than the one
/// pinned at creation must revert. This is the defense against the no-op-drain
/// attack - substituting a program that skips the real collateral `token::Transfer`
/// while the vault still pays out tokens. The pinned id (`ata_prog()`) is set in
/// `open_sale`; here we submit with `token()` as a stand-in wrong program.
#[test]
fn bc_buy_ata_rejects_a_substituted_ata_program() {
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_sale(&mut state, &i, 0, 0);

    let owner_key = PrivateKey::try_new([72; 32]).unwrap();
    let owner = id_of(&owner_key);
    let collateral_ata = ata_addr(owner, i.collateral_def);
    let token_ata = ata_addr(owner, i.token_def);
    state.force_insert_account(collateral_ata, fungible(i.collateral_def, 100_000));
    state.force_insert_account(token_ata, fungible(i.token_def, 0));

    let instruction = Instruction::BuyAta {
        collateral_in: 5_000,
        min_tokens_out: 0,
        ata_program_id: token(), // NOT the pinned ata_prog()
        deadline: u64::MAX,
    };
    let message = public_transaction::Message::try_new(
        bc(),
        vec![i.sale, i.token_vault, i.collateral_vault, i.treasury, owner, collateral_ata, token_ata, CLOCK_01],
        vec![Nonce(0)],
        instruction,
    )
    .unwrap();
    let witness = public_transaction::WitnessSet::for_message(&message, &[&owner_key]);
    let result =
        state.transition_from_public_transaction(&PublicTransaction::new(message, witness), 0, 0);
    assert!(result.is_err(), "BuyAta with a non-pinned ATA program must revert");
    // Vault untouched - no drain.
    assert_eq!(bal(&state, i.token_vault), D + R, "token vault unchanged after revert");
}
