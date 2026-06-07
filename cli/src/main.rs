//! `lpad` - command-line client for the LPAD launchpad platform.
//!
//! Offline commands compute quotes, costs, weights, and PDA addresses for both
//! the bonding-curve (RFP-015) and LBP (RFP-016) programs directly from the
//! on-chain pricing libraries (byte-identical to the programs). Online commands
//! open the LEZ wallet to read sale/pool state and build/sign/submit
//! transactions (public path). See `online.rs`.

use clap::{Parser, Subcommand};
use nssa_core::account::AccountId;

use lpad_cli::fmt::{hex_account, parse_account, parse_program, parse_proof, parse_root, parse_weight_q64, q64_to_f64};
use lpad_cli::online::{self, WalletPaths};
use lpad_cli::ui;

#[derive(Parser, Debug)]
#[command(name = "lpad", version, about = "Logos Privacy-Preserving Token Launchpad CLI")]
struct Cli {
    /// Emit machine-readable JSON instead of human-readable text.
    #[arg(long, global = true)]
    json: bool,
    /// Wallet config path (default: $NSSA_WALLET_HOME_DIR/wallet_config.json).
    #[arg(long, global = true)]
    config: Option<String>,
    /// Wallet storage path (default: $NSSA_WALLET_HOME_DIR/storage.json).
    #[arg(long, global = true)]
    storage: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// [online] Sequencer block height + wallet sync status.
    Status,
    /// [online] Token balance of an account.
    Balance {
        #[arg(long)]
        account: String,
    },
    /// Print a program's deployed id (RISC0 image id) from its guest ELF.
    ProgramId {
        /// Which program: `bc`, `lbp`, `ata`, or `wlez`.
        which: String,
    },
    /// [online] Your token balances across every wallet account (public + shielded).
    MyBalance,
    /// [online] Bonding-curve sales your wallet created (derivable ids; no LP-0012 indexer).
    MySales,
    /// [online] LBP pools your wallet created (derivable ids; no LP-0012 indexer).
    MyPools,
    /// [online] Wrap native LEZ into WLEZ (the collateral token for native-LEZ sales).
    Wrap { #[arg(long)] amount: u128, #[arg(long)] from: Option<String> },
    /// [online] Unwrap WLEZ back into native LEZ.
    Unwrap { #[arg(long)] amount: u128 },
    /// [online] Shield a public token holding into a private (shielded) holding.
    Shield { #[arg(long = "token-def")] token_def: String, #[arg(long)] amount: u128 },
    /// [online] Deshield a shielded holding back into a public one.
    Deshield { #[arg(long = "token-def")] token_def: String, #[arg(long)] amount: u128 },
    /// [online] Shield native LEZ: wrap LEZ -> WLEZ then shield it (one shot).
    ShieldLez { #[arg(long)] amount: u128, #[arg(long)] from: Option<String> },
    /// [online] Deshield WLEZ back to native LEZ: deshield WLEZ then unwrap (one shot).
    DeshieldLez { #[arg(long)] amount: u128 },
    /// [online] Create your Associated Token Account for a token definition (idempotent).
    CreateAta {
        #[arg(long = "token-def")]
        token_def: String,
        #[arg(long)]
        owner: Option<String>,
        #[arg(long = "ata-program")]
        ata_program: Option<String>,
    },
    /// Bonding-curve (RFP-015) operations.
    #[command(subcommand)]
    Bc(BcCmd),
    /// LBP (RFP-016) operations.
    #[command(subcommand)]
    Lbp(LbpCmd),
}

#[derive(Subcommand, Debug)]
enum BcCmd {
    /// Quote a buy: tokens received for a collateral input.
    Quote { #[arg(long)] vt: u128, #[arg(long)] vc: u128, #[arg(long = "fee-bps", default_value_t = 0)] fee_bps: u128, #[arg(long = "in")] collateral_in: u128 },
    /// Inverse: exact collateral cost to buy a target token quantity.
    Cost { #[arg(long)] vt: u128, #[arg(long)] vc: u128, #[arg(long = "fee-bps", default_value_t = 0)] fee_bps: u128, #[arg(long)] tokens: u128 },
    /// Quote a sell: collateral received for selling tokens back.
    SellQuote { #[arg(long)] vt: u128, #[arg(long)] vc: u128, #[arg(long = "fee-bps", default_value_t = 0)] fee_bps: u128, #[arg(long)] tokens: u128 },
    /// Derive the sale + vault PDA addresses for a sale.
    Ids { #[arg(long)] program: Option<String>, #[arg(long = "token-def")] token_def: String, #[arg(long = "collateral-def")] collateral_def: String, #[arg(long)] creator: String, #[arg(long, default_value_t = 0)] nonce: u64 },
    /// [online] Read a sale's on-chain state.
    SaleInfo { #[arg(long)] sale: String },
    /// [online] Launch a token: mint (name+symbol) + open a sale raising in native LEZ.
    CreateTokenSale {
        #[arg(long)] name: String,
        #[arg(long)] symbol: String,
        #[arg(long)] supply: u128,
        #[arg(long = "sale-quantity")] sale_quantity: u128,
        #[arg(long = "dex-seed", default_value_t = 0)] dex_seed: u128,
        #[arg(long)] vt: u128,
        #[arg(long)] vc: u128,
        #[arg(long = "fee-bps", default_value_t = 0)] fee_bps: u128,
        #[arg(long)] creator: Option<String>,
        #[arg(long, help = "fund the deposit from a shielded holding via an unlinkable creator account")] private: bool,
        #[arg(long, default_value_t = 0)] nonce: u64,
    },
    /// [online] Create a new sale (deposits D+R project tokens).
    CreateSale {
        #[arg(long)] program: Option<String>,
        #[arg(long = "collateral-def")] collateral_def: String,
        #[arg(long)] treasury: String,
        #[arg(long = "creator-token-holding")] creator_token_holding: String,
        #[arg(long)] creator: String,
        #[arg(long = "sale-quantity")] sale_quantity: u128,
        #[arg(long = "dex-seed", default_value_t = 0)] dex_seed: u128,
        #[arg(long)] vt: u128,
        #[arg(long)] vc: u128,
        #[arg(long = "fee-bps", default_value_t = 0)] fee_bps: u128,
        #[arg(long = "one-directional", default_value_t = false)] one_directional: bool,
        #[arg(long = "end-ts", default_value_t = 0)] end_ts: u64,
        #[arg(long = "min-duration", default_value_t = 0)] min_duration: u64,
        #[arg(long, default_value_t = 0)] nonce: u64,
    },
    /// [online] Buy from a sale (public path).
    Buy { #[arg(long)] program: Option<String>, #[arg(long)] sale: String, #[arg(long = "buyer-collateral")] buyer_collateral: Option<String>, #[arg(long = "buyer-token")] buyer_token: Option<String>, #[arg(long = "in")] collateral_in: u128, #[arg(long = "min-out")] min_out: Option<u128>, #[arg(long = "slippage-bps", default_value_t = 100)] slippage_bps: u128 },
    /// [online] Buy via Associated Token Accounts (RFP Func: ATAs). `--fund-from` seeds the collateral ATA first.
    BuyAta { #[arg(long)] program: Option<String>, #[arg(long = "ata-program")] ata_program: Option<String>, #[arg(long)] sale: String, #[arg(long)] owner: Option<String>, #[arg(long = "fund-from")] fund_from: Option<String>, #[arg(long = "in")] collateral_in: u128, #[arg(long = "min-out")] min_out: Option<u128>, #[arg(long = "slippage-bps", default_value_t = 100)] slippage_bps: u128 },
    /// [online] Private buy: deshield → buy → re-shield via a fresh ephemeral account A.
    BuyPrivate { #[arg(long)] program: Option<String>, #[arg(long)] sale: String, #[arg(long = "user-collateral")] user_collateral: Option<String>, #[arg(long = "user-token")] user_token: Option<String>, #[arg(long = "in")] collateral_in: u128, #[arg(long = "min-out")] min_out: Option<u128>, #[arg(long = "slippage-bps", default_value_t = 100)] slippage_bps: u128 },
    /// [online] Sell tokens back into a sale (public path).
    Sell { #[arg(long)] program: Option<String>, #[arg(long)] sale: String, #[arg(long = "seller-token")] seller_token: String, #[arg(long = "seller-collateral")] seller_collateral: String, #[arg(long)] tokens: u128, #[arg(long = "min-out")] min_out: Option<u128>, #[arg(long = "slippage-bps", default_value_t = 100)] slippage_bps: u128 },
    /// [online] Sell via Associated Token Accounts (RFP Func: ATAs). `--fund-from` seeds the token ATA first.
    SellAta { #[arg(long)] program: Option<String>, #[arg(long = "ata-program")] ata_program: Option<String>, #[arg(long)] sale: String, #[arg(long)] owner: Option<String>, #[arg(long = "fund-from")] fund_from: Option<String>, #[arg(long)] tokens: u128, #[arg(long = "min-out")] min_out: Option<u128>, #[arg(long = "slippage-bps", default_value_t = 100)] slippage_bps: u128 },
    /// [online] Private sell: deshield tokens → public sell → re-shield collateral.
    SellPrivate { #[arg(long)] program: Option<String>, #[arg(long)] sale: String, #[arg(long = "user-token")] user_token: Option<String>, #[arg(long = "user-collateral")] user_collateral: Option<String>, #[arg(long)] tokens: u128, #[arg(long = "min-out")] min_out: Option<u128>, #[arg(long = "slippage-bps", default_value_t = 100)] slippage_bps: u128 },
    /// [online] Close a sale (creator).
    Close { #[arg(long)] program: Option<String>, #[arg(long)] sale: String, #[arg(long)] creator: String },
    /// [online] Withdraw collateral + remaining tokens (creator).
    Withdraw { #[arg(long)] program: Option<String>, #[arg(long)] sale: String, #[arg(long = "creator-collateral")] creator_collateral: String, #[arg(long = "creator-token")] creator_token: String, #[arg(long)] creator: String },
}

#[derive(Subcommand, Debug)]
enum LbpCmd {
    /// Token weight (and collateral weight) at a point in time.
    Weight { #[arg(long = "w-start")] w_start: String, #[arg(long = "w-end")] w_end: String, #[arg(long = "t-start")] t_start: u64, #[arg(long = "t-end")] t_end: u64, #[arg(long)] at: u64 },
    /// Quote a buy at a point in time (weight-shifting price).
    Quote { #[arg(long = "reserve-token")] reserve_token: u128, #[arg(long = "reserve-collateral")] reserve_collateral: u128, #[arg(long = "w-start")] w_start: String, #[arg(long = "w-end")] w_end: String, #[arg(long = "t-start")] t_start: u64, #[arg(long = "t-end")] t_end: u64, #[arg(long)] at: u64, #[arg(long = "in")] collateral_in: u128 },
    /// Derive the pool + vault PDA addresses for a sale.
    Ids { #[arg(long)] program: Option<String>, #[arg(long = "token-def")] token_def: String, #[arg(long = "collateral-def")] collateral_def: String, #[arg(long)] creator: String, #[arg(long, default_value_t = 0)] nonce: u64 },
    /// [online] Read a pool's on-chain state at time `--at` (ms).
    PoolInfo { #[arg(long)] pool: String, #[arg(long, default_value_t = 0)] at: u64 },
    /// [online] Buy from a pool (public path).
    Buy { #[arg(long)] program: Option<String>, #[arg(long)] pool: String, #[arg(long = "buyer-collateral")] buyer_collateral: Option<String>, #[arg(long = "buyer-token")] buyer_token: Option<String>, #[arg(long = "in")] collateral_in: u128, #[arg(long = "min-out")] min_out: Option<u128>, #[arg(long = "slippage-bps", default_value_t = 100)] slippage_bps: u128 },
    /// [online] Buy from an allowlist-gated pool. The leaf is derived from the buyer's collateral holding; `--proof` is a comma-separated list of 32-byte hex sibling hashes (empty for a single-member tree).
    BuyGated { #[arg(long)] program: Option<String>, #[arg(long)] pool: String, #[arg(long = "buyer-collateral")] buyer_collateral: Option<String>, #[arg(long = "buyer-token")] buyer_token: Option<String>, #[arg(long = "in")] collateral_in: u128, #[arg(long = "min-out")] min_out: Option<u128>, #[arg(long = "slippage-bps", default_value_t = 100)] slippage_bps: u128, #[arg(long = "proof", default_value = "")] proof: String },
    /// [online] Buy via Associated Token Accounts (RFP Func: ATAs). `--fund-from` seeds the collateral ATA first.
    BuyAta { #[arg(long)] program: Option<String>, #[arg(long = "ata-program")] ata_program: Option<String>, #[arg(long)] pool: String, #[arg(long)] owner: Option<String>, #[arg(long = "fund-from")] fund_from: Option<String>, #[arg(long = "in")] collateral_in: u128, #[arg(long = "min-out")] min_out: Option<u128>, #[arg(long = "slippage-bps", default_value_t = 100)] slippage_bps: u128 },
    /// [online] Private buy: deshield → buy → re-shield via a fresh ephemeral account A (public buy leg priced at the live clock).
    BuyPrivate { #[arg(long)] program: Option<String>, #[arg(long)] pool: String, #[arg(long = "user-collateral")] user_collateral: Option<String>, #[arg(long = "user-token")] user_token: Option<String>, #[arg(long = "in")] collateral_in: u128, #[arg(long = "min-out")] min_out: Option<u128>, #[arg(long = "slippage-bps", default_value_t = 100)] slippage_bps: u128 },
    /// [online] Create an LBP sale (deposits project tokens; weights in [0,1] as `0.99` or `99/100`).
    CreateSale {
        #[arg(long)] program: Option<String>,
        #[arg(long = "collateral-def")] collateral_def: String,
        #[arg(long)] treasury: String,
        #[arg(long = "creator-token-holding")] creator_token_holding: String,
        #[arg(long = "creator-collateral-holding")] creator_collateral_holding: String,
        #[arg(long)] creator: String,
        #[arg(long = "token-deposit")] token_deposit: u128,
        #[arg(long = "collateral-seed", default_value_t = 1)] collateral_seed: u128,
        #[arg(long = "w-start")] w_start: String,
        #[arg(long = "w-end")] w_end: String,
        #[arg(long = "t-start")] t_start: u64,
        #[arg(long = "t-end")] t_end: u64,
        #[arg(long = "fee-bps", default_value_t = 0)] fee_bps: u128,
        #[arg(long = "block-ceiling", default_value_t = 0)] block_ceiling: u128,
        #[arg(long = "allowlist-root", default_value = "")] allowlist_root: String,
        #[arg(long = "fixed-price", default_value_t = false)] fixed_price: bool,
        #[arg(long = "min-duration", default_value_t = 0)] min_duration: u64,
        #[arg(long, default_value_t = 0)] nonce: u64,
    },
    /// [online] Pause a pool (does not halt weight progression).
    Pause { #[arg(long)] program: Option<String>, #[arg(long)] pool: String, #[arg(long)] creator: String },
    /// [online] Resume a paused pool.
    Resume { #[arg(long)] program: Option<String>, #[arg(long)] pool: String, #[arg(long)] creator: String },
    /// [online] Refresh the stored weight for off-chain readers (pricing is lazy).
    Poke { #[arg(long)] program: Option<String>, #[arg(long)] pool: String },
    /// [online] Close a pool (creator).
    Close { #[arg(long)] program: Option<String>, #[arg(long)] pool: String, #[arg(long)] creator: String },
    /// [online] Withdraw raised collateral + unsold tokens minus the at-close fee (creator).
    Withdraw { #[arg(long)] program: Option<String>, #[arg(long)] pool: String, #[arg(long = "creator-collateral")] creator_collateral: String, #[arg(long = "creator-token")] creator_token: String, #[arg(long)] creator: String },
}

fn main() {
    let Cli { json, config, storage, cmd } = Cli::parse();
    let paths = || WalletPaths::resolve(config.clone(), storage.clone());
    let result = match cmd {
        Cmd::Status => paths().and_then(|p| online::status(&p, json)),
        Cmd::Balance { account } => {
            parse_account(&account).and_then(|a| paths().and_then(|p| online::balance(&p, a, json)))
        }
        Cmd::ProgramId { which } => online::program_id(&which, json),
        Cmd::MyBalance => paths().and_then(|p| online::my_balance(&p, json)),
        Cmd::MySales => paths().and_then(|p| online::my_sales(&p, json)),
        Cmd::MyPools => paths().and_then(|p| online::my_pools(&p, json)),
        Cmd::Wrap { amount, from } => paths().and_then(|p| online::wrap(&p, amount, from, json)),
        Cmd::Unwrap { amount } => paths().and_then(|p| online::unwrap(&p, amount, json)),
        Cmd::Shield { token_def, amount } => parse_account(&token_def).and_then(|d| paths().and_then(|p| online::shield(&p, d, amount, json))),
        Cmd::Deshield { token_def, amount } => parse_account(&token_def).and_then(|d| paths().and_then(|p| online::deshield(&p, d, amount, json))),
        Cmd::ShieldLez { amount, from } => paths().and_then(|p| online::shield_lez(&p, amount, from, json)),
        Cmd::DeshieldLez { amount } => paths().and_then(|p| online::deshield_lez(&p, amount, json)),
        Cmd::CreateAta { token_def, owner, ata_program } => {
            let ata = match ata_program { Some(s) => parse_program(&s), None => lpad_sdk::ata_program_id() };
            ata.and_then(|ata| parse_account(&token_def).and_then(|d| paths().and_then(|p| online::create_ata(&p, ata, owner, d, json))))
        }
        Cmd::Bc(c) => run_bc(c, json, &config, &storage),
        Cmd::Lbp(c) => run_lbp(c, json, &config, &storage),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn out(json: bool, value: serde_json::Value, human: impl FnOnce()) {
    if json {
        println!("{}", serde_json::to_string_pretty(&value).unwrap());
    } else {
        human();
    }
}

/// Reject an out-of-range `--fee-bps` for the offline calculators so they error
/// cleanly rather than panicking inside the pricing math (the on-chain cap is
/// `MAX_FEE_BPS`; `fee_bps >= 10000` would also divide-by-zero in the inverse).
fn check_fee_bps(fee_bps: u128) -> Result<(), String> {
    if fee_bps > bonding_curve_core::MAX_FEE_BPS {
        return Err(format!(
            "fee-bps must be ≤ {} (the on-chain maximum)",
            bonding_curve_core::MAX_FEE_BPS
        ));
    }
    Ok(())
}

/// Reject a virtual collateral reserve outside the Q64.64 domain so the offline
/// calculators error cleanly rather than panicking inside the pricing math
/// (`spot_price_q64`'s `<< 64` asserts `vc < 2^64`; mirrors `check_fee_bps`).
fn check_vc_domain(vc: u128) -> Result<(), String> {
    if vc >= (1u128 << 64) {
        return Err("vc must be < 2^64 (the Q64.64 reserve domain)".into());
    }
    Ok(())
}

fn run_bc(cmd: BcCmd, json: bool, config: &Option<String>, storage: &Option<String>) -> Result<(), String> {
    use bonding_curve_core as bc;
    let paths = || WalletPaths::resolve(config.clone(), storage.clone());
    // --program is optional: default to the bonding-curve guest's image id.
    let prog_id = |p: Option<String>| match p {
        Some(s) => parse_program(&s),
        None => lpad_sdk::bc_program_id(),
    };
    // --ata-program is optional: default to the ATA guest's image id.
    let ata_prog_id = |p: Option<String>| match p {
        Some(s) => parse_program(&s),
        None => lpad_sdk::ata_program_id(),
    };
    match cmd {
        BcCmd::Quote { vt, vc, fee_bps, collateral_in } => {
            check_fee_bps(fee_bps)?;
            check_vc_domain(vc)?;
            // `buy_fee` computes `collateral_in * fee_bps` internally; bound it first
            // so a large `--in` errors cleanly rather than panicking inside `buy_fee`.
            if collateral_in.checked_mul(fee_bps).is_none() {
                return Err("collateral_in * fee_bps overflows the fee math; use a smaller --in".into());
            }
            // `buy_tokens_out` and the after-price feed `vc + c_eff` / `vt * c_eff`
            // into the same pricing math `check_vc_domain` guards for the before-price;
            // bound the post-trade reserve and product so a large `--in` errors cleanly
            // rather than panicking inside `buy_tokens_out`/`spot_price_q64`.
            let c_eff = collateral_in - bc::buy_fee(collateral_in, fee_bps);
            if vc.checked_add(c_eff).is_none_or(|v| v >= (1u128 << 64)) {
                return Err("collateral_in pushes the post-buy reserve past the Q64.64 domain (vc + effective collateral >= 2^64); use a smaller amount".into());
            }
            if vt.checked_mul(c_eff).is_none() {
                return Err("vt * effective collateral overflows the pricing math; reserves/input far above any real sale".into());
            }
            let (tokens_out, fee, c_eff) = bc::buy_tokens_out(vt, vc, fee_bps, collateral_in);
            let before = q64_to_f64(bc::spot_price_q64(vc, vt));
            let after = q64_to_f64(bc::spot_price_q64(vc + c_eff, vt - tokens_out));
            let impact = if before > 0.0 { (after / before - 1.0) * 100.0 } else { 0.0 };
            out(
                json,
                serde_json::json!({ "tokens_out": tokens_out.to_string(), "fee": fee.to_string(), "effective_collateral": c_eff.to_string(), "spot_price_before": before, "spot_price_after": after, "price_impact_pct": impact }),
                || {
                    ui::header("bonding-curve quote");
                    ui::kv("tokens_out", tokens_out);
                    ui::kv("protocol fee", fee);
                    ui::kv("effective collateral", c_eff);
                    ui::kv("spot price (before)", format!("{before:.10}"));
                    ui::kv("spot price (after)", format!("{after:.10}"));
                    ui::kv("price impact", format!("{impact:.4}%"));
                },
            );
        }
        BcCmd::Cost { vt, vc, fee_bps, tokens } => {
            check_fee_bps(fee_bps)?;
            if tokens >= vt {
                return Err("tokens must be < virtual token reserve (vt)".into());
            }
            // `buy_cost_for_tokens` computes `Vc * q` internally; mirror the SellQuote
            // guard so a large `--vc`/`--tokens` errors cleanly instead of panicking
            // inside the pricing math (tokens < vt is already checked above).
            let vc_q = match vc.checked_mul(tokens) {
                Some(p) => p,
                None => return Err("vc * tokens overflows the pricing math; reserves/tokens are far above any real sale".into()),
            };
            // `buy_cost_for_tokens` then grosses the floored cost up for the fee:
            // `c_eff_min * FEE_BPS_DENOMINATOR`. When `tokens` is close to `vt`,
            // `c_eff_min = ceil(Vc*q/(Vt-q))` can blow up to ~`Vc*q` and overflow
            // that multiply (independent of `fee_bps`); bound it cleanly here so the
            // calculator errors instead of panicking inside the pricing math.
            let c_eff_min = bc::ceil_div(vc_q, vt - tokens);
            if c_eff_min.checked_mul(bonding_curve_core::FEE_BPS_DENOMINATOR).is_none() {
                return Err("vc/tokens push the gross-up cost past u128; reserves/tokens are far above any real sale".into());
            }
            let cost = bc::buy_cost_for_tokens(vt, vc, fee_bps, tokens);
            out(json, serde_json::json!({ "collateral_in": cost.to_string(), "tokens": tokens.to_string() }), || {
                ui::header("bonding-curve cost");
                ui::kv("collateral_in", cost);
                ui::kv("to buy tokens", tokens);
            });
        }
        BcCmd::SellQuote { vt, vc, fee_bps, tokens } => {
            check_fee_bps(fee_bps)?;
            check_vc_domain(vc)?;
            if vc.checked_mul(tokens).is_none() {
                return Err("vc * tokens overflows the pricing math; reserves/tokens are far above any real sale".into());
            }
            let (to_seller, fee, raw) = bc::sell_collateral_out(vt, vc, fee_bps, tokens);
            out(json, serde_json::json!({ "collateral_out": to_seller.to_string(), "fee": fee.to_string(), "gross_out": raw.to_string() }), || {
                ui::header("bonding-curve sell quote");
                ui::kv("collateral to seller", to_seller);
                ui::kv("protocol fee", fee);
                ui::kv("gross released", raw);
            });
        }
        BcCmd::Ids { program, token_def, collateral_def, creator, nonce } => {
            let prog = prog_id(program)?;
            let (td, cd, cr) = (parse_account(&token_def)?, parse_account(&collateral_def)?, parse_account(&creator)?);
            let sale = bc::compute_sale_pda(prog, td, cd, cr, nonce);
            print_ids(json, "sale", sale, bc::compute_token_vault_pda(prog, sale), bc::compute_collateral_vault_pda(prog, sale));
        }
        BcCmd::SaleInfo { sale } => return online::bc_sale_info(&paths()?, parse_account(&sale)?, json),
        BcCmd::CreateTokenSale { name, symbol, supply, sale_quantity, dex_seed, vt, vc, fee_bps, creator, private, nonce } => {
            return online::bc_create_token_sale(&paths()?, name, symbol, supply, sale_quantity, dex_seed, vt, vc, fee_bps, creator, private, nonce, json);
        }
        BcCmd::CreateSale { program, collateral_def, treasury, creator_token_holding, creator, sale_quantity, dex_seed, vt, vc, fee_bps, one_directional, end_ts, min_duration, nonce } => {
            let a = online::BcCreate {
                program: prog_id(program)?,
                collateral_def: parse_account(&collateral_def)?,
                treasury: parse_account(&treasury)?,
                creator_token_holding: parse_account(&creator_token_holding)?,
                creator: parse_account(&creator)?,
                sale_quantity, dex_seed, vt, vc, fee_bps, one_directional, end_ts, min_duration, nonce,
            };
            return online::bc_create_sale(&paths()?, a, json);
        }
        BcCmd::Buy { program, sale, buyer_collateral, buyer_token, collateral_in, min_out, slippage_bps } => {
            return online::bc_buy(&paths()?, prog_id(program)?, parse_account(&sale)?, buyer_collateral, buyer_token, collateral_in, min_out, slippage_bps, json);
        }
        BcCmd::BuyAta { program, ata_program, sale, owner, fund_from, collateral_in, min_out, slippage_bps } => {
            return online::bc_buy_ata(&paths()?, prog_id(program)?, ata_prog_id(ata_program)?, parse_account(&sale)?, owner, fund_from, collateral_in, min_out, slippage_bps, json);
        }
        BcCmd::SellAta { program, ata_program, sale, owner, fund_from, tokens, min_out, slippage_bps } => {
            return online::bc_sell_ata(&paths()?, prog_id(program)?, ata_prog_id(ata_program)?, parse_account(&sale)?, owner, fund_from, tokens, min_out, slippage_bps, json);
        }
        BcCmd::BuyPrivate { program, sale, user_collateral, user_token, collateral_in, min_out, slippage_bps } => {
            return online::bc_buy_private(&paths()?, prog_id(program)?, parse_account(&sale)?, user_collateral, user_token, collateral_in, min_out, slippage_bps, json);
        }
        BcCmd::SellPrivate { program, sale, user_token, user_collateral, tokens, min_out, slippage_bps } => {
            return online::bc_sell_private(&paths()?, prog_id(program)?, parse_account(&sale)?, user_token, user_collateral, tokens, min_out, slippage_bps, json);
        }
        BcCmd::Sell { program, sale, seller_token, seller_collateral, tokens, min_out, slippage_bps } => {
            return online::bc_sell(&paths()?, prog_id(program)?, parse_account(&sale)?, parse_account(&seller_token)?, parse_account(&seller_collateral)?, tokens, min_out, slippage_bps, json);
        }
        BcCmd::Close { program, sale, creator } => {
            return online::bc_close(&paths()?, prog_id(program)?, parse_account(&sale)?, parse_account(&creator)?, json);
        }
        BcCmd::Withdraw { program, sale, creator_collateral, creator_token, creator } => {
            return online::bc_withdraw(&paths()?, prog_id(program)?, parse_account(&sale)?, parse_account(&creator_collateral)?, parse_account(&creator_token)?, parse_account(&creator)?, json);
        }
    }
    Ok(())
}

fn run_lbp(cmd: LbpCmd, json: bool, config: &Option<String>, storage: &Option<String>) -> Result<(), String> {
    use lbp_core as lbp;
    let paths = || WalletPaths::resolve(config.clone(), storage.clone());
    // --program is optional: default to the LBP guest's image id.
    let prog_id = |p: Option<String>| match p {
        Some(s) => parse_program(&s),
        None => lpad_sdk::lbp_program_id(),
    };
    let ata_prog_id = |p: Option<String>| match p {
        Some(s) => parse_program(&s),
        None => lpad_sdk::ata_program_id(),
    };
    match cmd {
        LbpCmd::Weight { w_start, w_end, t_start, t_end, at } => {
            let (ws, we) = (parse_weight_q64(&w_start)?, parse_weight_q64(&w_end)?);
            let wt = lbp::weight_token_q64(ws, we, t_start, t_end, at);
            let wc = lbp::fixed::ONE - wt;
            out(json, serde_json::json!({ "w_token": q64_to_f64(wt), "w_collateral": q64_to_f64(wc) }), || {
                ui::header("LBP weight");
                ui::kv("token weight", format!("{:.6}", q64_to_f64(wt)));
                ui::kv("collateral weight", format!("{:.6}", q64_to_f64(wc)));
            });
        }
        LbpCmd::Quote { reserve_token, reserve_collateral, w_start, w_end, t_start, t_end, at, collateral_in } => {
            let (ws, we) = (parse_weight_q64(&w_start)?, parse_weight_q64(&w_end)?);
            let wt = lbp::weight_token_q64(ws, we, t_start, t_end, at);
            // The pricing math requires a token weight strictly inside (0,1); the
            // resolved weight hits the 0/1 endpoints for ordinary inputs (e.g.
            // `--w-start 1`, or quoting at/after `--t-end`). Error cleanly rather
            // than panicking inside `spot_price_q64`/`buy_tokens_out`.
            if wt == 0 || wt >= lbp::fixed::ONE {
                return Err("token weight resolved to 0 or 1 at this time; price is undefined - pick weights/at strictly inside (0,1)".into());
            }
            // The price math left-shifts the reserves by 64 (`div_to_q64` asserts
            // numerator < 2^64) and `buy_tokens_out` asserts
            // `reserve_collateral + collateral_in < MAX_RESERVE`. Reject out-of-domain
            // reserves/input cleanly rather than panicking inside the pricing math.
            if reserve_token >= (1u128 << 64) || reserve_collateral >= (1u128 << 64) {
                return Err("reserves must be < 2^64 (the Q64.64 domain)".into());
            }
            if reserve_collateral.checked_add(collateral_in).is_none_or(|t| t >= lbp::MAX_RESERVE) {
                return Err("reserve_collateral + collateral_in must stay < 2^64 (the Q64.64 domain)".into());
            }
            let price = q64_to_f64(lbp::spot_price_q64(reserve_token, reserve_collateral, wt));
            let tokens_out = lbp::buy_tokens_out(reserve_token, reserve_collateral, wt, collateral_in);
            out(json, serde_json::json!({ "w_token": q64_to_f64(wt), "spot_price": price, "tokens_out": tokens_out.to_string() }), || {
                ui::header("LBP quote");
                ui::kv("token weight @t", format!("{:.6}", q64_to_f64(wt)));
                ui::kv("spot price", format!("{price:.10}"));
                ui::kv("tokens_out", tokens_out);
            });
        }
        LbpCmd::Ids { program, token_def, collateral_def, creator, nonce } => {
            let prog = prog_id(program)?;
            let (td, cd, cr) = (parse_account(&token_def)?, parse_account(&collateral_def)?, parse_account(&creator)?);
            let pool = lbp::compute_pool_pda(prog, td, cd, cr, nonce);
            print_ids(json, "pool", pool, lbp::compute_token_vault_pda(prog, pool), lbp::compute_collateral_vault_pda(prog, pool));
        }
        LbpCmd::PoolInfo { pool, at } => return online::lbp_pool_info(&paths()?, parse_account(&pool)?, at, json),
        LbpCmd::Buy { program, pool, buyer_collateral, buyer_token, collateral_in, min_out, slippage_bps } => {
            return online::lbp_buy(&paths()?, prog_id(program)?, parse_account(&pool)?, buyer_collateral, buyer_token, collateral_in, min_out, slippage_bps, json);
        }
        LbpCmd::BuyGated { program, pool, buyer_collateral, buyer_token, collateral_in, min_out, slippage_bps, proof } => {
            return online::lbp_buy_gated(&paths()?, prog_id(program)?, parse_account(&pool)?, buyer_collateral, buyer_token, collateral_in, min_out, slippage_bps, parse_proof(&proof)?, json);
        }
        LbpCmd::BuyAta { program, ata_program, pool, owner, fund_from, collateral_in, min_out, slippage_bps } => {
            return online::lbp_buy_ata(&paths()?, prog_id(program)?, ata_prog_id(ata_program)?, parse_account(&pool)?, owner, fund_from, collateral_in, min_out, slippage_bps, json);
        }
        LbpCmd::BuyPrivate { program, pool, user_collateral, user_token, collateral_in, min_out, slippage_bps } => {
            return online::lbp_buy_private(&paths()?, prog_id(program)?, parse_account(&pool)?, user_collateral, user_token, collateral_in, min_out, slippage_bps, json);
        }
        LbpCmd::CreateSale { program, collateral_def, treasury, creator_token_holding, creator_collateral_holding, creator, token_deposit, collateral_seed, w_start, w_end, t_start, t_end, fee_bps, block_ceiling, allowlist_root, fixed_price, min_duration, nonce } => {
            let a = online::LbpCreate {
                program: prog_id(program)?,
                collateral_def: parse_account(&collateral_def)?,
                treasury: parse_account(&treasury)?,
                creator_token_holding: parse_account(&creator_token_holding)?,
                creator_collateral_holding: parse_account(&creator_collateral_holding)?,
                creator: parse_account(&creator)?,
                token_deposit,
                collateral_seed,
                w_start_q64: parse_weight_q64(&w_start)?,
                w_end_q64: parse_weight_q64(&w_end)?,
                t_start_ms: t_start,
                t_end_ms: t_end,
                fee_bps,
                block_token_ceiling: block_ceiling,
                allowlist_root: parse_root(&allowlist_root)?,
                fixed_price,
                min_duration_ms: min_duration,
                nonce,
            };
            return online::lbp_create_sale(&paths()?, a, json);
        }
        LbpCmd::Pause { program, pool, creator } => return online::lbp_pause(&paths()?, prog_id(program)?, parse_account(&pool)?, parse_account(&creator)?, json),
        LbpCmd::Resume { program, pool, creator } => return online::lbp_resume(&paths()?, prog_id(program)?, parse_account(&pool)?, parse_account(&creator)?, json),
        LbpCmd::Poke { program, pool } => return online::lbp_poke(&paths()?, prog_id(program)?, parse_account(&pool)?, json),
        LbpCmd::Close { program, pool, creator } => return online::lbp_close(&paths()?, prog_id(program)?, parse_account(&pool)?, parse_account(&creator)?, json),
        LbpCmd::Withdraw { program, pool, creator_collateral, creator_token, creator } => {
            return online::lbp_withdraw(&paths()?, prog_id(program)?, parse_account(&pool)?, parse_account(&creator_collateral)?, parse_account(&creator_token)?, parse_account(&creator)?, json);
        }
    }
    Ok(())
}

fn print_ids(json: bool, label: &str, primary: AccountId, token_vault: AccountId, coll_vault: AccountId) {
    out(
        json,
        serde_json::json!({ label: hex_account(primary), "token_vault": hex_account(token_vault), "collateral_vault": hex_account(coll_vault) }),
        || {
            ui::header(&format!("{label} + vault addresses"));
            ui::kv(label, hex_account(primary));
            ui::kv("token_vault", hex_account(token_vault));
            ui::kv("collateral_vault", hex_account(coll_vault));
        },
    );
}

#[cfg(test)]
mod cli_tests {
    use super::Cli;
    use clap::CommandFactory;

    /// Catches clap mis-configuration (duplicate args, bad value parsers, etc.).
    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
