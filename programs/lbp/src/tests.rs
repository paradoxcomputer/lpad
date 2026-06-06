//! Host-logic unit tests for the LBP buy state machine (pure, no accounts).

use lbp_core::{PoolState, SaleStatus};

use crate::buy::apply_buy;

fn w(num: u128, den: u128) -> u128 {
    (num << 64) / den
}

fn sample_pool() -> PoolState {
    PoolState {
        fee_bps: 500,
        w_start_q64: w(99, 100),
        w_end_q64: w(1, 100),
        t_start_ms: 1_000,
        t_end_ms: 1_000_000,
        min_duration_ms: 0,
        reserve_token: 1_000_000_000,
        reserve_collateral: 50_000,
        status: SaleStatus::Open,
        ..Default::default()
    }
}

#[test]
fn buy_decreases_token_reserve_and_records() {
    let p = sample_pool();
    let out = apply_buy(p.clone(), 10_000, 0, 2_000, 1, true);
    assert!(out.tokens_out > 0);
    assert_eq!(out.state.reserve_token, p.reserve_token - out.tokens_out);
    assert_eq!(out.state.reserve_collateral, p.reserve_collateral + 10_000);
    assert_eq!(out.state.buy_count, 1);
    assert_eq!(out.state.cum_collateral_in, 10_000);
    assert_eq!(out.state.obs.len(), 1);
}

#[test]
#[should_panic(expected = "slippage")]
fn buy_reverts_on_slippage() {
    let p = sample_pool();
    let _ = apply_buy(p, 10_000, u128::MAX, 2_000, 1, true);
}

#[test]
#[should_panic(expected = "not started")]
fn buy_reverts_before_start() {
    let p = sample_pool();
    let _ = apply_buy(p, 10_000, 0, 500, 1, true);
}

#[test]
#[should_panic(expected = "ended")]
fn buy_reverts_after_end() {
    let p = sample_pool();
    let _ = apply_buy(p, 10_000, 0, 2_000_000, 1, true);
}

#[test]
#[should_panic(expected = "paused")]
fn buy_reverts_when_paused() {
    let mut p = sample_pool();
    p.paused = true;
    let _ = apply_buy(p, 10_000, 0, 2_000, 1, true);
}

#[test]
fn same_collateral_buys_more_later_as_price_falls() {
    // Two equal buys at different times against a fresh pool: the later one (lower
    // weight => lower price) should yield strictly more tokens.
    let p = sample_pool();
    let early = apply_buy(p.clone(), 10_000, 0, 100_000, 1, true).tokens_out;
    let late = apply_buy(p, 10_000, 0, 900_000, 1, true).tokens_out;
    assert!(late > early, "later buy gets more tokens as the LBP price falls: early={early} late={late}");
}

#[test]
fn per_block_ceiling_enforced_and_resets_each_block() {
    let mut p = sample_pool();
    let one_buy = apply_buy(p.clone(), 10_000, 0, 2_000, 1, true).tokens_out;
    p.block_token_ceiling = one_buy + 5;
    let s1 = apply_buy(p.clone(), 10_000, 0, 2_000, 1, true).state;
    assert_eq!(s1.block_window_id, 1);
    assert!(s1.block_sold <= p.block_token_ceiling);
    // A second buy in the SAME block exceeds the ceiling.
    let res = std::panic::catch_unwind(|| apply_buy(s1.clone(), 10_000, 0, 2_000, 1, true));
    assert!(res.is_err(), "second buy in same block must exceed the ceiling");
    // The same buy in a NEW block resets the window and succeeds.
    let s2 = apply_buy(s1, 10_000, 0, 2_000, 2, true).state;
    assert_eq!(s2.block_window_id, 2);
}

#[test]
fn fixed_price_mode_is_constant() {
    let mut p = sample_pool();
    p.fixed_price = true;
    p.fixed_price_q64 = (5u128 << 64) / 100; // 0.05 collateral per token
    let early = apply_buy(p.clone(), 10_000, 0, 100_000, 1, true).tokens_out;
    let late = apply_buy(p, 10_000, 0, 900_000, 1, true).tokens_out;
    assert_eq!(early, late, "fixed-price output independent of time");
    assert_eq!(early, (10_000u128 << 64) / ((5u128 << 64) / 100));
}
