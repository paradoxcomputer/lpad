//! Online (wallet-backed) CLI commands - a presentation layer over `lpad_sdk`.
//! All chain logic lives in the SDK; here we resolve wallet paths, drive a live
//! spinner from the SDK's progress hook, and render results (human or `--json`).

use std::path::PathBuf;

use lpad_sdk::{BcCreateArgs, LaunchpadClient, LbpCreateArgs, Network};
use lee_core::{account::AccountId, program::ProgramId};

use crate::ui;

/// Resolved wallet config + storage + statistics paths, and which sequencer to use.
pub struct WalletPaths {
    pub config: PathBuf,
    pub storage: PathBuf,
    /// Per-sequencer latency samples (v0.2.1+). See
    /// [`LaunchpadClient::record_sequencer_statistics`].
    pub statistics: PathBuf,
    /// `None` means "whatever the wallet config already points at". A `Some`
    /// overrides it for this invocation only, without rewriting the file.
    pub network: Option<Network>,
}

impl WalletPaths {
    /// Resolve wallet paths with zero required config. Precedence:
    /// explicit `--config`/`--storage` > `$LEE_WALLET_HOME_DIR` > the default
    /// home `~/.lpad`. Also auto-detects the proof mode: if `RISC0_DEV_MODE`
    /// isn't already set, it honors a `proof_mode` marker ("dev"/"real") written
    /// next to the wallet by the bootstrap - so the user never has to export
    /// `RISC0_DEV_MODE` or `LEE_WALLET_HOME_DIR` for a bootstrapped chain.
    ///
    /// `network` accepts `testnet`, `paradox`, or a sequencer URL. lpad has no
    /// bundled local sequencer; see [`Network`].
    pub fn resolve(
        config: Option<String>,
        storage: Option<String>,
        network: Option<String>,
    ) -> Result<Self, String> {
        let network = network.as_deref().map(Network::parse).transpose()?;
        // LEZ renamed the home-dir variable after rc4; keep accepting the old name so
        // existing shells and scripts keep working.
        let home = std::env::var("LEE_WALLET_HOME_DIR")
            .or_else(|_| std::env::var("NSSA_WALLET_HOME_DIR"))
            .ok()
            .unwrap_or_else(default_home);
        let config = PathBuf::from(config.unwrap_or_else(|| format!("{home}/wallet_config.json")));
        let storage = PathBuf::from(storage.unwrap_or_else(|| format!("{home}/storage.json")));
        let statistics = config
            .parent()
            .map_or_else(|| PathBuf::from("statistics.json"), |d| d.join("statistics.json"));
        if !config.exists() {
            return Err(format!(
                "no wallet config at {} - run `bash scripts/bootstrap.sh` to create one, \
                 or pass --config/--storage (or set $LEE_WALLET_HOME_DIR)",
                config.display()
            ));
        }
        reject_pre_v020_wallet(&config, &storage)?;
        if let Some(dir) = config.parent() {
            detect_proof_mode(dir);
        }
        Ok(Self { config, storage, statistics, network })
    }
}

/// Fail with an actionable message on a wallet directory created by LEZ
/// v0.2.0-rc4 or earlier.
///
/// Such a directory is not upgradable: `storage.json` moved its accounts under a
/// new `key_chain` object, private account ids are now derived with the viewing
/// key mixed in, and the note-encryption KDF changed - so previously-decoded
/// notes cannot be read back either. Without this check the user would see a raw
/// serde error from deep inside the wallet.
///
/// Only signals that actually changed are used. In particular `sequencer_addr` is
/// NOT one: v0.2.0 still has it (the `sequencers` array only arrives in v0.2.1),
/// so keying off that would reject every valid config.
fn reject_pre_v020_wallet(config: &std::path::Path, storage: &std::path::Path) -> Result<(), String> {
    let stale = |what: &str| {
        Err(format!(
            "{} was created by LEZ v0.2.0-rc4 or earlier and cannot be upgraded \
             (storage schema, private-account derivation and note encryption all changed). \
             Move {} aside and re-run `bash scripts/bootstrap.sh`.",
            what,
            config.parent().unwrap_or(config).display(),
        ))
    };
    // rc4 carried the genesis keys in the wallet config; v0.2.0 dropped the field.
    if let Ok(text) = std::fs::read_to_string(config)
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
        && v.get("initial_accounts").is_some()
    {
        return stale("this wallet config");
    }
    // rc4 stored accounts at the top level; v0.2.0 nests them under `key_chain`.
    if let Ok(text) = std::fs::read_to_string(storage)
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
        && v.get("key_chain").is_none()
        && v.get("accounts").is_some()
    {
        return stale("this wallet storage");
    }
    Ok(())
}

/// Default wallet home when nothing is specified: `~/.lpad`.
fn default_home() -> String {
    std::env::var("HOME")
        .map(|h| format!("{h}/.lpad"))
        .unwrap_or_else(|_| ".lpad".to_owned())
}

/// Honor a `proof_mode` marker ("dev"/"real") next to the wallet so private buys
/// match the sequencer without the user setting `RISC0_DEV_MODE`. An explicit env
/// value always wins.
fn detect_proof_mode(dir: &std::path::Path) {
    if std::env::var_os("RISC0_DEV_MODE").is_some() {
        return;
    }
    if let Ok(mode) = std::fs::read_to_string(dir.join("proof_mode"))
        && mode.trim() == "dev"
    {
        // Safe: set during single-threaded CLI startup, before the prover runs.
        unsafe { std::env::set_var("RISC0_DEV_MODE", "1") };
    }
}

/// Parsed `bc create-sale` arguments, mapped to [`BcCreateArgs`].
pub struct BcCreate {
    pub program: ProgramId,
    pub collateral_def: AccountId,
    pub treasury: AccountId,
    pub creator_token_holding: AccountId,
    pub creator: AccountId,
    pub sale_quantity: u128,
    pub dex_seed: u128,
    pub vt: u128,
    pub vc: u128,
    pub fee_bps: u128,
    pub one_directional: bool,
    pub end_ts: u64,
    pub min_duration: u64,
    pub nonce: u64,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn q64(x: u128) -> f64 {
    x as f64 / (1u128 << 64) as f64
}

/// Open the wallet and run `op` with a live spinner wired to the SDK's progress
/// hook. The spinner shows real phase labels (proving / submitting / inclusion)
/// and is hidden under `--json` or when stderr isn't a TTY.
fn run<T>(
    paths: &WalletPaths,
    json: bool,
    start: &str,
    op: impl FnOnce(&mut LaunchpadClient) -> Result<T, String>,
) -> Result<T, String> {
    let sp = ui::Spinner::new(start, json);
    let result = (|| {
        let mut c = LaunchpadClient::open(
            paths.config.clone(),
            paths.storage.clone(),
            paths.statistics.clone(),
            paths.network.as_ref(),
        )?;
        let h = sp.handle();
        c.set_progress(move |m| {
            h.set_message(m.to_owned());
        });
        let out = op(&mut c);
        // Persist the latency samples this process gathered, so the next command
        // does not re-calibrate every sequencer. Best-effort: a failure here must
        // not mask the command's own result.
        if out.is_ok() {
            let _ = c.record_sequencer_statistics();
        }
        out
    })();
    sp.clear();
    result
}

/// Render a submitted write: `{ok,op,tx}` JSON, or a green ✓ line.
fn submitted(json: bool, op: &str, tx: [u8; 32]) {
    if json {
        println!("{}", serde_json::json!({ "ok": true, "op": op, "tx": hex(&tx) }));
    } else {
        ui::ok(&format!("{op}  -  tx 0x{}", hex(&tx)));
    }
}

// ---------------------------------------------------------------------------
// Read-only
// ---------------------------------------------------------------------------

pub fn status(paths: &WalletPaths, json: bool) -> Result<(), String> {
    let (height, synced) = run(paths, json, "querying sequencer", |c| Ok((c.block_height()?, c.last_synced())))?;
    if json {
        println!("{}", serde_json::json!({ "block_height": height, "last_synced": synced }));
    } else {
        ui::header("status");
        ui::kv("sequencer block", height);
        ui::kv("wallet synced", synced);
    }
    Ok(())
}

pub fn balance(paths: &WalletPaths, account: AccountId, json: bool) -> Result<(), String> {
    let (def, bal) = run(paths, json, "reading balance", |c| c.balance(account))?;
    if json {
        println!("{}", serde_json::json!({ "balance": bal.to_string(), "definition": hex(def.to_bytes().as_ref()) }));
    } else {
        ui::header("balance");
        ui::kv("balance", bal);
        ui::kv("definition", hex(def.to_bytes().as_ref()));
    }
    Ok(())
}

pub fn program_id(which: &str, json: bool) -> Result<(), String> {
    let id = match which {
        "bc" | "bonding_curve" | "bonding-curve" => lpad_sdk::bc_program_id()?,
        "lbp" => lpad_sdk::lbp_program_id()?,
        "ata" => lpad_sdk::ata_program_id()?,
        "wlez" => lpad_sdk::wlez_program_id()?,
        _ => return Err("program must be 'bc', 'lbp', 'ata', or 'wlez'".into()),
    };
    let s = crate::fmt::hex_program(id);
    // wlez additionally reports the two PDAs it owns. They are pure derivations
    // from the program id (no wallet, no chain), and they are the only handle a
    // deployment check has on wlez: unlike a sale or a pool, nothing records a
    // wlez account id during bootstrap.
    let wlez_pdas = (which == "wlez").then(|| {
        (
            crate::fmt::hex_account(lpad_sdk::wlez_definition_id(id)),
            crate::fmt::hex_account(lpad_sdk::wlez_vault_id(id)),
        )
    });
    if json {
        let mut o = serde_json::json!({ "program": which, "program_id": s });
        if let Some((def, vault)) = &wlez_pdas {
            o["definition"] = serde_json::json!(def);
            o["vault"] = serde_json::json!(vault);
        }
        println!("{o}");
    } else {
        ui::header(&format!("{which} program id"));
        ui::kv("program id", s);
        if let Some((def, vault)) = &wlez_pdas {
            ui::kv("definition", def.clone());
            ui::kv("vault", vault.clone());
        }
    }
    Ok(())
}

pub fn bc_sale_info(paths: &WalletPaths, sale: AccountId, json: bool) -> Result<(), String> {
    let s = run(paths, json, "reading sale", |c| c.bc_sale(sale))?;
    let spot = q64(s.spot_price_q64());
    if json {
        println!(
            "{}",
            serde_json::json!({
                "status": format!("{:?}", s.status), "sale_reserve": s.sale_reserve.to_string(),
                "sale_reserve_initial": s.sale_reserve_initial.to_string(), "sold_bps": s.sold_bps().to_string(),
                "real_collateral": s.real_collateral.to_string(), "spot_price": spot,
                "fee_bps": s.fee_bps.to_string(), "cum_collateral_in": s.cum_collateral_in.to_string(),
                "cum_fees": s.cum_fees.to_string(), "buy_count": s.buy_count, "sell_count": s.sell_count,
                "token_name": s.token_name, "token_symbol": s.token_symbol,
            })
        );
    } else {
        ui::header("bonding-curve sale");
        if !s.token_name.is_empty() {
            ui::kv("token", format!("{} ({})", s.token_name, s.token_symbol));
        }
        ui::kv("status", format!("{:?}", s.status));
        ui::kv("sale reserve", format!("{} / {}  ({} bps sold)", s.sale_reserve, s.sale_reserve_initial, s.sold_bps()));
        ui::kv("collateral raised", s.real_collateral);
        ui::kv("spot price", format!("{spot:.10}"));
        ui::kv("buys / sells", format!("{} / {}", s.buy_count, s.sell_count));
        ui::kv("cum collat / fees", format!("{} / {}", s.cum_collateral_in, s.cum_fees));
    }
    Ok(())
}

pub fn lbp_pool_info(paths: &WalletPaths, pool: AccountId, at_ms: u64, json: bool) -> Result<(), String> {
    let p = run(paths, json, "reading pool", |c| c.lbp_pool(pool))?;
    let wt = q64(p.weight_token_q64(at_ms));
    let spot = q64(p.spot_price_q64(at_ms));
    if json {
        println!(
            "{}",
            serde_json::json!({
                "status": format!("{:?}", p.status), "paused": p.paused,
                "reserve_token": p.reserve_token.to_string(), "reserve_collateral": p.reserve_collateral.to_string(),
                "w_token_at": wt, "spot_price_at": spot, "at_ms": at_ms,
                "cum_collateral_in": p.cum_collateral_in.to_string(), "buy_count": p.buy_count,
                "token_name": p.token_name, "token_symbol": p.token_symbol,
            })
        );
    } else {
        ui::header("LBP pool");
        if !p.token_name.is_empty() {
            ui::kv("token", format!("{} ({})", p.token_name, p.token_symbol));
        }
        ui::kv("status", format!("{:?}{}", p.status, if p.paused { " (paused)" } else { "" }));
        ui::kv("reserves tok/col", format!("{} / {}", p.reserve_token, p.reserve_collateral));
        ui::kv("token weight @t", format!("{wt:.6}"));
        ui::kv("spot price @t", format!("{spot:.10}"));
        ui::kv("collateral raised", p.cum_collateral_in);
        ui::kv("buys", p.buy_count);
    }
    Ok(())
}

/// Resolve a holding argument: an explicit id/label if given, else the wallet's
/// largest holding of `definition` on the given side (public/shielded). Lets the
/// buy/sell/withdraw commands omit `--*-collateral`/`--*-token`.
fn holding(c: &LaunchpadClient, given: Option<String>, definition: AccountId, private: bool, exclude: &[AccountId], role: &str) -> Result<AccountId, String> {
    match given {
        Some(s) => crate::fmt::parse_account(&s),
        None => c.find_holding(definition, private, exclude).ok_or_else(|| {
            format!(
                "no {} {role} holding in this wallet - run `lpad my-balance` to see holdings, or pass it explicitly",
                if private { "shielded" } else { "public" }
            )
        }),
    }
}

/// Resolve a RECEIVING public token holding: explicit id, else the wallet's
/// existing holding, else initialise a fresh one. (A buyer needs somewhere to
/// receive the bought tokens even on their first purchase.)
fn recv_holding(c: &mut LaunchpadClient, given: Option<String>, definition: AccountId) -> Result<AccountId, String> {
    match given {
        Some(s) => crate::fmt::parse_account(&s),
        None => match c.find_holding(definition, false, &[]) {
            Some(h) => Ok(h),
            None => c.init_token_holding(definition),
        },
    }
}

/// The wallet's largest native-LEZ account (default payer / creator / unwrap dest).
fn largest_native(c: &LaunchpadClient) -> Result<AccountId, String> {
    c.my_holdings()
        .ok()
        .and_then(|hs| hs.into_iter().filter(|h| h.native).max_by_key(|h| h.balance).map(|h| h.account))
        .ok_or_else(|| "no native-LEZ account in this wallet - fund one or pass --from/--creator".to_owned())
}

// ---------------------------------------------------------------------------
// wlez: native-LEZ <-> WLEZ
// ---------------------------------------------------------------------------

pub fn wrap(paths: &WalletPaths, amount: u128, from: Option<String>, json: bool) -> Result<(), String> {
    let tx = run(paths, json, "wrapping LEZ → WLEZ", |c| {
        let wlez_program = lpad_sdk::wlez_program_id()?;
        let user_native = match from { Some(s) => crate::fmt::parse_account(&s)?, None => largest_native(c)? };
        // Initialise wlez only if its definition doesn't exist yet (needs any
        // wallet-held token as the init reference).
        if !c.wlez_initialized(wlez_program) {
            let ref_def = c.my_holdings()?.into_iter().find(|h| !h.native).map(|h| h.definition)
                .ok_or("wlez not initialised and no token holding in this wallet to use as the init reference")?;
            c.initialize_wlez(wlez_program, ref_def, user_native)?;
        }
        let wlez_def = c.wlez_definition(wlez_program);
        let user_holding = match c.find_holding(wlez_def, false, &[]) {
            Some(h) => h,
            None => c.init_token_holding(wlez_def)?,
        };
        c.wrap(wlez_program, amount, user_native, user_holding)
    })?;
    submitted(json, "wrap", tx);
    Ok(())
}

pub fn unwrap(paths: &WalletPaths, amount: u128, json: bool) -> Result<(), String> {
    let tx = run(paths, json, "unwrapping WLEZ → LEZ", |c| {
        let wlez_program = lpad_sdk::wlez_program_id()?;
        let wlez_def = c.wlez_definition(wlez_program);
        let user_holding = c.find_holding(wlez_def, false, &[]).ok_or("no WLEZ holding to unwrap - run `lpad wrap` first")?;
        let user_native = largest_native(c)?;
        c.unwrap(wlez_program, amount, user_holding, user_native)
    })?;
    submitted(json, "unwrap", tx);
    Ok(())
}

pub fn shield(paths: &WalletPaths, token_def: AccountId, amount: u128, json: bool) -> Result<(), String> {
    let tx = run(paths, json, "shielding", |c| {
        let public_holding = c.find_holding(token_def, false, &[]).ok_or("no public holding of that token to shield - run `lpad my-balance`")?;
        let private_holding = match c.find_holding(token_def, true, &[]) {
            Some(h) => h,
            None => c.new_private_account()?,
        };
        c.shield(public_holding, private_holding, amount)
    })?;
    submitted(json, "shield", tx);
    Ok(())
}

pub fn deshield(paths: &WalletPaths, token_def: AccountId, amount: u128, json: bool) -> Result<(), String> {
    let tx = run(paths, json, "deshielding", |c| {
        let private_holding = c.find_holding(token_def, true, &[]).ok_or("no shielded holding of that token to deshield")?;
        let public_holding = match c.find_holding(token_def, false, &[]) {
            Some(h) => h,
            None => c.init_token_holding(token_def)?,
        };
        c.deshield(private_holding, public_holding, amount)
    })?;
    submitted(json, "deshield", tx);
    Ok(())
}

/// One-shot: native LEZ -> WLEZ (wrap) -> shielded WLEZ (shield).
pub fn shield_lez(paths: &WalletPaths, amount: u128, from: Option<String>, json: bool) -> Result<(), String> {
    let tx = run(paths, json, "shielding LEZ (wrap + shield)", |c| {
        let wlez_program = lpad_sdk::wlez_program_id()?;
        let user_native = match from { Some(s) => crate::fmt::parse_account(&s)?, None => largest_native(c)? };
        if !c.wlez_initialized(wlez_program) {
            let ref_def = c.my_holdings()?.into_iter().find(|h| !h.native).map(|h| h.definition)
                .ok_or("wlez not initialised and no token holding to use as the init reference")?;
            c.initialize_wlez(wlez_program, ref_def, user_native)?;
        }
        let wlez_def = c.wlez_definition(wlez_program);
        let wlez_holding = match c.find_holding(wlez_def, false, &[]) { Some(h) => h, None => c.init_token_holding(wlez_def)? };
        c.wrap(wlez_program, amount, user_native, wlez_holding)?;
        let private = match c.find_holding(wlez_def, true, &[]) { Some(h) => h, None => c.new_private_account()? };
        c.shield(wlez_holding, private, amount)
    })?;
    submitted(json, "shield-lez (wrap + shield)", tx);
    Ok(())
}

/// One-shot: shielded WLEZ (deshield) -> WLEZ -> native LEZ (unwrap).
pub fn deshield_lez(paths: &WalletPaths, amount: u128, json: bool) -> Result<(), String> {
    let tx = run(paths, json, "deshielding WLEZ → LEZ", |c| {
        let wlez_program = lpad_sdk::wlez_program_id()?;
        let wlez_def = c.wlez_definition(wlez_program);
        let private = c.find_holding(wlez_def, true, &[]).ok_or("no shielded WLEZ to deshield")?;
        let public_wlez = match c.find_holding(wlez_def, false, &[]) { Some(h) => h, None => c.init_token_holding(wlez_def)? };
        c.deshield(private, public_wlez, amount)?;
        let user_native = largest_native(c)?;
        c.unwrap(wlez_program, amount, public_wlez, user_native)
    })?;
    submitted(json, "deshield-lez (deshield + unwrap)", tx);
    Ok(())
}

// ---------------------------------------------------------------------------
// Discovery (no ids to paste)
// ---------------------------------------------------------------------------

fn acct(id: AccountId, private: bool) -> String {
    format!("{}/{}", if private { "Private" } else { "Public" }, id)
}

pub fn my_balance(paths: &WalletPaths, json: bool) -> Result<(), String> {
    let hs = run(paths, json, "scanning wallet holdings", |c| c.my_holdings())?;
    // Hide empty holdings (the bootstrap/tests leave many drained/dust accounts).
    let hs: Vec<_> = hs.into_iter().filter(|h| h.balance > 0).collect();
    if json {
        let arr: Vec<_> = hs
            .iter()
            .map(|h| {
                serde_json::json!({
                    "account": acct(h.account, h.private), "private": h.private, "label": h.label,
                    "token": h.token_name, "definition": hex(h.definition.to_bytes().as_ref()),
                    "balance": h.balance.to_string(),
                })
            })
            .collect();
        println!("{}", serde_json::json!({ "holdings": arr }));
    } else {
        ui::header("my balances");
        if hs.is_empty() {
            ui::kv("(none)", "no fungible holdings in this wallet");
        }
        for h in &hs {
            let tok = h.token_name.clone().unwrap_or_else(|| format!("def {}…", &hex(h.definition.to_bytes().as_ref())[..8]));
            let side = if h.private { "shielded" } else { "public" };
            ui::kv(&format!("{tok} ({side})"), format!("{:>14}   {}", h.balance, acct(h.account, h.private)));
        }
    }
    Ok(())
}

pub fn my_sales(paths: &WalletPaths, json: bool) -> Result<(), String> {
    let sales = run(paths, json, "discovering your sales", |c| c.my_sales())?;
    if json {
        let arr: Vec<_> = sales
            .iter()
            .map(|(id, s)| {
                serde_json::json!({
                    "sale": hex(id.to_bytes().as_ref()), "status": format!("{:?}", s.status),
                    "sale_reserve": s.sale_reserve.to_string(), "sale_reserve_initial": s.sale_reserve_initial.to_string(),
                    "real_collateral": s.real_collateral.to_string(), "nonce": s.nonce,
                    "token_name": s.token_name, "token_symbol": s.token_symbol,
                })
            })
            .collect();
        println!("{}", serde_json::json!({ "sales": arr }));
    } else {
        ui::header("my bonding-curve sales");
        if sales.is_empty() {
            ui::kv("(none found)", "create one with `bc create-sale`, or derive with `bc ids`");
        }
        for (id, s) in &sales {
            let label = if s.token_name.is_empty() { String::new() } else { format!("{} ({})  ", s.token_name, s.token_symbol) };
            ui::kv(&hex(id.to_bytes().as_ref()), format!("{label}{:?}  reserve {}/{}  raised {}", s.status, s.sale_reserve, s.sale_reserve_initial, s.real_collateral));
        }
    }
    Ok(())
}

pub fn my_pools(paths: &WalletPaths, json: bool) -> Result<(), String> {
    let pools = run(paths, json, "discovering your pools", |c| c.my_pools())?;
    if json {
        let arr: Vec<_> = pools
            .iter()
            .map(|(id, p)| {
                serde_json::json!({
                    "pool": hex(id.to_bytes().as_ref()), "status": format!("{:?}", p.status), "paused": p.paused,
                    "reserve_token": p.reserve_token.to_string(), "reserve_collateral": p.reserve_collateral.to_string(),
                    "nonce": p.nonce,
                    "token_name": p.token_name, "token_symbol": p.token_symbol,
                })
            })
            .collect();
        println!("{}", serde_json::json!({ "pools": arr }));
    } else {
        ui::header("my LBP pools");
        if pools.is_empty() {
            ui::kv("(none found)", "create one with `lbp create-sale`, or derive with `lbp ids`");
        }
        for (id, p) in &pools {
            let label = if p.token_name.is_empty() { String::new() } else { format!("{} ({})  ", p.token_name, p.token_symbol) };
            ui::kv(&hex(id.to_bytes().as_ref()), format!("{label}{:?}{}  reserves {}/{}", p.status, if p.paused { " (paused)" } else { "" }, p.reserve_token, p.reserve_collateral));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Bonding curve - write
// ---------------------------------------------------------------------------

pub fn bc_create_sale(paths: &WalletPaths, a: BcCreate, json: bool) -> Result<(), String> {
    let (sale, tx) = run(paths, json, "creating sale", |c| {
        c.bc_create_sale(BcCreateArgs {
            program: a.program,
            collateral_def: a.collateral_def,
            treasury: a.treasury,
            creator_token_holding: a.creator_token_holding,
            creator: a.creator,
            sale_quantity: a.sale_quantity,
            dex_seed: a.dex_seed,
            virt_token: a.vt,
            virt_collateral: a.vc,
            fee_bps: a.fee_bps,
            one_directional: a.one_directional,
            end_timestamp_ms: a.end_ts,
            min_duration_ms: a.min_duration,
            nonce: a.nonce,
        })
    })?;
    submitted(json, &format!("bc create-sale (sale 0x{})", hex(sale.to_bytes().as_ref())), tx);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn bc_create_token_sale(paths: &WalletPaths, name: String, symbol: String, supply: u128, sale_quantity: u128, dex_seed: u128, vt: u128, vc: u128, fee_bps: u128, creator: Option<String>, private: bool, nonce: u64, json: bool) -> Result<(), String> {
    let start = if private { "launching token privately" } else { "launching token (mint + sale)" };
    let (sale, token_def, tx) = run(paths, json, start, |c| {
        let bc_program = lpad_sdk::bc_program_id()?;
        let wlez_program = lpad_sdk::wlez_program_id()?;
        let creator = match creator { Some(s) => crate::fmt::parse_account(&s)?, None => largest_native(c)? };
        let args = lpad_sdk::TokenSaleArgs {
            name, symbol, total_supply: supply, bc_program, wlez_program, creator,
            sale_quantity, dex_seed, vt, vc, fee_bps, nonce,
        };
        if private { c.bc_create_token_sale_private(args) } else { c.bc_create_token_sale(args) }
    })?;
    if json {
        println!("{}", serde_json::json!({ "ok": true, "op": "bc create-token-sale", "sale": hex(sale.to_bytes().as_ref()), "token_definition": hex(token_def.to_bytes().as_ref()), "tx": hex(&tx) }));
    } else {
        ui::ok(&format!("bc create-token-sale  -  tx 0x{}", hex(&tx)));
        ui::kv("sale id", hex(sale.to_bytes().as_ref()));
        ui::kv("token def", hex(token_def.to_bytes().as_ref()));
    }
    Ok(())
}

/// Resolve the slippage floor: an explicit `--min-out` wins, otherwise
/// `quoted * (1 - slippage_bps/10000)` so an omitted `--min-out` is never a silent
/// 0 (which would disable sandwich protection - review finding F9).
fn min_out_floor(explicit: Option<u128>, quoted: u128, slippage_bps: u128) -> u128 {
    match explicit {
        Some(v) => v,
        None => {
            let bps = slippage_bps.min(10_000);
            if quoted == 0 { 0 } else { (quoted.saturating_mul(10_000 - bps) / 10_000).max(1) }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn bc_buy(paths: &WalletPaths, program: ProgramId, sale: AccountId, buyer_collateral: Option<String>, buyer_token: Option<String>, collateral_in: u128, min_out: Option<u128>, slippage_bps: u128, json: bool) -> Result<(), String> {
    let tx = run(paths, json, "buying", |c| {
        let s = c.bc_sale(sale)?;
        let bc = holding(c, buyer_collateral, s.collateral_definition_id, false, &[s.treasury_id], "collateral")?;
        let bt = recv_holding(c, buyer_token, s.token_definition_id)?;
        let min = min_out_floor(min_out, lpad_sdk::bc_quote(&s, collateral_in).tokens_out, slippage_bps);
        c.bc_buy(program, sale, bc, bt, collateral_in, min)
    })?;
    submitted(json, "bc buy", tx);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn bc_buy_private(paths: &WalletPaths, program: ProgramId, sale: AccountId, user_collateral: Option<String>, user_token: Option<String>, collateral_in: u128, min_out: Option<u128>, slippage_bps: u128, json: bool) -> Result<(), String> {
    let tx = run(paths, json, "private buy (drift-free)", |c| {
        let s = c.bc_sale(sale)?;
        let uc = holding(c, user_collateral, s.collateral_definition_id, true, &[s.treasury_id], "collateral")?;
        let ut = holding(c, user_token, s.token_definition_id, true, &[s.treasury_id], "token")?;
        let min = min_out_floor(min_out, lpad_sdk::bc_quote(&s, collateral_in).tokens_out, slippage_bps);
        c.bc_buy_private(program, sale, uc, ut, collateral_in, min)
    })?;
    submitted(json, "bc buy-private", tx);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn bc_sell(paths: &WalletPaths, program: ProgramId, sale: AccountId, seller_token: AccountId, seller_collateral: AccountId, tokens_in: u128, min_out: Option<u128>, slippage_bps: u128, json: bool) -> Result<(), String> {
    let tx = run(paths, json, "selling", |c| {
        let s = c.bc_sale(sale)?;
        let min = min_out_floor(min_out, lpad_sdk::bc_sell_quote(&s, tokens_in), slippage_bps);
        c.bc_sell(program, sale, seller_token, seller_collateral, tokens_in, min)
    })?;
    submitted(json, "bc sell", tx);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn bc_sell_private(paths: &WalletPaths, program: ProgramId, sale: AccountId, user_token: Option<String>, user_collateral: Option<String>, tokens_in: u128, min_out: Option<u128>, slippage_bps: u128, json: bool) -> Result<(), String> {
    let tx = run(paths, json, "private sell (drift-free)", |c| {
        let s = c.bc_sale(sale)?;
        let ut = holding(c, user_token, s.token_definition_id, true, &[s.treasury_id], "token")?;
        let uc = holding(c, user_collateral, s.collateral_definition_id, true, &[s.treasury_id], "collateral")?;
        let min = min_out_floor(min_out, lpad_sdk::bc_sell_quote(&s, tokens_in), slippage_bps);
        c.bc_sell_private(program, sale, ut, uc, tokens_in, min)
    })?;
    submitted(json, "bc sell-private", tx);
    Ok(())
}

// ---------------------------------------------------------------------------
// Associated Token Accounts (RFP Func: ATAs for all token interactions)
// ---------------------------------------------------------------------------

/// Resolve an ATA owner: an explicit `--owner` wins, else the wallet's default
/// signing account.
fn owner_or(c: &LaunchpadClient, owner: Option<String>) -> Result<AccountId, String> {
    match owner {
        Some(s) => crate::fmt::parse_account(&s),
        None => c.default_owner(),
    }
}

/// Create the ATA for `(owner, token_def)` (idempotent) and print its id.
pub fn create_ata(paths: &WalletPaths, ata_program: ProgramId, owner: Option<String>, token_def: AccountId, json: bool) -> Result<(), String> {
    let ata = run(paths, json, "creating associated token account", |c| {
        let owner = owner_or(c, owner)?;
        c.create_ata(ata_program, owner, token_def)
    })?;
    if json {
        println!("{}", serde_json::json!({ "ok": true, "op": "create-ata", "ata": hex(ata.to_bytes().as_ref()) }));
    } else {
        ui::ok(&format!("associated token account  -  0x{}", hex(ata.to_bytes().as_ref())));
    }
    Ok(())
}

/// Create and initialise a fresh public token holding for `token_def`, printing
/// its id.
///
/// Needed because a token transfer to a never-initialised holding is rejected:
/// the token program claims the recipient with `Claim::Authorized`, which the
/// runtime only grants to an account the transaction signed for, and the wallet
/// does not co-sign transfer recipients. The LEZ wallet CLI has no
/// token-account-init subcommand of its own, so scripts that need a funded
/// recipient (bootstrap seeding a treasury, a buyer holding, ...) had no way to
/// make one. Use this, then send to the id it prints.
pub fn init_holding(paths: &WalletPaths, token_def: AccountId, json: bool) -> Result<(), String> {
    let holding = run(paths, json, "initialising token holding", |c| {
        c.init_token_holding(token_def)
    })?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "ok": true, "op": "init-holding",
                "holding": hex(holding.to_bytes().as_ref()),
                "account": acct(holding, false),
            })
        );
    } else {
        ui::ok(&format!("token holding  -  {}", acct(holding, false)));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn bc_buy_ata(paths: &WalletPaths, program: ProgramId, ata_program: ProgramId, sale: AccountId, owner: Option<String>, fund_from: Option<String>, collateral_in: u128, min_out: Option<u128>, slippage_bps: u128, json: bool) -> Result<(), String> {
    let tx = run(paths, json, "buying (ATA)", |c| {
        let s = c.bc_sale(sale)?;
        let owner = owner_or(c, owner)?;
        if let Some(src) = &fund_from {
            let src = crate::fmt::parse_account(src)?;
            c.fund_ata(ata_program, owner, s.collateral_definition_id, src, collateral_in)?;
        }
        let min = min_out_floor(min_out, lpad_sdk::bc_quote(&s, collateral_in).tokens_out, slippage_bps);
        c.bc_buy_ata(program, ata_program, sale, owner, collateral_in, min)
    })?;
    submitted(json, "bc buy-ata", tx);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn bc_sell_ata(paths: &WalletPaths, program: ProgramId, ata_program: ProgramId, sale: AccountId, owner: Option<String>, fund_from: Option<String>, tokens_in: u128, min_out: Option<u128>, slippage_bps: u128, json: bool) -> Result<(), String> {
    let tx = run(paths, json, "selling (ATA)", |c| {
        let s = c.bc_sale(sale)?;
        let owner = owner_or(c, owner)?;
        if let Some(src) = &fund_from {
            let src = crate::fmt::parse_account(src)?;
            c.fund_ata(ata_program, owner, s.token_definition_id, src, tokens_in)?;
        }
        let min = min_out_floor(min_out, lpad_sdk::bc_sell_quote(&s, tokens_in), slippage_bps);
        c.bc_sell_ata(program, ata_program, sale, owner, tokens_in, min)
    })?;
    submitted(json, "bc sell-ata", tx);
    Ok(())
}

pub fn bc_close(paths: &WalletPaths, program: ProgramId, sale: AccountId, creator: AccountId, json: bool) -> Result<(), String> {
    let tx = run(paths, json, "closing sale", |c| c.bc_close(program, sale, creator))?;
    submitted(json, "bc close", tx);
    Ok(())
}

pub fn bc_withdraw(paths: &WalletPaths, program: ProgramId, sale: AccountId, creator_collateral: AccountId, creator_token: AccountId, creator: AccountId, json: bool) -> Result<(), String> {
    let tx = run(paths, json, "withdrawing", |c| c.bc_withdraw(program, sale, creator_collateral, creator_token, creator))?;
    submitted(json, "bc withdraw", tx);
    Ok(())
}

// ---------------------------------------------------------------------------
// LBP - creator
// ---------------------------------------------------------------------------

/// Parsed `lbp create-sale` arguments, mapped to [`LbpCreateArgs`].
pub struct LbpCreate {
    pub program: ProgramId,
    pub collateral_def: AccountId,
    pub treasury: AccountId,
    pub creator_token_holding: AccountId,
    pub creator_collateral_holding: AccountId,
    pub creator: AccountId,
    pub token_deposit: u128,
    pub collateral_seed: u128,
    pub w_start_q64: u128,
    pub w_end_q64: u128,
    pub t_start_ms: u64,
    pub t_end_ms: u64,
    pub fee_bps: u128,
    pub block_token_ceiling: u128,
    pub allowlist_root: [u8; 32],
    pub fixed_price: bool,
    pub min_duration_ms: u64,
    pub nonce: u64,
}

pub fn lbp_create_sale(paths: &WalletPaths, a: LbpCreate, json: bool) -> Result<(), String> {
    let (pool, tx) = run(paths, json, "creating LBP sale", |c| {
        c.lbp_create_sale(LbpCreateArgs {
            program: a.program,
            collateral_def: a.collateral_def,
            treasury: a.treasury,
            creator_token_holding: a.creator_token_holding,
            creator_collateral_holding: a.creator_collateral_holding,
            creator: a.creator,
            token_deposit: a.token_deposit,
            collateral_seed: a.collateral_seed,
            w_start_q64: a.w_start_q64,
            w_end_q64: a.w_end_q64,
            t_start_ms: a.t_start_ms,
            t_end_ms: a.t_end_ms,
            fee_bps: a.fee_bps,
            block_token_ceiling: a.block_token_ceiling,
            allowlist_root: a.allowlist_root,
            fixed_price: a.fixed_price,
            min_duration_ms: a.min_duration_ms,
            nonce: a.nonce,
        })
    })?;
    submitted(json, &format!("lbp create-sale (pool 0x{})", hex(pool.to_bytes().as_ref())), tx);
    Ok(())
}

pub fn lbp_pause(paths: &WalletPaths, program: ProgramId, pool: AccountId, creator: AccountId, json: bool) -> Result<(), String> {
    let tx = run(paths, json, "pausing", |c| c.lbp_pause(program, pool, creator))?;
    submitted(json, "lbp pause", tx);
    Ok(())
}

pub fn lbp_resume(paths: &WalletPaths, program: ProgramId, pool: AccountId, creator: AccountId, json: bool) -> Result<(), String> {
    let tx = run(paths, json, "resuming", |c| c.lbp_resume(program, pool, creator))?;
    submitted(json, "lbp resume", tx);
    Ok(())
}

pub fn lbp_poke(paths: &WalletPaths, program: ProgramId, pool: AccountId, json: bool) -> Result<(), String> {
    let tx = run(paths, json, "poking", |c| c.lbp_poke(program, pool))?;
    submitted(json, "lbp poke", tx);
    Ok(())
}

pub fn lbp_close(paths: &WalletPaths, program: ProgramId, pool: AccountId, creator: AccountId, json: bool) -> Result<(), String> {
    let tx = run(paths, json, "closing pool", |c| c.lbp_close(program, pool, creator))?;
    submitted(json, "lbp close", tx);
    Ok(())
}

pub fn lbp_withdraw(paths: &WalletPaths, program: ProgramId, pool: AccountId, creator_collateral: AccountId, creator_token: AccountId, creator: AccountId, json: bool) -> Result<(), String> {
    let tx = run(paths, json, "withdrawing", |c| c.lbp_withdraw(program, pool, creator_collateral, creator_token, creator))?;
    submitted(json, "lbp withdraw", tx);
    Ok(())
}

// ---------------------------------------------------------------------------
// LBP - write (participant)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn lbp_buy(paths: &WalletPaths, program: ProgramId, pool: AccountId, buyer_collateral: Option<String>, buyer_token: Option<String>, collateral_in: u128, min_out: Option<u128>, slippage_bps: u128, json: bool) -> Result<(), String> {
    let tx = run(paths, json, "buying", |c| {
        let p = c.lbp_pool(pool)?;
        let bc = holding(c, buyer_collateral, p.collateral_definition_id, false, &[p.treasury_id], "collateral")?;
        let bt = recv_holding(c, buyer_token, p.token_definition_id)?;
        let min = min_out_floor(min_out, lpad_sdk::lbp_quote(&p, c.now_ms().max(0) as u64, collateral_in).tokens_out, slippage_bps);
        c.lbp_buy(program, pool, bc, bt, collateral_in, min)
    })?;
    submitted(json, "lbp buy", tx);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn lbp_buy_gated(paths: &WalletPaths, program: ProgramId, pool: AccountId, buyer_collateral: Option<String>, buyer_token: Option<String>, collateral_in: u128, min_out: Option<u128>, slippage_bps: u128, proof: Vec<[u8; 32]>, json: bool) -> Result<(), String> {
    let tx = run(paths, json, "buying (gated)", |c| {
        let p = c.lbp_pool(pool)?;
        let bc = holding(c, buyer_collateral, p.collateral_definition_id, false, &[p.treasury_id], "collateral")?;
        let bt = recv_holding(c, buyer_token, p.token_definition_id)?;
        let min = min_out_floor(min_out, lpad_sdk::lbp_quote(&p, c.now_ms().max(0) as u64, collateral_in).tokens_out, slippage_bps);
        c.lbp_buy_gated(program, pool, bc, bt, collateral_in, min, proof)
    })?;
    submitted(json, "lbp buy-gated", tx);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn lbp_buy_ata(paths: &WalletPaths, program: ProgramId, ata_program: ProgramId, pool: AccountId, owner: Option<String>, fund_from: Option<String>, collateral_in: u128, min_out: Option<u128>, slippage_bps: u128, json: bool) -> Result<(), String> {
    let tx = run(paths, json, "buying (ATA)", |c| {
        let p = c.lbp_pool(pool)?;
        let owner = owner_or(c, owner)?;
        if let Some(src) = &fund_from {
            let src = crate::fmt::parse_account(src)?;
            c.fund_ata(ata_program, owner, p.collateral_definition_id, src, collateral_in)?;
        }
        let min = min_out_floor(min_out, lpad_sdk::lbp_quote(&p, c.now_ms().max(0) as u64, collateral_in).tokens_out, slippage_bps);
        c.lbp_buy_ata(program, ata_program, pool, owner, collateral_in, min)
    })?;
    submitted(json, "lbp buy-ata", tx);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn lbp_buy_private(paths: &WalletPaths, program: ProgramId, pool: AccountId, user_collateral: Option<String>, user_token: Option<String>, collateral_in: u128, min_out: Option<u128>, slippage_bps: u128, json: bool) -> Result<(), String> {
    let tx = run(paths, json, "private buy (drift-free)", |c| {
        let p = c.lbp_pool(pool)?;
        let uc = holding(c, user_collateral, p.collateral_definition_id, true, &[p.treasury_id], "collateral")?;
        let ut = holding(c, user_token, p.token_definition_id, true, &[p.treasury_id], "token")?;
        let min = min_out_floor(min_out, lpad_sdk::lbp_quote(&p, c.now_ms().max(0) as u64, collateral_in).tokens_out, slippage_bps);
        c.lbp_buy_private(program, pool, uc, ut, collateral_in, min)
    })?;
    submitted(json, "lbp buy-private", tx);
    Ok(())
}
