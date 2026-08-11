//! # lpad-sdk - SDK for the LPAD launchpad (bonding curve = RFP-015, LBP = RFP-016).
//!
//! Full lifecycle for participants (discover, quote, buy/sell, query position)
//! and creators (create, pause/resume, close, withdraw), over a public account
//! or the private drift-free `deshield → public buy → re-shield` path. The
//! CLI and mini-app FFI are thin layers over this crate.

use std::collections::HashMap;
use std::path::PathBuf;

use common::transaction::LeeTransaction;
use lee::{
    privacy_preserving_transaction::circuit::ProgramWithDependencies,
    program::Program,
    public_transaction::{Message, WitnessSet},
    AccountId, PublicTransaction,
};
use lee_core::{account::Account, program::ProgramId};
use sequencer_service_rpc::RpcClient as _;
use serde::Serialize;
use wallet::{AccDecodeData, AccountIdentity, WalletCore};

pub use bonding_curve_core::{SaleState, SaleStatus as BcStatus};
pub use lbp_core::{PoolState, SaleStatus as LbpStatus};

pub type Result<T> = std::result::Result<T, String>;
pub type TxHash = [u8; 32];
/// Progress/phase sink: called with a short label before each long step.
pub type ProgressFn = Box<dyn Fn(&str)>;

/// Accounts/amount of one `AtomicDisposable` private saga (see
/// [`Lpad::private_disposable_saga`]). `spend_src` is the shielded account that
/// pays/sells; `reshield_target` the shielded account the realized output lands
/// in.
struct DisposableSaga {
    spend_src: AccountId,
    reshield_target: AccountId,
    amount: u128,
    /// Progress phase reported just before the public leg ("buying on the
    /// curve" / "selling on the curve" / "buying on the pool"). Per-call because
    /// bc-buy and lbp-buy share `text` but word this leg differently.
    op_phase: &'static str,
    text: SagaText,
}

/// The program-specific wording for one private saga, so the shared shape can
/// emit the exact same progress phases and no-loss error text the three callers
/// used before they were collapsed into [`Lpad::private_disposable_saga`].
struct SagaText {
    deshield_phase: &'static str,
    reshield_phase: &'static str,
    /// "public buy failed" / "public sell failed".
    op_failed: &'static str,
    /// "Buy" / "Sell" (the `{Op} error:` prefix).
    op_capitalized: &'static str,
    /// The deshielded asset noun: "collateral" / "tokens".
    spend_noun: &'static str,
    /// Verb agreement for `spend_noun`: "is" (collateral) / "are" (tokens).
    is_are: &'static str,
    /// Pronoun for `spend_noun`: "it" (collateral) / "them" (tokens).
    it_them: &'static str,
    /// "buy produced no tokens to re-shield" / "sell produced no collateral ...".
    no_output: &'static str,
}

/// Wording for a private buy that deshields collateral (bc buy, lbp buy).
const BUY_COLLATERAL_SAGA: SagaText = SagaText {
    deshield_phase: "deshielding collateral (privacy proof, minutes)",
    reshield_phase: "re-shielding tokens (privacy proof, minutes)",
    op_failed: "public buy failed",
    op_capitalized: "Buy",
    spend_noun: "collateral",
    is_are: "is",
    it_them: "it",
    no_output: "buy produced no tokens to re-shield",
};

/// Wording for a private sell that deshields project tokens (bc sell).
const SELL_TOKENS_SAGA: SagaText = SagaText {
    deshield_phase: "deshielding tokens (privacy proof, minutes)",
    reshield_phase: "re-shielding collateral (privacy proof, minutes)",
    op_failed: "public sell failed",
    op_capitalized: "Sell",
    spend_noun: "tokens",
    is_are: "are",
    it_them: "them",
    no_output: "sell produced no collateral to re-shield",
};

/// A wallet-owned fungible token holding (public or shielded), with the token's
/// on-chain name + wallet label resolved when available. Powers `my-balance`
/// and holding auto-detection so callers never have to paste account ids.
#[derive(Debug, Clone)]
pub struct Holding {
    pub account: AccountId,
    pub private: bool,
    /// True for the account's native LEZ balance (vs a fungible token holding).
    pub native: bool,
    pub label: Option<String>,
    pub definition: AccountId,
    pub token_name: Option<String>,
    pub balance: u128,
}

/// Arguments for the one-shot `bc_create_token_sale`: mint a project token (with
/// name+symbol metadata) and open a bonding-curve sale raising in native LEZ
/// (collateral = WLEZ).
pub struct TokenSaleArgs {
    pub name: String,
    pub symbol: String,
    pub total_supply: u128,
    pub bc_program: ProgramId,
    pub wlez_program: ProgramId,
    pub creator: AccountId,
    pub sale_quantity: u128,
    pub dex_seed: u128,
    pub vt: u128,
    pub vc: u128,
    pub fee_bps: u128,
    pub nonce: u64,
}

/// Pull `(name, symbol)` out of a `data:application/json,{...}` metadata URI,
/// decoding JSON escapes so quote/backslash-containing labels round-trip.
fn parse_name_symbol(uri: &str) -> Option<(String, String)> {
    let json = uri.split_once(',').map_or(uri, |(_, j)| j);
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let field = |k: &str| value.get(k)?.as_str().map(str::to_owned);
    Some((field("name")?, field("symbol")?))
}

/// Nonce range scanned when deriving the wallet's own sales/pools for discovery
/// (`my_sales`/`my_pools`). Full open enumeration needs the LP-0012 indexer.
const DISCOVERY_NONCE_MAX: u64 = 8;

// ---------------------------------------------------------------------------
// Quote types (pure, computed from on-chain state via the pricing libs)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BcQuote {
    pub tokens_out: u128,
    pub fee: u128,
    pub effective_collateral: u128,
    pub spot_price_before: f64,
    pub spot_price_after: f64,
    pub price_impact_pct: f64,
}

#[derive(Debug, Clone)]
pub struct LbpQuote {
    pub tokens_out: u128,
    pub w_token: f64,
    pub spot_price: f64,
}

fn q64_to_f64(x: u128) -> f64 {
    x as f64 / (1u128 << 64) as f64
}

/// Quote a bonding-curve buy from sale state (no chain access).
///
/// `virt_collateral` was bounded `< 2^64` at sale creation, but a caller-supplied
/// `collateral_in` can push the post-buy reserve `virt_collateral + c_eff` (or the
/// `Vt * c_eff` product) out of the Q64.64 pricing domain, which would panic the
/// `assert!` inside `buy_tokens_out`/`spot_price_q64`. The on-chain `Buy` reverts
/// under the same assert, so there is no funds risk; rather than crash the buy
/// command we return a degenerate quote (`tokens_out` = 0, post-price = pre-price)
/// so the caller's slippage floor stays clean. Mirrors the CLI `bc quote` guard.
#[must_use]
pub fn bc_quote(state: &SaleState, collateral_in: u128) -> BcQuote {
    let before = q64_to_f64(bonding_curve_core::spot_price_q64(state.virt_collateral, state.virt_token));
    // `buy_fee` computes `collateral_in * fee_bps` internally and panics on overflow,
    // so bound that multiply here too: an out-of-domain fee multiply, post-buy reserve,
    // or product yields a degenerate quote rather than a panic.
    let fee_overflows = collateral_in.checked_mul(state.fee_bps).is_none();
    let fee = if fee_overflows { 0 } else { bonding_curve_core::buy_fee(collateral_in, state.fee_bps) };
    let c_eff = collateral_in.saturating_sub(fee);
    let out_of_domain = fee_overflows
        || state
            .virt_collateral
            .checked_add(c_eff)
            .is_none_or(|v| v >= bonding_curve_core::MAX_VIRT_COLLATERAL)
        || state.virt_token.checked_mul(c_eff).is_none();
    if out_of_domain {
        return BcQuote {
            tokens_out: 0,
            fee,
            effective_collateral: c_eff,
            spot_price_before: before,
            spot_price_after: before,
            price_impact_pct: 0.0,
        };
    }
    let (tokens_out, fee, c_eff) =
        bonding_curve_core::buy_tokens_out(state.virt_token, state.virt_collateral, state.fee_bps, collateral_in);
    let after = q64_to_f64(bonding_curve_core::spot_price_q64(
        state.virt_collateral + c_eff,
        state.virt_token - tokens_out,
    ));
    BcQuote {
        tokens_out,
        fee,
        effective_collateral: c_eff,
        spot_price_before: before,
        spot_price_after: after,
        price_impact_pct: if before > 0.0 { (after / before - 1.0) * 100.0 } else { 0.0 },
    }
}

/// Expected collateral out for a bonding-curve **sell** of `tokens_in` (the seller's
/// receipt, net of fee; pre-slippage). Pairs with [`bc_quote`] for the buy side.
#[must_use]
pub fn bc_sell_quote(state: &SaleState, tokens_in: u128) -> u128 {
    bonding_curve_core::sell_collateral_out(state.virt_token, state.virt_collateral, state.fee_bps, tokens_in).0
}

/// Quote an LBP buy at time `at_ms` from pool state (no chain access).
///
/// The Q64.64 price math left-shifts the reserves/input by 64, so `buy_tokens_out`
/// asserts `reserve_collateral + collateral_in < MAX_RESERVE` (and the non-fixed
/// `spot_price_q64` asserts `reserve_collateral < 2^64`); fixed-price mode asserts
/// `collateral_in < MAX_RESERVE`. A pool created with out-of-domain reserves, or a
/// caller-supplied `collateral_in` that pushes the reserve past `2^64`, would panic
/// those asserts. The on-chain `Buy` reverts under the same bound, so there is no
/// funds risk; rather than crash the participant's buy we return a degenerate quote
/// (`tokens_out` = 0) so the slippage floor stays clean. Mirrors the CLI `lbp quote`
/// guard.
#[must_use]
pub fn lbp_quote(state: &PoolState, at_ms: u64, collateral_in: u128) -> LbpQuote {
    let wt = state.weight_token_q64(at_ms);
    let out_of_domain = if state.fixed_price {
        collateral_in >= lbp_core::MAX_RESERVE
    } else {
        state.reserve_collateral.checked_add(collateral_in).is_none_or(|t| t >= lbp_core::MAX_RESERVE)
            || state.reserve_token >= lbp_core::MAX_RESERVE
    };
    if out_of_domain {
        return LbpQuote { tokens_out: 0, w_token: q64_to_f64(wt), spot_price: 0.0 };
    }
    let tokens_out = if state.fixed_price {
        lbp_core::fixed_price_tokens_out(collateral_in, state.fixed_price_q64)
    } else {
        lbp_core::buy_tokens_out(state.reserve_token, state.reserve_collateral, wt, collateral_in)
    };
    LbpQuote { tokens_out, w_token: q64_to_f64(wt), spot_price: q64_to_f64(state.spot_price_q64(at_ms)) }
}

/// Derive the LBP allowlist Merkle leaf for a buyer's **collateral-holding**
/// account id - the exact value `BuyGated` binds against on-chain
/// (`leaf == allowlist_leaf(buyer_collateral_holding.account_id)`). Creators must
/// build the allowlist tree from the *collateral-holding* ids buyers pay from
/// (the account that authorizes the gated buy), not a wallet-identity account.
/// Use this so the leaf passed to [`LaunchpadClient::lbp_buy_gated`] can't be
/// mis-derived.
#[must_use]
pub fn lbp_allowlist_leaf(collateral_holding: AccountId) -> [u8; 32] {
    lbp_core::allowlist_leaf(&collateral_holding)
}

/// A program's deployed id (RISC0 image id).
///
/// These are compile-time constants derived from the committed guest ELFs
/// (`programs/artifacts/lpad/*.bin` for lpad's own programs, and upstream's
/// `artifacts/lez/programs/*.bin` for the ATA program), so they are identical on
/// every machine.
///
/// Until LEZ v0.2.1 the ELFs were built in-process by `risc0-build` and read off
/// disk at run time, which made ids toolchain-dependent and let the CLI submit
/// against a stale id after any guest rebuild. v0.2.1 builds them reproducibly in
/// a pinned container and commits the result, so that whole class of drift is
/// gone.
///
/// The `Result` is vestigial - these cannot fail - but is kept so the ~30 call
/// sites in the SDK and CLI are unaffected.
pub fn bc_program_id() -> Result<ProgramId> {
    Ok(lpad_guests::bonding_curve().id())
}
pub fn lbp_program_id() -> Result<ProgramId> {
    Ok(lpad_guests::lbp().id())
}
pub fn wlez_program_id() -> Result<ProgramId> {
    Ok(lpad_guests::wlez().id())
}
/// The canonical upstream ATA program. lpad used to deploy its own hardened fork;
/// the recipient contract that fork enforced is now re-asserted by the launchpad
/// programs themselves (`util::assert_ata_recipient`).
pub fn ata_program_id() -> Result<ProgramId> {
    Ok(programs::ata().id())
}

/// The WLEZ definition and vault PDAs, derived from the wlez program id alone.
///
/// Free functions rather than [`LpadClient`] methods because they need no wallet
/// and no chain: they are pure PDA derivation. That is what lets a deployment
/// check assert wlez is live - unlike the bonding curve and the LBP, wlez owns no
/// account whose id a bootstrap happens to record, so without these it was the
/// one deployed program nothing could verify.
pub fn wlez_definition_id(wlez_program: ProgramId) -> AccountId {
    wlez_core::get_wlez_definition_id(&wlez_program)
}

/// See [`wlez_definition_id`].
pub fn wlez_vault_id(wlez_program: ProgramId) -> AccountId {
    wlez_core::get_wlez_vault_id(&wlez_program)
}

// ---------------------------------------------------------------------------
// Creator argument bundles
// ---------------------------------------------------------------------------

pub struct BcCreateArgs {
    pub program: ProgramId,
    pub collateral_def: AccountId,
    pub treasury: AccountId,
    pub creator_token_holding: AccountId,
    pub creator: AccountId,
    pub sale_quantity: u128,
    pub dex_seed: u128,
    pub virt_token: u128,
    pub virt_collateral: u128,
    pub fee_bps: u128,
    pub one_directional: bool,
    pub end_timestamp_ms: u64,
    pub min_duration_ms: u64,
    pub nonce: u64,
}

pub struct LbpCreateArgs {
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

// ---------------------------------------------------------------------------
// Networks
// ---------------------------------------------------------------------------

/// The Logos public testnet sequencer. The default: it is the network most
/// users mean, and it is operated independently of this repo.
pub const TESTNET_SEQUENCER: &str = "https://testnet.lez.logos.co";

/// The Paradox Computer testnet sequencer.
pub const PARADOX_SEQUENCER: &str = "https://seq-testnet.paradox.computer";

/// Which sequencer to talk to.
///
/// lpad has no bundled local sequencer - it targets real networks. Mirrors the
/// LEZ wallet's own `NetworkAlias` so the vocabulary matches, with `paradox`
/// added.
///
/// Both known networks run a LEZ build whose built-in program image ids match
/// **v0.2.4**, which is why the crates are pinned there. The pin is not
/// cosmetic: a LEZ version bump changes those ids (token,
/// authenticated_transfer, clock, the privacy circuit), and a build pinned to a
/// version the operators have not adopted cannot transact against these chains
/// at all - it fails as a timeout, not as an error. The `chain_parity` tests at
/// the bottom of this file assert the equality on every CI run; treat a failure
/// there as "do not ship this pin", not as a flake.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Network {
    /// `https://testnet.lez.logos.co`
    #[default]
    Testnet,
    /// `https://seq-testnet.paradox.computer`
    Paradox,
    /// Any other sequencer URL, passed through verbatim.
    Custom(String),
}

impl Network {
    /// Resolve `testnet`, `paradox`, or a sequencer URL.
    pub fn parse(alias: &str) -> Result<Self> {
        match alias.trim() {
            "testnet" | "logos" => Ok(Self::Testnet),
            "paradox" => Ok(Self::Paradox),
            other if other.starts_with("http://") || other.starts_with("https://") => {
                Ok(Self::Custom(other.to_owned()))
            }
            other => Err(format!(
                "unknown network {other:?} - use `testnet`, `paradox`, or an http(s) sequencer URL"
            )),
        }
    }

    /// The sequencer URL this network resolves to.
    #[must_use]
    pub fn url(&self) -> &str {
        match self {
            Self::Testnet => TESTNET_SEQUENCER,
            Self::Paradox => PARADOX_SEQUENCER,
            Self::Custom(u) => u,
        }
    }
}

impl std::fmt::Display for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Testnet => write!(f, "testnet"),
            Self::Paradox => write!(f, "paradox"),
            Self::Custom(u) => write!(f, "{u}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// A wallet-backed launchpad client. Open once, then drive the lifecycle.
pub struct LaunchpadClient {
    wallet: WalletCore,
    rt: tokio::runtime::Runtime,
    progress: Option<ProgressFn>,
}

impl LaunchpadClient {
    /// Open the LEZ wallet (config + storage) and connect to its sequencer.
    ///
    /// `network`, when given, overrides the sequencer in the wallet config
    /// *without rewriting the file* - so `--network` is a per-invocation switch
    /// and cannot silently repoint a wallet for later commands.
    pub fn open(
        config: PathBuf,
        storage: PathBuf,
        statistics: PathBuf,
        network: Option<&Network>,
    ) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("tokio runtime: {e}"))?;
        let overrides = match network {
            None => None,
            Some(n) => {
                let url = n
                    .url()
                    .parse()
                    .map_err(|e| format!("invalid sequencer URL for network {n}: {e}"))?;
                // v0.2.1+ replaced the single `sequencer_addr` with a list, so the
                // override sets a one-element list rather than a scalar.
                Some(wallet::config::WalletConfigOverrides {
                    sequencers: Some(vec![wallet::config::SequencerConnectionData {
                        sequencer_addr: url,
                        basic_auth: None,
                    }]),
                    ..Default::default()
                })
            }
        };
        // Async since v0.2.1: opening may calibrate the configured sequencers.
        let wallet = rt
            .block_on(WalletCore::new_update_chain(config, storage, statistics, overrides))
            .map_err(|e| format!("open wallet: {e}"))?;
        Ok(Self { wallet, rt, progress: None })
    }

    /// Persist the sequencer latency statistics gathered during this session.
    ///
    /// On open the wallet calibrates every sequencer absent from
    /// `statistics.json` with `calibration_limit` (default 100) sequential
    /// requests. The CLI opens a fresh wallet per command, so skipping this would
    /// pay that cost on every invocation - and against a real WAN sequencer that
    /// is far worse than it was against localhost. Call once after a successful
    /// command, as the upstream wallet CLI does.
    pub fn record_sequencer_statistics(&mut self) -> Result<()> {
        let rt = &self.rt;
        let wallet = &mut self.wallet;
        let _silence = gag::Gag::stdout().ok();
        rt.block_on(async { wallet.client_rotation().await })
            .map_err(|e| format!("record sequencer statistics: {e}"))
    }

    /// Register a progress sink. The SDK calls it with a short phase label
    /// before each long step (proving, submit, inclusion), so a CLI/UI can show
    /// real loading states. Called on the caller's thread.
    pub fn set_progress(&mut self, f: impl Fn(&str) + 'static) {
        self.progress = Some(Box::new(f) as ProgressFn);
    }

    fn report(&self, phase: &str) {
        if let Some(p) = &self.progress {
            p(phase);
        }
    }

    /// Borrow the underlying wallet (for advanced callers).
    #[must_use]
    pub fn wallet(&self) -> &WalletCore {
        &self.wallet
    }

    /// Current sequencer block height.
    pub fn block_height(&self) -> Result<u64> {
        // v0.2.1+ routes this through the wallet's multi-sequencer client; the
        // public `sequencer_client` field is gone.
        self.rt
            .block_on(async { self.wallet.get_last_block_id().await })
            .map_err(|e| format!("get block height: {e}"))
    }

    /// The wallet's last-synced block (for the private commitment view).
    #[must_use]
    pub fn last_synced(&self) -> u64 {
        self.wallet.storage().last_synced_block()
    }

    /// Bring the wallet's private (shielded) note view up to chain head.
    ///
    /// `WalletCore::new_update_chain` only restores the *persisted* note state;
    /// it does not advance to the latest block. Without re-scanning, a process
    /// (or any op after a prior private op) spends/derives notes against a stale
    /// view and re-creates an output commitment already on chain - which the
    /// sequencer rejects as `InvalidInput("Commitment already seen")`. Every
    /// private entry point syncs first so each spend/output is fresh.
    pub fn sync_private(&mut self) -> Result<()> {
        let rt = &self.rt;
        let wallet = &mut self.wallet;
        // v0.2.1's sync_to_block prints per-block progress AND persists (which
        // prints again) on every block, so the gag matters more than it did on
        // rc4. `sync_to_latest_block` folds the old block_height + sync_to_block
        // pair into one call.
        let _silence = gag::Gag::stdout().ok();
        rt.block_on(async { wallet.sync_to_latest_block().await })
            .map(|_head| ())
            .map_err(|e| format!("sync private view: {e}"))
    }

    // ---- low-level submit -------------------------------------------------

    fn submit_public<I: Serialize>(
        &self,
        program_id: ProgramId,
        account_ids: Vec<AccountId>,
        signers: &[AccountId],
        instruction: I,
    ) -> Result<TxHash> {
        self.rt.block_on(async {
            let nonces = self
                .wallet
                .get_accounts_nonces(signers)
                .await
                .map_err(|e| format!("fetch nonces: {e}"))?;
            let mut keys: Vec<&lee::PrivateKey> = Vec::with_capacity(signers.len());
            for s in signers {
                keys.push(
                    self.wallet
                        .get_account_public_signing_key(*s)
                        .ok_or_else(|| format!("wallet has no signing key for {s}"))?,
                );
            }
            let message = Message::try_new(program_id, account_ids, nonces, instruction)
                .map_err(|e| format!("build message: {e}"))?;
            let witness = WitnessSet::for_message(&message, &keys);
            self.report("submitting transaction");
            // `helm_owned()` replaces the removed public `sequencer_client` field.
            let hash = self
                .wallet
                .helm_owned()
                .send_transaction(LeeTransaction::Public(PublicTransaction::new(message, witness)))
                .await
                .map_err(|e| format!("submit: {e}"))?;
            self.report("waiting for inclusion");
            self.wallet
                .poll_transaction(hash)
                .await
                .map_err(|e| format!("tx rejected / not included: {e}"))?;
            Ok(to_hash(hash))
        })
    }

    // ---- reads / discovery (participant) ---------------------------------

    /// Read a bonding-curve sale's on-chain state.
    pub fn bc_sale(&self, sale: AccountId) -> Result<SaleState> {
        let acc = self.read_account(sale)?;
        SaleState::try_from(&acc.data).map_err(|_| "account is not a bonding-curve sale".into())
    }

    /// Read a sale's state for *trading*, rejecting one whose pinned
    /// `ata_program_id` is not the canonical/deployed ATA program. The on-chain
    /// program cannot know the canonical ATA image id (programs are addressed by
    /// guest image id), so a malicious creator can pin a no-op ATA program and
    /// later drain the collateral vault via `SellAta`. A keypair `Buy` never
    /// inspects the pin, so guard it here before a participant commits collateral.
    fn bc_sale_checked(&self, sale: AccountId) -> Result<SaleState> {
        let s = self.bc_sale(sale)?;
        let canonical = ata_program_id()?;
        if s.ata_program_id != canonical {
            return Err(
                "sale pins a non-canonical ATA program (possible no-op-drain sale); refusing to trade"
                    .into(),
            );
        }
        Ok(s)
    }

    /// Read an LBP pool's on-chain state.
    pub fn lbp_pool(&self, pool: AccountId) -> Result<PoolState> {
        let acc = self.read_account(pool)?;
        PoolState::try_from(&acc.data).map_err(|_| "account is not an LBP pool".into())
    }

    /// Read an LBP pool's state for *trading*, rejecting one whose pinned
    /// `ata_program_id` is not the canonical/deployed ATA program (mirrors
    /// [`Self::bc_sale_checked`]). The on-chain program can't know the canonical
    /// ATA image id, so a creator could pin a non-canonical program; refuse to
    /// commit collateral to such a pool before the buy is built.
    fn lbp_pool_checked(&self, pool: AccountId) -> Result<PoolState> {
        let p = self.lbp_pool(pool)?;
        if p.ata_program_id != ata_program_id()? {
            return Err(
                "pool pins a non-canonical ATA program (possible no-op-drain pool); refusing to trade"
                    .into(),
            );
        }
        Ok(p)
    }

    /// Discover active bonding-curve sales among candidate ids (batch read,
    /// skipping non-sale / closed accounts). Full open enumeration needs the
    /// LP-0012 event indexer; this resolves a known/derived id set.
    pub fn discover_bc_sales(&self, ids: &[AccountId]) -> Vec<(AccountId, SaleState)> {
        ids.iter()
            .filter_map(|&id| {
                let s = self.bc_sale(id).ok()?;
                matches!(s.status, BcStatus::Open).then_some((id, s))
            })
            .collect()
    }

    /// Discover active LBP pools among candidate ids.
    pub fn discover_lbp_pools(&self, ids: &[AccountId]) -> Vec<(AccountId, PoolState)> {
        ids.iter()
            .filter_map(|&id| {
                let p = self.lbp_pool(id).ok()?;
                matches!(p.status, LbpStatus::Open).then_some((id, p))
            })
            .collect()
    }

    /// Public token balance of an account (definition id, balance).
    pub fn balance(&self, account: AccountId) -> Result<(AccountId, u128)> {
        let acc = self.read_account(account)?;
        match token_core::TokenHolding::try_from(&acc.data) {
            Ok(token_core::TokenHolding::Fungible { definition_id, balance }) => Ok((definition_id, balance)),
            _ => Err("account is not a fungible token holding".into()),
        }
    }

    /// A wallet-owned private (shielded) holding's (definition id, balance), if
    /// known. Sync the wallet's private view first for fresh data.
    #[must_use]
    pub fn private_balance(&self, account: AccountId) -> Option<(AccountId, u128)> {
        let acc = self.wallet.get_account_private(account)?;
        match token_core::TokenHolding::try_from(&acc.data) {
            Ok(token_core::TokenHolding::Fungible { definition_id, balance }) => Some((definition_id, balance)),
            _ => None,
        }
    }

    /// Fail-fast guard for the private sagas: the shielded source must cover
    /// `amount` *before* we begin a multi-minute privacy proof. Without it, an
    /// over-balance request only surfaces when tx1's deshield fails - after the
    /// user has already paid for the proof attempt. Read-only; returns a clean
    /// error. (Callers sync the private view first, so the balance is fresh.)
    fn ensure_shielded_covers(&self, source: AccountId, amount: u128) -> Result<()> {
        match self.private_balance(source) {
            Some((_, bal)) if bal >= amount => Ok(()),
            Some((_, bal)) => Err(format!(
                "shielded balance {bal} does not cover the {amount} requested (sync the wallet or use a smaller amount)"
            )),
            None => Err("spend source is not a known shielded holding in this wallet (sync first)".into()),
        }
    }

    fn read_account(&self, id: AccountId) -> Result<Account> {
        self.rt
            .block_on(async { self.wallet.get_account_public(id).await })
            .map_err(|e| format!("read account: {e}"))
    }

    /// Current on-chain wall-clock (ms) from the sequencer Clock account; 0 if
    /// unavailable. Used to quote a time-priced LBP buy at "now" for slippage.
    #[must_use]
    pub fn now_ms(&self) -> i64 {
        self.read_account(lbp_core::CLOCK_01)
            .map(|a| lbp_core::clock_ms(&a.data))
            .unwrap_or(0)
    }

    /// Absolute expiry timestamp for a new transaction: `now_ms + TTL`. This is the
    /// `with_timestamp_validity_window(..deadline)` upper bound the guests enforce,
    /// so a signed tx withheld past it can no longer be included - closing the
    /// "hold a signed order and submit it later at a worse price" vector. TTL
    /// defaults to 120s, overridable via `$LPAD_TX_TTL_MS`.
    ///
    /// Fails closed when the on-chain clock is unreadable: rather than mint a tx
    /// with an unbounded (`u64::MAX`) validity window - which the sequencer/relay
    /// could withhold and replay later at a worse price - we error out so the
    /// caller retries once the clock is reachable.
    fn tx_deadline(&self) -> Result<u64> {
        const DEFAULT_TX_TTL_MS: u64 = 120_000;
        let ttl = std::env::var("LPAD_TX_TTL_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_TX_TTL_MS);
        match self.now_ms() {
            n if n > 0 => Ok((n as u64).saturating_add(ttl)),
            _ => Err("on-chain clock unreadable - refusing to mint a transaction with an \
                      unbounded validity window; retry once the sequencer clock is reachable"
                .into()),
        }
    }

    /// Re-shield `amount` from a public ephemeral account A to the user's private
    /// holding, retrying the (minutes-long) privacy proof on transient failure.
    /// NOTE: this is best-effort, NOT a durable journal - if every attempt fails
    /// (or the process dies mid-saga) the funds are NOT lost: they sit in the
    /// wallet-owned public account A (visible in `lpad my-balance`), recoverable by
    /// re-running the re-shield, but PUBLICLY held until then (the privacy of that
    /// one trade is lost). The returned error says exactly that.
    fn reshield_with_retry(&mut self, from_public: AccountId, to_private: AccountId, amount: u128) -> Result<TxHash> {
        const ATTEMPTS: u32 = 3;
        let mut last = String::new();
        for attempt in 1..=ATTEMPTS {
            // Idempotency guard against the poll-timed-out-but-landed case: an
            // earlier re-shield attempt's STARK can actually land on-chain (the
            // source A is drained) while `poll_tx` times out and reports Err.
            // Re-submitting would over-draw the now-empty source and panic the
            // token guest (`Insufficient balance`). Detect the drained source
            // first and report the no-loss location instead of over-drawing.
            if self.holding_balance(from_public).unwrap_or(0) < amount {
                return Err(format!(
                    "re-shield source account {from_public} is already drained ({amount} no longer present); \
                     a prior re-shield attempt landed on-chain despite a poll timeout. No funds were lost - \
                     they are reachable in this wallet (re-run `lpad sync` then `lpad my-balance`). Last error: {last}"
                ));
            }
            match self.privacy_token_transfer(
                AccountIdentity::Public(from_public),
                AccountIdentity::PrivateOwned(to_private),
                to_private,
                amount,
            ) {
                Ok(tx) => return Ok(tx),
                Err(e) => {
                    last = e;
                    self.report(&format!("re-shield attempt {attempt}/{ATTEMPTS} failed; retrying"));
                }
            }
        }
        Err(format!(
            "re-shield failed after {ATTEMPTS} attempts - {amount} tokens are SAFE but PUBLICLY held in the \
             ephemeral account A (a wallet-owned public holding, visible in `lpad my-balance`); re-run to move \
             them into your private holding. No funds were lost. Last error: {last}"
        ))
    }

    // ---- discovery / auto-detect (no ids to paste) -----------------------

    /// Resolve a fungible token definition's on-chain name, if present.
    fn token_name(&self, definition: AccountId) -> Option<String> {
        let acc = self.read_account(definition).ok()?;
        match token_core::TokenDefinition::try_from(&acc.data) {
            Ok(token_core::TokenDefinition::Fungible { name, .. }) => Some(name),
            _ => None,
        }
    }

    /// Every wallet-owned fungible holding (public + shielded), with token name +
    /// wallet label resolved. Drives `my-balance` and holding auto-detection.
    pub fn my_holdings(&self) -> Result<Vec<Holding>> {
        let mut names: HashMap<AccountId, Option<String>> = HashMap::new();
        let mut out: Vec<Holding> = Vec::new();
        for (id, private) in self
            .wallet_public_accounts()
            .into_iter()
            .map(|id| (id, false))
            .chain(self.wallet_private_accounts().into_iter().map(|id| (id, true)))
        {
            let acc = if private {
                self.wallet.get_account_private(id)
            } else {
                self.read_account(id).ok()
            };
            let Some(acc) = acc else { continue };
            // v0.2.1 inverted the label map (it is keyed by label now, not by
            // account id) and made it private, so look labels up by account.
            let label = self.wallet_label(id, private);
            // Native LEZ lives in the account balance (public accounts only; there
            // is no shielded-native note).
            if !private && acc.balance > 0 {
                out.push(Holding {
                    account: id, private: false, native: true, label: label.clone(),
                    definition: id, token_name: Some("LEZ".to_owned()), balance: acc.balance,
                });
            }
            if let Ok(token_core::TokenHolding::Fungible { definition_id, balance }) =
                token_core::TokenHolding::try_from(&acc.data)
            {
                let token_name =
                    names.entry(definition_id).or_insert_with(|| self.token_name(definition_id)).clone();
                out.push(Holding { account: id, private, native: false, label, definition: definition_id, token_name, balance });
            }
        }
        Ok(out)
    }

    /// The wallet's largest-balance holding of `definition` on the given side
    /// (public/shielded) - auto-fills buyer/seller/user holdings from a sale's
    /// declared token + collateral definitions, so callers omit account ids.
    pub fn find_holding(&self, definition: AccountId, private: bool, exclude: &[AccountId]) -> Option<AccountId> {
        self.my_holdings()
            .ok()?
            .into_iter()
            .filter(|h| !h.native && h.private == private && h.definition == definition && !exclude.contains(&h.account))
            .max_by_key(|h| h.balance)
            .map(|h| h.account)
    }

    /// The wallet's own label for an account, if any.
    fn wallet_label(&self, id: AccountId, private: bool) -> Option<String> {
        let tagged = if private {
            wallet::account::AccountIdWithPrivacy::Private(id)
        } else {
            wallet::account::AccountIdWithPrivacy::Public(id)
        };
        self.wallet
            .storage()
            .labels_for_account(tagged)
            .next()
            .map(ToString::to_string)
    }

    /// Both imported accounts and those derived from the key tree.
    ///
    /// v0.2.1 replaced the public `WalletChainStore { user_data, .. }` with a
    /// private `Storage` behind accessors, so this reads the key chain instead of
    /// unioning two maps by hand.
    fn wallet_public_accounts(&self) -> Vec<AccountId> {
        self.wallet
            .storage()
            .key_chain()
            .public_account_ids()
            .map(|(id, _chain_index)| id)
            .collect()
    }

    /// As [`Self::wallet_public_accounts`], for the shielded side.
    ///
    /// Note this is a superset of the rc4 behaviour: it also yields shared
    /// (group-managed) private account ids. That is harmless here because
    /// `get_account_private` returns `None` for them, so `my_holdings` skips them.
    fn wallet_private_accounts(&self) -> Vec<AccountId> {
        self.wallet
            .storage()
            .key_chain()
            .private_account_ids()
            .map(|(id, _chain_index)| id)
            .collect()
    }

    /// Distinct fungible token definitions the wallet holds (sale token/collateral
    /// candidates for discovery).
    fn wallet_definitions(&self) -> Vec<AccountId> {
        let mut defs: Vec<AccountId> = self
            .my_holdings()
            .unwrap_or_default()
            .into_iter()
            .filter(|h| !h.native)
            .map(|h| h.definition)
            .collect();
        defs.sort();
        defs.dedup();
        defs
    }

    /// Bonding-curve sales the wallet can derive: creator ∈ your accounts,
    /// token/collateral defs ∈ tokens you hold, nonce `0..DISCOVERY_NONCE_MAX`.
    /// (Listing everyone's sales needs the LP-0012 event indexer, absent here.)
    pub fn my_sales(&self) -> Result<Vec<(AccountId, SaleState)>> {
        let program = bc_program_id()?;
        let (creators, defs) = (self.wallet_public_accounts(), self.wallet_definitions());
        let mut found = Vec::new();
        for &creator in &creators {
            for &td in &defs {
                for &cd in &defs {
                    if td == cd {
                        continue;
                    }
                    for nonce in 0..DISCOVERY_NONCE_MAX {
                        let id = bonding_curve_core::compute_sale_pda(program, td, cd, creator, nonce);
                        if let Ok(s) = self.bc_sale(id) {
                            found.push((id, s));
                        }
                    }
                }
            }
        }
        Ok(found)
    }

    /// LBP pools the wallet can derive (same candidate scheme as [`my_sales`]).
    pub fn my_pools(&self) -> Result<Vec<(AccountId, PoolState)>> {
        let program = lbp_program_id()?;
        let (creators, defs) = (self.wallet_public_accounts(), self.wallet_definitions());
        let mut found = Vec::new();
        for &creator in &creators {
            for &td in &defs {
                for &cd in &defs {
                    if td == cd {
                        continue;
                    }
                    for nonce in 0..DISCOVERY_NONCE_MAX {
                        let id = lbp_core::compute_pool_pda(program, td, cd, creator, nonce);
                        if let Ok(p) = self.lbp_pool(id) {
                            found.push((id, p));
                        }
                    }
                }
            }
        }
        Ok(found)
    }

    // ---- wlez: native-LEZ collateral (wrap / unwrap) ---------------------

    /// WLEZ token-definition id for a deployed wlez program - the collateral
    /// definition for native-LEZ sales.
    #[must_use]
    pub fn wlez_definition(&self, wlez_program: ProgramId) -> AccountId {
        wlez_core::get_wlez_definition_id(&wlez_program)
    }

    /// Whether wlez has been initialised (its token definition exists on-chain).
    #[must_use]
    pub fn wlez_initialized(&self, wlez_program: ProgramId) -> bool {
        let def = wlez_core::get_wlez_definition_id(&wlez_program);
        self.read_account(def)
            .map(|a| token_core::TokenDefinition::try_from(&a.data).is_ok())
            .unwrap_or(false)
    }

    /// One-shot wlez setup after deploy: claim the vault PDA + create the WLEZ
    /// token definition. `reference_token_def` is any token-program-owned
    /// definition (read only for its token program id); `payer` signs.
    pub fn initialize_wlez(
        &self,
        wlez_program: ProgramId,
        reference_token_def: AccountId,
        payer: AccountId,
    ) -> Result<TxHash> {
        let vault = wlez_core::get_wlez_vault_id(&wlez_program);
        let def = wlez_core::get_wlez_definition_id(&wlez_program);
        let init_holding = wlez_core::get_wlez_init_holding_id(&wlez_program);
        self.report("initializing wlez");
        self.submit_public(
            wlez_program,
            vec![vault, def, init_holding, reference_token_def, payer],
            &[payer],
            wlez_core::Instruction::Initialize {
                // Pin the canonical token program so a malicious reference
                // definition can't redirect the WLEZ definition's owning
                // program at bootstrap.
                token_program_id: programs::token().id(),
                // Pin the canonical native/authenticated-transfer program so
                // every later Wrap escrows through the real native program
                // (Wrap checks user_native against this stored id, preventing
                // an unbacked-WLEZ mint via a no-op native program).
                native_program_id: programs::authenticated_transfer().id(),
            },
        )
    }

    /// Guard a WLEZ deployment before committing real LEZ to it: reject a WLEZ
    /// whose pinned token OR native program is not the canonical one.
    /// `Initialize` is permissionless and the on-chain guest cannot know either
    /// canonical image id (programs are addressed by host-computed guest image
    /// ids), so a front-running attacker could pin a malicious token program
    /// (owning the WLEZ definition) or a no-op native program (recorded in the
    /// vault), then mint unbacked WLEZ - draining the native vault via `Unwrap`
    /// or extracting real assets on the AMM. The on-chain guest only rejects the
    /// zero/default no-op; the full pin is enforced here, participant-side,
    /// before any escrow commits (mirrors [`Self::bc_sale_checked`]). This
    /// downgrades the front-run from a drain to a recoverable DoS.
    fn wlez_programs_checked(&self, wlez_program: ProgramId) -> Result<()> {
        // 1. The WLEZ definition must be owned by the canonical token program
        //    (Wrap/Unwrap trust `definition.program_owner` for the Mint/Burn leg).
        let def = wlez_core::get_wlez_definition_id(&wlez_program);
        let acc = self.read_account(def)?;
        if acc.program_owner != programs::token().id() {
            return Err(
                "WLEZ definition is owned by a non-canonical token program (possible unbacked-mint drain); refusing to wrap/unwrap"
                    .into(),
            );
        }
        // 2. The native program id recorded in the vault (Wrap pins its escrow
        //    leg to it) must be the canonical authenticated-transfer program; a
        //    no-op here lets Wrap skip the real escrow and mint unbacked WLEZ.
        let vault = wlez_core::get_wlez_vault_id(&wlez_program);
        let vacc = self.read_account(vault)?;
        let pinned_native = wlez_core::decode_program_id(vacc.data.as_ref()).ok_or(
            "WLEZ vault is missing its pinned native program id; refusing to wrap/unwrap",
        )?;
        if pinned_native != programs::authenticated_transfer().id() {
            return Err(
                "WLEZ vault pins a non-canonical native program (possible unbacked-mint drain); refusing to wrap/unwrap"
                    .into(),
            );
        }
        Ok(())
    }

    /// Wrap `amount` native LEZ → WLEZ: lock LEZ from `user_native` (signs) into
    /// the vault, mint WLEZ into `user_holding` (an initialised WLEZ holding).
    pub fn wrap(
        &self,
        wlez_program: ProgramId,
        amount: u128,
        user_native: AccountId,
        user_holding: AccountId,
    ) -> Result<TxHash> {
        self.wlez_programs_checked(wlez_program)?;
        let vault = wlez_core::get_wlez_vault_id(&wlez_program);
        let def = wlez_core::get_wlez_definition_id(&wlez_program);
        self.report("wrapping LEZ → WLEZ");
        self.submit_public(
            wlez_program,
            vec![user_native, vault, def, user_holding],
            &[user_native],
            wlez_core::Instruction::Wrap { amount },
        )
    }

    /// Unwrap `amount` WLEZ → native LEZ: burn WLEZ from `user_holding` (signs),
    /// release LEZ from the vault to `user_native`.
    pub fn unwrap(
        &self,
        wlez_program: ProgramId,
        amount: u128,
        user_holding: AccountId,
        user_native: AccountId,
    ) -> Result<TxHash> {
        self.wlez_programs_checked(wlez_program)?;
        let vault = wlez_core::get_wlez_vault_id(&wlez_program);
        let def = wlez_core::get_wlez_definition_id(&wlez_program);
        self.report("unwrapping WLEZ → LEZ");
        self.submit_public(
            wlez_program,
            vec![user_holding, def, vault, user_native],
            &[user_holding],
            wlez_core::Instruction::Unwrap { amount },
        )
    }

    // ---- Associated Token Accounts (RFP Func: ATAs for all token ops) ----

    /// Derive the Associated Token Account id for `(owner, definition)` under the
    /// given ATA program. Deterministic - matches the on-chain `ata_core`.
    #[must_use]
    pub fn ata_id(&self, ata_program: ProgramId, owner: AccountId, definition: AccountId) -> AccountId {
        let seed = ata_core::compute_ata_seed(owner, definition);
        ata_core::get_associated_token_account_id(&ata_program, &seed)
    }

    /// Whether `id` is an initialized fungible token holding (so an ATA already exists).
    fn holding_exists(&self, id: AccountId) -> bool {
        self.read_account(id)
            .map(|a| token_core::TokenHolding::try_from(&a.data).is_ok())
            .unwrap_or(false)
    }

    /// A stable wallet-owned, signing-capable public account to use as the ATA
    /// owner when the caller does not pass one (smallest id, for determinism).
    pub fn default_owner(&self) -> Result<AccountId> {
        self.wallet_public_accounts()
            .into_iter()
            .filter(|id| self.wallet.get_account_public_signing_key(*id).is_some())
            .min_by_key(|id| id.to_bytes())
            .ok_or_else(|| "wallet has no signing-capable public account to use as ATA owner".into())
    }

    /// Create the ATA for `(owner, definition)` if it does not already exist
    /// (idempotent; `ata::Create` is itself a no-op on-chain when present, but we
    /// skip the tx when we can see it already exists). Returns the ATA id.
    pub fn create_ata(&self, ata_program: ProgramId, owner: AccountId, definition: AccountId) -> Result<AccountId> {
        let ata = self.ata_id(ata_program, owner, definition);
        if self.holding_exists(ata) {
            return Ok(ata);
        }
        self.report("creating associated token account");
        self.submit_public(
            ata_program,
            vec![owner, definition, ata],
            &[owner],
            // v0.2.1 made every ata_core instruction carry the ATA program id.
            ata_core::Instruction::Create {
                ata_program_id: ata_program,
            },
        )?;
        Ok(ata)
    }

    /// Fund the `(owner, definition)` ATA with `amount`, transferred from a wallet
    /// keypair `from_holding` (creating the ATA first if absent). Convenience for
    /// seeding a buyer's collateral ATA before an ATA buy. Returns the ATA id.
    pub fn fund_ata(
        &self,
        ata_program: ProgramId,
        owner: AccountId,
        definition: AccountId,
        from_holding: AccountId,
        amount: u128,
    ) -> Result<AccountId> {
        let ata = self.create_ata(ata_program, owner, definition)?;
        // An ATA is an ordinary fungible token holding at the token layer, so the
        // source keypair holding pays into it with a plain token::Transfer.
        self.report("funding associated token account");
        self.submit_public(
            programs::token().id(),
            vec![from_holding, ata],
            &[from_holding],
            token_core::Instruction::Transfer { amount_to_transfer: amount },
        )?;
        Ok(ata)
    }

    /// `BuyAta` - bonding-curve buy with the buyer side using ATAs. Ensures the
    /// buyer's collateral + token ATAs exist, then submits. The collateral ATA
    /// must already hold `collateral_in` (use [`fund_ata`]).
    pub fn bc_buy_ata(
        &self,
        program: ProgramId,
        ata_program: ProgramId,
        sale: AccountId,
        owner: AccountId,
        collateral_in: u128,
        min_tokens_out: u128,
    ) -> Result<TxHash> {
        let s = self.bc_sale_checked(sale)?;
        let collateral_ata = self.create_ata(ata_program, owner, s.collateral_definition_id)?;
        let token_ata = self.create_ata(ata_program, owner, s.token_definition_id)?;
        self.submit_public(
            program,
            vec![
                sale,
                s.token_vault_id,
                s.collateral_vault_id,
                s.treasury_id,
                owner,
                collateral_ata,
                token_ata,
                bonding_curve_core::CLOCK_01,
            ],
            &[owner],
            bonding_curve_core::Instruction::BuyAta {
                collateral_in,
                min_tokens_out,
                ata_program_id: ata_program,
                deadline: self.tx_deadline()?,
            },
        )
    }

    /// `SellAta` - bonding-curve sell with the seller side using ATAs. The
    /// seller's token ATA must hold `tokens_in`.
    pub fn bc_sell_ata(
        &self,
        program: ProgramId,
        ata_program: ProgramId,
        sale: AccountId,
        owner: AccountId,
        tokens_in: u128,
        min_collateral_out: u128,
    ) -> Result<TxHash> {
        let s = self.bc_sale_checked(sale)?;
        let token_ata = self.create_ata(ata_program, owner, s.token_definition_id)?;
        let collateral_ata = self.create_ata(ata_program, owner, s.collateral_definition_id)?;
        self.submit_public(
            program,
            vec![
                sale,
                s.token_vault_id,
                s.collateral_vault_id,
                s.treasury_id,
                owner,
                token_ata,
                collateral_ata,
                bonding_curve_core::CLOCK_01,
            ],
            &[owner],
            bonding_curve_core::Instruction::SellAta {
                tokens_in,
                min_collateral_out,
                ata_program_id: ata_program,
                deadline: self.tx_deadline()?,
            },
        )
    }

    /// `BuyAta` - LBP buy with the buyer side using ATAs. The collateral ATA must
    /// already hold `collateral_in` (use [`fund_ata`]).
    pub fn lbp_buy_ata(
        &self,
        program: ProgramId,
        ata_program: ProgramId,
        pool: AccountId,
        owner: AccountId,
        collateral_in: u128,
        min_tokens_out: u128,
    ) -> Result<TxHash> {
        let p = self.lbp_pool_checked(pool)?;
        let collateral_ata = self.create_ata(ata_program, owner, p.collateral_definition_id)?;
        let token_ata = self.create_ata(ata_program, owner, p.token_definition_id)?;
        self.submit_public(
            program,
            vec![
                pool,
                p.token_vault_id,
                p.collateral_vault_id,
                owner,
                collateral_ata,
                token_ata,
                lbp_core::CLOCK_01,
            ],
            &[owner],
            lbp_core::Instruction::BuyAta {
                collateral_in,
                min_tokens_out,
                ata_program_id: ata_program,
                deadline: self.tx_deadline()?,
            },
        )
    }

    // ---- shield / deshield (public token <-> shielded) -------------------

    /// Create a fresh shielded (private) holding account - a target for `shield`.
    pub fn new_private_account(&mut self) -> Result<AccountId> {
        let (id, _) = self.wallet.create_new_account_private(None);
        self.persist()?;
        Ok(id)
    }

    /// Shield `amount` from a public token holding into a shielded holding the
    /// wallet owns (`private_holding`). One privacy STARK.
    pub fn shield(&mut self, public_holding: AccountId, private_holding: AccountId, amount: u128) -> Result<TxHash> {
        self.sync_private()?;
        self.privacy_token_transfer(
            AccountIdentity::Public(public_holding),
            AccountIdentity::PrivateOwned(private_holding),
            private_holding,
            amount,
        )
    }

    /// Deshield `amount` from a shielded holding into a public token holding.
    pub fn deshield(&mut self, private_holding: AccountId, public_holding: AccountId, amount: u128) -> Result<TxHash> {
        self.sync_private()?;
        self.privacy_token_transfer(
            AccountIdentity::PrivateOwned(private_holding),
            AccountIdentity::Public(public_holding),
            public_holding,
            amount,
        )
    }

    // ---- token launch: mint-with-metadata + one-shot create-token-sale ----

    /// Persist the wallet (key-tree index + new account keys + decoded notes) so
    /// freshly-created public accounts survive across CLI processes; otherwise a
    /// later process re-derives the same ids and collides ("already initialized").
    fn persist(&mut self) -> Result<()> {
        // No longer async in v0.2.1, so no runtime needed. It prints to stdout,
        // which must not corrupt `--json` output.
        let _silence = gag::Gag::stdout().ok();
        self.wallet
            .store_persistent_data()
            .map_err(|e| format!("persist wallet: {e}"))
    }

    /// Initialise a fresh public token holding for `definition`. Returns its id.
    pub fn init_token_holding(&mut self, definition: AccountId) -> Result<AccountId> {
        let (holding, _) = self.wallet.create_new_account_public(None);
        self.persist()?;
        self.submit_public(
            programs::token().id(),
            vec![definition, holding],
            &[holding],
            token_core::Instruction::InitializeAccount,
        )?;
        Ok(holding)
    }

    /// Mint a new fungible token with on-chain metadata - a self-contained
    /// `data:application/json,{name,symbol}` URI (no external hosting). Returns
    /// `(definition_id, supply_holding_id)`; the holding receives `total_supply`.
    pub fn mint_token_with_metadata(
        &mut self,
        name: &str,
        symbol: &str,
        total_supply: u128,
    ) -> Result<(AccountId, AccountId)> {
        let (definition, _) = self.wallet.create_new_account_public(None);
        let (holding, _) = self.wallet.create_new_account_public(None);
        let (metadata, _) = self.wallet.create_new_account_public(None);
        self.persist()?;
        let body = serde_json::json!({ "name": name, "symbol": symbol }).to_string();
        let uri = format!("data:application/json,{body}");
        self.report("minting token + metadata");
        self.submit_public(
            programs::token().id(),
            vec![definition, holding, metadata],
            &[definition, holding, metadata],
            token_core::Instruction::NewDefinitionWithMetadata {
                new_definition: token_core::NewTokenDefinition::Fungible {
                    name: name.to_owned(),
                    total_supply,
                },
                metadata: Box::new(token_core::NewTokenMetadata {
                    standard: token_core::MetadataStandard::Simple,
                    uri,
                    creators: String::new(),
                }),
            },
        )?;
        Ok((definition, holding))
    }

    /// Resolve a token definition's `(name, symbol)` from its metadata account's
    /// `data:` URI, if present. Used by sale/pool info display.
    #[must_use]
    pub fn token_metadata(&self, definition: AccountId) -> Option<(String, String)> {
        let def = self.read_account(definition).ok()?;
        let meta_id = match token_core::TokenDefinition::try_from(&def.data) {
            Ok(token_core::TokenDefinition::Fungible { metadata_id: Some(m), .. }) => m,
            _ => return None,
        };
        let meta = self.read_account(meta_id).ok()?;
        let uri = token_core::TokenMetadata::try_from(&meta.data).ok()?.uri;
        parse_name_symbol(&uri)
    }

    /// One-shot launch: mint a project token (name+symbol metadata) and open a
    /// bonding-curve sale raising in **native LEZ** (collateral = WLEZ). wlez is
    /// initialised idempotently. Returns `(sale_id, token_definition_id, tx)`.
    pub fn bc_create_token_sale(&mut self, t: TokenSaleArgs) -> Result<(AccountId, AccountId, TxHash)> {
        let (proj_def, proj_holding) =
            self.mint_token_with_metadata(&t.name, &t.symbol, t.total_supply)?;
        self.report("initializing wlez (idempotent)");
        self.initialize_wlez(t.wlez_program, proj_def, t.creator)?;
        let wlez_def = wlez_core::get_wlez_definition_id(&t.wlez_program);
        let treasury = self.init_token_holding(wlez_def)?;
        let (sale, tx) = self.bc_create_sale(BcCreateArgs {
            program: t.bc_program,
            collateral_def: wlez_def,
            treasury,
            creator_token_holding: proj_holding,
            creator: t.creator,
            sale_quantity: t.sale_quantity,
            dex_seed: t.dex_seed,
            virt_token: t.vt,
            virt_collateral: t.vc,
            fee_bps: t.fee_bps,
            one_directional: false,
            end_timestamp_ms: 0,
            min_duration_ms: 0,
            nonce: t.nonce,
        })?;
        Ok((sale, proj_def, tx))
    }

    /// Private launch: like [`bc_create_token_sale`] but the on-chain `creator`
    /// is a fresh, unlinkable account A funded by deshielding the deposit from a
    /// shielded holding (mint → shield deposit → deshield into A → create-sale).
    /// An observer can't tie the sale to the real minter. Returns `(sale, def, tx)`.
    pub fn bc_create_token_sale_private(&mut self, t: TokenSaleArgs) -> Result<(AccountId, AccountId, TxHash)> {
        let deposit = t
            .sale_quantity
            .checked_add(t.dex_seed)
            .ok_or_else(|| "sale_quantity + dex_seed overflows u128".to_string())?;
        let (proj_def, proj_holding) =
            self.mint_token_with_metadata(&t.name, &t.symbol, t.total_supply)?;
        // Shield the deposit so the funding source is private.
        self.report("shielding deposit");
        let shielded = self.new_private_account()?;
        self.shield(proj_holding, shielded, deposit)?;
        // wlez collateral + treasury.
        self.initialize_wlez(t.wlez_program, proj_def, t.creator)?;
        let wlez_def = wlez_core::get_wlez_definition_id(&t.wlez_program);
        let treasury = self.init_token_holding(wlez_def)?;
        // Fresh unlinkable creator A + token holding A; deshield the deposit into A_token.
        let (a_creator, _) = self.wallet.create_new_account_public(None);
        let (a_token, _) = self.wallet.create_new_account_public(None);
        self.persist()?;
        self.report("deshielding deposit into a fresh creator");
        self.deshield(shielded, a_token, deposit)?;
        let (sale, tx) = self.bc_create_sale(BcCreateArgs {
            program: t.bc_program,
            collateral_def: wlez_def,
            treasury,
            creator_token_holding: a_token,
            creator: a_creator,
            sale_quantity: t.sale_quantity,
            dex_seed: t.dex_seed,
            virt_token: t.vt,
            virt_collateral: t.vc,
            fee_bps: t.fee_bps,
            one_directional: false,
            end_timestamp_ms: 0,
            min_duration_ms: 0,
            nonce: t.nonce,
        })?;
        Ok((sale, proj_def, tx))
    }

    // ---- bonding curve: creator ------------------------------------------

    /// Create a bonding-curve sale. Returns `(sale_id, tx_hash)`.
    pub fn bc_create_sale(&self, a: BcCreateArgs) -> Result<(AccountId, TxHash)> {
        let token_def = self.holding_definition(a.creator_token_holding)?;
        // Mirror the project token's name/symbol onto the sale so it is
        // self-describing on-chain. Prefer the metadata URI (name+symbol), fall
        // back to the definition's name, else empty.
        let (token_name, token_symbol) = self
            .token_metadata(token_def)
            .or_else(|| self.token_name(token_def).map(|n| (n, String::new())))
            .unwrap_or_default();
        let sale = bonding_curve_core::compute_sale_pda(a.program, token_def, a.collateral_def, a.creator, a.nonce);
        let token_vault = bonding_curve_core::compute_token_vault_pda(a.program, sale);
        let collateral_vault = bonding_curve_core::compute_collateral_vault_pda(a.program, sale);
        let instruction = bonding_curve_core::Instruction::CreateSale {
            collateral_definition_id: a.collateral_def,
            treasury_id: a.treasury,
            token_name,
            token_symbol,
            sale_quantity: a.sale_quantity,
            dex_seed_quantity: a.dex_seed,
            virt_token: a.virt_token,
            virt_collateral: a.virt_collateral,
            fee_bps: a.fee_bps,
            one_directional: a.one_directional,
            end_timestamp_ms: a.end_timestamp_ms,
            min_duration_ms: a.min_duration_ms,
            nonce: a.nonce,
            // Pin the canonical ATA program into the sale so BuyAta/SellAta can
            // reject any substitute (no-op-drain) ATA program at trade time.
            ata_program_id: ata_program_id()?,
            deadline: self.tx_deadline()?,
        };
        let accounts = vec![
            sale,
            token_vault,
            collateral_vault,
            a.collateral_def,
            a.creator_token_holding,
            a.creator,
            bonding_curve_core::CLOCK_01,
        ];
        let tx = self.submit_public(a.program, accounts, &[a.creator_token_holding, a.creator], instruction)?;
        Ok((sale, tx))
    }

    pub fn bc_close(&self, program: ProgramId, sale: AccountId, creator: AccountId) -> Result<TxHash> {
        self.submit_public(
            program,
            vec![sale, creator, bonding_curve_core::CLOCK_01],
            &[creator],
            bonding_curve_core::Instruction::CloseSale { deadline: self.tx_deadline()? },
        )
    }

    pub fn bc_withdraw(
        &self,
        program: ProgramId,
        sale: AccountId,
        creator_collateral: AccountId,
        creator_token: AccountId,
        creator: AccountId,
    ) -> Result<TxHash> {
        let s = self.bc_sale(sale)?;
        self.submit_public(
            program,
            vec![sale, s.token_vault_id, s.collateral_vault_id, creator_collateral, creator_token, creator],
            &[creator],
            bonding_curve_core::Instruction::Withdraw { deadline: self.tx_deadline()? },
        )
    }

    // ---- bonding curve: participant --------------------------------------

    pub fn bc_buy(
        &self,
        program: ProgramId,
        sale: AccountId,
        buyer_collateral: AccountId,
        buyer_token: AccountId,
        collateral_in: u128,
        min_tokens_out: u128,
    ) -> Result<TxHash> {
        let s = self.bc_sale_checked(sale)?;
        self.submit_public(
            program,
            vec![sale, s.token_vault_id, s.collateral_vault_id, s.treasury_id, buyer_collateral, buyer_token, bonding_curve_core::CLOCK_01],
            &[buyer_collateral],
            bonding_curve_core::Instruction::Buy { collateral_in, min_tokens_out, deadline: self.tx_deadline()? },
        )
    }

    pub fn bc_sell(
        &self,
        program: ProgramId,
        sale: AccountId,
        seller_token: AccountId,
        seller_collateral: AccountId,
        tokens_in: u128,
        min_collateral_out: u128,
    ) -> Result<TxHash> {
        let s = self.bc_sale_checked(sale)?;
        self.submit_public(
            program,
            vec![sale, s.token_vault_id, s.collateral_vault_id, s.treasury_id, seller_token, seller_collateral, bonding_curve_core::CLOCK_01],
            &[seller_token],
            bonding_curve_core::Instruction::Sell { tokens_in, min_collateral_out, deadline: self.tx_deadline()? },
        )
    }

    /// Private bonding-curve buy: deshield → buy → re-shield, as one atomic
    /// privacy transaction. Generates a fresh single-use account A (never
    /// reused), validates the re-shield target is a shielded account the wallet
    /// owns, and enforces the indivisible deshield (RFP Privacy #1/#3/#4).
    /// Private buy: the drift-free `deshield → public Buy → re-shield` saga (the
    /// only private mode - see [`bc_buy_atomic_disposable`]).
    pub fn bc_buy_private(
        &mut self,
        program: ProgramId,
        sale: AccountId,
        user_collateral: AccountId,
        user_token_out: AccountId,
        collateral_in: u128,
        min_tokens_out: u128,
    ) -> Result<TxHash> {
        self.sync_private()?;
        self.bc_buy_atomic_disposable(
            program, sale, user_collateral, user_token_out, collateral_in, min_tokens_out,
        )
    }

    /// Drift-free 3-tx private buy: deshield → public `Buy` → re-shield. Only the
    /// proofless public buy touches the sale PDA, so it can't drift. No-loss: a
    /// failed buy rolls the deshield back; a failed re-shield leaves the tokens
    /// recoverable in `a_token`.
    fn bc_buy_atomic_disposable(
        &mut self,
        program: ProgramId,
        sale: AccountId,
        user_collateral: AccountId,
        user_token_out: AccountId,
        collateral_in: u128,
        min_tokens_out: u128,
    ) -> Result<TxHash> {
        // Guard the no-op-drain pin BEFORE the deshield burns the privacy proof,
        // so a poisoned sale fails fast and never makes the funds public.
        let s = self.bc_sale_checked(sale)?;
        self.private_disposable_saga(
            DisposableSaga {
                spend_src: user_collateral,
                reshield_target: user_token_out,
                amount: collateral_in,
                op_phase: "buying on the curve",
                text: BUY_COLLATERAL_SAGA,
            },
            // tx2 - public buy (drift-free, min_out). a_collateral pays; the fresh
            // a_token co-signs so its claim holds without a separate init tx.
            |this, a_collateral, a_token| {
                this.submit_public(
                    program,
                    vec![sale, s.token_vault_id, s.collateral_vault_id, s.treasury_id, a_collateral, a_token, bonding_curve_core::CLOCK_01],
                    &[a_collateral, a_token],
                    bonding_curve_core::Instruction::Buy { collateral_in, min_tokens_out, deadline: this.tx_deadline()? },
                )
            },
        )
    }

    /// Private sell: the drift-free `deshield → public Sell → re-shield` saga -
    /// the mirror of [`bc_buy_private`]. Deshield project tokens, sell them
    /// publicly against the live curve (`min_collateral_out`), re-shield the
    /// collateral to a shielded holding.
    pub fn bc_sell_private(
        &mut self,
        program: ProgramId,
        sale: AccountId,
        user_token: AccountId,
        user_collateral_out: AccountId,
        tokens_in: u128,
        min_collateral_out: u128,
    ) -> Result<TxHash> {
        self.sync_private()?;
        self.bc_sell_atomic_disposable(
            program, sale, user_token, user_collateral_out, tokens_in, min_collateral_out,
        )
    }

    /// Drift-free 3-tx private sell: deshield tokens → public `Sell` → re-shield
    /// collateral. Only the proofless public sell touches the sale PDA. No-loss:
    /// a failed sell rolls the deshield back; a failed re-shield leaves the
    /// collateral recoverable in `a_collateral`.
    fn bc_sell_atomic_disposable(
        &mut self,
        program: ProgramId,
        sale: AccountId,
        user_token: AccountId,
        user_collateral_out: AccountId,
        tokens_in: u128,
        min_collateral_out: u128,
    ) -> Result<TxHash> {
        // Guard the no-op-drain pin BEFORE the deshield burns the privacy proof,
        // so a poisoned sale fails fast and never makes the tokens public.
        let s = self.bc_sale_checked(sale)?;
        self.private_disposable_saga(
            DisposableSaga {
                spend_src: user_token,
                reshield_target: user_collateral_out,
                amount: tokens_in,
                op_phase: "selling on the curve",
                text: SELL_TOKENS_SAGA,
            },
            // tx2 - public sell (drift-free, min_out). a_token sells; the fresh
            // a_collateral co-signs so its claim holds without a separate init tx.
            |this, a_token, a_collateral| {
                this.submit_public(
                    program,
                    vec![sale, s.token_vault_id, s.collateral_vault_id, s.treasury_id, a_token, a_collateral, bonding_curve_core::CLOCK_01],
                    &[a_token, a_collateral],
                    bonding_curve_core::Instruction::Sell { tokens_in, min_collateral_out, deadline: this.tx_deadline()? },
                )
            },
        )
    }

    // ---- LBP: creator ----------------------------------------------------

    /// Create an LBP sale. Returns `(pool_id, tx_hash)`.
    pub fn lbp_create_sale(&self, a: LbpCreateArgs) -> Result<(AccountId, TxHash)> {
        let token_def = self.holding_definition(a.creator_token_holding)?;
        // Mirror the project token's name/symbol onto the pool (self-describing
        // on-chain): metadata URI first, then the definition name, else empty.
        let (token_name, token_symbol) = self
            .token_metadata(token_def)
            .or_else(|| self.token_name(token_def).map(|n| (n, String::new())))
            .unwrap_or_default();
        let pool = lbp_core::compute_pool_pda(a.program, token_def, a.collateral_def, a.creator, a.nonce);
        let token_vault = lbp_core::compute_token_vault_pda(a.program, pool);
        let collateral_vault = lbp_core::compute_collateral_vault_pda(a.program, pool);
        let instruction = lbp_core::Instruction::CreateSale {
            collateral_definition_id: a.collateral_def,
            treasury_id: a.treasury,
            token_name,
            token_symbol,
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
            // Pin the canonical ATA program so BuyAta can reject a substitute.
            ata_program_id: ata_program_id()?,
            deadline: self.tx_deadline()?,
        };
        let accounts = vec![
            pool, token_vault, collateral_vault,
            a.creator_token_holding, a.creator_collateral_holding, a.creator,
            lbp_core::CLOCK_01,
        ];
        let tx = self.submit_public(
            a.program,
            accounts,
            &[a.creator_token_holding, a.creator_collateral_holding, a.creator],
            instruction,
        )?;
        Ok((pool, tx))
    }

    pub fn lbp_pause(&self, program: ProgramId, pool: AccountId, creator: AccountId) -> Result<TxHash> {
        self.submit_public(program, vec![pool, creator], &[creator], lbp_core::Instruction::Pause { deadline: self.tx_deadline()? })
    }
    pub fn lbp_resume(&self, program: ProgramId, pool: AccountId, creator: AccountId) -> Result<TxHash> {
        self.submit_public(program, vec![pool, creator], &[creator], lbp_core::Instruction::Resume { deadline: self.tx_deadline()? })
    }
    pub fn lbp_poke(&self, program: ProgramId, pool: AccountId) -> Result<TxHash> {
        self.submit_public(program, vec![pool, lbp_core::CLOCK_01], &[], lbp_core::Instruction::Poke { deadline: self.tx_deadline()? })
    }
    pub fn lbp_close(&self, program: ProgramId, pool: AccountId, creator: AccountId) -> Result<TxHash> {
        self.submit_public(program, vec![pool, creator, lbp_core::CLOCK_01], &[creator], lbp_core::Instruction::CloseSale { deadline: self.tx_deadline()? })
    }
    pub fn lbp_withdraw(
        &self,
        program: ProgramId,
        pool: AccountId,
        creator_collateral: AccountId,
        creator_token: AccountId,
        creator: AccountId,
    ) -> Result<TxHash> {
        let p = self.lbp_pool(pool)?;
        self.submit_public(
            program,
            vec![pool, p.token_vault_id, p.collateral_vault_id, p.treasury_id, creator_collateral, creator_token, creator],
            &[creator],
            lbp_core::Instruction::Withdraw { deadline: self.tx_deadline()? },
        )
    }

    // ---- LBP: participant ------------------------------------------------

    pub fn lbp_buy(
        &self,
        program: ProgramId,
        pool: AccountId,
        buyer_collateral: AccountId,
        buyer_token: AccountId,
        collateral_in: u128,
        min_tokens_out: u128,
    ) -> Result<TxHash> {
        let p = self.lbp_pool_checked(pool)?;
        self.submit_public(
            program,
            vec![pool, p.token_vault_id, p.collateral_vault_id, buyer_collateral, buyer_token, lbp_core::CLOCK_01],
            &[buyer_collateral],
            lbp_core::Instruction::Buy { collateral_in, min_tokens_out, deadline: self.tx_deadline()? },
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn lbp_buy_gated(
        &self,
        program: ProgramId,
        pool: AccountId,
        buyer_collateral: AccountId,
        buyer_token: AccountId,
        collateral_in: u128,
        min_tokens_out: u128,
        proof: Vec<[u8; 32]>,
    ) -> Result<TxHash> {
        let p = self.lbp_pool_checked(pool)?;
        // Mirror the on-chain bound: reject an over-long proof locally rather than
        // burn proving cost on a tx the guest will reject anyway.
        if proof.len() > lbp_core::MAX_ALLOWLIST_PROOF_DEPTH {
            return Err("allowlist proof exceeds maximum depth".into());
        }
        // Derive the leaf from buyer_collateral - the exact account BuyGated binds
        // against on-chain - so it can never be mis-bound to the wrong account.
        let leaf = lbp_core::allowlist_leaf(&buyer_collateral);
        self.submit_public(
            program,
            vec![pool, p.token_vault_id, p.collateral_vault_id, buyer_collateral, buyer_token, lbp_core::CLOCK_01],
            &[buyer_collateral],
            lbp_core::Instruction::BuyGated { collateral_in, min_tokens_out, leaf, proof, deadline: self.tx_deadline()? },
        )
    }

    /// Private LBP buy (disposable). One atomic privacy action: the drift-free
    /// `deshield → public Buy → re-shield` saga (the only private mode; the
    /// public buy leg prices off the live on-chain clock - there is no t_buy_ms).
    pub fn lbp_buy_private(
        &mut self,
        program: ProgramId,
        pool: AccountId,
        user_collateral: AccountId,
        user_token_out: AccountId,
        collateral_in: u128,
        min_tokens_out: u128,
    ) -> Result<TxHash> {
        self.sync_private()?;
        self.lbp_buy_atomic_disposable(
            program, pool, user_collateral, user_token_out, collateral_in, min_tokens_out,
        )
    }

    /// Drift-free 3-tx LBP private buy (`PrivateMode::AtomicDisposable`):
    /// deshield collateral → public `Buy` (live-clock priced, `min_tokens_out`)
    /// → re-shield tokens. Same no-loss recovery as the bonding-curve path.
    fn lbp_buy_atomic_disposable(
        &mut self,
        program: ProgramId,
        pool: AccountId,
        user_collateral: AccountId,
        user_token_out: AccountId,
        collateral_in: u128,
        min_tokens_out: u128,
    ) -> Result<TxHash> {
        // Guard the no-op-drain pin BEFORE the deshield burns the privacy proof,
        // so a poisoned pool fails fast and never makes the funds public.
        let p = self.lbp_pool_checked(pool)?;
        self.private_disposable_saga(
            DisposableSaga {
                spend_src: user_collateral,
                reshield_target: user_token_out,
                amount: collateral_in,
                op_phase: "buying on the pool",
                text: BUY_COLLATERAL_SAGA,
            },
            // tx2 - PUBLIC buy a_collateral → a_token (drift-free, live clock).
            // a_collateral pays AND the fresh a_token co-signs (self-claims).
            |this, a_collateral, a_token| {
                this.submit_public(
                    program,
                    vec![pool, p.token_vault_id, p.collateral_vault_id, a_collateral, a_token, lbp_core::CLOCK_01],
                    &[a_collateral, a_token],
                    lbp_core::Instruction::Buy { collateral_in, min_tokens_out, deadline: this.tx_deadline()? },
                )
            },
        )
    }

    // ---- private helpers --------------------------------------------------

    /// Shared shape of the three `AtomicDisposable` private sagas (bc buy/sell,
    /// lbp buy): deshield `spend_src` → run the program-specific public leg →
    /// re-shield the realized output to `reshield_target`, with the identical
    /// no-loss rollback. `build_public` supplies only the program-specific public
    /// submit, receiving `(a_in, a_out)`: `a_in` is the public account the
    /// deshield funds (it pays/sells), `a_out` the fresh account the public leg
    /// pays into (re-shielded on success). Callers guard the no-op-drain pin
    /// (`*_checked`) before calling, so a poisoned sale/pool never deshields.
    fn private_disposable_saga(
        &mut self,
        saga: DisposableSaga,
        build_public: impl FnOnce(&Self, AccountId, AccountId) -> Result<TxHash>,
    ) -> Result<TxHash> {
        let DisposableSaga { spend_src, reshield_target, amount, op_phase, text } = saga;
        // Privacy #3: re-shield target must be a shielded account we own.
        if self.wallet.get_account_private(reshield_target).is_none() {
            return Err("re-shield target must be a private (shielded) account in this wallet (RFP Privacy #3)".into());
        }
        // Fail fast before the multi-minute deshield proof if funds are short.
        self.ensure_shielded_covers(spend_src, amount)?;
        // Privacy #4: fresh single-use account A holdings (never reused).
        let (a_in, _) = self.wallet.create_new_account_public(None);
        let (a_out, _) = self.wallet.create_new_account_public(None);
        // Durably record the fresh slots BEFORE the deshield funds a_in (mirrors
        // bc_create_token_sale_private): if tx1 lands but the process dies before
        // privacy_token_transfer's persist, a_in is still in the wallet store - so
        // it stays visible in `my-balance` for the documented re-run recovery and
        // find_next_slot_layered never re-derives it, keeping A single-use.
        self.persist()?;

        // tx1 - DESHIELD the spend asset into the public A holding (privacy proof, no pool).
        self.report(text.deshield_phase);
        self.privacy_token_transfer(
            AccountIdentity::PrivateOwned(spend_src),
            AccountIdentity::Public(a_in),
            spend_src,
            amount,
        )?;

        // tx2 - the program-specific public leg (drift-free, min_out).
        self.report(op_phase);
        if let Err(e) = build_public(self, a_in, a_out) {
            // The public leg returned Err, but `poll_tx` can time out *after* the
            // tx landed on-chain. If so, a_in's deshielded funds were consumed by
            // the (actually-executed) public leg and the realized output sits in
            // a_out - rolling back a_in would over-draw an empty account. Detect
            // the landed-but-reported-failed leg and point the user at a_out.
            if self.holding_balance(a_in).unwrap_or(0) < amount {
                let realized = self.holding_balance(a_out).unwrap_or(0);
                return Err(format!(
                    "{} was reported failed but the deshielded {} ({amount}) is gone from the ephemeral \
                     account A {a_in}, so the public leg likely landed on-chain despite the error. The \
                     realized output ({realized}) is SAFE in the wallet-owned ephemeral account {a_out} \
                     (visible in `lpad my-balance` after `lpad sync`); re-shield it into your private \
                     holding to recover. No funds were lost. {} error: {e}",
                    text.op_failed, text.spend_noun, text.op_capitalized,
                ));
            }
            // ROLLBACK: re-shield the deshielded asset back to the user
            // (retry+sync, like the success path) - only claim it reached the
            // private holding if the re-shield actually returned Ok.
            return Err(match self.reshield_with_retry(a_in, spend_src, amount) {
                Ok(_) => format!("{} (deshielded {} rolled back to private): {e}", text.op_failed, text.spend_noun),
                Err(re) => format!(
                    "{} AND rollback re-shield failed: {amount} {} {} SAFE but \
                     PUBLICLY held in the wallet-owned ephemeral account A (visible in `lpad my-balance`), \
                     recoverable by re-shielding {} into your private holding; no funds were lost. \
                     {} error: {e}. Rollback error: {re}",
                    text.op_failed, text.spend_noun, text.is_are, text.it_them, text.op_capitalized,
                ),
            });
        }

        // Read the realized output now sitting in the public a_out.
        let received = self.holding_balance(a_out)?;
        if received == 0 {
            return Err(text.no_output.into());
        }

        // tx3 - RE-SHIELD the realized output to the user's private holding (privacy proof, no pool).
        self.report(text.reshield_phase);
        self.reshield_with_retry(a_out, reshield_target, received)
    }

    fn holding_definition(&self, holding: AccountId) -> Result<AccountId> {
        let acc = self.read_account(holding)?;
        match token_core::TokenHolding::try_from(&acc.data) {
            Ok(token_core::TokenHolding::Fungible { definition_id, .. }) => Ok(definition_id),
            _ => Err("account is not a fungible token holding".into()),
        }
    }

    /// One token privacy transfer (a single STARK): move `amount` across the
    /// privacy boundary (`from`/`to` set the visibility). `decode_into` is the
    /// wallet-owned account whose result is folded into the local cache so later
    /// reads are fresh. The deshield/re-shield legs of `AtomicDisposable`.
    fn privacy_token_transfer(
        &mut self,
        from: AccountIdentity,
        to: AccountIdentity,
        decode_into: AccountId,
        amount: u128,
    ) -> Result<TxHash> {
        let data = Program::serialize_instruction(token_core::Instruction::Transfer {
            amount_to_transfer: amount,
        })
        .map_err(|e| format!("serialize transfer: {e:?}"))?;
        let token_prog = ProgramWithDependencies::new(programs::token(), HashMap::new());
        let rt = &self.rt;
        let wallet = &mut self.wallet;
        // The wallet prints the decoded account and the persist path to stdout.
        let _silence = gag::Gag::stdout().ok();
        rt.block_on(async move {
            let (hash, secrets) = wallet
                .send_privacy_preserving_tx(vec![from, to], data, &token_prog)
                .await
                .map_err(|e| format!("privacy transfer failed: {e:?}"))?;
            let (tx, _block_id) = wallet
                .poll_transaction(hash)
                .await
                .map_err(|e| format!("tx not included: {e}"))?;
            if let LeeTransaction::PrivacyPreserving(ppt) = tx
                && let Some(secret) = secrets.into_iter().next()
            {
                // Keep this mask at exactly one entry. v0.2.1 no longer treats the
                // mask index as the note index - it locates the note by nullifier -
                // which is what makes a 1-entry mask correct even though the circuit
                // now pads the private inputs up to 7 notes.
                wallet
                    .decode_insert_privacy_preserving_transaction_results(
                        &ppt,
                        &[AccDecodeData::Decode(secret, decode_into)],
                    )
                    .map_err(|e| format!("decode results: {e:?}"))?;
                wallet
                    .store_persistent_data()
                    .map_err(|e| format!("persist wallet: {e}"))?;
            }
            Ok(to_hash(hash))
        })
    }

    /// Current fungible balance of a public holding (0 if absent/non-fungible).
    fn holding_balance(&self, holding: AccountId) -> Result<u128> {
        let acc = self.read_account(holding)?;
        Ok(match token_core::TokenHolding::try_from(&acc.data) {
            Ok(token_core::TokenHolding::Fungible { balance, .. }) => balance,
            _ => 0,
        })
    }
}

fn to_hash(h: common::HashType) -> TxHash {
    let mut out = [0u8; 32];
    let b: &[u8] = h.as_ref();
    if b.len() == 32 {
        out.copy_from_slice(b);
    }
    out
}

#[cfg(test)]
mod chain_parity {
    //! Guards that the LEZ pin still matches the networks lpad targets.
    //!
    //! A program is addressed by its RISC0 image id, so lpad and the chain must
    //! agree on the built-in ids or nothing works: a token transfer would be
    //! dispatched to a program that is not deployed, and shielded proofs would be
    //! rejected by a different privacy circuit.
    //!
    //! These ids were read off BOTH live sequencers (`getProgramIds`, plus the
    //! clock account's `program_owner`, which is an independent check because the
    //! clock account id is a fixed literal rather than an image id).
    //!
    //! Current values correspond to LEZ v0.2.4. The guest ELFs are byte-identical
    //! across v0.2.2, v0.2.3 and v0.2.4, so any of those tags satisfies these -
    //! which is why v0.2.4 is safe to pin even though the RPC cannot tell us which
    //! of the three the operators actually run.
    //!
    //! If one of these fails after a LEZ version bump, the new version is NOT
    //! deployed on these networks yet - do not ship it.

    /// `token` on testnet.lez.logos.co and seq-testnet.paradox.computer.
    const DEPLOYED_TOKEN: [u32; 8] = [
        1047643340, 4291649067, 2093396023, 4016657193, 3904308476, 481382041, 2987082047,
        2603530278,
    ];
    /// `authenticated_transfer` on both networks.
    const DEPLOYED_AUTH_TRANSFER: [u32; 8] = [
        583309054, 2344528779, 3806558405, 2890696795, 2257354672, 3978764116, 2273929063,
        1518858078,
    ];
    /// `clock` on both networks - cross-checked against the on-chain clock
    /// account's `program_owner`.
    const DEPLOYED_CLOCK: [u32; 8] = [
        96247601, 2082502477, 822865082, 1048693993, 3544189898, 772921104, 1694408900, 4234239033,
    ];

    /// lpad's OWN three programs, as this workspace resolves them.
    ///
    /// `programs/src/lib.rs::pinned_ids_match_artifacts` asserts the same thing,
    /// but it runs in the `programs` workspace. The id that actually reaches a
    /// sequencer is the one the CLI embeds, and the CLI is built here - a
    /// separate workspace with a separate lockfile, which is exactly where a
    /// divergence would hide. Cheap to assert in both places; catastrophic to
    /// discover on chain, where a wrong id is not an error but a transaction
    /// that never lands.
    #[test]
    fn lpad_program_ids_match_the_deployed_constants() {
        assert_eq!(
            lpad_guests::bonding_curve().id(),
            lpad_guests::deployed::BONDING_CURVE,
            "bonding_curve.bin hashes differently in this workspace than in programs/"
        );
        assert_eq!(
            lpad_guests::lbp().id(),
            lpad_guests::deployed::LBP,
            "lbp.bin hashes differently in this workspace than in programs/"
        );
        assert_eq!(
            lpad_guests::wlez().id(),
            lpad_guests::deployed::WLEZ,
            "wlez.bin hashes differently in this workspace than in programs/"
        );
    }

    #[test]
    fn builtin_program_ids_match_the_deployed_networks() {
        assert_eq!(
            programs::token().id(),
            DEPLOYED_TOKEN,
            "the pinned LEZ version's token program is not the one deployed on the target networks"
        );
        assert_eq!(
            programs::authenticated_transfer().id(),
            DEPLOYED_AUTH_TRANSFER,
            "authenticated_transfer mismatch - wlez wrap's native leg would fail on chain"
        );
        assert_eq!(
            programs::clock().id(),
            DEPLOYED_CLOCK,
            "clock mismatch - the clock-pinned buy/sell paths would fail on chain"
        );
    }
}
