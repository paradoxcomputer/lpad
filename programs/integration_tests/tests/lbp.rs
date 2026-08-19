//! End-to-end integration tests for the LBP program against an in-process LEZ
//! state machine, mirroring the bonding-curve E2E tests. Two buy paths, and
//! they are genuinely different kinds of transaction:
//!
//!   * the PUBLIC buys (`Buy`, `BuyGated`, `BuyAta`), priced at the on-chain
//!     clock and applied with `transition_from_public_transaction`;
//!   * the PRIVATE `BuyDisposable`, applied with
//!     `transition_from_privacy_preserving_transaction` - a real proof of the
//!     committed guest ELF inside the LEZ privacy circuit, in which the buyer's
//!     collateral holding and token holding are PRIVATE account slots.
//!
//! The LBP is the harder of the two launchpads to make private, because its
//! price depends on time and a privacy transaction cannot carry the clock. The
//! disposable tests therefore pin both halves of that: the price really is
//! taken at the caller-supplied `t_buy_ms` (and is therefore never better than
//! the price at admission), and the transaction's own validity window is what
//! binds that argument to real time.
//!
//! Run with `RISC0_DEV_MODE=1`; the disposable tests turn it on themselves, see
//! [`force_dev_mode`].

use std::collections::HashMap;

use ata_core::{compute_ata_seed, get_associated_token_account_id};
use lbp_core::{
    allowlist_leaf, buy_tokens_out, close_fee, compute_collateral_vault_pda,
    compute_creator_index_pda, compute_pool_pda, compute_token_vault_pda, compute_weight_obs_pda,
    weight_token_q64, CreatorIndex, Instruction, PoolState, SaleStatus, WeightObs, CLOCK_01,
    MAX_PRIVATE_WINDOW_MS,
};
use lee::{
    error::LeeError,
    execute_and_prove,
    privacy_preserving_transaction::{
        circuit::ProgramWithDependencies, message::Message as PrivacyMessage,
        witness_set::WitnessSet as PrivacyWitnessSet,
    },
    program::Program,
    program_deployment_transaction::{self, ProgramDeploymentTransaction},
    public_transaction, PrivacyPreservingTransaction, PrivateKey, PublicKey, PublicTransaction,
    V03State,
};
use lee_core::{
    account::{Account, AccountId, AccountWithMetadata, Data, Nonce},
    encryption::ViewingPublicKey,
    Commitment, EncryptedAccountData, InputAccountIdentity, Nullifier, NullifierPublicKey,
    NullifierSecretKey, ViewTag,
};
use token_core::{TokenDefinition, TokenHolding};

const RESERVE_TOKEN: u128 = 1_000_000;
const RESERVE_COLLATERAL: u128 = 50_000;
const T_START: u64 = 0;
const T_END: u64 = 1_000_000;
const FEE_BPS: u128 = 500; // 5% at close
const NONCE: u64 = 0;

fn lbp() -> lee_core::program::ProgramId {
    lpad_guests::lbp().id()
}
fn token() -> lee_core::program::ProgramId {
    programs::token().id()
}
fn ata_prog() -> lee_core::program::ProgramId {
    programs::ata().id()
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
/// A real Fungible token definition account. `CreateSale` now declares BOTH
/// definitions as accounts and asserts each parses as one, so a pool can no
/// longer be created against an id that names nothing - which is what an
/// `AccountId::new([7; 32])`/`([8; 32])` that was never inserted used to be.
/// Copied verbatim from the bonding-curve suite, which needs it for the same
/// reason.
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
/// A real NonFungible token definition account - an NFT collection.
///
/// F27: naming one of these as a pool's collateral used to be accepted, and
/// every balance this program reads goes through `read_fungible`, which panics
/// on any NFT holding. Copied from the bonding-curve suite, which needs it for
/// the same reason.
fn nft_definition(name: &str) -> Account {
    Account {
        program_owner: token(),
        balance: 0,
        data: Data::from(&TokenDefinition::NonFungible {
            name: name.to_owned(),
            printable_supply: 1_000,
            metadata_id: AccountId::new([13; 32]),
        }),
        nonce: Nonce(0),
    }
}

/// An NFT holding: an account that parses as a `TokenHolding` and still cannot
/// receive a divisible fee. The treasury shape that gets past a bare
/// "is it a token holding" check.
fn nft_holding(def: AccountId) -> Account {
    Account {
        program_owner: token(),
        balance: 0,
        data: Data::from(&TokenHolding::NftPrintedCopy { definition_id: def, owned: true }),
        nonce: Nonce(0),
    }
}

/// A collateral holding whose DATA is impeccable - the pool's own collateral
/// definition, a balance that covers the buy - but which is owned by
/// `program_owner` rather than by the token program.
///
/// This is the attacker's account, and it is cheap to make: program deployment
/// on LEZ is permissionless, so anyone can deploy a program, have it claim an
/// account and write whatever `TokenHolding` bytes they like into it. A check
/// that only reads those bytes is therefore not a check on anything the attacker
/// does not control.
fn substituted_holding(
    def: AccountId,
    balance: u128,
    program_owner: lee_core::program::ProgramId,
) -> Account {
    Account { program_owner, ..fungible(def, balance) }
}

/// The program the substituted holding is owned by: a real, deployed program
/// that is not the token program.
///
/// The full attack owns that holding with a freshly deployed no-op whose
/// `Transfer` handler echoes its pre-states, so both collateral legs move
/// nothing while the pool's post state commits anyway. No such guest can be
/// built from here - this suite runs against committed ELFs and builds none -
/// and none is needed: dispatch is taken from the vault now, and the payer must
/// match it, so what the no-op *would* have done is never reached. What has to
/// be shown is that a holding owned by anything other than the vault's program
/// is refused, and the built-in faucet is a truthful stand-in for "anything
/// other": it is genuinely deployed by `initial_state()`, so a rejection cannot
/// be blamed on a program id that names nothing on chain.
fn substituted_program() -> lee_core::program::ProgramId {
    programs::faucet().id()
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
        let elf = lpad_guests::lbp().elf().to_vec();
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
    /// The creator's `CreatorIndex` PDA. Derived from the creator and NOTHING
    /// else - not the nonce, not the definitions, not the pool - so every pool
    /// this creator ever makes appends to this one account. That is what turns
    /// listing a wallet's pools into a single read.
    creator_index: AccountId,
}
fn ids(creator: AccountId) -> Ids {
    ids_at_nonce(creator, NONCE)
}
/// The same fixture at a chosen pool nonce, for the tests that need a creator's
/// SECOND pool. Every PDA here moves with the nonce except `creator_index`,
/// which is exactly the asymmetry the append test is about.
fn ids_at_nonce(creator: AccountId, nonce: u64) -> Ids {
    let token_def = AccountId::new([7; 32]);
    let collateral_def = AccountId::new([8; 32]);
    let pool = compute_pool_pda(lbp(), token_def, collateral_def, creator, nonce);
    Ids {
        token_def,
        collateral_def,
        treasury: AccountId::new([9; 32]),
        creator,
        pool,
        token_vault: compute_token_vault_pda(lbp(), pool),
        collateral_vault: compute_collateral_vault_pda(lbp(), pool),
        creator_index: compute_creator_index_pda(lbp(), creator),
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
        // Nothing is owed until `Withdraw` escrows the at-close fee. The LBP
        // takes no per-swap fee at all, so an open pool holds this at 0 for its
        // whole life and `collateral_vault_balance == reserve_collateral`.
        treasury_owed: 0,
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

/// Seed an open pool: pool state (owned by LBP) and both vaults, the collateral
/// one holding the creator's seed exactly as `CreateSale` leaves it.
///
/// The collateral vault is part of the fixture rather than something the first
/// buy claims on the way past, because the buy paths take their chained-call
/// program FROM it: dispatching on the submitter-supplied payer instead is the
/// no-op-program hole that
/// `buy_rejects_a_collateral_holding_owned_by_another_program` covers, and an
/// unclaimed vault has no owner to anchor to.
fn seed_open_pool(state: &mut V03State, i: &Ids) {
    let pool_acc = Account {
        program_owner: lbp(),
        data: Data::from(&open_pool(i)),
        ..Default::default()
    };
    state.force_insert_account(i.pool, pool_acc);
    state.force_insert_account(i.token_vault, fungible(i.token_def, RESERVE_TOKEN));
    state.force_insert_account(i.collateral_vault, fungible(i.collateral_def, RESERVE_COLLATERAL));
}

/// Seed an open pool gated by `allowlist_root`.
fn seed_gated_pool(state: &mut V03State, i: &Ids, allowlist_root: [u8; 32]) {
    let mut p = open_pool(i);
    p.allowlist_root = allowlist_root;
    let pool_acc = Account {
        program_owner: lbp(),
        data: Data::from(&p),
        ..Default::default()
    };
    state.force_insert_account(i.pool, pool_acc);
    state.force_insert_account(i.token_vault, fungible(i.token_def, RESERVE_TOKEN));
    state.force_insert_account(i.collateral_vault, fungible(i.collateral_def, RESERVE_COLLATERAL));
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

/// The pool fixture's token weight at `t_ms`, straight off the declining schedule.
fn weight_at(t_ms: u64) -> u128 {
    weight_token_q64(w(99, 100), w(1, 100), T_START, T_END, t_ms)
}
fn expected_tokens_out_at(collateral_in: u128, t_ms: u64) -> u128 {
    buy_tokens_out(RESERVE_TOKEN, RESERVE_COLLATERAL, weight_at(t_ms), collateral_in)
}
fn expected_tokens_out(collateral_in: u128) -> u128 {
    expected_tokens_out_at(collateral_in, 0)
}

/// Everything a `CreateSale` needs on chain before it can succeed: the creator's
/// two holdings, their identity account, both definition ACCOUNTS (each deposit
/// leg is dispatched to the program that owns its own definition, so an id
/// naming nothing on chain is not enough) and - new in this release - the
/// TREASURY, which must already be an initialised Fungible holding of the
/// collateral definition.
///
/// The treasury is seeded here rather than per-test on purpose: an honest create
/// now has one, so a test that gets a rejection is rejecting a shape it
/// deliberately broke, not an omission it forgot to fix.
fn seed_create_fixture(
    state: &mut V03State,
    i: &Ids,
    creator_token_id: AccountId,
    creator_coll_id: AccountId,
) {
    state.force_insert_account(creator_token_id, fungible(i.token_def, RESERVE_TOKEN));
    state.force_insert_account(creator_coll_id, fungible(i.collateral_def, RESERVE_COLLATERAL));
    state.force_insert_account(i.creator, fungible(i.collateral_def, 0));
    state.force_insert_account(i.token_def, token_def_account("Project"));
    state.force_insert_account(i.collateral_def, token_def_account("Collateral"));
    state.force_insert_account(i.treasury, fungible(i.collateral_def, 0));
}

/// The parts of a `CreateSale` any test here varies, and nothing else.
///
/// The instruction carries eighteen fields over ten accounts, and spelling all
/// of that out at every call site is how the treasury came to sit in `PoolState`
/// for a whole release without a single test asking what account its id named.
/// [`CreateArgs::standard`] is the honest create; each test mutates exactly the
/// field it is about, so a rejection can only be blamed on that field.
struct CreateArgs {
    /// Slot 5 - the creator's project-token holding (a signer).
    creator_token_holding: AccountId,
    /// Slot 6 - the creator's collateral holding, which seeds the pool (a signer).
    creator_collateral_holding: AccountId,
    /// Slot 3 - the project token's definition account.
    token_definition: AccountId,
    /// Slot 8 - the ACCOUNT handed over as the treasury.
    treasury: AccountId,
    /// The `treasury_id` instruction ARGUMENT. Deliberately kept apart from the
    /// slot above: the id is what lands in `PoolState` and what `SweepTreasury`
    /// binds to, while the slot is the only thing `create_sale` can type-check.
    /// The assert binding the two is only testable if a test can make them
    /// disagree.
    treasury_id: AccountId,
    /// Slot 9 - the account handed over as the creator's pool index. Varied only
    /// by the test that checks the program pins this slot to the PDA of the
    /// creator who SIGNED: the submitter picks every non-signer id, so without
    /// that pin a create could append its pool into a stranger's listing.
    creator_index: AccountId,
    /// The pool `nonce` instruction argument. It must agree with the
    /// [`ids_at_nonce`] the message's PDAs came from, or `create_sale` rejects
    /// the pool slot before it reaches anything this suite is testing.
    nonce: u64,
    /// The nonce ALL THREE signers sign at. A rejected transaction advances no
    /// nonce, so the failure tests all leave this at 0; only a test that does two
    /// successful creates in a row has to raise it.
    signer_nonce: u128,
}

impl CreateArgs {
    /// Every slot the account it should be: the create [`seed_create_fixture`]
    /// is expected to accept.
    fn standard(i: &Ids, creator_token_holding: AccountId, creator_collateral_holding: AccountId) -> Self {
        Self {
            creator_token_holding,
            creator_collateral_holding,
            token_definition: i.token_def,
            treasury: i.treasury,
            treasury_id: i.treasury,
            creator_index: i.creator_index,
            nonce: NONCE,
            signer_nonce: 0,
        }
    }
}

/// A `CreateSale` message over this suite's standard pool parameters.
///
/// The account order is the program's. The treasury is at slot 8 - after the
/// creator, before the clock, which `echo_clock` keeps last - and is read-only
/// here, echoed back unchanged. It is in the list at all so `create_sale` can
/// type-check it, which is what makes the DEFAULT-owned branch of
/// `sweep_treasury` unreachable instead of merely unlikely.
///
/// The creator's pool index is at slot 9, between the treasury and the clock,
/// which `echo_clock` requires to stay last. It is the only account in this list
/// the program CLAIMS on a first pool and merely rewrites afterwards.
///
/// Three signers: both creator holdings (the two deposit legs' senders) and the
/// creator's identity. A rejected transaction advances no nonce, so every test
/// here signs at `CreateArgs::signer_nonce` == 0 no matter how many creates it
/// has already had refused.
fn create_msg(i: &Ids, a: &CreateArgs) -> public_transaction::Message {
    public_transaction::Message::try_new(
        lbp(),
        vec![
            i.pool,
            i.token_vault,
            i.collateral_vault,
            a.token_definition,
            i.collateral_def,
            a.creator_token_holding,
            a.creator_collateral_holding,
            i.creator,
            a.treasury,
            a.creator_index,
            CLOCK_01,
        ],
        vec![Nonce(a.signer_nonce), Nonce(a.signer_nonce), Nonce(a.signer_nonce)],
        Instruction::CreateSale {
            collateral_definition_id: i.collateral_def,
            treasury_id: a.treasury_id,
            token_name: String::new(),
            token_symbol: String::new(),
            token_deposit: RESERVE_TOKEN,
            collateral_seed: RESERVE_COLLATERAL,
            w_start_q64: w(99, 100),
            w_end_q64: w(1, 100),
            t_start_ms: T_START,
            t_end_ms: T_END,
            fee_bps: FEE_BPS,
            block_token_ceiling: 0,
            allowlist_root: [0; 32],
            fixed_price: false,
            min_duration_ms: 0,
            nonce: a.nonce,
            ata_program_id: ata_prog(),
            deadline: u64::MAX,
        },
    )
    .expect("message")
}

/// `CreateSale` end-to-end: claims the pool PDA, deposits the project tokens
/// into the token vault, and seeds the collateral vault.
///
/// Every other test in this file starts from `seed_open_pool`, which injects a
/// ready-made [`PoolState`] with `force_insert_account`. That skipped the create
/// path entirely - its two chained `token::Transfer` legs and its three
/// signatures (project holding, collateral holding, creator identity) were never
/// exercised, which is why a create that the unit tests accept could still be
/// rejected on chain. Mirrors `bonding_curve::create_sale_deposits_and_initializes_state`.
#[test]
fn create_sale_deposits_and_seeds_both_vaults() {
    let creator_token_key = PrivateKey::try_new([41; 32]).expect("key");
    let creator_coll_key = PrivateKey::try_new([43; 32]).expect("key");
    let creator_key = PrivateKey::try_new([42; 32]).expect("key");
    let creator_token_id = id_of(&creator_token_key);
    let creator_coll_id = id_of(&creator_coll_key);
    let i = ids(id_of(&creator_key));

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_create_fixture(&mut state, &i, creator_token_id, creator_coll_id);

    let message = create_msg(&i, &CreateArgs::standard(&i, creator_token_id, creator_coll_id));
    let witness = public_transaction::WitnessSet::for_message(
        &message,
        &[&creator_token_key, &creator_coll_key, &creator_key],
    );
    state
        .transition_from_public_transaction(&PublicTransaction::new(message, witness), 0, 0)
        .expect("lbp create_sale must succeed");

    let pool = PoolState::try_from(&state.get_account_by_id(i.pool).data).expect("pool");
    assert_eq!(state.get_account_by_id(i.pool).program_owner, lbp());
    assert_eq!(pool.reserve_token, RESERVE_TOKEN);
    assert_eq!(pool.reserve_collateral, RESERVE_COLLATERAL);
    assert_eq!(pool.ata_program_id, ata_prog(), "ATA program pinned at creation");
    assert!(matches!(pool.status, SaleStatus::Open));
    // Both deposit legs must have moved, and the creator's holdings drained.
    assert_eq!(bal(&state, i.token_vault), RESERVE_TOKEN, "token deposit landed");
    assert_eq!(bal(&state, i.collateral_vault), RESERVE_COLLATERAL, "collateral seed landed");
    assert_eq!(bal(&state, creator_token_id), 0);
    assert_eq!(bal(&state, creator_coll_id), 0);
    // The treasury is READ-ONLY here: declared so the program can type-check it,
    // echoed back byte for byte. Creation must not move a token into it, and
    // must not claim it - a claim is what the old design needed and what made
    // the whole thing griefable.
    assert_eq!(
        state.get_account_by_id(i.treasury),
        fungible(i.collateral_def, 0),
        "creation must leave the treasury exactly as it found it"
    );
    // Both vaults must come out owned by the real token program. A balance says
    // the deposit leg ran; only the owner says it ran under the program the buy
    // paths will later dispatch against - `finalize_buy` binds every buyer's
    // holding to `collateral_vault.program_owner`, so a vault claimed by anything
    // else makes the pool unbuyable by every honest holder of the real token.
    // Which program claims the collateral vault is exactly what the definition
    // account above now determines.
    // `first_buy_after_a_real_create_sale_succeeds` is the end-to-end form of
    // this; these two lines are the cheap version that runs on every create test.
    assert_eq!(
        state.get_account_by_id(i.token_vault).program_owner,
        token(),
        "the token vault must be owned by the token program after creation"
    );
    assert_eq!(
        state.get_account_by_id(i.collateral_vault).program_owner,
        token(),
        "the collateral vault must be owned by the token program after creation"
    );
}

/// `CreateSale` really produces vaults the buy guard will accept, proved by
/// doing a create and then the pool's FIRST buy in one state with nothing
/// hand-seeded. The bonding curve's
/// `first_buy_after_a_real_create_sale_succeeds` is the same test on the other
/// launchpad; both programs have the same shape of gap.
///
/// Every other buy test starts from [`seed_open_pool`], which
/// `force_insert_account`s both vaults - so those tests CHOOSE the
/// `program_owner` that `finalize_buy` compares the buyer's holding against, and
/// they agree with themselves by construction. The create test, in turn,
/// asserted balances but never ownership. Between them, a change to the
/// collateral seed leg - which program it is dispatched to, whether it still
/// claims the PDA - would break every real first buy on chain while the whole
/// suite stayed green.
///
/// That leg is exactly what the R1 hardening changed: it is now dispatched to
/// the pinned collateral DEFINITION's owner rather than to the creator's
/// holding, and `token::transfer`'s `new_claimed_if_default` makes whichever
/// program executes it the vault's owner for the pool's whole life. This test is
/// what says the honest case still works after that change.
#[test]
fn first_buy_after_a_real_create_sale_succeeds() {
    let creator_token_key = PrivateKey::try_new([41; 32]).expect("key");
    let creator_coll_key = PrivateKey::try_new([43; 32]).expect("key");
    let creator_key = PrivateKey::try_new([42; 32]).expect("key");
    let creator_token_id = id_of(&creator_token_key);
    let creator_coll_id = id_of(&creator_coll_key);
    let i = ids(id_of(&creator_key));

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_create_fixture(&mut state, &i, creator_token_id, creator_coll_id);

    // Mechanical proof that the fixture really left both vaults to the program:
    // an assertion, not a comment, so a later `force_insert_account` creeping in
    // here fails the test instead of silently hollowing it out.
    assert_eq!(
        state.get_account_by_id(i.token_vault),
        Account::default(),
        "the token vault must not be pre-seeded: CreateSale is what has to create it"
    );
    assert_eq!(
        state.get_account_by_id(i.collateral_vault),
        Account::default(),
        "the collateral vault must not be pre-seeded: CreateSale is what has to create it"
    );

    let create = create_msg(&i, &CreateArgs::standard(&i, creator_token_id, creator_coll_id));
    let w_set = public_transaction::WitnessSet::for_message(
        &create,
        &[&creator_token_key, &creator_coll_key, &creator_key],
    );
    state
        .transition_from_public_transaction(&PublicTransaction::new(create, w_set), 0, 0)
        .expect("lbp create_sale must succeed");

    // The vaults the program just made, and their owners - the field the first
    // buy is about to be dispatched to.
    assert_eq!(
        state.get_account_by_id(i.token_vault).program_owner,
        token(),
        "the token vault must come out owned by the real token program, not by whatever \
         program happened to claim it: the buy paths take their payout dispatch from here"
    );
    assert_eq!(
        state.get_account_by_id(i.collateral_vault).program_owner,
        token(),
        "the collateral vault must come out owned by the real token program: this is the id \
         finalize_buy makes every buyer's holding match"
    );
    assert_eq!(bal(&state, i.token_vault), RESERVE_TOKEN, "the token deposit landed");
    assert_eq!(bal(&state, i.collateral_vault), RESERVE_COLLATERAL, "the collateral seed landed");

    // ...and now the first buy against exactly that state. A fresh buyer, so its
    // nonce is 0 even though the creator's accounts have moved on.
    let buyer_key = PrivateKey::try_new([60; 32]).unwrap();
    let buyer_coll = id_of(&buyer_key);
    let buyer_tok = AccountId::new([61; 32]);
    let buyer_start: u128 = 100_000;
    let collateral_in: u128 = 5_000;
    state.force_insert_account(buyer_coll, fungible(i.collateral_def, buyer_start));
    state.force_insert_account(buyer_tok, fungible(i.token_def, 0));

    let tokens_out = expected_tokens_out(collateral_in);
    assert!(tokens_out > 0, "the buy must be one that really moves the pool");

    let buy = public_transaction::Message::try_new(
        lbp(),
        vec![i.pool, i.token_vault, i.collateral_vault, buyer_coll, buyer_tok, CLOCK_01],
        vec![Nonce(0)],
        Instruction::Buy { collateral_in, min_tokens_out: 0, deadline: u64::MAX },
    )
    .unwrap();
    let w_set = public_transaction::WitnessSet::for_message(&buy, &[&buyer_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(buy, w_set), 1, 0)
        .expect(
            "the first buy against a pool this program just created must succeed - if it does \
             not, create_sale is producing a vault the buy guard rejects, which is a chain \
             break no fixture-seeded test can see",
        );

    assert_eq!(bal(&state, buyer_tok), tokens_out, "the buyer really received tokens");
    assert_eq!(bal(&state, buyer_coll), buyer_start - collateral_in, "the buyer really paid");
    assert_eq!(
        bal(&state, i.collateral_vault),
        RESERVE_COLLATERAL + collateral_in,
        "the whole C_in landed in the created vault - the LBP takes no per-swap fee"
    );
    assert_eq!(bal(&state, i.token_vault), RESERVE_TOKEN - tokens_out);
    let pool = PoolState::try_from(&state.get_account_by_id(i.pool).data).unwrap();
    assert_eq!(pool.reserve_token, RESERVE_TOKEN - tokens_out);
    assert_eq!(pool.reserve_collateral, RESERVE_COLLATERAL + collateral_in);
    assert_eq!(pool.buy_count, 1);
}

// --- the per-creator pool index ---------------------------------------------
//
// WHAT THIS IS FOR. Nothing on chain used to record what a wallet had created,
// so `lpad my-pools` re-derived every PDA the wallet could conceivably have made
// - a product over (public account x token definition x collateral definition x
// nonce), thousands of sequential reads, measured at tens of minutes on a live
// wallet with history. `CreateSale` now writes one account per creator holding
// the ids it has made, and discovery is one read.
//
// The guest's own tests (`lbp_program`) pin its branches. These pin the parts
// only a real state machine can show: that the account lands in LEZ state at the
// id the SDK derives, carrying bytes the SDK decodes, and that it keeps
// accumulating across a creator's whole history.

/// Read a creator's index the way the SDK will - derive, read, decode - and say
/// which of the three steps failed.
///
/// Split out because "there is no index account", "it is owned by somebody else"
/// and "it does not decode" are three different bugs, and a helper returning
/// `Option` would let a test that meant to prove discovery works pass while
/// discovery returned nothing.
fn creator_index(state: &V03State, creator: AccountId) -> CreatorIndex {
    let account = state.get_account_by_id(compute_creator_index_pda(lbp(), creator));
    assert_ne!(
        account,
        Account::default(),
        "no index account exists at the creator's index PDA - discovery would find nothing"
    );
    assert_eq!(
        account.program_owner,
        lbp(),
        "the index account must be owned by the LBP program: it holds this program's own \
         data, and only this program may append to it"
    );
    CreatorIndex::try_from(&account.data)
        .expect("the index account does not decode as a CreatorIndex")
}

/// A create writes the creator's index, and the index names the pool.
///
/// The id is recomputed here from the program id and the creator alone -
/// `compute_creator_index_pda(lbp(), creator)`, the exact call the SDK makes -
/// rather than read back out of the transaction, so the test fails if the
/// program stores the index anywhere the SDK would not think to look.
#[test]
fn create_sale_records_the_new_pool_in_the_creators_index() {
    let creator_token_key = PrivateKey::try_new([41; 32]).expect("key");
    let creator_coll_key = PrivateKey::try_new([43; 32]).expect("key");
    let creator_key = PrivateKey::try_new([42; 32]).expect("key");
    let creator_token_id = id_of(&creator_token_key);
    let creator_coll_id = id_of(&creator_coll_key);
    let i = ids(id_of(&creator_key));

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_create_fixture(&mut state, &i, creator_token_id, creator_coll_id);
    // Mechanical proof the fixture does not pre-seed the thing under test: the
    // index has to be CLAIMED by the create, not found lying around.
    assert_eq!(
        state.get_account_by_id(i.creator_index),
        Account::default(),
        "the creator index must not be pre-seeded: the first CreateSale is what claims it"
    );

    let m = create_msg(&i, &CreateArgs::standard(&i, creator_token_id, creator_coll_id));
    let w = public_transaction::WitnessSet::for_message(
        &m,
        &[&creator_token_key, &creator_coll_key, &creator_key],
    );
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, w), 0, 0)
        .expect("create_sale must succeed");

    let index = creator_index(&state, i.creator);
    assert_eq!(
        index.creator, i.creator,
        "the index must record the creator it belongs to: the SDK reads this speculatively \
         for any wallet, and this field is what tells it the account is really theirs"
    );
    assert_eq!(
        index.pool_ids,
        vec![i.pool],
        "the index must name the pool that was just created, and nothing else"
    );
    // IDS ONLY, never state. Pool state (reserves, weights, status) changes on
    // every buy and with the clock; an index carrying any of it would be stale
    // the moment it was written. 78 bytes is exactly magic(8) + version(2) +
    // creator(32) + vec len(4) + one id(32).
    assert_eq!(
        state.get_account_by_id(i.creator_index).data.as_ref().len(),
        8 + 2 + 32 + 4 + 32,
        "the index must hold ids and nothing else - no reserve, no weight, no status"
    );
}

/// A creator's SECOND pool APPENDS. Two creates, two pools, one index holding
/// both ids oldest-first.
///
/// This is the test that fails if `create_sale` ever writes the index instead of
/// extending it - which is the natural shape of the bug, since the first-pool
/// branch does exactly that. An overwrite would leave a wallet's listing showing
/// only its most recent pool while every earlier one silently vanished, and the
/// single-create test above would stay green.
///
/// The two pools differ only in `nonce`: the pool PDA and both vault PDAs move
/// with it, and the index PDA - keyed on the creator alone - does not.
#[test]
fn a_second_pool_appends_to_the_creators_index_rather_than_replacing_it() {
    let creator_token_key = PrivateKey::try_new([41; 32]).expect("key");
    let creator_coll_key = PrivateKey::try_new([43; 32]).expect("key");
    let creator_key = PrivateKey::try_new([42; 32]).expect("key");
    let creator_token_id = id_of(&creator_token_key);
    let creator_coll_id = id_of(&creator_coll_key);
    let first = ids_at_nonce(id_of(&creator_key), NONCE);
    let second = ids_at_nonce(id_of(&creator_key), NONCE + 1);
    assert_ne!(first.pool, second.pool, "two nonces, two pool PDAs");
    assert_eq!(
        first.creator_index, second.creator_index,
        "...and ONE index PDA: it is derived from the creator, not from the pool"
    );

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_create_fixture(&mut state, &first, creator_token_id, creator_coll_id);
    // Enough of both tokens to fund two full deposits out of one pair of holdings.
    state.force_insert_account(creator_token_id, fungible(first.token_def, 2 * RESERVE_TOKEN));
    state.force_insert_account(
        creator_coll_id,
        fungible(first.collateral_def, 2 * RESERVE_COLLATERAL),
    );

    let m = create_msg(&first, &CreateArgs::standard(&first, creator_token_id, creator_coll_id));
    let w = public_transaction::WitnessSet::for_message(
        &m,
        &[&creator_token_key, &creator_coll_key, &creator_key],
    );
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, w), 0, 0)
        .expect("the first create must succeed");

    // All three signers advanced to 1 on the create above; a second create at 0
    // would be rejected for the nonce and prove nothing about the index.
    let m = create_msg(
        &second,
        &CreateArgs {
            nonce: NONCE + 1,
            signer_nonce: 1,
            ..CreateArgs::standard(&second, creator_token_id, creator_coll_id)
        },
    );
    let w = public_transaction::WitnessSet::for_message(
        &m,
        &[&creator_token_key, &creator_coll_key, &creator_key],
    );
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, w), 1, 0)
        .expect("the creator's second create must succeed");

    let index = creator_index(&state, first.creator);
    assert_eq!(
        index.pool_ids,
        vec![first.pool, second.pool],
        "both pools must be listed, oldest first - an append, not an overwrite"
    );
    // The append must not have disturbed the earlier pool itself. It is a
    // separate account, but the index write is the only part of a second create
    // that touches anything the first one produced.
    assert_eq!(
        state.get_account_by_id(first.pool).program_owner,
        lbp(),
        "the first pool account must survive its creator's second create"
    );
    assert_eq!(
        PoolState::try_from(&state.get_account_by_id(first.pool).data)
            .expect("the first pool must still decode")
            .nonce,
        NONCE,
    );
    assert_eq!(
        state.get_account_by_id(first.creator_index).data.as_ref().len(),
        8 + 2 + 32 + 4 + 64,
        "two ids and still nothing else"
    );
}

/// SECURITY: the index slot is pinned to the SIGNING creator's PDA.
///
/// Every non-signer account id in a message is chosen by whoever submits it. The
/// creator index is not signed by anybody - it is a keyless PDA - so without this
/// pin a stranger could point slot 9 at somebody else's index and have their own
/// pool appended to that wallet's listing. `my-pools` would then show a creator
/// pools they never made, at ids they do not control, which is a lie the CLI has
/// no way to detect: the pool accounts named are perfectly real.
///
/// Both shapes get the same named rejection, and - the part that matters - the
/// victim's index is still not there afterwards.
#[test]
fn create_sale_rejects_a_creator_index_that_is_not_the_signing_creators() {
    let creator_token_key = PrivateKey::try_new([41; 32]).expect("key");
    let creator_coll_key = PrivateKey::try_new([43; 32]).expect("key");
    let creator_key = PrivateKey::try_new([42; 32]).expect("key");
    let creator_token_id = id_of(&creator_token_key);
    let creator_coll_id = id_of(&creator_coll_key);
    let i = ids(id_of(&creator_key));
    let victim = id_of(&PrivateKey::try_new([77; 32]).expect("key"));
    let victim_index = compute_creator_index_pda(lbp(), victim);
    assert_ne!(victim_index, i.creator_index, "the victim is a different creator");

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_create_fixture(&mut state, &i, creator_token_id, creator_coll_id);

    let cases: [(&str, AccountId); 2] = [
        ("another wallet's index PDA - the listing-pollution attack", victim_index),
        ("an id that is no index PDA at all", AccountId::new([123; 32])),
    ];
    for (case, creator_index_slot) in cases {
        let m = create_msg(
            &i,
            &CreateArgs {
                creator_index: creator_index_slot,
                ..CreateArgs::standard(&i, creator_token_id, creator_coll_id)
            },
        );
        let w = public_transaction::WitnessSet::for_message(
            &m,
            &[&creator_token_key, &creator_coll_key, &creator_key],
        );
        let result = state.transition_from_public_transaction(&PublicTransaction::new(m, w), 0, 0);
        let Err(LeeError::ProgramExecutionFailed(err)) = result else {
            panic!("a create whose index slot is {case} must revert, got {result:?}");
        };
        assert!(
            err.contains("not the index PDA of the signing creator"),
            "the rejection must name the pin that caught it, got: {err}"
        );
    }
    assert_eq!(
        state.get_account_by_id(victim_index),
        Account::default(),
        "the victim's index must not exist: nothing may write to a wallet's listing but that \
         wallet's own creates"
    );

    // Control: the same fixture, the honest index slot, accepted. Without this
    // the test above would pass just as happily if creates had stopped working.
    let m = create_msg(&i, &CreateArgs::standard(&i, creator_token_id, creator_coll_id));
    let w = public_transaction::WitnessSet::for_message(
        &m,
        &[&creator_token_key, &creator_coll_key, &creator_key],
    );
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, w), 0, 0)
        .expect("the same create with the creator's own index PDA must succeed");
    assert_eq!(creator_index(&state, i.creator).pool_ids, vec![i.pool]);
}

/// SECURITY: an index account that exists but belongs to another program is
/// refused by name, rather than being parsed as if it were ours.
///
/// The state is reachable only through `force_insert_account`: on a live chain
/// nothing but this program can put an account at this PDA, so the guard is
/// defence in depth. The message says so, rather than offering "use a different
/// creator account" as a remedy - this seed carries no nonce, so that advice
/// would cost the operator their identity and fix nothing.
#[test]
fn create_sale_rejects_a_creator_index_owned_by_another_program() {
    let creator_token_key = PrivateKey::try_new([41; 32]).expect("key");
    let creator_coll_key = PrivateKey::try_new([43; 32]).expect("key");
    let creator_key = PrivateKey::try_new([42; 32]).expect("key");
    let creator_token_id = id_of(&creator_token_key);
    let creator_coll_id = id_of(&creator_coll_key);
    let i = ids(id_of(&creator_key));

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_create_fixture(&mut state, &i, creator_token_id, creator_coll_id);
    // The right id, the wrong owner: a token holding sitting where the index goes.
    state.force_insert_account(i.creator_index, fungible(i.collateral_def, 0));

    let m = create_msg(&i, &CreateArgs::standard(&i, creator_token_id, creator_coll_id));
    let w = public_transaction::WitnessSet::for_message(
        &m,
        &[&creator_token_key, &creator_coll_key, &creator_key],
    );
    let result = state.transition_from_public_transaction(&PublicTransaction::new(m, w), 0, 0);
    let Err(LeeError::ProgramExecutionFailed(err)) = result else {
        panic!("a create over a foreign-owned index account must revert, got {result:?}");
    };
    assert!(
        err.contains("exists but is not owned by this program"),
        "the rejection must name the ownership guard, got: {err}"
    );
}

/// Closing a pool leaves the creator's index alone, entry included.
///
/// `CloseSale` is `[pool, creator, clock]` - the index is not in its account list
/// at all, which is the whole reason this holds. The test is here because the
/// tempting "tidy up" change is to drop closed pools from the listing, and that
/// would be wrong twice over: a creator's history is what `my-pools` is for, and
/// removals would make the index's positions unstable for anything that cached
/// them. Status is resolved from the pool account, never from the index.
/// `Pause` really pauses and `Resume` really resumes, proved through the guest.
///
/// This is the one hazard `lbp_core`'s instruction-set comment calls out by name:
/// the wire discriminant is the declaration index, and `Pause`/`Resume` are
/// "byte-identical on the wire apart from that index and take identical account
/// lists, so swapping them would resume a paused sale (or pause a live one) with
/// no error anywhere". Nothing asserted it. Host-side tests cannot: they call
/// `set_paused(pool, creator, true/false, ..)` directly and so encode the mapping
/// they are supposed to be checking. Only a real instruction, decoded by the real
/// guest, pins `Pause -> paused = true`.
///
/// It also covers RFP-016 Functionality #6, whose emergency stop had no test at
/// any layer before this.
#[test]
fn pause_pauses_and_resume_resumes_through_the_guest() {
    let creator_token_key = PrivateKey::try_new([41; 32]).expect("key");
    let creator_coll_key = PrivateKey::try_new([43; 32]).expect("key");
    let creator_key = PrivateKey::try_new([42; 32]).expect("key");
    let creator_token_id = id_of(&creator_token_key);
    let creator_coll_id = id_of(&creator_coll_key);
    let i = ids(id_of(&creator_key));

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_create_fixture(&mut state, &i, creator_token_id, creator_coll_id);

    let m = create_msg(&i, &CreateArgs::standard(&i, creator_token_id, creator_coll_id));
    let w = public_transaction::WitnessSet::for_message(
        &m,
        &[&creator_token_key, &creator_coll_key, &creator_key],
    );
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, w), 0, 0)
        .expect("create_sale must succeed");

    macro_rules! paused_now {
        () => {
            PoolState::try_from(&state.get_account_by_id(i.pool).data).expect("pool").paused
        };
    }
    assert!(!paused_now!(), "a freshly created pool is live");

    // Pause. Account list is [pool, creator] - no clock, because weight
    // progression is lazy and continues regardless.
    let pause = public_transaction::Message::try_new(
        lbp(),
        vec![i.pool, i.creator],
        vec![Nonce(1)],
        Instruction::Pause { deadline: u64::MAX },
    )
    .expect("message");
    let w = public_transaction::WitnessSet::for_message(&pause, &[&creator_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(pause, w), 1, 1)
        .expect("the creator must be able to pause");
    assert!(paused_now!(), "Pause must set paused = true, not clear it");

    // Resume, and back to live. If the two arms were ever swapped, exactly one
    // of these two assertions fails - which is the point of asserting both.
    let resume = public_transaction::Message::try_new(
        lbp(),
        vec![i.pool, i.creator],
        vec![Nonce(2)],
        Instruction::Resume { deadline: u64::MAX },
    )
    .expect("message");
    let w = public_transaction::WitnessSet::for_message(&resume, &[&creator_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(resume, w), 2, 2)
        .expect("the creator must be able to resume");
    assert!(!paused_now!(), "Resume must clear paused, not set it");
}

#[test]
fn closing_a_pool_leaves_the_creators_index_intact() {
    let creator_token_key = PrivateKey::try_new([41; 32]).expect("key");
    let creator_coll_key = PrivateKey::try_new([43; 32]).expect("key");
    let creator_key = PrivateKey::try_new([42; 32]).expect("key");
    let creator_token_id = id_of(&creator_token_key);
    let creator_coll_id = id_of(&creator_coll_key);
    let i = ids(id_of(&creator_key));

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_create_fixture(&mut state, &i, creator_token_id, creator_coll_id);

    let m = create_msg(&i, &CreateArgs::standard(&i, creator_token_id, creator_coll_id));
    let w = public_transaction::WitnessSet::for_message(
        &m,
        &[&creator_token_key, &creator_coll_key, &creator_key],
    );
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, w), 0, 0)
        .expect("create_sale must succeed");
    let before = state.get_account_by_id(i.creator_index);

    // Close at the pool's own end timestamp. The creator signed the create, so
    // their identity account is at nonce 1.
    set_clock(&mut state, 1, i64::try_from(T_END).expect("T_END fits"));
    let close = public_transaction::Message::try_new(
        lbp(),
        vec![i.pool, i.creator, CLOCK_01],
        vec![Nonce(1)],
        Instruction::CloseSale { deadline: u64::MAX },
    )
    .expect("message");
    let w = public_transaction::WitnessSet::for_message(&close, &[&creator_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(close, w), 1, T_END)
        .expect("the creator must be able to close at the end timestamp");

    assert!(
        matches!(
            PoolState::try_from(&state.get_account_by_id(i.pool).data).expect("pool").status,
            SaleStatus::Closed
        ),
        "the pool really closed - otherwise this test proves nothing about closing"
    );
    assert_eq!(
        state.get_account_by_id(i.creator_index),
        before,
        "CloseSale must not touch the creator's index account in any way"
    );
    assert_eq!(
        creator_index(&state, i.creator).pool_ids,
        vec![i.pool],
        "a closed pool stays listed: the index is a creator's history, and status is read \
         from the pool account"
    );
}

/// `CreateSale` rejects a `treasury_id` that aliases the creator's identity
/// account - the fund-lock both launchpads shipped with, and the one a creator
/// is most likely to type by hand ("send the fees to me").
///
/// `Withdraw` no longer names a treasury, so an alias no longer costs the
/// creator their principal - but it still strands the escrow permanently.
/// `SweepTreasury` is `[pool, collateral_vault, treasury]` with both of the
/// first two ids pinned from the pool state, and LEZ rejects a message with a
/// repeated account id BEFORE the program runs, so a pool whose treasury
/// aliases one of them has no account list that can ever settle it. The creator
/// alias is worse still: an identity signer is not a Fungible holding, so the
/// fee leg would revert against it.
///
/// `CreateSale` takes the treasury as an ACCOUNT now, which is why the rejected
/// create below declares the CREATOR as its `treasury_id` while still passing a
/// perfectly good treasury holding at slot 8. That is the shape that reaches the
/// program at all: naming the creator in both places puts one id in the list
/// twice, and LEZ refuses that as `Duplicate account_ids` before this program
/// runs - a true answer that names neither slot. The alias rule deliberately
/// runs ahead of the type pin so what comes back names the id to change instead
/// of complaining about the account that is fine.
///
/// The control at the end is the point of the test as much as the rejection is:
/// the identical create, with only `treasury_id` repaired, must go through.
#[test]
fn create_sale_rejects_a_treasury_that_aliases_the_creator() {
    let creator_token_key = PrivateKey::try_new([41; 32]).expect("key");
    let creator_coll_key = PrivateKey::try_new([43; 32]).expect("key");
    let creator_key = PrivateKey::try_new([42; 32]).expect("key");
    let creator_token_id = id_of(&creator_token_key);
    let creator_coll_id = id_of(&creator_coll_key);
    let i = ids(id_of(&creator_key));

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_create_fixture(&mut state, &i, creator_token_id, creator_coll_id);

    // The same message twice, differing only in `treasury_id`. A rejected
    // transaction advances no nonce, so both sign at 0.
    let message = |treasury_id: AccountId| {
        create_msg(
            &i,
            &CreateArgs { treasury_id, ..CreateArgs::standard(&i, creator_token_id, creator_coll_id) },
        )
    };

    let m = message(i.creator);
    let w_set = public_transaction::WitnessSet::for_message(
        &m,
        &[&creator_token_key, &creator_coll_key, &creator_key],
    );
    let result = state.transition_from_public_transaction(&PublicTransaction::new(m, w_set), 0, 0);
    let Err(LeeError::ProgramExecutionFailed(err)) = result else {
        panic!("a create naming the creator as its treasury must revert, got {result:?}");
    };
    assert!(
        err.contains("treasury must not alias the pool account"),
        "it must revert on the treasury-alias guard and not for some unrelated reason - a test \
         that passes on any error would pass on the fund-locking build too: {err}"
    );

    // Nothing was created, so there is no half-built pool to clean up.
    assert_eq!(
        state.get_account_by_id(i.pool),
        Account::default(),
        "the pool PDA must not exist: a rejected create is not a create"
    );
    assert_eq!(bal(&state, creator_token_id), RESERVE_TOKEN, "the deposit was not taken");
    assert_eq!(bal(&state, creator_coll_id), RESERVE_COLLATERAL, "nor the collateral seed");

    // Control: repair only the treasury and the identical create succeeds.
    let m = message(i.treasury);
    let w_set = public_transaction::WitnessSet::for_message(
        &m,
        &[&creator_token_key, &creator_coll_key, &creator_key],
    );
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, w_set), 0, 0)
        .expect("the identical create must succeed once the treasury is a separate account");
    let pool = PoolState::try_from(&state.get_account_by_id(i.pool).data).expect("pool");
    assert_eq!(pool.treasury_id, i.treasury, "control: the pool really was created");
}

/// SECURITY: `CreateSale` pins the project token by ACCOUNT, and must refuse a
/// `token_definition` slot that is not the definition the creator's own holding
/// names.
///
/// `token_definition_id` is read out of `creator_token_holding`'s DATA, and LEZ
/// deployment is permissionless, so a creator can advertise a real, valuable
/// definition id while handing over a holding owned by a token program they
/// deployed themselves. The deposit leg is what CLAIMS the token-vault PDA
/// (`token::Transfer`'s `new_claimed_if_default`), so dispatching it on the
/// holding's owner made the vault come out owned by the impostor - and every
/// later buy dispatches its token leg on `token_vault.program_owner`, i.e.
/// straight back into it. Buyers pay real collateral and receive a holding that
/// merely READS as the valuable token.
///
/// The guard is a PAIR of asserts, and neither half is observable while the
/// other holds, so each gets its own test: this one hands over a decoy
/// definition account, and
/// `create_sale_rejects_a_creator_holding_under_a_foreign_token_program` hands
/// over a holding owned by a foreign program. Mirrors the bonding curve's pair
/// of the same name.
#[test]
fn create_sale_rejects_a_token_definition_account_it_did_not_declare() {
    let creator_token_key = PrivateKey::try_new([41; 32]).expect("key");
    let creator_coll_key = PrivateKey::try_new([43; 32]).expect("key");
    let creator_key = PrivateKey::try_new([42; 32]).expect("key");
    let creator_token_id = id_of(&creator_token_key);
    let creator_coll_id = id_of(&creator_coll_key);
    let i = ids(id_of(&creator_key));

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_create_fixture(&mut state, &i, creator_token_id, creator_coll_id);
    // A perfectly real definition account that simply is not this pool's. The
    // attack does not need it to be malformed - it needs the program to take its
    // `program_owner` as the deposit's dispatch target.
    let decoy_def = AccountId::new([12; 32]);
    state.force_insert_account(decoy_def, token_def_account("Decoy"));

    // The same message twice, differing only in the declared definition account.
    // A rejected transaction advances no nonce, so both sign at 0.
    let message = |token_definition: AccountId| {
        create_msg(
            &i,
            &CreateArgs { token_definition, ..CreateArgs::standard(&i, creator_token_id, creator_coll_id) },
        )
    };

    let m = message(decoy_def);
    let w_set = public_transaction::WitnessSet::for_message(
        &m,
        &[&creator_token_key, &creator_coll_key, &creator_key],
    );
    let result = state.transition_from_public_transaction(&PublicTransaction::new(m, w_set), 0, 0);
    let Err(LeeError::ProgramExecutionFailed(err)) = result else {
        panic!("a create declaring a definition account the holding does not name must revert, got {result:?}");
    };
    assert!(
        err.contains("token_definition: wrong definition id"),
        "it must revert on the definition pin and not for some unrelated reason - a test that \
         passes on any error would pass on the unpinned build too: {err}"
    );
    assert_eq!(
        state.get_account_by_id(i.pool),
        Account::default(),
        "the pool PDA must not exist: a rejected create is not a create"
    );
    assert_eq!(bal(&state, creator_token_id), RESERVE_TOKEN, "the deposit was not taken");

    // Control: repair only the definition slot and the identical create succeeds.
    let m = message(i.token_def);
    let w_set = public_transaction::WitnessSet::for_message(
        &m,
        &[&creator_token_key, &creator_coll_key, &creator_key],
    );
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, w_set), 0, 0)
        .expect("the identical create must succeed once the declared definition is the real one");
    assert_eq!(
        state.get_account_by_id(i.token_vault).program_owner,
        token(),
        "control: the vault the deposit leg claimed is owned by the real token program"
    );
}

/// The other half of the project-token pin: the creator's holding must be owned
/// by the token program that owns the definition it names.
///
/// Without this the mismatch still fails, but only deep inside the chained call
/// as an opaque `UnauthorizedDataModification` (rule 6) naming no cause - and
/// the pair is what makes the deposit's dispatch target unforgeable. See
/// `create_sale_rejects_a_token_definition_account_it_did_not_declare` for the
/// attack the pair defeats.
#[test]
fn create_sale_rejects_a_creator_holding_under_a_foreign_token_program() {
    let creator_token_key = PrivateKey::try_new([41; 32]).expect("key");
    let creator_coll_key = PrivateKey::try_new([43; 32]).expect("key");
    let creator_key = PrivateKey::try_new([42; 32]).expect("key");
    let creator_token_id = id_of(&creator_token_key);
    let creator_coll_id = id_of(&creator_coll_key);
    let i = ids(id_of(&creator_key));

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_create_fixture(&mut state, &i, creator_token_id, creator_coll_id);
    // The one thing this test breaks: impeccable data - the pool's own project
    // definition, a balance that covers the deposit - under a program that is
    // not the token program.
    state.force_insert_account(
        creator_token_id,
        substituted_holding(i.token_def, RESERVE_TOKEN, substituted_program()),
    );

    let m = create_msg(&i, &CreateArgs::standard(&i, creator_token_id, creator_coll_id));
    let w_set = public_transaction::WitnessSet::for_message(
        &m,
        &[&creator_token_key, &creator_coll_key, &creator_key],
    );
    let result = state.transition_from_public_transaction(&PublicTransaction::new(m, w_set), 0, 0);
    let Err(LeeError::ProgramExecutionFailed(err)) = result else {
        panic!("a create depositing from a foreign-owned holding must revert, got {result:?}");
    };
    assert!(
        err.contains("creator token: wrong program"),
        "it must revert on the holding/definition owner bind, with the message that tells the \
         creator which account is wrong: {err}"
    );
    assert_eq!(
        state.get_account_by_id(i.pool),
        Account::default(),
        "the pool PDA must not exist: a rejected create is not a create"
    );
    assert_eq!(
        state.get_account_by_id(i.token_vault),
        Account::default(),
        "and the token vault was never claimed - which is the whole point of the guard"
    );
}

/// Every treasury shape that could never receive the at-close fee, refused at
/// creation - and, at the end, the control that says this fixture would
/// otherwise have opened the pool.
///
/// `SweepTreasury` is the ONLY instruction that pays `treasury_owed` out, its
/// fee leg is a `token::Transfer` dispatched on the collateral vault's own token
/// program, and nothing anywhere rewrites `treasury_id`. So a treasury that
/// cannot receive that transfer does not fail once, it fails FOREVER, and the
/// collateral behind the escrow stays in the vault for good. Creation is the
/// last moment fixing it costs the creator nothing.
///
/// The uninitialised shapes are the ones a STRANGER could weaponise. Settling
/// into an account that does not exist has to CLAIM it, and LEZ admits a claim
/// only when the pre-state is `Account::default()` WHOLE - so anyone could send
/// that id a dust native balance (or get its key to sign anything, which bumps
/// the nonce) and the only instruction that can ever pay this escrow out would
/// fail forever, on a pool they had nothing to do with. Requiring an
/// ALREADY-initialised holding takes that away permanently rather than probably:
/// an initialised holding is owned by the token program, and LEZ has no
/// instruction that un-initialises one (`burn` only lowers a balance), so the
/// state cannot regress between creation and the sweep.
///
/// The first three cases share one message on purpose: unparseable,
/// uninitialised and NFT are one match arm in `create_sale` because the remedy
/// is identical for all three. Mirrors
/// `bonding_curve::create_sale_rejects_every_treasury_that_could_never_be_paid`,
/// which splits the same shapes across two asserts and therefore two messages.
#[test]
fn create_sale_rejects_every_treasury_that_could_never_be_paid() {
    let creator_token_key = PrivateKey::try_new([41; 32]).expect("key");
    let creator_coll_key = PrivateKey::try_new([43; 32]).expect("key");
    let creator_key = PrivateKey::try_new([42; 32]).expect("key");
    let creator_token_id = id_of(&creator_token_key);
    let creator_coll_id = id_of(&creator_coll_key);
    let i = ids(id_of(&creator_key));

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_create_fixture(&mut state, &i, creator_token_id, creator_coll_id);

    // (what the treasury is, the assert that must name it). Each case replaces
    // ONLY the account at slot 8; the id it is declared under never changes.
    let cases: [(&str, Account, &str); 5] = [
        (
            "never initialised - what a creator gets by naming a fresh key as their fee sink",
            Account::default(),
            "treasury is not an initialised Fungible token holding",
        ),
        (
            "uninitialised AND already dusted: the F26 attack state, now refused before the \
             pool exists instead of at the sweep that could never happen",
            Account { balance: 1, ..Account::default() },
            "treasury is not an initialised Fungible token holding",
        ),
        (
            "an NFT holding - it parses, and it still cannot hold a divisible fee",
            nft_holding(i.collateral_def),
            "treasury is not an initialised Fungible token holding",
        ),
        (
            "a holding of the PROJECT token: the fee leg moves collateral, so the transfer \
             reverts on the definition mismatch every single time",
            fungible(i.token_def, 0),
            "treasury holds a different token",
        ),
        (
            "the right definition under a token program the creator deployed themselves - \
             permissionless deployment makes this shape cheap to produce",
            substituted_holding(i.collateral_def, 0, substituted_program()),
            "treasury is not owned by the collateral definition's own token program",
        ),
    ];

    for (case, treasury, expected) in cases {
        state.force_insert_account(i.treasury, treasury);
        let m = create_msg(&i, &CreateArgs::standard(&i, creator_token_id, creator_coll_id));
        let w_set = public_transaction::WitnessSet::for_message(
            &m,
            &[&creator_token_key, &creator_coll_key, &creator_key],
        );
        let result =
            state.transition_from_public_transaction(&PublicTransaction::new(m, w_set), 0, 0);
        let Err(LeeError::ProgramExecutionFailed(err)) = result else {
            panic!("a create whose treasury is {case} must revert, got {result:?}");
        };
        assert!(
            err.contains(expected),
            "the create must revert on the treasury pin ({case}), not for some unrelated \
             reason - a test that passed on any error would pass on the fund-locking build \
             too: {err}"
        );
        assert_eq!(
            state.get_account_by_id(i.pool),
            Account::default(),
            "the pool PDA must not exist: a rejected create is not a create ({case})"
        );
        assert_eq!(
            bal(&state, creator_token_id),
            RESERVE_TOKEN,
            "and the creator's token deposit was not taken ({case})"
        );
        assert_eq!(
            bal(&state, creator_coll_id),
            RESERVE_COLLATERAL,
            "nor their collateral seed ({case})"
        );
    }

    // Control: put a usable treasury back and the identical create goes through.
    // Without it the loop above would still pass if the fixture had rotted in
    // some way that had nothing to do with the treasury.
    state.force_insert_account(i.treasury, fungible(i.collateral_def, 0));
    let m = create_msg(&i, &CreateArgs::standard(&i, creator_token_id, creator_coll_id));
    let w_set = public_transaction::WitnessSet::for_message(
        &m,
        &[&creator_token_key, &creator_coll_key, &creator_key],
    );
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, w_set), 0, 0)
        .expect("the identical create must succeed against an initialised treasury");
    let pool = PoolState::try_from(&state.get_account_by_id(i.pool).data).expect("pool");
    assert_eq!(pool.treasury_id, i.treasury, "control: the pool really was created");
}

/// The treasury ACCOUNT and the treasury ID must be the same account.
///
/// The id is what lands in `PoolState` and what `SweepTreasury` binds the
/// account it is handed to; the account at slot 8 is the only thing `CreateSale`
/// can type-check. Type-check one while pinning the other and the type check
/// secures nothing at all - a creator could hand over a perfectly good treasury
/// to satisfy it and still write an unpayable id into the pool, which is the
/// fund-lock this release closed, reopened by an off-by-one.
///
/// It is also the assert that fires first if this account list ever drifts out
/// of the order the program destructures it in.
#[test]
fn create_sale_rejects_a_treasury_account_that_is_not_the_declared_id() {
    let creator_token_key = PrivateKey::try_new([41; 32]).expect("key");
    let creator_coll_key = PrivateKey::try_new([43; 32]).expect("key");
    let creator_key = PrivateKey::try_new([42; 32]).expect("key");
    let creator_token_id = id_of(&creator_token_key);
    let creator_coll_id = id_of(&creator_coll_key);
    let i = ids(id_of(&creator_key));

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_create_fixture(&mut state, &i, creator_token_id, creator_coll_id);
    // A second, entirely valid treasury: same definition, same token program,
    // initialised. Nothing is wrong with this account except that it is not the
    // one the instruction declares.
    let other_treasury = AccountId::new([19; 32]);
    state.force_insert_account(other_treasury, fungible(i.collateral_def, 0));

    let m = create_msg(
        &i,
        &CreateArgs {
            treasury: other_treasury,
            ..CreateArgs::standard(&i, creator_token_id, creator_coll_id)
        },
    );
    let w_set = public_transaction::WitnessSet::for_message(
        &m,
        &[&creator_token_key, &creator_coll_key, &creator_key],
    );
    let result = state.transition_from_public_transaction(&PublicTransaction::new(m, w_set), 0, 0);
    let Err(LeeError::ProgramExecutionFailed(err)) = result else {
        panic!("a create whose treasury slot is not its treasury_id must revert, got {result:?}");
    };
    assert!(
        err.contains("does not match the treasury_id declared in the instruction"),
        "it must revert on the binding assert - a build that type-checks one account and pins \
         another has no treasury guarantee at all: {err}"
    );
    assert_eq!(
        state.get_account_by_id(i.pool),
        Account::default(),
        "the pool PDA must not exist: a rejected create is not a create"
    );
}

/// F27: an NFT collection cannot be a pool's collateral, and cannot be the token
/// it sells.
///
/// Self-inflicted rather than hostile - a creator names the wrong definition -
/// and unrecoverable, which is what makes it worth an assert. Every balance this
/// program reads goes through `read_fungible`, which panics on any NFT holding,
/// so a pool denominated in a collection would take the creator's deposit and
/// then refuse every buy and every withdrawal.
///
/// The treasury pin makes NFT collateral unconstructible on its own (a
/// `TokenHolding::Fungible` can never name a NonFungible definition, so no valid
/// treasury exists to pass), which is why the collateral case here hands over a
/// treasury forged to name the NFT collection anyway - a holding LEZ itself
/// would never mint. The create must still be refused, and refused by the
/// message that names the definition the creator got wrong rather than one about
/// the treasury: an error about the wrong account is how a creator with a
/// recoverable mistake ends up making an unrecoverable one.
#[test]
fn create_sale_rejects_an_nft_definition_on_either_side() {
    let creator_token_key = PrivateKey::try_new([41; 32]).expect("key");
    let creator_coll_key = PrivateKey::try_new([43; 32]).expect("key");
    let creator_key = PrivateKey::try_new([42; 32]).expect("key");
    let creator_token_id = id_of(&creator_token_key);
    let creator_coll_id = id_of(&creator_coll_key);
    let i = ids(id_of(&creator_key));

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_create_fixture(&mut state, &i, creator_token_id, creator_coll_id);

    let create = |state: &mut V03State| {
        let m = create_msg(&i, &CreateArgs::standard(&i, creator_token_id, creator_coll_id));
        let w_set = public_transaction::WitnessSet::for_message(
            &m,
            &[&creator_token_key, &creator_coll_key, &creator_key],
        );
        state.transition_from_public_transaction(&PublicTransaction::new(m, w_set), 0, 0)
    };

    // 1. NFT COLLATERAL. The treasury and the creator's seed holding are both
    //    Fungible holdings naming the NFT collection - impossible on chain,
    //    seeded here so the rejection cannot be dismissed as a missing account.
    state.force_insert_account(i.collateral_def, nft_definition("Collateral NFT"));
    let result = create(&mut state);
    let Err(LeeError::ProgramExecutionFailed(err)) = result else {
        panic!("a pool denominated in an NFT collection must revert, got {result:?}");
    };
    assert!(
        err.contains("collateral_definition: not Fungible"),
        "it must name the collateral DEFINITION, which is the account the creator got wrong: \
         {err}"
    );

    // 2. AN NFT PROJECT TOKEN. Nothing downstream would notice: the deposit leg
    //    is a `Transfer`, which never parses this account.
    state.force_insert_account(i.collateral_def, token_def_account("Collateral"));
    state.force_insert_account(i.token_def, nft_definition("Project NFT"));
    let result = create(&mut state);
    let Err(LeeError::ProgramExecutionFailed(err)) = result else {
        panic!("a pool selling an NFT collection must revert, got {result:?}");
    };
    assert!(
        err.contains("token_definition: not Fungible"),
        "it must name the project definition: {err}"
    );

    // Control: both definitions Fungible again and the identical create succeeds.
    state.force_insert_account(i.token_def, token_def_account("Project"));
    create(&mut state).expect("the identical create must succeed with two fungible definitions");
    assert_eq!(bal(&state, i.token_vault), RESERVE_TOKEN, "control: the pool really was created");
}

#[test]
fn public_buy_moves_tokens_and_collateral() {
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
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
    assert_eq!(
        bal(&state, i.collateral_vault),
        RESERVE_COLLATERAL + collateral_in,
        "the vault gains the whole C_in - the LBP takes no per-swap fee"
    );
    assert_eq!(bal(&state, i.token_vault), RESERVE_TOKEN - tokens_out);

    let pool = PoolState::try_from(&state.get_account_by_id(i.pool).data).unwrap();
    assert_eq!(pool.reserve_token, RESERVE_TOKEN - tokens_out);
    assert_eq!(pool.reserve_collateral, RESERVE_COLLATERAL + collateral_in);
    assert_eq!(pool.buy_count, 1);
}

/// A keypair `Buy` paying with a token other than the pool's collateral
/// definition is rejected at the program boundary with a clear message, rather
/// than failing later inside the token-program transfer.
#[test]
#[should_panic(expected = "buyer collateral: wrong definition")]
fn public_buy_rejects_wrong_collateral_token() {
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_open_pool(&mut state, &i);

    let buyer_key = PrivateKey::try_new([60; 32]).unwrap();
    let buyer_coll = id_of(&buyer_key);
    let buyer_tok = AccountId::new([61; 32]);
    let wrong_def = AccountId::new([99; 32]);
    state.force_insert_account(buyer_coll, fungible(wrong_def, 100_000));
    state.force_insert_account(buyer_tok, fungible(i.token_def, 0));

    let instruction = Instruction::Buy { collateral_in: 5_000, min_tokens_out: 0, deadline: u64::MAX };
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
}

/// SECURITY regression: a keypair `Buy` paying from a collateral holding that
/// some OTHER program owns must revert, and must leave the pool exactly as it
/// found it.
///
/// The hole this closes: the chained `token::Transfer` legs used to be
/// dispatched to `payer.account.program_owner`, and the payer is an account the
/// submitter hands in. Deployment on LEZ is permissionless, so that account can
/// be one claimed by a no-op program whose `Transfer` echoes its pre-states,
/// carrying data that decodes as a huge collateral holding. Both legs then move
/// nothing - the no-op is not blocked, it simply does nothing, and it could not
/// touch the token-program-owned vaults even if it tried - while `apply_buy`
/// still debits `reserve_token`. Repeated, that walks the reserve to zero for
/// free, after which every honest buy reverts on `tokens_out <= reserve_token`:
/// the pool is unbuyable for the rest of its window at no cost to the attacker,
/// and the LBP has no per-swap fee leg whose token type might have caught it.
///
/// So the assertions below are about the POOL. "The transaction failed" is not
/// the property; "no token reserve was consumed" is.
#[test]
fn buy_rejects_a_collateral_holding_owned_by_another_program() {
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_open_pool(&mut state, &i);

    let buyer_key = PrivateKey::try_new([84; 32]).unwrap();
    let buyer_coll = id_of(&buyer_key);
    let buyer_tok = AccountId::new([85; 32]);
    let buyer_start: u128 = 100_000;
    let collateral_in: u128 = 5_000;
    state.force_insert_account(
        buyer_coll,
        substituted_holding(i.collateral_def, buyer_start, substituted_program()),
    );
    state.force_insert_account(buyer_tok, fungible(i.token_def, 0));

    let tokens_out = expected_tokens_out(collateral_in);
    assert!(tokens_out > 0, "the buy must be one that would really move the pool");

    // The same message twice: once with the substituted holding, once with it
    // repaired. A rejected transaction advances no nonce, so both sign at 0.
    let message = || {
        public_transaction::Message::try_new(
            lbp(),
            vec![i.pool, i.token_vault, i.collateral_vault, buyer_coll, buyer_tok, CLOCK_01],
            vec![Nonce(0)],
            Instruction::Buy { collateral_in, min_tokens_out: 0, deadline: u64::MAX },
        )
        .unwrap()
    };

    let m = message();
    let w = public_transaction::WitnessSet::for_message(&m, &[&buyer_key]);
    let result = state.transition_from_public_transaction(&PublicTransaction::new(m, w), 0, 0);
    let Err(LeeError::ProgramExecutionFailed(err)) = result else {
        panic!("a buy paying from a foreign-owned holding must revert, got {result:?}");
    };
    assert!(
        err.contains("buyer collateral: wrong program"),
        "it must revert on the vault-anchored ownership guard and not for some unrelated \
         reason - a test that passes on any error would pass on the vulnerable build too: {err}"
    );

    let pool = PoolState::try_from(&state.get_account_by_id(i.pool).data).unwrap();
    assert_eq!(pool.reserve_token, RESERVE_TOKEN, "no token reserve consumed");
    assert_eq!(pool.reserve_collateral, RESERVE_COLLATERAL, "no collateral credited");
    assert_eq!(pool.cum_collateral_in, 0);
    assert_eq!(pool.cum_tokens_out, 0);
    assert_eq!(pool.buy_count, 0, "a rejected buy is not a buy");
    assert!(pool.obs.is_empty(), "and it leaves no observation in the price ring");
    assert_eq!(bal(&state, i.token_vault), RESERVE_TOKEN, "token vault untouched");
    assert_eq!(bal(&state, i.collateral_vault), RESERVE_COLLATERAL, "collateral vault untouched");
    assert_eq!(bal(&state, buyer_tok), 0, "no tokens delivered");
    assert_eq!(bal(&state, buyer_coll), buyer_start, "the fake holding is not debited either");

    // Control: repair the holding's owner, change nothing else, and the very
    // same buy goes through. Without this the test above would still pass if the
    // fixture were broken in a way that had nothing to do with ownership.
    state.force_insert_account(buyer_coll, fungible(i.collateral_def, buyer_start));
    let m = message();
    let w = public_transaction::WitnessSet::for_message(&m, &[&buyer_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, w), 0, 0)
        .expect("the identical buy must succeed once the holding is token-owned");
    assert_eq!(bal(&state, buyer_tok), tokens_out, "control: tokens delivered");
    assert_eq!(
        bal(&state, i.collateral_vault),
        RESERVE_COLLATERAL + collateral_in,
        "control: collateral really moved"
    );
    let pool = PoolState::try_from(&state.get_account_by_id(i.pool).data).unwrap();
    assert_eq!(
        pool.reserve_token,
        RESERVE_TOKEN - tokens_out,
        "control: the reserve moves for a real buy"
    );
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
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    // The ATA path dispatches through the ATA program, which is a built-in and so
    // is already registered by `initial_state()` - nothing to deploy.
    seed_open_pool(&mut state, &i);

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
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_open_pool(&mut state, &i);

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

/// Slippage really reverts on chain, and the buyer's collateral is not consumed.
///
/// RFP-016 §Supportability names "slippage revert"; §Reliability 2 requires the
/// failed buy to revert atomically with the collateral untouched. The only
/// slippage test in the LBP was host-side with a degenerate input
/// (`min = u128::MAX`), so neither the boundary nor the on-chain revert was ever
/// exercised. The floor here is exactly `tokens_out + 1` - one unit above what
/// the pool will pay at this instant - so it pins the boundary.
#[test]
fn lbp_buy_reverts_on_slippage_and_consumes_nothing() {
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_gated_pool(&mut state, &i, [0; 32]); // all-zero root = open pool

    let buyer_key = PrivateKey::try_new([66; 32]).unwrap();
    let buyer_coll = id_of(&buyer_key);
    let buyer_tok = AccountId::new([67; 32]);
    let buyer_start: u128 = 100_000;
    fund_buyer(&mut state, &i, buyer_coll, buyer_tok, buyer_start);

    let collateral_in: u128 = 5_000;
    let tokens_out = expected_tokens_out(collateral_in);
    assert!(tokens_out > 0, "fixture must pay a real output or the floor is meaningless");

    let pool_before = state.get_account_by_id(i.pool).clone();
    let vault_coll_before = bal(&state, i.collateral_vault);
    let vault_tok_before = bal(&state, i.token_vault);

    let msg = |min_out: u128| {
        public_transaction::Message::try_new(
            lbp(),
            vec![i.pool, i.token_vault, i.collateral_vault, buyer_coll, buyer_tok, CLOCK_01],
            vec![Nonce(0)],
            Instruction::Buy { collateral_in, min_tokens_out: min_out, deadline: u64::MAX },
        )
        .expect("message")
    };

    let m = msg(tokens_out + 1);
    let w = public_transaction::WitnessSet::for_message(&m, &[&buyer_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, w), 0, 0)
        .expect_err("a buy below min_tokens_out must be rejected");

    assert_eq!(bal(&state, buyer_coll), buyer_start, "the buyer's collateral is NOT consumed");
    assert_eq!(bal(&state, buyer_tok), 0, "no tokens delivered");
    assert_eq!(bal(&state, i.collateral_vault), vault_coll_before, "collateral vault untouched");
    assert_eq!(bal(&state, i.token_vault), vault_tok_before, "token vault untouched");
    assert_eq!(
        state.get_account_by_id(i.pool),
        pool_before,
        "a rejected buy must leave the pool account byte-identical"
    );

    // Control: the identical buy at exactly the payable floor succeeds, so the
    // rejection above was slippage and not a broken fixture.
    let m = msg(tokens_out);
    let w = public_transaction::WitnessSet::for_message(&m, &[&buyer_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, w), 0, 0)
        .expect("the identical buy must succeed when the floor is exactly met");
    assert_eq!(bal(&state, buyer_tok), tokens_out, "control: tokens delivered");
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

/// Fold a sorted pair exactly as `lbp_core::merkle_verify` does: SHA-256 over
/// `min(a,b) || max(a,b)`. Sorting is what removes the need for direction bits.
fn parent(a: [u8; 32], b: [u8; 32]) -> [u8; 32] {
    use sha2::{Digest as _, Sha256};
    let mut h = Sha256::new();
    if a <= b {
        h.update(a);
        h.update(b);
    } else {
        h.update(b);
        h.update(a);
    }
    h.finalize().into()
}

/// A gated buy against a FOUR-member allowlist, with a real two-step proof.
///
/// Every other gated test here uses a single-member tree, where the root IS the
/// leaf and the proof is empty - so `merkle_verify`'s fold loop never executes on
/// chain at all. The Merkle verification the allowlist gate rests on has,
/// literally, never run in a test against the state machine. This is the test
/// that runs it, and it also pins the two properties an inclusion proof must
/// have: a genuine member's path is accepted, and the SAME path presented for a
/// different account is not.
#[test]
fn gated_buy_verifies_a_real_multi_member_merkle_proof() {
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);

    // Four members; our buyer is index 0.
    let buyer_key = PrivateKey::try_new([70; 32]).unwrap();
    let buyer_coll = id_of(&buyer_key);
    let buyer_tok = AccountId::new([71; 32]);
    let others: Vec<AccountId> = [72u8, 73, 74]
        .into_iter()
        .map(|b| id_of(&PrivateKey::try_new([b; 32]).unwrap()))
        .collect();

    let l0 = allowlist_leaf(&buyer_coll);
    let l1 = allowlist_leaf(&others[0]);
    let l2 = allowlist_leaf(&others[1]);
    let l3 = allowlist_leaf(&others[2]);
    let root = parent(parent(l0, l1), parent(l2, l3));
    // Sibling, then the far subtree: two hashes, so the fold really loops.
    let proof = vec![l1, parent(l2, l3)];
    assert!(
        lbp_core::merkle_verify(l0, &proof, root),
        "the fixture's own proof must verify, or the test proves nothing"
    );

    seed_gated_pool(&mut state, &i, root);
    fund_buyer(&mut state, &i, buyer_coll, buyer_tok, 100_000);

    let collateral_in = 5_000u128;
    let tokens_out = expected_tokens_out(collateral_in);
    let msg = gated_msg(&i, buyer_coll, buyer_tok, l0, proof.clone(), collateral_in);
    let witness = public_transaction::WitnessSet::for_message(&msg, &[&buyer_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(msg, witness), 0, 0)
        .expect("a member with a valid multi-step proof must be admitted");
    assert_eq!(bal(&state, buyer_tok), tokens_out, "member received tokens");

    // The leaf is bound to the authorizing account, so a member's own published
    // (leaf, proof) is useless to anyone else. Without this binding a gated sale
    // would be open to whoever first watched a member buy.
    let thief_key = PrivateKey::try_new([79; 32]).unwrap();
    let thief_coll = id_of(&thief_key);
    let thief_tok = AccountId::new([78; 32]);
    fund_buyer(&mut state, &i, thief_coll, thief_tok, 100_000);
    let msg = gated_msg(&i, thief_coll, thief_tok, l0, proof, collateral_in);
    let witness = public_transaction::WitnessSet::for_message(&msg, &[&thief_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(msg, witness), 0, 0)
        .expect_err("a valid proof for someone else's leaf must not admit this buyer");
    assert_eq!(bal(&state, thief_tok), 0, "and no tokens were delivered");
}

#[test]
fn gated_buy_admits_an_allowlisted_member() {
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
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
    let mut state = testnet_initial_state::initial_state();
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
    let mut state = testnet_initial_state::initial_state();
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
    let mut state = testnet_initial_state::initial_state();
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

// --- At-close fee / withdraw / sweep (RFP-016 Func #5) -------------------
// The at-close protocol fee is the most subtle accounting in the program: it
// taxes ONLY buyer-raised collateral (cum_collateral_in), not the creator's own
// seed, via `close_fee(cum_collateral_in.min(creator_gross), fee_bps)`.
//
// `Withdraw` pays the creator that net collateral plus every unsold project
// token in a 2-leg chained sequence and names no treasury account at all: the
// fee is ESCROWED into `PoolState::treasury_owed`, its collateral left sitting
// in the vault, and the permissionless `SweepTreasury` hands it over afterwards.
// Paying it in the same transaction is what the fund-lock tests below exist to
// keep out of the program - see
// `an_unusable_treasury_cannot_block_the_creators_withdrawal`.

#[test]
fn withdraw_taxes_only_raised_collateral_and_pays_the_creator() {
    let creator_key = PrivateKey::try_new([42; 32]).unwrap();
    let creator = id_of(&creator_key);
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);

    // A CLOSED pool that raised 10_000 from buyers on top of a 5_000 creator seed.
    let raised = 10_000u128; // == cum_collateral_in
    let seed = 5_000u128; // creator's own collateral seed
    let vault_collateral = seed + raised; // 15_000 sits in the collateral vault
    let unsold_tokens = 200_000u128;
    let mut p = open_pool(&i);
    p.status = SaleStatus::Closed;
    p.cum_collateral_in = raised;
    let pool_acc = Account {
        program_owner: lbp(),
        data: Data::from(&p),
        ..Default::default()
    };
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

    // Six accounts, and no treasury among them. That absence is what makes the
    // withdrawal independent of whether the treasury can receive.
    let message = public_transaction::Message::try_new(
        lbp(),
        vec![i.pool, i.token_vault, i.collateral_vault, cc, ct, creator],
        vec![Nonce(0)],
        Instruction::Withdraw { deadline: u64::MAX },
    )
    .unwrap();
    let witness = public_transaction::WitnessSet::for_message(&message, &[&creator_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(message, witness), 0, 0)
        .expect("creator withdraw must succeed");

    assert_eq!(bal(&state, cc), net, "creator gets seed + raised − fee");
    assert_eq!(bal(&state, ct), unsold_tokens, "creator gets the unsold tokens");
    assert_eq!(bal(&state, i.token_vault), 0, "token vault drained");
    assert_eq!(bal(&state, i.treasury), 0, "withdraw names no treasury, so none is paid");
    assert_eq!(
        bal(&state, i.collateral_vault),
        fee,
        "the fee and the collateral backing it STAY in the vault: this is the invariant \
         `collateral_vault_balance == reserve_collateral + treasury_owed`, still true after a \
         withdrawal because the withdrawal took only the creator's share"
    );
    let pool = PoolState::try_from(&state.get_account_by_id(i.pool).data).unwrap();
    assert_eq!(pool.treasury_owed, fee, "and the escrow is recorded, not paid");
    assert_eq!(pool.reserve_collateral, 0, "the creator's collateral bucket is zeroed");
    // Seed-exclusion: had the fee taxed the whole balance it would be larger.
    assert!(
        fee < close_fee(vault_collateral, p.fee_bps),
        "the creator's seed must be excluded from the fee base"
    );

    // The escrow settles in its own transaction, and only then does the treasury
    // see anything. No signer and no nonce: anyone may submit it.
    let sweep = public_transaction::Message::try_new(
        lbp(),
        vec![i.pool, i.collateral_vault, i.treasury],
        vec![],
        Instruction::SweepTreasury { deadline: u64::MAX },
    )
    .unwrap();
    let witness = public_transaction::WitnessSet::for_message(&sweep, &[]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(sweep, witness), 1, 0)
        .expect("a permissionless sweep must settle the escrow after the withdrawal");

    assert_eq!(bal(&state, i.treasury), fee, "treasury gets the at-close fee (raised only)");
    assert_eq!(bal(&state, i.collateral_vault), 0, "collateral vault finally drained");
    assert_eq!(bal(&state, cc), net, "the creator's payout is untouched by the sweep");
    let pool = PoolState::try_from(&state.get_account_by_id(i.pool).data).unwrap();
    assert_eq!(pool.treasury_owed, 0, "the escrow bucket is zeroed once paid");
}

/// Take a freshly seeded open pool through one real public buy and the
/// creator's `CloseSale`, so the escrow tests below start from a pool that got
/// where it is the way a real one does. Returns the collateral raised, which is
/// also the pool's `cum_collateral_in` and therefore the whole fee base.
///
/// The creator identity account has to be owned by SOMETHING: it signs the close
/// and then the withdraw, and the close bumps its nonce - after which
/// `validate_execution` rule 7 (a post state with the default program owner
/// requires a fully default pre state) rejects any transaction that still
/// declares it while it is unclaimed. A real creator's account is owned by the
/// token or native program, so this is what production looks like, not a
/// convenience.
fn raise_and_close(state: &mut V03State, i: &Ids, creator_key: &PrivateKey) -> u128 {
    seed_open_pool(state, i);
    state.force_insert_account(i.creator, fungible(i.collateral_def, 0));

    let buyer_key = PrivateKey::try_new([60; 32]).unwrap();
    let buyer_coll = id_of(&buyer_key);
    let buyer_tok = AccountId::new([61; 32]);
    fund_buyer(state, i, buyer_coll, buyer_tok, 100_000);
    let collateral_in = 20_000_u128;
    let buy = public_transaction::Message::try_new(
        lbp(),
        vec![i.pool, i.token_vault, i.collateral_vault, buyer_coll, buyer_tok, CLOCK_01],
        vec![Nonce(0)],
        Instruction::Buy { collateral_in, min_tokens_out: 0, deadline: u64::MAX },
    )
    .unwrap();
    let witness = public_transaction::WitnessSet::for_message(&buy, &[&buyer_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(buy, witness), 1, 0)
        .expect("the public buy must be included");
    assert_eq!(
        bal(state, i.collateral_vault),
        RESERVE_COLLATERAL + collateral_in,
        "the LBP takes no per-swap fee: the whole C_in lands in the vault, and nothing is owed \
         to the treasury until Withdraw escrows the at-close fee"
    );

    // Close at the pool's own end timestamp. Withdrawal is gated on the pool
    // being Closed, not on a clock, so this is the last time-dependent step.
    set_clock(state, 2, i64::try_from(T_END).unwrap());
    let close = public_transaction::Message::try_new(
        lbp(),
        vec![i.pool, i.creator, CLOCK_01],
        vec![Nonce(0)],
        Instruction::CloseSale { deadline: u64::MAX },
    )
    .unwrap();
    let witness = public_transaction::WitnessSet::for_message(&close, &[creator_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(close, witness), 2, T_END)
        .expect("the creator must be able to close at the end timestamp");
    collateral_in
}

/// The creator's withdrawal: six accounts, and NO treasury among them. That
/// absence is the fix - it is what makes the payout independent of whether the
/// treasury can receive.
fn withdraw_msg(
    i: &Ids,
    cc: AccountId,
    ct: AccountId,
    creator_nonce: Nonce,
) -> public_transaction::Message {
    public_transaction::Message::try_new(
        lbp(),
        vec![i.pool, i.token_vault, i.collateral_vault, cc, ct, i.creator],
        vec![creator_nonce],
        Instruction::Withdraw { deadline: u64::MAX },
    )
    .unwrap()
}

/// The settlement: three accounts and no signer. Permissionless because there is
/// nothing here for a submitter to choose - payer, recipient and amount all come
/// off the pool state - so the most a stranger can do is deliver the treasury its
/// own money and pay the gas.
///
/// `nonces` is still a parameter because one test submits this SIGNED: the
/// treasury's own signature used to be what bootstrapped an uninitialised
/// treasury, and that path is gone now that creation refuses to pin one. See
/// `a_treasury_create_sale_could_never_have_accepted_is_refused_by_the_sweep`,
/// which submits it both ways and gets the same refusal.
fn sweep_msg(i: &Ids, nonces: Vec<Nonce>) -> public_transaction::Message {
    public_transaction::Message::try_new(
        lbp(),
        vec![i.pool, i.collateral_vault, i.treasury],
        nonces,
        Instruction::SweepTreasury { deadline: u64::MAX },
    )
    .unwrap()
}

/// The whole escrow path end to end, in the order it happens on chain: a real
/// buy raises collateral, the creator closes, `Withdraw` pays them everything
/// that is theirs and leaves the fee behind, and the permissionless
/// `SweepTreasury` hands it over afterwards.
///
/// Settling inside `Withdraw` is what the first cut of this feature did, and
/// `an_unusable_treasury_cannot_block_the_creators_withdrawal` is the other half
/// of this pair: it pins the reason the two were split. Mirrors
/// `bonding_curve::withdraw_leaves_the_escrow_and_sweep_treasury_settles_it`.
#[test]
fn withdraw_leaves_the_escrow_and_sweep_treasury_settles_it() {
    let creator_key = PrivateKey::try_new([42; 32]).unwrap();
    let i = ids(id_of(&creator_key));
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    state.force_insert_account(i.treasury, fungible(i.collateral_def, 0));
    let raised = raise_and_close(&mut state, &i, &creator_key);

    let vault_collateral = bal(&state, i.collateral_vault);
    let unsold_tokens = bal(&state, i.token_vault);
    let fee = close_fee(raised.min(vault_collateral), FEE_BPS);
    assert!(fee > 0 && unsold_tokens > 0, "the test must exercise a non-zero escrow and payout");

    let cc = AccountId::new([70; 32]); // creator collateral destination
    let ct = AccountId::new([71; 32]); // creator token destination
    state.force_insert_account(cc, fungible(i.collateral_def, 0));
    state.force_insert_account(ct, fungible(i.token_def, 0));

    let m = withdraw_msg(&i, cc, ct, Nonce(1));
    let witness = public_transaction::WitnessSet::for_message(&m, &[&creator_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, witness), 3, T_END)
        .expect("withdraw must succeed");

    assert_eq!(bal(&state, cc), vault_collateral - fee, "the creator is paid the raise NET of \
         the escrow, seed included");
    assert_eq!(bal(&state, ct), unsold_tokens, "and every unsold project token");
    assert_eq!(bal(&state, i.token_vault), 0, "token vault drained");
    assert_eq!(bal(&state, i.treasury), 0, "withdraw names no treasury, so none is paid");
    assert_eq!(
        bal(&state, i.collateral_vault),
        fee,
        "the escrow and its backing collateral STAY in the vault: this is the invariant \
         `collateral_vault_balance == reserve_collateral + treasury_owed`, still true after a \
         withdrawal because the withdrawal took only the creator's share"
    );
    let pool = PoolState::try_from(&state.get_account_by_id(i.pool).data).unwrap();
    assert_eq!(
        pool.treasury_owed, fee,
        "and the bucket is NOT zeroed by the withdrawal - zeroing it there is what turned the \
         escrow into an unclaimable balance"
    );

    // The sweep: three accounts, no signer, no nonce. Anyone may submit it, which
    // is why `lifecycle::sweep_treasury` anchors its dispatch on the vault rather
    // than on the treasury it was handed.
    let m = sweep_msg(&i, vec![]);
    let witness = public_transaction::WitnessSet::for_message(&m, &[]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, witness), 4, T_END)
        .expect("a permissionless sweep must be able to settle the escrow after the withdrawal");

    assert_eq!(bal(&state, i.treasury), fee, "the escrowed at-close fee reaches the treasury");
    assert_eq!(bal(&state, i.collateral_vault), 0, "and the collateral vault is finally drained");
    assert_eq!(
        bal(&state, cc),
        vault_collateral - fee,
        "the creator's payout is untouched by the sweep"
    );
    let pool = PoolState::try_from(&state.get_account_by_id(i.pool).data).unwrap();
    assert_eq!(pool.treasury_owed, 0, "the escrow bucket is zeroed once paid");

    // A second sweep reverts rather than paying twice. The instruction is
    // permissionless and carries no nonce of its own, so "submit it again" is
    // free for anyone to do - the revert is the only thing standing between that
    // and a drained vault.
    let m = sweep_msg(&i, vec![]);
    let witness = public_transaction::WitnessSet::for_message(&m, &[]);
    let result =
        state.transition_from_public_transaction(&PublicTransaction::new(m, witness), 5, T_END);
    let Err(LeeError::ProgramExecutionFailed(err)) = result else {
        panic!("re-sweeping a settled pool must revert, got {result:?}");
    };
    assert!(
        err.contains("nothing owed to the treasury"),
        "it must revert on the owed==0 guard, not on a transfer that happens to fail: {err}"
    );
    assert_eq!(bal(&state, i.treasury), fee, "and the treasury is not paid twice");
}

/// The fund-lock this decoupling exists to prevent: a treasury that cannot
/// receive must not be able to hold the creator's entire raise hostage.
///
/// `CreateSale` now refuses every treasury that could never receive the fee -
/// see `create_sale_rejects_every_treasury_that_could_never_be_paid` - so the
/// pool below is one this program can no longer build, and the fixture
/// force-inserts it. That is the reason the test stays, not a reason to delete
/// it: the creation pin is the first line of defence, and this is the second,
/// the one that BOUNDS the damage if the first is ever loosened - by a new
/// creation path, a migration, or a pool that predates the pin. When settlement
/// was a leg of `Withdraw`, a treasury that reverts took the whole withdrawal
/// with it: the creator's collateral AND the unsold project tokens, locked by a
/// fee bucket worth a percent of the raise, with no other drain on a closed
/// pool. Here the treasury is a holding of the PROJECT token rather than the
/// collateral token, its `token::Transfer` can never succeed, and the creator
/// still gets everything that is theirs.
///
/// The escrow itself does stay stranded, and that is what this test pins the
/// boundary of: the fee, and only the fee (`PoolState::treasury_owed` spells out
/// why nothing in this program can release it once it is stuck). Mirrors
/// `bonding_curve::an_unusable_treasury_cannot_block_the_creators_withdrawal`.
#[test]
fn an_unusable_treasury_cannot_block_the_creators_withdrawal() {
    let creator_key = PrivateKey::try_new([42; 32]).unwrap();
    let i = ids(id_of(&creator_key));
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    // The one difference from the test above: the treasury holds the PROJECT
    // token, not the collateral token. Nothing on chain rejects that at creation,
    // and the `token::Transfer` into it can never succeed.
    state.force_insert_account(i.treasury, fungible(i.token_def, 0));
    let raised = raise_and_close(&mut state, &i, &creator_key);

    let vault_collateral = bal(&state, i.collateral_vault);
    let unsold_tokens = bal(&state, i.token_vault);
    let fee = close_fee(raised.min(vault_collateral), FEE_BPS);
    assert!(fee > 0, "the test must exercise a non-zero escrow");

    let cc = AccountId::new([70; 32]);
    let ct = AccountId::new([71; 32]);
    state.force_insert_account(cc, fungible(i.collateral_def, 0));
    state.force_insert_account(ct, fungible(i.token_def, 0));

    let m = withdraw_msg(&i, cc, ct, Nonce(1));
    let witness = public_transaction::WitnessSet::for_message(&m, &[&creator_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, witness), 3, T_END)
        .expect(
            "withdraw must succeed even though the treasury cannot receive - if this fails, \
             settlement has been folded back into Withdraw and an unusable fee sink locks the \
             entire raise again",
        );

    assert_eq!(
        bal(&state, cc),
        vault_collateral - fee,
        "the creator got the whole raise net of the fee"
    );
    assert_eq!(bal(&state, ct), unsold_tokens, "and every unsold project token");
    assert_eq!(bal(&state, i.token_vault), 0, "token vault drained");

    // The sweep is the leg that fails, and it fails alone.
    let m = sweep_msg(&i, vec![]);
    let witness = public_transaction::WitnessSet::for_message(&m, &[]);
    let result =
        state.transition_from_public_transaction(&PublicTransaction::new(m, witness), 4, T_END);
    let Err(LeeError::ProgramExecutionFailed(err)) = result else {
        panic!("sweeping into a wrong-definition treasury must revert, got {result:?}");
    };
    assert!(
        err.contains("Sender and recipient definition id mismatch"),
        "it must fail deep inside the token program's transfer - the failure creation now \
         refuses to let a creator sign up for, and the one that is unrecoverable when it does \
         happen: {err}"
    );

    // Nothing was consumed by the failed sweep: the escrow is stranded, not
    // destroyed, so a later fix at the token level could still release it.
    assert_eq!(bal(&state, i.collateral_vault), fee, "the escrow is still in the vault");
    let pool = PoolState::try_from(&state.get_account_by_id(i.pool).data).unwrap();
    assert_eq!(pool.treasury_owed, fee, "and still recorded as owed, not written off");
}

/// A treasury `CreateSale` could never have accepted - and what the sweep does
/// when it is handed one anyway.
///
/// This pool is force-inserted, and could not exist otherwise: its treasury was
/// never initialised, which creation now refuses outright (see
/// `create_sale_rejects_every_treasury_that_could_never_be_paid`), and no LEZ
/// instruction can un-initialise a holding afterwards - `burn` only lowers a
/// balance - so the state cannot regress into this shape either. Two things
/// about it are still worth pinning.
///
/// FIRST: the creator's withdrawal does not care. It names no treasury at all,
/// which is the entire reason settlement is a separate instruction, so the raise
/// and every unsold token come out exactly as usual.
///
/// SECOND: the sweep refuses it UP FRONT, in words that say retrying cannot
/// help, rather than letting the submitter pay for a transaction that dies deep
/// in the framework as `ClaimedUnauthorizedAccount`. What used to stand there
/// was a bootstrap path: `token::transfer` creates a default recipient with
/// `new_claimed_if_default(.., Claim::Authorized)`, so the treasury's OWN
/// signature could claim the account and fill it in. This test submits the sweep
/// both ways - unsigned, and signed by the treasury key - to say that path is
/// gone. It was removed because a claim needs the pre-state to be
/// `Account::default()` WHOLE, so any stranger could send that id dust and the
/// bootstrap would stop working forever: a "remedy" the attacker always won the
/// race for. Refusing the shape at creation is the fix. Keeping the assert here
/// is what turns an unreachable state into a named error instead of a framework
/// rejection nobody can act on.
#[test]
fn a_treasury_create_sale_could_never_have_accepted_is_refused_by_the_sweep() {
    let creator_key = PrivateKey::try_new([42; 32]).unwrap();
    // A key-derived treasury, because one of the two sweeps below is SIGNED by
    // it; `ids` hands out a bare `[9; 32]` nobody can sign for. The pool PDA
    // does not depend on the treasury, so nothing else moves.
    let treasury_key = PrivateKey::try_new([44; 32]).unwrap();
    let mut i = ids(id_of(&creator_key));
    i.treasury = id_of(&treasury_key);
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    // ...and the treasury account is deliberately never inserted: it is a
    // default account, the shape a creator used to get by naming a fresh key as
    // their fee sink.
    let raised = raise_and_close(&mut state, &i, &creator_key);
    assert_eq!(
        state.get_account_by_id(i.treasury),
        Account::default(),
        "the treasury must still be uninitialised when the withdrawal runs"
    );

    let vault_collateral = bal(&state, i.collateral_vault);
    let unsold_tokens = bal(&state, i.token_vault);
    let fee = close_fee(raised.min(vault_collateral), FEE_BPS);
    assert!(fee > 0, "the test must exercise a non-zero escrow");

    let cc = AccountId::new([70; 32]);
    let ct = AccountId::new([71; 32]);
    state.force_insert_account(cc, fungible(i.collateral_def, 0));
    state.force_insert_account(ct, fungible(i.token_def, 0));

    let m = withdraw_msg(&i, cc, ct, Nonce(1));
    let witness = public_transaction::WitnessSet::for_message(&m, &[&creator_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, witness), 3, T_END)
        .expect(
            "withdraw must succeed against a treasury that does not exist yet - it is not one \
             of the accounts the instruction declares",
        );
    assert_eq!(
        bal(&state, cc),
        vault_collateral - fee,
        "the creator got the whole raise net of the fee"
    );
    assert_eq!(bal(&state, ct), unsold_tokens, "and every unsold project token");

    // Both sweeps, one after the other. A signature is the only thing that used
    // to make a difference here, and it must not make one now.
    for (block, label, nonces, keys) in [
        (4_u64, "unsigned", vec![], Vec::new()),
        (5, "signed by the treasury itself", vec![Nonce(0)], vec![&treasury_key]),
    ] {
        let m = sweep_msg(&i, nonces);
        let witness = public_transaction::WitnessSet::for_message(&m, &keys);
        let result = state.transition_from_public_transaction(
            &PublicTransaction::new(m, witness),
            block,
            T_END,
        );
        let Err(LeeError::ProgramExecutionFailed(err)) = result else {
            panic!("a sweep ({label}) into an uninitialised treasury must revert, got {result:?}");
        };
        assert!(
            err.contains("pool treasury is unowned"),
            "the sweep ({label}) must refuse on the treasury guard - the assert that names that \
             account, and whose refusal is final. Matching it exactly rather than on any \
             `treasury` wording keeps this from also passing on the vault/owed divergence \
             assert further down (`vault empty but treasury_owed > 0`), and a bootstrap that \
             only sometimes works is what made this griefable in the first place: {err}"
        );
        assert_eq!(bal(&state, i.collateral_vault), fee, "and the escrow is untouched ({label})");
    }

    let pool = PoolState::try_from(&state.get_account_by_id(i.pool).data).unwrap();
    assert_eq!(pool.treasury_owed, fee, "still recorded as owed, not written off");
}

/// F26, end to end: the dust grief is dead, and the escrow settles into a
/// treasury a stranger has dusted.
///
/// THE LOCK THIS REPLACES. `CreateSale` used to accept a `treasury_id` naming an
/// account that did not exist, and settling then had to CLAIM that account. LEZ
/// admits a claim only when the pre-state is `Account::default()` WHOLE, so
/// anyone could send the (publicly readable) treasury id a dust native balance
/// and the only instruction that can ever pay the escrow out would fail forever,
/// on a pool they had nothing to do with, for the price of one transfer.
///
/// Requiring an ALREADY-initialised holding at creation is what kills it, and
/// this test walks the whole attack rather than just `create_sale`'s asserts:
/// the stranger dusts the announced fee sink and the CREATE IS REFUSED, the
/// creator names an initialised holding instead and the pool is built BY THE
/// PROGRAM, the stranger dusts THAT account once the sale has closed, and the
/// sweep still settles into it - because a holding the token program already
/// owns is never claimed, so its native balance and its nonce stop being able to
/// matter. The first of those is the mutation kill: remove the pin and the
/// refused create succeeds, which this test asserts it must not.
///
/// Mirrors `bonding_curve::a_dusted_treasury_still_settles_after_a_real_create_sale`,
/// which has to carry a private buy to accrue an escrow at all; the LBP escrows
/// its fee in `Withdraw`, so the whole path is public here.
#[test]
fn a_dusted_treasury_still_settles_after_a_real_create_sale() {
    let creator_token_key = PrivateKey::try_new([41; 32]).expect("key");
    let creator_coll_key = PrivateKey::try_new([43; 32]).expect("key");
    let creator_key = PrivateKey::try_new([42; 32]).expect("key");
    let creator_token_id = id_of(&creator_token_key);
    let creator_coll_id = id_of(&creator_coll_key);
    let i = ids(id_of(&creator_key));

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_create_fixture(&mut state, &i, creator_token_id, creator_coll_id);

    // THE ATTACK, at the only moment it is still free. A stranger dusts the fee
    // sink this creator announced, and that id can now never be initialised at
    // all - `token::InitializeAccount` asserts its target is `Account::default()`
    // WHOLE, exactly as a claim does - so the old build would have pinned into
    // the pool a treasury that could never receive a token, and the at-close fee
    // would have been dead before it accrued. Creation refuses it instead, and
    // all the creator has lost is one transaction and the id: they name an
    // initialised holding and carry on. THIS assert is what the rest of the test
    // rests on, and the one whose removal brings the fund-lock back.
    let dusted_id = AccountId::new([29; 32]);
    state.force_insert_account(dusted_id, Account { balance: 1, ..Account::default() });
    let m = create_msg(
        &i,
        &CreateArgs {
            treasury: dusted_id,
            treasury_id: dusted_id,
            ..CreateArgs::standard(&i, creator_token_id, creator_coll_id)
        },
    );
    let witness = public_transaction::WitnessSet::for_message(
        &m,
        &[&creator_token_key, &creator_coll_key, &creator_key],
    );
    let result = state.transition_from_public_transaction(&PublicTransaction::new(m, witness), 0, 0);
    let Err(LeeError::ProgramExecutionFailed(err)) = result else {
        panic!("a create pinning a dusted, never-initialised treasury must revert, got {result:?}");
    };
    assert!(
        err.contains("treasury is not an initialised Fungible token holding"),
        "the create must refuse the dusted fee sink: {err}"
    );
    assert_eq!(
        state.get_account_by_id(i.pool),
        Account::default(),
        "and no pool exists to carry an unsettleable escrow"
    );

    // A REAL create against the initialised treasury, so the pin is genuinely
    // exercised and everything below runs against a pool this program built.
    let m = create_msg(&i, &CreateArgs::standard(&i, creator_token_id, creator_coll_id));
    let witness = public_transaction::WitnessSet::for_message(
        &m,
        &[&creator_token_key, &creator_coll_key, &creator_key],
    );
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, witness), 0, 0)
        .expect("create_sale must succeed against an initialised treasury");

    // A real buy raises collateral. The LBP takes no per-swap fee, so the whole
    // C_in lands in the vault and nothing is owed until `Withdraw` escrows the
    // at-close fee.
    let buyer_key = PrivateKey::try_new([60; 32]).unwrap();
    let buyer_coll = id_of(&buyer_key);
    let buyer_tok = AccountId::new([61; 32]);
    fund_buyer(&mut state, &i, buyer_coll, buyer_tok, 100_000);
    let collateral_in = 20_000_u128;
    let buy = public_transaction::Message::try_new(
        lbp(),
        vec![i.pool, i.token_vault, i.collateral_vault, buyer_coll, buyer_tok, CLOCK_01],
        vec![Nonce(0)],
        Instruction::Buy { collateral_in, min_tokens_out: 0, deadline: u64::MAX },
    )
    .unwrap();
    let witness = public_transaction::WitnessSet::for_message(&buy, &[&buyer_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(buy, witness), 1, 0)
        .expect("the public buy must be included");
    assert_eq!(
        bal(&state, i.collateral_vault),
        RESERVE_COLLATERAL + collateral_in,
        "the seed and the raise are both in the vault"
    );

    set_clock(&mut state, 2, i64::try_from(T_END).unwrap());
    let close = public_transaction::Message::try_new(
        lbp(),
        vec![i.pool, i.creator, CLOCK_01],
        vec![Nonce(1)], // the creator already signed the create
        Instruction::CloseSale { deadline: u64::MAX },
    )
    .unwrap();
    let witness = public_transaction::WitnessSet::for_message(&close, &[&creator_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(close, witness), 2, T_END)
        .expect("the creator must be able to close at the end timestamp");

    // THE ATTACK AGAIN, on the treasury this pool actually pinned. Harmless now:
    // the account is already a holding the token program owns, so the fee leg
    // writes to it instead of claiming it, and the native balance is simply
    // carried through.
    let dusted = Account { balance: 1, ..state.get_account_by_id(i.treasury) };
    state.force_insert_account(i.treasury, dusted);

    let vault_collateral = bal(&state, i.collateral_vault);
    let unsold_tokens = bal(&state, i.token_vault);
    let fee = close_fee(collateral_in.min(vault_collateral), FEE_BPS);
    assert!(fee > 0 && unsold_tokens > 0, "the test must exercise a non-zero escrow and payout");

    let cc = AccountId::new([70; 32]);
    let ct = AccountId::new([71; 32]);
    state.force_insert_account(cc, fungible(i.collateral_def, 0));
    state.force_insert_account(ct, fungible(i.token_def, 0));
    // Nonce 2: the creator signed the create, then the close.
    let m = withdraw_msg(&i, cc, ct, Nonce(2));
    let witness = public_transaction::WitnessSet::for_message(&m, &[&creator_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, witness), 3, T_END)
        .expect("withdraw must succeed");
    assert_eq!(bal(&state, cc), vault_collateral - fee, "the creator is paid the raise net of \
         the escrow, seed included");
    assert_eq!(bal(&state, ct), unsold_tokens, "and every unsold project token");
    assert_eq!(bal(&state, i.collateral_vault), fee, "the escrow stays behind in the vault");

    let m = sweep_msg(&i, vec![]);
    let witness = public_transaction::WitnessSet::for_message(&m, &[]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, witness), 4, T_END)
        .expect(
            "the permissionless sweep must settle into a DUSTED treasury - if this reverts, \
             the fund-lock a stranger could impose for the price of a dust transfer is back",
        );

    assert_eq!(bal(&state, i.treasury), fee, "the escrowed at-close fee reached the treasury");
    assert_eq!(bal(&state, i.collateral_vault), 0, "and the collateral vault is drained");
    let pool = PoolState::try_from(&state.get_account_by_id(i.pool).data).unwrap();
    assert_eq!(pool.treasury_owed, 0, "nothing is left owed");
    assert_eq!(
        state.get_account_by_id(i.treasury).balance,
        1,
        "the dust is still there and still irrelevant - the sweep wrote the holding's data and \
         carried the native balance through, which is exactly why an INITIALISED treasury \
         cannot be griefed"
    );
}

/// SECURITY regression: the ungated `Buy`/`BuyAta` instructions must REJECT a
/// pool that has an allowlist, otherwise the gate is trivially bypassed by
/// choosing the ungated instruction.
#[test]
fn plain_buy_is_rejected_on_a_gated_pool() {
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
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


// --- BuyDisposable: the private buy --------------------------------------
//
// These are not public transactions. Each builds a real
// `PrivacyPreservingTransaction`: `execute_and_prove` runs the committed guest
// ELF and then the LEZ privacy circuit over its output, and
// `transition_from_privacy_preserving_transaction` applies the result to the
// same `V03State` the public tests use.
//
// Privacy is a per-slot LABEL. `InputAccountIdentity` is positionally aligned
// 1:1 with the guest's `pre_states`, so "the buyer's holdings are private" is
// literally the last two entries of that vector being private rather than
// `Public`. There is no ephemeral account, no deshield leg and no re-shield
// leg - "disposable" names the single-use NOTE. The SDK's 3-transaction
// `buy-private` saga is a different mechanism and still ships.

/// Slot indices of the buyer's two private notes within `buyer_keys()`.
const COLLATERAL_NOTE: u128 = 0;
const TOKEN_NOTE: u128 = 1;

/// Turn on risc0 dev mode for this test binary.
///
/// `execute_and_prove` is the only thing in this suite that PROVES rather than
/// executes, and a real succinct proof of the privacy circuit takes minutes of
/// CPU - far too slow to gate a PR on. Dev mode still runs the guest and the
/// circuit in the rv32im emulator, so every assert exercised below is really
/// evaluated; it only swaps the receipt for a fake one that `Receipt::verify`
/// accepts while dev mode is on. Mirrors `detect_proof_mode` in
/// `cli/src/online.rs`.
///
/// If you are changing this: the value must be set before any prover is built,
/// so call it first in every test that proves.
///
/// SAFETY: `set_var` is not thread-safe and libtest runs tests in parallel.
/// `Once` makes the write happen exactly once, and makes it happen-before every
/// return from `call_once`; since no test here reaches a prover without calling
/// this first, no prover in this binary can observe the variable mid-write.
fn force_dev_mode() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe { std::env::set_var("RISC0_DEV_MODE", "1") });
}

/// The key material behind a private account: `nsk` spends its notes, `(d, z)`
/// derive the viewing key. The account id is a hash of the two public halves,
/// which is precisely why a private slot has no address anyone can look up.
struct PrivateKeys {
    nsk: NullifierSecretKey,
    d: [u8; 32],
    z: [u8; 32],
}
impl PrivateKeys {
    fn npk(&self) -> NullifierPublicKey {
        NullifierPublicKey::from(&self.nsk)
    }
    fn vpk(&self) -> ViewingPublicKey {
        ViewingPublicKey::from_seed(&self.d, &self.z)
    }
    /// Id of this key's `identifier`-th private account.
    fn id(&self, identifier: u128) -> AccountId {
        AccountId::for_regular_private_account(&self.npk(), &self.vpk(), identifier)
    }
    fn view_tag(&self) -> ViewTag {
        EncryptedAccountData::compute_view_tag(&self.npk(), &self.vpk())
    }
}
fn buyer_keys() -> PrivateKeys {
    PrivateKeys { nsk: [77; 32], d: [78; 32], z: [79; 32] }
}

/// A token holding as it exists inside a private NOTE: the same account
/// [`fungible`] builds, carrying the nonce the circuit stamps on a freshly
/// initialised private account.
fn private_holding(def: AccountId, balance: u128, id: AccountId) -> Account {
    Account { nonce: Nonce::private_account_nonce_init(&id), ..fungible(def, balance) }
}

/// Seed `note` as an already-on-chain private note for `id`.
///
/// The paired nullifier is the one the initialising transaction would have
/// published (`for_account_initialization`), so the seeded state is exactly what
/// a real shield leaves behind - and, as on chain, that account can never be
/// initialised a second time.
fn seed_private_note(state: V03State, id: AccountId, note: &Account) -> V03State {
    state.with_private_accounts([(
        Commitment::new(&id, note),
        Nullifier::for_account_initialization(&id),
    )])
}

/// Spend-and-replace identity for an existing note: proves membership of the
/// note's commitment, and authorises the slot with `nsk` rather than a signature.
fn spend_note(
    keys: &PrivateKeys,
    identifier: u128,
    note: &Account,
    state: &V03State,
    random_seed: [u8; 32],
) -> InputAccountIdentity {
    let commitment = Commitment::new(&keys.id(identifier), note);
    InputAccountIdentity::PrivateAuthorizedUpdate {
        vpk: keys.vpk(),
        random_seed,
        view_tag: keys.view_tag(),
        nsk: keys.nsk,
        membership_proof: state
            .get_proof_for_commitment(&commitment)
            .expect("the note being spent must already be in the commitment set"),
        identifier,
    }
}

/// Identity for a brand-new note the buyer owns. The pre-state must be
/// `Account::default()`; the token program's credit leg is what fills it in.
fn new_note(
    keys: &PrivateKeys,
    identifier: u128,
    state: &V03State,
    random_seed: [u8; 32],
) -> InputAccountIdentity {
    InputAccountIdentity::PrivateAuthorizedInit {
        vpk: keys.vpk(),
        random_seed,
        nsk: keys.nsk,
        identifier,
        commitment_root: state.commitment_root(),
    }
}

/// Prove a `BuyDisposable` over its five accounts and wrap it in a
/// `PrivacyPreservingTransaction`.
///
/// ZERO public signers, which is legal and is the point: the only two accounts
/// that would have signed are the buyer's, and a private slot is authorised by
/// its nullifier key inside the circuit, not by a signature on the message. The
/// three public slots are the pool and its two vaults, all unauthorized at top
/// level - the vaults get their authority from the chained calls' PDA seeds.
///
/// Returns the `LeeError` instead of panicking so the prove-time reverts can be
/// asserted on: those failures happen on the buyer's own machine and the
/// transaction is never submitted at all.
fn prove_disposable(
    state: &V03State,
    i: &Ids,
    keys: &PrivateKeys,
    collateral_note: &Account,
    instruction: Instruction,
) -> Result<PrivacyPreservingTransaction, LeeError> {
    let pre_states = vec![
        AccountWithMetadata::new(state.get_account_by_id(i.pool), false, i.pool),
        AccountWithMetadata::new(state.get_account_by_id(i.token_vault), false, i.token_vault),
        AccountWithMetadata::new(
            state.get_account_by_id(i.collateral_vault),
            false,
            i.collateral_vault,
        ),
        AccountWithMetadata::new(collateral_note.clone(), true, keys.id(COLLATERAL_NOTE)),
        AccountWithMetadata::new(Account::default(), true, keys.id(TOKEN_NOTE)),
    ];
    let identities = vec![
        InputAccountIdentity::Public,
        InputAccountIdentity::Public,
        InputAccountIdentity::Public,
        spend_note(keys, COLLATERAL_NOTE, collateral_note, state, [1; 32]),
        new_note(keys, TOKEN_NOTE, state, [2; 32]),
    ];
    // The two chained `token::Transfer`s run the token program, so it must be
    // declared as a dependency or the circuit rejects the call as undeclared.
    let program = ProgramWithDependencies::new(
        lpad_guests::lbp(),
        HashMap::from([(token(), programs::token())]),
    );
    let (output, proof) = execute_and_prove(
        pre_states,
        Program::serialize_instruction(instruction).expect("instruction serializes"),
        identities,
        &program,
    )?;
    let message = PrivacyMessage::from_circuit_output(vec![], output);
    let witness = PrivacyWitnessSet::for_message(&message, proof, &[]);
    Ok(PrivacyPreservingTransaction::new(message, witness))
}

/// Seed an open pool whose [`PoolState`] `tweak` has adjusted, with BOTH vaults
/// funded consistently with the reserves the pool records.
///
/// `seed_open_pool` deliberately leaves the collateral vault absent (the public
/// tests exercise the lazy claim); the disposable tests need the vault balance
/// and `reserve_collateral` to agree, because they assert the post-buy balance
/// against the price the reserves imply. Returns the seeded state so the caller
/// can price against exactly what went on chain.
fn seed_tweaked_pool(
    state: &mut V03State,
    i: &Ids,
    tweak: impl FnOnce(&mut PoolState),
) -> PoolState {
    let mut pool = open_pool(i);
    tweak(&mut pool);
    let pool_acc = Account {
        program_owner: lbp(),
        data: Data::from(&pool),
        ..Default::default()
    };
    state.force_insert_account(i.pool, pool_acc);
    state.force_insert_account(i.token_vault, fungible(i.token_def, pool.reserve_token));
    state.force_insert_account(
        i.collateral_vault,
        fungible(i.collateral_def, pool.reserve_collateral),
    );
    pool
}

/// Fund the buyer's private collateral note, returning the updated state and the
/// note. `start` is what the note holds before the buy. Takes the state by value
/// because `with_private_accounts` does.
fn fund_private_collateral(
    state: V03State,
    i: &Ids,
    keys: &PrivateKeys,
    start: u128,
) -> (V03State, Account) {
    let id = keys.id(COLLATERAL_NOTE);
    let note = private_holding(i.collateral_def, start, id);
    (seed_private_note(state, id, &note), note)
}

/// The buyer's private collateral note, claimed by `program_owner` instead of by
/// the token program - the private-path form of [`substituted_holding`]. A note
/// commits to whatever account bytes its owner chose to put in it, so this is
/// something the attacker can really produce.
fn fund_substituted_private_collateral(
    state: V03State,
    i: &Ids,
    keys: &PrivateKeys,
    start: u128,
    program_owner: lee_core::program::ProgramId,
) -> (V03State, Account) {
    let id = keys.id(COLLATERAL_NOTE);
    let note = Account { program_owner, ..private_holding(i.collateral_def, start, id) };
    (seed_private_note(state, id, &note), note)
}

/// Overwrite the canonical clock account with `ts_ms`, which is what the public
/// instructions read for their time-dependent pricing and guards.
///
/// [`lbp_core::ClockData`] is deserialize-only - the sequencer writes that
/// account, programs only read it - so its 16-byte Borsh body is written out by
/// hand: a `u64` block id then an `i64` millisecond timestamp, both
/// little-endian. `probe.rs` decodes the same layout.
fn set_clock(state: &mut V03State, block_id: u64, ts_ms: i64) {
    let mut clock = state.get_account_by_id(CLOCK_01);
    let mut bytes = Vec::with_capacity(16);
    bytes.extend_from_slice(&block_id.to_le_bytes());
    bytes.extend_from_slice(&ts_ms.to_le_bytes());
    clock.data = Data::try_from(bytes).expect("16 bytes is well within the account data cap");
    state.force_insert_account(CLOCK_01, clock);
}

/// A buy big enough that the output and the reserve move are both non-zero.
const DISPOSABLE_IN: u128 = 5_000;

#[test]
fn disposable_buy_moves_the_pool_and_credits_a_private_note() {
    force_dev_mode();
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_tweaked_pool(&mut state, &i, |_| {});

    let keys = buyer_keys();
    let start: u128 = 100_000;
    let (mut state, note) = fund_private_collateral(state, &i, &keys, start);

    let t_buy_ms = 100_000_u64;
    let tokens_out = expected_tokens_out_at(DISPOSABLE_IN, t_buy_ms);
    assert!(tokens_out > 0, "the buy must move a non-zero output");

    let tx = prove_disposable(
        &state,
        &i,
        &keys,
        &note,
        Instruction::BuyDisposable {
            collateral_in: DISPOSABLE_IN,
            min_tokens_out: 0,
            t_buy_ms,
            deadline: u64::MAX,
        },
    )
    .expect("proving a disposable buy against an open pool must succeed");
    state
        .transition_from_privacy_preserving_transaction(&tx, 1, t_buy_ms)
        .expect("a disposable buy inside its window must be included");

    // The public half of the trade: the pool moved and both vaults settled.
    assert_eq!(
        bal(&state, i.collateral_vault),
        RESERVE_COLLATERAL + DISPOSABLE_IN,
        "collateral vault receives the whole collateral_in (the LBP has no per-swap fee)"
    );
    assert_eq!(
        bal(&state, i.token_vault),
        RESERVE_TOKEN - tokens_out,
        "token vault paid tokens_out"
    );

    let pool = PoolState::try_from(&state.get_account_by_id(i.pool).data).unwrap();
    assert_eq!(pool.reserve_token, RESERVE_TOKEN - tokens_out);
    assert_eq!(pool.reserve_collateral, RESERVE_COLLATERAL + DISPOSABLE_IN);
    assert_eq!(pool.cum_tokens_out, tokens_out);
    assert_eq!(pool.buy_count, 1);
    assert!(
        pool.obs.is_empty(),
        "a buyer-chosen timestamp must never be written into the observation ring - it \
         would let anyone forge the oracle, and a conspicuously stale observation would \
         itself fingerprint the private trade"
    );

    // The private half: two notes, both recognisable only with the buyer's keys.
    let credited = private_holding(i.token_def, tokens_out, keys.id(TOKEN_NOTE));
    assert!(
        state
            .get_proof_for_commitment(&Commitment::new(&keys.id(TOKEN_NOTE), &credited))
            .is_some(),
        "tokens_out must land in a private note the buyer can open"
    );
    let change = Account {
        nonce: note.nonce.private_account_nonce_increment(&keys.nsk),
        ..fungible(i.collateral_def, start - DISPOSABLE_IN)
    };
    assert!(
        state
            .get_proof_for_commitment(&Commitment::new(&keys.id(COLLATERAL_NOTE), &change))
            .is_some(),
        "the buyer's change must land in a private note too"
    );
}

/// THE UNSKIPPABILITY TEST - the whole security claim, machine-checked.
///
/// The saga's failure mode is that its middle leg is a public buy: an observer
/// sees the trade, and a client that dies between legs leaves the funds sitting
/// in the open. `BuyDisposable` has no such leg to skip, and this asserts it
/// structurally rather than by inspection: after the transition the
/// transaction's PUBLIC account ids are exactly the pool and its two vaults, and
/// the buyer's two holdings resolve to nothing in public state. The collateral
/// was spent and the tokens were delivered, so there is no reachable state in
/// which the payment happened and the tokens are public.
#[test]
fn disposable_buy_publishes_only_the_pool_and_its_vaults() {
    force_dev_mode();
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_tweaked_pool(&mut state, &i, |_| {});

    let keys = buyer_keys();
    let (mut state, note) = fund_private_collateral(state, &i, &keys, 100_000);
    let t_buy_ms = 100_000_u64;
    let tokens_out = expected_tokens_out_at(DISPOSABLE_IN, t_buy_ms);

    let tx = prove_disposable(
        &state,
        &i,
        &keys,
        &note,
        Instruction::BuyDisposable {
            collateral_in: DISPOSABLE_IN,
            min_tokens_out: 0,
            t_buy_ms,
            deadline: u64::MAX,
        },
    )
    .expect("proving must succeed");

    assert_eq!(
        tx.message().public_account_ids(),
        vec![i.pool, i.token_vault, i.collateral_vault],
        "a disposable buy publishes the pool and its two vaults and NOTHING else - no \
         clock, no weight-observation PDA, and neither buyer holding"
    );

    state
        .transition_from_privacy_preserving_transaction(&tx, 1, t_buy_ms)
        .expect("transition must succeed");

    for (label, id) in [
        ("collateral", keys.id(COLLATERAL_NOTE)),
        ("token", keys.id(TOKEN_NOTE)),
    ] {
        assert!(
            state.get_account_by_id_ref(id).is_none(),
            "the buyer's {label} holding must not exist in public state"
        );
        assert_eq!(
            state.get_account_by_id(id),
            Account::default(),
            "looking up the buyer's {label} holding must return the default account"
        );
    }
    // ...and yet the trade settled: the collateral is in the vault and the
    // tokens left it. There is no half-state where one happened without the other.
    assert_eq!(bal(&state, i.collateral_vault), RESERVE_COLLATERAL + DISPOSABLE_IN);
    assert_eq!(bal(&state, i.token_vault), RESERVE_TOKEN - tokens_out);
}

/// The LBP's price depends on time, and a private buy has no clock to read it
/// from - so it prices at the caller-supplied `t_buy_ms`. That is only sound
/// because `buy_tokens_out` is non-decreasing in time at fixed reserves: pricing
/// at an EARLIER moment than admission hands the buyer FEWER tokens, so the pool
/// is never underpaid by a buyer who back-dates their quote.
///
/// The direction of the inequality is the whole safety argument, so it is
/// asserted rather than described.
#[test]
fn disposable_buy_prices_at_t_buy_ms_and_never_better() {
    force_dev_mode();
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_tweaked_pool(&mut state, &i, |_| {});

    let keys = buyer_keys();
    let (mut state, note) = fund_private_collateral(state, &i, &keys, 100_000);

    let t_buy_ms = 100_000_u64;
    let admitted_at = 900_000_u64; // still inside [t_buy_ms, t_end_ms)
    let at_quote = expected_tokens_out_at(DISPOSABLE_IN, t_buy_ms);
    let at_admission = expected_tokens_out_at(DISPOSABLE_IN, admitted_at);
    assert!(
        at_quote < at_admission,
        "the fixture must actually exercise the schedule: {at_quote} tokens at the quote \
         time must be strictly fewer than the {at_admission} the same buy would fetch at \
         admission"
    );

    let tx = prove_disposable(
        &state,
        &i,
        &keys,
        &note,
        Instruction::BuyDisposable {
            collateral_in: DISPOSABLE_IN,
            min_tokens_out: 0,
            t_buy_ms,
            deadline: u64::MAX,
        },
    )
    .expect("proving must succeed");
    state
        .transition_from_privacy_preserving_transaction(&tx, 1, admitted_at)
        .expect("the buy is admitted late but still inside its window");

    assert_eq!(
        bal(&state, i.token_vault),
        RESERVE_TOKEN - at_quote,
        "the pool paid out the price at t_buy_ms, NOT the (larger) price at admission"
    );
    assert!(
        state
            .get_proof_for_commitment(&Commitment::new(
                &keys.id(TOKEN_NOTE),
                &private_holding(i.token_def, at_quote, keys.id(TOKEN_NOTE)),
            ))
            .is_some(),
        "the buyer's note holds the t_buy_ms price"
    );
}

#[test]
fn disposable_buy_is_rejected_before_t_buy_ms() {
    force_dev_mode();
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_tweaked_pool(&mut state, &i, |_| {});

    let keys = buyer_keys();
    let (mut state, note) = fund_private_collateral(state, &i, &keys, 100_000);
    let t_buy_ms = 100_000_u64;

    let tx = prove_disposable(
        &state,
        &i,
        &keys,
        &note,
        Instruction::BuyDisposable {
            collateral_in: DISPOSABLE_IN,
            min_tokens_out: 0,
            t_buy_ms,
            deadline: 200_000,
        },
    )
    .expect("proving must succeed");
    assert_eq!(
        tx.message().timestamp_validity_window.start(),
        Some(t_buy_ms),
        "the window's lower bound must be the price timestamp itself - it is the only \
         thing tying a clock-less quote to real time"
    );

    let early = state.transition_from_privacy_preserving_transaction(&tx, 1, t_buy_ms - 1);
    assert!(
        matches!(early, Err(LeeError::OutOfValidityWindow)),
        "a buy submitted before the moment it was priced at must be rejected as out of \
         validity window, got {early:?}"
    );
    assert_eq!(bal(&state, i.collateral_vault), RESERVE_COLLATERAL, "no collateral moved");
    assert_eq!(bal(&state, i.token_vault), RESERVE_TOKEN, "no tokens moved");

    // The same proof, one millisecond later, is admitted - so the rejection above
    // was the bound and not some unrelated defect in the transaction.
    state
        .transition_from_privacy_preserving_transaction(&tx, 1, t_buy_ms)
        .expect("at t_buy_ms the window is open (it is inclusive from below)");
}

#[test]
fn disposable_buy_is_rejected_at_the_pool_end_timestamp() {
    force_dev_mode();
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_tweaked_pool(&mut state, &i, |_| {});

    let keys = buyer_keys();
    let (mut state, note) = fund_private_collateral(state, &i, &keys, 100_000);

    // Deadline u64::MAX and a quote well inside the schedule: the pool's own
    // t_end_ms is the only bound that can produce T_END here.
    let tx = prove_disposable(
        &state,
        &i,
        &keys,
        &note,
        Instruction::BuyDisposable {
            collateral_in: DISPOSABLE_IN,
            min_tokens_out: 0,
            t_buy_ms: 900_000,
            deadline: u64::MAX,
        },
    )
    .expect("proving must succeed");
    assert_eq!(
        tx.message().timestamp_validity_window.end(),
        Some(T_END),
        "the window's upper bound must be the pool's t_end_ms - a private buy carries no \
         clock, so this is the ONLY way it can honour the end of the sale"
    );

    let at_end = state.transition_from_privacy_preserving_transaction(&tx, 1, T_END);
    assert!(
        matches!(at_end, Err(LeeError::OutOfValidityWindow)),
        "the window is half-open, so a buy at exactly t_end_ms must be rejected, got {at_end:?}"
    );
    assert_eq!(bal(&state, i.collateral_vault), RESERVE_COLLATERAL, "no collateral moved");
    assert_eq!(bal(&state, i.token_vault), RESERVE_TOKEN, "no tokens moved");

    state
        .transition_from_privacy_preserving_transaction(&tx, 1, T_END - 1)
        .expect("one millisecond before the end the pool is still open");
}

/// The guest CLAMPS the validity window instead of echoing what the submitter
/// asked for. With `deadline = u64::MAX` and a pool whose end is far away, the
/// only bound left is the staleness cap, and it must be the one that lands in
/// the transaction - otherwise a buyer could hold a finished proof indefinitely
/// and submit it only once the weight schedule had moved their way (a free
/// option on the pool).
#[test]
fn disposable_buy_window_is_clamped_to_the_staleness_cap() {
    force_dev_mode();
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    // A schedule far longer than the staleness cap, so the cap is what binds.
    let distant_end = 100_000_000_u64;
    seed_tweaked_pool(&mut state, &i, |p| p.t_end_ms = distant_end);

    let keys = buyer_keys();
    let (mut state, note) = fund_private_collateral(state, &i, &keys, 100_000);
    let t_buy_ms = 1_000_000_u64;
    let capped_hi = t_buy_ms + MAX_PRIVATE_WINDOW_MS;
    assert!(capped_hi < distant_end, "the cap, not the pool's end, must be the binding bound");

    let tx = prove_disposable(
        &state,
        &i,
        &keys,
        &note,
        Instruction::BuyDisposable {
            collateral_in: DISPOSABLE_IN,
            min_tokens_out: 0,
            t_buy_ms,
            deadline: u64::MAX,
        },
    )
    .expect("proving must succeed");
    assert_eq!(
        tx.message().timestamp_validity_window.end(),
        Some(capped_hi),
        "the caller asked for u64::MAX; the guest must emit t_buy_ms + \
         MAX_PRIVATE_WINDOW_MS instead"
    );

    let stale = state.transition_from_privacy_preserving_transaction(&tx, 1, capped_hi);
    assert!(
        matches!(stale, Err(LeeError::OutOfValidityWindow)),
        "a proof submitted MAX_PRIVATE_WINDOW_MS after its own t_buy_ms must be rejected \
         even though the caller's deadline was u64::MAX, got {stale:?}"
    );
    assert_eq!(bal(&state, i.collateral_vault), RESERVE_COLLATERAL, "no collateral moved");

    state
        .transition_from_privacy_preserving_transaction(&tx, 1, capped_hi - 1)
        .expect("one millisecond inside the cap the proof is still good");
}

/// The drift trade-off, made explicit. A disposable buy pins the pool PDA
/// byte-for-byte at proving time and the sequencer re-verifies it against live
/// state at inclusion, so a competing PUBLIC buy that lands first invalidates
/// the proof. This is the price of atomicity and privacy, and it is a rejection,
/// never a mispriced fill - which matters more here than on the bonding curve,
/// because the pinned pool is also the precondition that makes pricing at a
/// caller-chosen `t_buy_ms` safe at all.
#[test]
fn disposable_buy_is_rejected_after_a_competing_public_buy() {
    force_dev_mode();
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_tweaked_pool(&mut state, &i, |_| {});

    let keys = buyer_keys();
    let (mut state, note) = fund_private_collateral(state, &i, &keys, 100_000);
    let t_buy_ms = 100_000_u64;
    let tokens_out = expected_tokens_out_at(DISPOSABLE_IN, t_buy_ms);

    let tx = prove_disposable(
        &state,
        &i,
        &keys,
        &note,
        Instruction::BuyDisposable {
            collateral_in: DISPOSABLE_IN,
            min_tokens_out: 0,
            t_buy_ms,
            deadline: u64::MAX,
        },
    )
    .expect("proving must succeed");

    // A competing public buy lands while the private buyer is still proving.
    let comp_key = PrivateKey::try_new([80; 32]).unwrap();
    let comp_coll = id_of(&comp_key);
    let comp_tok = AccountId::new([81; 32]);
    fund_buyer(&mut state, &i, comp_coll, comp_tok, 100_000);
    let m = public_transaction::Message::try_new(
        lbp(),
        vec![i.pool, i.token_vault, i.collateral_vault, comp_coll, comp_tok, CLOCK_01],
        vec![Nonce(0)],
        Instruction::Buy { collateral_in: 7_000, min_tokens_out: 0, deadline: u64::MAX },
    )
    .unwrap();
    let w = public_transaction::WitnessSet::for_message(&m, &[&comp_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, w), 0, 0)
        .expect("competing buy must succeed");
    let moved = PoolState::try_from(&state.get_account_by_id(i.pool).data).unwrap();
    assert!(moved.reserve_token < RESERVE_TOKEN, "the competing buy must actually move the pool");

    let stale = state.transition_from_privacy_preserving_transaction(&tx, 1, t_buy_ms);
    assert!(
        matches!(stale, Err(LeeError::InvalidPrivacyPreservingProof)),
        "the pinned pool pre-state no longer matches live state, so the proof must be \
         rejected as invalid rather than settled at the stale price, got {stale:?}"
    );
    assert!(
        state
            .get_proof_for_commitment(&Commitment::new(
                &keys.id(TOKEN_NOTE),
                &private_holding(i.token_def, tokens_out, keys.id(TOKEN_NOTE)),
            ))
            .is_none(),
        "no note may be created by a rejected transaction"
    );
    let after = PoolState::try_from(&state.get_account_by_id(i.pool).data).unwrap();
    assert_eq!(after.buy_count, 1, "only the competing public buy counted");
}

/// The old clock-substitution attack is gone by construction. `BuyDisposable`
/// destructures a fixed-arity five-account list, so a submitter who bolts
/// `CLOCK_01` on the end does not get a clock-reading buy - they get a guest
/// panic, on their own machine, before any transaction exists. (The public
/// paths still take a clock and still pin it to `CLOCK_01`; those tests are
/// untouched.)
#[test]
fn disposable_buy_declaring_a_clock_account_cannot_be_proved() {
    force_dev_mode();
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_tweaked_pool(&mut state, &i, |_| {});

    let keys = buyer_keys();
    let (state, note) = fund_private_collateral(state, &i, &keys, 100_000);

    // The five real accounts, plus the clock the attacker wants read.
    let pre_states = vec![
        AccountWithMetadata::new(state.get_account_by_id(i.pool), false, i.pool),
        AccountWithMetadata::new(state.get_account_by_id(i.token_vault), false, i.token_vault),
        AccountWithMetadata::new(
            state.get_account_by_id(i.collateral_vault),
            false,
            i.collateral_vault,
        ),
        AccountWithMetadata::new(note.clone(), true, keys.id(COLLATERAL_NOTE)),
        AccountWithMetadata::new(Account::default(), true, keys.id(TOKEN_NOTE)),
        AccountWithMetadata::new(state.get_account_by_id(CLOCK_01), false, CLOCK_01),
    ];
    let identities = vec![
        InputAccountIdentity::Public,
        InputAccountIdentity::Public,
        InputAccountIdentity::Public,
        spend_note(&keys, COLLATERAL_NOTE, &note, &state, [1; 32]),
        new_note(&keys, TOKEN_NOTE, &state, [2; 32]),
        InputAccountIdentity::Public,
    ];
    let result = execute_and_prove(
        pre_states,
        Program::serialize_instruction(Instruction::BuyDisposable {
            collateral_in: DISPOSABLE_IN,
            min_tokens_out: 0,
            t_buy_ms: 100_000,
            deadline: u64::MAX,
        })
        .unwrap(),
        identities,
        &ProgramWithDependencies::new(
            lpad_guests::lbp(),
            HashMap::from([(token(), programs::token())]),
        ),
    );
    let Err(LeeError::ProgramProveFailed(message)) = result else {
        panic!("a six-account BuyDisposable must fail to prove, got {result:?}");
    };
    assert!(
        message.contains("BuyDisposable: 5 accounts"),
        "it must fail on the fixed-arity destructure, not somewhere later: {message}"
    );
}

/// A quote outside `[t_start_ms, t_end_ms)` never becomes a transaction: the
/// guard is the same one the public path runs, and it fires on the buyer's own
/// machine at prove time.
#[test]
fn disposable_buy_outside_the_schedule_cannot_be_proved() {
    force_dev_mode();
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);

    for (label, t_start_ms, t_buy_ms, expected) in [
        ("at the end of the schedule", T_START, T_END, "sale has ended"),
        ("before it starts", 200_000_u64, 199_999_u64, "sale has not started"),
    ] {
        let mut state = testnet_initial_state::initial_state();
        deploy(&mut state);
        seed_tweaked_pool(&mut state, &i, |p| p.t_start_ms = t_start_ms);
        let keys = buyer_keys();
        let (state, note) = fund_private_collateral(state, &i, &keys, 100_000);

        let result = prove_disposable(
            &state,
            &i,
            &keys,
            &note,
            Instruction::BuyDisposable {
                collateral_in: DISPOSABLE_IN,
                min_tokens_out: 0,
                t_buy_ms,
                deadline: u64::MAX,
            },
        );
        let Err(LeeError::ProgramProveFailed(message)) = result else {
            panic!("a buy priced {label} must fail to prove, got {result:?}");
        };
        assert!(
            message.contains(expected),
            "a buy priced {label} must fail with \"{expected}\", got: {message}"
        );
    }
}

/// Two pool controls that `BuyDisposable` cannot honour, and therefore refuses
/// outright rather than silently bypassing: a silent bypass is the same bug as
/// having no control at all, only harder to notice.
///
///   * an ALLOWLIST binds an inclusion proof to the buyer's own leaf, and there
///     is no private equivalent of that binding - so a gated pool must be bought
///     through `BuyGated`;
///   * a PER-BLOCK CEILING is keyed on the clock's block id, and a proof carries
///     no block id to key on.
#[test]
fn disposable_buy_is_refused_by_pool_controls_it_cannot_enforce() {
    force_dev_mode();
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let member = id_of(&PrivateKey::try_new([99; 32]).unwrap());

    let gated: Box<dyn FnOnce(&mut PoolState)> =
        Box::new(move |p: &mut PoolState| p.allowlist_root = allowlist_leaf(&member));
    let ceilinged: Box<dyn FnOnce(&mut PoolState)> =
        Box::new(|p: &mut PoolState| p.block_token_ceiling = 1);

    for (label, tweak, expected) in [
        ("an allowlist-gated pool", gated, "allowlist-gated and cannot be bought privately"),
        ("a per-block-ceilinged pool", ceilinged, "per-block ceiling"),
    ] {
        let mut state = testnet_initial_state::initial_state();
        deploy(&mut state);
        seed_tweaked_pool(&mut state, &i, tweak);
        let keys = buyer_keys();
        let (state, note) = fund_private_collateral(state, &i, &keys, 100_000);

        let result = prove_disposable(
            &state,
            &i,
            &keys,
            &note,
            Instruction::BuyDisposable {
                collateral_in: DISPOSABLE_IN,
                min_tokens_out: 0,
                t_buy_ms: 100_000,
                deadline: u64::MAX,
            },
        );
        let Err(LeeError::ProgramProveFailed(message)) = result else {
            panic!("a disposable buy against {label} must fail to prove, got {result:?}");
        };
        assert!(
            message.contains(expected),
            "{label} must be refused with \"{expected}\", got: {message}"
        );
    }
}

/// The same substitution through the PRIVATE path. Cheapest version of the
/// attack to run: the buyer's holdings are private notes, so the account whose
/// owner is being lied about never appears in public state at all, and the pool
/// it drains its reserve from is the only thing an observer sees move.
///
/// The fixture is the one `disposable_buy_moves_the_pool_and_credits_a_private_note`
/// proves and includes successfully; the note's `program_owner` is the only
/// difference, which is what makes the rejection attributable to it.
#[test]
fn disposable_buy_rejects_a_collateral_note_owned_by_another_program() {
    force_dev_mode();
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_tweaked_pool(&mut state, &i, |_| {});

    let keys = buyer_keys();
    let (state, note) =
        fund_substituted_private_collateral(state, &i, &keys, 100_000, substituted_program());

    let t_buy_ms = 100_000_u64;
    let result = prove_disposable(
        &state,
        &i,
        &keys,
        &note,
        Instruction::BuyDisposable {
            collateral_in: DISPOSABLE_IN,
            min_tokens_out: 0,
            t_buy_ms,
            deadline: u64::MAX,
        },
    );
    let Err(LeeError::ProgramProveFailed(message)) = result else {
        panic!(
            "a disposable buy paying from a foreign-owned note must fail to prove, got {result:?}"
        );
    };
    assert!(
        message.contains("buyer collateral: wrong program"),
        "it must fail on the vault-anchored ownership guard, not on the definition check \
         before it or anything after it: {message}"
    );

    // The revert lands on the buyer's own machine, so there is no transaction to
    // reject and the pool is untouched by construction. Assert it anyway: the
    // claim being defended is about the pool's state, not about the prover's
    // return value.
    let pool = PoolState::try_from(&state.get_account_by_id(i.pool).data).unwrap();
    assert_eq!(pool.reserve_token, RESERVE_TOKEN, "no token reserve consumed");
    assert_eq!(pool.reserve_collateral, RESERVE_COLLATERAL, "no collateral credited");
    assert_eq!(pool.cum_tokens_out, 0);
    assert_eq!(pool.buy_count, 0);
    assert_eq!(bal(&state, i.token_vault), RESERVE_TOKEN, "token vault untouched");
    assert_eq!(bal(&state, i.collateral_vault), RESERVE_COLLATERAL, "collateral vault untouched");
    assert!(
        state
            .get_proof_for_commitment(&Commitment::new(
                &keys.id(TOKEN_NOTE),
                &private_holding(
                    i.token_def,
                    expected_tokens_out_at(DISPOSABLE_IN, t_buy_ms),
                    keys.id(TOKEN_NOTE),
                ),
            ))
            .is_none(),
        "and no token note may exist: the buy never became a transaction"
    );
}

/// `Poke` must not be able to grief in-flight private buys.
///
/// The stored weight used to live in the pool account, which made a
/// permissionless, economically inert `Poke` into a denial-of-service on
/// `BuyDisposable`: a private buy pins the pool byte-for-byte at proving time,
/// so any write to it invalidates every proof in flight. The observation now
/// lives in its own per-pool PDA that a private buy never declares. Both halves
/// are asserted here - the pool account comes back byte-identical, and a proof
/// built BEFORE the poke still lands after it.
#[test]
fn poke_leaves_the_pool_byte_identical_so_private_buys_survive_it() {
    force_dev_mode();
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_tweaked_pool(&mut state, &i, |_| {});

    let keys = buyer_keys();
    let (mut state, note) = fund_private_collateral(state, &i, &keys, 100_000);
    let t_buy_ms = 100_000_u64;
    let tokens_out = expected_tokens_out_at(DISPOSABLE_IN, t_buy_ms);

    // Prove FIRST: the buyer is holding a finished proof when the poke lands.
    let tx = prove_disposable(
        &state,
        &i,
        &keys,
        &note,
        Instruction::BuyDisposable {
            collateral_in: DISPOSABLE_IN,
            min_tokens_out: 0,
            t_buy_ms,
            deadline: u64::MAX,
        },
    )
    .expect("proving must succeed");

    let weight_obs = compute_weight_obs_pda(lbp(), i.pool);
    assert!(
        state.get_account_by_id_ref(weight_obs).is_none(),
        "the observation PDA is claimed lazily and must not exist before the first poke"
    );
    let pool_before = state.get_account_by_id(i.pool);

    // Anyone may poke; it takes no signatures at all.
    let poke_ts = 500_000_u64;
    set_clock(&mut state, 2, i64::try_from(poke_ts).unwrap());
    let m = public_transaction::Message::try_new(
        lbp(),
        vec![i.pool, weight_obs, CLOCK_01],
        vec![],
        Instruction::Poke { deadline: u64::MAX },
    )
    .unwrap();
    let w = public_transaction::WitnessSet::for_message(&m, &[]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, w), 2, poke_ts)
        .expect("poke is permissionless and must succeed");

    assert_eq!(
        state.get_account_by_id(i.pool),
        pool_before,
        "Poke must echo the pool account byte-identical - anything else lets a cheap, \
         economically inert transaction invalidate every private buy in flight"
    );
    assert_eq!(
        WeightObs::try_from(&state.get_account_by_id(weight_obs).data).expect("weight obs"),
        WeightObs { w_token_q64: weight_at(poke_ts), ts_ms: poke_ts },
        "the advanced weight lands in the pool's own observation PDA"
    );
    assert_eq!(
        state.get_account_by_id(weight_obs).program_owner,
        lbp(),
        "the observation PDA is claimed by the LBP program on first poke"
    );

    // The proof predates the poke and is still good.
    state
        .transition_from_privacy_preserving_transaction(&tx, 3, 600_000)
        .expect("a disposable buy proved before a poke must still be includable after it");
    assert_eq!(bal(&state, i.token_vault), RESERVE_TOKEN - tokens_out);
}
