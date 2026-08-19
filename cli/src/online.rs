//! Online (wallet-backed) CLI commands - a presentation layer over `lpad_sdk`.
//! All chain logic lives in the SDK; here we resolve wallet paths, drive a live
//! spinner from the SDK's progress hook, and render results (human or `--json`).

use std::path::PathBuf;

use lpad_sdk::{BcCreateArgs, FaucetState, LaunchpadClient, LbpCreateArgs, Network, PoolState, SaleState};
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

/// Which side of the network picker a resolution is on.
#[derive(Clone, Copy)]
enum Picker {
    /// Any normal command: run the first-run picker if this wallet has never
    /// been asked, and treat an unreadable marker as fatal.
    Auto,
    /// A `--json` run: report only. Never prompt, and an unreadable marker is
    /// still fatal - a machine consumer must not be handed a silent fallback.
    Off,
    /// `lpad network`, which owns the picker: never prompt from inside
    /// resolution, and tolerate an unreadable marker.
    Owned,
}

impl WalletPaths {
    /// Resolve wallet paths with zero required config. Precedence:
    /// explicit `--config`/`--storage` > `$LEE_WALLET_HOME_DIR` > the default
    /// home `~/.lpad`. Also auto-detects the proof mode: if `RISC0_DEV_MODE`
    /// isn't already set, it honors a `proof_mode` marker ("dev"/"real") written
    /// next to the wallet by the bootstrap - so the user never has to export
    /// `RISC0_DEV_MODE` or `LEE_WALLET_HOME_DIR` for a bootstrapped chain.
    ///
    /// `network` accepts `testnet`, `paradox`, `local`, `local=<url>`, or a
    /// sequencer URL, and overrides the remembered choice for THIS invocation
    /// only - it is never written back. Omitted, the choice stored beside the
    /// wallet is used; if this wallet has never been asked and there is a
    /// terminal to ask on, the first-run picker runs once and remembers the
    /// answer. lpad bundles no local sequencer; see [`Network`].
    ///
    /// `json` is the machine-readable surface, and the picker never runs there:
    /// a menu is not an answer a script can consume, and its "which sequencer?"
    /// would be a question asked of a program. Such a run behaves exactly as it
    /// does today - the wallet config's own sequencer - and `lpad network`
    /// remains the way to choose one.
    pub fn resolve(
        config: Option<String>,
        storage: Option<String>,
        network: Option<String>,
        json: bool,
    ) -> Result<Self, String> {
        Self::resolve_inner(config, storage, network, if json { Picker::Off } else { Picker::Auto })
    }

    /// [`Self::resolve`] WITHOUT the first-run picker: for `lpad network`, which
    /// runs the picker itself and must not ask the same question twice.
    pub fn resolve_no_prompt(
        config: Option<String>,
        storage: Option<String>,
        network: Option<String>,
    ) -> Result<Self, String> {
        Self::resolve_inner(config, storage, network, Picker::Owned)
    }

    /// Paths for a wallet that does not exist yet.
    ///
    /// [`Self::resolve_inner`] refuses outright when the config file is absent -
    /// correct for every other command, and precisely wrong for the one whose job
    /// is to create it. This is the same resolution minus that guard, minus the
    /// first-run picker (there is nothing to pick for: the network is an argument
    /// here, and gets written down rather than asked about).
    ///
    /// The network defaults to `testnet` rather than to whatever LEZ's own config
    /// default happens to be. A wallet is chain-specific - every id in it is a PDA
    /// on exactly one chain - so "created, pointed somewhere unstated" is not a
    /// usable outcome. The caller prints which chain it chose.
    pub fn resolve_for_create(
        config: Option<String>,
        storage: Option<String>,
        network: Option<String>,
    ) -> Result<Self, String> {
        let network = Some(
            network
                .as_deref()
                .map(Network::parse)
                .transpose()?
                .unwrap_or(Network::Testnet),
        );
        let home = std::env::var("LEE_WALLET_HOME_DIR")
            .or_else(|_| std::env::var("NSSA_WALLET_HOME_DIR"))
            .ok()
            .unwrap_or_else(default_home);
        let config = PathBuf::from(config.unwrap_or_else(|| format!("{home}/wallet_config.json")));
        let storage = PathBuf::from(storage.unwrap_or_else(|| format!("{home}/storage.json")));
        let statistics = config
            .parent()
            .map_or_else(|| PathBuf::from("statistics.json"), |d| d.join("statistics.json"));
        if let Some(dir) = config.parent()
            && !dir.exists()
        {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("create wallet home {}: {e}", dir.display()))?;
        }
        Ok(Self { config, storage, statistics, network })
    }

    fn resolve_inner(
        config: Option<String>,
        storage: Option<String>,
        network: Option<String>,
        picker: Picker,
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
            // `lpad init-wallet` FIRST, and a repo script only as an aside. This
            // used to name `scripts/bootstrap.sh` alone, which is unreachable
            // advice for the audience most likely to hit it: someone who ran
            // `npm i -g @paradoxcomputer/lpad` has no checkout to run a script
            // from, and this is the very first message they see.
            return Err(format!(
                "no wallet at {} - create one with `lpad init-wallet` (it prints a recovery \
                 phrase once; keep it). From a clone, `bash scripts/bootstrap.sh` also \
                 builds a funded demo environment. To point at an existing wallet instead, \
                 pass --config/--storage or set $LEE_WALLET_HOME_DIR.",
                config.display()
            ));
        }
        reject_pre_v020_wallet(&config, &storage)?;
        if let Some(dir) = config.parent() {
            detect_proof_mode(dir);
        }
        // Network selection, in precedence order. Deliberately AFTER the
        // `config.exists()` check above: with no wallet at all the answer is
        // "run the bootstrap", not a menu, and prompting first would replace a
        // precise message with a question that cannot be acted on yet.
        let network = match network {
            // `--network` is documented as a one-invocation override, so it is
            // used and NOT remembered.
            Some(n) => Some(n),
            None => Self::remembered_or_pick(&config, &storage, &statistics, picker)?,
        };
        Ok(Self { config, storage, statistics, network })
    }

    /// The stored choice, or the first-run picker, or `None`.
    ///
    /// `None` is the pre-existing behaviour - "whatever the wallet config
    /// already points at" - and is what a run with no terminal gets. That is
    /// load-bearing: scripts/test-all-cli.sh, scripts/ci-e2e.sh and CI all run
    /// non-interactively against a bootstrapped wallet, and they must keep
    /// working exactly as before rather than hang on a menu nobody can see or
    /// fail on a question they never had to answer.
    fn remembered_or_pick(
        config: &std::path::Path,
        storage: &std::path::Path,
        statistics: &std::path::Path,
        picker: Picker,
    ) -> Result<Option<Network>, String> {
        let stored = match crate::network::stored(config) {
            Ok(n) => n,
            // A marker nobody can read is fatal everywhere EXCEPT in `lpad
            // network`, whose whole job is to rewrite it - failing there would
            // lock the user out of the one command that fixes the problem the
            // error is complaining about.
            Err(e) if matches!(picker, Picker::Owned) => {
                ui::warn(&e);
                None
            }
            Err(e) => return Err(e),
        };
        if let Some(n) = stored {
            return Ok(Some(n));
        }
        if !matches!(picker, Picker::Auto) || !crate::network::interactive() {
            return Ok(None);
        }
        let picked = crate::network::pick(true)?;
        // Remember BEFORE the deployment check, so declining the deploy costs
        // the user the deploy and not the pick: they are two questions, and
        // re-asking "which sequencer?" on every later command is the hang-shaped
        // behaviour this whole path exists to avoid.
        crate::network::remember(config, &picked)?;
        crate::network::check_deployment(
            config,
            storage,
            statistics,
            &picked,
            crate::network::Entry::FirstRun,
        )?;
        Ok(Some(picked))
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
    let (height, synced, chain_ms) = run(paths, json, "querying sequencer", |c| {
        Ok((c.block_height()?, c.last_synced(), c.now_ms()))
    })?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "block_height": height,
                "last_synced": synced,
                "chain_time_ms": chain_ms,
            })
        );
    } else {
        ui::header("status");
        ui::kv("sequencer block", height);
        ui::kv("wallet synced", synced);
        // The on-chain clock, not this machine's. Both launchpads gate on it -
        // a bonding-curve sale's `end_timestamp_ms`, an LBP's `t_start`/`t_end`
        // - and it only advances when a block is produced, so it trails wall
        // clock on an idle chain. Anything that waits for a sale window to open
        // or close has to watch THIS number; waiting on `date` instead is how a
        // close fails with "sale has not ended" on a chain that agrees it has.
        ui::kv("chain time (ms)", chain_ms);
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

/// The sequencer the wallet config itself points at, for reporting only.
///
/// Read straight out of the JSON rather than by opening a wallet: opening one
/// may calibrate every configured sequencer over the network, which is far too
/// much work for a line of output - and `lpad network` must stay answerable
/// while the chain is down. v0.2.1+ writes a `sequencers` list; the scalar
/// `sequencer_addr` is the pre-list spelling, still read so an older config
/// reports something truthful instead of nothing.
fn wallet_config_sequencer(config: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(config).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let from_list = v
        .get("sequencers")
        .and_then(|s| s.get(0))
        .and_then(|s| s.get("sequencer_addr"))
        .and_then(serde_json::Value::as_str);
    from_list
        .or_else(|| v.get("sequencer_addr").and_then(serde_json::Value::as_str))
        .map(str::to_owned)
}

/// `lpad network` - re-open the picker and remember the answer, or, under
/// `--json`, report the current selection without asking anything.
///
/// `overridden` says a `--network` flag was passed. That flag is documented as a
/// one-invocation override, so this command reports it and refuses to write it:
/// silently persisting an override would make `--network` mean two different
/// things depending on which command it was passed to.
/// `lpad init-wallet` - the first command a fresh install can run.
///
/// Everything else in lpad opens a wallet; nothing until now could make one.
/// `Storage::from_path` fails outright on a missing file, so a user who
/// installed the CLI and nothing else hit `Failed to load storage from …` with
/// no way forward, and the only thing that could create the file was the LEZ
/// `wallet` binary - a whole checkout, for the one step that has to happen
/// before any other. That is the gap this closes.
pub fn init_wallet(paths: &WalletPaths, json: bool) -> Result<(), String> {
    let network = paths.network.clone().unwrap_or_default();
    let (_client, mnemonic) = LaunchpadClient::create(
        paths.config.clone(),
        paths.storage.clone(),
        paths.statistics.clone(),
        Some(&network),
    )?;

    // The keys are in the clear (see LaunchpadClient::create), so the file mode
    // is the only thing standing between them and every other account on the
    // box. The default is 0644 minus umask, which on a stock Ubuntu is 0664 -
    // group-readable. Tightened here rather than hoped for.
    restrict_to_owner(&paths.storage);

    // Remember the chain, so the picker never fires for this wallet and every
    // later command is silent. A wallet is chain-specific; recording the answer
    // at creation is the only moment it is unambiguous.
    crate::network::remember(&paths.config, &network)?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "op": "init-wallet",
                "network": network.to_string(),
                "url": network.url(),
                "wallet_config": paths.config.display().to_string(),
                "storage": paths.storage.display().to_string(),
                "mnemonic": mnemonic,
                "storage_encrypted": false,
            })
        );
        return Ok(());
    }

    ui::header("new wallet");
    ui::kv("network", format!("{network}  ({})", network.url()));
    ui::kv("config", paths.config.display().to_string());
    ui::kv("storage", paths.storage.display().to_string());
    ui::warn("write this recovery phrase down now - it is shown once and stored nowhere:");
    eprintln!();
    eprintln!("    {mnemonic}");
    eprintln!();
    ui::warn(&format!(
        "{} is NOT encrypted: it holds your signing and spending keys as plain JSON. LEZ \
         v0.2.4 accepts a wallet password and discards it, so lpad does not ask for one \
         rather than implying a protection that is not there. The file has been set to \
         owner-only (0600); back it up as you would a private key.",
        paths.storage.display()
    ));
    ui::ok("next: `lpad faucet` for native LEZ, then `lpad wrap --amount 100`");
    Ok(())
}

/// Best-effort `chmod 0600`. Unix-only by construction: the mode bits do not
/// exist elsewhere, and failing to tighten a file is not a reason to fail a
/// wallet that was created successfully - it is a reason to say so.
fn restrict_to_owner(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            ui::warn(&format!(
                "could not set owner-only permissions on {}: {e} - it holds keys in the clear, \
                 so tighten it yourself",
                path.display()
            ));
        }
    }
    #[cfg(not(unix))]
    ui::warn(&format!(
        "{} holds keys in the clear and this platform has no mode bits to restrict it with",
        path.display()
    ));
}

/// `lpad network --check`: is this build's set of programs on this chain?
///
/// The same question the picker asks after a pick, minus the pick - so a script
/// can ask it. Until this existed the answer was reachable only by running the
/// interactive picker to completion, which a script cannot do, so every driver
/// either skipped the check or reimplemented the block scan in bash.
///
/// Exits non-zero when anything is missing, because the failure it prevents is
/// silent: a transaction against a program that is not deployed is dropped from
/// the mempool and reads as a timeout rather than an error, so a caller that
/// carries on spends its whole poll budget - 30 minutes per transaction, on the
/// bootstrap's own config - to learn nothing.
///
/// "No record" is not proof of absence, and the message says so: a chain that
/// was reset or restored can have forgotten the deploy transaction while still
/// running the program. `scripts/verify-deployment.sh` asks the stronger
/// question (does lpad's on-chain state still exist, and does its owner match
/// this build's id) and is the one to believe on a disagreement.
fn deployment_check(paths: &WalletPaths, json: bool) -> Result<(), String> {
    let url = paths
        .network
        .as_ref()
        .map(|n| n.url().to_owned())
        .or_else(|| wallet_config_sequencer(&paths.config))
        .unwrap_or_else(|| "the wallet's configured sequencer".to_owned());

    let status = run(paths, json, "checking which lpad programs are on this chain", |c| {
        c.lpad_deployment_status()
    })?;
    let missing: Vec<&str> = status.iter().filter(|s| !s.deployed()).map(|s| s.name).collect();

    if json {
        println!(
            "{}",
            serde_json::json!({
                "url": url,
                "deployed": missing.is_empty(),
                "missing": missing,
                "programs": status
                    .iter()
                    .map(|s| serde_json::json!({
                        "name": s.name,
                        "program_id": crate::fmt::hex_program(s.program_id),
                        "block": s.block,
                        "deployed": s.deployed(),
                    }))
                    .collect::<Vec<_>>(),
            })
        );
    } else {
        ui::header(&format!("lpad programs on {url}"));
        for s in &status {
            let block = s.block.map_or_else(|| "NO RECORD".to_owned(), |b| format!("block {b}"));
            ui::kv(s.name, format!("{block}  {}", crate::fmt::hex_program(s.program_id)));
        }
    }

    if missing.is_empty() {
        if !json {
            ui::ok("all three programs are on this chain");
        }
        return Ok(());
    }
    Err(format!(
        "{url} has no record of lpad's {} program(s). A transaction against a program that is \
         not deployed is dropped from the mempool and reads as a timeout, not an error, so \
         nothing here should submit one. Deploy with `bash scripts/bootstrap.sh` (or `lpad \
         network` on a `local=<url>` chain). A chain that was reset can forget the deploy \
         transaction while still running the program, so this can be a false alarm - confirm \
         with `bash scripts/verify-deployment.sh`, which checks lpad's on-chain state and its \
         owner rather than the deploy transaction.",
        missing.join(", ")
    ))
}

pub fn network(paths: &WalletPaths, overridden: bool, check: bool, json: bool) -> Result<(), String> {
    if check {
        return deployment_check(paths, json);
    }
    let marker = crate::network::marker_path(&paths.config);
    let source = match (&paths.network, overridden) {
        (Some(_), true) => "flag",
        (Some(_), false) => "stored",
        (None, _) => "wallet-config",
    };
    if json {
        // Report, never prompt: `--json` is the machine-readable surface, and a
        // menu is not an answer a script can consume.
        let url = paths
            .network
            .as_ref()
            .map(|n| n.url().to_owned())
            .or_else(|| wallet_config_sequencer(&paths.config));
        println!(
            "{}",
            serde_json::json!({
                "network": paths.network.as_ref().map(ToString::to_string),
                "url": url,
                "display_name": paths.network.as_ref().map(Network::display_name),
                "source": source,
                "marker": marker.display().to_string(),
                "wallet_config": paths.config.display().to_string(),
            })
        );
        return Ok(());
    }

    if overridden {
        let n = paths.network.as_ref().expect("--network parsed into Some");
        ui::header("network (this invocation only)");
        ui::kv("network", n.to_string());
        ui::kv("sequencer", n.url());
        ui::kv("stored choice", marker.display().to_string());
        eprintln!("--network overrides one command and is not remembered.");
        eprintln!("Run `lpad network` with no --network to change the stored choice.");
        return Ok(());
    }

    if !crate::network::interactive() {
        return Err(crate::network::no_tty_error(&paths.config));
    }
    if let Some(current) = &paths.network {
        eprintln!("Currently: {} ({})", current.display_name(), current.url());
    }
    let picked = crate::network::pick(false)?;
    crate::network::remember(&paths.config, &picked)?;
    crate::network::check_deployment(
        &paths.config,
        &paths.storage,
        &paths.statistics,
        &picked,
        crate::network::Entry::NetworkCmd,
    )?;
    ui::header("network");
    ui::kv("network", picked.to_string());
    ui::kv("sequencer", picked.url());
    ui::kv("remembered in", marker.display().to_string());
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

/// The creator is reported here, in both renderings, and it is not cosmetic:
/// `bc close --creator` and `bc withdraw --creator` cannot be run without it,
/// and for a `create-token-sale --private` launch the creator is a throwaway
/// account whose id appears in no other lpad output. This is the lookup that
/// recovers it from the sale id alone.
///
/// It is public information - `SaleState::creator` is plain, unencrypted state
/// that anyone who can read the sale can read - so printing it discloses nothing
/// the chain does not already publish. What a private launch buys is that the
/// creator cannot be tied to the wallet that funded it, and that is unaffected
/// by naming it here.
pub fn bc_sale_info(paths: &WalletPaths, sale: AccountId, json: bool) -> Result<(), String> {
    let s = run(paths, json, "reading sale", |c| c.bc_sale(sale))?;
    let spot = q64(s.spot_price_q64());
    if json {
        println!("{}", sale_info_json(&s, spot));
    } else {
        ui::header("bonding-curve sale");
        for (k, v) in sale_info_rows(&s, spot) {
            ui::kv(k, v);
        }
    }
    Ok(())
}

/// The `--json` body of [`bc_sale_info`]. Split out so the key set is testable
/// without a chain - `creator` in particular, which scripts driving `bc close`
/// and `bc withdraw` need and which nothing used to emit.
fn sale_info_json(s: &SaleState, spot: f64) -> serde_json::Value {
    serde_json::json!({
        "creator": hex(s.creator.to_bytes().as_ref()),
        "status": format!("{:?}", s.status), "sale_reserve": s.sale_reserve.to_string(),
        "sale_reserve_initial": s.sale_reserve_initial.to_string(), "sold_bps": s.sold_bps().to_string(),
        "real_collateral": s.real_collateral.to_string(), "spot_price": spot,
        "fee_bps": s.fee_bps.to_string(), "cum_collateral_in": s.cum_collateral_in.to_string(),
        "cum_fees": s.cum_fees.to_string(), "treasury_owed": s.treasury_owed.to_string(),
        "buy_count": s.buy_count, "sell_count": s.sell_count,
        "token_name": s.token_name, "token_symbol": s.token_symbol,
    })
}

/// The human rendering of [`bc_sale_info`], as label/value rows. A pure function
/// for the same reason as [`sale_info_json`], and it keeps the two renderings
/// from drifting: a field added to one and forgotten in the other is the bug
/// that hid the creator from `--json` consumers and humans alike.
fn sale_info_rows(s: &SaleState, spot: f64) -> Vec<(&'static str, String)> {
    let mut rows = Vec::new();
    if !s.token_name.is_empty() {
        rows.push(("token", format!("{} ({})", s.token_name, s.token_symbol)));
    }
    rows.push(("status", format!("{:?}", s.status)));
    // The account `bc close`/`bc withdraw` demand. For a `create-token-sale
    // --private` sale this is the only place it can be recovered from.
    rows.push(("creator", hex(s.creator.to_bytes().as_ref())));
    rows.push(("sale reserve", format!("{} / {}  ({} bps sold)", s.sale_reserve, s.sale_reserve_initial, s.sold_bps())));
    rows.push(("collateral raised", s.real_collateral.to_string()));
    rows.push(("spot price", format!("{spot:.10}")));
    rows.push(("buys / sells", format!("{} / {}", s.buy_count, s.sell_count)));
    rows.push(("cum collat / fees", format!("{} / {}", s.cum_collateral_in, s.cum_fees)));
    // Non-zero only after a private (disposable) buy: those escrow their fee
    // in the collateral vault instead of paying the treasury inline. It is
    // the only way to see whether `bc sweep-treasury` has anything to do.
    rows.push(("fee owed to treasury", s.treasury_owed.to_string()));
    rows
}

/// The same gap as [`bc_sale_info`] had, and closed the same way: `lbp close`
/// and `lbp withdraw` both take `--creator`, and nothing else rendered a pool's.
/// (The LBP has no private-launch command, so no pool's creator is unknowable
/// the way a `bc create-token-sale --private` one was - but a creator that only
/// `lbp ids` can reproduce, and only if you still remember the four inputs it
/// was derived from, is not much better.)
pub fn lbp_pool_info(paths: &WalletPaths, pool: AccountId, at_ms: u64, json: bool) -> Result<(), String> {
    let p = run(paths, json, "reading pool", |c| c.lbp_pool(pool))?;
    let wt = q64(p.weight_token_q64(at_ms));
    let spot = q64(p.spot_price_q64(at_ms));
    if json {
        println!("{}", pool_info_json(&p, wt, spot, at_ms));
    } else {
        ui::header("LBP pool");
        for (k, v) in pool_info_rows(&p, wt, spot) {
            ui::kv(k, v);
        }
    }
    Ok(())
}

/// The `--json` body of [`lbp_pool_info`]. See [`sale_info_json`].
fn pool_info_json(p: &PoolState, wt: f64, spot: f64, at_ms: u64) -> serde_json::Value {
    serde_json::json!({
        "creator": hex(p.creator.to_bytes().as_ref()),
        "status": format!("{:?}", p.status), "paused": p.paused,
        "reserve_token": p.reserve_token.to_string(), "reserve_collateral": p.reserve_collateral.to_string(),
        "w_token_at": wt, "spot_price_at": spot, "at_ms": at_ms,
        "cum_collateral_in": p.cum_collateral_in.to_string(), "buy_count": p.buy_count,
        "treasury_owed": p.treasury_owed.to_string(),
        "token_name": p.token_name, "token_symbol": p.token_symbol,
    })
}

/// The human rendering of [`lbp_pool_info`]. See [`sale_info_rows`].
fn pool_info_rows(p: &PoolState, wt: f64, spot: f64) -> Vec<(&'static str, String)> {
    let mut rows = Vec::new();
    if !p.token_name.is_empty() {
        rows.push(("token", format!("{} ({})", p.token_name, p.token_symbol)));
    }
    rows.push(("status", format!("{:?}{}", p.status, if p.paused { " (paused)" } else { "" })));
    // The account `lbp close`/`lbp withdraw` demand.
    rows.push(("creator", hex(p.creator.to_bytes().as_ref())));
    rows.push(("reserves tok/col", format!("{} / {}", p.reserve_token, p.reserve_collateral)));
    rows.push(("token weight @t", format!("{wt:.6}")));
    rows.push(("spot price @t", format!("{spot:.10}")));
    rows.push(("collateral raised", p.cum_collateral_in.to_string()));
    rows.push(("buys", p.buy_count.to_string()));
    // Non-zero only after `lbp withdraw`: the at-close fee is escrowed in the
    // collateral vault rather than paid out with the creator's principal. It
    // is the only way to see whether `lbp sweep-treasury` has anything to do.
    rows.push(("fee owed to treasury", p.treasury_owed.to_string()));
    rows
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
// pinata faucet: the first native LEZ an empty wallet ever holds
// ---------------------------------------------------------------------------

/// `lpad faucet` - mine the pinata proof-of-work and claim the prize.
///
/// The only command here whose slow step is neither the network nor a proof:
/// the faucet pays whoever submits a proof-of-work solution, and lpad searches
/// for that solution locally (upstream's search lives in the wallet BINARY's cli
/// module, so there is nothing to call). The search is CPU-bound, silent, and of
/// unknown length, which is why this says what it is about to do BEFORE the
/// wallet is even opened: a user who does not know the command has to solve a
/// puzzle reads a stalled CLI as a hung one, and kills it.
///
/// The four failure modes are kept apart on purpose, because their remedies are
/// different: no pinata on this chain (fund from another account), a drained
/// faucet (wait for a refill, or fund from another account), a recipient that
/// cannot hold native LEZ (initialise it), and losing the race to another
/// claimer (just re-run). The last is checked rather than guessed - see
/// [`explain_claim_failure`].
///
/// There is no cooldown and no per-account limit to hit: the guest's only test
/// is the solution itself (`lez/programs/pinata/src/main.rs` validates the hash
/// and pays, with no record of who was paid), so a second run claims again.
pub fn faucet(paths: &WalletPaths, to: Option<AccountId>, json: bool) -> Result<(), String> {
    // On stderr, and before anything slow: `--json` stdout stays machine-only
    // (same rule as the network picker's notices), and the one thing the user
    // needs to know - that a silent busy CPU is the command working, not hanging
    // - has to be on screen BEFORE the mining starts, not after it.
    ui::e_header("faucet");
    ui::e_kv("what this is", "proof-of-work: lpad solves the faucet's puzzle locally, then claims");
    ui::e_kv("expect", "usually seconds of one busy core (LPAD_FAUCET_MAX_ATTEMPTS raises the budget)");

    let (claim, before) = run(paths, json, "reading the faucet", |c| {
        let recipient = match to {
            Some(a) => a,
            // NOT `default_owner`: that picks the smallest signing-capable id for
            // ATA determinism, and the smallest id is routinely a token holding,
            // which can never hold native LEZ. The live sweep failed on exactly
            // that. `default_faucet_recipient` tests what this command needs -
            // authenticated-transfer-owned, or still initialisable - and its error
            // already names every account it skipped and why.
            None => c.default_faucet_recipient()?,
        };
        // A chain that never ran pinata, and an id that some other program has
        // taken over, are both reported by the SDK in terms of the chain it
        // actually looked at and the account it actually read. Re-wording that
        // here would only stutter, so this passes it through.
        let before = c.faucet_state()?;
        if before.drained() {
            return Err(format!(
                "the faucet is drained: it holds {} native LEZ and one claim pays {}, so there is \
                 nothing here to mine for. That is a used testnet, not a broken one - wait for an \
                 operator to refill it, or fund this wallet from an account that already holds \
                 LEZ (scripts/bootstrap.sh takes LPAD_FUNDER for exactly that)",
                before.balance,
                lpad_sdk::FAUCET_PRIZE
            ));
        }
        // Remembered BEFORE the claim, because the claim initialises the
        // recipient itself when it can: afterwards there is no way to tell an
        // account that was always ready from one that just failed to become so.
        let was_initialized = c.native_account_initialized(recipient)?;
        match c.faucet_claim(recipient) {
            Ok(claim) => Ok((claim, before)),
            Err(e) => Err(explain_claim_failure(c, &before, recipient, was_initialized, &e)),
        }
    })?;

    let amount = claim.amount;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "op": "faucet",
                "recipient": hex(claim.recipient.to_bytes().as_ref()),
                "account": acct(claim.recipient, false),
                "amount": amount.to_string(),
                "balance": claim.balance.to_string(),
                "block": claim.block,
                "tx": hex(&claim.tx_hash),
                "solution": claim.solution.to_string(),
                "difficulty": claim.difficulty,
                "initialized_recipient": claim.initialized_recipient,
                "faucet_balance_before": before.balance.to_string(),
                "faucet_claims_left": before.claims_left().saturating_sub(1).to_string(),
            })
        );
    } else {
        ui::header("faucet claim");
        ui::kv("recipient", acct(claim.recipient, false));
        ui::kv("received", format!("{amount} native LEZ"));
        ui::kv("balance", claim.balance);
        ui::kv("block", claim.block);
        ui::kv("tx", format!("0x{}", hex(&claim.tx_hash)));
        ui::kv("difficulty", claim.difficulty);
        ui::kv("solution", claim.solution);
        if claim.initialized_recipient {
            ui::kv("initialised", "yes - it could not hold native LEZ until this run");
        }
        ui::ok(&format!(
            "claimed {amount} native LEZ  -  next: `lpad wrap --amount {amount}` turns it into \
             WLEZ, the collateral `lpad bc create-token-sale` raises in"
        ));
    }
    // Both modes: a faucet about to run out is the difference between "re-run
    // it" and "go find a funder", and stderr is not the machine surface.
    let left = before.claims_left().saturating_sub(1);
    if left <= 3 {
        ui::warn(&format!(
            "the faucet has about {left} claim(s) left at {} each - the next run may find it \
             drained (fund from an existing account then, or ask an operator to refill it)",
            lpad_sdk::FAUCET_PRIZE
        ));
    }
    Ok(())
}

/// Say WHICH of the faucet's failures this was, from evidence rather than from
/// pattern-matching the message.
///
/// A claim can fail for reasons whose remedies point in opposite directions, and
/// the one that matters most - somebody else claimed while lpad was mining - is
/// not a fault at all. It is also checkable: the pinata guest re-seeds its
/// challenge from the old seed on every successful claim, so a seed that moved
/// between the read and the failure IS the proof that a claim landed in between,
/// and the solution mined here was scored against a challenge that no longer
/// exists. Re-running mines against the new one and wins.
///
/// A re-read that itself fails proves nothing, so it is never turned into a
/// verdict. Where it changes what can honestly be said, it is reported as what
/// it is - a read that failed - and otherwise the original error is passed
/// through untouched rather than dressed up in a guess.
///
/// Both re-reads are made here, before the verdict, so the verdict itself is a
/// pure function of what they returned. That costs one extra read in the case
/// the account check alone used to settle - on a path that has already failed
/// and is about to print, which is the cheapest place in the command to spend
/// it, and buys the branch coverage this diagnosis had none of.
fn explain_claim_failure(
    c: &LaunchpadClient,
    before: &FaucetState,
    recipient: AccountId,
    was_initialized: bool,
    e: &str,
) -> String {
    claim_failure_message(
        recipient,
        was_initialized,
        c.native_account_initialized(recipient),
        before,
        c.faucet_state(),
        e,
    )
}

/// The verdict itself, over the two re-reads rather than over a client, so the
/// distinction each branch turns on is testable without a chain.
fn claim_failure_message(
    recipient: AccountId,
    was_initialized: bool,
    initialized_now: Result<bool, String>,
    before: &FaucetState,
    now: Result<FaucetState, String>,
    e: &str,
) -> String {
    // Checked first: this one fails before any mining happens, and it is the
    // only mode whose remedy is a command run from a DIFFERENT wallet.
    //
    // Only a read that actually SAID "not initialised" earns that verdict. This
    // used to be `.unwrap_or(false)`, which promoted "the sequencer did not
    // answer" into a diagnosis - and being the first branch, it pre-empted the
    // two evidence-based ones below. The account lpad had just initialised and
    // possibly already paid was then declared unable to receive native LEZ "at
    // all", and the user sent to the one remedy in this command that needs a
    // different wallet. An `Err` here proves nothing and is treated as nothing.
    if !was_initialized && matches!(initialized_now, Ok(false)) {
        return format!(
            "nothing was claimed: {} is not initialised under the authenticated-transfer program, \
             so it cannot receive native LEZ at all, and lpad could not initialise it. {e}",
            acct(recipient, false)
        );
    }
    match now {
        Ok(now) if now.drained() => format!(
            "the faucet ran dry while lpad was mining: it held {} when this started and holds {} \
             now, under the {} one claim pays. Wait for an operator to refill it, or fund this \
             wallet from an account that already holds LEZ",
            before.balance,
            now.balance,
            lpad_sdk::FAUCET_PRIZE
        ),
        Ok(now) if now.challenge.seed != before.challenge.seed => format!(
            "somebody else claimed the faucet while lpad was mining: the pinata re-seeds its \
             challenge on every successful claim, so the solution mined here scores against a \
             challenge that no longer exists. Nothing is wrong and nothing is used up - the chain \
             sets no cooldown and no per-account limit - so re-run `lpad faucet` to mine against \
             the new challenge. (the rejection itself: {e})"
        ),
        // Neither faucet branch had evidence either. If the account re-read is
        // what failed, say so instead of passing through an error that reads as
        // though lpad had checked: "we could not look" and "we looked and it is
        // unusable" have different remedies, and only one of them is `re-run`.
        _ => match initialized_now {
            Err(re) if !was_initialized => format!(
                "the claim failed and lpad could not tell whether it landed: {} was not \
                 initialised under the authenticated-transfer program when this run started, and \
                 re-reading the account to see whether lpad initialised it failed too ({re}). \
                 Re-run `lpad faucet` - a claim that did land leaves the account initialised, and \
                 a re-run mines against the current challenge either way. (the original failure: \
                 {e})",
                acct(recipient, false)
            ),
            _ => e.to_owned(),
        },
    }
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

/// The spinner label for a discovery run. The default path reads the on-chain
/// per-creator index and is over in a few reads; `--refresh`/`--deep` add the
/// pre-index fallback, which is thousands of sequential reads with nothing to
/// shortcut it - so a user who asked for one is told that is what they are
/// waiting on rather than left to guess.
fn discovery_phase(opts: lpad_sdk::Discovery, fast: &'static str, full: &'static str) -> &'static str {
    if opts.refresh || opts.deep { full } else { fast }
}

/// The `unreadable` array both listings emit under `--json`: one object per id
/// the chain named and this run could not read, carrying the read's own error so
/// a caller can tell a flaky sequencer from a state this build cannot decode.
fn unreadable_json(noun: &str, unreadable: &[(AccountId, String)]) -> serde_json::Value {
    serde_json::Value::Array(
        unreadable
            .iter()
            .map(|(id, e)| serde_json::json!({ noun: hex(id.to_bytes().as_ref()), "error": e }))
            .collect(),
    )
}

/// The `--json` body of [`my_sales`], carrying the incompleteness marker beside
/// the rows.
fn sales_json(sales: &[(AccountId, SaleState)], unreadable: &[(AccountId, String)]) -> serde_json::Value {
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
    serde_json::json!({
        "sales": arr,
        "partial": !unreadable.is_empty(),
        "unreadable": unreadable_json("sale", unreadable),
    })
}

/// The pool half of [`sales_json`].
fn pools_json(pools: &[(AccountId, PoolState)], unreadable: &[(AccountId, String)]) -> serde_json::Value {
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
    serde_json::json!({
        "pools": arr,
        "partial": !unreadable.is_empty(),
        "unreadable": unreadable_json("pool", unreadable),
    })
}

/// The human-path counterpart: one summary line under the rows, so a short
/// listing is visible where the rows are, not only in the per-id warnings the
/// SDK printed before the header. Silent when nothing was missed.
fn warn_partial(noun: &str, unreadable: &[(AccountId, String)]) {
    if !unreadable.is_empty() {
        ui::warn(&format!(
            "this listing is INCOMPLETE: {} {noun}(s) the chain lists for this wallet could not be read (see the warnings above). Do not read a missing row as \"that {noun} is gone\"",
            unreadable.len()
        ));
    }
}

/// A listing can be SHORT, and both renderings have to say so.
///
/// The SDK returns the ids the chain's own creator index named and this run
/// could not read (`Listing::unreadable`); those are not absences, they are
/// sales that exist behind a read that failed. The human path already got a
/// stderr warning per id. `--json` got nothing at all: the payload looked like a
/// complete listing and the process exited 0, so a script asking "did my sale
/// land?" was told no. `partial` and `unreadable` are that fact in-band, where a
/// `jq` filter can see it.
///
/// The exit code stays 0 deliberately. This IS a successful listing - the
/// entries in `sales` are real and usable, and a hard failure would throw them
/// away along with the warning. What the caller must not do is read the array as
/// exhaustive, so the marker rides with the data rather than replacing it:
/// `if .partial then error else .sales end` is the check.
pub fn my_sales(paths: &WalletPaths, refresh: bool, deep: bool, json: bool) -> Result<(), String> {
    let opts = lpad_sdk::Discovery { refresh, deep };
    let phase = discovery_phase(
        opts,
        "reading your on-chain sale index",
        "re-deriving every candidate sale id (pre-index fallback - this is slow)",
    );
    let listing = run(paths, json, phase, |c| c.my_sales_with(opts))?;
    let sales = listing.found;
    if json {
        println!("{}", sales_json(&sales, &listing.unreadable));
    } else {
        ui::header("my bonding-curve sales");
        // "None found" is a claim, and it is only true when nothing was missed.
        // An empty listing that failed to read the one sale it was told about
        // must not answer "create one" - that is the wrong answer at its most
        // expensive.
        if sales.is_empty() && !listing.unreadable.is_empty() {
            ui::kv("(none readable)", "every sale the chain lists for this wallet failed to read - see below");
        } else if sales.is_empty() {
            // The `--refresh` hint matters here and only here. Empty is now a
            // real answer - the chain's per-creator index said so - but it is
            // also what a wallet whose sales predate that index sees, and
            // nothing else would tell the user that a full re-derivation is a
            // thing they can ask for. Wording unchanged: a live sweep reads it.
            ui::kv("(none found)", "create one with `bc create-sale`, derive with `bc ids`, or re-scan with `--refresh`");
        }
        for (id, s) in &sales {
            let label = if s.token_name.is_empty() { String::new() } else { format!("{} ({})  ", s.token_name, s.token_symbol) };
            ui::kv(&hex(id.to_bytes().as_ref()), format!("{label}{:?}  reserve {}/{}  raised {}", s.status, s.sale_reserve, s.sale_reserve_initial, s.real_collateral));
        }
        warn_partial("sale", &listing.unreadable);
    }
    Ok(())
}

/// The pool half of [`my_sales`], including the `partial`/`unreadable` marker
/// and the reason the exit code stays 0.
pub fn my_pools(paths: &WalletPaths, refresh: bool, deep: bool, json: bool) -> Result<(), String> {
    let opts = lpad_sdk::Discovery { refresh, deep };
    let phase = discovery_phase(
        opts,
        "reading your on-chain pool index",
        "re-deriving every candidate pool id (pre-index fallback - this is slow)",
    );
    let listing = run(paths, json, phase, |c| c.my_pools_with(opts))?;
    let pools = listing.found;
    if json {
        println!("{}", pools_json(&pools, &listing.unreadable));
    } else {
        ui::header("my LBP pools");
        // See the note in `my_sales`, both halves.
        if pools.is_empty() && !listing.unreadable.is_empty() {
            ui::kv("(none readable)", "every pool the chain lists for this wallet failed to read - see below");
        } else if pools.is_empty() {
            ui::kv("(none found)", "create one with `lbp create-sale`, derive with `lbp ids`, or re-scan with `--refresh`");
        }
        for (id, p) in &pools {
            let label = if p.token_name.is_empty() { String::new() } else { format!("{} ({})  ", p.token_name, p.token_symbol) };
            ui::kv(&hex(id.to_bytes().as_ref()), format!("{label}{:?}{}  reserves {}/{}", p.status, if p.paused { " (paused)" } else { "" }, p.reserve_token, p.reserve_collateral));
        }
        warn_partial("pool", &listing.unreadable);
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

/// Launch a token and its sale, and print every id the sale's own operator will
/// need again.
///
/// The creator and its token holding are printed because of `--private`. That
/// path mints a throwaway, unlinkable creator A inside the SDK and never names
/// the wallet's real account on chain; `bc close --creator`, `bc withdraw
/// --creator --creator-token` and nothing else can settle the sale afterwards,
/// and A is derivable from no output lpad has - not `bc sale-info`, not
/// `my-sales`, not the sale id. Printing the sale id alone therefore handed the
/// operator a sale they could not close: the keys were in the wallet, the ids
/// were unknowable.
///
/// PRIVACY: these ids are deliberately unlinkable ON CHAIN, and showing them to
/// their own owner in their own terminal does not change that - the unlinkability
/// is a property of what was published, not of what the launching process knows.
/// The one thing that would break it is the operator pasting creator and sale id
/// somewhere public together, which is exactly the link the launch spent a
/// shield/deshield round trip to avoid, so the private arm says so on the line
/// after. That is also why the reminder is only on the private arm: for a public
/// launch the creator is the account the user passed in and the chain already
/// names it, so warning about it would be noise that teaches the warning is
/// ignorable.
#[allow(clippy::too_many_arguments)]
pub fn bc_create_token_sale(paths: &WalletPaths, name: String, symbol: String, supply: u128, sale_quantity: u128, dex_seed: u128, vt: u128, vc: u128, fee_bps: u128, creator: Option<String>, private: bool, nonce: u64, json: bool) -> Result<(), String> {
    let start = if private { "launching token privately" } else { "launching token (mint + sale)" };
    let l = run(paths, json, start, |c| {
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
        println!("{}", launch_json(&l, private));
    } else {
        ui::ok(&format!("bc create-token-sale  -  tx 0x{}", hex(&l.tx)));
        for (k, v) in launch_rows(&l) {
            ui::kv(k, v);
        }
        if private {
            ui::warn(PRIVATE_LAUNCH_KEEP_THESE);
        }
    }
    Ok(())
}

/// Printed after a `--private` launch, where the creator and its token holding
/// are unrecoverable from anywhere else. Both halves matter: record them, and do
/// not publish them next to the sale id.
const PRIVATE_LAUNCH_KEEP_THESE: &str =
    "record the creator and creator token above: `bc close --creator` and `bc withdraw --creator \
     --creator-token` need them, this launch is the only thing that will ever tell you what they \
     are, and no lpad command can derive them from the sale id. Keep them to yourself - \
     publishing them beside the sale id is the one act that undoes this launch's unlinkability";

/// The `--json` body of [`bc_create_token_sale`].
fn launch_json(l: &lpad_sdk::TokenLaunch, private: bool) -> serde_json::Value {
    serde_json::json!({
        "ok": true, "op": "bc create-token-sale",
        "sale": hex(l.sale.to_bytes().as_ref()),
        "token_definition": hex(l.token_definition.to_bytes().as_ref()),
        "creator": hex(l.creator.to_bytes().as_ref()),
        "creator_token_holding": hex(l.creator_token_holding.to_bytes().as_ref()),
        "private": private,
        "tx": hex(&l.tx),
    })
}

/// The human rendering of [`bc_create_token_sale`], as label/value rows.
fn launch_rows(l: &lpad_sdk::TokenLaunch) -> Vec<(&'static str, String)> {
    vec![
        ("sale id", hex(l.sale.to_bytes().as_ref())),
        ("token def", hex(l.token_definition.to_bytes().as_ref())),
        // The two `bc close`/`bc withdraw` cannot be run without, and the two a
        // private launch mints where the operator cannot see them.
        ("creator", hex(l.creator.to_bytes().as_ref())),
        ("creator token", hex(l.creator_token_holding.to_bytes().as_ref())),
    ]
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

/// One atomic private buy (`BuyDisposable`): the buyer's two holdings are
/// private slots of a single transaction. Additive - `bc buy-private`'s saga is
/// still here and unchanged.
#[allow(clippy::too_many_arguments)]
pub fn bc_buy_disposable(paths: &WalletPaths, program: ProgramId, sale: AccountId, user_collateral: Option<String>, user_token: Option<String>, collateral_in: u128, min_out: Option<u128>, slippage_bps: u128, json: bool) -> Result<(), String> {
    let tx = run(paths, json, "private buy (one atomic proof)", |c| {
        let s = c.bc_sale(sale)?;
        let uc = holding(c, user_collateral, s.collateral_definition_id, true, &[s.treasury_id], "collateral")?;
        let ut = holding(c, user_token, s.token_definition_id, true, &[s.treasury_id], "token")?;
        // One clock read for both the price timestamp and the deadline; the
        // bonding curve prices off supply, not time, so the quote below needs no
        // timestamp - unlike the LBP, see `lbp_buy_disposable`.
        let at = c.private_buy_time()?;
        let min = min_out_floor(min_out, lpad_sdk::bc_quote(&s, collateral_in).tokens_out, slippage_bps);
        c.bc_buy_disposable(program, sale, uc, ut, collateral_in, min, at)
    })?;
    submitted(json, "bc buy-disposable", tx);
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
///
/// It is also the only way to satisfy `create-sale`'s treasury precondition:
/// both programs now check on chain that `--treasury` is already an initialised
/// fungible holding of the sale's collateral definition, so this has to run -
/// and land - before the create. Renaming or removing this command means every
/// error message that names it as the remedy is pointing at nothing.
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

/// Hand the fee a private (disposable) buy escrowed in the collateral vault to
/// the treasury pinned into the sale at creation.
///
/// Takes no creator account, unlike `bc_close`/`bc_withdraw` beside it: the
/// instruction has no signer slot at all, and the destination is fixed in the
/// sale, so the wallet here is a submitter paying gas rather than an authority.
/// It still needs a wallet because someone has to sign and pay for the
/// transaction envelope.
pub fn bc_sweep_treasury(paths: &WalletPaths, program: ProgramId, sale: AccountId, json: bool) -> Result<(), String> {
    let tx = run(paths, json, "sweeping the escrowed fee to the treasury", |c| c.bc_sweep_treasury(program, sale))?;
    submitted(json, "bc sweep-treasury", tx);
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

/// Hand the at-close fee `lbp withdraw` escrowed in the collateral vault to the
/// treasury pinned into the pool at creation.
///
/// The mirror of [`bc_sweep_treasury`], including the missing creator account:
/// the instruction has no signer slot and the destination is fixed in the pool,
/// so the wallet here is a submitter paying gas rather than an authority. It
/// still needs a wallet because someone has to sign and pay for the envelope.
pub fn lbp_sweep_treasury(paths: &WalletPaths, program: ProgramId, pool: AccountId, json: bool) -> Result<(), String> {
    let tx = run(paths, json, "sweeping the escrowed fee to the treasury", |c| c.lbp_sweep_treasury(program, pool))?;
    submitted(json, "lbp sweep-treasury", tx);
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

/// One atomic private buy (`BuyDisposable`): the buyer's two holdings are
/// private slots of a single transaction. Additive - `lbp buy-private`'s saga is
/// still here and unchanged.
#[allow(clippy::too_many_arguments)]
pub fn lbp_buy_disposable(paths: &WalletPaths, program: ProgramId, pool: AccountId, user_collateral: Option<String>, user_token: Option<String>, collateral_in: u128, min_out: Option<u128>, slippage_bps: u128, json: bool) -> Result<(), String> {
    let tx = run(paths, json, "private buy (one atomic proof)", |c| {
        let p = c.lbp_pool(pool)?;
        let uc = holding(c, user_collateral, p.collateral_definition_id, true, &[p.treasury_id], "collateral")?;
        let ut = holding(c, user_token, p.token_definition_id, true, &[p.treasury_id], "token")?;
        // THE SLIPPAGE TRAP: an LBP is priced at a point in time, and this buy is
        // priced at `at.at_ms` rather than at inclusion (a privacy transaction
        // cannot carry the clock account). Quoting the floor at any other "now"
        // prices the trade against a different weight than the guest will use, so
        // every private buy would trip its own `min-out`. Same value, both places.
        let at = c.private_buy_time()?;
        let min = min_out_floor(min_out, lpad_sdk::lbp_quote(&p, at.at_ms, collateral_in).tokens_out, slippage_bps);
        c.lbp_buy_disposable(program, pool, uc, ut, collateral_in, min, at)
    })?;
    submitted(json, "lbp buy-disposable", tx);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lpad_sdk::{FaucetChallenge, FAUCET_PRIZE};

    fn recipient() -> AccountId {
        AccountId::new([7u8; 32])
    }
    fn faucet(balance: u128, seed: u8) -> FaucetState {
        FaucetState {
            account: AccountId::new([1u8; 32]),
            balance,
            challenge: FaucetChallenge { difficulty: 2, seed: [seed; 32] },
        }
    }
    /// What every re-read looks like when the sequencer is the thing that failed.
    fn unread() -> String {
        "connection reset by peer".to_owned()
    }

    /// The verdict "this account cannot receive native LEZ at all" is only
    /// earned by a read that SAID so. A read that failed knows nothing, and
    /// saying it anyway tells a user whose account lpad had just initialised -
    /// and quite possibly already paid - that it never can be, with a remedy
    /// that needs a second wallet on another machine.
    #[test]
    fn an_unreadable_account_is_not_reported_as_unable_to_receive_lez() {
        let m = claim_failure_message(
            recipient(),
            false,
            Err(unread()),
            &faucet(10 * FAUCET_PRIZE, 1),
            Ok(faucet(10 * FAUCET_PRIZE, 1)),
            "the claim timed out waiting for inclusion",
        );
        assert!(
            !m.contains("cannot receive native LEZ"),
            "a failed read was dressed up as a diagnosis: {m}"
        );
        // ...and it says which of the two it is, rather than passing through an
        // error that reads as though lpad had checked.
        assert!(m.contains("could not tell"), "the uncertainty is not stated: {m}");
        assert!(m.contains(&unread()), "the read failure that caused it is not named: {m}");
        assert!(m.contains("the claim timed out"), "the original failure is not carried: {m}");
    }

    /// The same read, when it succeeds and says "not initialised", still gets
    /// the definitive answer - the fix must not cost the diagnosis that works.
    #[test]
    fn an_account_read_as_uninitialised_is_still_diagnosed_definitively() {
        let m = claim_failure_message(
            recipient(),
            false,
            Ok(false),
            &faucet(10 * FAUCET_PRIZE, 1),
            Ok(faucet(10 * FAUCET_PRIZE, 1)),
            "register_account was rejected",
        );
        assert!(m.contains("cannot receive native LEZ at all"), "{m}");
    }

    /// The uninitialised branch is checked first, so a wrong answer there
    /// pre-empts the two branches that DO have evidence. A drained faucet is one
    /// of them, and it must survive an account read that failed.
    #[test]
    fn a_failed_account_read_does_not_pre_empt_the_drained_faucet_diagnosis() {
        let m = claim_failure_message(
            recipient(),
            false,
            Err(unread()),
            &faucet(10 * FAUCET_PRIZE, 1),
            Ok(faucet(FAUCET_PRIZE - 1, 1)),
            "the claim was rejected",
        );
        assert!(m.contains("ran dry while lpad was mining"), "{m}");
    }

    /// The other one: somebody else's claim re-seeded the challenge, which is
    /// not a fault at all and whose remedy is simply to re-run.
    #[test]
    fn a_failed_account_read_does_not_pre_empt_the_reseeded_challenge_diagnosis() {
        let m = claim_failure_message(
            recipient(),
            false,
            Err(unread()),
            &faucet(10 * FAUCET_PRIZE, 1),
            Ok(faucet(10 * FAUCET_PRIZE, 2)),
            "the claim was rejected",
        );
        assert!(m.contains("somebody else claimed the faucet"), "{m}");
    }

    /// An account that was already initialised before the run cannot be the
    /// uninitialised case whatever the re-read says, so the original error is
    /// passed through untouched - the discipline the doc comment promises.
    #[test]
    fn an_already_initialised_account_passes_the_original_error_through() {
        let e = "the claim timed out waiting for inclusion";
        let m = claim_failure_message(
            recipient(),
            true,
            Err(unread()),
            &faucet(10 * FAUCET_PRIZE, 1),
            Err(unread()),
            e,
        );
        assert_eq!(m, e, "an error was dressed up in a guess");
    }

    // -----------------------------------------------------------------------
    // Rendering: the ids an operator cannot get anywhere else
    // -----------------------------------------------------------------------

    fn id(b: u8) -> AccountId {
        AccountId::new([b; 32])
    }
    fn launch() -> lpad_sdk::TokenLaunch {
        lpad_sdk::TokenLaunch {
            sale: id(1),
            token_definition: id(2),
            creator: id(3),
            creator_token_holding: id(4),
            tx: [9u8; 32],
        }
    }

    /// A `--private` launch mints its own creator and its own token holding, and
    /// they are settlement-critical: `bc close --creator` and `bc withdraw
    /// --creator --creator-token` take no other accounts, the wallet holds the
    /// keys without knowing which they are, and nothing else lpad prints - not
    /// the sale id, not `my-sales` - can derive them. A launch that reports only
    /// the sale hands its operator a raise they cannot withdraw.
    #[test]
    fn a_launch_reports_the_two_accounts_that_can_close_and_withdraw_it() {
        let l = launch();
        let rows = launch_rows(&l);
        let creator = hex(l.creator.to_bytes().as_ref());
        let token = hex(l.creator_token_holding.to_bytes().as_ref());
        assert!(
            rows.iter().any(|(k, v)| *k == "creator" && *v == creator),
            "the launch did not print its creator: {rows:?}"
        );
        assert!(
            rows.iter().any(|(k, v)| *k == "creator token" && *v == token),
            "the launch did not print the creator's token holding: {rows:?}"
        );
        let j = launch_json(&l, true);
        assert_eq!(j["creator"], creator, "--json dropped the creator: {j}");
        assert_eq!(j["creator_token_holding"], token, "--json dropped the creator holding: {j}");
        assert_eq!(j["private"], true, "--json must say which arm ran: {j}");
    }

    /// The reminder is only worth printing if it says both halves: keep these,
    /// and do not publish them beside the sale id. The second half is the whole
    /// privacy answer - the ids are unlinkable on chain, and showing them to
    /// their owner in their own terminal does not change that, but pasting them
    /// somewhere public does.
    #[test]
    fn the_private_launch_reminder_says_both_record_them_and_keep_them_private() {
        let m = PRIVATE_LAUNCH_KEEP_THESE;
        assert!(m.contains("bc close --creator") && m.contains("--creator-token"), "{m}");
        assert!(m.contains("Keep them to yourself"), "{m}");
        assert!(m.contains("unlinkability"), "{m}");
    }

    /// `bc sale-info` is the only way back to a sale's creator once the launch
    /// output has scrolled away, so both renderings have to carry it - the JSON
    /// one because `bc close`/`bc withdraw` are what a script drives next.
    #[test]
    fn sale_info_renders_the_creator_in_both_renderings() {
        let s = SaleState { creator: id(5), ..SaleState::default() };
        let want = hex(id(5).to_bytes().as_ref());
        assert_eq!(sale_info_json(&s, 0.0)["creator"], want);
        let rows = sale_info_rows(&s, 0.0);
        assert!(rows.iter().any(|(k, v)| *k == "creator" && *v == want), "{rows:?}");
    }

    /// `lbp pool-info` had the same gap: `lbp close` and `lbp withdraw` both
    /// take `--creator` and nothing rendered a pool's.
    #[test]
    fn pool_info_renders_the_creator_in_both_renderings() {
        let p = PoolState { creator: id(6), ..PoolState::default() };
        let want = hex(id(6).to_bytes().as_ref());
        assert_eq!(pool_info_json(&p, 0.5, 1.0, 0)["creator"], want);
        let rows = pool_info_rows(&p, 0.5, 1.0);
        assert!(rows.iter().any(|(k, v)| *k == "creator" && *v == want), "{rows:?}");
    }

    // -----------------------------------------------------------------------
    // Rendering: a listing that is known to be short has to say so in-band
    // -----------------------------------------------------------------------

    /// The concrete harm this closes: a script polls `my-sales --json` for the
    /// sale it just created, the sequencer fails to serve that one account, and
    /// the payload it gets back is a well-formed listing that does not contain
    /// it. The stderr warning is invisible to that script, and the exit code is
    /// 0 because the listing did succeed. So the incompleteness has to be IN the
    /// payload.
    #[test]
    fn a_short_sales_listing_is_marked_partial_and_names_what_it_missed() {
        let missed = id(8);
        let j = sales_json(&[(id(7), SaleState::default())], &[(missed, unread())]);
        assert_eq!(j["partial"], true, "a short listing did not say so: {j}");
        assert_eq!(j["unreadable"][0]["sale"], hex(missed.to_bytes().as_ref()), "{j}");
        assert_eq!(j["unreadable"][0]["error"], unread(), "the read error was dropped: {j}");
        // The rows that WERE read are still delivered: this is a partial
        // success, not a failure, which is why the exit code stays 0 and the
        // marker rides along instead of replacing the data.
        assert_eq!(j["sales"].as_array().map(Vec::len), Some(1), "{j}");
    }

    /// And the marker is present-and-false on a healthy run, not absent: a
    /// consumer must be able to write `.partial` without first testing whether
    /// the key exists, or the check is one an old payload silently passes.
    #[test]
    fn a_complete_listing_still_carries_the_marker_set_to_false() {
        let j = sales_json(&[(id(7), SaleState::default())], &[]);
        assert_eq!(j["partial"], false, "{j}");
        assert_eq!(j["unreadable"].as_array().map(Vec::len), Some(0), "{j}");
        let j = pools_json(&[(id(7), PoolState::default())], &[]);
        assert_eq!(j["partial"], false, "{j}");
        assert_eq!(j["unreadable"].as_array().map(Vec::len), Some(0), "{j}");
    }

    /// The pool listing carries the same marker, under its own noun.
    #[test]
    fn a_short_pools_listing_is_marked_partial_and_names_what_it_missed() {
        let missed = id(8);
        let j = pools_json(&[], &[(missed, unread())]);
        assert_eq!(j["partial"], true, "{j}");
        assert_eq!(j["unreadable"][0]["pool"], hex(missed.to_bytes().as_ref()), "{j}");
    }
}
