// End-to-end integration tests for the bonding-curve program against an
// in-process LEZ state machine (`V03State`). Two buy paths are covered, and
// they are genuinely different kinds of transaction:
//
//   * the PUBLIC `Buy`, applied with `transition_from_public_transaction`;
//   * the PRIVATE `BuyDisposable`, applied with
//     `transition_from_privacy_preserving_transaction` - a real proof of the
//     committed guest ELF inside the LEZ privacy circuit, in which the buyer's
//     collateral holding and token holding are PRIVATE account slots.
//
// The `BuyDisposable` tests are the machine-checkable form of the security
// claim, so they come in two halves. The positive ones say the trade really
// happened and left nothing of the buyer public. The negative ones each fail
// for one NAMED reason - out of validity window, stale proof, wrong arity -
// because "some error" would pass just as happily if the instruction had
// reverted for the wrong reason.
//
// Run with `RISC0_DEV_MODE=1`; the disposable tests turn it on themselves, see
// [`force_dev_mode`].

use std::collections::HashMap;

use bonding_curve_core::{
    buy_tokens_out, compute_collateral_vault_pda, compute_creator_index_pda, compute_sale_pda,
    compute_token_vault_pda, CreatorIndex, Instruction, SaleState, SaleStatus, CLOCK_01,
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
/// A real NonFungible token definition account - an NFT collection.
///
/// F27: naming one of these as a sale's collateral used to be accepted, and the
/// vault-init leg is a chained `token::InitializeAccount`, which types the
/// holding it creates FROM the definition it is handed - so the collateral vault
/// came out an `NftPrintedCopy`. Every balance this program reads goes through
/// `read_fungible`, so every buy died there, and `Withdraw` died on the
/// collateral vault before it ever reached the token vault: the creator's whole
/// `D + R` deposit stranded for good, by one wrong argument.
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

/// A collateral holding whose DATA is impeccable - the sale's own collateral
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
/// nothing while the sale's post state commits anyway. No such guest can be
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
    /// The creator's `CreatorIndex` PDA. Derived from the creator and NOTHING
    /// else - deliberately not from the nonce, the definitions or the sale - so
    /// every sale this creator ever makes appends to this one account. That is
    /// what turns listing a wallet's sales into a single read.
    creator_index: AccountId,
}
fn ids(creator: AccountId) -> Ids {
    ids_at_nonce(creator, NONCE)
}
/// The same fixture at a chosen sale nonce, for the tests that need a creator's
/// SECOND sale. Every PDA here moves with the nonce except `creator_index`,
/// which is exactly the asymmetry the append test is about.
fn ids_at_nonce(creator: AccountId, nonce: u64) -> Ids {
    let token_def = AccountId::new([7; 32]);
    let collateral_def = AccountId::new([8; 32]);
    let treasury = AccountId::new([9; 32]);
    let sale = compute_sale_pda(bc(), token_def, collateral_def, creator, nonce);
    Ids {
        token_def,
        collateral_def,
        treasury,
        creator,
        sale,
        token_vault: compute_token_vault_pda(bc(), sale),
        collateral_vault: compute_collateral_vault_pda(bc(), sale),
        creator_index: compute_creator_index_pda(bc(), creator),
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
        // Nothing owed to the treasury until a `BuyDisposable` escrows a fee in
        // the collateral vault; the public buy path sweeps its fee immediately.
        treasury_owed: 0,
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

/// Seed an open sale: sale state (owned by BC), token vault (holding D+R), the
/// collateral vault (initialized empty, exactly as `CreateSale` leaves it), and
/// an empty treasury collateral holding.
///
/// The collateral vault is part of the fixture rather than something the first
/// buy claims on the way past, because the buy paths take their chained-call
/// program FROM it: dispatching on the submitter-supplied payer instead is the
/// no-op-program hole that
/// `buy_rejects_a_collateral_holding_owned_by_another_program` covers, and an
/// unclaimed vault has no owner to anchor to.
fn seed_open_sale(state: &mut V03State, i: &Ids) {
    let sale_acc = Account {
        program_owner: bc(),
        data: Data::from(&open_sale(i, false)),
        ..Default::default()
    };
    state.force_insert_account(i.sale, sale_acc);
    state.force_insert_account(i.token_vault, fungible(i.token_def, D + R));
    state.force_insert_account(i.collateral_vault, fungible(i.collateral_def, 0));
    state.force_insert_account(i.treasury, fungible(i.collateral_def, 0));
}

/// Everything a `CreateSale` needs on chain before it can succeed: the creator's
/// project-token holding, their identity account, both definition ACCOUNTS (each
/// deposit leg is dispatched to the program that owns its own definition, so an
/// id naming nothing on chain is not enough), and - new in this release - the
/// TREASURY, which must already be an initialised Fungible holding of the
/// collateral definition.
///
/// The treasury is seeded here rather than per-test on purpose: an honest create
/// now has one, so a test that gets a rejection is rejecting a shape it
/// deliberately broke, not an omission it forgot to fix.
fn seed_create_fixture(state: &mut V03State, i: &Ids, creator_token_id: AccountId) {
    state.force_insert_account(creator_token_id, fungible(i.token_def, D + R));
    state.force_insert_account(i.creator, fungible(i.collateral_def, 0));
    state.force_insert_account(i.token_def, token_def_account("Project"));
    state.force_insert_account(i.collateral_def, token_def_account("Collateral"));
    state.force_insert_account(i.treasury, fungible(i.collateral_def, 0));
}

/// The parts of a `CreateSale` any test here varies, and nothing else.
///
/// The instruction carries fifteen fields over nine accounts, and spelling all
/// of that out at every call site is how the treasury came to sit in `SaleState`
/// for a whole release without a single test asking what account its id named.
/// [`CreateArgs::standard`] is the honest create; each test mutates exactly the
/// field it is about, so a rejection can only be blamed on that field.
struct CreateArgs {
    /// Slot 6 - the creator's project-token holding, and a signer, so its id is
    /// whatever its key derives.
    creator_token_holding: AccountId,
    /// Slot 4 - the project token's definition account.
    token_definition: AccountId,
    /// Slot 3 - the ACCOUNT handed over as the treasury.
    treasury: AccountId,
    /// The `treasury_id` instruction ARGUMENT. Deliberately kept apart from the
    /// slot above: the id is what lands in `SaleState` and what every later buy,
    /// sell and sweep validates the account it is handed against, while the slot
    /// is the only thing `create_sale` can type-check. The assert binding the
    /// two is only testable if a test can make them disagree.
    treasury_id: AccountId,
    /// `0` means "no end timestamp"; the escrow tests need a sale that can be
    /// closed on a clock rather than after the 7-day recovery floor.
    end_timestamp_ms: u64,
    /// Slot 8 - the account handed over as the creator's sale index. Varied only
    /// by the test that checks the program pins this slot to the PDA of the
    /// creator who SIGNED: the submitter picks every non-signer id, so without
    /// that pin a create could append its sale into a stranger's listing.
    creator_index: AccountId,
    /// The sale `nonce` instruction argument. It must agree with the
    /// [`ids_at_nonce`] the message's PDAs came from, or `create_sale` rejects
    /// the sale slot before it reaches anything this suite is testing.
    nonce: u64,
    /// The nonce BOTH signers sign at. A rejected transaction advances no nonce,
    /// so the failure tests all leave this at 0; only a test that does two
    /// successful creates in a row has to raise it.
    signer_nonce: u128,
}

impl CreateArgs {
    /// Every slot the account it should be: the create [`seed_create_fixture`]
    /// is expected to accept.
    fn standard(i: &Ids, creator_token_holding: AccountId) -> Self {
        Self {
            creator_token_holding,
            token_definition: i.token_def,
            treasury: i.treasury,
            treasury_id: i.treasury,
            end_timestamp_ms: 0,
            creator_index: i.creator_index,
            nonce: NONCE,
            signer_nonce: 0,
        }
    }
}

/// A `CreateSale` message over this suite's standard sale parameters.
///
/// The account order is the program's. The treasury is at slot 3, directly after
/// the collateral vault - exactly where `Buy`, `Sell`, `BuyAta` and `SellAta`
/// keep it - and is read-only here, echoed back unchanged. It is in the list at
/// all so `create_sale` can type-check it, which is what makes the DEFAULT-owned
/// branch of `sweep_treasury` unreachable instead of merely unlikely.
///
/// The creator's sale index is at slot 8, between the creator it is derived from
/// and the clock, which `echo_clock` requires to stay last. It is the only
/// account in this list the program CLAIMS on a first sale and merely rewrites
/// afterwards.
///
/// Two signers: the creator's token holding (the deposit's sender) and the
/// creator's identity. A rejected transaction advances no nonce, so every test
/// here signs at `CreateArgs::signer_nonce` == 0 no matter how many creates it
/// has already had refused.
fn create_msg(i: &Ids, a: &CreateArgs) -> public_transaction::Message {
    public_transaction::Message::try_new(
        bc(),
        vec![
            i.sale,
            i.token_vault,
            i.collateral_vault,
            a.treasury,
            a.token_definition,
            i.collateral_def,
            a.creator_token_holding,
            i.creator,
            a.creator_index,
            CLOCK_01,
        ],
        vec![Nonce(a.signer_nonce), Nonce(a.signer_nonce)],
        Instruction::CreateSale {
            collateral_definition_id: i.collateral_def,
            treasury_id: a.treasury_id,
            token_name: String::new(),
            token_symbol: String::new(),
            sale_quantity: D,
            dex_seed_quantity: R,
            virt_token: VT,
            virt_collateral: VC,
            fee_bps: FEE_BPS,
            one_directional: false,
            end_timestamp_ms: a.end_timestamp_ms,
            min_duration_ms: 0,
            nonce: a.nonce,
            ata_program_id: programs::ata().id(),
            deadline: u64::MAX,
        },
    )
    .expect("message")
}

#[test]
fn create_sale_deposits_and_initializes_state() {
    let creator_token_key = PrivateKey::try_new([41; 32]).expect("key");
    let creator_key = PrivateKey::try_new([42; 32]).expect("key");
    let creator_token_id = id_of(&creator_token_key);
    let i = ids(id_of(&creator_key));

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_create_fixture(&mut state, &i, creator_token_id);

    let message = create_msg(&i, &CreateArgs::standard(&i, creator_token_id));
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
    // paths will later dispatch against - `check_buyer_collateral` compares every
    // buyer's holding to `collateral_vault.program_owner`, so a vault claimed by
    // anything else rejects every honest buyer.
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
/// doing a create and then the sale's FIRST buy in one state with nothing
/// hand-seeded.
///
/// Every other buy test starts from [`seed_open_sale`], which
/// `force_insert_account`s the collateral vault - so those tests CHOOSE the
/// `program_owner` that `check_buyer_collateral` compares the buyer's holding
/// against, and they agree with themselves by construction. The create tests, in
/// turn, asserted balances but never ownership. Between them, a change to
/// `create_sale`'s vault-init leg - dispatching it somewhere else, dropping the
/// `InitializeAccount`, claiming the PDA under a different program - would break
/// every real first buy on chain while the whole suite stayed green. That gap is
/// what this test closes, and it is the only test here whose vaults come from
/// the program rather than from the fixture.
///
/// Both vault owners are asserted explicitly, because that field is the anchor
/// the buy paths dispatch their chained `token::Transfer`s to (see
/// `buy::buy_transfers`): if a vault came out owned by anything other than the
/// real token program, the deposit leg would be routed to that program instead.
#[test]
fn first_buy_after_a_real_create_sale_succeeds() {
    let creator_token_key = PrivateKey::try_new([41; 32]).expect("key");
    let creator_key = PrivateKey::try_new([42; 32]).expect("key");
    let creator_token_id = id_of(&creator_token_key);
    let i = ids(id_of(&creator_key));

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_create_fixture(&mut state, &i, creator_token_id);

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

    let create = create_msg(&i, &CreateArgs::standard(&i, creator_token_id));
    let w =
        public_transaction::WitnessSet::for_message(&create, &[&creator_token_key, &creator_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(create, w), 0, 0)
        .expect("create_sale must succeed");

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
         check_buyer_collateral makes every buyer's holding match"
    );
    assert_eq!(bal(&state, i.token_vault), D + R, "the deposit landed in the token vault");
    assert_eq!(
        bal(&state, i.collateral_vault),
        0,
        "the collateral vault is initialized empty and typed to the collateral definition"
    );

    // ...and now the first buy against exactly that state. A fresh buyer, so its
    // nonce is 0 even though the creator's accounts have moved on.
    let buyer_key = PrivateKey::try_new([60; 32]).unwrap();
    let buyer_coll = id_of(&buyer_key);
    let buyer_tok = AccountId::new([61; 32]);
    let buyer_start: u128 = 100_000;
    let collateral_in: u128 = 5_000;
    state.force_insert_account(buyer_coll, fungible(i.collateral_def, buyer_start));
    state.force_insert_account(buyer_tok, fungible(i.token_def, 0));

    let (tokens_out, fee, c_eff) = buy_tokens_out(VT, VC, FEE_BPS, collateral_in);
    assert!(tokens_out > 0 && fee > 0, "the buy must be one that really moves the sale");

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
        Instruction::Buy { collateral_in, min_tokens_out: 0, deadline: u64::MAX },
    )
    .unwrap();
    let w = public_transaction::WitnessSet::for_message(&buy, &[&buyer_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(buy, w), 1, 0)
        .expect(
            "the first buy against a sale this program just created must succeed - if it does \
             not, create_sale is producing a vault the buy guard rejects, which is a chain \
             break no fixture-seeded test can see",
        );

    assert_eq!(bal(&state, buyer_tok), tokens_out, "the buyer really received tokens");
    assert_eq!(bal(&state, buyer_coll), buyer_start - collateral_in, "the buyer really paid");
    assert_eq!(bal(&state, i.collateral_vault), c_eff, "collateral landed in the created vault");
    assert_eq!(bal(&state, i.treasury), fee, "the fee was swept in the same transaction");
    assert_eq!(bal(&state, i.token_vault), D + R - tokens_out);
    let sale = SaleState::try_from(&state.get_account_by_id(i.sale).data).unwrap();
    assert_eq!(sale.sale_reserve, D - tokens_out);
    assert_eq!(sale.real_collateral, c_eff);
    assert_eq!(sale.treasury_owed, 0, "a public buy escrows nothing: it pays the treasury now");
    assert_eq!(sale.buy_count, 1);
}

// --- the per-creator sale index ---------------------------------------------
//
// WHAT THIS IS FOR. Nothing on chain used to record what a wallet had created,
// so `lpad my-sales` re-derived every PDA the wallet could conceivably have
// made - a product over (public account x token definition x collateral
// definition x nonce), roughly 4,800 sequential reads, measured at over half an
// hour on a live wallet with history. `CreateSale` now writes one account per
// creator holding the ids it has made, and discovery is one read.
//
// The guest's own tests (`bonding_curve_program`) pin its branches. These pin
// the parts only a real state machine can show: that the account lands in LEZ
// state at the id the SDK derives, carrying bytes the SDK decodes, and that it
// keeps accumulating across a creator's whole history.

/// Read a creator's index the way the SDK will - derive, read, decode - and say
/// which of the three steps failed.
///
/// Split out because "there is no index account", "it is owned by somebody else"
/// and "it does not decode" are three different bugs, and a helper returning
/// `Option` would let a test that meant to prove discovery works pass while
/// discovery returned nothing.
fn creator_index(state: &V03State, creator: AccountId) -> CreatorIndex {
    let id = compute_creator_index_pda(bc(), creator);
    let account = state.get_account_by_id(id);
    assert_ne!(
        account,
        Account::default(),
        "no index account exists at the creator's index PDA - discovery would find nothing"
    );
    assert_eq!(
        account.program_owner,
        bc(),
        "the index account must be owned by the bonding-curve program: it holds this \
         program's own data, and only this program may append to it"
    );
    CreatorIndex::try_from(&account.data)
        .expect("the index account does not decode as a CreatorIndex")
}

/// A create writes the creator's index, and the index names the sale.
///
/// The id is recomputed here from the program id and the creator alone -
/// `compute_creator_index_pda(bc(), creator)`, the exact call the SDK makes -
/// rather than read back out of the transaction, so the test fails if the
/// program stores the index anywhere the SDK would not think to look.
#[test]
fn create_sale_records_the_new_sale_in_the_creators_index() {
    let creator_token_key = PrivateKey::try_new([41; 32]).expect("key");
    let creator_key = PrivateKey::try_new([42; 32]).expect("key");
    let creator_token_id = id_of(&creator_token_key);
    let i = ids(id_of(&creator_key));

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_create_fixture(&mut state, &i, creator_token_id);
    // Mechanical proof the fixture does not pre-seed the thing under test: the
    // index has to be CLAIMED by the create, not found lying around.
    assert_eq!(
        state.get_account_by_id(i.creator_index),
        Account::default(),
        "the creator index must not be pre-seeded: the first CreateSale is what claims it"
    );

    let m = create_msg(&i, &CreateArgs::standard(&i, creator_token_id));
    let w = public_transaction::WitnessSet::for_message(&m, &[&creator_token_key, &creator_key]);
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
        index.sale_ids,
        vec![i.sale],
        "the index must name the sale that was just created, and nothing else"
    );
    // IDS ONLY, never state. Sale state (reserves, status) changes on every buy;
    // an index carrying any of it would be stale the moment it was written, and
    // the SDK would show numbers that disagree with the sale account. 78 bytes is
    // exactly magic(8) + version(2) + creator(32) + vec len(4) + one id(32).
    assert_eq!(
        state.get_account_by_id(i.creator_index).data.as_ref().len(),
        8 + 2 + 32 + 4 + 32,
        "the index must hold ids and nothing else - no reserve, no status, nothing that goes \
         stale"
    );
}

/// A creator's SECOND sale APPENDS. Two creates, two sales, one index holding
/// both ids oldest-first.
///
/// This is the test that fails if `create_sale` ever writes the index instead of
/// extending it - which is the natural shape of the bug, since the first-sale
/// branch does exactly that. An overwrite would leave a wallet's listing showing
/// only its most recent sale while every earlier one silently vanished, and the
/// single-create test above would stay green.
///
/// The two sales differ only in `nonce`: the sale PDA and both vault PDAs move
/// with it, and the index PDA - keyed on the creator alone - does not.
#[test]
fn a_second_sale_appends_to_the_creators_index_rather_than_replacing_it() {
    let creator_token_key = PrivateKey::try_new([41; 32]).expect("key");
    let creator_key = PrivateKey::try_new([42; 32]).expect("key");
    let creator_token_id = id_of(&creator_token_key);
    let first = ids_at_nonce(id_of(&creator_key), NONCE);
    let second = ids_at_nonce(id_of(&creator_key), NONCE + 1);
    assert_ne!(first.sale, second.sale, "two nonces, two sale PDAs");
    assert_eq!(
        first.creator_index, second.creator_index,
        "...and ONE index PDA: it is derived from the creator, not from the sale"
    );

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_create_fixture(&mut state, &first, creator_token_id);
    // Enough project tokens to fund both deposits out of one holding.
    state.force_insert_account(creator_token_id, fungible(first.token_def, 2 * (D + R)));

    let m = create_msg(&first, &CreateArgs::standard(&first, creator_token_id));
    let w = public_transaction::WitnessSet::for_message(&m, &[&creator_token_key, &creator_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, w), 0, 0)
        .expect("the first create must succeed");

    // Both signers advanced to 1 on the create above; a second create at 0 would
    // be rejected for the nonce and prove nothing about the index.
    let m = create_msg(
        &second,
        &CreateArgs {
            nonce: NONCE + 1,
            signer_nonce: 1,
            ..CreateArgs::standard(&second, creator_token_id)
        },
    );
    let w = public_transaction::WitnessSet::for_message(&m, &[&creator_token_key, &creator_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, w), 1, 0)
        .expect("the creator's second create must succeed");

    let index = creator_index(&state, first.creator);
    assert_eq!(
        index.sale_ids,
        vec![first.sale, second.sale],
        "both sales must be listed, oldest first - an append, not an overwrite"
    );
    // The append must not have disturbed the earlier sale itself. It is a
    // separate account, but the index write is the only part of a second create
    // that touches anything the first one produced.
    assert_eq!(
        state.get_account_by_id(first.sale).program_owner,
        bc(),
        "the first sale account must survive its creator's second create"
    );
    assert_eq!(
        SaleState::try_from(&state.get_account_by_id(first.sale).data)
            .expect("the first sale must still decode")
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
/// pin a stranger could point slot 8 at somebody else's index and have their own
/// sale appended to that wallet's listing. `my-sales` would then show a creator
/// sales they never made, at ids they do not control, which is a lie the CLI has
/// no way to detect: the sale accounts named are perfectly real.
///
/// Both shapes get the same named rejection, and - the part that matters - the
/// victim's index is still not there afterwards.
#[test]
fn create_sale_rejects_a_creator_index_that_is_not_the_signing_creators() {
    let creator_token_key = PrivateKey::try_new([41; 32]).expect("key");
    let creator_key = PrivateKey::try_new([42; 32]).expect("key");
    let creator_token_id = id_of(&creator_token_key);
    let i = ids(id_of(&creator_key));
    let victim = id_of(&PrivateKey::try_new([77; 32]).expect("key"));
    let victim_index = compute_creator_index_pda(bc(), victim);
    assert_ne!(victim_index, i.creator_index, "the victim is a different creator");

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_create_fixture(&mut state, &i, creator_token_id);

    let cases: [(&str, AccountId); 2] = [
        ("another wallet's index PDA - the listing-pollution attack", victim_index),
        ("an id that is no index PDA at all", AccountId::new([123; 32])),
    ];
    for (case, creator_index_slot) in cases {
        let m = create_msg(
            &i,
            &CreateArgs {
                creator_index: creator_index_slot,
                ..CreateArgs::standard(&i, creator_token_id)
            },
        );
        let w =
            public_transaction::WitnessSet::for_message(&m, &[&creator_token_key, &creator_key]);
        let result = state.transition_from_public_transaction(&PublicTransaction::new(m, w), 0, 0);
        let Err(LeeError::ProgramExecutionFailed(err)) = result else {
            panic!("a create whose index slot is {case} must revert, got {result:?}");
        };
        assert!(
            err.contains("creator index account is not the index PDA of the signing creator"),
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
    let m = create_msg(&i, &CreateArgs::standard(&i, creator_token_id));
    let w = public_transaction::WitnessSet::for_message(&m, &[&creator_token_key, &creator_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, w), 0, 0)
        .expect("the same create with the creator's own index PDA must succeed");
    assert_eq!(creator_index(&state, i.creator).sale_ids, vec![i.sale]);
}

/// SECURITY: an index account that exists but belongs to another program is
/// refused by name, rather than being parsed as if it were ours.
///
/// Defence in depth - the claim rules make this unreachable in practice, since
/// nothing but this program can put data at this PDA. It is tested because that
/// argument is about LEZ's claim rules, not about this program: if a later
/// release delegates this seed to a chained call, or the seed changes, the guard
/// is what stops a foreign account's bytes being decoded as a sale list.
#[test]
fn create_sale_rejects_a_creator_index_owned_by_another_program() {
    let creator_token_key = PrivateKey::try_new([41; 32]).expect("key");
    let creator_key = PrivateKey::try_new([42; 32]).expect("key");
    let creator_token_id = id_of(&creator_token_key);
    let i = ids(id_of(&creator_key));

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_create_fixture(&mut state, &i, creator_token_id);
    // The right id, the wrong owner: a token holding sitting where the index goes.
    state.force_insert_account(i.creator_index, fungible(i.collateral_def, 0));

    let m = create_msg(&i, &CreateArgs::standard(&i, creator_token_id));
    let w = public_transaction::WitnessSet::for_message(&m, &[&creator_token_key, &creator_key]);
    let result = state.transition_from_public_transaction(&PublicTransaction::new(m, w), 0, 0);
    let Err(LeeError::ProgramExecutionFailed(err)) = result else {
        panic!("a create over a foreign-owned index account must revert, got {result:?}");
    };
    assert!(
        err.contains("creator index account exists but is not owned by this program"),
        "the rejection must name the ownership guard, got: {err}"
    );
}

/// Closing a sale leaves the creator's index alone, entry included.
///
/// `CloseSale` is `[sale, creator, clock]` - the index is not in its account list
/// at all, which is the whole reason this holds. The test is here because the
/// tempting "tidy up" change is to drop closed sales from the listing, and that
/// would be wrong twice over: a creator's history is what `my-sales` is for, and
/// removals would make the index's positions unstable for anything that cached
/// them. Status is resolved from the sale account, never from the index.
#[test]
fn closing_a_sale_leaves_the_creators_index_intact() {
    let creator_token_key = PrivateKey::try_new([41; 32]).expect("key");
    let creator_key = PrivateKey::try_new([42; 32]).expect("key");
    let creator_token_id = id_of(&creator_token_key);
    let creator = id_of(&creator_key);
    let i = ids(creator);
    let end_ms: u64 = 60_000;

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_create_fixture(&mut state, &i, creator_token_id);

    let m = create_msg(
        &i,
        &CreateArgs { end_timestamp_ms: end_ms, ..CreateArgs::standard(&i, creator_token_id) },
    );
    let w = public_transaction::WitnessSet::for_message(&m, &[&creator_token_key, &creator_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, w), 0, 0)
        .expect("create_sale must succeed");
    let before = state.get_account_by_id(i.creator_index);

    // Close at the sale's end timestamp. The creator signed the create, so their
    // identity account is at nonce 1.
    set_clock(&mut state, 1, i64::try_from(end_ms).expect("end_ms fits"));
    let close = public_transaction::Message::try_new(
        bc(),
        vec![i.sale, creator, CLOCK_01],
        vec![Nonce(1)],
        Instruction::CloseSale { deadline: u64::MAX },
    )
    .expect("message");
    let w = public_transaction::WitnessSet::for_message(&close, &[&creator_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(close, w), 1, end_ms)
        .expect("the creator must be able to close at the end timestamp");

    assert!(
        matches!(
            SaleState::try_from(&state.get_account_by_id(i.sale).data).expect("sale").status,
            SaleStatus::Closed
        ),
        "the sale really closed - otherwise this test proves nothing about closing"
    );
    assert_eq!(
        state.get_account_by_id(i.creator_index),
        before,
        "CloseSale must not touch the creator's index account in any way"
    );
    assert_eq!(
        creator_index(&state, creator).sale_ids,
        vec![i.sale],
        "a closed sale stays listed: the index is a creator's history, and status is read \
         from the sale account"
    );
}

/// `CreateSale` rejects a `treasury_id` that aliases the creator's identity
/// account - the fund-lock this pair of launchpads shipped with, and the one a
/// creator is most likely to type by hand ("send the fees to me").
///
/// It is not merely odd, it is unrecoverable. `SweepTreasury` is
/// `[sale, collateral_vault, treasury]` and `Withdraw` names the creator, and
/// LEZ rejects a message with a repeated account id BEFORE the program runs, so
/// with the treasury pointed at any of those there is no account list left that
/// can move the escrow.
///
/// `CreateSale` takes the treasury as an ACCOUNT now, which is why the rejected
/// create below declares the CREATOR as its `treasury_id` while still passing a
/// perfectly good treasury holding at slot 3. That is the shape that reaches the
/// program at all: naming the creator in both places puts one id in the list
/// twice, and LEZ refuses that as `Duplicate account_ids` before this program
/// runs - a true answer that names neither slot. The alias rule deliberately
/// runs ahead of the type pin so what comes back names the id to change instead
/// of complaining about the account that is fine.
///
/// The control at the end is the point of the test as much as the rejection is:
/// the identical create, with only `treasury_id` repaired, must go through.
/// Without it this would still pass if the fixture were broken for some reason
/// that had nothing to do with the treasury.
#[test]
fn create_sale_rejects_a_treasury_that_aliases_the_creator() {
    let creator_token_key = PrivateKey::try_new([41; 32]).expect("key");
    let creator_key = PrivateKey::try_new([42; 32]).expect("key");
    let creator_token_id = id_of(&creator_token_key);
    let i = ids(id_of(&creator_key));

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_create_fixture(&mut state, &i, creator_token_id);

    // The same message twice, differing only in `treasury_id`. A rejected
    // transaction advances no nonce, so both sign at 0.
    let message = |treasury_id: AccountId| {
        create_msg(&i, &CreateArgs { treasury_id, ..CreateArgs::standard(&i, creator_token_id) })
    };

    let m = message(i.creator);
    let w = public_transaction::WitnessSet::for_message(
        &m,
        &[&creator_token_key, &creator_key],
    );
    let result = state.transition_from_public_transaction(&PublicTransaction::new(m, w), 0, 0);
    let Err(LeeError::ProgramExecutionFailed(err)) = result else {
        panic!("a create naming the creator as its treasury must revert, got {result:?}");
    };
    assert!(
        err.contains("treasury must not alias the sale account, its token or collateral vault"),
        "it must revert on the treasury-alias guard and not for some unrelated reason - a test \
         that passes on any error would pass on the fund-locking build too: {err}"
    );

    // Nothing was created, so there is no half-built sale to clean up.
    assert_eq!(
        state.get_account_by_id(i.sale),
        Account::default(),
        "the sale PDA must not exist: a rejected create is not a create"
    );
    assert_eq!(bal(&state, creator_token_id), D + R, "the creator's deposit was not taken");

    // Control: repair only the treasury and the identical create succeeds.
    let m = message(i.treasury);
    let w = public_transaction::WitnessSet::for_message(
        &m,
        &[&creator_token_key, &creator_key],
    );
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, w), 0, 0)
        .expect("the identical create must succeed once the treasury is a separate account");
    let sale = SaleState::try_from(&state.get_account_by_id(i.sale).data).expect("sale");
    assert_eq!(sale.treasury_id, i.treasury, "control: the sale really was created");
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
/// over a holding owned by a foreign program.
#[test]
fn create_sale_rejects_a_token_definition_account_it_did_not_declare() {
    let creator_token_key = PrivateKey::try_new([41; 32]).expect("key");
    let creator_key = PrivateKey::try_new([42; 32]).expect("key");
    let creator_token_id = id_of(&creator_token_key);
    let i = ids(id_of(&creator_key));

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_create_fixture(&mut state, &i, creator_token_id);
    // A perfectly real definition account that simply is not this sale's. The
    // attack does not need it to be malformed - it needs the program to take its
    // `program_owner` as the deposit's dispatch target.
    let decoy_def = AccountId::new([12; 32]);
    state.force_insert_account(decoy_def, token_def_account("Decoy"));

    // The same message twice, differing only in the declared definition account.
    // A rejected transaction advances no nonce, so both sign at 0.
    let message = |token_definition: AccountId| {
        create_msg(
            &i,
            &CreateArgs { token_definition, ..CreateArgs::standard(&i, creator_token_id) },
        )
    };

    let m = message(decoy_def);
    let w =
        public_transaction::WitnessSet::for_message(&m, &[&creator_token_key, &creator_key]);
    let result = state.transition_from_public_transaction(&PublicTransaction::new(m, w), 0, 0);
    let Err(LeeError::ProgramExecutionFailed(err)) = result else {
        panic!("a create declaring a definition account the holding does not name must revert, got {result:?}");
    };
    assert!(
        err.contains("token definition account does not match the definition id the creator's holding declares"),
        "it must revert on the definition pin and not for some unrelated reason - a test that \
         passes on any error would pass on the unpinned build too: {err}"
    );
    assert_eq!(
        state.get_account_by_id(i.sale),
        Account::default(),
        "the sale PDA must not exist: a rejected create is not a create"
    );
    assert_eq!(bal(&state, creator_token_id), D + R, "the deposit was not taken");

    // Control: repair only the definition slot and the identical create succeeds.
    let m = message(i.token_def);
    let w =
        public_transaction::WitnessSet::for_message(&m, &[&creator_token_key, &creator_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, w), 0, 0)
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
/// as `UnauthorizedDataModification` (rule 6) naming no cause - and the pair is
/// what makes the deposit's dispatch target unforgeable. See
/// `create_sale_rejects_a_token_definition_account_it_did_not_declare` for the
/// attack the pair defeats.
#[test]
fn create_sale_rejects_a_creator_holding_under_a_foreign_token_program() {
    let creator_token_key = PrivateKey::try_new([41; 32]).expect("key");
    let creator_key = PrivateKey::try_new([42; 32]).expect("key");
    let creator_token_id = id_of(&creator_token_key);
    let i = ids(id_of(&creator_key));

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_create_fixture(&mut state, &i, creator_token_id);
    // The one thing this test breaks: impeccable data - the sale's own project
    // definition, a balance that covers the deposit - under a program that is
    // not the token program.
    state.force_insert_account(
        creator_token_id,
        substituted_holding(i.token_def, D + R, substituted_program()),
    );

    let m = create_msg(&i, &CreateArgs::standard(&i, creator_token_id));
    let w =
        public_transaction::WitnessSet::for_message(&m, &[&creator_token_key, &creator_key]);
    let result = state.transition_from_public_transaction(&PublicTransaction::new(m, w), 0, 0);
    let Err(LeeError::ProgramExecutionFailed(err)) = result else {
        panic!("a create depositing from a foreign-owned holding must revert, got {result:?}");
    };
    assert!(
        err.contains(
            "creator token holding is not owned by the project token definition's own token \
             program"
        ),
        "it must revert on the holding/definition owner bind, with the message that tells the \
         creator which account is wrong: {err}"
    );
    assert_eq!(
        state.get_account_by_id(i.sale),
        Account::default(),
        "the sale PDA must not exist: a rejected create is not a create"
    );
    assert_eq!(
        state.get_account_by_id(i.token_vault),
        Account::default(),
        "and the token vault was never claimed - which is the whole point of the guard"
    );
}

/// Every treasury shape that could never receive the fee, refused at creation -
/// and, at the end, the control that says this fixture would otherwise have
/// opened the sale.
///
/// `SweepTreasury` is the ONLY instruction that pays `treasury_owed` out, its
/// fee leg is a `token::Transfer` dispatched on the collateral vault's own token
/// program, and nothing anywhere rewrites `treasury_id`. So a treasury that
/// cannot receive that transfer does not fail once, it fails FOREVER, and the
/// collateral behind the escrow stays in the vault for good. Creation is the
/// last moment fixing it costs the creator nothing.
///
/// The uninitialised shapes are the ones a STRANGER could weaponise. Settling
/// into an account that does not exist has to CLAIM it, and LEZ claims an
/// account only when its pre-state is `Account::default()` WHOLE - so anyone
/// could send that id a dust native balance (or get its key to sign anything,
/// which bumps the nonce) and the only instruction that can ever pay this escrow
/// out would fail forever, on a sale they had nothing to do with. Requiring an
/// ALREADY-initialised holding takes that away permanently rather than probably:
/// an initialised holding is owned by the token program, and LEZ has no
/// instruction that un-initialises one (`burn` only lowers a balance), so the
/// state cannot regress between creation and the last sweep.
///
/// One test rather than five because the cases share a fixture and the control
/// is what makes each rejection mean anything: the same create, the same
/// everything, differing only in the account at slot 3.
#[test]
fn create_sale_rejects_every_treasury_that_could_never_be_paid() {
    let creator_token_key = PrivateKey::try_new([41; 32]).expect("key");
    let creator_key = PrivateKey::try_new([42; 32]).expect("key");
    let creator_token_id = id_of(&creator_token_key);
    let i = ids(id_of(&creator_key));

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_create_fixture(&mut state, &i, creator_token_id);

    // (what the treasury is, the assert that must name it). Each case replaces
    // ONLY the account at slot 3; the id it is declared under never changes.
    let cases: [(&str, Account, &str); 5] = [
        (
            "never initialised - what a creator gets by naming a fresh key as their fee sink",
            Account::default(),
            "the sale's treasury account does not exist yet",
        ),
        (
            "uninitialised AND already dusted: the F26 attack state, now refused before the \
             sale exists instead of at the sweep that could never happen",
            Account { balance: 1, ..Account::default() },
            // Past the exists-at-all assert (a dusted account is not default),
            // so this one is caught by the parse. Named exactly, because a
            // change that made it fall through to a claim would still panic
            // somewhere and a test matching on "any error" would not notice.
            "CreateSale: treasury: expected a valid Token Holding account",
        ),
        (
            "an NFT holding - it parses, and it still cannot hold a divisible fee",
            nft_holding(i.collateral_def),
            "CreateSale: treasury: expected a Fungible Token Holding account",
        ),
        (
            "a holding of the PROJECT token: the fee leg moves collateral, so the transfer \
             reverts on the definition mismatch every single time",
            fungible(i.token_def, 0),
            "the sale's treasury holds a different token than the sale's collateral",
        ),
        (
            "the right definition under a token program the creator deployed themselves - \
             permissionless deployment makes this shape cheap to produce",
            substituted_holding(i.collateral_def, 0, substituted_program()),
            "the sale's treasury is not owned by the collateral definition's own token program",
        ),
    ];

    for (case, treasury, expected) in cases {
        state.force_insert_account(i.treasury, treasury);
        let m = create_msg(&i, &CreateArgs::standard(&i, creator_token_id));
        let w =
            public_transaction::WitnessSet::for_message(&m, &[&creator_token_key, &creator_key]);
        let result = state.transition_from_public_transaction(&PublicTransaction::new(m, w), 0, 0);
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
            state.get_account_by_id(i.sale),
            Account::default(),
            "the sale PDA must not exist: a rejected create is not a create ({case})"
        );
        assert_eq!(
            bal(&state, creator_token_id),
            D + R,
            "and the creator's D + R deposit was not taken ({case})"
        );
    }

    // Control: put a usable treasury back and the identical create goes through.
    // Without it the loop above would still pass if the fixture had rotted in
    // some way that had nothing to do with the treasury.
    state.force_insert_account(i.treasury, fungible(i.collateral_def, 0));
    let m = create_msg(&i, &CreateArgs::standard(&i, creator_token_id));
    let w = public_transaction::WitnessSet::for_message(&m, &[&creator_token_key, &creator_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, w), 0, 0)
        .expect("the identical create must succeed against an initialised treasury");
    let sale = SaleState::try_from(&state.get_account_by_id(i.sale).data).expect("sale");
    assert_eq!(sale.treasury_id, i.treasury, "control: the sale really was created");
}

/// The treasury ACCOUNT and the treasury ID must be the same account.
///
/// The id is what lands in `SaleState` and what every later `Buy`, `Sell` and
/// `SweepTreasury` validates the account it is handed against; the account at
/// slot 3 is the only thing `CreateSale` can type-check. Type-check one while
/// pinning the other and the type check secures nothing at all - a creator could
/// hand over a perfectly good treasury to satisfy it and still write an
/// unpayable id into the sale, which is the fund-lock this release closed,
/// reopened by an off-by-one.
///
/// It is also the assert that fires first if this account list ever drifts out
/// of the order the program destructures it in.
#[test]
fn create_sale_rejects_a_treasury_account_that_is_not_the_declared_id() {
    let creator_token_key = PrivateKey::try_new([41; 32]).expect("key");
    let creator_key = PrivateKey::try_new([42; 32]).expect("key");
    let creator_token_id = id_of(&creator_token_key);
    let i = ids(id_of(&creator_key));

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_create_fixture(&mut state, &i, creator_token_id);
    // A second, entirely valid treasury: same definition, same token program,
    // initialised. Nothing is wrong with this account except that it is not the
    // one the instruction declares.
    let other_treasury = AccountId::new([19; 32]);
    state.force_insert_account(other_treasury, fungible(i.collateral_def, 0));

    let m = create_msg(
        &i,
        &CreateArgs { treasury: other_treasury, ..CreateArgs::standard(&i, creator_token_id) },
    );
    let w = public_transaction::WitnessSet::for_message(&m, &[&creator_token_key, &creator_key]);
    let result = state.transition_from_public_transaction(&PublicTransaction::new(m, w), 0, 0);
    let Err(LeeError::ProgramExecutionFailed(err)) = result else {
        panic!("a create whose treasury slot is not its treasury_id must revert, got {result:?}");
    };
    assert!(
        err.contains(
            "the treasury account passed does not match the treasury_id declared in the \
             instruction"
        ),
        "it must revert on the binding assert - a build that type-checks one account and pins \
         another has no treasury guarantee at all: {err}"
    );
    assert_eq!(
        state.get_account_by_id(i.sale),
        Account::default(),
        "the sale PDA must not exist: a rejected create is not a create"
    );
}

/// F27: an NFT collection cannot be a sale's collateral, and cannot be the token
/// it sells.
///
/// Self-inflicted rather than hostile - a creator names the wrong definition -
/// and unrecoverable, which is what makes it worth an assert. The vault-init leg
/// is a chained `token::InitializeAccount`, which types the holding it creates
/// FROM the definition it is handed, so a NonFungible collateral definition
/// yields an `NftPrintedCopy` collateral vault. Every balance this program reads
/// goes through `read_fungible`, so every buy dies there - and `Withdraw` dies
/// on the collateral vault before it ever reaches the token vault, stranding the
/// creator's whole `D + R` deposit for good.
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
    let creator_key = PrivateKey::try_new([42; 32]).expect("key");
    let creator_token_id = id_of(&creator_token_key);
    let i = ids(id_of(&creator_key));

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_create_fixture(&mut state, &i, creator_token_id);

    let create = |state: &mut V03State| {
        let m = create_msg(&i, &CreateArgs::standard(&i, creator_token_id));
        let w =
            public_transaction::WitnessSet::for_message(&m, &[&creator_token_key, &creator_key]);
        state.transition_from_public_transaction(&PublicTransaction::new(m, w), 0, 0)
    };

    // 1. NFT COLLATERAL. The treasury is a Fungible holding naming the NFT
    //    collection - impossible on chain, seeded here so the rejection cannot
    //    be dismissed as "the treasury was missing".
    state.force_insert_account(i.collateral_def, nft_definition("Collateral NFT"));
    let result = create(&mut state);
    let Err(LeeError::ProgramExecutionFailed(err)) = result else {
        panic!("a sale denominated in an NFT collection must revert, got {result:?}");
    };
    assert!(
        err.contains("collateral definition account does not hold a FUNGIBLE token definition"),
        "it must name the collateral DEFINITION, which is the account the creator got wrong: \
         {err}"
    );

    // 2. AN NFT PROJECT TOKEN. Nothing downstream would notice: the deposit leg
    //    is a `Transfer`, which never parses this account.
    state.force_insert_account(i.collateral_def, token_def_account("Collateral"));
    state.force_insert_account(i.token_def, nft_definition("Project NFT"));
    let result = create(&mut state);
    let Err(LeeError::ProgramExecutionFailed(err)) = result else {
        panic!("a sale of an NFT collection must revert, got {result:?}");
    };
    assert!(
        err.contains("project token definition account does not hold a FUNGIBLE token definition"),
        "it must name the project definition: {err}"
    );

    // Control: both definitions Fungible again and the identical create succeeds.
    state.force_insert_account(i.token_def, token_def_account("Project"));
    create(&mut state).expect("the identical create must succeed with two fungible definitions");
    assert_eq!(bal(&state, i.token_vault), D + R, "control: the sale really was created");
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

/// SECURITY regression: a keypair `Buy` paying from a collateral holding that
/// some OTHER program owns must revert, and must leave the sale exactly as it
/// found it.
///
/// The hole this closes: the chained `token::Transfer` legs used to be
/// dispatched to `payer.account.program_owner`, and the payer is an account the
/// submitter hands in. Deployment on LEZ is permissionless, so that account can
/// be one claimed by a no-op program whose `Transfer` echoes its pre-states,
/// carrying data that decodes as a huge collateral holding. Both legs then move
/// nothing - the no-op is not blocked, it simply does nothing, and it could not
/// touch the token-program-owned vaults even if it tried - while the sale's own
/// post state still commits: reserve consumed, `real_collateral` and
/// `virt_collateral` grown, fee accrued. That is a free sale-kill, and on a
/// two-way sale a drain, because the inflated `real_collateral` is what bounds a
/// later `Sell` payout - and THAT leg runs on the vault's real token program.
///
/// So the assertions below are about the SALE. "The transaction failed" is not

/// Slippage really reverts, on chain, and the revert is atomic.
///
/// RFP-015 §Supportability names "slippage revert (tokens_out below minimum)" as
/// required coverage, and §Reliability 2 requires a failed buy to revert
/// atomically with the buyer's collateral not consumed. Both were unproven here:
/// every one of the 46 `min_tokens_out` values in this whole test suite was `0`,
/// and with `0` the guard cannot fire even in principle - `buy.rs:63` asserts
/// `tokens_out >= 1` BEFORE the slippage assert at `:69`, so `min_tokens_out: 0`
/// makes the slippage check a tautology. The only tests that reached it called
/// `apply_buy` directly, bypassing dispatch, accounts and the state machine.
///
/// The floor is set to exactly `tokens_out + 1` - one unit above what the curve
/// will actually pay - so this pins the boundary rather than passing because the
/// number was absurd.
#[test]
fn public_buy_reverts_on_slippage_and_leaves_the_sale_untouched() {
    let buyer_key = PrivateKey::try_new([61; 32]).expect("key");
    let buyer_coll = id_of(&buyer_key);
    let buyer_tok = AccountId::new([62; 32]);
    let creator = AccountId::new([9u8; 32]);
    let i = ids(creator);

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_open_sale(&mut state, &i);

    let buyer_start: u128 = 100_000;
    let collateral_in: u128 = 5_000;
    state.force_insert_account(buyer_coll, fungible(i.collateral_def, buyer_start));
    state.force_insert_account(buyer_tok, fungible(i.token_def, 0));

    // Exactly what the curve pays for this input, then ask for one more.
    let (tokens_out, fee, c_eff) = buy_tokens_out(VT, VC, FEE_BPS, collateral_in);
    assert!(tokens_out > 0, "fixture must produce a real output or the floor is meaningless");

    let message = || {
        public_transaction::Message::try_new(
            bc(),
            vec![i.sale, i.token_vault, i.collateral_vault, i.treasury, buyer_coll, buyer_tok, CLOCK_01],
            vec![Nonce(0)],
            Instruction::Buy { collateral_in, min_tokens_out: tokens_out + 1, deadline: u64::MAX },
        )
        .expect("message")
    };
    let m = message();
    let w = public_transaction::WitnessSet::for_message(&m, &[&buyer_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, w), 0, 0)
        .expect_err("a buy whose output is below min_tokens_out must be rejected");

    // Atomic: nothing moved anywhere.
    let sale = SaleState::try_from(&state.get_account_by_id(i.sale).data).unwrap();
    assert_eq!(sale.sale_reserve, D, "no reserve consumed");
    assert_eq!(sale.real_collateral, 0, "no collateral credited");
    assert_eq!(sale.virt_token, VT, "the curve must not move");
    assert_eq!(sale.virt_collateral, VC, "the curve must not move");
    assert_eq!(sale.treasury_owed, 0);
    assert_eq!(sale.cum_collateral_in, 0);
    assert_eq!(sale.cum_fees, 0);
    assert_eq!(sale.buy_count, 0, "a rejected buy is not a buy");
    assert_eq!(bal(&state, buyer_coll), buyer_start, "the buyer's collateral is NOT consumed");
    assert_eq!(bal(&state, buyer_tok), 0, "no tokens delivered");
    assert_eq!(bal(&state, i.token_vault), D + R, "token vault untouched");
    assert_eq!(bal(&state, i.collateral_vault), 0, "collateral vault untouched");
    assert_eq!(bal(&state, i.treasury), 0, "no fee swept");

    // Control: the SAME buy with the floor at exactly what the curve pays goes
    // through. Without this, the test would still pass if the fixture were
    // broken in some way unrelated to slippage.
    let m = public_transaction::Message::try_new(
        bc(),
        vec![i.sale, i.token_vault, i.collateral_vault, i.treasury, buyer_coll, buyer_tok, CLOCK_01],
        vec![Nonce(0)],
        Instruction::Buy { collateral_in, min_tokens_out: tokens_out, deadline: u64::MAX },
    )
    .expect("message");
    let w = public_transaction::WitnessSet::for_message(&m, &[&buyer_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, w), 0, 0)
        .expect("the identical buy must succeed when the floor is exactly met");
    assert_eq!(bal(&state, buyer_tok), tokens_out, "control: tokens delivered");
    assert_eq!(bal(&state, i.collateral_vault), c_eff, "control: collateral really moved");
    assert_eq!(bal(&state, i.treasury), fee, "control: fee really swept");
}

/// the property; "no reserve was consumed and no collateral was credited" is.
#[test]
fn buy_rejects_a_collateral_holding_owned_by_another_program() {
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_open_sale(&mut state, &i);

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

    let (tokens_out, fee, c_eff) = buy_tokens_out(VT, VC, FEE_BPS, collateral_in);
    assert!(tokens_out > 0 && fee > 0, "the buy must be one that would really move the sale");

    // The same message twice: once with the substituted holding, once with it
    // repaired. A rejected transaction advances no nonce, so both sign at 0.
    let message = || {
        public_transaction::Message::try_new(
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
        err.contains(
            "buyer collateral holding is not owned by the sale's collateral token program"
        ),
        "it must revert on the vault-anchored ownership guard and not for some unrelated \
         reason - a test that passes on any error would pass on the vulnerable build too: {err}"
    );

    // Nothing moved, and - the point of the attack - nothing was consumed.
    let sale = SaleState::try_from(&state.get_account_by_id(i.sale).data).unwrap();
    assert_eq!(sale.sale_reserve, D, "no reserve consumed: this is the free sale-kill");
    assert_eq!(sale.real_collateral, 0, "no collateral credited: this is what bounds a later Sell");
    assert_eq!(sale.virt_collateral, VC, "the curve must not move");
    assert_eq!(sale.virt_token, VT);
    assert_eq!(sale.treasury_owed, 0);
    assert_eq!(sale.cum_collateral_in, 0);
    assert_eq!(sale.cum_fees, 0);
    assert_eq!(sale.buy_count, 0, "a rejected buy is not a buy");
    assert_eq!(bal(&state, i.token_vault), D + R, "token vault untouched");
    assert_eq!(bal(&state, i.collateral_vault), 0, "collateral vault untouched");
    assert_eq!(bal(&state, i.treasury), 0, "no fee swept");
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
    assert_eq!(bal(&state, i.collateral_vault), c_eff, "control: collateral really moved");
    assert_eq!(bal(&state, i.treasury), fee, "control: fee really swept");
    let sale = SaleState::try_from(&state.get_account_by_id(i.sale).data).unwrap();
    assert_eq!(sale.sale_reserve, D - tokens_out, "control: the reserve moves for a real buy");
    assert_eq!(sale.buy_count, 1);
}

/// The SDK's 3-transaction `buy-private` saga prices its middle leg as a PUBLIC
/// `Buy`, and this test pins the half of the trade-off that leg buys you: it
/// re-prices against LIVE sale state, so a competing buy landing first does not
/// invalidate it. Also confirms a fresh account-A token holding is initialised
/// by the buy.
///
/// That is a trade-off and not a verdict, which is what the older comment here
/// got wrong. The two private paths fail in opposite directions and lpad ships
/// both:
///
///   * the SAGA is drift-free, because its buy is a public transaction quoted
///     against live state - and it pays for that with a publicly visible buy
///     leg, plus a window between transactions in which a client crash strands
///     the funds in the open;
///   * `BuyDisposable` is atomic and private - one proof, both buyer holdings
///     private notes, nothing to strand - and it pays for that by pinning the
///     sale PDA at proving time, so a competing buy that lands first makes the
///     proof stale and the sequencer rejects it
///     (`disposable_buy_is_rejected_after_a_competing_public_buy`).
///
/// Neither is strictly better: a quiet sale favours the in-proof buy, a
/// contended one favours the saga.
#[test]
fn saga_public_buy_leg_reprices_against_live_state() {
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_open_sale(&mut state, &i);

    let collateral_in: u128 = 5_000;
    // What A would get against the INITIAL reserves - the snapshot a
    // `BuyDisposable` proof would pin, and then drift off of.
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

    // A's public buy leg against the moved sale; a_coll + a_tok sign.
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
    // moved sale rather than a pinned snapshot. That is what the saga buys, and
    // the publicly visible leg above is what it costs.
    assert_eq!(bal(&state, a_tok), live_out, "output holding credited at the live price");
    assert!(live_out > 0);
    assert_eq!(bal(&state, a_coll), 0, "all deshielded collateral consumed by the public buy");
}

// --- BuyDisposable: the private buy --------------------------------------
//
// These are not public transactions, so they do not go through
// `transition_from_public_transaction` at all. Each builds a real
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
// `buy-private` saga is a different mechanism and still ships; nothing here
// replaces it (see `saga_public_buy_leg_reprices_against_live_state`).

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
/// three public slots are the sale and its two vaults, all unauthorized at top
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
        AccountWithMetadata::new(state.get_account_by_id(i.sale), false, i.sale),
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
        lpad_guests::bonding_curve(),
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

/// Seed an open sale that ends at `end_timestamp_ms`. The shared fixture uses 0
/// ("no end"), which the window tests specifically need to differ from.
fn seed_sale_ending_at(state: &mut V03State, i: &Ids, end_timestamp_ms: u64) {
    let mut sale = open_sale(i, false);
    sale.end_timestamp_ms = end_timestamp_ms;
    let sale_acc = Account {
        program_owner: bc(),
        data: Data::from(&sale),
        ..Default::default()
    };
    state.force_insert_account(i.sale, sale_acc);
    state.force_insert_account(i.token_vault, fungible(i.token_def, D + R));
    state.force_insert_account(i.collateral_vault, fungible(i.collateral_def, 0));
    state.force_insert_account(i.treasury, fungible(i.collateral_def, 0));
}

/// Balance of a vault, treating "no account" as 0. The fixtures seed the
/// collateral vault (as `CreateSale` does), but a buy that reverts before the
/// vault is ever written to can still leave it absent, and [`bal`] would panic
/// instead of reporting the zero we want to see.
fn vault_balance(state: &V03State, id: AccountId) -> u128 {
    if state.get_account_by_id_ref(id).is_some() { bal(state, id) } else { 0 }
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
/// lifecycle instructions read for their end-timestamp guards.
///
/// [`bonding_curve_core::ClockData`] is deserialize-only - the sequencer writes
/// that account, programs only read it - so its 16-byte Borsh body is written
/// out by hand: a `u64` block id then an `i64` millisecond timestamp, both
/// little-endian. `probe.rs` decodes the same layout.
fn set_clock(state: &mut V03State, block_id: u64, ts_ms: i64) {
    let mut clock = state.get_account_by_id(CLOCK_01);
    let mut bytes = Vec::with_capacity(16);
    bytes.extend_from_slice(&block_id.to_le_bytes());
    bytes.extend_from_slice(&ts_ms.to_le_bytes());
    clock.data = Data::try_from(bytes).expect("16 bytes is well within the account data cap");
    state.force_insert_account(CLOCK_01, clock);
}

/// A buy big enough that the fee, the output and the reserve move are all
/// non-zero - a zero-fee buy would pass the escrow assertions vacuously.
const DISPOSABLE_IN: u128 = 5_000;

#[test]
fn disposable_buy_moves_the_curve_and_credits_a_private_note() {
    force_dev_mode();
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_open_sale(&mut state, &i);

    let keys = buyer_keys();
    let start: u128 = 100_000;
    let (mut state, note) = fund_private_collateral(state, &i, &keys, start);

    let (tokens_out, fee, c_eff) = buy_tokens_out(VT, VC, FEE_BPS, DISPOSABLE_IN);
    assert!(tokens_out > 0 && fee > 0, "the buy must move a non-zero fee and output");

    let tx = prove_disposable(
        &state,
        &i,
        &keys,
        &note,
        Instruction::BuyDisposable {
            collateral_in: DISPOSABLE_IN,
            min_tokens_out: 0,
            not_before_ms: 0,
            deadline: u64::MAX,
        },
    )
    .expect("proving a disposable buy against an open sale must succeed");
    state
        .transition_from_privacy_preserving_transaction(&tx, 1, 0)
        .expect("a disposable buy inside its window must be included");

    // The public half of the trade: the curve moved and both vaults settled.
    assert_eq!(
        vault_balance(&state, i.collateral_vault),
        DISPOSABLE_IN,
        "collateral vault receives the FULL gross collateral_in - the fee is escrowed here, \
         not swept to the treasury, because a private buy cannot pin the treasury account"
    );
    assert_eq!(bal(&state, i.token_vault), D + R - tokens_out, "token vault drained by tokens_out");
    assert_eq!(bal(&state, i.treasury), 0, "the treasury is not even declared by this instruction");

    let sale = SaleState::try_from(&state.get_account_by_id(i.sale).data).unwrap();
    assert_eq!(sale.sale_reserve, D - tokens_out);
    assert_eq!(sale.virt_token, VT - tokens_out);
    assert_eq!(sale.virt_collateral, VC + c_eff);
    assert_eq!(sale.real_collateral, c_eff);
    assert_eq!(sale.treasury_owed, fee, "the fee is escrowed as treasury_owed");
    assert_eq!(sale.cum_fees, fee);
    assert_eq!(sale.buy_count, 1);
    assert_eq!(
        vault_balance(&state, i.collateral_vault),
        sale.real_collateral + sale.treasury_owed,
        "vault invariant: balance == real_collateral + treasury_owed"
    );

    // The private half: two notes, both recognisable only with the buyer's keys.
    // Finding them in the commitment set is the buyer's proof of payment - and
    // the only trace either holding leaves.
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
/// transaction's PUBLIC account ids are exactly the sale and its two vaults, and
/// the buyer's two holdings resolve to nothing in public state. The collateral
/// was spent and the tokens were delivered, so there is no reachable state in
/// which the payment happened and the tokens are public.
#[test]
fn disposable_buy_publishes_only_the_sale_and_its_vaults() {
    force_dev_mode();
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_open_sale(&mut state, &i);

    let keys = buyer_keys();
    let (mut state, note) = fund_private_collateral(state, &i, &keys, 100_000);
    let (tokens_out, _, _) = buy_tokens_out(VT, VC, FEE_BPS, DISPOSABLE_IN);

    let tx = prove_disposable(
        &state,
        &i,
        &keys,
        &note,
        Instruction::BuyDisposable {
            collateral_in: DISPOSABLE_IN,
            min_tokens_out: 0,
            not_before_ms: 0,
            deadline: u64::MAX,
        },
    )
    .expect("proving must succeed");

    assert_eq!(
        tx.message().public_account_ids(),
        vec![i.sale, i.token_vault, i.collateral_vault],
        "a disposable buy publishes the sale and its two vaults and NOTHING else - \
         no treasury, no clock, and neither buyer holding"
    );

    state
        .transition_from_privacy_preserving_transaction(&tx, 1, 0)
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
    assert_eq!(vault_balance(&state, i.collateral_vault), DISPOSABLE_IN);
    assert_eq!(bal(&state, i.token_vault), D + R - tokens_out);
}

#[test]
fn disposable_buy_is_rejected_before_not_before_ms() {
    force_dev_mode();
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_open_sale(&mut state, &i);

    let keys = buyer_keys();
    let (mut state, note) = fund_private_collateral(state, &i, &keys, 100_000);
    let not_before_ms = 10_000_u64;

    let tx = prove_disposable(
        &state,
        &i,
        &keys,
        &note,
        Instruction::BuyDisposable {
            collateral_in: DISPOSABLE_IN,
            min_tokens_out: 0,
            not_before_ms,
            deadline: 20_000,
        },
    )
    .expect("proving must succeed");
    assert_eq!(
        tx.message().timestamp_validity_window.start(),
        Some(not_before_ms),
        "the guest must bound the window from BELOW - it is the only thing tying a \
         clock-less proof to real time"
    );

    let early = state.transition_from_privacy_preserving_transaction(&tx, 1, not_before_ms - 1);
    assert!(
        matches!(early, Err(LeeError::OutOfValidityWindow)),
        "a disposable buy submitted before not_before_ms must be rejected as out of \
         validity window, got {early:?}"
    );
    assert_eq!(vault_balance(&state, i.collateral_vault), 0, "no collateral moved");
    assert_eq!(bal(&state, i.token_vault), D + R, "no tokens moved");

    // The same proof, one millisecond later, is admitted - so the rejection above
    // was the bound and not some unrelated defect in the transaction.
    state
        .transition_from_privacy_preserving_transaction(&tx, 1, not_before_ms)
        .expect("at not_before_ms the window is open (it is inclusive from below)");
    assert_eq!(vault_balance(&state, i.collateral_vault), DISPOSABLE_IN);
}

#[test]
fn disposable_buy_is_rejected_at_the_sale_end_timestamp() {
    force_dev_mode();
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    let end_ms = 500_000_u64;
    seed_sale_ending_at(&mut state, &i, end_ms);

    let keys = buyer_keys();
    let (mut state, note) = fund_private_collateral(state, &i, &keys, 100_000);

    // Deadline u64::MAX and a not_before well inside the sale: the sale's own end
    // is the only bound that can produce `end_ms` here.
    let tx = prove_disposable(
        &state,
        &i,
        &keys,
        &note,
        Instruction::BuyDisposable {
            collateral_in: DISPOSABLE_IN,
            min_tokens_out: 0,
            not_before_ms: 400_000,
            deadline: u64::MAX,
        },
    )
    .expect("proving must succeed");
    assert_eq!(
        tx.message().timestamp_validity_window.end(),
        Some(end_ms),
        "the window's upper bound must be the sale's end_timestamp_ms - a private buy \
         carries no clock, so this is the ONLY way it can honour the end of the sale"
    );

    let at_end = state.transition_from_privacy_preserving_transaction(&tx, 1, end_ms);
    assert!(
        matches!(at_end, Err(LeeError::OutOfValidityWindow)),
        "the window is half-open, so a buy at exactly end_timestamp_ms must be rejected, \
         got {at_end:?}"
    );
    assert_eq!(vault_balance(&state, i.collateral_vault), 0, "no collateral moved");
    assert_eq!(bal(&state, i.token_vault), D + R, "no tokens moved");

    state
        .transition_from_privacy_preserving_transaction(&tx, 1, end_ms - 1)
        .expect("one millisecond before the end the sale is still open");
}

/// The guest CLAMPS the validity window instead of echoing what the submitter
/// asked for. With `deadline = u64::MAX` and no configured sale end, the only
/// bound left is the staleness cap, and it must be the one that lands in the
/// transaction - otherwise a buyer could hold a finished proof indefinitely and
/// submit it only once the curve had moved their way (a free option on the sale).
#[test]
fn disposable_buy_window_is_clamped_to_the_staleness_cap() {
    force_dev_mode();
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_open_sale(&mut state, &i); // end_timestamp_ms == 0: no configured end

    let keys = buyer_keys();
    let (mut state, note) = fund_private_collateral(state, &i, &keys, 100_000);
    let not_before_ms = 1_000_000_u64;
    let capped_hi = not_before_ms + MAX_PRIVATE_WINDOW_MS;

    let tx = prove_disposable(
        &state,
        &i,
        &keys,
        &note,
        Instruction::BuyDisposable {
            collateral_in: DISPOSABLE_IN,
            min_tokens_out: 0,
            not_before_ms,
            deadline: u64::MAX,
        },
    )
    .expect("proving must succeed");
    assert_eq!(
        tx.message().timestamp_validity_window.end(),
        Some(capped_hi),
        "the caller asked for u64::MAX; the guest must emit not_before_ms + \
         MAX_PRIVATE_WINDOW_MS instead"
    );

    let stale = state.transition_from_privacy_preserving_transaction(&tx, 1, capped_hi);
    assert!(
        matches!(stale, Err(LeeError::OutOfValidityWindow)),
        "a proof submitted MAX_PRIVATE_WINDOW_MS after its own not_before_ms must be \
         rejected even though the caller's deadline was u64::MAX, got {stale:?}"
    );
    assert_eq!(vault_balance(&state, i.collateral_vault), 0, "no collateral moved");

    state
        .transition_from_privacy_preserving_transaction(&tx, 1, capped_hi - 1)
        .expect("one millisecond inside the cap the proof is still good");
}

/// The drift trade-off, made explicit. A disposable buy pins the sale PDA
/// byte-for-byte at proving time and the sequencer re-verifies it against live
/// state at inclusion, so a competing PUBLIC buy that lands first invalidates
/// the proof. This is the price of atomicity and privacy, and it is a rejection,
/// never a mispriced fill.
#[test]
fn disposable_buy_is_rejected_after_a_competing_public_buy() {
    force_dev_mode();
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_open_sale(&mut state, &i);

    let keys = buyer_keys();
    let (mut state, note) = fund_private_collateral(state, &i, &keys, 100_000);
    let tx = prove_disposable(
        &state,
        &i,
        &keys,
        &note,
        Instruction::BuyDisposable {
            collateral_in: DISPOSABLE_IN,
            min_tokens_out: 0,
            not_before_ms: 0,
            deadline: u64::MAX,
        },
    )
    .expect("proving must succeed");

    // A competing public buy lands while the private buyer is still proving.
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
    let moved = SaleState::try_from(&state.get_account_by_id(i.sale).data).unwrap();
    assert!(moved.virt_token < VT, "the competing buy must actually move the curve");

    let stale = state.transition_from_privacy_preserving_transaction(&tx, 1, 0);
    assert!(
        matches!(stale, Err(LeeError::InvalidPrivacyPreservingProof)),
        "the pinned sale pre-state no longer matches live state, so the proof must be \
         rejected as invalid rather than settled at the stale price, got {stale:?}"
    );
    assert!(
        state
            .get_proof_for_commitment(&Commitment::new(
                &keys.id(TOKEN_NOTE),
                &private_holding(
                    i.token_def,
                    buy_tokens_out(VT, VC, FEE_BPS, DISPOSABLE_IN).0,
                    keys.id(TOKEN_NOTE),
                ),
            ))
            .is_none(),
        "no note may be created by a rejected transaction"
    );
    let after = SaleState::try_from(&state.get_account_by_id(i.sale).data).unwrap();
    assert_eq!(after.buy_count, 1, "only the competing public buy counted");
    assert_eq!(after.treasury_owed, 0, "no disposable fee was ever escrowed");
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
    seed_open_sale(&mut state, &i);

    let keys = buyer_keys();
    let (state, note) = fund_private_collateral(state, &i, &keys, 100_000);

    // The five real accounts, plus the clock the attacker wants read.
    let pre_states = vec![
        AccountWithMetadata::new(state.get_account_by_id(i.sale), false, i.sale),
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
            not_before_ms: 0,
            deadline: u64::MAX,
        })
        .unwrap(),
        identities,
        &ProgramWithDependencies::new(
            lpad_guests::bonding_curve(),
            HashMap::from([(token(), programs::token())]),
        ),
    );
    let Err(LeeError::ProgramProveFailed(message)) = result else {
        panic!("a six-account BuyDisposable must fail to prove, got {result:?}");
    };
    assert!(
        message.contains("BuyDisposable requires exactly five accounts"),
        "it must fail on the fixed-arity destructure, not somewhere later: {message}"
    );
}

/// A buy priced at or after the sale's end never becomes a transaction: the
/// window would be empty, which LEZ rejects outright, so the guest asserts it
/// first and says what to do about it.
#[test]
fn disposable_buy_after_the_sale_end_cannot_be_proved() {
    force_dev_mode();
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    let end_ms = 500_000_u64;
    seed_sale_ending_at(&mut state, &i, end_ms);

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
            not_before_ms: end_ms,
            deadline: u64::MAX,
        },
    );
    let Err(LeeError::ProgramProveFailed(message)) = result else {
        panic!("a buy that starts at the sale's end must fail to prove, got {result:?}");
    };
    assert!(
        message.contains("empty timestamp validity window"),
        "it must fail on the empty-window assert: {message}"
    );
}

/// The same substitution through the PRIVATE path, and this is the one that bit
/// hardest: `BuyDisposable` has no separate fee transfer whose token type could
/// accidentally cross-check the deposit, so the payer's two chained legs were
/// the only thing standing between a fabricated holding and a consumed reserve.
/// It is also the cheapest version of the attack to run - the buyer's holdings
/// are private notes, so the account whose owner is being lied about never
/// appears in public state at all.
///
/// The fixture is the one `disposable_buy_moves_the_curve_and_credits_a_private_note`
/// proves and includes successfully; the note's `program_owner` is the only
/// difference, which is what makes the rejection attributable to it.
#[test]
fn disposable_buy_rejects_a_collateral_note_owned_by_another_program() {
    force_dev_mode();
    let creator = id_of(&PrivateKey::try_new([42; 32]).unwrap());
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_open_sale(&mut state, &i);

    let keys = buyer_keys();
    let (state, note) =
        fund_substituted_private_collateral(state, &i, &keys, 100_000, substituted_program());

    let result = prove_disposable(
        &state,
        &i,
        &keys,
        &note,
        Instruction::BuyDisposable {
            collateral_in: DISPOSABLE_IN,
            min_tokens_out: 0,
            not_before_ms: 0,
            deadline: u64::MAX,
        },
    );
    let Err(LeeError::ProgramProveFailed(message)) = result else {
        panic!(
            "a disposable buy paying from a foreign-owned note must fail to prove, got {result:?}"
        );
    };
    assert!(
        message.contains(
            "buyer collateral holding is not owned by the sale's collateral token program"
        ),
        "it must fail on the vault-anchored ownership guard, not on the definition check \
         before it or anything after it: {message}"
    );

    // The revert lands on the buyer's own machine, so there is no transaction to
    // reject and the sale is untouched by construction. Assert it anyway: the
    // claim being defended is about the sale's state, not about the prover's
    // return value.
    let sale = SaleState::try_from(&state.get_account_by_id(i.sale).data).unwrap();
    assert_eq!(sale.sale_reserve, D, "no reserve consumed");
    assert_eq!(sale.real_collateral, 0, "no collateral credited");
    assert_eq!(sale.virt_collateral, VC, "the curve must not move");
    assert_eq!(sale.virt_token, VT);
    assert_eq!(sale.treasury_owed, 0, "no fee escrowed");
    assert_eq!(sale.buy_count, 0);
    assert_eq!(bal(&state, i.token_vault), D + R, "token vault untouched");
    assert_eq!(vault_balance(&state, i.collateral_vault), 0, "collateral vault untouched");
    assert!(
        state
            .get_proof_for_commitment(&Commitment::new(
                &keys.id(TOKEN_NOTE),
                &private_holding(
                    i.token_def,
                    buy_tokens_out(VT, VC, FEE_BPS, DISPOSABLE_IN).0,
                    keys.id(TOKEN_NOTE),
                ),
            ))
            .is_none(),
        "and no token note may exist: the buy never became a transaction"
    );
}
/// The escrowed fee is not lost, and settling it is decoupled from the raise.
///
/// A disposable buy leaves the whole gross `collateral_in` in the collateral
/// vault and records the fee as `treasury_owed`, because it cannot pin the
/// treasury account into its proof. `Withdraw` deliberately does NOT settle
/// that: it declares no treasury account at all, pays the creator
/// `vault_balance - treasury_owed`, and leaves the escrow - and the collateral
/// backing it - in the vault. The permissionless `SweepTreasury` hands it over
/// afterwards. This is the whole escrow path end to end, in the order it happens
/// on chain.
///
/// Settling it inside `Withdraw` is what the first cut of this feature did, and
/// `an_unusable_treasury_cannot_block_the_creators_withdrawal` is the other half
/// of this pair: it pins the reason the two were split.
#[test]
fn withdraw_leaves_the_escrow_and_sweep_treasury_settles_it() {
    force_dev_mode();
    let creator_key = PrivateKey::try_new([42; 32]).unwrap();
    let creator = id_of(&creator_key);
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    let end_ms = 500_000_u64;
    seed_sale_ending_at(&mut state, &i, end_ms);

    // The creator identity account must be owned by SOMETHING. It signs the close
    // and then the withdraw, and the close bumps its nonce - after which
    // `validate_execution` rule 7 (a post state with the default program owner
    // requires a fully default pre state) rejects any transaction that still
    // declares it while it is unclaimed. See the note in
    // `bonding_curve/src/dispatch.rs`; a real creator's account is owned by the
    // token or native program, so this is what production looks like, not a
    // convenience.
    state.force_insert_account(creator, fungible(i.collateral_def, 0));

    let keys = buyer_keys();
    let (mut state, note) = fund_private_collateral(state, &i, &keys, 100_000);
    let (tokens_out, fee, c_eff) = buy_tokens_out(VT, VC, FEE_BPS, DISPOSABLE_IN);
    assert!(fee > 0, "the test must exercise a non-zero escrow");

    let tx = prove_disposable(
        &state,
        &i,
        &keys,
        &note,
        Instruction::BuyDisposable {
            collateral_in: DISPOSABLE_IN,
            min_tokens_out: 0,
            not_before_ms: 100_000,
            deadline: u64::MAX,
        },
    )
    .expect("proving must succeed");
    state
        .transition_from_privacy_preserving_transaction(&tx, 1, 100_000)
        .expect("the disposable buy must be included");
    assert_eq!(
        bal(&state, i.collateral_vault),
        DISPOSABLE_IN,
        "the disposable buy moves the FULL gross collateral into the vault - the fee is not \
         split off into its own transfer, it is escrowed in place"
    );

    // Close the sale at its end timestamp, then withdraw.
    set_clock(&mut state, 2, i64::try_from(end_ms).unwrap());
    let close = public_transaction::Message::try_new(
        bc(),
        vec![i.sale, creator, CLOCK_01],
        vec![Nonce(0)],
        Instruction::CloseSale { deadline: u64::MAX },
    )
    .unwrap();
    let w = public_transaction::WitnessSet::for_message(&close, &[&creator_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(close, w), 2, end_ms)
        .expect("the creator must be able to close at the end timestamp");

    let creator_coll = AccountId::new([90; 32]);
    let creator_tok = AccountId::new([91; 32]);
    state.force_insert_account(creator_coll, fungible(i.collateral_def, 0));
    state.force_insert_account(creator_tok, fungible(i.token_def, 0));
    // Six accounts, and no treasury among them. That absence is the fix: it is
    // what makes the withdrawal independent of whether the treasury is usable.
    let withdraw = public_transaction::Message::try_new(
        bc(),
        vec![
            i.sale,
            i.token_vault,
            i.collateral_vault,
            creator_coll,
            creator_tok,
            creator,
        ],
        vec![Nonce(1)], // the creator already signed the close
        Instruction::Withdraw { deadline: u64::MAX },
    )
    .unwrap();
    let w = public_transaction::WitnessSet::for_message(&withdraw, &[&creator_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(withdraw, w), 3, end_ms)
        .expect("withdraw must succeed");

    assert_eq!(
        bal(&state, creator_coll),
        c_eff,
        "the creator is paid the raise NET of the escrow - which for a disposable buy is \
         exactly the effective collateral the curve priced"
    );
    assert_eq!(bal(&state, creator_tok), D + R - tokens_out, "the creator gets the unsold tokens");
    assert_eq!(bal(&state, i.token_vault), 0, "token vault drained");
    assert_eq!(
        bal(&state, i.collateral_vault),
        fee,
        "the escrow and its backing collateral STAY in the vault: this is the invariant \
         `collateral_vault_balance == real_collateral + treasury_owed`, still true after a \
         withdrawal because the withdrawal took only `real_collateral`"
    );
    assert_eq!(bal(&state, i.treasury), 0, "withdraw names no treasury, so none is paid");
    let sale = SaleState::try_from(&state.get_account_by_id(i.sale).data).unwrap();
    assert_eq!(
        sale.treasury_owed, fee,
        "and the bucket is NOT zeroed by the withdrawal - zeroing it there is what turned the \
         escrow into an unclaimable balance"
    );

    // The sweep: three accounts, no signer, no nonce. Anyone may submit it, which
    // is why `lifecycle::sweep_treasury` anchors its dispatch on the vault rather
    // than on the treasury it was handed.
    let sweep = || {
        public_transaction::Message::try_new(
            bc(),
            vec![i.sale, i.collateral_vault, i.treasury],
            vec![],
            Instruction::SweepTreasury { deadline: u64::MAX },
        )
        .unwrap()
    };
    let m = sweep();
    let w = public_transaction::WitnessSet::for_message(&m, &[]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, w), 4, end_ms)
        .expect("a permissionless sweep must be able to settle the escrow after the withdrawal");

    assert_eq!(bal(&state, i.treasury), fee, "the escrowed disposable fee reaches the treasury");
    assert_eq!(bal(&state, i.collateral_vault), 0, "and the collateral vault is finally drained");
    assert_eq!(bal(&state, creator_coll), c_eff, "the creator's payout is untouched by the sweep");
    let sale = SaleState::try_from(&state.get_account_by_id(i.sale).data).unwrap();
    assert_eq!(sale.treasury_owed, 0, "the escrow bucket is zeroed once paid");

    // A second sweep reverts rather than paying twice. The instruction is
    // permissionless and carries no nonce of its own, so "submit it again" is
    // free for anyone to do - the revert is the only thing standing between that
    // and a drained vault.
    let m = sweep();
    let w = public_transaction::WitnessSet::for_message(&m, &[]);
    let result = state.transition_from_public_transaction(&PublicTransaction::new(m, w), 5, end_ms);
    let Err(LeeError::ProgramExecutionFailed(err)) = result else {
        panic!("re-sweeping a settled sale must revert, got {result:?}");
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
/// sale below is one this program can no longer build, and the fixture
/// force-inserts it. That is the reason the test stays, not a reason to delete
/// it: the creation pin is the first line of defence, and this is the second,
/// the one that BOUNDS the damage if the first is ever loosened - by a new
/// creation path, a migration, or a sale that predates the pin. Here the
/// treasury is a holding of the PROJECT token rather than the collateral token,
/// its `token::Transfer` can never succeed, and the creator still gets
/// everything that is theirs.
///
/// The escrow itself does stay stranded, and that is what this test pins the
/// boundary of: the fee, and only the fee. `SaleState::treasury_owed` spells out
/// why nothing in this program can release it once it is stuck, which is the
/// whole reason the shapes that get it stuck are rejected at creation.
#[test]
fn an_unusable_treasury_cannot_block_the_creators_withdrawal() {
    force_dev_mode();
    let creator_key = PrivateKey::try_new([42; 32]).unwrap();
    let creator = id_of(&creator_key);
    let i = ids(creator);
    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    let end_ms = 500_000_u64;
    seed_sale_ending_at(&mut state, &i, end_ms);
    // The one difference from the test above: the treasury holds the PROJECT
    // token, not the collateral token. Nothing on chain rejects that at creation,
    // and the `token::Transfer` into it can never succeed.
    state.force_insert_account(i.treasury, fungible(i.token_def, 0));
    state.force_insert_account(creator, fungible(i.collateral_def, 0));

    let keys = buyer_keys();
    let (mut state, note) = fund_private_collateral(state, &i, &keys, 100_000);
    let (tokens_out, fee, c_eff) = buy_tokens_out(VT, VC, FEE_BPS, DISPOSABLE_IN);
    assert!(fee > 0, "the test must exercise a non-zero escrow");

    let tx = prove_disposable(
        &state,
        &i,
        &keys,
        &note,
        Instruction::BuyDisposable {
            collateral_in: DISPOSABLE_IN,
            min_tokens_out: 0,
            not_before_ms: 100_000,
            deadline: u64::MAX,
        },
    )
    .expect("proving must succeed");
    state
        .transition_from_privacy_preserving_transaction(&tx, 1, 100_000)
        .expect("the disposable buy must be included");

    set_clock(&mut state, 2, i64::try_from(end_ms).unwrap());
    let close = public_transaction::Message::try_new(
        bc(),
        vec![i.sale, creator, CLOCK_01],
        vec![Nonce(0)],
        Instruction::CloseSale { deadline: u64::MAX },
    )
    .unwrap();
    let w = public_transaction::WitnessSet::for_message(&close, &[&creator_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(close, w), 2, end_ms)
        .expect("the creator must be able to close at the end timestamp");

    let creator_coll = AccountId::new([90; 32]);
    let creator_tok = AccountId::new([91; 32]);
    state.force_insert_account(creator_coll, fungible(i.collateral_def, 0));
    state.force_insert_account(creator_tok, fungible(i.token_def, 0));
    let withdraw = public_transaction::Message::try_new(
        bc(),
        vec![
            i.sale,
            i.token_vault,
            i.collateral_vault,
            creator_coll,
            creator_tok,
            creator,
        ],
        vec![Nonce(1)],
        Instruction::Withdraw { deadline: u64::MAX },
    )
    .unwrap();
    let w = public_transaction::WitnessSet::for_message(&withdraw, &[&creator_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(withdraw, w), 3, end_ms)
        .expect(
            "withdraw must succeed even though the treasury cannot receive - if this fails, \
             settlement has been folded back into Withdraw and an unusable fee sink locks the \
             entire raise again",
        );

    assert_eq!(bal(&state, creator_coll), c_eff, "the creator got the whole raise net of the fee");
    assert_eq!(bal(&state, creator_tok), D + R - tokens_out, "and every unsold project token");
    assert_eq!(bal(&state, i.token_vault), 0, "token vault drained");

    // The sweep is the leg that fails, and it fails alone.
    let m = public_transaction::Message::try_new(
        bc(),
        vec![i.sale, i.collateral_vault, i.treasury],
        vec![],
        Instruction::SweepTreasury { deadline: u64::MAX },
    )
    .unwrap();
    let w = public_transaction::WitnessSet::for_message(&m, &[]);
    let result = state.transition_from_public_transaction(&PublicTransaction::new(m, w), 4, end_ms);
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
    let sale = SaleState::try_from(&state.get_account_by_id(i.sale).data).unwrap();
    assert_eq!(sale.treasury_owed, fee, "and still recorded as owed, not written off");
}

/// F26, end to end: the dust grief is dead, and the escrow settles into a
/// treasury a stranger has dusted.
///
/// THE LOCK THIS REPLACES. `CreateSale` used to accept a `treasury_id` naming an
/// account that did not exist, and settling then had to CLAIM that account. LEZ
/// admits a claim only when the pre-state is `Account::default()` WHOLE, so
/// anyone could send the (publicly readable) treasury id a dust native balance
/// and the only instruction that can ever pay the escrow out would fail forever,
/// on a sale they had nothing to do with, for the price of one transfer. The
/// error even said so: "retrying cannot help".
///
/// Requiring an ALREADY-initialised holding at creation is what kills it, and
/// this test walks the whole attack rather than just `create_sale`'s asserts:
/// the stranger dusts the announced fee sink and the CREATE IS REFUSED, the
/// creator names an initialised holding instead and the sale is built BY THE
/// PROGRAM, the stranger dusts THAT account mid-sale, and the sweep still
/// settles into it - because a holding the token program already owns is never
/// claimed, so its native balance and its nonce stop being able to matter. The
/// first of those is the mutation kill: remove the pin and the refused create
/// succeeds, which this test asserts it must not.
///
/// It carries the mixed-path escrow invariant too, which is the other thing that
/// must not regress. The PUBLIC buy pays its fee to the treasury inside the same
/// transaction; the PRIVATE `BuyDisposable` cannot name the treasury at all (a
/// public account in a privacy transaction is pinned byte-for-byte at proving
/// time, so any fee-bearing activity anywhere would invalidate every in-flight
/// private buy) and escrows its fee in the collateral vault instead. Both paths
/// run against one sale here, and
/// `collateral_vault_balance == real_collateral + treasury_owed` is asserted
/// after each of them, after the withdrawal, and down to zero after the sweep.
#[test]
fn a_dusted_treasury_still_settles_after_a_real_create_sale() {
    force_dev_mode();
    let creator_token_key = PrivateKey::try_new([41; 32]).expect("key");
    let creator_key = PrivateKey::try_new([42; 32]).expect("key");
    let creator_token_id = id_of(&creator_token_key);
    let creator = id_of(&creator_key);
    let i = ids(creator);
    let end_ms = 500_000_u64;

    let mut state = testnet_initial_state::initial_state();
    deploy(&mut state);
    seed_create_fixture(&mut state, &i, creator_token_id);

    // --- 0. THE ATTACK, at the only moment it is still free -------------------
    // A stranger dusts the fee sink this creator announced. That id can now never
    // be initialised at all - `token::InitializeAccount` asserts its target is
    // `Account::default()` WHOLE, exactly as a claim does - so the old build
    // would have pinned into the sale a treasury that could never receive a
    // token, and the escrow would have been dead before the first fee accrued.
    // Creation refuses it instead, and all the creator has lost is one
    // transaction and the id: they name an initialised holding and carry on. THIS
    // assert is what the rest of the test rests on, and the one whose removal
    // brings the fund-lock back.
    let dusted_id = AccountId::new([29; 32]);
    state.force_insert_account(dusted_id, Account { balance: 1, ..Account::default() });
    let m = create_msg(
        &i,
        &CreateArgs {
            treasury: dusted_id,
            treasury_id: dusted_id,
            end_timestamp_ms: end_ms,
            ..CreateArgs::standard(&i, creator_token_id)
        },
    );
    let w = public_transaction::WitnessSet::for_message(&m, &[&creator_token_key, &creator_key]);
    let result = state.transition_from_public_transaction(&PublicTransaction::new(m, w), 0, 0);
    let Err(LeeError::ProgramExecutionFailed(err)) = result else {
        panic!("a create pinning a dusted, never-initialised treasury must revert, got {result:?}");
    };
    assert!(
        err.contains("CreateSale: treasury: expected a valid Token Holding account"),
        "the create must refuse the dusted fee sink: {err}"
    );
    assert_eq!(
        state.get_account_by_id(i.sale),
        Account::default(),
        "and no sale exists to carry an unsettleable escrow"
    );

    // --- 1. the create the creator can actually make --------------------------
    // A REAL create against the initialised treasury, so the pin is genuinely
    // exercised and everything below runs against a sale this program built.
    let m = create_msg(
        &i,
        &CreateArgs {
            end_timestamp_ms: end_ms,
            ..CreateArgs::standard(&i, creator_token_id)
        },
    );
    let w = public_transaction::WitnessSet::for_message(&m, &[&creator_token_key, &creator_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, w), 0, 0)
        .expect("create_sale must succeed against an initialised treasury");

    // --- 2. a PUBLIC buy: fee swept to the treasury in the same transaction ---
    let buyer_key = PrivateKey::try_new([60; 32]).unwrap();
    let buyer_coll = id_of(&buyer_key);
    let buyer_tok = AccountId::new([61; 32]);
    state.force_insert_account(buyer_coll, fungible(i.collateral_def, 100_000));
    state.force_insert_account(buyer_tok, fungible(i.token_def, 0));
    let public_in: u128 = 5_000;
    let (public_out, public_fee, public_c_eff) = buy_tokens_out(VT, VC, FEE_BPS, public_in);
    assert!(public_out > 0 && public_fee > 0, "the public buy must move the curve and the fee");

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
        Instruction::Buy { collateral_in: public_in, min_tokens_out: 0, deadline: u64::MAX },
    )
    .unwrap();
    let w = public_transaction::WitnessSet::for_message(&buy, &[&buyer_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(buy, w), 1, 0)
        .expect("the public buy must be included");

    let sale = SaleState::try_from(&state.get_account_by_id(i.sale).data).unwrap();
    assert_eq!(bal(&state, i.treasury), public_fee, "a public buy pays the treasury at once");
    assert_eq!(sale.treasury_owed, 0, "...and escrows nothing");
    assert_eq!(
        bal(&state, i.collateral_vault),
        sale.real_collateral + sale.treasury_owed,
        "invariant after the public buy"
    );

    // --- 3. a PRIVATE buy: fee escrowed in the vault, treasury not named ------
    // Priced off the LIVE reserves, which the public buy just moved: the
    // disposable buy is proved against this state, so anything else would be
    // asserting the fixture's arithmetic rather than the program's.
    let (private_out, private_fee, private_c_eff) =
        buy_tokens_out(sale.virt_token, sale.virt_collateral, FEE_BPS, DISPOSABLE_IN);
    assert!(private_fee > 0, "the test must exercise a non-zero escrow");

    let keys = buyer_keys();
    let (mut state, note) = fund_private_collateral(state, &i, &keys, 100_000);
    let tx = prove_disposable(
        &state,
        &i,
        &keys,
        &note,
        Instruction::BuyDisposable {
            collateral_in: DISPOSABLE_IN,
            min_tokens_out: 0,
            not_before_ms: 100_000,
            deadline: u64::MAX,
        },
    )
    .expect("proving must succeed");
    state
        .transition_from_privacy_preserving_transaction(&tx, 2, 100_000)
        .expect("the disposable buy must be included");

    let sale = SaleState::try_from(&state.get_account_by_id(i.sale).data).unwrap();
    assert_eq!(sale.treasury_owed, private_fee, "the private fee is escrowed, not paid");
    assert_eq!(bal(&state, i.treasury), public_fee, "and the treasury is untouched by it");
    assert_eq!(
        bal(&state, i.collateral_vault),
        public_c_eff + DISPOSABLE_IN,
        "the disposable buy moves its FULL gross collateral into the vault"
    );
    assert_eq!(
        bal(&state, i.collateral_vault),
        sale.real_collateral + sale.treasury_owed,
        "invariant across the mixed public/private history"
    );

    // --- 4. THE ATTACK AGAIN, on the treasury that WAS initialised ------------
    // The same dust, mid-sale, on the account this sale actually pinned. It is
    // harmless now: the account is already a holding the token program owns, so
    // the fee leg writes to it instead of claiming it, and the native balance is
    // simply carried through.
    let dusted = Account { balance: 1, ..state.get_account_by_id(i.treasury) };
    state.force_insert_account(i.treasury, dusted);

    // --- 5. close, withdraw, sweep -------------------------------------------
    set_clock(&mut state, 3, i64::try_from(end_ms).unwrap());
    let close = public_transaction::Message::try_new(
        bc(),
        vec![i.sale, creator, CLOCK_01],
        vec![Nonce(1)], // the creator already signed the create
        Instruction::CloseSale { deadline: u64::MAX },
    )
    .unwrap();
    let w = public_transaction::WitnessSet::for_message(&close, &[&creator_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(close, w), 3, end_ms)
        .expect("the creator must be able to close at the end timestamp");

    let creator_coll = AccountId::new([90; 32]);
    let creator_tok = AccountId::new([91; 32]);
    state.force_insert_account(creator_coll, fungible(i.collateral_def, 0));
    state.force_insert_account(creator_tok, fungible(i.token_def, 0));
    let withdraw = public_transaction::Message::try_new(
        bc(),
        vec![i.sale, i.token_vault, i.collateral_vault, creator_coll, creator_tok, creator],
        vec![Nonce(2)], // create, then close
        Instruction::Withdraw { deadline: u64::MAX },
    )
    .unwrap();
    let w = public_transaction::WitnessSet::for_message(&withdraw, &[&creator_key]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(withdraw, w), 4, end_ms)
        .expect("withdraw must succeed");

    assert_eq!(
        bal(&state, creator_coll),
        public_c_eff + private_c_eff,
        "the creator is paid both buys' collateral net of both fees"
    );
    assert_eq!(
        bal(&state, creator_tok),
        D + R - public_out - private_out,
        "and every unsold project token"
    );
    assert_eq!(
        bal(&state, i.collateral_vault),
        private_fee,
        "the escrow stays behind in the vault: only the private fee is still owed"
    );

    let m = public_transaction::Message::try_new(
        bc(),
        vec![i.sale, i.collateral_vault, i.treasury],
        vec![],
        Instruction::SweepTreasury { deadline: u64::MAX },
    )
    .unwrap();
    let w = public_transaction::WitnessSet::for_message(&m, &[]);
    state
        .transition_from_public_transaction(&PublicTransaction::new(m, w), 5, end_ms)
        .expect(
            "the permissionless sweep must settle into a DUSTED treasury - if this reverts, \
             the fund-lock a stranger could impose for the price of a dust transfer is back",
        );

    assert_eq!(
        bal(&state, i.treasury),
        public_fee + private_fee,
        "the treasury ends up with both fees: the public one it was paid at buy time, and the \
         escrowed private one the sweep delivered"
    );
    assert_eq!(bal(&state, i.collateral_vault), 0, "the collateral vault is finally drained");
    let sale = SaleState::try_from(&state.get_account_by_id(i.sale).data).unwrap();
    assert_eq!(sale.treasury_owed, 0, "and nothing is left owed");
    assert_eq!(
        state.get_account_by_id(i.treasury).balance,
        1,
        "the dust is still there and still irrelevant - the sweep wrote the holding's data and \
         carried the native balance through, which is exactly why an INITIALISED treasury \
         cannot be griefed"
    );
}
