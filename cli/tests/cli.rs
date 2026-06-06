//! End-to-end tests for the `lpad` binary.
//!
//! These drive the actual compiled CLI (`CARGO_BIN_EXE_lpad`) and assert its
//! stdout/stderr/exit. Offline commands are checked against the *same* on-chain
//! libraries the CLI uses (so a pricing/PDA regression fails here). Online
//! commands are exercised on their error paths (the happy paths need a live
//! wallet + sequencer - covered by the bootstrap e2e, not here).

use std::process::{Command, Output};

use lpad_cli::fmt::{hex_account, parse_account, parse_program, parse_weight_q64, q64_to_f64};
use nssa_core::account::AccountId;

fn lpad(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_lpad"))
        .args(args)
        // Don't let an ambient wallet home leak into the "no wallet" error tests.
        .env_remove("NSSA_WALLET_HOME_DIR")
        .output()
        .expect("spawn lpad")
}
fn so(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}
fn se(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}
fn json(o: &Output) -> serde_json::Value {
    serde_json::from_str(&so(o)).expect("stdout is valid JSON")
}
fn acct(b: u8) -> AccountId {
    AccountId::new([b; 32])
}

// ---------------------------------------------------------------------------
// Bonding-curve offline commands (deterministic, no wallet)
// ---------------------------------------------------------------------------

#[test]
fn bc_quote_human_matches_core() {
    let (t, f, c) = bonding_curve_core::buy_tokens_out(2_000_000, 50_000, 100, 5_000);
    let o = lpad(&["bc", "quote", "--vt", "2000000", "--vc", "50000", "--fee-bps", "100", "--in", "5000"]);
    assert!(o.status.success(), "stderr: {}", se(&o));
    let s = so(&o);
    assert!(s.contains("tokens_out") && s.contains(&t.to_string()), "{s}");
    assert!(s.contains("effective collateral") && s.contains(&c.to_string()), "{s}");
    let _ = f; // fee value is small/ambiguous in text; the JSON test asserts it exactly
}

#[test]
fn bc_quote_json_matches_core() {
    let (t, f, c) = bonding_curve_core::buy_tokens_out(2_000_000, 50_000, 100, 5_000);
    let o = lpad(&["--json", "bc", "quote", "--vt", "2000000", "--vc", "50000", "--fee-bps", "100", "--in", "5000"]);
    assert!(o.status.success(), "stderr: {}", se(&o));
    let v = json(&o);
    assert_eq!(v["tokens_out"], t.to_string());
    assert_eq!(v["fee"], f.to_string());
    assert_eq!(v["effective_collateral"], c.to_string());
    assert!(v["spot_price_before"].is_number());
    assert!(v["price_impact_pct"].is_number());
}

#[test]
fn bc_cost_matches_core_and_buys_at_least_target() {
    let (vt, vc, fee, target) = (2_000_000u128, 50_000u128, 100u128, 10_000u128);
    let cost = bonding_curve_core::buy_cost_for_tokens(vt, vc, fee, target);
    let o = lpad(&["--json", "bc", "cost", "--vt", "2000000", "--vc", "50000", "--fee-bps", "100", "--tokens", "10000"]);
    assert!(o.status.success(), "stderr: {}", se(&o));
    assert_eq!(json(&o)["collateral_in"], cost.to_string());
    // The quoted cost must actually buy >= the target (rounding favours the pool).
    let (got, _, _) = bonding_curve_core::buy_tokens_out(vt, vc, fee, cost);
    assert!(got >= target, "cost {cost} should buy >= {target}, got {got}");
}

#[test]
fn bc_cost_rejects_tokens_ge_vt() {
    let o = lpad(&["bc", "cost", "--vt", "100", "--vc", "50", "--tokens", "100"]);
    assert!(!o.status.success());
    assert!(se(&o).contains("tokens must be < virtual token reserve"), "stderr: {}", se(&o));
}

#[test]
fn bc_sell_quote_matches_core() {
    let (to_seller, f, raw) = bonding_curve_core::sell_collateral_out(2_000_000, 50_000, 100, 1_000);
    let o = lpad(&["--json", "bc", "sell-quote", "--vt", "2000000", "--vc", "50000", "--fee-bps", "100", "--tokens", "1000"]);
    assert!(o.status.success(), "stderr: {}", se(&o));
    let v = json(&o);
    assert_eq!(v["collateral_out"], to_seller.to_string());
    assert_eq!(v["fee"], f.to_string());
    assert_eq!(v["gross_out"], raw.to_string());
}

#[test]
fn bc_ids_matches_core_pda_derivation() {
    let prog_hex = "11".repeat(32);
    let (td_hex, cd_hex, cr_hex) = (hex_account(acct(7)), hex_account(acct(8)), hex_account(acct(9)));
    let prog = parse_program(&prog_hex).unwrap();
    let (td, cd, cr) = (parse_account(&td_hex).unwrap(), parse_account(&cd_hex).unwrap(), parse_account(&cr_hex).unwrap());
    let sale = bonding_curve_core::compute_sale_pda(prog, td, cd, cr, 0);
    let o = lpad(&["--json", "bc", "ids", "--program", &prog_hex, "--token-def", &td_hex, "--collateral-def", &cd_hex, "--creator", &cr_hex]);
    assert!(o.status.success(), "stderr: {}", se(&o));
    let v = json(&o);
    assert_eq!(v["sale"], hex_account(sale));
    assert_eq!(v["token_vault"], hex_account(bonding_curve_core::compute_token_vault_pda(prog, sale)));
    assert_eq!(v["collateral_vault"], hex_account(bonding_curve_core::compute_collateral_vault_pda(prog, sale)));
}

// ---------------------------------------------------------------------------
// LBP offline commands
// ---------------------------------------------------------------------------

#[test]
fn lbp_weight_matches_core() {
    // Use the CLI's own weight parser so the comparison is exact.
    let ws = parse_weight_q64("0.99").unwrap();
    let we = parse_weight_q64("0.01").unwrap();
    let wt = lbp_core::weight_token_q64(ws, we, 0, 1000, 500);
    let o = lpad(&["--json", "lbp", "weight", "--w-start", "0.99", "--w-end", "0.01", "--t-start", "0", "--t-end", "1000", "--at", "500"]);
    assert!(o.status.success(), "stderr: {}", se(&o));
    let v = json(&o);
    let got = v["w_token"].as_f64().unwrap();
    assert!((got - q64_to_f64(wt)).abs() < 1e-9, "got {got}, exp {}", q64_to_f64(wt));
    // weights sum to ~1
    assert!((got + v["w_collateral"].as_f64().unwrap() - 1.0).abs() < 1e-9);
}

#[test]
fn lbp_quote_matches_core() {
    let ws = parse_weight_q64("0.8").unwrap();
    let we = parse_weight_q64("0.2").unwrap();
    let wt = lbp_core::weight_token_q64(ws, we, 0, 1000, 250);
    let tokens_out = lbp_core::buy_tokens_out(1_000_000, 100_000, wt, 5_000);
    let o = lpad(&[
        "--json", "lbp", "quote", "--reserve-token", "1000000", "--reserve-collateral", "100000",
        "--w-start", "0.8", "--w-end", "0.2", "--t-start", "0", "--t-end", "1000", "--at", "250", "--in", "5000",
    ]);
    assert!(o.status.success(), "stderr: {}", se(&o));
    let v = json(&o);
    assert_eq!(v["tokens_out"], tokens_out.to_string());
    assert!(v["spot_price"].is_number());
}

#[test]
fn lbp_ids_matches_core_pda_derivation() {
    let prog_hex = "22".repeat(32);
    let (td_hex, cd_hex, cr_hex) = (hex_account(acct(1)), hex_account(acct(2)), hex_account(acct(3)));
    let prog = parse_program(&prog_hex).unwrap();
    let (td, cd, cr) = (parse_account(&td_hex).unwrap(), parse_account(&cd_hex).unwrap(), parse_account(&cr_hex).unwrap());
    let pool = lbp_core::compute_pool_pda(prog, td, cd, cr, 0);
    let o = lpad(&["--json", "lbp", "ids", "--program", &prog_hex, "--token-def", &td_hex, "--collateral-def", &cd_hex, "--creator", &cr_hex]);
    assert!(o.status.success(), "stderr: {}", se(&o));
    let v = json(&o);
    assert_eq!(v["pool"], hex_account(pool));
    assert_eq!(v["token_vault"], hex_account(lbp_core::compute_token_vault_pda(prog, pool)));
    assert_eq!(v["collateral_vault"], hex_account(lbp_core::compute_collateral_vault_pda(prog, pool)));
}

// ---------------------------------------------------------------------------
// Argument parsing / flags
// ---------------------------------------------------------------------------

#[test]
fn help_and_version_succeed() {
    let h = lpad(&["--help"]);
    assert!(h.status.success());
    assert!(so(&h).contains("Launchpad"), "{}", so(&h));
    assert!(lpad(&["--version"]).status.success());
}

#[test]
fn missing_required_arg_is_rejected() {
    let o = lpad(&["bc", "quote", "--vt", "100"]); // missing --vc and --in
    assert!(!o.status.success());
}

#[test]
fn bc_buy_private_errors_without_wallet() {
    // The `--atomic` flag was removed (single mode dropped; AtomicDisposable is
    // the only private mode). With valid args + a guaranteed-missing config, the
    // command must parse and then error cleanly at wallet resolution.
    let h = "11".repeat(32);
    let o = lpad(&[
        "bc", "buy-private", "--program", &h, "--sale", &h, "--user-collateral", &h,
        "--user-token", &h, "--in", "100", "--config", "/nonexistent/lpad/wallet_config.json",
    ]);
    assert!(!o.status.success());
    assert!(se(&o).contains("no wallet config at"), "expected wallet-resolve error, got: {}", se(&o));
}

// ---------------------------------------------------------------------------
// Online / error paths (no live chain needed)
// ---------------------------------------------------------------------------

#[test]
fn status_without_wallet_errors_cleanly() {
    // Hermetic: force a guaranteed-missing config so resolution deterministically
    // errors regardless of any bootstrapped ~/.lpad.
    let o = lpad(&["status", "--config", "/nonexistent/lpad/wallet_config.json"]);
    assert!(!o.status.success());
    assert!(se(&o).contains("no wallet config at"), "stderr: {}", se(&o));
}

#[test]
fn balance_bad_account_is_a_parse_error() {
    let o = lpad(&["balance", "--account", "____"]);
    assert!(!o.status.success());
    assert!(se(&o).contains("bad account id"), "stderr: {}", se(&o));
}

#[test]
fn program_id_rejects_unknown_program() {
    let o = lpad(&["program-id", "nonsense"]);
    assert!(!o.status.success());
    assert!(se(&o).contains("program must be"), "stderr: {}", se(&o));
}
