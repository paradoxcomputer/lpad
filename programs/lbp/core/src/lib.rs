//! Core types, weighted-pool math, and PDA derivation for the LPAD **LBP**
//! launchpad program (RFP-016).
//!
//! Pricing is a Balancer-style weight-shifting AMM: token weight declines
//! linearly over the sale window, so the price falls over time unless buying
//! pressure counteracts it. The out-given-in formula uses the integer Q64.64
//! power in [`fixed`]. Unlike the bonding curve, the protocol fee is collected
//! **at close** (the sale is time-bounded, so it is always collectible).

pub mod fixed;

use borsh::{BorshDeserialize, BorshSerialize};
use nssa_core::{
    account::{AccountId, AccountWithMetadata, Data},
    program::{PdaSeed, ProgramId},
};
use serde::{Deserialize, Serialize};
use spel_framework_macros::account_type;

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

/// Maximum sale-schedule span (`t_end_ms - t_start_ms`), ~10 years in ms. The
/// linear weight interpolation in [`weight_token_q64`] forms `delta * elapsed`
/// with `delta` up to ~2^64 (Q64.64 weight gap) and `elapsed` up to the span;
/// capping the span keeps that product far below `i128::MAX`, so it can never
/// wrap (the guest runs in release with overflow-checks OFF - see
/// [`fixed::div_to_q64`]). `create_sale` rejects a longer span at creation.
pub const MAX_DURATION_MS: u64 = 10 * 365 * 24 * 60 * 60 * 1_000;

/// Canonical on-chain Clock account (sequencer-updated each block).
pub const CLOCK_01: AccountId = AccountId::new(*b"/LEZ/ClockProgramAccount/0000001");

const TOKEN_VAULT_TAG: &[u8; 16] = b"lbp/token_vault\0";
const COLLATERAL_VAULT_TAG: &[u8; 16] = b"lbp/coll_vault\0\0";

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

#[account_type]
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

#[account_type]
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
    // ---- poke convenience (off-chain consumers) ----
    pub stored_w_token_q64: u128,
    pub stored_w_ts_ms: u64,
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

#[derive(Serialize, Deserialize)]
pub enum Instruction {
    CreateSale {
        collateral_definition_id: AccountId,
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
    /// Creator withdrawal: collateral net of the at-close fee + unsold tokens.
    Withdraw { deadline: u64 },
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
        + delta.checked_mul(elapsed).expect("weight interpolation overflows i128") / dur;
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
#[must_use]
pub fn buy_tokens_out(
    reserve_token: u128,
    reserve_collateral: u128,
    w_token_q64: u128,
    collateral_in: u128,
) -> u128 {
    assert!(w_token_q64 > 0 && w_token_q64 < ONE, "token weight must be in (0,1)");
    // Keep reserves inside the Q64.64 numerator domain so the price math cannot
    // silently overflow and misprice (vault-drain class bug). Reject cleanly.
    let total_collateral = reserve_collateral
        .checked_add(collateral_in)
        .expect("LBP collateral reserve + input overflows u128");
    assert!(
        total_collateral < MAX_RESERVE,
        "LBP buy would push the collateral reserve past the 64-bit Q64.64 domain"
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
    assert!(collateral_in < MAX_RESERVE, "LBP collateral input exceeds the 64-bit Q64.64 domain");
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
        .expect("collateral * fee_bps overflows u128")
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
        BorshSerialize::serialize(state, &mut data).expect("Serialization to Vec should not fail");
        Data::try_from(data).expect("Pool state encoded data should fit into Data")
    }
}

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
}
