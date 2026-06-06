//! Property-based tests for the LBP weighted-pool math - the integer Q64.64
//! `pow`/weight/price path is the program's subtlest (and most vault-drain-
//! relevant) code, so these check it against a floating-point reference and
//! assert the economic invariants across the realistic input domain.

use proptest::prelude::*;

use crate::{
    buy_tokens_out, close_fee,
    fixed::{pow_q64, ONE},
    weight_token_q64, FEE_BPS_DENOMINATOR, MAX_DURATION_MS, MAX_FEE_BPS,
};

/// A Q64.64 weight strictly inside `(0, 1)` - `num/10000` for `num ∈ 1..=9999`.
fn weight_q64() -> impl Strategy<Value = u128> {
    (1u128..=9_999u128).prop_map(|num| (num << 64) / 10_000)
}

proptest! {
    /// Token weight interpolates monotonically and stays within `[w_end,
    /// w_start]` for a declining schedule. The span ranges up to the full
    /// `MAX_DURATION_MS` domain `create_sale` permits (and `t_start` near
    /// `u64::MAX`), so the `delta * elapsed` interpolation is exercised at the
    /// edge where it must stay in-range without wrapping.
    #[test]
    fn weight_monotonic_and_bounded(
        w_start in weight_q64(),
        w_end in weight_q64(),
        t0 in 0u64..=(u64::MAX - MAX_DURATION_MS),
        span in 1u64..=MAX_DURATION_MS,
        // Sample times across the whole schedule (and a little past each end) so
        // the before/after-clamp and interior branches are all hit.
        a in 0u64..=u64::MAX,
        b in 0u64..=u64::MAX,
    ) {
        prop_assume!(w_start > w_end); // declining LBP
        let (t_start, t_end) = (t0, t0 + span);
        let (ta, tb) = (a.min(b), a.max(b));
        let wa = weight_token_q64(w_start, w_end, t_start, t_end, ta);
        let wb = weight_token_q64(w_start, w_end, t_start, t_end, tb);
        prop_assert!(wa >= wb, "weight rose over time: {wa} < {wb}");
        prop_assert!(wa <= w_start && wa >= w_end, "weight left [w_end, w_start]");
        prop_assert!(wb <= w_start && wb >= w_end, "weight left [w_end, w_start]");
    }

    /// At-close fee rounds up (by at most one unit) and never exceeds the balance.
    #[test]
    fn close_fee_rounds_up_and_bounded(
        balance in 0u128..=1_000_000_000_000u128,
        fee_bps in 0u128..=MAX_FEE_BPS,
    ) {
        let fee = close_fee(balance, fee_bps);
        let floor = balance.saturating_mul(fee_bps) / FEE_BPS_DENOMINATOR;
        prop_assert!(fee >= floor && fee <= floor + 1, "fee not a 1-unit round-up of the floor");
        prop_assert!(fee <= balance, "fee {fee} exceeds the balance {balance}");
    }

    /// `pow_q64(base, exp)` matches an `f64` reference within tolerance over the
    /// realistic LBP domain (base a ratio in [0.01, 1], exp a weight ratio).
    #[test]
    fn pow_matches_reference(
        base_num in 100u128..=10_000u128, // base in [0.01, 1]
        exp_milli in 1u128..=10_000u128,  // exp in (0.001, 10]
    ) {
        let base = (base_num << 64) / 10_000;
        let exp = (exp_milli << 64) / 1_000;
        let got = pow_q64(base, exp) as f64 / ONE as f64;
        let want = (base as f64 / ONE as f64).powf(exp as f64 / ONE as f64);
        prop_assert!(
            (got - want).abs() <= 1e-4 + 2e-3 * want,
            "pow(base={base}, exp={exp}) = {got}, reference {want}"
        );
        prop_assert!(got <= 1.0 + 1e-9, "pow of a base ≤ 1 must stay ≤ 1, got {got}");
    }

    /// The same collateral buys at least as many tokens later in a declining-
    /// weight pool - the price falls over time (the core LBP property).
    #[test]
    fn same_collateral_buys_more_as_price_falls(
        rt in 1_000_000u128..=1_000_000_000u128,
        rc in 1_000u128..=1_000_000u128,
        c_in in 1u128..=100_000u128,
        w_start in weight_q64(),
        w_end in weight_q64(),
    ) {
        prop_assume!(w_start > w_end);
        let (t_start, t_end) = (0u64, 1_000_000u64);
        let w_early = weight_token_q64(w_start, w_end, t_start, t_end, 100_000);
        let w_late = weight_token_q64(w_start, w_end, t_start, t_end, 900_000);
        prop_assume!(w_early < ONE && w_late > 0 && w_late < ONE);
        let early = buy_tokens_out(rt, rc, w_early, c_in);
        let late = buy_tokens_out(rt, rc, w_late, c_in);
        prop_assert!(late >= early, "price did not fall over time: late {late} < early {early}");
        prop_assert!(late <= rt, "tokens out exceeds the reserve");
    }
}
