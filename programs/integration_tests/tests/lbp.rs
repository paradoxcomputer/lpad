//! End-to-end integration tests for the LBP program against an in-process LEZ
//! state machine. Exercises the **public** buy (clock-priced) and the
//! **private** disposable buy (deshield → buy → re-shield), mirroring the
//! bonding-curve E2E tests. Run with `RISC0_DEV_MODE=1`.

use ata_core::{compute_ata_seed, get_associated_token_account_id};
use lbp_core::{
    allowlist_leaf, buy_tokens_out, close_fee, compute_collateral_vault_pda, compute_pool_pda,
    compute_token_vault_pda, weight_token_q64, Instruction, PoolState, SaleStatus, CLOCK_01,
};
use nssa::{
    program_deployment_transaction::{self, ProgramDeploymentTransaction},
    public_transaction, PrivateKey, PublicKey, PublicTransaction, V03State,
};
use nssa_core::account::{Account, AccountId, Data, Nonce};
use token_core::TokenHolding;

const RESERVE_TOKEN: u128 = 1_000_000;
const RESERVE_COLLATERAL: u128 = 50_000;
const T_START: u64 = 0;
const T_END: u64 = 1_000_000;
const FEE_BPS: u128 = 500; // 5% at close
const NONCE: u64 = 0;

fn lbp() -> nssa_core::program::ProgramId {
    lbp_methods::LBP_ID
}
fn token() -> nssa_core::program::ProgramId {
    token_methods::TOKEN_ID
}
fn ata_prog() -> nssa_core::program::ProgramId {
    ata_methods::ATA_ID
}
fn ata_addr(owner: AccountId, def: AccountId) -> AccountId {
    get_associated_token_account_id(&ata_prog(), &compute_ata_seed(owner, def))
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
fn bal(state: &V03State, id: AccountId) -> u128 {
    match TokenHolding::try_from(&state.get_account_by_id(id).data) {
        Ok(TokenHolding::Fungible { balance, .. }) => balance,
        _ => panic!("account {id:?} is not a fungible holding"),
    }
}
fn deploy(state: &mut V03State) {
    for elf in [token_methods::TOKEN_ELF.to_vec(), lbp_methods::LBP_ELF.to_vec()] {
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
    pool: AccountId,
    token_vault: AccountId,
    collateral_vault: AccountId,
}
fn ids(creator: AccountId) -> Ids {
    let token_def = AccountId::new([7; 32]);
    let collateral_def = AccountId::new([8; 32]);
    let pool = compute_pool_pda(lbp(), token_def, collateral_def, creator, NONCE);
    Ids {
        token_def,
        collateral_def,
        treasury: AccountId::new([9; 32]),
        creator,
        pool,
        token_vault: compute_token_vault_pda(lbp(), pool),
        collateral_vault: compute_collateral_vault_pda(lbp(), pool),
    }
}

fn open_pool(i: &Ids) -> PoolState {
    PoolState {
        creator: i.creator,
        token_definition_id: i.token_def,
        collateral_definition_id: i.collateral_def,
        token_vault_id: i.token_vault,
        collateral_vault_id: i.collateral_vault,
        treasury_id: i.treasury,
        ata_program_id: ata_prog(),
        fee_bps: FEE_BPS,
        w_start_q64: w(99, 100),
        w_end_q64: w(1, 100),
        t_start_ms: T_START,
        t_end_ms: T_END,
        min_duration_ms: 0,
        reserve_token: RESERVE_TOKEN,
        reserve_collateral: RESERVE_COLLATERAL,
        stored_w_token_q64: w(99, 100),
        stored_w_ts_ms: T_START,
        paused: false,
        block_token_ceiling: 0,
        block_sold: 0,
        block_window_id: 0,
        allowlist_root: [0; 32],
        fixed_price: false,
        fixed_price_q64: 0,
        nonce: NONCE,
        created_ts_ms: 0,
        status: SaleStatus::Open,
        cum_collateral_in: 0,
        cum_tokens_out: 0,
        buy_count: 0,
        obs: Vec::new(),
        token_name: String::new(),
        token_symbol: String::new(),
    }
}

fn seed_open_pool(state: &mut V03State, i: &Ids) {
    let mut pool_acc = Account::default();
    pool_acc.program_owner = lbp();
    pool_acc.data = Data::from(&open_pool(i));
    state.force_insert_account(i.pool, pool_acc);
    state.force_insert_account(i.token_vault, fungible(i.token_def, RESERVE_TOKEN));
}

/// Seed an open pool gated by `allowlist_root`.
fn seed_gated_pool(state: &mut V03State, i: &Ids, allowlist_root: [u8; 32]) {
    let mut p = open_pool(i);
    p.allowlist_root = allowlist_root;
    let mut pool_acc = Account::default();
    pool_acc.program_owner = lbp();
    pool_acc.data = Data::from(&p);
    state.force_insert_account(i.pool, pool_acc);
    state.force_insert_account(i.token_vault, fungible(i.token_def, RESERVE_TOKEN));
}

/// Build a `BuyGated` message for `buyer` with `(leaf, proof)`.
fn gated_msg(
    i: &Ids,
    buyer_coll: AccountId,
    buyer_tok: AccountId,
    leaf: [u8; 32],
    proof: Vec<[u8; 32]>,
    collateral_in: u128,
) -> public_transaction::Message {
    public_transaction::Message::try_new(
        lbp(),
        vec![i.pool, i.token_vault, i.collateral_vault, buyer_coll, buyer_tok, CLOCK_01],
        vec![Nonce(0)],
        Instruction::BuyGated { collateral_in, min_tokens_out: 0, leaf, proof, deadline: u64::MAX },
    )
    .unwrap()
}

fn expected_tokens_out(collateral_in: u128) -> u128 {
    let wt = weight_token_q64(w(99, 100), w(1, 100), T_START, T_END, 0);
    buy_tokens_out(RESERVE_TOKEN, RESERVE_COLLATERAL, wt, collateral_in)
}

#[test]
fn public_buy_moves_tokens_and_collateral() {
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = V03State::new_with_genesis_accounts(&[], vec![], 0);
    deploy(&mut state);
    seed_open_pool(&mut state, &i);

    let buyer_key = PrivateKey::try_new([60; 32]).unwrap();
    let buyer_coll = id_of(&buyer_key);
    let buyer_tok = AccountId::new([61; 32]);
    let buyer_start: u128 = 100_000;
    let collateral_in: u128 = 5_000;
    state.force_insert_account(buyer_coll, fungible(i.collateral_def, buyer_start));
    state.force_insert_account(buyer_tok, fungible(i.token_def, 0));

    let tokens_out = expected_tokens_out(collateral_in);
    assert!(tokens_out > 0);

    let instruction = Instruction::Buy { collateral_in, min_tokens_out: 0, deadline: u64::MAX };
    let message = public_transaction::Message::try_new(
        lbp(),
        vec![i.pool, i.token_vault, i.collateral_vault, buyer_coll, buyer_tok, CLOCK_01],
        vec![Nonce(0)],
        instruction,
    )
    .unwrap();
    let witness = public_transaction::WitnessSet::for_message(&message, &[&buyer_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(message, witness), 0, 0)
        .expect("public LBP buy must succeed");

    assert_eq!(bal(&state, buyer_tok), tokens_out, "buyer received tokens");
    assert_eq!(bal(&state, buyer_coll), buyer_start - collateral_in, "buyer paid C_in");
    assert_eq!(bal(&state, i.collateral_vault), collateral_in, "no per-swap fee on LBP buy");
    assert_eq!(bal(&state, i.token_vault), RESERVE_TOKEN - tokens_out);

    let pool = PoolState::try_from(&state.get_account_by_id(i.pool).data).unwrap();
    assert_eq!(pool.reserve_token, RESERVE_TOKEN - tokens_out);
    assert_eq!(pool.reserve_collateral, RESERVE_COLLATERAL + collateral_in);
    assert_eq!(pool.buy_count, 1);
}

/// LBP buy with the buyer side using ATAs (RFP-016 Func #9). The collateral leg
/// is routed through the ATA program; tokens land in the buyer's token ATA. The
/// collateral vault is pre-seeded (as `CreateSale` does) so the `ata::Transfer`
/// recipient is never default.
#[test]
fn buy_ata_routes_collateral_and_tokens_through_atas() {
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = V03State::new_with_genesis_accounts(&[], vec![], 0);
    deploy(&mut state);
    // The ATA path dispatches through the ATA program, so deploy it too.
    let ata_msg = program_deployment_transaction::Message::new(ata_methods::ATA_ELF.to_vec());
    state
        .transition_from_program_deployment_transaction(&ProgramDeploymentTransaction::new(ata_msg))
        .expect("ata program deployment must succeed");
    seed_open_pool(&mut state, &i);
    // LBP seeds the collateral vault at creation with the collateral reserve.
    state.force_insert_account(i.collateral_vault, fungible(i.collateral_def, RESERVE_COLLATERAL));

    let owner_key = PrivateKey::try_new([72; 32]).unwrap();
    let owner = id_of(&owner_key);
    let collateral_ata = ata_addr(owner, i.collateral_def);
    let token_ata = ata_addr(owner, i.token_def);
    let buyer_start: u128 = 100_000;
    let collateral_in: u128 = 5_000;
    state.force_insert_account(collateral_ata, fungible(i.collateral_def, buyer_start));
    state.force_insert_account(token_ata, fungible(i.token_def, 0));

    let tokens_out = expected_tokens_out(collateral_in);
    assert!(tokens_out > 0);

    let instruction = Instruction::BuyAta {
        collateral_in,
        min_tokens_out: 0,
        ata_program_id: ata_prog(),
        deadline: u64::MAX,
    };
    let message = public_transaction::Message::try_new(
        lbp(),
        vec![i.pool, i.token_vault, i.collateral_vault, owner, collateral_ata, token_ata, CLOCK_01],
        vec![Nonce(0)],
        instruction,
    )
    .unwrap();
    let witness = public_transaction::WitnessSet::for_message(&message, &[&owner_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(message, witness), 0, 0)
        .expect("LBP buy_ata must succeed");

    assert_eq!(bal(&state, token_ata), tokens_out, "buyer token ATA received tokens");
    assert_eq!(bal(&state, collateral_ata), buyer_start - collateral_in, "buyer collateral ATA paid C_in");
    assert_eq!(
        bal(&state, i.collateral_vault),
        RESERVE_COLLATERAL + collateral_in,
        "collateral vault received C_in (LBP has no per-swap fee)"
    );
    assert_eq!(bal(&state, i.token_vault), RESERVE_TOKEN - tokens_out);

    let pool = PoolState::try_from(&state.get_account_by_id(i.pool).data).unwrap();
    assert_eq!(pool.reserve_token, RESERVE_TOKEN - tokens_out);
    assert_eq!(pool.reserve_collateral, RESERVE_COLLATERAL + collateral_in);
    assert_eq!(pool.buy_count, 1);
}

/// SECURITY regression: an LBP `BuyAta` that names an ATA program other than the
/// one pinned at pool creation must revert. This is the defense against the
/// no-op-drain attack - substituting a program that skips the real collateral
/// `token::Transfer` while the token vault still pays out (LBP has no fee leg to
/// cross-check the deposit). The pinned id (`ata_prog()`) is set in `open_pool`;
/// here we submit with `token()` as a stand-in wrong program.
#[test]
fn buy_ata_rejects_a_substituted_ata_program() {
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = V03State::new_with_genesis_accounts(&[], vec![], 0);
    deploy(&mut state);
    seed_open_pool(&mut state, &i);
    state.force_insert_account(i.collateral_vault, fungible(i.collateral_def, RESERVE_COLLATERAL));

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
        lbp(),
        vec![i.pool, i.token_vault, i.collateral_vault, owner, collateral_ata, token_ata, CLOCK_01],
        vec![Nonce(0)],
        instruction,
    )
    .unwrap();
    let witness = public_transaction::WitnessSet::for_message(&message, &[&owner_key]);
    let result =
        state.transition_from_public_transaction(&PublicTransaction::new(message, witness), 0, 0);
    assert!(result.is_err(), "LBP BuyAta with a non-pinned ATA program must revert");
    // Vault untouched - no drain.
    assert_eq!(bal(&state, i.token_vault), RESERVE_TOKEN, "token vault unchanged after revert");
}

// --- Allowlist gate (BuyGated) -------------------------------------------
// Single-member allowlists: the root IS the member's leaf and the proof is
// empty (the fold of a 1-leaf tree), which exercises the full on-chain gate
// (open-pool guard, buyer leaf-binding, Merkle verify) without rebuilding the
// sorted-pair hash here. Rejections surface as a failed state transition.

fn fund_buyer(state: &mut V03State, i: &Ids, buyer_coll: AccountId, buyer_tok: AccountId, start: u128) {
    state.force_insert_account(buyer_coll, fungible(i.collateral_def, start));
    state.force_insert_account(buyer_tok, fungible(i.token_def, 0));
}

#[test]
fn gated_buy_admits_an_allowlisted_member() {
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = V03State::new_with_genesis_accounts(&[], vec![], 0);
    deploy(&mut state);

    let buyer_key = PrivateKey::try_new([60; 32]).unwrap();
    let buyer_coll = id_of(&buyer_key);
    let buyer_tok = AccountId::new([61; 32]);
    let leaf = allowlist_leaf(&buyer_coll); // 1-member root == the member's leaf
    seed_gated_pool(&mut state, &i, leaf);
    fund_buyer(&mut state, &i, buyer_coll, buyer_tok, 100_000);

    let collateral_in = 5_000u128;
    let tokens_out = expected_tokens_out(collateral_in);
    let msg = gated_msg(&i, buyer_coll, buyer_tok, leaf, vec![], collateral_in);
    let witness = public_transaction::WitnessSet::for_message(&msg, &[&buyer_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(msg, witness), 0, 0)
        .expect("a gated buy by an allowlisted member must succeed");
    assert_eq!(bal(&state, buyer_tok), tokens_out, "member received tokens");
}

#[test]
fn gated_buy_rejects_a_non_member() {
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = V03State::new_with_genesis_accounts(&[], vec![], 0);
    deploy(&mut state);

    // The allowlist commits to a DIFFERENT member; the buyer is not in it.
    let member = id_of(&PrivateKey::try_new([99; 32]).unwrap());
    seed_gated_pool(&mut state, &i, allowlist_leaf(&member));

    let buyer_key = PrivateKey::try_new([60; 32]).unwrap();
    let buyer_coll = id_of(&buyer_key);
    let buyer_tok = AccountId::new([61; 32]);
    fund_buyer(&mut state, &i, buyer_coll, buyer_tok, 100_000);

    // The buyer honestly presents their own leaf; it isn't in the tree.
    let msg = gated_msg(&i, buyer_coll, buyer_tok, allowlist_leaf(&buyer_coll), vec![], 5_000);
    let witness = public_transaction::WitnessSet::for_message(&msg, &[&buyer_key]);
    let res = state.transition_from_public_transaction(&PublicTransaction::new(msg, witness), 0, 0);
    assert!(res.is_err(), "a non-member must be rejected by the Merkle gate");
    assert_eq!(bal(&state, buyer_tok), 0, "no tokens delivered on a rejected gated buy");
}

#[test]
fn gated_buy_rejects_a_replayed_foreign_leaf() {
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = V03State::new_with_genesis_accounts(&[], vec![], 0);
    deploy(&mut state);

    // The real allowlisted member's leaf is the root.
    let member = id_of(&PrivateKey::try_new([99; 32]).unwrap());
    let member_leaf = allowlist_leaf(&member);
    seed_gated_pool(&mut state, &i, member_leaf);

    // An attacker replays the member's published leaf but signs as themselves -
    // the buyer leaf-binding assert (`leaf == allowlist_leaf(buyer)`) rejects it.
    let attacker_key = PrivateKey::try_new([60; 32]).unwrap();
    let attacker_coll = id_of(&attacker_key);
    let attacker_tok = AccountId::new([61; 32]);
    fund_buyer(&mut state, &i, attacker_coll, attacker_tok, 100_000);

    let msg = gated_msg(&i, attacker_coll, attacker_tok, member_leaf, vec![], 5_000);
    let witness = public_transaction::WitnessSet::for_message(&msg, &[&attacker_key]);
    let res = state.transition_from_public_transaction(&PublicTransaction::new(msg, witness), 0, 0);
    assert!(res.is_err(), "a replayed foreign leaf must be rejected (leaf must be the buyer's own)");
    assert_eq!(bal(&state, attacker_tok), 0);
}

#[test]
fn gated_buy_reverts_on_open_pool() {
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = V03State::new_with_genesis_accounts(&[], vec![], 0);
    deploy(&mut state);
    seed_open_pool(&mut state, &i); // allowlist_root = [0; 32] (open)

    let buyer_key = PrivateKey::try_new([60; 32]).unwrap();
    let buyer_coll = id_of(&buyer_key);
    let buyer_tok = AccountId::new([61; 32]);
    fund_buyer(&mut state, &i, buyer_coll, buyer_tok, 100_000);

    let msg = gated_msg(&i, buyer_coll, buyer_tok, allowlist_leaf(&buyer_coll), vec![], 5_000);
    let witness = public_transaction::WitnessSet::for_message(&msg, &[&buyer_key]);
    let res = state.transition_from_public_transaction(&PublicTransaction::new(msg, witness), 0, 0);
    assert!(res.is_err(), "BuyGated against an open (un-gated) pool must revert");
}

// --- At-close fee / withdraw (RFP-016 Func #5) ---------------------------
// The at-close protocol fee is the most subtle accounting in the program: it
// taxes ONLY buyer-raised collateral (cum_collateral_in), not the creator's own
// seed, via `close_fee(cum_collateral_in.min(balance), fee_bps)`, then pays the
// creator the net collateral + unsold tokens in a 3-leg chained sequence.

#[test]
fn withdraw_taxes_only_raised_collateral_and_pays_the_creator() {
    let creator_key = PrivateKey::try_new([42; 32]).unwrap();
    let creator = id_of(&creator_key);
    let i = ids(creator);
    let mut state = V03State::new_with_genesis_accounts(&[], vec![], 0);
    deploy(&mut state);

    // A CLOSED pool that raised 10_000 from buyers on top of a 5_000 creator seed.
    let raised = 10_000u128; // == cum_collateral_in
    let seed = 5_000u128; // creator's own collateral seed
    let vault_collateral = seed + raised; // 15_000 sits in the collateral vault
    let unsold_tokens = 200_000u128;
    let mut p = open_pool(&i);
    p.status = SaleStatus::Closed;
    p.cum_collateral_in = raised;
    let mut pool_acc = Account::default();
    pool_acc.program_owner = lbp();
    pool_acc.data = Data::from(&p);
    state.force_insert_account(i.pool, pool_acc);
    state.force_insert_account(i.collateral_vault, fungible(i.collateral_def, vault_collateral));
    state.force_insert_account(i.token_vault, fungible(i.token_def, unsold_tokens));
    state.force_insert_account(i.treasury, fungible(i.collateral_def, 0));
    let cc = AccountId::new([70; 32]); // creator collateral destination
    let ct = AccountId::new([71; 32]); // creator token destination
    state.force_insert_account(cc, fungible(i.collateral_def, 0));
    state.force_insert_account(ct, fungible(i.token_def, 0));

    // Fee taxes only the raised collateral, capped at the balance - not the seed.
    let fee = close_fee(raised.min(vault_collateral), p.fee_bps); // 5% of 10_000 = 500
    let net = vault_collateral - fee;
    assert!(fee > 0 && net > seed, "test must exercise a non-zero fee on the raised portion");

    let message = public_transaction::Message::try_new(
        lbp(),
        vec![i.pool, i.token_vault, i.collateral_vault, i.treasury, cc, ct, creator],
        vec![Nonce(0)],
        Instruction::Withdraw { deadline: u64::MAX },
    )
    .unwrap();
    let witness = public_transaction::WitnessSet::for_message(&message, &[&creator_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(message, witness), 0, 0)
        .expect("creator withdraw must succeed");

    assert_eq!(bal(&state, i.treasury), fee, "treasury gets the at-close fee (raised only)");
    assert_eq!(bal(&state, cc), net, "creator gets seed + raised − fee");
    assert_eq!(bal(&state, ct), unsold_tokens, "creator gets the unsold tokens");
    assert_eq!(bal(&state, i.collateral_vault), 0, "collateral vault drained");
    assert_eq!(bal(&state, i.token_vault), 0, "token vault drained");
    // Seed-exclusion: had the fee taxed the whole balance it would be larger.
    assert!(
        fee < close_fee(vault_collateral, p.fee_bps),
        "the creator's seed must be excluded from the fee base"
    );
}

/// SECURITY regression: the ungated `Buy`/`BuyAta` instructions must REJECT a
/// pool that has an allowlist, otherwise the gate is trivially bypassed by
/// choosing the ungated instruction.
#[test]
fn plain_buy_is_rejected_on_a_gated_pool() {
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = V03State::new_with_genesis_accounts(&[], vec![], 0);
    deploy(&mut state);
    let member = id_of(&PrivateKey::try_new([99; 32]).unwrap());
    seed_gated_pool(&mut state, &i, allowlist_leaf(&member)); // non-zero allowlist root

    let buyer_key = PrivateKey::try_new([60; 32]).unwrap();
    let buyer_coll = id_of(&buyer_key);
    let buyer_tok = AccountId::new([61; 32]);
    fund_buyer(&mut state, &i, buyer_coll, buyer_tok, 100_000);

    let msg = public_transaction::Message::try_new(
        lbp(),
        vec![i.pool, i.token_vault, i.collateral_vault, buyer_coll, buyer_tok, CLOCK_01],
        vec![Nonce(0)],
        Instruction::Buy { collateral_in: 5_000, min_tokens_out: 0, deadline: u64::MAX },
    )
    .unwrap();
    let witness = public_transaction::WitnessSet::for_message(&msg, &[&buyer_key]);
    let res = state.transition_from_public_transaction(&PublicTransaction::new(msg, witness), 0, 0);
    assert!(res.is_err(), "ungated Buy must revert on an allowlist-gated pool");
    assert_eq!(bal(&state, buyer_tok), 0, "no tokens delivered when the gate is enforced");
}

