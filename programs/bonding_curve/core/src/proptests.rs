//! Property-based tests for the bonding-curve pricing math: the solvency +
//! monotonicity invariants must hold across the whole bounded input domain, not
//! just the point cases in `mod tests`. A failure here is either a real rounding
//! bug or a domain the asserts should reject.

use proptest::prelude::*;

use crate::{
    buy_cost_for_tokens, buy_fee, buy_tokens_out, sell_collateral_out, spot_price_q64,
    FEE_BPS_DENOMINATOR, MAX_FEE_BPS,
};

/// Bounded so `Vt * c_eff` and `Vc + c_eff` stay far inside `u128`
/// (`< 1e30 << 2^128`) and `Vc < 2^64` (the `spot_price_q64` Q64.64 domain).
const MAXV: u128 = 1_000_000_000_000_000; // 1e15

prop_compose! {
    fn curve_params()(
        vt in 2u128..=MAXV,
        vc in 1u128..=MAXV,
        fee_bps in 0u128..=MAX_FEE_BPS,
    ) -> (u128, u128, u128) {
        (vt, vc, fee_bps)
    }
}

proptest! {
    /// Buying then immediately selling never returns more collateral than was
    /// put in, and the pool never releases more than it took - it stays solvent.
    #[test]
    fn buy_then_sell_is_pool_safe(
        (vt, vc, fee_bps) in curve_params(),
        c_in in 1u128..=MAXV,
    ) {
        let (out, _fee, c_eff) = buy_tokens_out(vt, vc, fee_bps, c_in);
        prop_assume!(out > 0 && out < vt);
        let (to_seller, _sfee, c_out_raw) = sell_collateral_out(vt - out, vc + c_eff, fee_bps, out);
        prop_assert!(c_out_raw <= c_eff, "pool released more than it received: {c_out_raw} > {c_eff}");
        prop_assert!(to_seller <= c_in, "round trip minted a riskless profit: {to_seller} > {c_in}");
    }

    /// The inverse-cost query never under-charges: paying `cost_for(q)` buys at
    /// least `q` tokens (rounding favors the pool, never the buyer's shortfall).
    #[test]
    fn inverse_cost_buys_at_least_q(
        (vt, vc, fee_bps) in curve_params(),
        q in 1u128..=MAXV,
    ) {
        prop_assume!(q < vt);
        let cost = buy_cost_for_tokens(vt, vc, fee_bps, q);
        prop_assume!(cost > 0 && cost <= MAXV);
        let (out, _, _) = buy_tokens_out(vt, vc, fee_bps, cost);
        prop_assert!(out >= q, "underpriced: cost {cost} bought {out} < {q}");
    }

    /// The constant-product invariant `Vt*Vc` never weakens across a buy.
    #[test]
    fn invariant_never_weakens_on_buy(
        (vt, vc, fee_bps) in curve_params(),
        c_in in 1u128..=MAXV,
    ) {
        let k = vt.checked_mul(vc).unwrap();
        let (out, _fee, c_eff) = buy_tokens_out(vt, vc, fee_bps, c_in);
        prop_assume!(out > 0 && out < vt);
        prop_assert!((vt - out).checked_mul(vc + c_eff).unwrap() >= k, "Vt*Vc dropped below k");
    }

    /// The spot price is non-decreasing across a buy (supply-driven price rises).
    #[test]
    fn price_non_decreasing_on_buy(
        (vt, vc, fee_bps) in curve_params(),
        c_in in 1u128..=MAXV,
    ) {
        let before = spot_price_q64(vc, vt);
        let (out, _fee, c_eff) = buy_tokens_out(vt, vc, fee_bps, c_in);
        prop_assume!(out > 0 && out < vt);
        let after = spot_price_q64(vc + c_eff, vt - out);
        prop_assert!(after >= before, "price fell on a buy: {after} < {before}");
    }

    /// The per-swap fee always rounds UP (against the trader), by at most one unit.
    #[test]
    fn fee_rounds_against_trader(
        c_in in 0u128..=MAXV,
        fee_bps in 0u128..=MAX_FEE_BPS,
    ) {
        let fee = buy_fee(c_in, fee_bps);
        let floor = c_in.saturating_mul(fee_bps) / FEE_BPS_DENOMINATOR;
        prop_assert!(fee >= floor);
        prop_assert!(fee <= floor + 1, "fee rounded by more than one unit");
    }
}
