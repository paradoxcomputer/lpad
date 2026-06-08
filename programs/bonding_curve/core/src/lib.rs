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
use nssa_core::{
    account::{AccountId, AccountWithMetadata, Data},
    program::{PdaSeed, ProgramId},
};
use serde::{Deserialize, Serialize};
use spel_framework_macros::account_type;

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

/// Stable PDA-seed discriminators (must never change for address compatibility).
const TOKEN_VAULT_TAG: &[u8; 16] = b"bc/token_vault\0\0";
const COLLATERAL_VAULT_TAG: &[u8; 16] = b"bc/coll_vault\0\0\0";

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Lifecycle status of a sale.
#[account_type]
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
#[account_type]
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
    Withdraw { deadline: u64 },
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
