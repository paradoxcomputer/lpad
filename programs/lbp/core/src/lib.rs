//! Core types, weighted-pool math, and PDA derivation for the LPAD **LBP**
//! launchpad program (RFP-016).
//!
//! Pricing is a Balancer-style weight-shifting AMM: token weight declines
//! linearly over the sale window, so the price falls over time unless buying
//! pressure counteracts it. The out-given-in formula uses the integer Q64.64
//! power in [`fixed`]. Unlike the bonding curve, the protocol fee is charged
//! **at close** rather than per swap (the sale is time-bounded, so it is always
//! chargeable) - and charging it means escrowing it in the collateral vault as
//! [`PoolState::treasury_owed`], which [`Instruction::SweepTreasury`] later hands
//! to the treasury. `CreateSale` pins that treasury to an already-initialised
//! holding of the collateral, and `Withdraw` deliberately pays no treasury
//! account, so a treasury that cannot receive is both unconstructible and unable
//! to hold the creator's payout hostage even if it were.

pub mod fixed;

use borsh::{BorshDeserialize, BorshSerialize};
use lee_core::{
    account::{AccountId, AccountWithMetadata, Data},
    program::{PdaSeed, ProgramId},
};
use serde::{Deserialize, Serialize};

use fixed::{div_q64, div_to_q64, mul_shr_64, mul_shr_64_checked, pow_q64, ONE};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

pub const FEE_BPS_DENOMINATOR: u128 = 10_000;
/// Maximum at-close fee an admin may configure (10%).
pub const MAX_FEE_BPS: u128 = 1_000;
pub const ORACLE_RING_CAP: usize = 64;

/// Maximum byte length of the self-describing `token_name`/`token_symbol`
/// metadata copied into the pool state. Bounded at creation so the serialized
/// state stays well under `Data`'s `DATA_MAX_LENGTH` (100 KiB) even once the
/// 64-entry observation ring fills - an unbounded creator-set string could
/// otherwise grow the state across that cap mid-life, panicking the
/// `From<&PoolState> for Data` encode and reverting every buy/sell/close.
/// Mirrors `bonding_curve_core::MAX_METADATA_LEN`.
pub const MAX_METADATA_LEN: usize = 64;

/// Maximum length of a `BuyGated` Merkle inclusion proof (tree depth). A depth
/// of 32 supports a 2^32-member allowlist - far beyond any realistic sale -
/// while bounding the caller-supplied `proof: Vec<[u8; 32]>` so an attacker
/// cannot inflate guest cycle count / proving cost with junk siblings: each
/// extra entry forces one more SHA-256 in [`merkle_verify`]. Enforced before
/// verification on the on-chain path (`buy::buy_gated`) and mirrored
/// client-side in the SDK.
pub const MAX_ALLOWLIST_PROOF_DEPTH: usize = 32;

/// Maximum sale-schedule span (`t_end_ms - t_start_ms`), ~10 years in ms. The
/// linear weight interpolation in [`weight_token_q64`] forms `delta * elapsed`
/// with `delta` up to ~2^64 (Q64.64 weight gap) and `elapsed` up to the span;
/// capping the span keeps that product far below `i128::MAX`, so it can never
/// wrap (the guest runs in release with overflow-checks OFF - see
/// [`fixed::div_to_q64`]). `create_sale` rejects a longer span at creation.
pub const MAX_DURATION_MS: u64 = 10 * 365 * 24 * 60 * 60 * 1_000;

/// Widest timestamp validity window a `BuyDisposable` output may carry (1 hour).
///
/// A private buy is priced at the caller-supplied `t_buy_ms`, so the window is
/// the only thing binding that argument to real time: the sequencer admits the
/// transaction iff `t_buy_ms <= now < hi` (`LeeError::OutOfValidityWindow`), and
/// the guest has no clock to check it against. Capping the span bounds how stale
/// the quoted price may be when the proof finally lands. It is deliberately far
/// wider than a public buy needs - a real private-buy STARK takes minutes to
/// produce - and still short enough that the quote is recognisably current.
pub const MAX_PRIVATE_WINDOW_MS: u64 = 3_600_000;

/// Canonical on-chain Clock account (sequencer-updated each block).
pub const CLOCK_01: AccountId = AccountId::new(*b"/LEZ/ClockProgramAccount/0000001");

const TOKEN_VAULT_TAG: &[u8; 16] = b"lbp/token_vault\0";
const COLLATERAL_VAULT_TAG: &[u8; 16] = b"lbp/coll_vault\0\0";
const WEIGHT_OBS_TAG: &[u8; 16] = b"lbp/weight_obs\0\0";
const CREATOR_INDEX_TAG: &[u8; 16] = b"lbp/creator_idx\0";

/// First eight bytes of every [`CreatorIndex`] account. `PoolState` carries no
/// discriminant because nothing else is ever stored at a pool PDA, but this
/// account is *read speculatively* - the SDK derives the index PDA for a wallet
/// and reads whatever is there - so it has to be able to say "this is not one of
/// mine" rather than decode some other account's bytes into a plausible-looking
/// list of pool ids. Distinct from the bonding curve's `lpad/bci`.
pub const CREATOR_INDEX_MAGIC: [u8; 8] = *b"lpad/lbi";

/// Layout version of [`CreatorIndex`]. Bumped only for a breaking field change;
/// [`CreatorIndex::try_from`] refuses anything it does not recognise, so an old
/// reader fails loudly instead of mis-parsing a newer index.
pub const CREATOR_INDEX_VERSION: u16 = 1;

/// Maximum number of pool ids one creator's index may hold.
///
/// `Data` caps an account at `DATA_MAX_LENGTH` (100 KiB). A full index encodes
/// to `8 + 2 + 32 + 4 + 32*n` bytes, so 3,000 ids is 96,046 - comfortably under
/// the cap with room for a future field. The bound is asserted in
/// [`CreatorIndex::push`] rather than left to the encode, because the encode's
/// failure is a `DataTooBigError` panic from inside `From<&CreatorIndex> for
/// Data` that says nothing about which account or what to do next.
///
/// Mirrors `bonding_curve_core::MAX_INDEXED_SALES`, ceiling included.
pub const MAX_INDEXED_POOLS: usize = 3_000;

/// Local mirror of the sequencer `ClockAccountData` (Borsh: u64 block id, i64 ms).
#[derive(Clone, Copy, BorshDeserialize)]
pub struct ClockData {
    pub block_id: u64,
    pub timestamp: i64,
}

/// Parse the sequencer Clock account `data` into wall-clock ms (0 if absent/invalid).
/// Used off-chain (SDK/CLI) to quote a time-priced LBP buy at the current time.
#[must_use]
pub fn clock_ms(data: &Data) -> i64 {
    <ClockData as BorshDeserialize>::try_from_slice(data.as_ref())
        .map(|c| c.timestamp)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub enum SaleStatus {
    #[default]
    Open,
    Closed,
}

#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct PriceObservation {
    pub ts_ms: u64,
    pub price_q64: u128,
}

/// The stored weight `Poke` advances, in its own per-pool PDA rather than in
/// [`PoolState`].
///
/// It lives here and not in the pool account because `Poke` is permissionless
/// and economically inert, while a `BuyDisposable` proof PINS the pool account
/// byte-for-byte and the sequencer re-verifies it against live state at
/// inclusion: a poke that wrote into the pool would invalidate every in-flight
/// private buy for the price of one cheap transaction. Split out, a private buy
/// never has to declare this account at all.
///
/// Claimed lazily on the first `Poke` - the account simply does not exist before
/// then. A consumer that finds it absent (or stale) recomputes the weight from
/// the schedule with [`PoolState::weight_token_q64`]; this is a convenience for
/// off-chain readers and is never an input to pricing.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct WeightObs {
    /// Token weight (Q64.64) at `ts_ms`.
    pub w_token_q64: u128,
    /// Clock timestamp (ms) the weight was advanced to.
    pub ts_ms: u64,
}

/// Per-creator index of the pools that creator has created: ONE account per
/// (program, creator), appended to by `CreateSale`.
///
/// This exists so discovery is a single read per creator. Without it the SDK has
/// to re-derive every pool PDA the wallet could possibly have made - a product
/// over (creator account, token definition, collateral definition, nonce), each
/// arm of which is a network round-trip - which measured at ~4,800 reads and
/// tens of minutes on a wallet with history. (LP-0012, the event indexer, does
/// not exist on this network.)
///
/// IDS ONLY, deliberately. Pool state (reserves, status, the observation ring)
/// changes on every buy, so an index of state would be stale the moment it was
/// written; an id is permanent. Readers resolve the ids against the pool
/// accounts.
///
/// Pools created before this account existed appear in no index, which is why
/// the SDK keeps the brute-force derivation as a FALLBACK rather than deleting
/// it.
///
/// Mirrors `bonding_curve_core::CreatorIndex` field for field, with `pool_ids`
/// where that one has `sale_ids`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct CreatorIndex {
    /// [`CREATOR_INDEX_MAGIC`]. Checked on decode.
    pub magic: [u8; 8],
    /// [`CREATOR_INDEX_VERSION`]. Checked on decode.
    pub version: u16,
    /// The creator this index belongs to. Redundant with the PDA derivation and
    /// kept anyway: it makes a decoded index self-describing, and it is what
    /// `create_sale` re-checks before appending, so two creators' lists can never
    /// be merged by a derivation change or a seed collision.
    pub creator: AccountId,
    /// Pool PDAs this creator has created, oldest first. Never removed: a closed
    /// pool is still one the creator created and still worth listing.
    pub pool_ids: Vec<AccountId>,
}

impl CreatorIndex {
    /// A fresh, empty index for `creator`.
    ///
    /// Deliberately not `Default`: a defaulted `CreatorIndex` would carry a zero
    /// magic and so could never be decoded back, which is a trap worth not
    /// leaving lying around.
    #[must_use]
    pub fn new(creator: AccountId) -> Self {
        Self {
            magic: CREATOR_INDEX_MAGIC,
            version: CREATOR_INDEX_VERSION,
            creator,
            pool_ids: Vec::new(),
        }
    }

    /// Append a newly created pool id, enforcing [`MAX_INDEXED_POOLS`].
    ///
    /// No dedup check: the caller only ever appends a pool PDA it has just
    /// asserted was uninitialized, and a pool PDA is unique per
    /// `(token_def, collateral_def, creator, nonce)`, so the same id cannot be
    /// created twice.
    ///
    /// Unlike the ownership assert in `create_sale`'s else branch, "use a
    /// different creator account" really is the remedy here, so the message says
    /// so - in the same words as `bonding_curve_core::CreatorIndex::push`.
    pub fn push(&mut self, pool_id: AccountId) {
        assert!(
            self.pool_ids.len() < MAX_INDEXED_POOLS,
            "creator pool index is full - create further pools from a different creator account"
        );
        self.pool_ids.push(pool_id);
    }
}

#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct PoolState {
    // ---- identity / config ----
    pub creator: AccountId,
    pub token_definition_id: AccountId,
    pub collateral_definition_id: AccountId,
    pub token_vault_id: AccountId,
    pub collateral_vault_id: AccountId,
    pub treasury_id: AccountId,
    /// ATA program pinned at creation. `BuyAta` asserts the submitter's
    /// `ata_program_id` equals this, so the buyer's collateral leg cannot be
    /// dispatched to a substitute (e.g. no-op) program that skips the real
    /// `token::Transfer` while the vault still delivers tokens.
    pub ata_program_id: ProgramId,
    pub fee_bps: u128, // at-close fee on collateral raised
    // ---- weight schedule (Q64.64 token-weight fractions in (0,1)) ----
    pub w_start_q64: u128,
    pub w_end_q64: u128,
    pub t_start_ms: u64,
    pub t_end_ms: u64,
    pub min_duration_ms: u64,
    // ---- reserves ----
    pub reserve_token: u128,
    pub reserve_collateral: u128,
    /// At-close protocol fee escrowed *inside the collateral vault*, owed to the
    /// treasury and settled by [`Instruction::SweepTreasury`]. `Withdraw` credits
    /// this bucket instead of paying the treasury in the same transaction as the
    /// creator's principal: a treasury that could not receive would otherwise
    /// revert the creator's payout along with it and lock the entire raise plus
    /// every unsold token. Settled on its own, that same failure would strand the
    /// fee and nothing else.
    ///
    /// INVARIANT: `collateral_vault_balance == reserve_collateral +
    /// treasury_owed`. While the sale runs it holds trivially - there is no
    /// per-swap fee, so this stays 0 and the vault is exactly
    /// `reserve_collateral` - and `Withdraw` preserves it by zeroing
    /// `reserve_collateral` in the same step that credits the fee here and leaves
    /// exactly that much collateral behind in the vault.
    ///
    /// WHY THE TREASURY IS PINNED AT CREATION - nothing ever rewrites
    /// `treasury_id`, and [`Instruction::SweepTreasury`] is the only instruction
    /// that can pay this bucket out, so a treasury that cannot receive strands it
    /// **forever**. `CreateSale` therefore takes the treasury as an ACCOUNT and
    /// requires it to be an already-initialised `Fungible` holding of the pool's
    /// collateral definition, owned by that definition's own token program. That
    /// rejects, at the one moment the creator can still fix it for free, each
    /// shape that could never receive:
    ///   * **uninitialised** - the fee leg would have to CLAIM it with
    ///     `Claim::Authorized`, which LEZ admits only for an account that is
    ///     itself authorized, and [`Instruction::SweepTreasury`] is
    ///     permissionless with no signer slot at all - so nothing could supply
    ///     that. A claim also needs the pre-state to be `Account::default()`
    ///     WHOLE, which the treasury's own key destroys by signing anything at
    ///     all (LEZ bumps a signer's nonce even when its account is unowned). Not
    ///     a third-party grief: nobody can modify a DEFAULT-owned account they
    ///     cannot claim, balance included, so the id cannot be dusted from
    ///     outside. Requiring an initialised holding removes the branch rather
    ///     than documenting it.
    ///   * **wrong definition, or a holding under a different token program** -
    ///     the fee leg is dispatched on the collateral vault's program and moves
    ///     the vault's definition, so such a recipient reverts every time and no
    ///     signature changes that.
    ///
    /// The pin cannot decay: LEZ has no instruction that un-initialises a holding
    /// (`burn` only reduces the balance), so a treasury that passed `CreateSale`
    /// is still a valid recipient at every later sweep.
    /// Mirrors `bonding_curve_core::SaleState::treasury_owed`.
    pub treasury_owed: u128,
    // ---- poke convenience: NOT stored here, see `WeightObs` ----
    // `Poke` is permissionless. Writing its output into this account would let
    // anyone invalidate every in-flight `BuyDisposable` for the price of one
    // cheap transaction that changes nothing economically, because a private buy
    // pins the pool account byte-for-byte. The stored weight therefore lives in
    // the per-pool PDA behind `compute_weight_obs_pda_seed`.
    // ---- controls ----
    pub paused: bool,
    pub block_token_ceiling: u128, // 0 = none
    pub block_sold: u128,
    pub block_window_id: u64,
    pub allowlist_root: [u8; 32], // all-zero = open sale
    pub fixed_price: bool,
    pub fixed_price_q64: u128, // constant collateral-per-token price (fixed_price mode)
    pub nonce: u64,
    pub created_ts_ms: i64,
    pub status: SaleStatus,
    // ---- analytics ----
    pub cum_collateral_in: u128,
    pub cum_tokens_out: u128,
    pub buy_count: u64,
    pub obs: Vec<PriceObservation>,
    // ---- self-describing metadata (copied from the project token at creation) ----
    /// Token display name, mirrored on-chain so the pool is self-describing
    /// without an off-chain metadata lookup. Empty if minted without metadata.
    pub token_name: String,
    /// Token ticker symbol (same provenance as `token_name`).
    pub token_symbol: String,
}

impl PoolState {
    /// Token weight at `t_ms` (Q64.64), clamped to the schedule. Constant in
    /// fixed-price mode.
    #[must_use]
    pub fn weight_token_q64(&self, t_ms: u64) -> u128 {
        if self.fixed_price {
            return self.w_start_q64;
        }
        weight_token_q64(self.w_start_q64, self.w_end_q64, self.t_start_ms, self.t_end_ms, t_ms)
    }

    /// Spot price (Q64.64, collateral per token) at `t_ms`.
    #[must_use]
    pub fn spot_price_q64(&self, t_ms: u64) -> u128 {
        if self.fixed_price {
            return self.fixed_price_q64;
        }
        spot_price_q64(self.reserve_token, self.reserve_collateral, self.weight_token_q64(t_ms))
    }

    pub fn record_observation(&mut self, t_ms: u64) {
        self.obs.push(PriceObservation { ts_ms: t_ms, price_q64: self.spot_price_q64(t_ms) });
        let n = self.obs.len();
        if n > ORACLE_RING_CAP {
            self.obs.drain(0..n - ORACLE_RING_CAP);
        }
    }
}

// ---------------------------------------------------------------------------
// Instruction set
// ---------------------------------------------------------------------------

// APPEND ONLY. The wire discriminant is the declaration index, so inserting or
// reordering a variant silently re-points every already-encoded instruction at a
// different arm - and nothing downstream would fail to decode. `Pause` and
// `Resume` are the sharp edge: they are byte-identical on the wire apart from
// that index and take identical account lists, so swapping them would resume a
// paused sale (or pause a live one) with no error anywhere.
#[derive(Serialize, Deserialize)]
// CreateSale is much larger than the hot Buy/lifecycle variants, but it is
// constructed once per sale (cold) and the enum is serde-only (no fixed wire
// layout), so boxing it is not worth it - and the #[lez_program] guest dispatcher
// destructures CreateSale by named fields, which a Box<...> tuple variant breaks.
#[allow(clippy::large_enum_variant)]
pub enum Instruction {
    /// Account order: `[pool, token_vault, collateral_vault, token_definition,
    /// collateral_definition, creator_token_holding, creator_collateral_holding,
    /// creator, treasury, creator_index, clock]`.
    ///
    /// `creator_index` is the creator's [`CreatorIndex`] PDA - claimed here on
    /// the creator's first pool, appended to on every later one, and pinned to
    /// `compute_creator_index_pda(self_program_id, creator)` so no one can append
    /// into another creator's list. It is what makes pool discovery one read per
    /// creator; pools created before this account existed appear in no index, so
    /// a reader still needs the brute-force derivation as a fallback.
    CreateSale {
        collateral_definition_id: AccountId,
        /// The sale's fee sink. Also passed as an ACCOUNT (the `treasury` slot),
        /// which `create_sale` binds to this id and requires to be an
        /// already-initialised `Fungible` holding of the collateral definition -
        /// see [`PoolState::treasury_owed`] for why nothing weaker is settleable.
        treasury_id: AccountId,
        token_name: String,
        token_symbol: String,
        token_deposit: u128,
        collateral_seed: u128, // initial collateral reserve seeded by creator (>=1 to anchor price)
        w_start_q64: u128,
        w_end_q64: u128,
        t_start_ms: u64,
        t_end_ms: u64,
        fee_bps: u128,
        block_token_ceiling: u128,
        allowlist_root: [u8; 32],
        fixed_price: bool,
        min_duration_ms: u64,
        nonce: u64,
        /// ATA program to pin into the pool (asserted by `BuyAta`).
        ata_program_id: ProgramId,
        deadline: u64,
    },
    /// Public buy (keypair holdings), priced at the on-chain clock time.
    Buy {
        collateral_in: u128,
        min_tokens_out: u128,
        deadline: u64,
    },
    /// Public buy gated by a ZK/Merkle allowlist inclusion proof.
    BuyGated {
        collateral_in: u128,
        min_tokens_out: u128,
        leaf: [u8; 32],
        proof: Vec<[u8; 32]>,
        deadline: u64,
    },
    /// Public buy where the user side uses **Associated Token Accounts** (RFP-016
    /// Func #9 - ATAs for all token interactions, per LP-0014 / RFP-008). The
    /// buyer's collateral leg is dispatched through the ATA program; tokens are
    /// delivered into the buyer's token ATA. `ata_program_id` is required to
    /// dispatch the chained `ata::Transfer` (the ATA's `program_owner` is the
    /// token program, so the dispatch target cannot be read from it).
    BuyAta {
        collateral_in: u128,
        min_tokens_out: u128,
        ata_program_id: ProgramId,
        deadline: u64,
    },
    /// Advance the stored weight for off-chain consumers (idempotent; pricing is lazy).
    Poke { deadline: u64 },
    /// Emergency stop (creator). Does NOT halt weight progression.
    Pause { deadline: u64 },
    Resume { deadline: u64 },
    /// Close after the end timestamp.
    CloseSale { deadline: u64 },
    /// Creator withdrawal: collateral net of the at-close fee, plus every unsold
    /// project token.
    ///
    /// The fee is ESCROWED into [`PoolState::treasury_owed`] and left sitting in
    /// the collateral vault for [`Instruction::SweepTreasury`], not paid here -
    /// a treasury that cannot receive would otherwise take the creator's whole
    /// payout down with it (see [`PoolState::treasury_owed`]).
    ///
    /// Account order: `[pool, token_vault, collateral_vault,
    /// creator_collateral_holding, creator_token_holding, creator]` - note that
    /// it names no treasury account at all.
    Withdraw { deadline: u64 },
    /// Private buy: the buyer's collateral holding and token holding are PRIVATE
    /// account slots, so the debit and the credit are private notes inside one
    /// proof. "Disposable" names the single-use NOTE, not an account - there is
    /// no ephemeral account, no deshield leg and no re-shield leg. Privacy on
    /// LEZ is a per-slot label positionally aligned 1:1 with the guest's
    /// `pre_states`, so these are simply private slots of an otherwise ordinary
    /// buy. (The older design documents in `docs/` describe a 5-leg
    /// ephemeral-account router that this deliberately does not build.)
    ///
    /// A privacy transaction cannot carry the Clock account - every public
    /// account in one is pinned byte-for-byte at proving time and re-verified
    /// against live state at inclusion, and `CLOCK_01`'s data is rewritten every
    /// block - so the price is taken at the caller-supplied `t_buy_ms` and bound
    /// to real time by the output's timestamp validity window
    /// (see [`private_window_hi`]).
    ///
    /// Account order: `[pool, token_vault, collateral_vault,
    /// buyer_collateral_holding, buyer_token_holding]`.
    BuyDisposable {
        collateral_in: u128,
        min_tokens_out: u128,
        t_buy_ms: u64,
        deadline: u64,
    },
    /// Pay the at-close fee that [`Instruction::Withdraw`] escrowed out of the
    /// collateral vault to the pool's pinned treasury, clearing
    /// [`PoolState::treasury_owed`] by exactly the amount that moved.
    ///
    /// Account order: `[pool, collateral_vault, treasury]`.
    ///
    /// **Permissionless, and that is the intent.** No signature is required: the
    /// only effect available to a submitter is moving the owed fee from the vault
    /// to the `treasury_id` the pool pinned at creation, so a stranger who sends
    /// this hands the fee to its rightful owner and pays the gas for the
    /// privilege. A creator/admin signature would buy nothing and would add a
    /// party able to withhold settlement.
    ///
    /// Not even the treasury's own signature is needed: `CreateSale` pins an
    /// already-initialised holding, so the fee leg never has to create its
    /// recipient (see [`PoolState::treasury_owed`]).
    ///
    /// Deliberately NOT a leg of `Withdraw` even so: if the creation-time pin were
    /// ever wrong, a treasury that cannot receive would revert the creator's
    /// payout with it and lock the whole raise.
    SweepTreasury { deadline: u64 },
}

/// Exclusive upper bound of the timestamp validity window a `BuyDisposable`
/// output carries: the earliest of the caller's `deadline`, `t_buy_ms +
/// MAX_PRIVATE_WINDOW_MS`, and the sale's own `t_end_ms`.
///
/// Clamping to `t_end_ms` is the ONLY end-of-sale check on this path: the guest
/// has no clock account to compare against, so a window that outlived the sale
/// would let a proof built before `t_end_ms` settle after it. Clamping to the
/// caller's `deadline` keeps the usual promise that a transaction expires when
/// the submitter said it would; the guest must never echo that deadline
/// unclamped, or the other two bounds do nothing.
///
/// `saturating_add` so a `t_buy_ms` near `u64::MAX` cannot wrap the staleness
/// cap into a bound *below* `t_buy_ms` (the guest runs with overflow-checks
/// off). The caller must still assert `t_buy_ms < hi`: all three bounds can sit
/// at or below `t_buy_ms`, and a `from >= to` range is an empty window.
#[must_use]
pub fn private_window_hi(deadline: u64, t_buy_ms: u64, t_end_ms: u64) -> u64 {
    deadline.min(t_buy_ms.saturating_add(MAX_PRIVATE_WINDOW_MS)).min(t_end_ms)
}

// ---------------------------------------------------------------------------
// Weighted-pool math
// ---------------------------------------------------------------------------

/// Linear token-weight interpolation, clamped to `[t_start, t_end]`.
#[must_use]
pub fn weight_token_q64(
    w_start_q64: u128,
    w_end_q64: u128,
    t_start_ms: u64,
    t_end_ms: u64,
    t_ms: u64,
) -> u128 {
    if t_ms <= t_start_ms {
        return w_start_q64;
    }
    if t_ms >= t_end_ms {
        return w_end_q64;
    }
    let dur = (t_end_ms - t_start_ms) as i128;
    let elapsed = (t_ms - t_start_ms) as i128;
    let delta = w_end_q64 as i128 - w_start_q64 as i128;
    // `delta * elapsed` can reach ~2^64 * span; an out-of-domain span (`> 2^63`)
    // would overflow i128 and silently WRAP under release overflow-checks. Revert
    // cleanly instead - `create_sale` caps the span at `MAX_DURATION_MS`, so this
    // never trips for a well-formed pool. (Mirrors `fixed::div_to_q64`'s hard
    // domain assert.)
    let w = w_start_q64 as i128
        + delta.checked_mul(elapsed).expect("weight overflow") / dur;
    w as u128
}

/// Spot price (Q64.64, collateral per token):
/// `(reserve_collateral * w_token) / (reserve_token * w_collateral)`.
#[must_use]
pub fn spot_price_q64(reserve_token: u128, reserve_collateral: u128, w_token_q64: u128) -> u128 {
    let w_collateral_q64 = ONE - w_token_q64;
    if reserve_token == 0 || w_collateral_q64 == 0 {
        return 0;
    }
    let rc_over_rt = div_to_q64(reserve_collateral, reserve_token);
    let weight_ratio = div_q64(w_token_q64, w_collateral_q64);
    // The product can exceed the Q64.64 u128 range when the token reserve is
    // drained low while the token weight stays high (e.g. fixed-price mode), which
    // would silently WRAP and poison the on-chain observation ring this feeds.
    // Saturate to the max representable price instead of recording a wrapped value.
    mul_shr_64_checked(rc_over_rt, weight_ratio).unwrap_or(u128::MAX)
}

/// Exclusive upper bound for raw reserve / collateral amounts: the Q64.64 price
/// math left-shifts these by 64, so any value `>= 2^64` would overflow `u128`.
/// Buys/creates that would exceed it revert (see [`fixed::div_to_q64`]).
pub const MAX_RESERVE: u128 = 1u128 << 64;

/// Balancer out-given-in. `tokens_out` is floored against the trader. No
/// per-swap fee (LBP fee is collected at close).
///
/// `tokens_out = reserve_token * (1 - (rc / (rc + C_in)) ^ (w_collateral / w_token))`.
///
/// **Non-decreasing in time at fixed reserves.** As `t` rises the token weight
/// falls, so the exponent `w_collateral / w_token` rises; the base
/// `rc / (rc + C_in)` is in `(0,1)`, so `base^x` falls and `tokens_out` rises.
/// `BuyDisposable` prices at a caller-chosen `t_buy_ms` and leans on exactly
/// that: an earlier timestamp yields FEWER tokens than the fair amount at
/// admission, so the pool is never underpaid. Pinned by
/// `proptests::tokens_out_non_decreasing_in_time`; if that test ever fails,
/// `t_buy_ms` is unsound and `BuyDisposable` must be pulled.
#[must_use]
pub fn buy_tokens_out(
    reserve_token: u128,
    reserve_collateral: u128,
    w_token_q64: u128,
    collateral_in: u128,
) -> u128 {
    assert!(w_token_q64 > 0 && w_token_q64 < ONE, "weight must be in (0,1)");
    // Keep reserves inside the Q64.64 numerator domain so the price math cannot
    // silently overflow and misprice (vault-drain class bug). Reject cleanly.
    let total_collateral = reserve_collateral
        .checked_add(collateral_in)
        .expect("collateral reserve overflow");
    assert!(
        total_collateral < MAX_RESERVE,
        "reserve exceeds Q64.64 domain"
    );
    let w_collateral_q64 = ONE - w_token_q64;
    let base = div_to_q64(reserve_collateral, reserve_collateral + collateral_in);
    let exponent = div_q64(w_collateral_q64, w_token_q64);
    let powv = pow_q64(base, exponent); // in (0,1]
    mul_shr_64(reserve_token, ONE - powv)
}

/// Fixed-price-mode output: `floor(collateral_in / price)`.
#[must_use]
pub fn fixed_price_tokens_out(collateral_in: u128, price_q64: u128) -> u128 {
    assert!(price_q64 > 0, "fixed price must be positive");
    assert!(collateral_in < MAX_RESERVE, "input exceeds Q64.64 domain");
    div_to_q64(collateral_in, price_q64)
}

/// At-close protocol fee on the raised collateral, rounded up (RFP-016).
#[must_use]
pub fn close_fee(collateral_balance: u128, fee_bps: u128) -> u128 {
    if collateral_balance == 0 || fee_bps == 0 {
        return 0;
    }
    collateral_balance
        .checked_mul(fee_bps)
        .expect("fee overflow")
        .div_ceil(FEE_BPS_DENOMINATOR)
}

// ---------------------------------------------------------------------------
// Allowlist (ZK set-membership via sorted-pair Merkle inclusion)
// ---------------------------------------------------------------------------

fn hash32(bytes: &[u8]) -> [u8; 32] {
    use risc0_zkvm::sha::{Impl, Sha256};
    Impl::hash_bytes(bytes).as_bytes().try_into().expect("32-byte hash")
}

/// Verify a sorted-pair Merkle inclusion proof of `leaf` against `root`.
/// Sorting each pair removes the need for direction bits and avoids
/// second-preimage ambiguity.
#[must_use]
pub fn merkle_verify(leaf: [u8; 32], proof: &[[u8; 32]], root: [u8; 32]) -> bool {
    let mut node = leaf;
    for sib in proof {
        let mut buf = [0u8; 64];
        if node <= *sib {
            buf[0..32].copy_from_slice(&node);
            buf[32..64].copy_from_slice(sib);
        } else {
            buf[0..32].copy_from_slice(sib);
            buf[32..64].copy_from_slice(&node);
        }
        node = hash32(&buf);
    }
    node == root
}

/// True when the sale has no allowlist (open participation).
#[must_use]
pub fn is_open_allowlist(root: &[u8; 32]) -> bool {
    root.iter().all(|&b| b == 0)
}

/// Canonical allowlist leaf for a buyer: SHA-256 of the account id that
/// **authorizes** the gated buy - i.e. the `buyer_collateral_holding` account
/// `buy_gated` binds against (`leaf == allowlist_leaf(buyer_collateral_holding
/// .account_id)`), NOT a separate wallet-identity account. The off-chain
/// allowlist root MUST commit to these per-holding leaves, so a published
/// `(leaf, proof)` cannot be replayed by a different account. The SDK exposes
/// `lpad_sdk::lbp_allowlist_leaf(collateral_holding)` so callers derive it the
/// same way and cannot mis-bind it to the wrong account.
#[must_use]
pub fn allowlist_leaf(buyer: &AccountId) -> [u8; 32] {
    hash32(&buyer.to_bytes())
}

// ---------------------------------------------------------------------------
// PDA derivation
// ---------------------------------------------------------------------------

#[must_use]
pub fn compute_pool_pda(
    program_id: ProgramId,
    token_definition_id: AccountId,
    collateral_definition_id: AccountId,
    creator: AccountId,
    nonce: u64,
) -> AccountId {
    AccountId::for_public_pda(
        &program_id,
        &compute_pool_pda_seed(token_definition_id, collateral_definition_id, creator, nonce),
    )
}

#[must_use]
pub fn compute_pool_pda_seed(
    token_definition_id: AccountId,
    collateral_definition_id: AccountId,
    creator: AccountId,
    nonce: u64,
) -> PdaSeed {
    let mut bytes = [0u8; 104];
    bytes[0..32].copy_from_slice(&token_definition_id.to_bytes());
    bytes[32..64].copy_from_slice(&collateral_definition_id.to_bytes());
    bytes[64..96].copy_from_slice(&creator.to_bytes());
    bytes[96..104].copy_from_slice(&nonce.to_le_bytes());
    PdaSeed::new(hash32(&bytes))
}

#[must_use]
pub fn compute_token_vault_pda(program_id: ProgramId, pool_id: AccountId) -> AccountId {
    AccountId::for_public_pda(&program_id, &compute_token_vault_pda_seed(pool_id))
}

#[must_use]
pub fn compute_token_vault_pda_seed(pool_id: AccountId) -> PdaSeed {
    let mut bytes = [0u8; 48];
    bytes[0..32].copy_from_slice(&pool_id.to_bytes());
    bytes[32..48].copy_from_slice(TOKEN_VAULT_TAG);
    PdaSeed::new(hash32(&bytes))
}

#[must_use]
pub fn compute_collateral_vault_pda(program_id: ProgramId, pool_id: AccountId) -> AccountId {
    AccountId::for_public_pda(&program_id, &compute_collateral_vault_pda_seed(pool_id))
}

#[must_use]
pub fn compute_collateral_vault_pda_seed(pool_id: AccountId) -> PdaSeed {
    let mut bytes = [0u8; 48];
    bytes[0..32].copy_from_slice(&pool_id.to_bytes());
    bytes[32..48].copy_from_slice(COLLATERAL_VAULT_TAG);
    PdaSeed::new(hash32(&bytes))
}

/// Per-pool [`WeightObs`] account. Derived the same way as the two vault PDAs,
/// so `Poke` can claim it lazily on first use without any creation-time setup.
#[must_use]
pub fn compute_weight_obs_pda(program_id: ProgramId, pool_id: AccountId) -> AccountId {
    AccountId::for_public_pda(&program_id, &compute_weight_obs_pda_seed(pool_id))
}

#[must_use]
pub fn compute_weight_obs_pda_seed(pool_id: AccountId) -> PdaSeed {
    let mut bytes = [0u8; 48];
    bytes[0..32].copy_from_slice(&pool_id.to_bytes());
    bytes[32..48].copy_from_slice(WEIGHT_OBS_TAG);
    PdaSeed::new(hash32(&bytes))
}

/// Per-creator pool index PDA (see [`CreatorIndex`]). Keyed on the creator
/// ALONE - that is what makes discovery one read per creator - so it is stable
/// for the life of the account and shared by every pool that creator opens.
///
/// The program id is mixed in by `for_public_pda`, so the bonding curve's index
/// for the same creator is a different account: there is nothing shared between
/// the two programs to keep in step.
#[must_use]
pub fn compute_creator_index_pda(program_id: ProgramId, creator: AccountId) -> AccountId {
    AccountId::for_public_pda(&program_id, &compute_creator_index_pda_seed(creator))
}

#[must_use]
pub fn compute_creator_index_pda_seed(creator: AccountId) -> PdaSeed {
    let mut bytes = [0u8; 48];
    bytes[0..32].copy_from_slice(&creator.to_bytes());
    bytes[32..48].copy_from_slice(CREATOR_INDEX_TAG);
    PdaSeed::new(hash32(&bytes))
}

// ---------------------------------------------------------------------------
// Serialization + helpers
// ---------------------------------------------------------------------------

impl TryFrom<&Data> for PoolState {
    type Error = std::io::Error;
    fn try_from(data: &Data) -> Result<Self, Self::Error> {
        PoolState::try_from_slice(data.as_ref())
    }
}

impl From<&PoolState> for Data {
    fn from(state: &PoolState) -> Self {
        let mut data = Vec::with_capacity(std::mem::size_of_val(state));
        BorshSerialize::serialize(state, &mut data).expect("borsh serialize");
        Data::try_from(data).expect("PoolState too big")
    }
}

impl TryFrom<&Data> for WeightObs {
    type Error = std::io::Error;
    fn try_from(data: &Data) -> Result<Self, Self::Error> {
        WeightObs::try_from_slice(data.as_ref())
    }
}

impl From<&WeightObs> for Data {
    fn from(obs: &WeightObs) -> Self {
        let mut data = Vec::with_capacity(std::mem::size_of_val(obs));
        BorshSerialize::serialize(obs, &mut data).expect("borsh serialize");
        Data::try_from(data).expect("WeightObs too big")
    }
}

impl TryFrom<&Data> for CreatorIndex {
    type Error = std::io::Error;

    /// Decodes and AUTHENTICATES: an account that merely happens to Borsh-decode
    /// is rejected unless it carries the magic and a version this build knows.
    fn try_from(data: &Data) -> Result<Self, Self::Error> {
        let index = CreatorIndex::try_from_slice(data.as_ref())?;
        if index.magic != CREATOR_INDEX_MAGIC || index.version != CREATOR_INDEX_VERSION {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "not an LBP creator index",
            ));
        }
        Ok(index)
    }
}

impl From<&CreatorIndex> for Data {
    fn from(index: &CreatorIndex) -> Self {
        let mut data = Vec::new();
        BorshSerialize::serialize(index, &mut data).expect("borsh serialize");
        // Unreachable: `CreatorIndex::push` bounds the list at MAX_INDEXED_POOLS,
        // whose encoded size is well under Data's cap.
        Data::try_from(data).expect("CreatorIndex too big")
    }
}

#[must_use]
pub fn read_fungible(account: &AccountWithMetadata, context: &str) -> (AccountId, u128) {
    let holding = token_core::TokenHolding::try_from(&account.account.data)
        .unwrap_or_else(|_| panic!("{context}: not a token holding"));
    match holding {
        token_core::TokenHolding::Fungible { definition_id, balance } => (definition_id, balance),
        _ => panic!("{context}: not Fungible"),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod proptests;

#[cfg(test)]
mod tests {
    use super::*;

    fn w(frac_num: u128, frac_den: u128) -> u128 {
        (frac_num << 64) / frac_den
    }

    #[test]
    fn weight_interpolates_linearly_and_clamps() {
        let ws = w(99, 100); // 0.99
        let we = w(1, 100); // 0.01
        assert_eq!(weight_token_q64(ws, we, 1_000, 2_000, 500), ws, "before start clamps");
        assert_eq!(weight_token_q64(ws, we, 1_000, 2_000, 3_000), we, "after end clamps");
        let mid = weight_token_q64(ws, we, 1_000, 2_000, 1_500);
        let expect = (ws + we) / 2;
        assert!(mid.abs_diff(expect) < (ONE / 1_000), "midpoint ~ average weight");
    }

    #[test]
    fn price_falls_as_weight_shifts_with_constant_reserves() {
        let ws = w(99, 100);
        let we = w(1, 100);
        let rt = 1_000_000u128;
        let rc = 50_000u128;
        let mut last = u128::MAX;
        for step in 0..=10 {
            let t = 1_000 + step * 100;
            let wt = weight_token_q64(ws, we, 1_000, 2_000, t);
            let p = spot_price_q64(rt, rc, wt);
            assert!(p < last, "LBP price must decline as weight shifts (step {step})");
            last = p;
        }
    }

    #[test]
    fn buy_output_floored_and_positive() {
        let wt = w(1, 2); // 0.5/0.5 -> behaves like constant product
        let out = buy_tokens_out(1_000_000, 1_000_000, wt, 100_000);
        let cp = 1_000_000u128 * 100_000 / (1_000_000 + 100_000);
        assert!(out.abs_diff(cp) <= 3, "0.5/0.5 weighted ~ constant product: out={out} cp={cp}");
        assert!(out > 0);
    }

    #[test]
    fn buy_more_when_token_weight_low() {
        // LBP economics: spot price = (Bc*Wt)/(Bt*Wc), so a HIGH token weight means
        // a HIGH price and FEWER tokens per collateral. As the weight declines over
        // the sale, the price falls and the same collateral buys more tokens.
        let cin = 100_000u128;
        let high_weight = buy_tokens_out(1_000_000, 1_000_000, w(9, 10), cin); // high price
        let low_weight = buy_tokens_out(1_000_000, 1_000_000, w(1, 10), cin); // low price
        assert!(
            low_weight > high_weight,
            "fewer tokens at high token weight (high price): high_weight={high_weight} low_weight={low_weight}",
        );
    }

    #[test]
    fn close_fee_rounds_up() {
        assert_eq!(close_fee(0, 500), 0);
        assert_eq!(close_fee(10_000, 0), 0);
        assert_eq!(close_fee(10_000, 500), 500); // 5%
        assert_eq!(close_fee(1, 500), 1); // rounds up
    }

    #[test]
    fn merkle_inclusion_roundtrip() {
        let a = hash32(b"alice");
        let b = hash32(b"bob");
        let c = hash32(b"carol");
        let d = hash32(b"dave");
        let pair = |x: [u8; 32], y: [u8; 32]| {
            let mut buf = [0u8; 64];
            if x <= y {
                buf[0..32].copy_from_slice(&x);
                buf[32..].copy_from_slice(&y);
            } else {
                buf[0..32].copy_from_slice(&y);
                buf[32..].copy_from_slice(&x);
            }
            hash32(&buf)
        };
        let ab = pair(a, b);
        let cd = pair(c, d);
        let root = pair(ab, cd);
        assert!(merkle_verify(a, &[b, cd], root), "valid proof accepted");
        assert!(!merkle_verify(a, &[c, cd], root), "wrong sibling rejected");
        assert!(!merkle_verify(hash32(b"eve"), &[b, cd], root), "non-member rejected");
    }

    #[test]
    fn open_allowlist_detected() {
        assert!(is_open_allowlist(&[0u8; 32]));
        let mut r = [0u8; 32];
        r[5] = 1;
        assert!(!is_open_allowlist(&r));
    }

    #[test]
    fn pool_state_serde_roundtrip() {
        let s = PoolState {
            fee_bps: 500,
            w_start_q64: w(99, 100),
            w_end_q64: w(1, 100),
            t_start_ms: 1_000,
            t_end_ms: 100_000,
            reserve_token: 1_000_000,
            reserve_collateral: 50_000,
            status: SaleStatus::Open,
            ..Default::default()
        };
        let data: Data = (&s).into();
        let back = PoolState::try_from(&data).unwrap();
        assert_eq!(s, back);
    }

    // ---- creator index ----------------------------------------------------
    //
    // The account that makes `my-pools` one read per creator. Every test here
    // fails if the guard it names is removed.

    #[test]
    fn creator_index_round_trips_through_data() {
        let creator = AccountId::new([9u8; 32]);
        let mut index = CreatorIndex::new(creator);
        index.push(AccountId::new([1u8; 32]));
        index.push(AccountId::new([2u8; 32]));
        let data: Data = (&index).into();
        assert_eq!(CreatorIndex::try_from(&data).unwrap(), index);
        assert_eq!(
            index.pool_ids,
            vec![AccountId::new([1u8; 32]), AccountId::new([2u8; 32])],
            "ids are kept in creation order"
        );
        assert_eq!(index.creator, creator, "a decoded index says whose it is");
    }

    /// An empty index must decode from a freshly claimed account rather than
    /// erroring: the first `CreateSale` writes one, and every later read of a
    /// creator with no pools yet sees it.
    #[test]
    fn an_empty_creator_index_round_trips() {
        let data: Data = (&CreatorIndex::new(AccountId::new([9u8; 32]))).into();
        assert!(CreatorIndex::try_from(&data).unwrap().pool_ids.is_empty());
    }

    /// The SDK derives this PDA and reads whatever is there, so the decode has to
    /// be able to say "not one of mine". Drop the magic check and a `PoolState`
    /// (or anything else that happens to Borsh-decode) is read as a pool list.
    #[test]
    fn a_creator_index_decode_rejects_foreign_bytes() {
        let pool: Data = (&PoolState::default()).into();
        assert!(
            CreatorIndex::try_from(&pool).is_err(),
            "another account's bytes must never decode as an index"
        );
        let mut index = CreatorIndex::new(AccountId::new([9u8; 32]));
        index.magic = *b"lpad/bci";
        let data: Data = (&index).into();
        assert!(
            CreatorIndex::try_from(&data).is_err(),
            "the bonding curve's index is not this program's"
        );
    }

    /// A newer layout must fail loudly here rather than be mis-parsed by an old
    /// reader as a shorter list.
    #[test]
    fn a_creator_index_decode_rejects_an_unknown_version() {
        let mut index = CreatorIndex::new(AccountId::new([9u8; 32]));
        index.version = CREATOR_INDEX_VERSION + 1;
        let data: Data = (&index).into();
        assert!(CreatorIndex::try_from(&data).is_err());
    }

    /// Discovery reads this account knowing only the creator, so the seed must
    /// depend on the creator - and on nothing else. Two creators, two accounts.
    #[test]
    fn creator_index_pda_is_per_creator_and_per_program() {
        let a = AccountId::new([1u8; 32]);
        let b = AccountId::new([2u8; 32]);
        let p: ProgramId = [7u32; 8];
        let q: ProgramId = [8u32; 8];
        assert_ne!(
            compute_creator_index_pda(p, a),
            compute_creator_index_pda(p, b),
            "one creator's index must never be another's"
        );
        assert_ne!(
            compute_creator_index_pda(p, a),
            compute_creator_index_pda(q, a),
            "the bonding curve's index for the same creator is a different account"
        );
        assert_eq!(
            compute_creator_index_pda(p, a),
            compute_creator_index_pda(p, a),
            "and it is derivable, not remembered"
        );
    }

    /// The tag is what keeps this PDA off the vault/observation PDAs: a
    /// collision would write index bytes over a live account.
    #[test]
    fn creator_index_pda_does_not_collide_with_the_other_pdas() {
        let p: ProgramId = [7u32; 8];
        let id = AccountId::new([1u8; 32]);
        let index = compute_creator_index_pda(p, id);
        assert_ne!(index, compute_token_vault_pda(p, id));
        assert_ne!(index, compute_collateral_vault_pda(p, id));
        assert_ne!(index, compute_weight_obs_pda(p, id));
    }

    /// Drop the assert in `push` and this test stops panicking.
    #[test]
    #[should_panic(expected = "creator pool index is full")]
    fn creator_index_push_stops_at_the_bound() {
        let mut index = CreatorIndex::new(AccountId::new([9u8; 32]));
        index.pool_ids = vec![AccountId::new([1u8; 32]); MAX_INDEXED_POOLS];
        index.push(AccountId::new([2u8; 32]));
    }

    /// ...and the bound it stops at really does still encode: the point of
    /// asserting early is that the account never reaches `DATA_MAX_LENGTH` and
    /// fails inside the `Data` conversion instead, which names nothing.
    #[test]
    fn a_full_creator_index_still_fits_in_one_account() {
        let mut index = CreatorIndex::new(AccountId::new([9u8; 32]));
        index.pool_ids = vec![AccountId::new([1u8; 32]); MAX_INDEXED_POOLS];
        let data: Data = (&index).into();
        assert_eq!(data.as_ref().len(), 8 + 2 + 32 + 4 + 32 * MAX_INDEXED_POOLS);
        assert_eq!(CreatorIndex::try_from(&data).unwrap().pool_ids.len(), MAX_INDEXED_POOLS);
    }
}
