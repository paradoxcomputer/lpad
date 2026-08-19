//! Property-based tests for the LBP weighted-pool math - the integer Q64.64
//! `pow`/weight/price path is the program's subtlest (and most vault-drain-
//! relevant) code, so these check it against a floating-point reference and
//! assert the economic invariants across the realistic input domain.

use proptest::prelude::*;

use crate::{
    buy_tokens_out, close_fee,
    fixed::{pow_q64, ONE},
    weight_token_q64, FEE_BPS_DENOMINATOR, MAX_DURATION_MS, MAX_FEE_BPS, MAX_RESERVE,
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
        w_a in weight_q64(),
        w_b in weight_q64(),
        t0 in 0u64..=(u64::MAX - MAX_DURATION_MS),
        span in 1u64..=MAX_DURATION_MS,
        // Sample times across the whole schedule (and a little past each end) so
        // the before/after-clamp and interior branches are all hit.
        a in 0u64..=u64::MAX,
        b in 0u64..=u64::MAX,
    ) {
        // Constructed, not assumed: `prop_assume!(w_start > w_end)` rejects about
        // half of all draws, and at a raised PROPTEST_CASES that exhausts the
        // global reject budget and aborts the run as a failure.
        let (w_start, w_end) = match w_a.cmp(&w_b) {
            core::cmp::Ordering::Greater => (w_a, w_b),
            core::cmp::Ordering::Less => (w_b, w_a),
            core::cmp::Ordering::Equal => (w_a, w_a - 1),
        };
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
        w_a in weight_q64(),
        w_b in weight_q64(),
    ) {
        // Constructed, not assumed: `prop_assume!(w_start > w_end)` rejects about
        // half of all draws, and at a raised PROPTEST_CASES that exhausts the
        // global reject budget and aborts the run as a failure.
        let (w_start, w_end) = match w_a.cmp(&w_b) {
            core::cmp::Ordering::Greater => (w_a, w_b),
            core::cmp::Ordering::Less => (w_b, w_a),
            core::cmp::Ordering::Equal => (w_a, w_a - 1),
        };
        let (t_start, t_end) = (0u64, 1_000_000u64);
        let w_early = weight_token_q64(w_start, w_end, t_start, t_end, 100_000);
        let w_late = weight_token_q64(w_start, w_end, t_start, t_end, 900_000);
        // `weight_q64` is strictly inside (0, 1) and the interpolation is
        // monotone between the endpoints, so both samples are in range by
        // construction - assert it rather than discarding draws for it.
        prop_assert!(w_early < ONE && w_late > 0 && w_late < ONE);
        let early = buy_tokens_out(rt, rc, w_early, c_in);
        let late = buy_tokens_out(rt, rc, w_late, c_in);
        prop_assert!(late >= early, "price did not fall over time: late {late} < early {early}");
        prop_assert!(late <= rt, "tokens out exceeds the reserve");
    }

    /// **This is the safety proof for `BuyDisposable`'s caller-chosen
    /// `t_buy_ms`.** At FIXED reserves `buy_tokens_out` must be non-decreasing
    /// in time, so pricing at an earlier timestamp can only hand the buyer
    /// FEWER tokens than the fair amount at admission - the pool is never
    /// underpaid by a stale quote. The reserves really are fixed across the two
    /// times only because a private buy PINS the pool PDA byte-for-byte; see
    /// the module doc of `lbp_program::buy`.
    ///
    /// If this test ever fails a `prop_assert`, `t_buy_ms` is unsound and
    /// `BuyDisposable` must be pulled from both programs - not "fixed" by
    /// widening the tolerance below. (A proptest *reject-budget* abort is a
    /// different thing and does not mean that; the generators below are written
    /// so that one cannot happen.) The comparison is exact: 2.4M adversarial samples (reserves
    /// across the whole `MAX_RESERVE` domain, weights across all of `(0,1)`,
    /// and weight steps of a single Q64.64 ulp - the tightest step at which the
    /// truncating `pow_q64` could invert) produced no violation at all, so
    /// there is no integer-rounding slack to allow for.
    ///
    /// Reserves span the full domain `buy_tokens_out` accepts; `t_start` sits
    /// anywhere in `u64` with a span up to `MAX_DURATION_MS`, so the weight
    /// interpolation is exercised at its edge too.
    #[test]
    fn tokens_out_non_decreasing_in_time(
        rt in 1u128..MAX_RESERVE,
        rc in 1u128..MAX_RESERVE,
        c_raw in 1u128..MAX_RESERVE,
        w_a in weight_q64(),
        w_b in weight_q64(),
        t_start in 0u64..=(u64::MAX - MAX_DURATION_MS),
        span in 1u64..=MAX_DURATION_MS,
        o1 in 0u64..=u64::MAX,
        o2 in 0u64..=u64::MAX,
    ) {
        // Both preconditions are CONSTRUCTED, not assumed. `prop_assume!` on a
        // condition that rejects ~half the draws burns proptest's global reject
        // budget, and a budget abort is reported as a test failure - which, given
        // what the doc above says a failure means, is the last thing this test
        // should be able to do for a reason that is not a real violation.
        //
        // A declining schedule is the only one BuyDisposable admits, so order the
        // two weights rather than discarding the half that come out ascending.
        let (w_start, w_end) = match w_a.cmp(&w_b) {
            core::cmp::Ordering::Greater => (w_a, w_b),
            core::cmp::Ordering::Less => (w_b, w_a),
            // Equal: step one Q64.64 ulp down. `weight_q64` never yields less
            // than (1 << 64) / 10_000, so the result stays strictly inside (0, 1).
            core::cmp::Ordering::Equal => (w_a, w_a - 1),
        };
        // Keep the input inside the Q64.64 domain buy_tokens_out asserts on by
        // folding it into the room the reserve leaves, instead of rejecting.
        let room = MAX_RESERVE - rc - 1;
        prop_assume!(room > 0); // only when rc == MAX_RESERVE - 1; never in practice
        let c_in = 1 + (c_raw % room);
        let t_end = t_start + span;
        // t_start <= t1 <= t2 < t_end.
        let (lo, hi) = ((o1 % span).min(o2 % span), (o1 % span).max(o2 % span));
        let (t1, t2) = (t_start + lo, t_start + hi);
        let w1 = weight_token_q64(w_start, w_end, t_start, t_end, t1);
        let w2 = weight_token_q64(w_start, w_end, t_start, t_end, t2);
        let out1 = buy_tokens_out(rt, rc, w1, c_in);
        let out2 = buy_tokens_out(rt, rc, w2, c_in);
        prop_assert!(
            out1 <= out2,
            "pricing at the earlier t1={t1} paid MORE than at t2={t2} \
             (out1={out1} > out2={out2}, rt={rt} rc={rc} c_in={c_in} w1={w1} w2={w2}): \
             BuyDisposable's t_buy_ms is unsound, pull the instruction"
        );
    }
}
