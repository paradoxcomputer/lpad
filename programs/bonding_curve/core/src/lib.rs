//! Core data structures, PDA derivation, and pricing math for the LPAD
//! **bonding curve** launchpad program (RFP-015).
//!
//! Pricing is a constant-product AMM with **virtual reserves** (pump.fun /
//! Meteora style): supply-driven, rises with each purchase. Integer-only mul/div
//! (no fixed-point exponentiation) that rounds against the trader so the pool
//! stays solvent and rounding never violates the invariant (RFP-015 Func #1).
//!
//! Pricing lives in free functions over `u128` so it is reused verbatim by the
//! host program, the FFI, and the disposable (private) buy path.

use borsh::{BorshDeserialize, BorshSerialize};
use lee_core::{
    account::{AccountId, AccountWithMetadata, Data},
    program::{PdaSeed, ProgramId},
};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Basis-points denominator for the per-swap protocol fee.
pub const FEE_BPS_DENOMINATOR: u128 = 10_000;

/// Upper bound on an admin-configured per-swap fee (10%). Guards against a
/// fat-fingered fee that would make the curve unusable.
pub const MAX_FEE_BPS: u128 = 1_000;

/// Exclusive upper bound on `virt_collateral`: the Q64.64 spot-price domain.
/// `spot_price_q64` shifts `virt_collateral` left by 64 bits, which wraps once
/// it reaches 2^64 (overflow-checks are off in the release guest). Enforced at
/// sale creation so an out-of-domain reserve is rejected up-front rather than
/// reverting every public buy/sell once the sale is live. Mirrors the LBP
/// `MAX_RESERVE` defense.
pub const MAX_VIRT_COLLATERAL: u128 = 1u128 << 64;

/// Bounded on-chain observation ring (gapless recent history with no off-chain
/// indexer, matching the ldex AMM oracle approach).
pub const ORACLE_RING_CAP: usize = 64;

/// Maximum byte length of the self-describing `token_name`/`token_symbol`
/// metadata copied into the sale state. Bounded at creation so the serialized
/// state stays well under `Data`'s `DATA_MAX_LENGTH` (100 KiB) even once the
/// 64-entry observation ring fills - an unbounded creator-set string could
/// otherwise grow the state across that cap mid-life, panicking the
/// `From<&SaleState> for Data` encode and reverting every buy/sell/close.
pub const MAX_METADATA_LEN: usize = 64;

/// Canonical on-chain Clock account (sequencer-updated every block). Threaded
/// read-only into the public buy/sell paths for the optional end-timestamp
/// check and analytics timestamps. Private paths omit it (proof-time drift).
pub const CLOCK_01: AccountId = AccountId::new(*b"/LEZ/ClockProgramAccount/0000001");

/// Local mirror of the sequencer `ClockAccountData`: identical Borsh layout
/// (`u64` block id then ms timestamp; read as `i64` like the ldex AMM).
#[derive(Clone, Copy, BorshDeserialize)]
pub struct ClockData {
    pub block_id: u64,
    pub timestamp: i64,
}

/// Cap on the width of a `BuyDisposable` timestamp validity window (1 hour).
///
/// A disposable buy carries no clock account, so the window is its only notion
/// of time - and a window is a free option: the buyer prices against a pinned
/// pre-state, then holds the finished proof, submitting only if the curve moved
/// their way and letting it expire otherwise. Capping the width bounds how long
/// that option runs while still leaving room for a slow proof.
pub const MAX_PRIVATE_WINDOW_MS: u64 = 3_600_000;

/// Stable PDA-seed discriminators (must never change for address compatibility).
const TOKEN_VAULT_TAG: &[u8; 16] = b"bc/token_vault\0\0";
const COLLATERAL_VAULT_TAG: &[u8; 16] = b"bc/coll_vault\0\0\0";
const CREATOR_INDEX_TAG: &[u8; 16] = b"bc/creator_idx\0\0";

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Lifecycle status of a sale.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub enum SaleStatus {
    #[default]
    Open,
    Closed,
}

/// One on-chain analytics observation. `spot_price_q64` is the Q64.64 spot
/// price (collateral per token) sampled at `ts_ms`.
#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct SupplyObservation {
    pub ts_ms: u64,
    pub sale_reserve: u128,
    pub spot_price_q64: u128,
}

/// Full on-chain state of a bonding-curve sale. Public by design (RFP-015
/// Privacy Architecture): any participant can verify `tokens_out` against the
/// formula independently.
#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct SaleState {
    // ---- identity / config (immutable after creation) ----
    pub creator: AccountId,
    pub token_definition_id: AccountId,
    pub collateral_definition_id: AccountId,
    pub token_vault_id: AccountId,
    pub collateral_vault_id: AccountId,
    pub treasury_id: AccountId,
    /// ATA program pinned at creation. `BuyAta`/`SellAta` assert the submitter's
    /// `ata_program_id` equals this, so the user leg cannot be dispatched to a
    /// substitute (e.g. a no-op) program that would skip the real `token::Transfer`
    /// and let the vault legs pay out against an uncredited deposit.
    pub ata_program_id: ProgramId,
    pub fee_bps: u128,
    pub one_directional: bool,
    pub end_timestamp_ms: u64,
    pub min_duration_ms: u64,
    pub nonce: u64,
    pub created_ts_ms: i64,
    // ---- curve params ----
    /// Virtual token reserve `Vt` (synthetic; `> sale_reserve_initial`).
    pub virt_token: u128,
    /// Virtual collateral reserve `Vc` (synthetic; no real deposit).
    pub virt_collateral: u128,
    /// `k = Vt0 * Vc0`, computed once at creation and never changed. Stored for
    /// the inverse-cost query and the invariant check; pricing itself uses the
    /// overflow-safe `Vt*c/(Vc+c)` form, so `k` is never an intermediate.
    pub k: u128,
    /// Sale quantity `D`.
    pub sale_reserve_initial: u128,
    /// DEX seed quantity `R` (untouched until close).
    pub dex_seed_reserve: u128,
    // ---- live accounting ----
    /// Real project tokens left for sale (starts at `D`, hits 0 at graduation).
    pub sale_reserve: u128,
    /// Real collateral raised, net of fees already swept to treasury.
    pub real_collateral: u128,
    /// Protocol fees escrowed *inside the collateral vault*, owed to the
    /// treasury and settled by [`Instruction::SweepTreasury`]. Only
    /// `BuyDisposable` credits this: it cannot pay the treasury directly,
    /// because the treasury is a `CreateSale` argument that lpad's own bootstrap
    /// shares across sales, and every public account in a privacy transaction is
    /// pinned byte-for-byte at proving time - so pinning the treasury would let
    /// any fee-bearing activity anywhere invalidate every in-flight private buy.
    /// The public paths sweep their fee in the same transaction and leave this
    /// at zero.
    ///
    /// INVARIANT: `collateral_vault_balance == real_collateral + treasury_owed`.
    /// `Withdraw` pays the creator `balance - treasury_owed` and deliberately
    /// leaves this bucket - and the collateral backing it - in the vault, which
    /// is what keeps the invariant true across a withdrawal.
    ///
    /// RESIDUAL RISK - a treasury that cannot receive would make this bucket
    /// **unsweepable forever**, and it is worth being blunt about what that
    /// would mean: no instruction in this program can release it. `SweepTreasury`
    /// is the only one that pays it out, `Withdraw` deliberately subtracts it
    /// from the creator's payout, and nothing rewrites `treasury_id`. The
    /// collateral behind it would simply stay in the vault for good. What is
    /// bounded is the damage, not the loss: it is only ever the accrued
    /// disposable-buy fee, and the raise itself still comes out (that is why
    /// settlement is its own instruction).
    ///
    /// `CreateSale` takes the treasury as an ACCOUNT and requires it to ALREADY
    /// be an initialised `TokenHolding::Fungible` of the sale's collateral
    /// definition, under that definition's own token program. That is what makes
    /// the paragraph above hypothetical instead of a live hazard. The shapes it
    /// rules out, each of which was permanent:
    ///   * **uninitialised** - settling has to CREATE the treasury, and
    ///     `token::transfer` creates a recipient with
    ///     `new_claimed_if_default(.., Claim::Authorized)`, a claim LEZ admits
    ///     only when the account being claimed is itself authorized. So nothing
    ///     but the treasury's OWN signature could settle such a sale - and that
    ///     same key is the only thing that can destroy the id, because a claim
    ///     also needs the pre-state to be `Account::default()` WHOLE while LEZ
    ///     bumps a signer's nonce even when its account is unowned. Let it sign
    ///     anything else first and no program can ever claim the account: it can
    ///     never receive a token. A STRANGER cannot do any of this - LEZ refuses
    ///     any transaction that leaves a DEFAULT-owned account modified but
    ///     unclaimed (`DefaultAccountModifiedWithoutClaim`), balance included, so
    ///     a publicly readable `treasury_id` cannot be dusted from outside. The
    ///     hazard was self-inflicted; it was permanent all the same.
    ///   * **wrong definition, or a holding under a different token program** -
    ///     the fee leg moves the collateral vault's token and is dispatched on
    ///     that vault's own program, so either shape reverts on every sweep, and
    ///     no signature changes that. Deployment on LEZ is permissionless, so the
    ///     second one is a shape anyone can mint.
    ///
    /// The pin holds for the life of the sale because the state cannot regress:
    /// an initialised holding is owned by the token program, and LEZ's token
    /// program has no instruction that un-initialises one (`burn` only lowers a
    /// balance). `SweepTreasury` keeps its uninitialised-treasury branch anyway,
    /// as unreachable defence-in-depth - see the note there.
    pub treasury_owed: u128,
    pub status: SaleStatus,
    // ---- gapless analytics (RFP-015 Usability #8) ----
    pub cum_collateral_in: u128,
    pub cum_fees: u128,
    pub buy_count: u64,
    pub sell_count: u64,
    pub obs: Vec<SupplyObservation>,
    // ---- self-describing metadata (copied from the project token at creation) ----
    /// Token display name, mirrored on-chain so the sale is self-describing
    /// without an off-chain metadata lookup. Empty if minted without metadata.
    pub token_name: String,
    /// Token ticker symbol (same provenance as `token_name`).
    pub token_symbol: String,
}

impl SaleState {
    /// Current Q64.64 spot price (collateral per token): `Vc / Vt`.
    #[must_use]
    pub fn spot_price_q64(&self) -> u128 {
        spot_price_q64(self.virt_collateral, self.virt_token)
    }

    /// Sale reserve sold so far, as a fraction of `D` in basis points.
    #[must_use]
    pub fn sold_bps(&self) -> u128 {
        if self.sale_reserve_initial == 0 {
            return 0;
        }
        let sold = self.sale_reserve_initial.saturating_sub(self.sale_reserve);
        sold.saturating_mul(FEE_BPS_DENOMINATOR) / self.sale_reserve_initial
    }

    /// The constant-product invariant must never weaken: integer rounding always
    /// accrues to the pool, so `Vt * Vc >= k` holds after every operation.
    /// Returns false if a (buggy) state ever violates it.
    #[must_use]
    pub fn invariant_holds(&self) -> bool {
        match self.virt_token.checked_mul(self.virt_collateral) {
            Some(prod) => prod >= self.k,
            // Overflow means the product is astronomically large, so the
            // invariant trivially holds; we only flag genuine shrinkage below k.
            None => true,
        }
    }

    /// Push an analytics observation, keeping the ring bounded.
    pub fn record_observation(&mut self, ts_ms: u64) {
        self.obs.push(SupplyObservation {
            ts_ms,
            sale_reserve: self.sale_reserve,
            spot_price_q64: self.spot_price_q64(),
        });
        let n = self.obs.len();
        if n > ORACLE_RING_CAP {
            self.obs.drain(0..n - ORACLE_RING_CAP);
        }
    }
}

// ---------------------------------------------------------------------------
// Per-creator sale index
// ---------------------------------------------------------------------------

/// First eight bytes of every [`CreatorIndex`] account. `SaleState` carries no
/// discriminant because nothing else is ever stored at a sale PDA, but this
/// account is *read speculatively* - the SDK derives the index PDA for a wallet
/// and reads whatever is there - so it has to be able to say "this is not one of
/// mine" rather than decode some other account's bytes into a plausible-looking
/// list of sale ids.
pub const CREATOR_INDEX_MAGIC: [u8; 8] = *b"lpad/bci";

/// Layout version of [`CreatorIndex`]. Bumped only for a breaking field change;
/// [`CreatorIndex::try_from`] refuses anything it does not recognise, so an old
/// reader fails loudly instead of mis-parsing a newer index.
pub const CREATOR_INDEX_VERSION: u16 = 1;

/// Maximum number of sale ids one creator's index may hold.
///
/// `Data` caps an account at `DATA_MAX_LENGTH` (100 KiB). A full index encodes
/// to `8 + 2 + 32 + 4 + 32*n` bytes, so 3,000 ids is 96,046 - comfortably under
/// the cap with room for a future field. The bound is asserted in
/// [`CreatorIndex::push`] rather than left to the encode, because the encode's
/// failure is a `DataTooBigError` panic from inside `From<&CreatorIndex> for
/// Data` that says nothing about which account or what to do next.
pub const MAX_INDEXED_SALES: usize = 3_000;

/// Per-creator index of the sales that creator has created: ONE account per
/// (program, creator), appended to by `CreateSale`.
///
/// This exists so discovery is a single read per creator. Without it the SDK has
/// to re-derive every sale PDA the wallet could possibly have made - a product
/// over (creator account, token definition, collateral definition, nonce), each
/// arm of which is a network round-trip - which measured at ~4,800 reads and
/// tens of minutes on a wallet with history.
///
/// IDS ONLY, deliberately. Sale state (reserves, status, counters) changes on
/// every trade, so an index of state would be stale the moment it was written;
/// an id is permanent. Readers resolve the ids against the sale accounts.
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
    /// Sale PDAs this creator has created, oldest first. Never removed - see the
    /// note on `CloseSale` in `lifecycle.rs`: a closed sale is still one the
    /// creator created and still worth listing.
    pub sale_ids: Vec<AccountId>,
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
            sale_ids: Vec::new(),
        }
    }

    /// Append a newly created sale id, enforcing [`MAX_INDEXED_SALES`].
    ///
    /// No dedup check: the caller only ever appends a sale PDA it has just
    /// asserted was uninitialized, and a sale PDA is unique per
    /// `(token_def, collateral_def, creator, nonce)`, so the same id cannot be
    /// created twice.
    pub fn push(&mut self, sale_id: AccountId) {
        assert!(
            self.sale_ids.len() < MAX_INDEXED_SALES,
            "creator sale index is full - create further sales from a different creator account"
        );
        self.sale_ids.push(sale_id);
    }
}

// ---------------------------------------------------------------------------
// Instruction set
// ---------------------------------------------------------------------------

/// Bonding-curve program instructions. See `SPEC.md` §1.4 for account orders.
#[derive(Serialize, Deserialize)]
// CreateSale is much larger than the hot Buy/Sell/lifecycle variants, but it is
// constructed once per sale (cold) and the enum is serde-only (no fixed wire
// layout), so boxing it is not worth it - and the #[lez_program] guest dispatcher
// destructures CreateSale by named fields, which a Box<...> tuple variant breaks.
#[allow(clippy::large_enum_variant)]
pub enum Instruction {
    /// Create a new sale. Transfers `D + R` project tokens from the creator into
    /// the token vault. No real collateral is deposited.
    ///
    /// Account order: `[sale, token_vault, collateral_vault, treasury,
    /// token_definition, collateral_definition, creator_token_holding, creator,
    /// creator_index, clock]`.
    ///
    /// `creator_index` is the creator's [`CreatorIndex`] PDA - claimed here on
    /// the creator's first sale, appended to on every later one, and pinned to
    /// `compute_creator_index_pda(self_program_id, creator)` so no one can append
    /// into another creator's list. It is what makes sale discovery one read per
    /// creator; sales created before this account existed appear in no index, so
    /// a reader still needs the brute-force derivation as a fallback.
    ///
    /// The `treasury` slot is read-only and is here to be type-checked: it must
    /// already be an initialised Fungible holding of `collateral_definition_id`
    /// under that definition's own token program, which is what keeps the fee
    /// escrow sweepable for the life of the sale (see
    /// [`SaleState::treasury_owed`]). `treasury_id` below is the id pinned into
    /// the sale and must equal that account's id.
    CreateSale {
        collateral_definition_id: AccountId,
        treasury_id: AccountId,
        token_name: String,
        token_symbol: String,
        sale_quantity: u128,
        dex_seed_quantity: u128,
        virt_token: u128,
        virt_collateral: u128,
        fee_bps: u128,
        one_directional: bool,
        end_timestamp_ms: u64,
        min_duration_ms: u64,
        nonce: u64,
        /// ATA program to pin into the sale (asserted by `BuyAta`/`SellAta`).
        ata_program_id: ProgramId,
        deadline: u64,
    },
    /// Public buy with keypair token holdings.
    Buy {
        collateral_in: u128,
        min_tokens_out: u128,
        deadline: u64,
    },
    /// Public buy where the user side uses **Associated Token Accounts** (RFP-015
    /// Func #7 - ATAs for all token interactions, per LP-0014 / RFP-008). The
    /// buyer's collateral leg is dispatched through the ATA program (which chains
    /// the token::Transfer from the ATA-PDA), and tokens are delivered into the
    /// buyer's token ATA. `ata_program_id` is required to dispatch the chained
    /// `ata::Transfer` (it cannot be read from the ATA's `program_owner`, which is
    /// the token program - the ATA program holds PDA authority, not storage).
    BuyAta {
        collateral_in: u128,
        min_tokens_out: u128,
        ata_program_id: ProgramId,
        deadline: u64,
    },
    /// Public sell back into the curve (reverts if `one_directional`).
    Sell {
        tokens_in: u128,
        min_collateral_out: u128,
        deadline: u64,
    },
    /// Public sell where the user side uses ATAs (symmetric to [`BuyAta`]).
    SellAta {
        tokens_in: u128,
        min_collateral_out: u128,
        ata_program_id: ProgramId,
        deadline: u64,
    },
    /// Manual close (creator) once the end timestamp has passed.
    CloseSale { deadline: u64 },
    /// Creator withdrawal of raised collateral + unused DEX seed reserve.
    ///
    /// Pays out `collateral_vault_balance - treasury_owed`: the escrowed
    /// protocol fee stays in the vault for [`Instruction::SweepTreasury`], a
    /// separate instruction so that an unusable treasury can never block the
    /// creator's payout (see [`SaleState::treasury_owed`]).
    ///
    /// Account order: `[sale, token_vault, collateral_vault,
    /// creator_collateral_holding, creator_token_holding, creator]`.
    Withdraw { deadline: u64 },
    /// Buy in which the buyer's collateral holding and token holding are
    /// **private** account slots, so the debit and the credit are private notes
    /// inside one atomic proof. "Disposable" names the single-use note, not an
    /// account: LEZ privacy is a per-slot label on an otherwise ordinary buy, so
    /// there is no ephemeral account, no deshield leg and no re-shield leg.
    ///
    /// Account order: `[sale, token_vault, collateral_vault,
    /// buyer_collateral_holding, buyer_token_holding]` - note the two accounts
    /// the public `Buy` has and this does not:
    ///   * **no clock**, because every public account in a privacy transaction is
    ///     pinned byte-for-byte at proving time and re-verified against live
    ///     state at inclusion, while the clock's data is rewritten every block.
    ///     The sale's `end_timestamp_ms` guard is enforced instead by the
    ///     timestamp validity window the guest emits, whose exclusive upper bound
    ///     is [`private_window_hi`] and whose inclusive lower bound is
    ///     `not_before_ms`.
    ///   * **no treasury**, for the same pinning reason (see
    ///     [`SaleState::treasury_owed`]): the fee is escrowed in the collateral
    ///     vault and handed over later by [`Instruction::SweepTreasury`] rather
    ///     than swept here.
    ///
    /// `not_before_ms` is the window's lower bound - the earliest wall-clock time
    /// the buy may be included at.
    BuyDisposable {
        collateral_in: u128,
        min_tokens_out: u128,
        not_before_ms: u64,
        deadline: u64,
    },
    /// Pay the fees escrowed by [`Instruction::BuyDisposable`] out of the
    /// collateral vault to the sale's pinned treasury, clearing
    /// [`SaleState::treasury_owed`] by exactly the amount that moved.
    ///
    /// Account order: `[sale, collateral_vault, treasury]`.
    ///
    /// **Permissionless, and that is the intent.** No signature is required: the
    /// only effect available to a submitter is moving the owed fee from the vault
    /// to the `treasury_id` pinned in the sale at creation, so a stranger who
    /// sends this hands the fee to its rightful owner and pays the gas for the
    /// privilege. A creator/admin signature would buy nothing and would add a
    /// party able to withhold settlement.
    ///
    /// The treasury's *own* signature is accepted though - it would be the only
    /// way to settle into a treasury account that was never initialised, since the
    /// fee leg has to create it and LEZ lets only the claimed account's own
    /// authorization do that. `CreateSale` no longer admits such a sale (see
    /// [`SaleState::treasury_owed`]), so that path is unreachable and kept only as
    /// defence-in-depth; the account list is the same either way and the slot is
    /// simply forwarded as it arrives, never forced.
    ///
    /// Valid whether the sale is Open or Closed: the escrow accrues while the
    /// sale runs, and the treasury should not have to wait on the creator to
    /// close. Deliberately NOT a leg of `Withdraw` - a treasury that cannot
    /// receive (uninitialised, wrong definition, wrong token program) would
    /// otherwise revert the creator's payout with it and lock the entire raise.
    SweepTreasury { deadline: u64 },
}

// ---------------------------------------------------------------------------
// Disposable (private) buy validity window
// ---------------------------------------------------------------------------

/// Exclusive upper bound of the timestamp validity window a `BuyDisposable`
/// output carries, given the submitter's `deadline`, the window's lower bound
/// `not_before_ms`, and the sale's end (`end_or_max` is `end_timestamp_ms`, or
/// `u64::MAX` for a sale with no configured end).
///
/// All three are upper bounds and the tightest wins: the submitter's own
/// deadline, the free-option cap [`MAX_PRIVATE_WINDOW_MS`] past the lower bound,
/// and the sale's end - the last is the *only* way a private buy honours
/// `end_timestamp_ms`, since it carries no clock to compare against.
///
/// Kept pure (and free of the emptiness check) so it is unit-testable: the guest
/// separately asserts `not_before_ms < hi`, which is what the LEZ
/// `ValidityWindow` would otherwise reject as an empty window.
#[must_use]
pub fn private_window_hi(deadline: u64, not_before_ms: u64, end_or_max: u64) -> u64 {
    deadline
        .min(not_before_ms.saturating_add(MAX_PRIVATE_WINDOW_MS))
        .min(end_or_max)
}

// ---------------------------------------------------------------------------
// Pricing math (integer-only, rounds against the trader)
// ---------------------------------------------------------------------------

/// `ceil(a / b)` for `b > 0`, panicking on overflow. Rounds **up** - used
/// wherever the trader must not be advantaged by rounding.
#[must_use]
pub fn ceil_div(a: u128, b: u128) -> u128 {
    assert!(b != 0, "ceil_div by zero");
    if a == 0 {
        0
    } else {
        // Equivalent to (a + b - 1) / b but cannot overflow when a is near u128::MAX.
        (a - 1) / b + 1
    }
}

/// Q64.64 spot price `Vc / Vt` (collateral per token).
#[must_use]
pub fn spot_price_q64(virt_collateral: u128, virt_token: u128) -> u128 {
    if virt_token == 0 {
        return 0;
    }
    // Domain guard: the `<< 64` wraps once `virt_collateral` reaches 2^64
    // (overflow-checks are off in the release guest), which would silently poison
    // the on-chain observation ring written on every buy/sell. Because integer
    // flooring leaves the constant-product invariant slack, the live Vc can drift
    // above the idealized graduation value the create-time bound checks, so this
    // is reachable near graduation. Saturate to the max representable price
    // instead of asserting, so a near-2^64 Vc records a clamped (not wrapped)
    // observation and can never revert a live buy/sell or close - the solvency
    // path must not be bricked by this analytics value. Mirrors the LBP oracle
    // (`lbp_core::spot_price_q64`, which likewise saturates via `unwrap_or`).
    if virt_collateral >= MAX_VIRT_COLLATERAL {
        return u128::MAX;
    }
    (virt_collateral << 64) / virt_token
}

/// Per-swap fee on a gross collateral input, rounded **up** (against trader).
#[must_use]
pub fn buy_fee(collateral_in: u128, fee_bps: u128) -> u128 {
    ceil_div(
        collateral_in
            .checked_mul(fee_bps)
            .expect("collateral_in * fee_bps overflows u128"),
        FEE_BPS_DENOMINATOR,
    )
}

/// Tokens received for a gross collateral input. Fee is removed first (rounded
/// up), then the constant-product output is floored. Returns
/// `(tokens_out, fee, c_eff)` where `c_eff = collateral_in - fee` is the
/// amount that actually enters the curve.
///
/// Uses the overflow-safe identity `tokens_out = Vt*c_eff / (Vc + c_eff)`,
/// equivalent to `Vt - k/(Vc + c_eff)`, so `k` is never an intermediate.
#[must_use]
pub fn buy_tokens_out(
    virt_token: u128,
    virt_collateral: u128,
    fee_bps: u128,
    collateral_in: u128,
) -> (u128, u128, u128) {
    let fee = buy_fee(collateral_in, fee_bps);
    let c_eff = collateral_in
        .checked_sub(fee)
        .expect("fee exceeds collateral_in");
    let tokens_out = if c_eff == 0 {
        0
    } else {
        virt_token
            .checked_mul(c_eff)
            .expect("Vt * c_eff overflows u128")
            / virt_collateral
                .checked_add(c_eff)
                .expect("Vc + c_eff overflows u128")
    };
    (tokens_out, fee, c_eff)
}

/// Exact gross collateral cost to buy a specific token quantity `q` (RFP-015
/// Func #1 inverse). Rounds **up** so the pool is never short-changed. `q` must
/// be `< virt_token`.
#[must_use]
pub fn buy_cost_for_tokens(
    virt_token: u128,
    virt_collateral: u128,
    fee_bps: u128,
    q: u128,
) -> u128 {
    assert!(q < virt_token, "requested quantity exceeds virtual token reserve");
    assert!(
        fee_bps < FEE_BPS_DENOMINATOR,
        "fee_bps must be < the basis-point denominator for the inverse-cost query"
    );
    // c_eff_min = ceil(Vc * q / (Vt - q))  == ceil(k/(Vt-q) - Vc)
    let c_eff_min = ceil_div(
        virt_collateral
            .checked_mul(q)
            .expect("Vc * q overflows u128"),
        virt_token - q,
    );
    // gross-up for the fee: ceil(c_eff * DENOM / (DENOM - fee_bps))
    let denom = FEE_BPS_DENOMINATOR
        .checked_sub(fee_bps)
        .expect("fee_bps exceeds denominator");
    ceil_div(
        c_eff_min
            .checked_mul(FEE_BPS_DENOMINATOR)
            .expect("c_eff * DENOM overflows u128"),
        denom,
    )
}

/// Collateral returned for selling `tokens_in` back into the curve. Returns
/// `(collateral_to_seller, fee, c_out_raw)` where `c_out_raw` is the total
/// collateral the pool releases (`= collateral_to_seller + fee`). The raw
/// output is floored, then the fee is removed (rounded up).
#[must_use]
pub fn sell_collateral_out(
    virt_token: u128,
    virt_collateral: u128,
    fee_bps: u128,
    tokens_in: u128,
) -> (u128, u128, u128) {
    let c_out_raw = if tokens_in == 0 {
        0
    } else {
        virt_collateral
            .checked_mul(tokens_in)
            .expect("Vc * tokens_in overflows u128")
            / virt_token
                .checked_add(tokens_in)
                .expect("Vt + tokens_in overflows u128")
    };
    let fee = ceil_div(
        c_out_raw
            .checked_mul(fee_bps)
            .expect("c_out_raw * fee_bps overflows u128"),
        FEE_BPS_DENOMINATOR,
    );
    let to_seller = c_out_raw.checked_sub(fee).expect("fee exceeds c_out_raw");
    (to_seller, fee, c_out_raw)
}

// ---------------------------------------------------------------------------
// PDA derivation
// ---------------------------------------------------------------------------

fn hash32(bytes: &[u8]) -> [u8; 32] {
    use risc0_zkvm::sha::{Impl, Sha256};
    Impl::hash_bytes(bytes)
        .as_bytes()
        .try_into()
        .expect("Hash output must be exactly 32 bytes long")
}

/// Sale account PDA, bound to `(token_def, collateral_def, creator, nonce)` so a
/// creator can run multiple distinct sales for the same pair.
#[must_use]
pub fn compute_sale_pda(
    program_id: ProgramId,
    token_definition_id: AccountId,
    collateral_definition_id: AccountId,
    creator: AccountId,
    nonce: u64,
) -> AccountId {
    AccountId::for_public_pda(
        &program_id,
        &compute_sale_pda_seed(token_definition_id, collateral_definition_id, creator, nonce),
    )
}

#[must_use]
pub fn compute_sale_pda_seed(
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

/// Token vault PDA (holds the sale + DEX-seed project tokens).
#[must_use]
pub fn compute_token_vault_pda(program_id: ProgramId, sale_id: AccountId) -> AccountId {
    AccountId::for_public_pda(&program_id, &compute_token_vault_pda_seed(sale_id))
}

#[must_use]
pub fn compute_token_vault_pda_seed(sale_id: AccountId) -> PdaSeed {
    let mut bytes = [0u8; 48];
    bytes[0..32].copy_from_slice(&sale_id.to_bytes());
    bytes[32..48].copy_from_slice(TOKEN_VAULT_TAG);
    PdaSeed::new(hash32(&bytes))
}

/// Collateral vault PDA (holds raised collateral).
#[must_use]
pub fn compute_collateral_vault_pda(program_id: ProgramId, sale_id: AccountId) -> AccountId {
    AccountId::for_public_pda(&program_id, &compute_collateral_vault_pda_seed(sale_id))
}

#[must_use]
pub fn compute_collateral_vault_pda_seed(sale_id: AccountId) -> PdaSeed {
    let mut bytes = [0u8; 48];
    bytes[0..32].copy_from_slice(&sale_id.to_bytes());
    bytes[32..48].copy_from_slice(COLLATERAL_VAULT_TAG);
    PdaSeed::new(hash32(&bytes))
}

/// Per-creator sale index PDA (see [`CreatorIndex`]). Keyed on the creator
/// ALONE - that is what makes discovery one read per creator - so it is stable
/// for the life of the account and shared by every sale that creator opens.
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
// Serialization (Data <-> SaleState)
// ---------------------------------------------------------------------------

impl TryFrom<&Data> for SaleState {
    type Error = std::io::Error;

    fn try_from(data: &Data) -> Result<Self, Self::Error> {
        SaleState::try_from_slice(data.as_ref())
    }
}

impl From<&SaleState> for Data {
    fn from(state: &SaleState) -> Self {
        let mut data = Vec::with_capacity(std::mem::size_of_val(state));
        BorshSerialize::serialize(state, &mut data).expect("Serialization to Vec should not fail");
        Data::try_from(data).expect("Sale state encoded data should fit into Data")
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
                "not a bonding-curve creator index",
            ));
        }
        Ok(index)
    }
}

impl From<&CreatorIndex> for Data {
    fn from(index: &CreatorIndex) -> Self {
        let mut data = Vec::new();
        BorshSerialize::serialize(index, &mut data).expect("Serialization to Vec should not fail");
        // Unreachable: `CreatorIndex::push` bounds the list at MAX_INDEXED_SALES,
        // whose encoded size is well under Data's cap.
        Data::try_from(data).expect("Creator index encoded data should fit into Data")
    }
}

/// Read a Fungible token holding's `(definition_id, balance)`, panicking with a
/// contextual message otherwise.
#[must_use]
pub fn read_fungible(account: &AccountWithMetadata, context: &str) -> (AccountId, u128) {
    let holding = token_core::TokenHolding::try_from(&account.account.data)
        .unwrap_or_else(|_| panic!("{context}: expected a valid Token Holding account"));
    match holding {
        token_core::TokenHolding::Fungible { definition_id, balance } => (definition_id, balance),
        _ => panic!("{context}: expected a Fungible Token Holding account"),
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

    // Representative params: Vt = 1.1e9, Vc = 3e7, D = 1e9 (9-decimals scaled).
    const VT0: u128 = 1_100_000_000;
    const VC0: u128 = 30_000_000;

    #[test]
    fn ceil_div_basic() {
        assert_eq!(ceil_div(0, 7), 0);
        assert_eq!(ceil_div(1, 7), 1);
        assert_eq!(ceil_div(7, 7), 1);
        assert_eq!(ceil_div(8, 7), 2);
        assert_eq!(ceil_div(u128::MAX, 1), u128::MAX);
    }

    #[test]
    fn buy_is_floored_and_fee_rounds_up() {
        let (out, fee, c_eff) = buy_tokens_out(VT0, VC0, 100, 1_000_000); // 1% fee
        assert_eq!(fee, ceil_div(1_000_000 * 100, 10_000)); // = 10_000
        assert_eq!(c_eff, 1_000_000 - fee);
        let expect = VT0 * c_eff / (VC0 + c_eff); // floored CP output
        assert_eq!(out, expect);
    }

    #[test]
    fn zero_fee_buy_matches_pure_cp() {
        let (out, fee, c_eff) = buy_tokens_out(VT0, VC0, 0, 500_000);
        assert_eq!(fee, 0);
        assert_eq!(c_eff, 500_000);
        assert_eq!(out, VT0 * 500_000 / (VC0 + 500_000));
    }

    #[test]
    fn inverse_round_trips_within_one_unit() {
        // cost-for-tokens(q) then buy-tokens-out must yield >= q (rounding favors the pool).
        let fee_bps = 30;
        for q in [1u128, 10, 1000, 250_000, 10_000_000] {
            let cost = buy_cost_for_tokens(VT0, VC0, fee_bps, q);
            let (out, _, _) = buy_tokens_out(VT0, VC0, fee_bps, cost);
            assert!(out >= q, "q={q} cost={cost} out={out}: buyer must receive at least q");
            // One unit cheaper must under-deliver (tight bound).
            if cost > 0 {
                let (out_cheaper, _, _) = buy_tokens_out(VT0, VC0, fee_bps, cost - 1);
                assert!(out_cheaper <= out);
            }
        }
    }

    #[test]
    fn price_rises_monotonically_with_supply_sold() {
        // Sequential buys: spot price must be non-decreasing.
        let mut vt = VT0;
        let mut vc = VC0;
        let mut last_price = spot_price_q64(vc, vt);
        for _ in 0..50 {
            let (out, _fee, c_eff) = buy_tokens_out(vt, vc, 30, 200_000);
            assert!(out > 0);
            vt -= out;
            vc += c_eff;
            let p = spot_price_q64(vc, vt);
            assert!(p >= last_price, "price must rise as tokens are bought");
            last_price = p;
        }
    }

    #[test]
    fn spot_price_saturates_at_domain_boundary() {
        // At/above MAX_VIRT_COLLATERAL the `<< 64` would wrap (overflow-checks are
        // off in the release guest) and poison the observation ring. The guard must
        // saturate to u128::MAX instead of returning a wrapped value.
        assert_eq!(spot_price_q64(MAX_VIRT_COLLATERAL, 1), u128::MAX);
        assert_eq!(spot_price_q64(MAX_VIRT_COLLATERAL, VT0), u128::MAX);
        assert_eq!(spot_price_q64(u128::MAX, VT0), u128::MAX);

        // Wrapped value the unclamped `<< 64` would yield at the boundary: for
        // Vc == 2^64 the shift overflows to 0, so the (poisoned) price would be 0 -
        // proving the saturation is load-bearing, not cosmetic.
        assert_eq!(MAX_VIRT_COLLATERAL.wrapping_shl(64), 0);

        // Just under the boundary stays priceable (no saturation) and finite.
        let just_under = MAX_VIRT_COLLATERAL - 1;
        let p = spot_price_q64(just_under, VT0);
        assert!(p < u128::MAX, "price below the domain boundary must not saturate");
        assert_eq!(p, (just_under << 64) / VT0);
    }

    #[test]
    fn invariant_never_weakens_across_buys() {
        let k = VT0.checked_mul(VC0).unwrap();
        let mut vt = VT0;
        let mut vc = VC0;
        for _ in 0..200 {
            let (out, _fee, c_eff) = buy_tokens_out(vt, vc, 30, 123_457);
            if out == 0 {
                break;
            }
            vt -= out;
            vc += c_eff;
            assert!(vt.checked_mul(vc).unwrap() >= k, "Vt*Vc must stay >= k");
        }
    }

    #[test]
    fn sell_inverts_buy_and_is_pool_safe() {
        // Buy then immediately sell: seller must not extract more collateral than
        // they put in (fees + rounding favor the pool).
        let fee_bps = 30;
        let (out, _fee, c_eff) = buy_tokens_out(VT0, VC0, fee_bps, 1_000_000);
        let vt = VT0 - out;
        let vc = VC0 + c_eff;
        let (to_seller, _sfee, c_out_raw) = sell_collateral_out(vt, vc, fee_bps, out);
        assert!(c_out_raw <= c_eff, "round-trip must not mint collateral for the pool's loss");
        assert!(to_seller <= 1_000_000, "seller cannot profit risklessly from a round trip");
    }

    #[test]
    fn sale_state_serde_roundtrip() {
        let s = SaleState {
            fee_bps: 30,
            virt_token: VT0,
            virt_collateral: VC0,
            k: VT0 * VC0,
            sale_reserve_initial: 1_000_000_000,
            sale_reserve: 1_000_000_000,
            status: SaleStatus::Open,
            ..Default::default()
        };
        let data: Data = (&s).into();
        let back = SaleState::try_from(&data).unwrap();
        assert_eq!(s, back);
        assert!(back.invariant_holds());
    }

    // ---- per-creator sale index ------------------------------------------

    const CREATOR_A: AccountId = AccountId::new([1u8; 32]);
    const CREATOR_B: AccountId = AccountId::new([2u8; 32]);
    const PROGRAM_A: ProgramId = [7u32; 8];
    const PROGRAM_B: ProgramId = [8u32; 8];

    fn index_with(n: usize) -> CreatorIndex {
        let mut index = CreatorIndex::new(CREATOR_A);
        for i in 0..n {
            index.push(AccountId::new([u8::try_from(i % 251).unwrap(); 32]));
        }
        index
    }

    #[test]
    fn creator_index_roundtrips_and_keeps_order() {
        let mut index = CreatorIndex::new(CREATOR_A);
        let first = AccountId::new([21u8; 32]);
        let second = AccountId::new([22u8; 32]);
        index.push(first);
        index.push(second);

        let data: Data = (&index).into();
        let back = CreatorIndex::try_from(&data).unwrap();
        assert_eq!(back, index);
        assert_eq!(back.creator, CREATOR_A);
        assert_eq!(
            back.sale_ids,
            vec![first, second],
            "ids must stay in creation order - the SDK lists them as history"
        );
    }

    /// The discriminant is the whole reason the SDK can read a speculative PDA
    /// safely. Corrupting only the magic must make the account undecodable: with
    /// the check removed this decodes fine and yields a plausible sale list.
    #[test]
    fn creator_index_rejects_a_foreign_account_that_happens_to_decode() {
        let index = index_with(2);
        let mut bytes: Vec<u8> = Data::from(&index).to_vec();
        bytes[0] ^= 0xff;
        let data = Data::try_from(bytes).unwrap();
        assert!(
            CreatorIndex::try_from(&data).is_err(),
            "an account without the index magic must not decode as an index"
        );
    }

    /// Same for the version: a future layout must fail loudly here rather than
    /// be reinterpreted through this build's field order.
    #[test]
    fn creator_index_rejects_an_unknown_version() {
        let index = index_with(2);
        let mut bytes: Vec<u8> = Data::from(&index).to_vec();
        // magic is bytes 0..8, version the little-endian u16 at 8..10.
        bytes[8] = bytes[8].wrapping_add(1);
        let data = Data::try_from(bytes).unwrap();
        assert!(CreatorIndex::try_from(&data).is_err());
    }

    /// Trailing bytes are rejected too (Borsh is exact), so an index cannot be
    /// smuggled inside a longer account.
    #[test]
    fn creator_index_rejects_trailing_bytes() {
        let mut bytes: Vec<u8> = Data::from(&index_with(1)).to_vec();
        bytes.push(0);
        let data = Data::try_from(bytes).unwrap();
        assert!(CreatorIndex::try_from(&data).is_err());
    }

    #[test]
    #[should_panic(expected = "creator sale index is full")]
    fn creator_index_push_rejects_going_over_the_cap() {
        let mut index = index_with(MAX_INDEXED_SALES);
        index.push(AccountId::new([99u8; 32]));
    }

    /// The cap is only worth anything if a FULL index still encodes: the whole
    /// point of asserting it in `push` is that the alternative failure is a
    /// `DataTooBigError` panic on every later `CreateSale`, from inside the
    /// encode, naming nothing. This pins the arithmetic behind the number.
    #[test]
    fn a_full_creator_index_still_fits_in_an_account() {
        let index = index_with(MAX_INDEXED_SALES);
        let data: Data = (&index).into();
        assert_eq!(data.as_ref().len(), 8 + 2 + 32 + 4 + 32 * MAX_INDEXED_SALES);
        assert!(
            data.as_ref().len() <= 100 * 1024,
            "a full index must stay under Data's DATA_MAX_LENGTH"
        );
        assert_eq!(CreatorIndex::try_from(&data).unwrap().sale_ids.len(), MAX_INDEXED_SALES);
    }

    /// One index per (program, creator): different creators must never share a
    /// list, and the two launchpad programs must never share one either.
    #[test]
    fn creator_index_pda_is_unique_per_program_and_creator() {
        let a = compute_creator_index_pda(PROGRAM_A, CREATOR_A);
        assert_ne!(a, compute_creator_index_pda(PROGRAM_A, CREATOR_B));
        assert_ne!(a, compute_creator_index_pda(PROGRAM_B, CREATOR_A));
        assert_eq!(a, compute_creator_index_pda(PROGRAM_A, CREATOR_A), "derivation is stable");
    }

    /// The index seed shares its shape (32-byte id + 16-byte tag) with the two
    /// vault seeds, so the tag is the only thing keeping an index off a vault's
    /// address. Checked against a creator id used as a sale id, which is the
    /// collision this rules out.
    #[test]
    fn creator_index_seed_does_not_collide_with_the_vault_seeds() {
        let seed = compute_creator_index_pda_seed(CREATOR_A);
        assert_ne!(seed, compute_token_vault_pda_seed(CREATOR_A));
        assert_ne!(seed, compute_collateral_vault_pda_seed(CREATOR_A));
    }

    #[test]
    fn sold_bps_tracks_progress() {
        let mut s = SaleState {
            sale_reserve_initial: 1_000,
            sale_reserve: 1_000,
            ..Default::default()
        };
        assert_eq!(s.sold_bps(), 0);
        s.sale_reserve = 750;
        assert_eq!(s.sold_bps(), 2_500); // 25%
        s.sale_reserve = 0;
        assert_eq!(s.sold_bps(), 10_000); // 100%
    }
}
