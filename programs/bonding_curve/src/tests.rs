//! Host-logic unit tests for the bonding-curve state machine (pure, no accounts).

use bonding_curve_core::{SaleState, SaleStatus};

use crate::buy::apply_buy;

fn sample_sale() -> SaleState {
    SaleState {
        fee_bps: 100, // 1%
        virt_token: 1_100_000_000,
        virt_collateral: 30_000_000,
        k: 1_100_000_000u128 * 30_000_000u128,
        sale_reserve_initial: 1_000_000_000,
        sale_reserve: 1_000_000_000,
        status: SaleStatus::Open,
        ..Default::default()
    }
}

#[test]
fn buy_updates_reserves_and_accounting() {
    let s = sample_sale();
    let out = apply_buy(s.clone(), 1_000_000, 0, Some(1_000));
    assert!(out.tokens_out > 0);
    assert_eq!(out.fee, (1_000_000u128 * 100).div_ceil(10_000));
    assert_eq!(out.state.sale_reserve, s.sale_reserve - out.tokens_out);
    assert_eq!(out.state.real_collateral, out.c_eff);
    assert_eq!(out.state.virt_token, s.virt_token - out.tokens_out);
    assert_eq!(out.state.virt_collateral, s.virt_collateral + out.c_eff);
    assert_eq!(out.state.cum_collateral_in, 1_000_000);
    assert_eq!(out.state.cum_fees, out.fee);
    assert_eq!(out.state.buy_count, 1);
    assert_eq!(out.state.obs.len(), 1);
}

#[test]
#[should_panic(expected = "slippage")]
fn buy_reverts_on_slippage() {
    let s = sample_sale();
    let _ = apply_buy(s, 1_000_000, u128::MAX, Some(1_000));
}

#[test]
#[should_panic(expected = "remaining sale reserve")]
fn buy_reverts_when_exceeding_reserve() {
    let mut s = sample_sale();
    s.sale_reserve = 10;
    let _ = apply_buy(s, 1_000_000, 0, Some(1_000));
}

#[test]
fn buy_auto_closes_on_supply_target() {
    // Drive the sale until the reserve is exhausted; the final buy must flip to Closed.
    let mut s = sample_sale();
    s.sale_reserve = s.sale_reserve.min(s.virt_token - 1);
    let mut closed = false;
    for _ in 0..100_000 {
        if s.sale_reserve == 0 {
            break;
        }
        let want = s.sale_reserve;
        let cost = bonding_curve_core::buy_cost_for_tokens(
            s.virt_token,
            s.virt_collateral,
            s.fee_bps,
            want,
        );
        let out = apply_buy(s.clone(), cost, 0, Some(1_000));
        s = out.state;
        if matches!(s.status, SaleStatus::Closed) {
            closed = true;
            assert_eq!(s.sale_reserve, 0);
            break;
        }
    }
    assert!(closed, "sale must auto-close when the supply target is reached");
}

#[test]
#[should_panic(expected = "not open")]
fn buy_reverts_when_closed() {
    let mut s = sample_sale();
    s.status = SaleStatus::Closed;
    let _ = apply_buy(s, 1_000_000, 0, Some(1_000));
}

#[test]
#[should_panic(expected = "ended")]
fn buy_reverts_after_end_timestamp() {
    let mut s = sample_sale();
    s.end_timestamp_ms = 5_000;
    let _ = apply_buy(s, 1_000_000, 0, Some(6_000));
}

#[test]
fn private_buy_ignores_end_timestamp_in_logic() {
    // The private path passes None for the clock; end-timestamp is enforced by the
    // SDK's validity window, not in-circuit. apply_buy must not panic on time here.
    let mut s = sample_sale();
    s.end_timestamp_ms = 5_000;
    let out = apply_buy(s, 1_000_000, 0, None);
    assert!(out.tokens_out > 0);
    assert!(out.state.obs.is_empty(), "private path records no timestamped observation");
}
