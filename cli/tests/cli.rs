//! End-to-end tests for the `lpad` binary.
//!
//! These drive the actual compiled CLI (`CARGO_BIN_EXE_lpad`) and assert its
//! stdout/stderr/exit. Offline commands are checked against the *same* on-chain
//! libraries the CLI uses (so a pricing/PDA regression fails here). Online
//! commands are exercised on their error paths (the happy paths need a live
//! wallet + sequencer - covered by the bootstrap e2e, not here).

use std::path::PathBuf;
use std::process::{Command, Output};

use lpad_cli::fmt::{hex_account, parse_account, parse_program, parse_weight_q64, q64_to_f64};
use lee_core::account::AccountId;

fn lpad(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_lpad"))
        .args(args)
        // Don't let an ambient wallet home leak into the "no wallet" error tests.
        .env_remove("LEE_WALLET_HOME_DIR")
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

/// The leaf command names clap exposes under `path` (`&[]` = top level), read
/// out of `--help` rather than a hand-kept list, so a command added without a
/// test shows up as a count mismatch instead of silently shipping untested.
///
/// Group entries (`bc`, `lbp`) and clap's own `help` are excluded: the first two
/// are recursed into by the caller, and `help` is not a command of ours. Only
/// lines indented exactly two spaces are read, so a wrapped description line
/// (indented further) can never be mistaken for a command name.
fn subcommand_names(path: &[&str]) -> Vec<String> {
    let mut args = path.to_vec();
    args.push("--help");
    let o = lpad(&args);
    assert!(o.status.success(), "`{path:?} --help` failed: {}", se(&o));
    so(&o)
        .lines()
        .skip_while(|l| !l.starts_with("Commands:"))
        .skip(1)
        .take_while(|l| !l.trim().is_empty())
        .filter(|l| l.starts_with("  ") && !l.starts_with("   "))
        .filter_map(|l| l.split_whitespace().next())
        .filter(|n| !matches!(*n, "help" | "bc" | "lbp"))
        .map(str::to_owned)
        .collect()
}

/// The whole command surface, in one list: top level + `bc` + `lbp`.
///
/// Two private modes now coexist and both must stay reachable - `buy-private`
/// (the `deshield → public buy → re-shield` saga) and `buy-disposable` (one
/// atomic transaction whose buyer-side holdings are private account slots). The
/// saga is the mode proven against a real sequencer, so a change that dropped it
/// in favour of the newer one would be a regression, not a cleanup; this test is
/// what says so.
#[test]
fn command_surface_is_the_expected_51() {
    let mut all = subcommand_names(&[]);
    all.extend(subcommand_names(&["bc"]));
    all.extend(subcommand_names(&["lbp"]));
    assert_eq!(
        all.len(),
        51,
        "the CLI's command count changed (found {all:?}) - if that was intended, update this \
         count, cli/README.md and the CHANGELOG together"
    );
    // Both programs escrow their protocol fee in the collateral vault - the
    // bonding curve on every disposable buy, the LBP at withdrawal - and
    // `sweep-treasury` is the ONLY instruction on either that pays it out.
    // Losing one strands that program's escrow with no way to reach the treasury,
    // so both must exist and neither may be dropped as a duplicate of the other.
    for want in ["sweep-treasury", "buy", "buy-ata", "buy-private", "buy-disposable"] {
        assert_eq!(
            all.iter().filter(|n| *n == want).count(),
            2,
            "`{want}` must exist on BOTH `bc` and `lbp`; found {all:?}"
        );
    }
}

#[test]
fn bc_buy_private_errors_without_wallet() {
    // With valid args + a guaranteed-missing config, the command must parse and
    // then error cleanly at wallet resolution.
    let h = "11".repeat(32);
    let o = lpad(&[
        "bc", "buy-private", "--program", &h, "--sale", &h, "--user-collateral", &h,
        "--user-token", &h, "--in", "100", "--config", "/nonexistent/lpad/wallet_config.json",
    ]);
    assert!(!o.status.success());
    assert!(se(&o).contains("no wallet config at"), "expected wallet-resolve error, got: {}", se(&o));
}

/// The second private mode parses the same flags as the first and reaches the
/// same clean wallet-resolution error. Both modes stay: `buy-disposable` is
/// additive and is not yet validated against a real sequencer, so nothing may be
/// deleted in favour of it.
#[test]
fn buy_disposable_errors_without_wallet_on_both_programs() {
    let h = "11".repeat(32);
    for (group, target) in [("bc", "--sale"), ("lbp", "--pool")] {
        let o = lpad(&[
            group, "buy-disposable", "--program", &h, target, &h, "--user-collateral", &h,
            "--user-token", &h, "--in", "100", "--config", "/nonexistent/lpad/wallet_config.json",
        ]);
        assert!(!o.status.success(), "{group} buy-disposable unexpectedly succeeded");
        assert!(
            se(&o).contains("no wallet config at"),
            "expected wallet-resolve error from {group} buy-disposable, got: {}",
            se(&o)
        );
    }
}

/// The treasury has to exist BEFORE the sale does - both guests reject a
/// `CreateSale` whose treasury account is not already an initialised fungible
/// holding of the collateral definition, and `treasury_id` can never be changed
/// afterwards. That ordering is invisible from the flag list, and a creator who
/// learns it from a rejected transaction has already paid for the lesson, so
/// `--help` has to name both the precondition and the command that satisfies it.
/// `init-holding` is the remedy every treasury error message points at; if it is
/// ever renamed, this fails on both programs at once.
#[test]
fn create_sale_help_names_the_treasury_precondition_and_its_remedy() {
    for group in ["bc", "lbp"] {
        // Long help: the command's own explanation of the ordering.
        let o = lpad(&[group, "create-sale", "--help"]);
        assert!(o.status.success(), "`{group} create-sale --help` failed: {}", se(&o));
        let h = so(&o);
        assert!(
            h.contains("EXIST FIRST"),
            "{group} create-sale --help must say the treasury has to exist first: {h}"
        );
        assert!(
            h.contains("init-holding"),
            "{group} create-sale --help must name the command that creates the treasury: {h}"
        );
        // Short help: clap drops everything but the first line of the command
        // doc, so the flag's OWN help has to carry the same two facts or `-h`
        // users see a bare `--treasury <TREASURY>` and learn the rule from a
        // rejected transaction.
        let o = lpad(&[group, "create-sale", "-h"]);
        assert!(o.status.success(), "`{group} create-sale -h` failed: {}", se(&o));
        let h = so(&o);
        assert!(
            h.contains("EXISTING fungible holding") && h.contains("init-holding"),
            "{group} create-sale -h must document --treasury as an existing holding: {h}"
        );
    }
    // And the remedy is a real command, taking the definition the error tells
    // the user to pass.
    let o = lpad(&["init-holding", "--help"]);
    assert!(o.status.success(), "`init-holding --help` failed: {}", se(&o));
    assert!(so(&o).contains("--token-def"), "init-holding must still take --token-def: {}", so(&o));
}

/// The two discovery commands must keep BOTH paths reachable from the command
/// line, and must keep saying which is which.
///
/// The default path is now the programs' own per-creator index: one read per
/// account, and authoritative for the deployed program id. That makes
/// `--refresh` look redundant, and it is not - it is the only way to reach a
/// sale created by an lpad build from before the index existed, because nothing
/// on chain recorded those. If a later change removes the flag as dead weight,
/// those sales become unlistable with no way to ask for them, so pin it here
/// along with the sentence that explains why it is slow and when it is needed.
#[test]
fn my_sales_and_my_pools_keep_the_pre_index_fallback_reachable() {
    for cmd in ["my-sales", "my-pools"] {
        let o = lpad(&[cmd, "--help"]);
        assert!(o.status.success(), "`{cmd} --help` failed: {}", se(&o));
        let h = so(&o);
        assert!(h.contains("--refresh"), "{cmd} must keep the pre-index fallback flag: {h}");
        assert!(h.contains("--deep"), "{cmd} must keep the unpruned-creator flag: {h}");
        assert!(
            h.contains("index"),
            "{cmd} --help must say the on-chain index is what it reads: {h}"
        );
        assert!(
            h.contains("older lpad build") || h.contains("before the on-chain index"),
            "{cmd} --help must say what --refresh is for now: {h}"
        );
        assert!(h.contains("slow"), "{cmd} --help must warn that the fallback is slow: {h}");
    }
}

/// `SweepTreasury` has no signer slot on chain - on EITHER program - so neither
/// command may ask for a creator. A `--creator` flag reappearing here would mean
/// the fee settlement had been re-attached to an authority, which is exactly the
/// shape whose failure mode (an unusable treasury reverting the creator's payout
/// with it) splitting the instruction out was meant to end. The LBP is the one
/// that was still live: its fee was a leg of `Withdraw`, so a dead treasury
/// locked the whole raise, not just the fee.
#[test]
fn sweep_treasury_needs_no_creator_and_errors_without_wallet_on_both_programs() {
    let h = "11".repeat(32);
    for (group, target) in [("bc", "--sale"), ("lbp", "--pool")] {
        let help = lpad(&[group, "sweep-treasury", "--help"]);
        assert!(help.status.success(), "`{group} sweep-treasury --help` failed: {}", se(&help));
        assert!(
            !so(&help).contains("--creator"),
            "{group} sweep-treasury is permissionless and must take no creator flag: {}",
            so(&help)
        );
        let o = lpad(&[
            group, "sweep-treasury", "--program", &h, target, &h,
            "--config", "/nonexistent/lpad/wallet_config.json",
        ]);
        assert!(!o.status.success(), "{group} sweep-treasury unexpectedly succeeded");
        assert!(
            se(&o).contains("no wallet config at"),
            "expected wallet-resolve error from {group} sweep-treasury, got: {}",
            se(&o)
        );
    }
}

/// The help for both sweeps must not still be selling the bootstrap the guests
/// deleted.
///
/// Until this release, `sweep-treasury` accepted one signature - the treasury's
/// own - so the fee transfer could CREATE a treasury that had never been
/// initialised. That is gone: `create-sale` now requires an already-initialised
/// holding, and both guests `assert_ne!` on a DEFAULT-owned treasury before they
/// look at anything else. An operator who believes the old help does the one
/// thing that cannot work, on the one balance in either program that can be lost
/// for good - so the help promising it is worse than no help at all.
///
/// Asserted as an absence because that is the failure mode: nobody deletes a
/// paragraph they are not looking at, and this text has now outlived the
/// behaviour it described once.
#[test]
fn neither_sweep_help_still_offers_to_sign_for_the_treasury() {
    for group in ["bc", "lbp"] {
        let o = lpad(&[group, "sweep-treasury", "--help"]);
        assert!(o.status.success(), "`{group} sweep-treasury --help` failed: {}", se(&o));
        let h = so(&o);
        let lower = h.to_lowercase();
        for gone in [
            "signs for the treasury automatically",
            "sign for itself",
            "signs for itself",
            "wallet holding it",
            "must be sent from the wallet",
            "must come from the wallet",
            "created by the fee transfer",
        ] {
            assert!(
                !lower.contains(gone),
                "{group} sweep-treasury --help still advertises the removed self-signing \
                 bootstrap ({gone:?}): {h}"
            );
        }
        // And it has to say the true thing in its place, or the removal just
        // leaves a silence an operator fills with the old behaviour.
        assert!(
            lower.contains("nothing signs for the treasury"),
            "{group} sweep-treasury --help must say nothing signs for the treasury: {h}"
        );
        assert!(
            lower.contains("create-sale"),
            "{group} sweep-treasury --help must point at where an unusable treasury is caught \
             instead (create-sale): {h}"
        );
    }
}

/// A `--private` launch's creator is not recoverable from anything but the
/// launch's own output, so the help has to say the output exists and has to be
/// kept - and `bc close`/`bc withdraw` have to keep demanding exactly the
/// accounts it prints.
///
/// The gap this pins: `create-token-sale --private` used to print the sale id
/// and nothing else, while `close` and `withdraw` required a creator and a
/// creator token holding that no lpad command anywhere rendered. The keys were
/// in the wallet; the ids were unknowable; the raise was unwithdrawable.
#[test]
fn create_token_sale_help_says_it_prints_the_accounts_close_and_withdraw_need() {
    let o = lpad(&["bc", "create-token-sale", "--help"]);
    assert!(o.status.success(), "`bc create-token-sale --help` failed: {}", se(&o));
    let h = so(&o).to_lowercase();
    assert!(h.contains("creator"), "help must name what it prints: {h}");
    assert!(
        h.contains("bc close --creator") && h.contains("--creator-token"),
        "help must say which commands need those ids: {h}"
    );
    assert!(
        h.contains("keep both to yourself") || h.contains("keep them to yourself"),
        "help must say not to publish them beside the sale id: {h}"
    );
    // And the two commands still take them, so the help is not describing a
    // contract that moved.
    for (cmd, flags) in [
        ("close", vec!["--creator"]),
        ("withdraw", vec!["--creator", "--creator-token", "--creator-collateral"]),
    ] {
        let o = lpad(&["bc", cmd, "--help"]);
        assert!(o.status.success(), "`bc {cmd} --help` failed: {}", se(&o));
        for f in flags {
            assert!(so(&o).contains(f), "`bc {cmd}` must still take {f}: {}", so(&o));
        }
    }
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

// ---------------------------------------------------------------------------
// Network selection (`lpad network`, the first-run picker, the no-TTY rule)
// ---------------------------------------------------------------------------

/// A throwaway wallet directory holding only a `wallet_config.json`.
///
/// That single file is all `WalletPaths::resolve` needs to get past the "no
/// wallet config" error, which is exactly what puts a run at the point where the
/// first-run picker would fire - the point these tests are about. Its sequencer
/// is a closed port, so nothing here can reach a chain even if a code path tried.
struct TempWallet(PathBuf);

impl TempWallet {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("lpad-cli-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp wallet dir");
        std::fs::write(
            dir.join("wallet_config.json"),
            r#"{"sequencers":[{"sequencer_addr":"http://127.0.0.1:1"}]}"#,
        )
        .expect("write wallet config");
        Self(dir)
    }
    fn config(&self) -> String {
        self.0.join("wallet_config.json").display().to_string()
    }
    /// Where the remembered choice would be written, if anything wrote one.
    fn marker(&self) -> PathBuf {
        self.0.join("network")
    }
}

impl Drop for TempWallet {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Nothing in a test run has a terminal, so every `lpad(...)` call here is
/// already the non-interactive case. This is the string the picker would print.
const MENU: &str = "Which sequencer?";

/// The load-bearing rule: a run with no TTY must never wait on stdin. `Command`
/// gives the child a null stdin, so a version that prompted would hang here
/// until the harness killed it - and scripts/test-all-cli.sh and the e2e gate
/// would hang the same way in CI.
#[test]
fn network_without_a_tty_does_not_prompt() {
    let w = TempWallet::new("no-tty");
    let o = lpad(&["network", "--config", &w.config()]);
    assert!(!o.status.success(), "expected a clean failure, got: {}", so(&o));
    let err = se(&o);
    assert!(!err.contains(MENU) && !so(&o).contains(MENU), "the picker ran without a TTY: {err}");
    assert!(err.contains("no terminal to ask on"), "stderr: {err}");
    // The message has to be actionable, i.e. name the file to write and a value
    // to write into it.
    assert!(err.contains(&w.marker().display().to_string()), "stderr: {err}");
    assert!(err.contains("--network"), "stderr: {err}");
    assert!(!w.marker().exists(), "a failed pick must not leave a stored choice behind");
}

/// The same rule for an ordinary command: reaching a wallet that has never
/// chosen a network must not turn into a prompt, it must behave exactly as it
/// did before this feature existed (use the wallet config's own sequencer) and
/// fail, if at all, on its own terms.
#[test]
fn a_normal_command_without_a_tty_does_not_prompt() {
    let w = TempWallet::new("no-tty-status");
    let o = lpad(&["status", "--config", &w.config()]);
    assert!(!o.status.success(), "the closed-port sequencer should have failed the command");
    assert!(!se(&o).contains(MENU) && !so(&o).contains(MENU), "the picker ran: {}", se(&o));
    assert!(!w.marker().exists(), "no choice was made, so none may be stored");
    // And with no wallet at all the answer is still the bootstrap message, not a
    // question the user cannot act on yet.
    let o = lpad(&["status", "--config", "/nonexistent/lpad/wallet_config.json"]);
    assert!(se(&o).contains("no wallet config at"), "stderr: {}", se(&o));
    assert!(!se(&o).contains(MENU), "stderr: {}", se(&o));
}

/// `--json` reports the selection instead of asking for one, and says where the
/// answer came from.
#[test]
fn network_json_reports_the_current_selection() {
    let w = TempWallet::new("json");
    let o = lpad(&["--json", "network", "--config", &w.config()]);
    assert!(o.status.success(), "stderr: {}", se(&o));
    let v = json(&o);
    assert!(v["network"].is_null(), "nothing is stored yet: {v}");
    assert_eq!(v["source"], "wallet-config");
    assert_eq!(v["url"], "http://127.0.0.1:1", "the wallet config's own sequencer");
    assert_eq!(v["marker"], w.marker().display().to_string());
    assert!(!w.marker().exists(), "reporting must not write a choice");

    // A stored choice reads back in the config form it was written in - the
    // local URL included, which is the whole reason `local=<url>` is the stored
    // spelling rather than a bare `local`.
    std::fs::write(w.marker(), "local=http://127.0.0.1:39999\n").unwrap();
    let v = json(&lpad(&["--json", "network", "--config", &w.config()]));
    assert_eq!(v["network"], "local=http://127.0.0.1:39999");
    assert_eq!(v["url"], "http://127.0.0.1:39999");
    assert_eq!(v["display_name"], "Local");
    assert_eq!(v["source"], "stored");
}

/// `--network` is documented as a one-invocation override. If it ever started
/// writing itself into the marker, the same flag would mean two different things
/// depending on which command it was passed to.
#[test]
fn network_flag_overrides_without_storing_the_choice() {
    let w = TempWallet::new("override");
    std::fs::write(w.marker(), "testnet\n").unwrap();
    let v = json(&lpad(&["--json", "network", "--network", "paradox", "--config", &w.config()]));
    assert_eq!(v["network"], "paradox");
    assert_eq!(v["source"], "flag");
    assert_eq!(
        std::fs::read_to_string(w.marker()).unwrap().trim(),
        "testnet",
        "--network must not overwrite the remembered choice"
    );
}

/// A marker nobody can parse is an error, not a silent fallback: quietly
/// ignoring it would aim the next transaction at a different chain than the user
/// believes they picked, and that does not fail loudly - it fails as accounts
/// that read as absent.
#[test]
fn an_unreadable_stored_choice_is_an_error_not_a_silent_default() {
    let w = TempWallet::new("corrupt");
    std::fs::write(w.marker(), "nonsense\n").unwrap();
    let o = lpad(&["status", "--config", &w.config()]);
    assert!(!o.status.success());
    let err = se(&o);
    assert!(err.contains("unknown network"), "stderr: {err}");
    assert!(err.contains(&w.marker().display().to_string()), "stderr: {err}");
    // ...but `lpad network` exists to rewrite that file, so it must not be the
    // one command the broken file locks the user out of. Without a TTY it still
    // stops - at the "needs a terminal" message, which is a problem the user can
    // act on, rather than at the corrupt marker.
    let o = lpad(&["network", "--config", &w.config()]);
    assert!(se(&o).contains("no terminal to ask on"), "stderr: {}", se(&o));
}

// ---------------------------------------------------------------------------
// `lpad faucet` (the pinata proof-of-work claim)
//
// Nothing here claims from a real faucet - a claim spends a shared testnet's
// money and mutates it for everyone. What is testable without a chain is what
// this command promises before it ever reaches one: that it says what it is
// doing, that it parses a recipient without a round trip, and that it keeps its
// notices off a `--json` stdout.
// ---------------------------------------------------------------------------

/// The command name says nothing about mining, so `--help` has to. A user who
/// does not know a claim means solving a puzzle reads the busy CPU as a hang and
/// kills it - and the two things that actually change the wait (the difficulty
/// knob) and the semantics (no cooldown, so re-running is fine) are invisible
/// from the flags.
#[test]
fn faucet_help_explains_the_proof_of_work_and_the_recipient() {
    let o = lpad(&["faucet", "--help"]);
    assert!(o.status.success(), "`faucet --help` failed: {}", se(&o));
    let h = so(&o);
    assert!(h.contains("proof-of-work"), "help must say a claim is mined: {h}");
    assert!(h.contains("LPAD_FAUCET_MAX_ATTEMPTS"), "help must name the budget knob: {h}");
    assert!(h.contains("no cooldown"), "help must say re-running is allowed: {h}");
    assert!(h.contains("--to"), "help must document the recipient flag: {h}");
    // The recipient precondition is the trap that eats a first run: an account
    // that was never initialised cannot receive native LEZ at all.
    assert!(
        h.contains("authenticated-transfer"),
        "help must name the initialisation the recipient needs: {h}"
    );
}

/// A bad `--to` is caught by the parser, BEFORE any wallet or sequencer is
/// touched - which is why the dispatcher parses it up front. A run that had to
/// open a wallet first would report a typo as a connection failure.
#[test]
fn faucet_rejects_a_bad_recipient_before_touching_a_wallet() {
    let o = lpad(&["faucet", "--to", "____"]);
    assert!(!o.status.success());
    let err = se(&o);
    assert!(err.contains("bad account id"), "stderr: {err}");
    assert!(!err.contains("proof-of-work"), "nothing should have started mining: {err}");
}

#[test]
fn faucet_without_wallet_errors_cleanly() {
    let o = lpad(&["faucet", "--config", "/nonexistent/lpad/wallet_config.json"]);
    assert!(!o.status.success());
    assert!(se(&o).contains("no wallet config at"), "stderr: {}", se(&o));
}

/// With a wallet to open, the notice comes out BEFORE the slow part - and on
/// stderr, so a `--json` run's stdout stays parseable. This wallet points at a
/// closed port, so the command gets as far as the announcement and then fails on
/// the network, which is exactly the window being asserted.
#[test]
fn faucet_announces_the_mining_before_it_starts_and_never_prompts() {
    let w = TempWallet::new("faucet");
    let o = lpad(&["faucet", "--config", &w.config()]);
    assert!(!o.status.success(), "the closed-port sequencer should have failed the claim");
    let err = se(&o);
    assert!(err.contains("proof-of-work"), "the user must be told what the wait is: {err}");
    // The same no-TTY rule every other command follows: never wait on stdin.
    assert!(!err.contains(MENU) && !so(&o).contains(MENU), "the picker ran without a TTY: {err}");
    assert!(!w.marker().exists(), "a failed claim must not store a network choice");
}

/// `--json` keeps stdout machine-only: the human notice is on stderr, and a
/// failed claim writes no JSON at all (the error goes through the same
/// `error: ...` path as every other command).
#[test]
fn faucet_json_keeps_stdout_machine_readable() {
    let w = TempWallet::new("faucet-json");
    let o = lpad(&["--json", "faucet", "--config", &w.config()]);
    assert!(!o.status.success());
    assert!(so(&o).trim().is_empty(), "stdout must stay clean under --json: {}", so(&o));
    assert!(se(&o).contains("proof-of-work"), "the notice belongs on stderr: {}", se(&o));
}
