//! # lpad-sdk - SDK for the LPAD launchpad (bonding curve = RFP-015, LBP = RFP-016).
//!
//! Full lifecycle for participants (discover, quote, buy/sell, query position)
//! and creators (create, pause/resume, close, withdraw), plus the permissionless
//! protocol-fee settlement anyone may send on either program
//! ([`LaunchpadClient::bc_sweep_treasury`],
//! [`LaunchpadClient::lbp_sweep_treasury`]), over a public account
//! or either of the two private paths: the drift-free `deshield → public buy →
//! re-shield` saga ([`LaunchpadClient::bc_buy_private`]), and the single-
//! transaction `BuyDisposable` ([`LaunchpadClient::bc_buy_disposable`]), whose
//! buyer-side holdings are private account slots inside one proof. The CLI and
//! mini-app FFI are thin layers over this crate.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use common::transaction::LeeTransaction;
use lee::{
    privacy_preserving_transaction::circuit::ProgramWithDependencies,
    public_transaction::{Message, WitnessSet},
    AccountId, PublicTransaction,
};
use lee_core::{
    account::{Account, Data},
    program::{ProgramId, DEFAULT_PROGRAM_ID},
};
use sequencer_service_rpc::RpcClient as _;
use serde::{Deserialize, Serialize};
use wallet::{AccDecodeData, AccountIdentity, WalletCore};

pub use bonding_curve_core::{SaleState, SaleStatus as BcStatus};
/// Re-exported so a caller can name what [`lpad_programs`] hands back - a
/// program is its committed ELF plus the image id of exactly those bytes.
pub use lee::program::Program;
pub use lbp_core::{PoolState, SaleStatus as LbpStatus};

/// The pinata faucet: native LEZ for an empty wallet, proof-of-work mined here.
mod faucet;
pub use faucet::{
    faucet_max_attempts, pinata_account_id, sha256, FaucetChallenge, FaucetClaim, FaucetState,
    DEFAULT_FAUCET_MAX_ATTEMPTS, FAUCET_PRIZE,
};

pub type Result<T> = std::result::Result<T, String>;
pub type TxHash = [u8; 32];
/// Progress/phase sink: called with a short label before each long step.
pub type ProgressFn = Box<dyn Fn(&str)>;

/// Accounts/amount of one `AtomicDisposable` private saga (see
/// [`LaunchpadClient::private_disposable_saga`]). `spend_src` is the shielded
/// account that pays/sells; `reshield_target` the shielded account the realized
/// output lands in.
struct DisposableSaga {
    spend_src: AccountId,
    reshield_target: AccountId,
    amount: u128,
    /// Token definition `spend_src` must hold. Threaded in per-caller because
    /// only the caller knows which side of the trade it is spending: the sale's
    /// collateral definition for a buy, its token definition for a sell.
    /// Checked before the deshield proof - see
    /// [`LaunchpadClient::ensure_shielded_covers`].
    spend_definition: AccountId,
    /// Progress phase reported just before the public leg ("buying on the
    /// curve" / "selling on the curve" / "buying on the pool"). Per-call because
    /// bc-buy and lbp-buy share `text` but word this leg differently.
    op_phase: &'static str,
    text: SagaText,
}

/// The program-specific wording for one private saga, so the shared shape can
/// emit the exact same progress phases and no-loss error text the three callers
/// used before they were collapsed into
/// [`LaunchpadClient::private_disposable_saga`].
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
    /// The wallet's real, funded account. It is the sale's on-chain creator in
    /// the public launch - and in `bc_create_token_sale_private` it is the one
    /// account that must appear in NO transaction of the launch, where it names
    /// what the deanonymisation tripwire refuses (the throwaway creator A is
    /// minted inside that call).
    pub creator: AccountId,
    pub sale_quantity: u128,
    pub dex_seed: u128,
    pub vt: u128,
    pub vc: u128,
    pub fee_bps: u128,
    pub nonce: u64,
}

/// What a one-shot launch produced. Returned by
/// [`LaunchpadClient::bc_create_token_sale`] and
/// [`LaunchpadClient::bc_create_token_sale_private`].
///
/// The last two fields exist because of the private launch. It mints its own
/// unlinkable creator and its own token holding, and every later lifecycle call
/// (`bc close --creator`, `bc withdraw --creator --creator-token`) names them.
/// Returning only `(sale, definition, tx)` left the caller holding the keys to a
/// sale it could not close or withdraw from: the ids are derived inside this
/// call, appear on chain only as opaque PDAs, and no read path renders a sale's
/// creator back to its owner. They are reported for the public launch too, where
/// they are simply the arguments the caller passed in - one shape for both, so a
/// caller does not have to know which arm it took to know what to record.
pub struct TokenLaunch {
    pub sale: AccountId,
    pub token_definition: AccountId,
    /// The sale's on-chain creator: `TokenSaleArgs::creator` for a public
    /// launch, the freshly minted unlinkable account A for a private one.
    pub creator: AccountId,
    /// The creator's holding of the project token - the deposit source, and the
    /// account `Withdraw` returns the unsold tokens to.
    pub creator_token_holding: AccountId,
    pub tx: TxHash,
}

/// Pull `(name, symbol)` out of a `data:application/json,{...}` metadata URI,
/// decoding JSON escapes so quote/backslash-containing labels round-trip.
fn parse_name_symbol(uri: &str) -> Option<(String, String)> {
    let json = uri.split_once(',').map_or(uri, |(_, j)| j);
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let field = |k: &str| value.get(k)?.as_str().map(str::to_owned);
    Some((field("name")?, field("symbol")?))
}

/// Nonce range covered by discovery's brute-force FALLBACK - the pre-index
/// derivation in [`LaunchpadClient::discover`], not the default path. It bounds
/// a guess, so it is a guess's worth of coverage: a sale created at nonce 9 by
/// an lpad build too old to have written an index is not findable at all. Every
/// sale this build creates is, whatever its nonce, because the chain records
/// it. (Listing OTHER people's sales still needs the LP-0012 indexer.)
const DISCOVERY_NONCE_MAX: u64 = 8;

// ---------------------------------------------------------------------------
// Discovery: pruning the candidate space, and remembering what was found
// ---------------------------------------------------------------------------

/// One row of [`LaunchpadClient::wallet_account_scan`]: `(id, is_private,
/// account)`, where `None` means the read FAILED (unknown), not "absent".
type WalletAccount = (AccountId, bool, Option<Account>);

/// What a discovery listing found, and what it could not read.
///
/// `unreadable` is not an error and not a warning-only condition: each entry is
/// an id the CHAIN's own creator index (or this wallet's append-only discovery
/// cache) named, so the sale EXISTS and this run simply could not read its
/// state. A listing carrying a non-empty `unreadable` is therefore SHORT, and
/// the caller has to be able to tell - `found` alone reads as "these are all
/// your sales", which for a script polling "did my launch land?" is the wrong
/// answer in the one direction that costs money.
///
/// The stderr warning that used to be the only signal stays, for humans. This is
/// the same fact in a form a program can branch on.
pub struct Listing<S> {
    /// The sales/pools whose state was read, in discovery order.
    pub found: Vec<(AccountId, S)>,
    /// Ids the chain says exist and this run could not read, each with the read
    /// error. Non-empty means `found` is incomplete.
    pub unreadable: Vec<(AccountId, String)>,
}

impl<S> Listing<S> {
    /// Whether `found` is known to be short. See the type docs.
    #[must_use]
    pub fn partial(&self) -> bool {
        !self.unreadable.is_empty()
    }
}

/// Knobs for [`LaunchpadClient::my_sales_with`] and
/// [`LaunchpadClient::my_pools_with`]. `Default` is the fast path: read each
/// creator candidate's on-chain index and stop there.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Discovery {
    /// Run the brute-force derivation as well, for every creator. The
    /// `discover` engine behind `my_sales_with`/`my_pools_with` documents the
    /// two paths and which is which.
    ///
    /// It is worth being exact about what this is now FOR, because it used to
    /// be for something else. Both programs record every sale/pool in a
    /// per-creator index account the moment they create it, and the default
    /// path reads exactly those: one read per creator instead of
    /// `creators x defs^2 x nonces` speculative probes. That index is
    /// authoritative for the program id this build carries, so a refresh is no
    /// longer needed for a sale created on another machine holding the same key
    /// or in a wallet home restored from backup - the chain itself now knows
    /// about those, and the plain listing finds them.
    ///
    /// What is left is the one thing no index can hold: a sale created by an
    /// OLDER lpad build, before `create_sale` wrote one. Re-deriving and probing
    /// the candidate space is the only way to find such a sale, and it is the
    /// 40-minute scan this whole change exists to avoid - so it is a flag, not
    /// the default. It is also purely ADDITIVE (it can only lengthen a listing,
    /// never shorten one) and a once-per-wallet cost: whatever it turns up is
    /// remembered next to the wallet and probed by every later run.
    pub refresh: bool,
    /// As [`Self::refresh`], and drop the creator prune: treat every public
    /// wallet account as a possible creator, not just the identity accounts.
    /// See [`creator_ineligible_owners`] for the one case this exists to
    /// recover.
    ///
    /// Note this widens the INDEX reads too, and that half is cheap - one read
    /// per account. A sale created with `--creator <a token holding>` is
    /// therefore usually found by the index alone, long before the derivation
    /// this also triggers gets anywhere.
    pub deep: bool,
}

/// Cache namespaces. They keep bonding-curve ids and LBP ids apart in one file,
/// which matters because the same wallet + definitions derive DIFFERENT PDAs
/// under the two programs.
const DISCOVERY_KIND_BC: &str = "bc";
const DISCOVERY_KIND_LBP: &str = "lbp";

/// The discovery cache, written next to the wallet config - the same directory
/// `WalletPaths::resolve` puts `statistics.json` in and the bootstrap writes the
/// `proof_mode` marker into. Point `--config` (or `$LEE_WALLET_HOME_DIR`) at a
/// different wallet and you get that wallet's cache.
const DISCOVERY_CACHE_FILE: &str = "lpad_discovery.json";

/// Bumped only if the on-disk shape changes; an older/newer file is treated as a
/// miss (rescan), never as an error.
///
/// 2 dropped the `scanned` flag: see [`CacheEntry`]. Nothing is lost by the
/// bump - every v1 entry is keyed by a program id that this build, which
/// carries the indexing guests, no longer has.
const DISCOVERY_CACHE_VERSION: u32 = 2;

/// What is remembered per `(kind, program id)`.
///
/// **Ids, never state.** A sale/pool PDA is a hash of
/// `(token_def, collateral_def, creator, nonce)` under a program id: deterministic
/// and permanent, so an id that resolved once resolves forever and can be cached
/// indefinitely. Its STATE cannot: reserves move on every buy, `status` flips on
/// close, `treasury_owed` accrues. Caching the rendered state would make
/// `my-sales` faster still - zero reads - but it would print numbers that were
/// true minutes or days ago, and a listing that lies is worse than a listing that
/// waits. Caching ids keeps every printed figure exactly as live as it is today
/// (one read per sale) while deleting the thousands of derivation probes that
/// were only ever used to FIND those ids.
///
/// **This file is no longer the fast path** - the on-chain per-creator index is
/// (see [`CreatorIndexLookup`]). What is left for the cache is the two things
/// that index cannot answer: an id a `--refresh` derivation dug up because it
/// predates the index, and an id of a sale this wallet just created while the
/// index read happened to be failing. Both are probed on every run, so neither
/// can silently fall off a listing.
///
/// The v1 `scanned` flag went with that change. It existed to stop
/// `remember_discovery`'s one-at-a-time appends from looking like a completed
/// scan and putting later runs on a fast path that would hide older sales.
/// There is no such trap now: the fast path is the chain, which is complete by
/// construction, and these ids are only ever added to what it returns.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct CacheEntry {
    /// Discovered account ids, lowercase hex, in the order they were found (so a
    /// cached listing prints in the same order it printed before).
    #[serde(default)]
    ids: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DiscoveryCache {
    version: u32,
    /// `"<kind>:<program id hex>" -> entry`.
    ///
    /// The PROGRAM ID is in the key because every id in the entry is derived
    /// under it: rebuild the guests and each PDA moves, so a cache keyed only by
    /// kind would hand back accounts on a program that is no longer deployed -
    /// worse than no cache, because the failure would look like "your sales are
    /// gone". A changed program id is simply a key that is not there: a miss,
    /// and a miss always falls back to the full scan.
    #[serde(default)]
    entries: BTreeMap<String, CacheEntry>,
}

/// `"<kind>:<program id hex>"` - see [`DiscoveryCache::entries`].
fn discovery_key(kind: &str, program: ProgramId) -> String {
    format!("{kind}:{}", hex_program_id(program))
}

/// Lowercase hex of a program id, laid out exactly as the CLI's
/// `fmt::hex_program` writes it, so a cache key can be eyeballed against
/// `lpad program-id bc`.
fn hex_program_id(program: ProgramId) -> String {
    let mut bytes = [0u8; 32];
    for (i, lane) in program.iter().enumerate() {
        bytes[i * 4..i * 4 + 4].copy_from_slice(&lane.to_ne_bytes());
    }
    hex_bytes(&bytes)
}

/// Inverse of [`hex_id`]. `None` for anything that is not 64 hex chars, so one
/// mangled line in the cache costs that id, not the file.
fn account_from_hex(s: &str) -> Option<AccountId> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(AccountId::new(out))
}

/// Append-only merge of a discovery result into what was already remembered:
/// everything confirmed now, in the order it was found, then every previously
/// remembered id this run did not confirm.
///
/// The second half is the whole point. `read` cannot tell "this is not a sale"
/// from "the sequencer did not answer", and a PDA that resolved once resolves
/// forever - so a failed probe must never evict. The list can only grow, and it
/// is bounded by the number of sales that actually exist.
fn merged_ids(found: &[String], previous: &[String]) -> Vec<String> {
    let mut ids = found.to_vec();
    for id in previous {
        if !ids.contains(id) {
            ids.push(id.clone());
        }
    }
    ids
}

/// The entry's ids, dropping any that no longer parse.
fn cached_ids(entry: &CacheEntry) -> Vec<AccountId> {
    entry.ids.iter().filter_map(|s| account_from_hex(s)).collect()
}

/// Read the cache. NEVER fatal, and never on stdout: a missing file is an
/// ordinary miss, and an unreadable, corrupt or wrong-version one is a miss with
/// a warning on stderr. Every caller falls back to the full scan, so the worst a
/// broken cache can cost is the time this change saved.
fn load_discovery_cache(path: &Path) -> DiscoveryCache {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return DiscoveryCache::default(),
        Err(e) => {
            eprintln!("warning: discovery cache {} is unreadable ({e}) - rescanning", path.display());
            return DiscoveryCache::default();
        }
    };
    match serde_json::from_str::<DiscoveryCache>(&raw) {
        Ok(cache) if cache.version == DISCOVERY_CACHE_VERSION => cache,
        Ok(cache) => {
            eprintln!(
                "warning: discovery cache {} is version {} (this build writes {DISCOVERY_CACHE_VERSION}) - rescanning",
                path.display(),
                cache.version
            );
            DiscoveryCache::default()
        }
        Err(e) => {
            eprintln!("warning: discovery cache {} is corrupt ({e}) - rescanning", path.display());
            DiscoveryCache::default()
        }
    }
}

/// Write the cache through a temp file + rename, so a crash mid-write leaves the
/// previous cache intact rather than a half-written one. Best-effort: a cache
/// that cannot be written is a slower next run, never a failed command.
fn store_discovery_cache(path: &Path, cache: &DiscoveryCache) {
    let write = || -> std::io::Result<()> {
        let body = serde_json::to_string_pretty(cache).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, path)
    };
    if let Err(e) = write() {
        eprintln!("warning: cannot write the discovery cache {} ({e}) - the next run will rescan", path.display());
    }
}

/// Program owners that rule an account OUT as a sale/pool creator.
///
/// The wallet mints tokens through its own key accounts - `mint_token_with_metadata`
/// creates the DEFINITION, the supply HOLDING and the METADATA account with
/// `create_new_account_public`, and `init_token_holding` creates one more per
/// receiving holding - so a wallet that has launched a few tokens carries far
/// more token-program-owned accounts than identity accounts, and every one of
/// them used to be probed as a possible creator across the whole `defs²·nonces`
/// space.
///
/// None of them is one. A creator is an identity account: `create_sale` /
/// LBP `create_sale` store it as the withdrawal authority and echo it unchanged
/// in the post-states, which under `validate_execution` rule 7
/// (`NonDefaultAccountWithDefaultOwner`) is why the bootstrap initialises the
/// creator under the authenticated-transfer program first. Token holdings and
/// definitions are owned by the token program; ATAs by the ATA program.
///
/// The list is a BLOCKLIST, not an allowlist, on purpose - a missed sale is a
/// worse bug than a slow listing:
///   * an account whose read FAILED is never pruned (unknown ≠ ruled out);
///   * a DEFAULT-owned account is never pruned. That case is live, not
///     theoretical: `bc_create_token_sale_private` creates a fresh, never-funded,
///     never-initialised account as the unlinkable creator A, which stays
///     DEFAULT-owned forever;
///   * an account owned by anything else at all - a program this build has never
///     heard of - is never pruned.
///
/// The one thing it does assume is that nobody passed `--creator <a token
/// holding>`. That is not rejected on chain (the guests only assert
/// `creator.is_authorized`, and a token holding CAN sign - the creator's own
/// token holding signs the very same transaction), merely nonsensical: the slot
/// is documented as an identity account, and `create_sale` errors already say so.
/// `--deep` exists for that one case, and for it alone.
fn creator_ineligible_owners() -> Vec<ProgramId> {
    vec![programs::token().id(), programs::ata().id()]
}

/// `(id, program owner)` for the PUBLIC half of a scan - `None` where the read
/// failed. Private accounts are not creator candidates at all: a sale PDA is
/// derived from a public creator id.
fn public_owners(scan: &[WalletAccount]) -> Vec<(AccountId, Option<ProgramId>)> {
    scan.iter()
        .filter(|&&(_, private, _)| !private)
        .map(|(id, _, acc)| (*id, acc.as_ref().map(|a| a.program_owner)))
        .collect()
}

/// The creator prune (see [`creator_ineligible_owners`]): keep every account
/// except those whose owner is known to be one that cannot be a creator.
fn creator_candidates(
    accounts: &[(AccountId, Option<ProgramId>)],
    ineligible: &[ProgramId],
) -> Vec<AccountId> {
    accounts
        .iter()
        .filter(|(_, owner)| owner.is_none_or(|o| !ineligible.contains(&o)))
        .map(|(id, _)| *id)
        .collect()
}

/// Distinct fungible token definitions across a holding set, sorted - the
/// token/collateral candidates a sale PDA is derived from.
fn definitions_of(holdings: &[Holding]) -> Vec<AccountId> {
    let mut defs: Vec<AccountId> =
        holdings.iter().filter(|h| !h.native).map(|h| h.definition).collect();
    defs.sort();
    defs.dedup();
    defs
}

// ---------------------------------------------------------------------------
// Discovery: the on-chain creator index (the fast path)
// ---------------------------------------------------------------------------

/// What one read of a creator's on-chain index account told us.
///
/// The three answers are not interchangeable, and the difference between the
/// last two IS the performance story this type exists for: "the chain says this
/// creator made nothing" costs no further reads at all, while "I could not tell"
/// costs a full derivation over that creator's candidate space -
/// `defs^2 x DISCOVERY_NONCE_MAX` speculative reads. Collapsing them would
/// either put every listing back on the 40-minute path or, far worse, report
/// "no sales" because a sequencer blipped.
#[derive(Clone, Debug, PartialEq, Eq)]
enum CreatorIndexLookup {
    /// The index PDA holds this creator's index. Its ids are the COMPLETE set of
    /// sales/pools that creator has created under this program id: the guest
    /// appends inside the same transaction that creates the sale and never
    /// removes an entry, so there is nothing left to derive for this creator.
    Indexed(Vec<AccountId>),
    /// The read succeeded and the index PDA is untouched. Authoritative: this
    /// creator has created nothing under this program id.
    ///
    /// Trusting that is what makes `my-sales` fast, so it is worth writing down
    /// why it is safe rather than optimistic. A LEZ program id IS the hash of
    /// the guest ELF, and the only `create_sale` this ELF can run is the one
    /// that writes the index. "No index under program P" therefore implies "no
    /// sale under program P" - not as a heuristic about what users usually do,
    /// but because no code that could have skipped the index was ever able to
    /// create an account at these addresses. A sale from an older lpad build
    /// lives under THAT build's program id, at PDAs this build cannot even
    /// name; [`Discovery::refresh`] is the recourse for it.
    Absent,
    /// The read failed, or the PDA holds something that is not this creator's
    /// index. Says nothing either way, so the caller falls back to the scan
    /// instead of reporting an empty listing.
    Unknown,
}

/// Classify one creator-index read. `decode` is the owning program's own
/// authenticating decode, yielding `(stored creator, ids)`.
///
/// Shared by the two programs because the classification - not the decode - is
/// where a mistake is expensive, and it is identical for both.
fn classify_creator_index(
    program: ProgramId,
    creator: AccountId,
    read: Result<Account>,
    decode: impl Fn(&Data) -> Option<(AccountId, Vec<AccountId>)>,
) -> CreatorIndexLookup {
    // A read that FAILED is not an answer. This is the branch that keeps a flaky
    // sequencer from turning into "you have no sales".
    let Ok(account) = read else { return CreatorIndexLookup::Unknown };
    // An account that does not exist reads back as `Account::default()` - LEZ
    // `State::get_account_by_id` fills the gap rather than erroring - so this is
    // the ordinary "never created anything" answer, not a failure.
    if account == Account::default() {
        return CreatorIndexLookup::Absent;
    }
    match decode(&account.data) {
        // Three pins, all cheap, none redundant with the others: the PDA address
        // (the caller derived it), the program that owns it (only the launchpad
        // guest can claim its own PDA), and the creator recorded INSIDE it (what
        // the guest asserts before appending). Anything else at this address is
        // some stranger's account that happens to decode.
        Some((stored, ids)) if account.program_owner == program && stored == creator => {
            CreatorIndexLookup::Indexed(ids)
        }
        // Deliberately `Unknown`, not `Absent`: bytes we cannot explain are a
        // reason to go and look, not a licence to say there is nothing here.
        _ => CreatorIndexLookup::Unknown,
    }
}

/// Which ids the on-chain index already handed over, and which creators still
/// need the brute-force derivation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DiscoveryPlan {
    /// Ids the index answered with, in creator order then index order (which is
    /// creation order - the guests append and never remove), deduped.
    indexed: Vec<AccountId>,
    /// Creators the derivation still has to cover, in creator order.
    derive: Vec<AccountId>,
}

/// Turn one index read per creator into a plan. Pure, so the decision table -
/// the part worth being sure about - is testable without a chain.
///
/// `force_derive` is `--refresh`/`--deep`: derive for EVERY creator, on top of
/// the index reads, because what those flags are hunting for is a sale created
/// before the index existed and no index read can rule one out.
fn plan_discovery(
    creators: &[AccountId],
    force_derive: bool,
    lookup: impl Fn(AccountId) -> CreatorIndexLookup,
) -> DiscoveryPlan {
    let mut plan = DiscoveryPlan::default();
    let mut seen: HashSet<AccountId> = HashSet::new();
    for &creator in creators {
        let answered = match lookup(creator) {
            CreatorIndexLookup::Indexed(ids) => {
                for id in ids {
                    if seen.insert(id) {
                        plan.indexed.push(id);
                    }
                }
                true
            }
            CreatorIndexLookup::Absent => true,
            CreatorIndexLookup::Unknown => false,
        };
        if (!answered || force_derive) && !plan.derive.contains(&creator) {
            plan.derive.push(creator);
        }
    }
    plan
}

/// What one [`resolve_candidates`] pass produced: the states it read, paired
/// with their ids, and the authoritative ids it could NOT read, paired with the
/// read's own error.
type Resolved<S> = (Vec<(AccountId, S)>, Vec<(AccountId, String)>);

/// Resolve the candidate ids into states, keeping the two kinds of candidate
/// apart: returns what was read, and the AUTHORITATIVE ids that could not be.
///
/// The distinction is the whole reason this is not one `filter_map`, because
/// `read` cannot tell "this account is not a sale" from "the sequencer did not
/// answer" - both come back `Err`.
///
///  * A SPECULATIVE id is one the derivation invented. All but a handful of the
///    thousands it probes are not sales, so a failed read there IS the expected
///    answer and dropping it silently is correct.
///  * An AUTHORITATIVE id is one the on-chain index named, or one a past run
///    already resolved and remembered. Both are positive statements that the
///    sale EXISTS - the guest appends the id inside the very transaction that
///    creates it, and the cache is append-only - so a failed read on one is a
///    sequencer failure or a state this build cannot decode, never an absence.
///
/// Dropping an authoritative id shortens a listing whose entire job is to be
/// complete, and in the one direction that costs: a creator checking whether
/// the sale they just paid for landed is told it did not exist. Under-reporting
/// has to be louder than a missing row, so it is returned rather than dropped.
fn resolve_candidates<S>(
    candidates: Vec<AccountId>,
    authoritative: &HashSet<AccountId>,
    read: impl Fn(AccountId) -> Result<S>,
) -> Resolved<S> {
    let mut found: Vec<(AccountId, S)> = Vec::new();
    let mut unreadable: Vec<(AccountId, String)> = Vec::new();
    for id in candidates {
        match read(id) {
            Ok(state) => found.push((id, state)),
            Err(e) if authoritative.contains(&id) => unreadable.push((id, e)),
            Err(_) => {}
        }
    }
    (found, unreadable)
}

/// What one discovery result is called when a message names it. Derived from
/// `kind` rather than passed next to it, so the noun and the cache key cannot
/// drift apart.
fn discovery_noun(kind: &str) -> &'static str {
    if kind == DISCOVERY_KIND_LBP {
        "pool"
    } else {
        "sale"
    }
}

// ---------------------------------------------------------------------------
// Reads: decoding launchpad state, owner-pinned
// ---------------------------------------------------------------------------

/// Decode a bonding-curve `SaleState`, pinning the account's `program_owner` to
/// the bonding-curve program first.
///
/// The pin is not redundant with the decode. LEZ deployment is permissionless,
/// so anyone can put an account under their OWN program whose bytes happen to
/// Borsh-decode as a `SaleState` - with a chosen `token_name`/`token_symbol`,
/// chosen reserves and a plausible treasury. Without the pin, `bc sale-info`
/// and `quote-live` present that account as a genuine sale and a participant
/// pays for a transaction the guest then reverts (it asserts the owner itself).
/// `classify_creator_index` already singles this pin out as the one that
/// authenticates a launchpad account; the two read paths agree now.
fn decode_bc_sale(acc: &Account, program: ProgramId) -> Result<SaleState> {
    if acc.program_owner != program {
        return Err("account is not owned by the bonding-curve program, so it is not a sale of \
                    this build - check the id, and `lpad program-id bc` against the sale's chain"
            .into());
    }
    SaleState::try_from(&acc.data).map_err(|_| "account is not a bonding-curve sale".into())
}

/// [`decode_bc_sale`] for the LBP program - same pin, same reasoning.
fn decode_lbp_pool(acc: &Account, program: ProgramId) -> Result<PoolState> {
    if acc.program_owner != program {
        return Err("account is not owned by the LBP program, so it is not a pool of this build \
                    - check the id, and `lpad program-id lbp` against the pool's chain"
            .into());
    }
    PoolState::try_from(&acc.data).map_err(|_| "account is not an LBP pool".into())
}

// ---------------------------------------------------------------------------
// Sweep: who has to sign a SweepTreasury
// ---------------------------------------------------------------------------

/// The signer set for a `SweepTreasury` against `acc`, or the error that says
/// why no signature can help. See [`LaunchpadClient::sweep_treasury_signers`]
/// for why the second arm is a dead end rather than a bootstrap.
fn sweep_treasury_signers_for(treasury: AccountId, acc: &Account) -> Result<Vec<AccountId>> {
    // Owned by something means the account exists. Whether it can RECEIVE this
    // fee (right definition, right token program) is a different question, and
    // one no signature answers - the guest reverts and says so.
    if acc.program_owner != DEFAULT_PROGRAM_ID {
        return Ok(Vec::new());
    }
    Err(format!(
        "the treasury {} is unowned. `CreateSale` pins an initialised holding as the treasury and \
         nothing on LEZ can un-initialise one, so a sale naming this account should not exist - \
         and both guests reject an unowned treasury outright, whatever signs the sweep. The \
         escrowed fee cannot be settled here and retrying cannot help: report this, quoting the \
         sale id and this treasury id",
        hex_id(treasury)
    ))
}

// ---------------------------------------------------------------------------
// Creator: the account every settlement has to echo
// ---------------------------------------------------------------------------

/// Reject a creator that can sign the `CreateSale` and then never be named
/// again - which strands the deposit and the whole raise, permanently.
///
/// `close_sale` and `withdraw` both echo the creator's own account in their
/// post-states (`AccountPostState::new(creator.account)`), and neither may drop
/// it: an account declared in the message and missing from the output is
/// `DeclaredAccountMissingFromOutput`. So whatever signs a create has to stay an
/// account a program is allowed to echo, for the whole life of the sale.
///
/// `validate_execution` rule 7 (`NonDefaultAccountWithDefaultOwner`) forbids one
/// shape: a post state owned by the DEFAULT program whose PRE state is not the
/// whole-default account. LEZ bumps every signer's nonce AFTER the diff is
/// applied (`State::apply_state_diff`), so an account that was never initialised
/// and signs one transaction lands exactly there - DEFAULT-owned, nonce 1. The
/// create itself succeeds, because during it the account is still whole-default;
/// every close and withdraw afterwards fails. Nor can it be repaired later:
/// `authenticated_transfer::Initialize` claims the account with
/// `Claim::Authorized`, and rule 7 rejects that claim for the same reason.
///
/// This is the trap scripts/bootstrap.sh initialises the creator to avoid (its
/// "keeps the creator clear of `validate_execution` rule 7" note), and it is why
/// the private launch initialises its unlinkable creator A the moment it mints
/// it. Checked here because it can be checked
/// nowhere else: the guests see a whole-default creator and are right to accept
/// it, and the runtime only sees the stranded state one transaction too late.
fn ensure_creator_settleable_for(creator: AccountId, acc: &Account) -> Result<()> {
    // Owned by any program at all - the bootstrap's authenticated-transfer
    // creator, or the token-program-owned account an integration fixture uses -
    // can never trip rule 7, whatever it goes on to sign.
    if acc.program_owner != DEFAULT_PROGRAM_ID {
        return Ok(());
    }
    if *acc == Account::default() {
        return Err(format!(
            "the creator {} has never been initialised, so creating a sale from it would strand \
             that sale: signing the create bumps its nonce, and LEZ then rejects every later \
             transaction that names it - including the close and the withdraw, both of which MUST \
             echo the creator. The deposit and the raise would be locked for good. Initialise it \
             first with `wallet auth-transfer init --account-id Public/{}` (what \
             scripts/bootstrap.sh does before the creator signs anything, and what `lpad faucet \
             --to` does on the way to funding it), then retry",
            hex_id(creator),
            hex_id(creator)
        ));
    }
    Err(format!(
        "the creator {} is unowned and has already signed or been paid, so LEZ rejects any \
         transaction that echoes it - which the close and the withdraw of this sale both must - \
         and it can no longer be initialised either (`authenticated_transfer::Initialize` claims \
         only a whole-default account). This account cannot be a creator on this chain. Create \
         the sale from a fresh one: `wallet account new public`, then `wallet auth-transfer init \
         --account-id Public/<new id>` BEFORE it signs anything",
        hex_id(creator)
    ))
}

// ---------------------------------------------------------------------------
// wlez bootstrap: which launch is allowed to pay for the one-time Initialize
// ---------------------------------------------------------------------------

/// Which kind of launch is asking - the ordinary public one, or the unlinkable
/// private one whose entire product is that nothing ties the sale to the wallet
/// that minted the token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LaunchLinkage {
    Public,
    Unlinkable,
}

/// What a launch must do about wlez before it can raise in native LEZ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WlezBootstrap {
    /// Initialised and canonically pinned: submit nothing.
    Done,
    /// Not initialised (or not readable): this launch has to bootstrap it.
    Needed,
}

/// Decide that - and refuse, for an unlinkable launch.
///
/// wlez `Initialize` is a PUBLIC transaction over `[vault, def, init_holding,
/// reference_token_def, payer]`, signed by the payer. Run from a private launch
/// with the project's own definition as the reference, it publishes the pair
/// `(real creator, project token definition)` in cleartext - and that same
/// definition is the sale's public `token_definition_id` and a seed of its PDA.
/// An observer needs no timing correlation and no chain analysis: they join the
/// two values and read off the wallet behind a sale whose on-chain creator is a
/// throwaway account. The one guarantee this launch sells, lost to a transaction
/// that is not even part of it.
///
/// So the private path does not initialise wlez, with ANY payer. Paying with the
/// throwaway creator A would close this particular join, but it would still
/// entangle a chain-wide, permissionless, once-ever bootstrap - the one whose
/// failure mode is a front-run recoverable only by rebuilding wlez under a new
/// image id (see [`LaunchpadClient::wlez_programs_checked`]) - with the launch
/// whose anonymity is the point, and it would fund it from an account that has
/// to stay cheap to abandon. Bootstrapping is an operator act with no secret in
/// it; the refusal names the public command that performs it.
///
/// A probe that FAILED counts as "not ready" on purpose. For a public launch
/// that is harmless - the guest's `Initialize` is idempotent, so re-sending it
/// costs one transaction and settles the question. For a private launch there is
/// nothing to lose by refusing, and proceeding on a maybe is how the leak got
/// there in the first place.
fn wlez_bootstrap(linkage: LaunchLinkage, probe: Result<()>) -> Result<WlezBootstrap> {
    match (linkage, probe) {
        (_, Ok(())) => Ok(WlezBootstrap::Done),
        (LaunchLinkage::Public, Err(_)) => Ok(WlezBootstrap::Needed),
        (LaunchLinkage::Unlinkable, Err(e)) => Err(format!(
            "this chain's wlez is not usable yet ({e}), and an unlinkable launch must not be the \
             thing that initialises it: `Initialize` is a public transaction naming its payer \
             beside a token definition, so sending it here would publish your real account next \
             to the very token this sale is supposed to be unlinkable from. Run `lpad wrap --amount 1` \
             once from a funded account first - an ordinary public wrap bootstraps wlez, needs \
             only some token holding of this wallet as its reference, and says nothing about \
             this launch - then re-run the private launch"
        )),
    }
}

/// The accounts a wlez `Initialize` names, in the order the guest destructures
/// them. Split out of [`LaunchpadClient::initialize_wlez`] for the same reason
/// the `CreateSale` lists are: so a test can pin what this transaction PUBLISHES
/// without a chain - which is how the private path's leak is stated as an
/// executable fact rather than a comment.
#[must_use]
fn wlez_initialize_accounts(
    wlez_program: ProgramId,
    reference_token_def: AccountId,
    payer: AccountId,
) -> Vec<AccountId> {
    vec![
        wlez_core::get_wlez_vault_id(&wlez_program),
        wlez_core::get_wlez_definition_id(&wlez_program),
        wlez_core::get_wlez_init_holding_id(&wlez_program),
        reference_token_def,
        payer,
    ]
}

// ---------------------------------------------------------------------------
// Deanonymisation tripwire
// ---------------------------------------------------------------------------

/// The id an in-flight unlinkable launch must never publish, if this account
/// list names it.
///
/// [`LaunchpadClient::bc_create_token_sale_private`] arms this for the whole
/// launch instead of auditing each leg, because the leak it exists to stop was a
/// leg nobody counted as part of the private flow at all - the wlez bootstrap -
/// and the next one will be too. Every transaction the launch submits passes
/// through one of two functions; both consult this.
fn deanonymising_account(accounts: &[AccountId], forbidden: Option<AccountId>) -> Option<AccountId> {
    let forbidden = forbidden?;
    accounts.iter().copied().find(|id| *id == forbidden)
}

/// The account id of a PUBLIC slot of a privacy transaction - the slots a
/// privacy proof still publishes. `None` for a private slot, whose id never
/// reaches the chain.
fn public_slot_id(slot: &AccountIdentity) -> Option<AccountId> {
    match slot {
        AccountIdentity::Public(id) | AccountIdentity::PublicNoSign(id) => Some(*id),
        AccountIdentity::PublicKeycard { account_id, .. } => Some(*account_id),
        _ => None,
    }
}

/// TTL for a transaction the sequencer proves: signed here, submitted straight
/// away, so 120s is generous.
const DEFAULT_TX_TTL_MS: u64 = 120_000;

/// TTL for a transaction whose proof is generated *here* (the `BuyDisposable`
/// paths): 40 minutes.
///
/// The public 120s TTL is useless against a real recursive STARK - the proof
/// alone takes many minutes, so a 120s deadline would expire before the
/// transaction could be submitted, let alone included, and every private buy
/// would fail as `OutOfValidityWindow`. A generous value is safe here because
/// the deadline is not the only bound: both guests clamp the window they emit
/// down to the earliest of it, one hour past the price timestamp
/// (`MAX_PRIVATE_WINDOW_MS`), and the sale's own end - so a long TTL cannot buy
/// the submitter a longer free option than the guest allows.
///
/// It is therefore set to the guest's own cap. It used to be 40 minutes, chosen
/// against a stale ~31.5-minute estimate for a private op. A real-proof run on
/// Logos testnet measured **38-40 minutes for a single STARK**, so the deadline -
/// which is stamped from a clock read BEFORE proving starts - expired at almost
/// exactly the moment the proof became available, and `bc buy-disposable` failed
/// as an inclusion timeout. Anything tighter than the guest cap is throwing away
/// slack for nothing, since the guest clamps to that cap regardless.
///
/// STRUCTURAL LIMIT worth knowing: proving is ~40 min against a 60-min cap, so a
/// disposable buy has ~20 minutes of headroom. If proving ever exceeds
/// `MAX_PRIVATE_WINDOW_MS`, single-transaction private buys become impossible
/// and that constant - a free-option security parameter - has to be revisited
/// rather than the TTL.
const DEFAULT_PRIVATE_TX_TTL_MS: u64 = 3_600_000;

/// Read a TTL override in ms from `var`, falling back to `default_ms`. A value
/// that does not parse is ignored rather than fatal, so a typo in the
/// environment degrades to the default instead of bricking every command.
fn ttl_ms(var: &str, default_ms: u64) -> u64 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default_ms)
}

/// How long [`LaunchpadClient::sync_private`] tolerates *no forward progress*
/// before it calls the sync wedged. Overridable via `$LPAD_SYNC_STALL_SECS`.
///
/// This is a stall bound, not a duration bound: the window restarts every time
/// the wallet's last-synced block advances, so a slow-but-healthy first scan of
/// a long chain is never killed merely for being slow.
///
/// It exists because `WalletCore::sync_to_latest_block` awaits a block stream
/// that can stop yielding *without erroring*. A live run once sat inside that
/// await for 5h33m at 0% CPU - indistinguishable from a hang, and it cost a
/// whole session. Five minutes is far longer than a healthy sequencer needs to
/// hand over a single block and short enough that a wedge is a coffee break
/// rather than an afternoon.
const DEFAULT_SYNC_STALL_SECS: u64 = 300;

/// Ceiling on one [`LaunchpadClient::sync_private`] call, however healthy it
/// looks. Overridable via `$LPAD_SYNC_MAX_SECS`.
///
/// The stall bound alone cannot guarantee termination: if a chain produces
/// blocks faster than this wallet scans them, every window makes progress and
/// the loop never reaches head. This is the backstop for exactly that case.
/// Giving up is cheap - `sync_to_block` persists after every block, so the next
/// run resumes from where this one stopped and nothing is rescanned.
const DEFAULT_SYNC_MAX_SECS: u64 = 3_600;

/// Replace LEZ's default polling numbers in a freshly created `wallet_config.json`.
///
/// The defaults are not merely conservative, they are wrong for a public chain,
/// and the symptom is the worst kind: `seq_poll_timeout` 12s x `seq_poll_max_retries`
/// 5 is 60 seconds of patience against a block every ~46-50s, so a transaction
/// that lands perfectly normally in the next block is reported as
/// `All pollers failed` - a rejection, for a success. The first `lpad faucet` on
/// a wallet created with the defaults failed exactly that way, which is how this
/// function came to exist.
///
/// `calibration_limit` matters for a different reason: the wallet probes every
/// sequencer missing from `statistics.json` that many times (default 100) on
/// every open, and the CLI opens a fresh wallet per command, so over a WAN hop
/// the default is paid on every single invocation.
///
/// The numbers are `scripts/bootstrap.sh`'s, deliberately - they are the ones
/// measured against these sequencers, and two sets of tuning that could drift
/// apart would be worse than one. 60 x 30s is a ~30 minute budget per
/// transaction, which is what a real private operation needs.
///
/// Best-effort in one respect only: it is applied to the FILE, so it governs
/// every later command. The wallet already built in this process keeps the
/// defaults in memory, which costs nothing because creating a wallet polls for
/// nothing.
fn tune_fresh_config(path: &Path) -> Result<()> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("read fresh wallet config {}: {e}", path.display()))?;
    let mut cfg: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("parse fresh wallet config {}: {e}", path.display()))?;
    let Some(obj) = cfg.as_object_mut() else {
        return Err(format!("fresh wallet config {} is not a JSON object", path.display()));
    };
    // Wall-clock budget per transaction is `seq_tx_poll_max_blocks x
    // seq_poll_timeout` - the poller sleeps `polling_delay` between rounds - so
    // this is 30 x 10s = 5 minutes, about six blocks of patience against a
    // ~46-50s block. The happy path returns as soon as the transaction is seen
    // (one block), so this budget is only ever spent on something that did not
    // land, and 30 minutes of spending it was the single largest source of dead
    // time in a run.
    //
    // `seq_poll_max_retries` is NOT the wall-clock knob: it bounds an INNER loop
    // that fires `get_transaction` back-to-back with no sleep, so the old 60
    // meant 61 RPC round trips per round. Two is enough to ride out a transient.
    //
    // Shortening this is only safe because every leg that can be reported failed
    // after actually landing now confirms by reading chain state instead of
    // trusting the poller - the deshield checks account A was funded, the public
    // leg checks A was drained, and the re-shield checks its source is drained.
    // Without those guards a short budget turns a landed transaction into a
    // rollback that proves a second STARK to undo a trade that succeeded.
    obj.insert("seq_poll_timeout".into(), serde_json::json!("10s"));
    obj.insert("seq_tx_poll_max_blocks".into(), serde_json::json!(30));
    obj.insert("seq_poll_max_retries".into(), serde_json::json!(2));
    let multi = obj
        .entry("multi_sequencer_client_config")
        .or_insert_with(|| serde_json::json!({}));
    if let Some(m) = multi.as_object_mut() {
        m.insert("distribution_limit".into(), serde_json::json!(1));
        m.insert("calibration_limit".into(), serde_json::json!(3));
    }
    let out = serde_json::to_string_pretty(&cfg)
        .map_err(|e| format!("serialise wallet config: {e}"))?;
    std::fs::write(path, out)
        .map_err(|e| format!("write fresh wallet config {}: {e}", path.display()))
}

/// Parse a seconds-valued override, falling back to `default_secs`.
///
/// Split from the environment read so the parsing rule is testable without
/// mutating a process-global. Absent, unparseable and zero all degrade to the
/// default: a typo must not brick every private command, and a zero window
/// would fail every sync on its first tick.
fn secs_or(raw: Option<&str>, default_secs: u64) -> u64 {
    raw.and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(default_secs)
}

/// [`secs_or`] against the live environment.
fn secs_env(var: &str, default_secs: u64) -> u64 {
    secs_or(std::env::var(var).ok().as_deref(), default_secs)
}

/// What a timed-out sync window earns: another window, or an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncStep {
    /// Blocks were scanned inside the window. Give it another one.
    Retry,
    /// A whole stall window passed and not one block was scanned. The stream is
    /// wedged, not slow.
    Stalled,
    /// Blocks are still arriving, but the call has spent its entire budget.
    Exhausted,
}

/// Decide, from the only two things that matter - did the last-synced block
/// move, and is the budget gone - whether a timed-out sync window deserves
/// another. Pure, so the policy is tested without a chain.
///
/// `Stalled` outranks `Exhausted` on purpose: when a window both made no
/// progress and ran out the clock, "the stream is dead" is the useful diagnosis
/// and "this chain is fast" is the misleading one.
fn sync_step(before: u64, after: u64, spent: std::time::Duration, budget: std::time::Duration) -> SyncStep {
    if after <= before {
        SyncStep::Stalled
    } else if spent >= budget {
        SyncStep::Exhausted
    } else {
        SyncStep::Retry
    }
}

/// The single clock read a `BuyDisposable` is built from.
///
/// Both fields come from ONE [`LaunchpadClient::now_ms`] call on purpose. The
/// LBP prices the buy at `at_ms` rather than at inclusion time, so a caller that
/// quotes its slippage floor at a *different* timestamp than the one it passes
/// in the instruction quotes a different price than it buys at - and on a
/// declining-weight schedule that is how a private buy trips its own
/// `min_tokens_out`. Take this once, quote with it, and pass it in.
#[derive(Debug, Clone, Copy)]
pub struct PrivateBuyTime {
    /// On-chain wall-clock (ms) at build time: the bonding curve's
    /// `not_before_ms`, the LBP's `t_buy_ms`. Also the *inclusive lower bound*
    /// of the validity window the guest emits, so the transaction cannot be
    /// included before the moment it was priced at.
    pub at_ms: u64,
    /// `at_ms` plus the private-path TTL (40 minutes by default, see
    /// `$LPAD_PRIVATE_TX_TTL_MS`). The guest clamps it further; see
    /// `bonding_curve_core::private_window_hi`.
    pub deadline: u64,
}

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
// Privacy: packaging a launchpad program for a proof
// ---------------------------------------------------------------------------

/// Package one of lpad's own guests for a privacy proof: the program itself,
/// plus the token program its chained `token::Transfer`s dispatch to.
///
/// A privacy transaction proves the whole call tree in one circuit, so the host
/// must hand the prover every program the guest chains into before proving
/// starts. LEZ v0.2.4 refuses to guess: an undeclared one is
/// `UndeclaredProgramDependency`
/// (`lee/state_machine/src/privacy_preserving_transaction/circuit/mod.rs`, the
/// `dependencies.get(..).ok_or(..)` in the chained-call loop). Both
/// `BuyDisposable` guests emit exactly two `token::Transfer`s and nothing else -
/// the vault legs and the buyer legs all run on the token program the buyer's
/// holding is owned by - so the token program is the entire dependency map.
///
/// `requested` is the program id the caller asked to transact against. Proving
/// needs the ELF, not just the id, and the only ELF this build has is the
/// committed artifact - so a `--program` override naming some other deployment
/// can be submitted publicly but cannot be proved here. Say that, rather than
/// silently proving against a different program than the caller named.
fn disposable_program(requested: ProgramId, program: Program, what: &str) -> Result<ProgramWithDependencies> {
    if requested != program.id() {
        return Err(format!(
            "a private (disposable) buy proves the {what} guest locally, and this build only \
             carries the committed {what} artifact's ELF - it cannot prove the --program id you \
             passed. Drop --program to use the built-in {what} program, or use a public path \
             (which submits an id without proving it)"
        ));
    }
    let token = programs::token();
    Ok(ProgramWithDependencies::new(program, HashMap::from([(token.id(), token)])))
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
// CreateSale account lists
// ---------------------------------------------------------------------------
//
// Both guests destructure `CreateSale`'s accounts POSITIONALLY, so the order of
// these vecs is a load-bearing part of the call - and it is the part with no
// type to protect it. A vec of the right length in the wrong order still
// serialises, still submits, still costs a transaction, and comes back as an
// assert about whichever account happened to land in the slot the guest was
// checking ("treasury account does not match the treasury id..." when what went
// wrong was the token definition). Split out of the builders so a test can pin
// each id to its index without a wallet or a chain; the builders themselves need
// both.
//
// The two lists are deliberately NOT the same shape: the curve puts the treasury
// at index 3 (where `Buy`/`Sell`/`BuyAta`/`SellAta` also put it), the LBP puts it
// after `creator`, which kept every pre-existing index stable when it was added.
// Do not "harmonise" them here - each mirrors its own guest, and only the guest
// gets to decide.
//
// Both lists carry the creator's INDEX PDA in the slot directly before the
// clock. That account is what makes `my-sales`/`my-pools` cheap - see
// [`CreatorIndexLookup`] - and it is written by the very transaction these
// vecs describe, so omitting it does not degrade discovery, it fails the
// create. The clock stays last in both because each guest's `echo_clock`
// appends it there.

/// The bonding-curve `CreateSale` account list, in the order
/// `programs/bonding_curve/src/create_sale.rs` destructures it.
#[must_use]
fn bc_create_sale_accounts(
    sale: AccountId,
    token_vault: AccountId,
    collateral_vault: AccountId,
    token_def: AccountId,
    a: &BcCreateArgs,
) -> Vec<AccountId> {
    vec![
        sale,
        token_vault,
        collateral_vault,
        a.treasury,
        token_def,
        a.collateral_def,
        a.creator_token_holding,
        a.creator,
        bonding_curve_core::compute_creator_index_pda(a.program, a.creator),
        bonding_curve_core::CLOCK_01,
    ]
}

/// The LBP `CreateSale` account list, in the order
/// `programs/lbp/src/create_sale.rs` destructures it.
#[must_use]
fn lbp_create_sale_accounts(
    pool: AccountId,
    token_vault: AccountId,
    collateral_vault: AccountId,
    token_def: AccountId,
    a: &LbpCreateArgs,
) -> Vec<AccountId> {
    vec![
        pool,
        token_vault,
        collateral_vault,
        token_def,
        a.collateral_def,
        a.creator_token_holding,
        a.creator_collateral_holding,
        a.creator,
        a.treasury,
        lbp_core::compute_creator_index_pda(a.program, a.creator),
        lbp_core::CLOCK_01,
    ]
}

// ---------------------------------------------------------------------------
// Networks
// ---------------------------------------------------------------------------

/// The Logos public testnet sequencer. The default: it is the network most
/// users mean, and it is operated independently of this repo.
pub const TESTNET_SEQUENCER: &str = "https://testnet.lez.logos.co";

/// The Paradox Computer testnet sequencer.
pub const PARADOX_SEQUENCER: &str = "https://seq-testnet.paradox.computer";

/// Where a sequencer the user runs themselves is *offered* to be, before they
/// edit it.
///
/// 3040 is the port lpad's own `run-sequencer.sh` listened on (`--port 3040
/// --listen-address 127.0.0.1`) until commit 7119197 deleted it, and the port
/// upstream's own `sequencer_service_rpc` example uses, so it is what a freshly
/// started LEZ node is most likely on. It is only a default: a
/// [`Network::Local`] carries whatever URL it was built with.
pub const LOCAL_SEQUENCER: &str = "http://127.0.0.1:3040";

/// Which sequencer to talk to.
///
/// Three named choices a picker can offer by number - [`Self::Testnet`],
/// [`Self::Paradox`], [`Self::Local`] - plus [`Self::Custom`], which is not a
/// menu entry but what a pasted http(s) URL parses to, exactly as `--network`
/// has always accepted. Mirrors the LEZ wallet's own `NetworkAlias` so the
/// vocabulary matches, with `paradox` and `local` added.
///
/// lpad bundles no sequencer: `Local` is one the *user* installs and runs
/// (7119197 deleted `run-sequencer.sh`). It is its own variant rather than just
/// a `Custom` URL because a caller has to be able to ask "did the user pick
/// local?" without string-matching a hostname: a fresh local chain carries none
/// of lpad's programs, and a transaction against a program that is not there is
/// dropped from the mempool and reads as a timeout rather than an error - so
/// that is the one network where a CLI should check
/// [`LaunchpadClient::lpad_deployment_status`] and offer
/// [`LaunchpadClient::deploy_lpad_programs`] instead of letting the user
/// discover it as flakiness.
///
/// Every variant round-trips through [`std::fmt::Display`] and [`Self::parse`],
/// which is what lets a caller remember the choice in a config file and read it
/// back - `Local` included (`local=<url>`), so a user's edited local URL
/// survives a restart instead of silently reverting to [`LOCAL_SEQUENCER`].
///
/// The two public networks run a LEZ build whose built-in program image ids
/// match **v0.2.4**, which is why the crates are pinned there. The pin is not
/// cosmetic: a LEZ version bump changes those ids (token,
/// authenticated_transfer, clock, the privacy circuit), and a build pinned to a
/// version the operators have not adopted cannot transact against these chains
/// at all - it fails as a timeout, not as an error. The `chain_parity` tests at
/// the bottom of this file assert the equality on every CI run; treat a failure
/// there as "do not ship this pin", not as a flake. The same applies to a node
/// you run yourself: it must be v0.2.4 too, or nothing lands and nothing says
/// why.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Network {
    /// `https://testnet.lez.logos.co`
    #[default]
    Testnet,
    /// `https://seq-testnet.paradox.computer`
    Paradox,
    /// A sequencer the user installs and runs, at the URL they gave (default
    /// [`LOCAL_SEQUENCER`]). Build it with [`Self::local`].
    Local(String),
    /// Any other sequencer URL, passed through verbatim.
    Custom(String),
}

impl Network {
    /// Resolve one of `testnet` (or `logos`), `paradox`, `local`,
    /// `local=<url>`, or a bare http(s) sequencer URL.
    ///
    /// `local=<url>` is the stored form of a local pick, so this reads back
    /// exactly what [`std::fmt::Display`] wrote; a bare `local` means
    /// [`LOCAL_SEQUENCER`]. Bare URLs still resolve to [`Self::Custom`],
    /// unchanged, so every `--network` argument that worked before still works.
    pub fn parse(alias: &str) -> Result<Self> {
        let alias = alias.trim();
        if let Some(url) = alias.strip_prefix("local=") {
            let url = url.trim();
            if !is_http_url(url) {
                return Err(format!(
                    "local sequencer URL {url:?} is not http(s) - write it as \
                     `local={LOCAL_SEQUENCER}`, scheme and port included"
                ));
            }
            return Ok(Self::Local(url.to_owned()));
        }
        match alias {
            "testnet" | "logos" => Ok(Self::Testnet),
            "paradox" => Ok(Self::Paradox),
            "local" => Ok(Self::local("")),
            other if is_http_url(other) => Ok(Self::Custom(other.to_owned())),
            other => Err(format!(
                "unknown network {other:?} - use `testnet`, `paradox`, `local` (or \
                 `local=<url>` for a sequencer you run elsewhere), or an http(s) sequencer URL"
            )),
        }
    }

    /// A local network at `url`, or at [`LOCAL_SEQUENCER`] when `url` is blank -
    /// so a picker can pass straight through whatever the user typed, including
    /// nothing at all (the "just press enter" path).
    #[must_use]
    pub fn local(url: &str) -> Self {
        let url = url.trim();
        Self::Local(if url.is_empty() { LOCAL_SEQUENCER.to_owned() } else { url.to_owned() })
    }

    /// The three named choices, in the order a first-run picker should offer
    /// them. [`Self::Custom`] is deliberately absent: it is not something to
    /// pick from a list, it is what a pasted URL becomes.
    #[must_use]
    pub fn choices() -> [Self; 3] {
        [Self::Testnet, Self::Paradox, Self::local("")]
    }

    /// The sequencer URL this network resolves to.
    #[must_use]
    pub fn url(&self) -> &str {
        match self {
            Self::Testnet => TESTNET_SEQUENCER,
            Self::Paradox => PARADOX_SEQUENCER,
            Self::Local(u) | Self::Custom(u) => u,
        }
    }

    /// The human label for a menu line or a summary header - "Logos testnet",
    /// not "testnet". It deliberately carries no URL of its own, so a caller can
    /// print the label and [`Self::url`] in two aligned columns.
    #[must_use]
    pub const fn display_name(&self) -> &'static str {
        match self {
            Self::Testnet => "Logos testnet",
            Self::Paradox => "Paradox",
            Self::Local(_) => "Local",
            Self::Custom(_) => "Custom",
        }
    }

    /// True for a chain the user runs themselves - the only one lpad's programs
    /// may simply not be on. See the type docs for why that is worth a variant.
    #[must_use]
    pub const fn is_local(&self) -> bool {
        matches!(self, Self::Local(_))
    }
}

/// The scheme test [`Network::parse`] applies in both of its places, so a bare
/// URL and a `local=` URL are held to the same standard.
fn is_http_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

impl std::fmt::Display for Network {
    /// The *config* form: whatever this prints, [`Network::parse`] accepts and
    /// resolves back to an equal value. It is not a human label - that is
    /// [`Network::display_name`] - and `local=<url>` must not be "tidied" down
    /// to `local`, which would silently discard the user's edited URL the next
    /// time the config is read.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Testnet => write!(f, "testnet"),
            Self::Paradox => write!(f, "paradox"),
            Self::Local(u) => write!(f, "local={u}"),
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
    /// The wallet home: the directory that holds `wallet_config.json`, and that
    /// the CLI already treats as the wallet's own scratch space - it is where
    /// `WalletPaths::resolve` puts `statistics.json` and where the bootstrap
    /// writes the `proof_mode` marker the CLI reads back. The discovery cache
    /// lives here for the same reason those do: it describes ONE wallet, so it
    /// has to travel with that wallet and not with the process. `None` only when
    /// the config path has no parent, in which case discovery just runs uncached.
    home: Option<PathBuf>,
    /// Armed for the duration of an unlinkable (private) launch: the real,
    /// funded wallet account that no transaction may name until the launch is
    /// finished. `None` at every other moment. See [`deanonymising_account`].
    unlinkable_launch_creator: Option<AccountId>,
}

impl LaunchpadClient {
    /// Open the LEZ wallet (config + storage) and connect to its sequencer.
    ///
    /// `network`, when given, overrides the sequencer in the wallet config
    /// *without rewriting the file* - so `--network` is a per-invocation switch
    /// and cannot silently repoint a wallet for later commands.
    /// Create a brand-new wallet: fresh keys, written to `storage`, and the
    /// recovery mnemonic returned ONCE.
    ///
    /// Until this existed, lpad could not produce a wallet at all - it could only
    /// open one. `Storage::from_path` fails outright when the file is absent, so
    /// a user who installed the CLI and nothing else got
    /// `Failed to load storage from …` and no way forward; the only thing that
    /// could create the file was the LEZ `wallet` BINARY, which means a LEZ
    /// checkout, which is exactly what packaging lpad for npm was meant to make
    /// unnecessary. The `wallet` crate is already a dependency of this one, so
    /// the capability was always linked in - just unreachable.
    ///
    /// # There is no password, and that is not an omission here
    ///
    /// `WalletCore::new_init_storage` takes one and **discards it**:
    /// `Storage::new` is `// TODO: Use password for storage encryption` followed
    /// by `let _ = password;`, and `Storage::from_path` takes no password at all,
    /// because `save_to_path` writes plain `serde_json`. So `storage.json` holds
    /// raw signing and spending keys in cleartext. Prompting for a password here
    /// would be worse than not having one: it would tell a user their keys are
    /// protected when nothing is protecting them. This passes the empty string
    /// and says so out loud instead. The file is created `0600` by the caller for
    /// the same reason - it is the only protection there actually is.
    ///
    /// Returns the wallet and the mnemonic. The mnemonic is the ONLY way back to
    /// these keys; nothing stores it.
    pub fn create(
        config: PathBuf,
        storage: PathBuf,
        statistics: PathBuf,
        network: Option<&Network>,
    ) -> Result<(Self, String)> {
        // Refusing to overwrite is the whole safety story of this function. An
        // existing storage.json is somebody's keys, and replacing it with fresh
        // ones strands every balance behind them permanently - there is no undo
        // and no support desk. Checked here rather than only in the CLI so no
        // other caller can skip it.
        if storage.exists() {
            return Err(format!(
                "a wallet already exists at {} - refusing to overwrite it. Creating a new one \
                 would replace its keys, and every balance held by the old ones would be \
                 unreachable forever. Move that file aside first if you really want a fresh \
                 wallet.",
                storage.display()
            ));
        }
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("tokio runtime: {e}"))?;
        let overrides = Self::network_overrides(network)?;
        let home = config.parent().map(Path::to_path_buf);
        // Kept because `config` is moved into the wallet below, and the file it
        // names still needs its polling numbers fixed afterwards.
        let config_file = config.clone();
        // Creating a wallet is the chattiest path in the wallet crate - it
        // announces the missing config, the directory it made, that it made it,
        // and the absent statistics, all on stdout. Under `--json` that is four
        // lines of prose in front of the payload, and the mnemonic is in that
        // payload, so a caller piping this to a file would get neither valid
        // JSON nor their only copy of the recovery phrase.
        let (wallet, mnemonic) = {
            let _silence = gag::Gag::stdout().ok();
            rt
            .block_on(WalletCore::new_init_storage(
                config,
                storage,
                statistics,
                overrides,
                // See the doc comment: upstream discards this.
                "",
            ))
            .map_err(|e| format!("create wallet: {e}"))?
        };
        let mut client =
            Self { wallet, rt, progress: None, home, unlinkable_launch_creator: None };
        // `new_init_storage` builds the wallet in memory only; without this the
        // command would report success and leave no file behind.
        client.persist()?;
        tune_fresh_config(&config_file)?;
        Ok((client, mnemonic.to_string()))
    }

    /// The one-element sequencer list a `--network` choice becomes.
    ///
    /// v0.2.1+ replaced the single `sequencer_addr` with a list, so the override
    /// sets a list rather than a scalar. Shared by [`Self::open`] and
    /// [`Self::create`] so the two cannot drift on what a network means.
    fn network_overrides(
        network: Option<&Network>,
    ) -> Result<Option<wallet::config::WalletConfigOverrides>> {
        let Some(n) = network else { return Ok(None) };
        let url = n
            .url()
            .parse()
            .map_err(|e| format!("invalid sequencer URL for network {n}: {e}"))?;
        Ok(Some(wallet::config::WalletConfigOverrides {
            sequencers: Some(vec![wallet::config::SequencerConnectionData {
                sequencer_addr: url,
                basic_auth: None,
            }]),
            ..Default::default()
        }))
    }

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
        let overrides = Self::network_overrides(network)?;
        // Async since v0.2.1: opening may calibrate the configured sequencers.
        // Taken BEFORE `config` is moved into the wallet.
        let home = config.parent().map(Path::to_path_buf);
        let wallet = rt
            .block_on(WalletCore::new_update_chain(config, storage, statistics, overrides))
            .map_err(|e| format!("open wallet: {e}"))?;
        Ok(Self { wallet, rt, progress: None, home, unlinkable_launch_creator: None })
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
    ///
    /// # Bounded
    ///
    /// This used to be a bare `block_on` with no deadline of any kind, and a
    /// live run once spent 5h33m inside it at 0% CPU. **Which await hung was
    /// never established**, and that is the point: the bound is on the whole
    /// call rather than on any one await inside it, so it holds without a
    /// diagnosis. The obvious suspect does not even survive scrutiny - the block
    /// stream's requests go through a jsonrpsee HTTP client whose builder
    /// default is a 60s request timeout, so a dead socket surfaces as an error,
    /// not a wedge. A bound placed on the await we guessed at would have been a
    /// bound resting on a guess.
    ///
    /// # What this does NOT bound
    ///
    /// Synchronous work inside the future. `tokio::time::timeout` fires only
    /// when the task yields, so anything that blocks the thread without an
    /// `.await` is not preemptible by it. `sync_to_block` does exactly that
    /// once per block: `store_persistent_data` serialises the whole wallet
    /// storage and writes it, synchronously, inside the async task. A write
    /// that never returns - a stalled disk, an unresponsive network mount -
    /// would still hang here, and would look like the 5h33m symptom (0% CPU) as
    /// closely as anything else. Bounding that needs the sync moved off the
    /// runtime thread entirely, which is not something this wrapper can do
    /// while it holds `&mut wallet`. Said plainly rather than left for the next
    /// person to rediscover at hour five.
    ///
    /// It is now run in [`DEFAULT_SYNC_STALL_SECS`] windows, and only a window
    /// that scans *no block at all* - or a call that outlives
    /// [`DEFAULT_SYNC_MAX_SECS`] - fails. Both bounds are overridable; see
    /// `$LPAD_SYNC_STALL_SECS` and `$LPAD_SYNC_MAX_SECS`.
    ///
    /// Cutting a window short is safe, and that is a property of the wallet, not
    /// an assumption: the only `.await` in `sync_to_block`'s per-block loop is
    /// the one that pulls the next block, and `set_last_synced_block` +
    /// `store_persistent_data` both run synchronously before it. So a cancelled
    /// window is always cancelled *between* blocks, with every scanned block
    /// already persisted - the next window resumes from `last_synced_block + 1`
    /// and rescans nothing. Dropping the stream is in fact the repair: a fresh
    /// window opens a fresh connection, which is what a wedged one needs.
    ///
    /// Failing here is also always clean on chain. Every caller syncs as its
    /// first act, before anything is quoted, proved, signed or submitted, so a
    /// stalled sync costs the command and nothing else.
    pub fn sync_private(&mut self) -> Result<()> {
        let stall = std::time::Duration::from_secs(secs_env("LPAD_SYNC_STALL_SECS", DEFAULT_SYNC_STALL_SECS));
        let budget = std::time::Duration::from_secs(secs_env("LPAD_SYNC_MAX_SECS", DEFAULT_SYNC_MAX_SECS));
        let started = std::time::Instant::now();
        let origin = self.wallet.storage().last_synced_block();

        loop {
            let before = self.wallet.storage().last_synced_block();
            let outcome = {
                let rt = &self.rt;
                let wallet = &mut self.wallet;
                // v0.2.1's sync_to_block prints per-block progress AND persists
                // (which prints again) on every block, so the gag matters more
                // than it did on rc4. `sync_to_latest_block` folds the old
                // block_height + sync_to_block pair into one call.
                let _silence = gag::Gag::stdout().ok();
                rt.block_on(async {
                    tokio::time::timeout(stall, wallet.sync_to_latest_block()).await
                })
            };
            match outcome {
                Ok(Ok(_head)) => return Ok(()),
                Ok(Err(e)) => return Err(format!("sync private view: {e}")),
                Err(_window_elapsed) => {
                    let after = self.wallet.storage().last_synced_block();
                    match sync_step(before, after, started.elapsed(), budget) {
                        SyncStep::Retry => self.report("syncing private view"),
                        SyncStep::Stalled => {
                            return Err(format!(
                                "sync private view: the sequencer sent no block for {}s. That is a \
                                 wedged sync, not a slow one - an earlier build had no bound here \
                                 at all and sat in this call for 5h33m at 0% CPU. The wallet is \
                                 synced to block {after} \
                                 and every scanned block is already persisted, so nothing was lost \
                                 and nothing was submitted: re-run to resume from there. If this \
                                 sequencer really is that slow, widen the window with \
                                 LPAD_SYNC_STALL_SECS=<seconds>.",
                                stall.as_secs()
                            ))
                        }
                        SyncStep::Exhausted => {
                            return Err(format!(
                                "sync private view: still short of chain head after {}s (scanned \
                                 blocks {origin} -> {after}). The stream is alive - this chain just \
                                 produces blocks faster than this wallet scans them. Every scanned \
                                 block is persisted and nothing was submitted, so re-run to continue \
                                 from block {after}, or raise the budget with \
                                 LPAD_SYNC_MAX_SECS=<seconds>.",
                                budget.as_secs()
                            ))
                        }
                    }
                }
            }
        }
    }

    // ---- low-level submit -------------------------------------------------

    fn submit_public<I: Serialize>(
        &self,
        program_id: ProgramId,
        account_ids: Vec<AccountId>,
        signers: &[AccountId],
        instruction: I,
    ) -> Result<TxHash> {
        // Before the nonces are even fetched: a transaction that would
        // deanonymise an in-flight private launch is never built, let alone
        // signed or paid for.
        self.refuse_deanonymising(&account_ids)?;
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

    /// Read a bonding-curve sale's on-chain state. Owner-pinned; see
    /// [`decode_bc_sale`] for why decoding the bytes is not enough.
    pub fn bc_sale(&self, sale: AccountId) -> Result<SaleState> {
        decode_bc_sale(&self.read_account(sale)?, bc_program_id()?)
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

    /// Read an LBP pool's on-chain state. Owner-pinned; see
    /// [`decode_lbp_pool`].
    pub fn lbp_pool(&self, pool: AccountId) -> Result<PoolState> {
        decode_lbp_pool(&self.read_account(pool)?, lbp_program_id()?)
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

    /// Fail-fast guard for every private path: the shielded source must be a
    /// holding this wallet knows, of `expect_definition`, holding at least
    /// `amount` - all checked *before* we begin a multi-minute privacy proof.
    /// Read-only; returns a clean error. (Callers sync the private view first, so
    /// the balance is fresh.)
    ///
    /// The DEFINITION check is the load-bearing half. This call is the only look
    /// any private path takes at the note it is about to spend, and the guest's
    /// matching assert - "buyer collateral token does not match the sale's
    /// collateral definition" - is the vault-poisoning guard, not a formality.
    /// Nothing else catches a mismatch on the way in: the CLI filters by
    /// definition only when it *auto-resolves* the holding, so an explicit
    /// `--user-collateral <id>` arrives here unchecked. Without this, naming a
    /// shielded holding of the wrong token buys a full multi-minute STARK and
    /// then a `ProgramProveFailed` with the guest panic buried inside it.
    ///
    /// The balance half is the same bargain for a cheaper mistake: an over-large
    /// request would otherwise surface only when the deshield (or the disposable
    /// proof) fails, after the user has paid for the attempt.
    fn ensure_shielded_covers(
        &self,
        source: AccountId,
        amount: u128,
        expect_definition: AccountId,
    ) -> Result<()> {
        let Some((definition, bal)) = self.private_balance(source) else {
            return Err("spend source is not a known shielded holding in this wallet (sync first)".into());
        };
        if definition != expect_definition {
            return Err(format!(
                "that shielded holding is of token definition {}, but this operation spends {} - \
                 the guest would reject it, and on a private path that verdict costs a full \
                 multi-minute proof. Pass a shielded holding of the right token (`lpad my-balance` \
                 lists them), or omit the flag and let the wallet pick one",
                hex_id(definition),
                hex_id(expect_definition)
            ));
        }
        if bal < amount {
            return Err(format!(
                "shielded balance {bal} does not cover the {amount} requested (sync the wallet or use a smaller amount)"
            ));
        }
        Ok(())
    }

    /// RFP Privacy #3 for the `BuyDisposable` paths: a slot the caller asked to
    /// be private must really be a shielded account this wallet owns.
    ///
    /// Without this, a public holding id passed here would be labelled
    /// `PrivateOwned`, and the failure would surface as a bare `KeyNotFoundError`
    /// from inside the prover - or, if the wallet happened to know the account,
    /// as a "private" buy whose holdings were never private at all.
    fn ensure_private_slot(&self, account: AccountId, role: &str) -> Result<()> {
        if self.wallet.get_account_private(account).is_none() {
            return Err(format!(
                "the {role} holding is not a private (shielded) account in this wallet - a \
                 disposable buy needs BOTH buyer slots shielded (RFP Privacy #3). Run `lpad \
                 shield --token-def <def> --amount <n>` to create one, then retry"
            ));
        }
        Ok(())
    }

    /// Reject, client-side and by name, a treasury that could never receive its
    /// fee - before a transaction is spent finding out.
    ///
    /// No longer the only defence, and this doc used to say it was. The treasury
    /// is now a top-level ACCOUNT of `CreateSale` on both programs, and both
    /// guests pin it there: it must already be an initialised `Fungible` holding
    /// of the sale's collateral definition, under that definition's own token
    /// program. A sale whose treasury fails any of that cannot be created at
    /// all, so the state this check used to be the last thing standing between a
    /// creator and is now unreachable.
    ///
    /// It stays because it FIRES FIRST and because it can say things a guest
    /// assert structurally cannot: which definition the treasury actually holds,
    /// which one this sale needs, and the exact command that fixes it. The guest
    /// sees one account and one expectation; this sees both accounts and the
    /// wallet. Its failure mode is also cheaper - an error before anything is
    /// signed, rather than a rejected transaction. So keep it ahead of every
    /// other read in both `create_sale` builders; a check that runs after the
    /// chain has already answered explains nothing.
    ///
    /// Run unconditionally on BOTH programs, `fee_bps == 0` included. The LBP
    /// used to gate it on the fee, reasoning that a zero-fee pool never names
    /// its treasury; that stopped being true the moment the treasury became a
    /// `CreateSale` account, and the gate would hand exactly the zero-fee
    /// creator the bare assert this function exists to pre-empt.
    ///
    /// What makes it worth failing loudly: `treasury_id` is immutable once the
    /// sale exists. Both programs escrow the protocol fee and settle it in a
    /// separate, permissionless `SweepTreasury`, so a treasury that cannot
    /// receive costs the accrued fee for good - and on the bonding curve, which
    /// declares the treasury on every public trade whatever the fee is, it costs
    /// the tradability of the sale with it.
    fn check_treasury_target(&self, treasury: AccountId, collateral_def: AccountId) -> Result<()> {
        let acc = self
            .read_account(treasury)
            .map_err(|e| format!("cannot read the treasury account: {e}"))?;
        match token_core::TokenHolding::try_from(&acc.data) {
            Ok(token_core::TokenHolding::Fungible { definition_id, .. })
                if definition_id == collateral_def => {}
            Ok(token_core::TokenHolding::Fungible { definition_id, .. }) => {
                return Err(format!(
                    "the treasury holds token definition {}, but this sale's fees are paid in {} - \
                     a transfer between two definitions reverts, so the fee could never be paid, \
                     and `create_sale` rejects the sale on chain for the same reason. Run `lpad \
                     init-holding --token-def {}`, pass the id it prints as the treasury, and \
                     retry",
                    hex_id(definition_id),
                    hex_id(collateral_def),
                    hex_id(collateral_def)
                ));
            }
            _ => {
                return Err(format!(
                    "the treasury account {} is not an initialised fungible token holding. The \
                     treasury has to EXIST BEFORE the sale is created - both programs check it \
                     on chain and reject the create - and it can never be changed afterwards. \
                     Run `lpad init-holding --token-def {}`, pass the id it prints as the \
                     treasury, and retry",
                    hex_id(treasury),
                    hex_id(collateral_def)
                ));
            }
        }
        // Matching definitions are not enough. The fee transfer is dispatched to
        // the program that owns the *vault*, and the runtime rejects that program
        // writing a holding it does not own, so a treasury under some other token
        // program fails as an opaque framework rejection. Anchor on the definition
        // account's owner: of everything involved here, it is the one thing the
        // creator cannot choose.
        let def = self
            .read_account(collateral_def)
            .map_err(|e| format!("cannot read the collateral definition account: {e}"))?;
        if acc.program_owner != def.program_owner {
            return Err(format!(
                "the treasury account {} is not owned by the collateral token's own program, so a \
                 fee transfer to it would be rejected by the runtime - and `create_sale` rejects \
                 the sale on chain for the same reason. Make the treasury with `lpad \
                 init-holding --token-def {}` and retry",
                hex_id(treasury),
                hex_id(collateral_def)
            ));
        }
        Ok(())
    }

    /// The signer set a `SweepTreasury` needs. Always empty - and the point of
    /// this function is now the ERROR it returns instead.
    ///
    /// It used to return `[treasury]` for a pristine, DEFAULT-owned treasury the
    /// wallet held a key for, on the theory that the treasury's own signature
    /// was the one thing that could bootstrap an uninitialised treasury into
    /// existence. Both guests removed that possibility: `sweep_treasury` now
    /// asserts `treasury.program_owner != DEFAULT_PROGRAM_ID` before it looks at
    /// any authorization at all (`bonding_curve/src/lifecycle.rs`,
    /// `lbp/src/lifecycle.rs`, pinned by the unit test
    /// `sweep_rejects_an_unowned_treasury_even_when_it_signs_for_itself`).
    /// Collecting that signature therefore bought the operator nothing but a
    /// paid-for transaction that reverts, under an error message telling them to
    /// retry from a different wallet - a remedy that cannot work.
    ///
    /// The state is unreachable anyway: `CreateSale` pins the treasury as an
    /// already-initialised holding, an initialised holding is owned by the token
    /// program, and LEZ has no instruction that un-initialises one (`burn` only
    /// lowers a balance). So a DEFAULT-owned treasury under a live sale means
    /// chain state disagrees with the sale that named it, and the honest answer
    /// is to say so here rather than to spend a transaction discovering it.
    ///
    /// The instruction itself stays permissionless: anyone can settle a live
    /// treasury, which is why the ordinary path needs no signer.
    fn sweep_treasury_signers(&self, treasury: AccountId) -> Result<Vec<AccountId>> {
        sweep_treasury_signers_for(treasury, &self.read_account(treasury)?)
    }

    /// Refuse to submit a transaction that names the real creator of an
    /// unlinkable launch that is still in flight. No-op at every other time.
    fn refuse_deanonymising(&self, accounts: &[AccountId]) -> Result<()> {
        match deanonymising_account(accounts, self.unlinkable_launch_creator) {
            None => Ok(()),
            Some(id) => Err(format!(
                "refusing to submit: this transaction names {}, the real account behind the \
                 unlinkable launch in progress, so publishing it would tie that launch to this \
                 wallet in cleartext chain data. Nothing was submitted and nothing has been \
                 deanonymised - but the launch is incomplete. This is a bug in lpad: report it \
                 with the command you ran",
                hex_id(id)
            )),
        }
    }

    /// [`ensure_creator_settleable_for`] against the chain's view of `creator`.
    /// One read, before anything is signed - see that function for what it buys.
    fn ensure_creator_settleable(&self, creator: AccountId) -> Result<()> {
        let acc = self
            .read_account(creator)
            .map_err(|e| format!("cannot read the creator account: {e}"))?;
        ensure_creator_settleable_for(creator, &acc)
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
        let ttl = ttl_ms("LPAD_TX_TTL_MS", DEFAULT_TX_TTL_MS);
        Ok(self.clock_now_ms()?.saturating_add(ttl))
    }

    /// The one clock read a `BuyDisposable` is built from - see
    /// [`PrivateBuyTime`] for why the price timestamp and the deadline must come
    /// from the same read. The TTL defaults to 40 minutes and is overridable via
    /// `$LPAD_PRIVATE_TX_TTL_MS` - a separate knob from `$LPAD_TX_TTL_MS`,
    /// because a locally-proved transaction and a sequencer-proved one have
    /// wildly different latencies.
    ///
    /// Fails closed on an unreadable clock, exactly as [`Self::tx_deadline`]
    /// does: on this path an unbounded window would additionally mean an
    /// unbounded *lower* bound, i.e. a proof priced at a timestamp nothing
    /// relates to real time.
    pub fn private_buy_time(&self) -> Result<PrivateBuyTime> {
        let at_ms = self.clock_now_ms()?;
        let ttl = ttl_ms("LPAD_PRIVATE_TX_TTL_MS", DEFAULT_PRIVATE_TX_TTL_MS);
        Ok(PrivateBuyTime { at_ms, deadline: at_ms.saturating_add(ttl) })
    }

    /// The on-chain clock as a `u64`, or an error. The fail-closed half of
    /// [`Self::tx_deadline`] and [`Self::private_buy_time`], shared so neither
    /// can grow a "just use 0" fallback: a zero timestamp there means an
    /// unbounded validity window.
    fn clock_now_ms(&self) -> Result<u64> {
        match self.now_ms() {
            n if n > 0 => Ok(n as u64),
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

    /// One pass over every account the wallet knows about, reading each PUBLIC
    /// account exactly ONCE.
    ///
    /// `None` means "the read failed", i.e. UNKNOWN - never "known to be X". No
    /// caller may treat an unreadable account as ruled out, because a flaky
    /// sequencer would then silently narrow the search.
    ///
    /// This exists so a single `my-sales`/`my-pools` pays for the account reads
    /// once and gets both the holdings (token definitions) and the account
    /// owners (creator prune) out of them - the prune below therefore costs no
    /// extra RPC at all.
    fn wallet_account_scan(&self) -> Vec<WalletAccount> {
        self.wallet_public_accounts()
            .into_iter()
            .map(|id| (id, false, self.read_account(id).ok()))
            .chain(
                self.wallet_private_accounts()
                    .into_iter()
                    .map(|id| (id, true, self.wallet.get_account_private(id))),
            )
            .collect()
    }

    /// Every wallet-owned fungible holding (public + shielded), with token name +
    /// wallet label resolved. Drives `my-balance` and holding auto-detection.
    pub fn my_holdings(&self) -> Result<Vec<Holding>> {
        Ok(self.holdings_from(&self.wallet_account_scan()))
    }

    /// [`Self::my_holdings`] over an already-read scan, so discovery does not
    /// re-read every account to learn the same thing twice.
    fn holdings_from(&self, scan: &[WalletAccount]) -> Vec<Holding> {
        let mut names: HashMap<AccountId, Option<String>> = HashMap::new();
        let mut out: Vec<Holding> = Vec::new();
        for &(id, private, ref acc) in scan {
            let Some(acc) = acc.as_ref() else { continue };
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
        out
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

    /// Bonding-curve sales this wallet created: for each of your accounts, the
    /// sale ids the bonding-curve program recorded in that account's on-chain
    /// creator index. (Listing OTHER people's sales still needs the LP-0012
    /// event indexer, which does not exist here.)
    pub fn my_sales(&self) -> Result<Listing<SaleState>> {
        self.my_sales_with(Discovery::default())
    }

    /// [`Self::my_sales`] with the knobs. See [`Discovery`], and [`Listing`] for
    /// why the ids this could not read come back rather than being dropped.
    pub fn my_sales_with(&self, opts: Discovery) -> Result<Listing<SaleState>> {
        let program = bc_program_id()?;
        self.discover(
            DISCOVERY_KIND_BC,
            program,
            opts,
            |creator| self.bc_creator_index(program, creator),
            |td, cd, creator, nonce| bonding_curve_core::compute_sale_pda(program, td, cd, creator, nonce),
            |id| self.bc_sale(id),
        )
    }

    /// LBP pools this wallet created (same scheme as [`Self::my_sales`], against
    /// the LBP program's own per-creator index).
    pub fn my_pools(&self) -> Result<Listing<PoolState>> {
        self.my_pools_with(Discovery::default())
    }

    /// [`Self::my_pools`] with the knobs. See [`Discovery`] and [`Listing`].
    pub fn my_pools_with(&self, opts: Discovery) -> Result<Listing<PoolState>> {
        let program = lbp_program_id()?;
        self.discover(
            DISCOVERY_KIND_LBP,
            program,
            opts,
            |creator| self.lbp_creator_index(program, creator),
            |td, cd, creator, nonce| lbp_core::compute_pool_pda(program, td, cd, creator, nonce),
            |id| self.lbp_pool(id),
        )
    }

    /// One read of one creator's bonding-curve index. See
    /// [`CreatorIndexLookup`] for what each answer commits the caller to.
    fn bc_creator_index(&self, program: ProgramId, creator: AccountId) -> CreatorIndexLookup {
        let pda = bonding_curve_core::compute_creator_index_pda(program, creator);
        classify_creator_index(program, creator, self.read_account(pda), |data| {
            bonding_curve_core::CreatorIndex::try_from(data)
                .ok()
                .map(|index| (index.creator, index.sale_ids))
        })
    }

    /// [`Self::bc_creator_index`] for the LBP program. The two indexes are
    /// separate accounts under separate program ids and carry different magic
    /// (`lpad/bci` vs `lpad/lbi`), so neither can be mistaken for the other.
    fn lbp_creator_index(&self, program: ProgramId, creator: AccountId) -> CreatorIndexLookup {
        let pda = lbp_core::compute_creator_index_pda(program, creator);
        classify_creator_index(program, creator, self.read_account(pda), |data| {
            lbp_core::CreatorIndex::try_from(data).ok().map(|index| (index.creator, index.pool_ids))
        })
    }

    /// The one discovery engine behind `my-sales` and `my-pools`.
    ///
    /// The two commands are the same search over two PDA functions, and they
    /// have already drifted apart in this repo once when only one of them was
    /// changed. Sharing the body is what stops that: `index_of` reads one
    /// creator's on-chain index, `pda` derives a speculative candidate id,
    /// `read` decodes one, and everything else - the prune, the plan, the cache,
    /// the append-only merge - is written once.
    ///
    /// **There are two discovery paths here, and only one of them is the fast
    /// one. Both have to stay.**
    ///
    ///  * THE INDEX (default, and how essentially every run should finish).
    ///    Both guests append each sale/pool id to a per-creator index account -
    ///    `compute_creator_index_pda(program, creator)` - inside the very
    ///    transaction that creates it. So discovery is one read per creator
    ///    candidate, and the answer is complete: roughly four reads on a real
    ///    wallet.
    ///
    ///  * THE DERIVATION (the fallback below, `--refresh`/`--deep`, and any
    ///    creator whose index read FAILED). Nothing on chain recorded what was
    ///    created before the index existed, so the only way to find a sale from
    ///    an older lpad build is to re-derive every PDA the wallet could
    ///    possibly have made - every creator x every definition pair x
    ///    `0..DISCOVERY_NONCE_MAX` - and read each one. That is thousands of
    ///    sequential round-trips and tens of minutes; it is exactly what the
    ///    index was added to stop paying on every listing.
    ///
    /// **Do not delete the derivation as dead code.** It looks unreachable and
    /// is not. It is what a `--refresh` runs, it is what covers a creator whose
    /// index read failed rather than reporting that creator has no sales, and
    /// it is the only thing that can find a pre-index sale at all. What made it
    /// intolerable was running it *by default*, on every listing, forever - not
    /// its existence.
    ///
    /// Whatever either path turns up is merged into the local id cache
    /// append-only, so a pre-index sale found once by a `--refresh` keeps
    /// showing up on plain runs afterwards.
    ///
    /// **A failed read is only silent when the id was a guess.** The two paths
    /// disagree about what `Err` from `read` means - "not a sale" for a derived
    /// candidate, "the sequencer did not answer" for one the chain's index
    /// named - so [`resolve_candidates`] splits them and this warns on stderr
    /// for every authoritative id it could not read. Under-reporting is the one
    /// direction that costs here: a creator reads a short listing as "my sale
    /// is gone", and the index exists precisely to stop that.
    fn discover<S>(
        &self,
        kind: &str,
        program: ProgramId,
        opts: Discovery,
        index_of: impl Fn(AccountId) -> CreatorIndexLookup,
        pda: impl Fn(AccountId, AccountId, AccountId, u64) -> AccountId,
        read: impl Fn(AccountId) -> Result<S>,
    ) -> Result<Listing<S>> {
        let path = self.discovery_cache_path();
        let mut cache = path.as_deref().map(load_discovery_cache).unwrap_or_default();
        let key = discovery_key(kind, program);
        let entry = cache.entries.get(&key).cloned().unwrap_or_default();

        // One pass over the wallet's own accounts. It names the creators whose
        // index gets read, and - only if something still needs deriving - the
        // token definitions the speculative PDAs are built from.
        let scan = self.wallet_account_scan();
        let creators: Vec<AccountId> = if opts.deep {
            scan.iter().filter(|&&(_, private, _)| !private).map(|&(id, _, _)| id).collect()
        } else {
            creator_candidates(&public_owners(&scan), &creator_ineligible_owners())
        };

        let plan = plan_discovery(&creators, opts.refresh || opts.deep, index_of);
        let mut seen: HashSet<AccountId> = HashSet::new();
        let mut candidates: Vec<AccountId> = Vec::new();
        // The ids that are NOT guesses. Everything the index named and
        // everything a past run remembered belongs here, and a failed read on
        // one of them is reported rather than dropped - see
        // [`resolve_candidates`].
        let mut authoritative: HashSet<AccountId> = HashSet::new();
        for id in plan.indexed {
            authoritative.insert(id);
            if seen.insert(id) {
                candidates.push(id);
            }
        }

        // The fallback. `plan.derive` is empty on a healthy default run, which
        // is what turns thousands of reads into a handful; when it is not empty
        // it is because a flag asked for this or an index read did not answer.
        if !plan.derive.is_empty() {
            let defs = definitions_of(&self.holdings_from(&scan));
            for &creator in &plan.derive {
                for &td in &defs {
                    for &cd in &defs {
                        if td == cd {
                            continue;
                        }
                        for nonce in 0..DISCOVERY_NONCE_MAX {
                            let id = pda(td, cd, creator, nonce);
                            if seen.insert(id) {
                                candidates.push(id);
                            }
                        }
                    }
                }
            }
        }

        // Remembered ids are always probed, on top of both paths. They are the
        // safety net for the two cases neither path covers on its own: a
        // pre-index sale a past `--refresh` already paid to find, and a sale
        // this wallet created while its index read was failing.
        for id in cached_ids(&entry) {
            authoritative.insert(id);
            if seen.insert(id) {
                candidates.push(id);
            }
        }

        let (found, unreadable) = resolve_candidates(candidates, &authoritative, read);
        // Say it, every time, one line per id. This listing is the answer to
        // "did my sale land?", and a short listing that exits 0 in silence
        // answers it wrongly. stderr, so a `--json` stdout stays parseable.
        // Returned as well as printed: `--json` consumers cannot see stderr, and
        // the incompleteness is the part a script must branch on. See
        // [`Listing`].
        let noun = discovery_noun(kind);
        for (id, e) in &unreadable {
            eprintln!(
                "warning: the chain lists {noun} {} for this wallet but its state could not be \
                 read ({e}) - THIS LISTING IS INCOMPLETE, do not read it as \"that {noun} is \
                 gone\". Retry; if it persists, the sequencer is not serving that account",
                hex_id(*id)
            );
        }

        // Persist APPEND-ONLY: a PDA that resolved once resolves forever, so a
        // read that failed this time is far likelier to be a flaky sequencer
        // than a vanished sale. Never evict on a failed probe.
        //
        // The unreadable AUTHORITATIVE ids are remembered too, which is a
        // stronger claim than "found": the chain's index named them, so they
        // exist whatever this run could read. Without this, a listing run while
        // BOTH the state read and the next index read are failing would have no
        // record of them left at all.
        let mut found_ids: Vec<String> = found.iter().map(|(id, _)| hex_id(*id)).collect();
        found_ids.extend(unreadable.iter().map(|(id, _)| hex_id(*id)));
        let updated = CacheEntry { ids: merged_ids(&found_ids, &entry.ids) };
        if let Some(path) = &path
            && updated != entry
        {
            cache.version = DISCOVERY_CACHE_VERSION;
            cache.entries.insert(key, updated);
            store_discovery_cache(path, &cache);
        }
        Ok(Listing { found, unreadable })
    }

    /// Where this wallet's discovery cache lives (see [`LaunchpadClient::home`]).
    fn discovery_cache_path(&self) -> Option<PathBuf> {
        self.home.as_ref().map(|h| h.join(DISCOVERY_CACHE_FILE))
    }

    /// Remember a sale/pool id the moment it is created.
    ///
    /// The chain already knows - the same transaction appended this id to the
    /// creator's index - so on a healthy run this is redundant, and that is
    /// fine. It costs one small local write and it is the only record left if
    /// the index read is failing when the next listing runs, which is exactly
    /// when a creator most wants to see the sale they just paid for.
    /// Best-effort: a cache that cannot be written is a slower next run, never
    /// a failed create.
    fn remember_discovery(&self, kind: &str, program: ProgramId, id: AccountId) {
        let Some(path) = self.discovery_cache_path() else { return };
        let mut cache = load_discovery_cache(&path);
        cache.version = DISCOVERY_CACHE_VERSION;
        let hex = hex_id(id);
        let entry = cache.entries.entry(discovery_key(kind, program)).or_default();
        if entry.ids.contains(&hex) {
            return;
        }
        entry.ids.push(hex);
        store_discovery_cache(&path, &cache);
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
    ///
    /// It is a PUBLIC transaction and it publishes both of those: the pair
    /// `(payer, reference_token_def)` is on chain in cleartext for anyone who
    /// scans for `Initialize`. Harmless for the bootstrap it is meant for, fatal
    /// for a private launch - see [`wlez_bootstrap`], which is the gate every
    /// launch path asks before calling this.
    pub fn initialize_wlez(
        &self,
        wlez_program: ProgramId,
        reference_token_def: AccountId,
        payer: AccountId,
    ) -> Result<TxHash> {
        self.report("initializing wlez");
        let tx = self.submit_public(
            wlez_program,
            wlez_initialize_accounts(wlez_program, reference_token_def, payer),
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
        )?;
        // Verify what actually landed, here, rather than leaving it to the first
        // `wrap`. `Initialize` is permissionless and claim-once, so an observer
        // watching the deployment can land their own first and pin a token or
        // native program of their choosing; every check above is on OUR
        // instruction, not on the resulting chain state. Discovering that at
        // bootstrap - while the operator is still at the keyboard and nothing has
        // been escrowed - is worth one extra read.
        self.wlez_programs_checked(wlez_program).map_err(|e| {
            format!(
                "wlez initialization did not produce the canonical pins: {e}\n\
                 This wlez program id has been claimed with pins that are not ours - most \
                 likely front-run between deploy and initialize. It cannot be re-initialized: \
                 the guest's idempotency branch requires the canonical token program, and the \
                 vault PDA is no longer pristine. Rebuild wlez under a different image id and \
                 redeploy. No funds are at risk - nothing has been wrapped."
            )
        })?;
        Ok(tx)
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
    /// downgrades the front-run from a drain to a DoS - but not a cheaply
    /// recoverable one: a claimed wlez id cannot be re-initialized (the guest's
    /// idempotency branch demands the canonical token program, and the vault PDA
    /// is no longer pristine), so recovery means rebuilding wlez under a
    /// different image id. `initialize_wlez` therefore runs this check
    /// immediately after initializing, so the race is caught at bootstrap.
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
    /// bootstrapped here if this chain has not had it done yet, and left alone
    /// otherwise. See [`TokenLaunch`] for what comes back.
    pub fn bc_create_token_sale(&mut self, t: TokenSaleArgs) -> Result<TokenLaunch> {
        let (proj_def, proj_holding) =
            self.mint_token_with_metadata(&t.name, &t.symbol, t.total_supply)?;
        // Probe, then bootstrap only if it is really needed. The GUEST is
        // idempotent, the client was not: it re-sent `Initialize` on every
        // launch, which cost the creator a transaction each time. A public
        // launch may pay for the bootstrap when the chain does need it - its
        // creator is named on chain anyway - which is the whole difference
        // between this arm and the private one. See [`wlez_bootstrap`].
        if wlez_bootstrap(LaunchLinkage::Public, self.wlez_programs_checked(t.wlez_program))?
            == WlezBootstrap::Needed
        {
            self.report("initializing wlez (one-time bootstrap)");
            self.initialize_wlez(t.wlez_program, proj_def, t.creator)?;
        }
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
        Ok(TokenLaunch {
            sale,
            token_definition: proj_def,
            creator: t.creator,
            creator_token_holding: proj_holding,
            tx,
        })
    }

    /// Private launch: like [`bc_create_token_sale`] but the on-chain `creator`
    /// is a fresh, unlinkable account A funded by deshielding the deposit from a
    /// shielded holding (mint → shield deposit → deshield into A → create-sale).
    /// An observer can't tie the sale to the real minter.
    ///
    /// A and its token holding are RETURNED, in [`TokenLaunch`], and the caller
    /// has to keep them: they are the only accounts `bc close` and `bc withdraw`
    /// will accept for this sale, the wallet holds their keys but not their
    /// meaning, and nothing on chain links them back to this launch - which is
    /// the point of the launch and, without this, also the way to lose the raise.
    ///
    /// `t.creator` is deliberately NOT used by this path - it is the wallet's
    /// real, funded account, and naming it in any transaction of this launch
    /// would hand an observer the link the launch exists to prevent. It is
    /// carried in the args only so the tripwire below knows which id to refuse.
    ///
    /// Requires wlez to be initialised on this chain ALREADY: bootstrapping it
    /// is a public act that names its payer, so this path refuses rather than
    /// performing it (see [`wlez_bootstrap`]).
    pub fn bc_create_token_sale_private(&mut self, t: TokenSaleArgs) -> Result<TokenLaunch> {
        // Arm the tripwire around the whole launch, then disarm it whichever way
        // the launch ends. Auditing each leg is what failed here before: the
        // transaction that leaked was one nobody counted as part of the private
        // flow at all.
        self.unlinkable_launch_creator = Some(t.creator);
        let out = self.token_sale_unlinkable(t);
        self.unlinkable_launch_creator = None;
        out
    }

    /// The body of [`Self::bc_create_token_sale_private`], run with the
    /// deanonymisation tripwire armed.
    fn token_sale_unlinkable(&mut self, t: TokenSaleArgs) -> Result<TokenLaunch> {
        let deposit = t
            .sale_quantity
            .checked_add(t.dex_seed)
            .ok_or_else(|| "sale_quantity + dex_seed overflows u128".to_string())?;
        // FIRST, before a single transaction: this launch will not bootstrap
        // wlez, so if this chain's wlez is not ready there is no launch to make.
        // Answering it here costs one read; answering it after the mint and the
        // shield would cost a minted token, a multi-minute proof and a
        // half-built launch. The returned verdict can only be `Done` - the other
        // one is the Err this `?` propagates.
        wlez_bootstrap(LaunchLinkage::Unlinkable, self.wlez_programs_checked(t.wlez_program))?;
        let (proj_def, proj_holding) =
            self.mint_token_with_metadata(&t.name, &t.symbol, t.total_supply)?;
        // Shield the deposit so the funding source is private.
        self.report("shielding deposit");
        let shielded = self.new_private_account()?;
        self.shield(proj_holding, shielded, deposit)?;
        // wlez collateral + treasury.
        let wlez_def = wlez_core::get_wlez_definition_id(&t.wlez_program);
        let treasury = self.init_token_holding(wlez_def)?;
        // Fresh unlinkable creator A + token holding A; deshield the deposit into A_token.
        let (a_creator, _) = self.wallet.create_new_account_public(None);
        let (a_token, _) = self.wallet.create_new_account_public(None);
        self.persist()?;
        // A signs the create, and the close and the withdraw must both echo it,
        // so it has to be initialised BEFORE it signs anything - a signature
        // bumps the nonce of a DEFAULT-owned account and rule 7 then rejects
        // every transaction naming it, sale unclosable and raise locked (see
        // `ensure_creator_settleable_for`). Nothing else can do this for A
        // afterwards, which is why it happens here and not lazily: the claim
        // needs A's pre-state to be whole-default, and A claims itself in the
        // same transaction, so no funding and no other account is involved.
        // A_token needs no such step - the deshield below claims it under the
        // token program in the transaction that funds it.
        self.initialize_native_account(a_creator).map_err(|e| {
            format!(
                "could not initialise the unlinkable creator {}, and a creator that signs before \
                 it is initialised can never close its sale or withdraw from it - so the launch \
                 stops here rather than building one that is dead on arrival. Nothing about this \
                 launch has been published, and the deposit is still shielded in this wallet \
                 (`lpad sync`, then `lpad my-balance`). {e}",
                hex_id(a_creator)
            )
        })?;
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
        Ok(TokenLaunch {
            sale,
            token_definition: proj_def,
            creator: a_creator,
            creator_token_holding: a_token,
            tx,
        })
    }

    // ---- bonding curve: creator ------------------------------------------

    /// Create a bonding-curve sale. Returns `(sale_id, tx_hash)`.
    ///
    /// The treasury must ALREADY be an initialised fungible holding of
    /// `collateral_def` - the guest rejects the create otherwise. Nothing here
    /// creates it, deliberately: it is the sale's permanent fee sink, so which
    /// account it is has to be the caller's decision, not a side effect.
    /// [`Self::bc_create_token_sale`] makes one first (`init_token_holding`,
    /// which only returns once the holding is on chain) and passes it in.
    pub fn bc_create_sale(&self, a: BcCreateArgs) -> Result<(AccountId, TxHash)> {
        // FIRST, before any other read: `treasury_id` is immutable once the sale
        // exists, and the guest's own pin can only report that some account
        // failed some expectation. This names both definitions and the command
        // that fixes it. NOT gated on `fee_bps` - the guest checks the treasury
        // whatever the fee is, and the curve declares it on every public trade
        // besides. See `check_treasury_target`.
        self.check_treasury_target(a.treasury, a.collateral_def)?;
        // Then the creator, for the same reason and at the same cost: it is
        // named in the create and echoed by every settlement, and a creator that
        // cannot be echoed turns this sale into a permanent hole. One read, no
        // signature spent - see `ensure_creator_settleable_for`.
        self.ensure_creator_settleable(a.creator)?;
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
        // Ten accounts, ordered by `bc_create_sale_accounts` - see the note
        // there on why the ORDER is the part worth pinning.
        //
        // `treasury` is at index 3, the same slot it occupies in
        // `Buy`/`Sell`/`BuyAta`/`SellAta`, and is read-only: the guest names it
        // only to type-check it, then echoes it byte-for-byte. `token_def`
        // follows, ahead of the collateral definition, because the `D + R`
        // deposit leg - the one that claims and TYPES the token vault - is
        // dispatched to the program that owns THAT account, and the guest pins
        // it to the definition id read out of the creator's holding. The
        // holding's owner is data the creator controls; the definition account's
        // owner is not, so declaring the definition is what makes the vault's
        // owner chain-determined. Already computed above for the name/symbol
        // lookup. The `CreateSale` wire format is unchanged (an account list is
        // not part of it, and `treasury_id` is still carried in the
        // instruction), so only this vec moves. The last account before the
        // clock is the creator's index PDA, claimed on their first sale and
        // appended to here; it is what makes the sale this call creates
        // findable by one read instead of a derivation sweep.
        let accounts = bc_create_sale_accounts(sale, token_vault, collateral_vault, token_def, &a);
        let tx = self.submit_public(a.program, accounts, &[a.creator_token_holding, a.creator], instruction)?;
        // Belt and braces. The transaction above just appended this id to the
        // creator's on-chain index, which is what a later `my-sales` reads; this
        // is the local copy that still answers if that read fails.
        self.remember_discovery(DISCOVERY_KIND_BC, a.program, sale);
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
        // Six accounts, and deliberately NO treasury slot - passing one reverts,
        // because the guest destructures exactly six.
        //
        // `Withdraw` pays the creator `collateral_vault_balance -
        // treasury_owed` and leaves the escrowed protocol fee (the part
        // `BuyDisposable` could not sweep per trade, since a privacy proof cannot
        // pin a treasury account) sitting in the vault for
        // [`Self::bc_sweep_treasury`]. That split is the whole point: settling the
        // fee here would mean a treasury that cannot receive - uninitialised, of
        // the wrong definition, or under a different token program - reverts the
        // creator's payout with it and locks the entire raise, not just the fee.
        self.submit_public(
            program,
            vec![sale, s.token_vault_id, s.collateral_vault_id, creator_collateral, creator_token, creator],
            &[creator],
            bonding_curve_core::Instruction::Withdraw { deadline: self.tx_deadline()? },
        )
    }

    /// Settle the protocol fee `BuyDisposable` escrowed in the collateral vault
    /// (`SaleState::treasury_owed`) to the treasury pinned into the sale at
    /// creation. Accounts: `[sale, collateral_vault, treasury]`.
    ///
    /// **Permissionless, by construction**: three accounts and no signer slot.
    /// The only effect available to a submitter is moving the owed fee to the
    /// pinned `treasury_id`, so a stranger who sends this hands the fee to its
    /// rightful owner and pays the gas for the privilege - which is why no
    /// creator signature is asked for, and why the treasury cannot be redirected
    /// by whoever submits. Valid whether the sale is Open or Closed: the escrow
    /// accrues while the sale runs and the treasury should not have to wait on
    /// the creator to close.
    ///
    /// No exception, and there used to be one advertised here: a treasury that
    /// did not exist yet was said to be creatable by the fee leg if it signed for
    /// itself. `CreateSale` now pins an already-initialised holding as the
    /// treasury, and both guests `assert_ne!` on a DEFAULT-owned treasury before
    /// they look at anything else, so that path is gone - a signature cannot
    /// revive it. [`Self::sweep_treasury_signers`] returns an empty signer set
    /// for every treasury that can be swept at all, and an error naming the id
    /// for one that cannot.
    ///
    /// Nothing owed is an error rather than a no-op, matching the guest, and it
    /// is caught here so a pointless transaction is never signed or paid for.
    pub fn bc_sweep_treasury(&self, program: ProgramId, sale: AccountId) -> Result<TxHash> {
        let s = self.bc_sale(sale)?;
        if s.treasury_owed == 0 {
            return Err(
                "this sale owes the treasury nothing: only a private (disposable) buy escrows its \
                 fee in the vault, and every fee escrowed so far has already been swept. The \
                 public buy/sell paths pay the treasury in the same transaction, so they never \
                 leave anything for this to settle"
                    .into(),
            );
        }
        let signers = self.sweep_treasury_signers(s.treasury_id)?;
        self.submit_public(
            program,
            vec![sale, s.collateral_vault_id, s.treasury_id],
            &signers,
            bonding_curve_core::Instruction::SweepTreasury { deadline: self.tx_deadline()? },
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
    /// Private buy: the drift-free `deshield → public Buy → re-shield` saga (see
    /// [`bc_buy_atomic_disposable`]). The other private mode is
    /// [`Self::bc_buy_disposable`], one transaction instead of three; this one
    /// stays because it is the one proven against a real sequencer.
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
                spend_definition: s.collateral_definition_id,
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

    /// Private bonding-curve buy as ONE transaction: `BuyDisposable`, in which
    /// the buyer's collateral holding and token holding are PRIVATE account
    /// slots, so the collateral debit and the token credit are private notes
    /// inside a single proof.
    ///
    /// "Disposable" names the single-use NOTE, not an account: privacy on LEZ is
    /// a per-slot label on an otherwise ordinary buy, so there is no ephemeral
    /// account, no deshield leg and no re-shield leg. Nothing can be left
    /// half-done, because there is no "half": either the transaction lands or
    /// nothing happened.
    ///
    /// This is a SECOND private mode, not a replacement.
    /// [`Self::bc_buy_private`] (deshield, public buy, re-shield, sequenced
    /// across three transactions) stays, and remains the path that has actually
    /// been exercised against a real sequencer. The trade-off: the saga loses
    /// that one trade's privacy if the client dies between legs, while this
    /// proves the whole buy locally and so costs minutes of proving per
    /// operation.
    ///
    /// Accounts, in the exact order the guest destructures them: `[sale,
    /// token_vault, collateral_vault, buyer_collateral_holding (private),
    /// buyer_token_holding (private)]`. No clock and no treasury - see
    /// `bonding_curve_core::Instruction::BuyDisposable` for why neither can be
    /// pinned into a proof.
    #[allow(clippy::too_many_arguments)]
    pub fn bc_buy_disposable(
        &mut self,
        program: ProgramId,
        sale: AccountId,
        user_collateral: AccountId,
        user_token: AccountId,
        collateral_in: u128,
        min_tokens_out: u128,
        at: PrivateBuyTime,
    ) -> Result<TxHash> {
        // The wallet does not advance to chain head on its own. Spending against
        // a stale note view re-derives an output commitment that is already on
        // chain, which the sequencer rejects as a seen commitment - after the
        // proof has been paid for.
        self.sync_private()?;
        // Guard the no-op-drain pin BEFORE proving, exactly as the saga does: a
        // poisoned sale must fail in milliseconds, not after a STARK.
        let s = self.bc_sale_checked(sale)?;
        let prog = disposable_program(program, lpad_guests::bonding_curve(), "bonding-curve")?;
        // Both buyer slots must genuinely be shielded accounts we own, or this is
        // a public buy wearing a private label.
        self.ensure_private_slot(user_collateral, "collateral")?;
        self.ensure_private_slot(user_token, "token")?;
        // Fail fast on an over-large request rather than after a multi-minute
        // proof the guest will only revert.
        self.ensure_shielded_covers(user_collateral, collateral_in, s.collateral_definition_id)?;
        bc_disposable_preflight(&s, at.at_ms)?;

        self.report("private buy (one atomic proof, minutes)");
        self.send_privacy_tx(
            &prog,
            vec![
                AccountIdentity::Public(sale),
                AccountIdentity::Public(s.token_vault_id),
                AccountIdentity::Public(s.collateral_vault_id),
                AccountIdentity::PrivateOwned(user_collateral),
                AccountIdentity::PrivateOwned(user_token),
            ],
            // One entry per private slot, in slot position order.
            &[user_collateral, user_token],
            bonding_curve_core::Instruction::BuyDisposable {
                collateral_in,
                min_tokens_out,
                not_before_ms: at.at_ms,
                deadline: at.deadline,
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
                spend_definition: s.token_definition_id,
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
    ///
    /// As on the bonding curve, the treasury must ALREADY be an initialised
    /// fungible holding of `collateral_def`; the guest rejects the create
    /// otherwise, and nothing here creates one for the caller.
    pub fn lbp_create_sale(&self, a: LbpCreateArgs) -> Result<(AccountId, TxHash)> {
        // FIRST, before any other read, and NO LONGER gated on `fee_bps`. The
        // old gate reasoned that a zero-fee pool never names its treasury, which
        // was true while the treasury was only an id in the instruction. It is a
        // pinned account of `CreateSale` now and the guest type-checks it
        // whatever the fee is, so gating this would leave exactly the zero-fee
        // creator reading a guest assert instead of a fixable error. See
        // `check_treasury_target`.
        self.check_treasury_target(a.treasury, a.collateral_def)?;
        // Then the creator, for the same reason and at the same cost: it is
        // named in the create and echoed by every settlement, and a creator that
        // cannot be echoed turns this sale into a permanent hole. One read, no
        // signature spent - see `ensure_creator_settleable_for`.
        self.ensure_creator_settleable(a.creator)?;
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
        // Eleven accounts, ordered by `lbp_create_sale_accounts` - see the note
        // there on why the ORDER is the part worth pinning.
        //
        // BOTH definitions are declared, byte-for-byte where the bonding curve
        // puts them. Each vault-init leg (the project-token deposit, the
        // collateral seed) is dispatched to the program that owns the matching
        // DEFINITION account, not to the creator's holding: the holding's owner
        // is data the creator controls, the definition's is not, so declaring
        // both is what makes each vault's owner chain-determined instead of
        // creator-determined. `token_def` is the id the guest pins against the
        // creator's holding, and it is already computed above for the
        // name/symbol lookup. `treasury` is read-only - type-checked and echoed
        // unchanged - and sits after `creator` rather than beside the vaults,
        // which is where the LBP guest destructures it; the curve puts it at
        // index 3, and the two lists are NOT interchangeable. The creator's
        // index PDA is the last account before the clock, exactly as on the
        // curve. The `CreateSale` wire format is unchanged (an account list is
        // not part of it), so the instruction above needs no edit - only this
        // vec.
        let accounts = lbp_create_sale_accounts(pool, token_vault, collateral_vault, token_def, &a);
        let tx = self.submit_public(
            a.program,
            accounts,
            &[a.creator_token_holding, a.creator_collateral_holding, a.creator],
            instruction,
        )?;
        // See the note in `bc_create_sale`.
        self.remember_discovery(DISCOVERY_KIND_LBP, a.program, pool);
        Ok((pool, tx))
    }

    pub fn lbp_pause(&self, program: ProgramId, pool: AccountId, creator: AccountId) -> Result<TxHash> {
        self.submit_public(program, vec![pool, creator], &[creator], lbp_core::Instruction::Pause { deadline: self.tx_deadline()? })
    }
    pub fn lbp_resume(&self, program: ProgramId, pool: AccountId, creator: AccountId) -> Result<TxHash> {
        self.submit_public(program, vec![pool, creator], &[creator], lbp_core::Instruction::Resume { deadline: self.tx_deadline()? })
    }
    /// Advance the stored weight. It lives in a per-pool PDA rather than in the
    /// pool account, which is why this takes three accounts: `Poke` is
    /// permissionless, and a `BuyDisposable` proof pins the pool account
    /// byte-for-byte, so a poke that rewrote the pool would let anyone invalidate
    /// every in-flight private buy for free.
    pub fn lbp_poke(&self, program: ProgramId, pool: AccountId) -> Result<TxHash> {
        let weight_obs = lbp_core::compute_weight_obs_pda(program, pool);
        self.submit_public(program, vec![pool, weight_obs, lbp_core::CLOCK_01], &[], lbp_core::Instruction::Poke { deadline: self.tx_deadline()? })
    }
    pub fn lbp_close(&self, program: ProgramId, pool: AccountId, creator: AccountId) -> Result<TxHash> {
        self.submit_public(program, vec![pool, creator, lbp_core::CLOCK_01], &[creator], lbp_core::Instruction::CloseSale { deadline: self.tx_deadline()? })
    }
    /// Withdraw the raise (less the at-close fee) and every unsold token.
    ///
    /// Six accounts, and deliberately NO treasury slot - passing one reverts,
    /// because the guest destructures exactly six.
    ///
    /// `Withdraw` pays the creator `collateral_vault_balance - treasury_owed`
    /// and ESCROWS the at-close protocol fee in the vault for
    /// [`Self::lbp_sweep_treasury`] instead of transferring it here. That split
    /// is the whole point: as a leg of this instruction, a treasury that cannot
    /// receive - uninitialised, of the wrong definition, or under a different
    /// token program - reverted the creator's payout with it, and a closed pool
    /// has no other drain, so the entire raise AND every unsold token were locked
    /// forever by one immutable argument. Now the same dead treasury costs the
    /// fee alone.
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
            vec![pool, p.token_vault_id, p.collateral_vault_id, creator_collateral, creator_token, creator],
            &[creator],
            lbp_core::Instruction::Withdraw { deadline: self.tx_deadline()? },
        )
    }

    /// Settle the at-close protocol fee `Withdraw` escrowed in the collateral
    /// vault (`PoolState::treasury_owed`) to the treasury pinned into the pool at
    /// creation. Accounts: `[pool, collateral_vault, treasury]`.
    ///
    /// The mirror of [`Self::bc_sweep_treasury`], down to the signer rule (see
    /// [`Self::sweep_treasury_signers`]): permissionless, no creator slot, and
    /// the treasury cannot be redirected by whoever submits, because the
    /// destination is read out of the pool. Only the source of the escrow
    /// differs - here it is the fee `Withdraw` charges on the raise, so nothing
    /// is owed until the creator has closed and withdrawn.
    ///
    /// Nothing owed is an error rather than a no-op, matching the guest, and it
    /// is caught here so a pointless transaction is never signed or paid for.
    pub fn lbp_sweep_treasury(&self, program: ProgramId, pool: AccountId) -> Result<TxHash> {
        let p = self.lbp_pool(pool)?;
        if p.treasury_owed == 0 {
            return Err(
                "this pool owes the treasury nothing: the at-close fee is escrowed by Withdraw, \
                 so close the pool and withdraw first - or it has already been swept. A pool with \
                 fee_bps = 0, or one that never raised any collateral, never owes anything"
                    .into(),
            );
        }
        let signers = self.sweep_treasury_signers(p.treasury_id)?;
        self.submit_public(
            program,
            vec![pool, p.collateral_vault_id, p.treasury_id],
            &signers,
            lbp_core::Instruction::SweepTreasury { deadline: self.tx_deadline()? },
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

    /// Private LBP buy: the drift-free `deshield → public Buy → re-shield` saga.
    /// Its public buy leg prices off the live on-chain clock, so there is no
    /// `t_buy_ms` to choose. The other private mode is
    /// [`Self::lbp_buy_disposable`], one transaction instead of three; this one
    /// stays because it is the one proven against a real sequencer.
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
                spend_definition: p.collateral_definition_id,
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

    /// Private LBP buy as ONE transaction: `BuyDisposable`, in which the buyer's
    /// collateral holding and token holding are PRIVATE account slots, so the
    /// debit and the credit are private notes inside a single proof. See
    /// [`Self::bc_buy_disposable`] for what "disposable" does and does not mean;
    /// this is the same shape over the pool's account list.
    ///
    /// A SECOND private mode, not a replacement: [`Self::lbp_buy_private`]'s
    /// 3-transaction saga stays and is still the sequencer-proven one.
    ///
    /// The pool is priced at `at.at_ms`, NOT at inclusion time - a privacy
    /// transaction cannot carry the clock account, so the timestamp is a caller
    /// input bounded by the validity window rather than something read on chain.
    /// Callers must therefore quote their `min_tokens_out` floor at that same
    /// `at.at_ms`; quoting at some other "now" is how a private buy trips its own
    /// slippage floor. See [`PrivateBuyTime`].
    ///
    /// Accounts: `[pool, token_vault, collateral_vault, buyer_collateral_holding
    /// (private), buyer_token_holding (private)]`.
    #[allow(clippy::too_many_arguments)]
    pub fn lbp_buy_disposable(
        &mut self,
        program: ProgramId,
        pool: AccountId,
        user_collateral: AccountId,
        user_token: AccountId,
        collateral_in: u128,
        min_tokens_out: u128,
        at: PrivateBuyTime,
    ) -> Result<TxHash> {
        // See bc_buy_disposable: sync before spending, pin-check before proving.
        self.sync_private()?;
        let p = self.lbp_pool_checked(pool)?;
        let prog = disposable_program(program, lpad_guests::lbp(), "LBP")?;
        self.ensure_private_slot(user_collateral, "collateral")?;
        self.ensure_private_slot(user_token, "token")?;
        self.ensure_shielded_covers(user_collateral, collateral_in, p.collateral_definition_id)?;
        lbp_disposable_preflight(&p, at.at_ms)?;

        self.report("private buy (one atomic proof, minutes)");
        self.send_privacy_tx(
            &prog,
            vec![
                AccountIdentity::Public(pool),
                AccountIdentity::Public(p.token_vault_id),
                AccountIdentity::Public(p.collateral_vault_id),
                AccountIdentity::PrivateOwned(user_collateral),
                AccountIdentity::PrivateOwned(user_token),
            ],
            // One entry per private slot, in slot position order.
            &[user_collateral, user_token],
            lbp_core::Instruction::BuyDisposable {
                collateral_in,
                min_tokens_out,
                t_buy_ms: at.at_ms,
                deadline: at.deadline,
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
        let DisposableSaga { spend_src, reshield_target, amount, spend_definition, op_phase, text } = saga;
        // Privacy #3: re-shield target must be a shielded account we own.
        if self.wallet.get_account_private(reshield_target).is_none() {
            return Err("re-shield target must be a private (shielded) account in this wallet (RFP Privacy #3)".into());
        }
        // Fail fast before the multi-minute deshield proof: wrong token, or short.
        self.ensure_shielded_covers(spend_src, amount, spend_definition)?;
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
        if let Err(e) = self.privacy_token_transfer(
            AccountIdentity::PrivateOwned(spend_src),
            AccountIdentity::Public(a_in),
            spend_src,
            amount,
        ) {
            // Same trap the public leg below already guards, and it was missing
            // here: `poll_tx` can time out *after* the transaction landed. For a
            // deshield the evidence is unambiguous and free to read - the whole
            // point of tx1 is to put `amount` into the freshly created,
            // single-use account A, and nothing else can ever pay into it. So a
            // funded a_in means the proof landed and only the poller gave up.
            //
            // Failing here on a poller's word would be the expensive kind of
            // wrong: it strands the deshielded funds in a public account and
            // throws away a ~40-minute STARK that actually succeeded, and the
            // obvious "fix" of re-running deshields a SECOND time. Confirming by
            // state read instead is what makes a short poll budget safe.
            if self.holding_balance(a_in).unwrap_or(0) >= amount {
                self.report("deshield landed despite a poll timeout - continuing");
            } else {
                return Err(format!(
                    "{}: {e}\nThe ephemeral account A {a_in} did not receive the {} either, so the \
                     deshield did not land and nothing was spent - the shielded balance is untouched.",
                    text.op_failed, text.spend_noun,
                ));
            }
        }

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
    ///
    /// The token program chains into nothing, so it declares no dependencies -
    /// which is the whole difference between this and a `BuyDisposable`, whose
    /// guest dispatches `token::Transfer`s and must therefore hand the prover the
    /// token program up front (see [`disposable_program`]).
    fn privacy_token_transfer(
        &mut self,
        from: AccountIdentity,
        to: AccountIdentity,
        decode_into: AccountId,
        amount: u128,
    ) -> Result<TxHash> {
        let token_prog = ProgramWithDependencies::new(programs::token(), HashMap::new());
        self.send_privacy_tx(
            &token_prog,
            vec![from, to],
            &[decode_into],
            token_core::Instruction::Transfer { amount_to_transfer: amount },
        )
    }

    /// One privacy-preserving transaction: prove `instruction` against `program`
    /// over `accounts`, submit it, wait for inclusion, and fold the resulting
    /// notes back into the wallet.
    ///
    /// `accounts` carries each slot's visibility label, positionally aligned 1:1
    /// with the guest's `pre_states` - that alignment is what "private" means on
    /// LEZ, and it is asserted in the circuit
    /// (`lee/privacy_preserving_circuit/src/output.rs`). `program` must declare
    /// every program the guest chains into, or proving fails with
    /// `UndeclaredProgramDependency`.
    ///
    /// `decode_into` names the wallet-owned account behind each PRIVATE slot, in
    /// slot order: the prover returns one shared secret per private slot in that
    /// same order, and the decode mask pairs them up. A mis-ordered mask would
    /// decrypt a note into the wrong local account, so the counts are checked
    /// rather than trusted. (v0.2.1+ locates each note by nullifier rather than
    /// by mask index, which is what lets the mask stay as short as the private
    /// slot count even though the circuit pads the private inputs up to 7.)
    fn send_privacy_tx<I: Serialize>(
        &mut self,
        program: &ProgramWithDependencies,
        accounts: Vec<AccountIdentity>,
        decode_into: &[AccountId],
        instruction: I,
    ) -> Result<TxHash> {
        let private_slots = accounts.iter().filter(|a| a.is_private()).count();
        if private_slots != decode_into.len() {
            return Err(format!(
                "internal: {private_slots} private account slots but {} decode targets - the mask \
                 must name one wallet account per private slot, in slot order; this is a bug in \
                 the caller, not something a user can fix",
                decode_into.len()
            ));
        }
        // A privacy transaction hides its PRIVATE slots; its public ones are as
        // visible as any other transaction's, so they answer to the same
        // tripwire.
        let public_slots: Vec<AccountId> = accounts.iter().filter_map(public_slot_id).collect();
        self.refuse_deanonymising(&public_slots)?;
        let data = Program::serialize_instruction(instruction)
            .map_err(|e| format!("serialize instruction: {e:?}"))?;
        let decode_into = decode_into.to_vec();
        let rt = &self.rt;
        let wallet = &mut self.wallet;
        // The wallet prints the decoded account and the persist path to stdout.
        let _silence = gag::Gag::stdout().ok();
        rt.block_on(async move {
            let (hash, secrets) = wallet
                .send_privacy_preserving_tx(accounts, data, program)
                .await
                .map_err(|e| format!("privacy transaction failed: {e:?}"))?;
            let (tx, _block_id) = wallet
                .poll_transaction(hash)
                .await
                // A privacy transaction that never lands is nearly always the
                // validity window closing while the proof was still being
                // computed - the deadline is stamped from a clock read BEFORE
                // proving starts, and a STARK measures ~40 minutes. The poller's
                // own text ("All pollers failed") points at the network, which is
                // the wrong place to look; say what actually goes wrong here.
                .map_err(|e| {
                    format!(
                        "the proved transaction was never included: {e}\n\
                         The usual cause is its timestamp validity window expiring \
                         while the proof was being computed - proving takes roughly \
                         40 minutes and the window is capped at 60 (the guest's \
                         MAX_PRIVATE_WINDOW_MS), measured from before proving began. \
                         Nothing was spent: an unincluded transaction moves no funds. \
                         Retry, and if this recurs the machine is proving too slowly \
                         for the window rather than the chain being unreachable."
                    )
                })?;
            if let LeeTransaction::PrivacyPreserving(ppt) = tx {
                let mask: Vec<AccDecodeData> = secrets
                    .into_iter()
                    .zip(decode_into)
                    .map(|(secret, account)| AccDecodeData::Decode(secret, account))
                    .collect();
                wallet
                    .decode_insert_privacy_preserving_transaction_results(&ppt, &mask)
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

// ---------------------------------------------------------------------------
// Deployment
// ---------------------------------------------------------------------------

/// How far back [`LaunchpadClient::deploy_program`] scans for a guest's own
/// bytes when neither the poll nor the sequencer's transaction index can prove
/// the deploy landed.
///
/// Bounded on purpose, and the range is always named in the failure message, so
/// a "not deployed" verdict can never come from a scan that quietly gave up
/// early. `$LPAD_DEPLOY_SCAN_BLOCKS` (the same knob scripts/bootstrap.sh uses)
/// raises it when the deploy is older than this many blocks.
const DEPLOY_SCAN_BLOCKS: u64 = 400;

fn deploy_scan_blocks() -> u64 {
    std::env::var("LPAD_DEPLOY_SCAN_BLOCKS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEPLOY_SCAN_BLOCKS)
}

/// lpad's three programs, in the order to deploy them, as `(name, program)`.
///
/// The name is what a summary line and a deploy error call the program; the
/// [`Program`] carries both the committed ELF ([`Program::elf`], what a deploy
/// submits) and its id ([`Program::id`], the RISC0 image id of exactly those
/// bytes). Those ids are the same on every network - what differs per network is
/// only whether these programs have been deployed there, which is what
/// [`LaunchpadClient::lpad_deployment_status`] answers.
#[must_use]
pub fn lpad_programs() -> [(&'static str, Program); 3] {
    [
        ("bonding_curve", lpad_guests::bonding_curve()),
        ("lbp", lpad_guests::lbp()),
        ("wlez", lpad_guests::wlez()),
    ]
}

/// Where one lpad program stands on the chain a client is pointed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramStatus {
    /// `bonding_curve`, `lbp` or `wlez`.
    pub name: &'static str,
    /// The program id (RISC0 image id of the committed ELF). Identical on every
    /// network - it is a compile-time constant of this build.
    pub program_id: ProgramId,
    /// The block holding this program's deploy transaction, or `None` if this
    /// chain has no record of it. See [`LaunchpadClient::program_deployed`] for
    /// what `None` does and does not prove.
    pub block: Option<u64>,
}

impl ProgramStatus {
    /// Whether this chain has a record of the deploy. `false` means "deploy it";
    /// doing so when it is in fact already there is a harmless no-op, which is
    /// the cheap direction to be wrong in.
    #[must_use]
    pub const fn deployed(&self) -> bool {
        self.block.is_some()
    }
}

/// The outcome of [`LaunchpadClient::deploy_program`]: the program is on chain,
/// and that was *verified* by reading `block`, not inferred from a wallet
/// report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deployment {
    /// The name passed to [`LaunchpadClient::deploy_program`].
    pub name: &'static str,
    /// The deploy transaction's hash - a pure function of the ELF, so it is the
    /// same on every network and across every re-deploy. See [`deploy_tx_hash`].
    pub tx_hash: TxHash,
    /// The block whose body contains this exact ELF.
    pub block: u64,
    /// True when nothing was submitted because the program was already on chain:
    /// the idempotent no-op. `block` is then the ORIGINAL deploy's block, which
    /// may be far behind the head - report it as "already deployed in block N",
    /// not as "deployed just now".
    pub already_deployed: bool,
}

/// The transaction hash a deploy of `elf` will have, computed offline.
///
/// A program-deployment transaction is *only* its bytecode - no signer, no
/// nonce, no timestamp (`lee::program_deployment_transaction::Message`) - so its
/// hash is a pure function of the ELF. Two consequences the whole deploy path
/// rests on: the same guest always produces the same transaction hash, whoever
/// sends it and whenever; and the chain therefore never includes it twice, so a
/// re-deploy is a no-op rather than a second copy of the program.
#[must_use]
pub fn deploy_tx_hash(elf: &[u8]) -> TxHash {
    lee::ProgramDeploymentTransaction::new(lee::program_deployment_transaction::Message::new(
        elf.to_vec(),
    ))
    .hash()
}

impl LaunchpadClient {
    /// Is this program already on the chain this client is pointed at, and in
    /// which block?
    ///
    /// Answered by computing the deploy transaction's hash offline
    /// ([`deploy_tx_hash`]) and asking the sequencer for that transaction: the
    /// hash is a pure function of the bytecode, so the deploy of THIS exact ELF,
    /// by anyone and at any time, has exactly that hash and no other. What comes
    /// back is then checked to be a program deployment carrying these exact
    /// bytes, so the answer never rests on the hash lookup alone.
    ///
    /// Why not `getProgramIds`: it is a hardcoded five-entry list of LEZ's
    /// built-ins with a TODO upstream, not an inventory of deployed programs.
    /// lpad's programs will never appear in it on any network, so absence there
    /// means nothing at all - never infer a deployment state from it.
    ///
    /// `None` means "this chain has no record of that deploy", which is the
    /// honest answer rather than proof of absence: a sequencer that was reset,
    /// or restored from a store missing the early blocks, can have forgotten the
    /// transaction. Treat `None` as "deploy it" - [`Self::deploy_program`] is
    /// idempotent, so being wrong in that direction costs nothing.
    pub fn program_deployed(&self, elf: &[u8]) -> Result<Option<u64>> {
        let hash = common::HashType(deploy_tx_hash(elf));
        let found = self
            .rt
            .block_on(async {
                let _silence = gag::Gag::stdout().ok();
                self.wallet.get_transaction(hash).await
            })
            .map_err(|e| {
                format!(
                    "ask {} whether the program is deployed: {e} - the sequencer is not \
                     answering, so nothing can be said about what is on it",
                    self.wallet.helm_url()
                )
            })?;
        let Some((tx, block)) = found else {
            return Ok(None);
        };
        // Confirm the bytes, not just the hash: a deployment that carried other
        // bytecode under this hash would mean the derivation drifted, and
        // "deployed" would be a lie that costs a whole session of timeouts.
        let LeeTransaction::ProgramDeployment(deploy) = tx else {
            return Ok(None);
        };
        Ok((deploy.into_message().into_bytecode() == elf).then_some(block))
    }

    /// Where all three lpad programs stand on this chain, so a caller can render
    /// one summary and decide whether to offer a deploy.
    ///
    /// Three transaction-index lookups and no block scanning, so it is cheap
    /// enough to run whenever the network is picked or changed. See
    /// [`Self::program_deployed`] for what a `None` block does and does not
    /// prove.
    pub fn lpad_deployment_status(&self) -> Result<Vec<ProgramStatus>> {
        lpad_programs()
            .into_iter()
            .map(|(name, program)| {
                self.report(&format!("checking {name}"));
                Ok(ProgramStatus {
                    name,
                    program_id: program.id(),
                    block: self.program_deployed(program.elf())?,
                })
            })
            .collect()
    }

    /// Deploy `elf` to this chain and PROVE it landed, or fail.
    ///
    /// Idempotent, deliberately. A deploy transaction's hash is a pure function
    /// of its bytecode, so re-deploying an unchanged guest resolves to the
    /// ORIGINAL deploy - a feature, since it makes "deploy the three programs"
    /// safe to run twice. That is why this asks [`Self::program_deployed`] FIRST
    /// and returns without submitting when the answer is yes: submitting anyway
    /// is not merely wasteful, because the duplicate is never re-included, so
    /// the wallet polls its whole budget (`seq_poll_max_retries` x
    /// `seq_poll_timeout`, 30 minutes by default) to prove nothing. That was
    /// measured on testnet, per program.
    ///
    /// Then it VERIFIES, because a deploy that reports success may not be
    /// deployed - scripts/bootstrap.sh documents the incident this discipline
    /// comes from. Submitting without error is not proof, and neither is the
    /// block the wallet names; a deploy that never lands surfaces only as a
    /// generic poll timeout, indistinguishable from a slow chain; and a later
    /// transaction against a program that is not there is dropped from the
    /// mempool and reads as *the same* timeout. lbp once reported inclusion in
    /// block 1161, was never deployed, and every `create-sale` after it was
    /// silently discarded - read as poll flakiness for a whole session. The one
    /// thing that cannot lie is whether the ELF's own bytes are in a block, so
    /// that is what is checked here, in three stages (the named block, the
    /// sequencer's transaction index, then a bounded backward scan), and failing
    /// all three is an error naming the remedy - never a warning.
    pub fn deploy_program(&self, name: &'static str, elf: &[u8]) -> Result<Deployment> {
        let tx_hash = deploy_tx_hash(elf);
        self.report(&format!("checking whether {name} is already deployed"));
        if let Some(block) = self.program_deployed(elf)? {
            return Ok(Deployment { name, tx_hash, block, already_deployed: true });
        }

        self.report(&format!("deploying {name} ({} bytes)", elf.len()));
        let bytecode = elf.to_vec();
        let polled_block = self.rt.block_on(async {
            // The multi-sequencer client prints ("Statistics not found, ...") on
            // paths like this; keep it off a possibly --json stdout.
            let _silence = gag::Gag::stdout().ok();
            let hash = self
                .wallet
                .send_program_deployment_transaction(bytecode)
                .await
                .map_err(|e| {
                    format!(
                        "submit {name} deploy to {}: {e} - the sequencer did not accept the \
                         transaction; check it is running and reachable, then re-run",
                        self.wallet.helm_url()
                    )
                })?;
            if to_hash(hash) != tx_hash {
                return Err(format!(
                    "internal: this build computed {name}'s deploy hash as {} but the wallet \
                     sent {hash} - `deploy_tx_hash` no longer matches how LEZ hashes a \
                     deployment, so `program_deployed` would answer \"no\" forever and every \
                     deploy would poll to exhaustion. Report this rather than retrying",
                    hex_bytes(&tx_hash)
                ));
            }
            // A deploy that never landed and a merely slow chain both surface here
            // as "All pollers failed", so a poll failure decides nothing - the
            // verification below does. Keep whatever block it managed to name.
            Ok(self.wallet.poll_transaction(hash).await.ok().map(|(_tx, block)| block))
        })?;

        // Stage 1: the block the wallet named. Cheap, and the common case.
        self.report(&format!("verifying {name}'s bytes are on chain"));
        if let Some(block) = polled_block
            && self.block_holds_elf(block, elf)?
        {
            return Ok(Deployment { name, tx_hash, block, already_deployed: false });
        }
        // Stage 2: the sequencer's own transaction index. Resolves the case where
        // the poller gave up (it polls on a timeout, not on blocks) while the
        // deploy did land. `already_deployed` stays false: the pre-check asked
        // this same index moments ago and it did not have the deploy, so what it
        // has now is this call's own submission.
        if let Some(block) = self.program_deployed(elf)? {
            return Ok(Deployment { name, tx_hash, block, already_deployed: false });
        }
        // Stage 3: read the blocks. The index is rebuilt from the block store, so
        // it can be missing an entry the store still holds (a reset chain that
        // kept its blocks did exactly that: testnet block 1138 held bonding_curve
        // while every re-deploy could only time out). Bounded, and the range is
        // reported either way.
        let head = self.block_height()?;
        let span = deploy_scan_blocks();
        let low = head.saturating_sub(span);
        self.report(&format!("scanning blocks {low}..{head} for {name}"));
        let (found, unreadable) = self.scan_for_deploy(elf, head, span)?;
        if let Some(block) = found {
            // Bytes in a block the index does not know about: the deploy predates
            // this call (the index is built at startup from the store, so it can
            // lag it), and this call's submission was the duplicate that is
            // correctly never re-included.
            return Ok(Deployment { name, tx_hash, block, already_deployed: true });
        }

        let named = match polled_block {
            Some(b) => format!("the wallet named block {b} but its bytes are not there"),
            None => "it never reported an inclusion block".to_owned(),
        };
        let skipped = if unreadable == 0 {
            String::new()
        } else {
            format!(
                " ({unreadable} of those blocks could not be read, so the scan was not \
                 exhaustive - if the sequencer is unhealthy, fix that first)"
            )
        };
        Err(format!(
            "{name} was submitted to {} but its bytes are in no block: {named}, this chain's \
             transaction index does not have the deploy, and blocks {low}..{head} do not contain \
             it{skipped}. Treat it as NOT deployed: every transaction against this program will \
             be silently dropped from the mempool and read as a timeout, not as an error. \
             Re-run the deploy once the chain is healthy; if it was deployed longer ago than \
             {span} blocks, raise $LPAD_DEPLOY_SCAN_BLOCKS and re-check instead of deploying",
            self.wallet.helm_url()
        ))
    }

    /// Deploy all three lpad programs, skipping any already on chain.
    ///
    /// One call for the "you picked a chain you run yourself, it has none of
    /// lpad's programs" flow. Each is verified as in [`Self::deploy_program`],
    /// and the first failure stops the run: the later programs are useless
    /// without the earlier ones, and continuing would bury the one error that
    /// matters under two more.
    pub fn deploy_lpad_programs(&self) -> Result<Vec<Deployment>> {
        lpad_programs()
            .into_iter()
            .map(|(name, program)| self.deploy_program(name, program.elf()))
            .collect()
    }

    /// Does `block` contain a program deployment of exactly these bytes?
    ///
    /// Compares the bytecode rather than the transaction hash: the hash is what
    /// led us to this block, so re-checking it would only restate the question.
    fn block_holds_elf(&self, block_id: u64, elf: &[u8]) -> Result<bool> {
        let block = self
            .rt
            .block_on(async {
                let _silence = gag::Gag::stdout().ok();
                self.wallet.get_block(block_id).await
            })
            .map_err(|e| format!("read block {block_id}: {e}"))?;
        let Some(block) = block else { return Ok(false) };
        Ok(block.body.transactions.into_iter().any(|tx| match tx {
            LeeTransaction::ProgramDeployment(deploy) => {
                deploy.into_message().into_bytecode() == elf
            }
            _ => false,
        }))
    }

    /// Scan `head` down to `head - span` for a deployment of `elf`, returning
    /// the block it is in and how many blocks could not be read.
    ///
    /// An unreadable block is skipped rather than fatal - one flaky read must not
    /// turn a program that IS deployed into a hard error - but the count is
    /// returned so the caller can say the scan was incomplete instead of
    /// claiming a clean "not found".
    fn scan_for_deploy(&self, elf: &[u8], head: u64, span: u64) -> Result<(Option<u64>, u64)> {
        let low = head.saturating_sub(span);
        let mut unreadable = 0_u64;
        for block in (low..=head).rev() {
            match self.block_holds_elf(block, elf) {
                Ok(true) => return Ok((Some(block), unreadable)),
                Ok(false) => {}
                Err(_) => unreadable = unreadable.saturating_add(1),
            }
        }
        Ok((None, unreadable))
    }
}

/// Reject, in milliseconds, a bonding-curve sale whose `BuyDisposable` the guest
/// would only revert several minutes into a proof. The counterpart of
/// [`lbp_disposable_preflight`], and the same bargain: an actionable sentence
/// now instead of a `ProgramProveFailed` later. The guest stays the authority.
///
/// Note what is NOT here: the buyer's collateral definition. That one needs the
/// wallet's view of the note rather than the sale state, so it lives in
/// [`LaunchpadClient::ensure_shielded_covers`], which callers run immediately
/// before this.
fn bc_disposable_preflight(s: &SaleState, not_before_ms: u64) -> Result<()> {
    // `apply_buy` asserts this on every path, disposable included. Cheap to
    // learn now; several minutes of proving to learn from the guest.
    if !matches!(s.status, BcStatus::Open) {
        return Err(
            "this sale is closed - nothing can be bought from it, on the private path or any \
             other. A closed sale is final: it either reached its end timestamp or sold out"
                .into(),
        );
    }
    // The guest does not read a clock on this path; its end-of-sale guard is the
    // emitted validity window, whose upper bound collapses onto `not_before_ms`
    // once the sale has ended - an empty `ValidityWindow` and a panic several
    // minutes from now.
    if s.end_timestamp_ms != 0 && not_before_ms >= s.end_timestamp_ms {
        return Err(format!(
            "this sale ended at {} and the private buy would be priced at {} - no validity \
             window is left to include it in",
            s.end_timestamp_ms, not_before_ms
        ));
    }
    Ok(())
}

/// Reject, in milliseconds, an LBP pool whose `BuyDisposable` the guest would
/// only revert several minutes into a proof.
///
/// Each check mirrors an assert the guest makes, and exists purely so the
/// operator gets an actionable sentence instead of a `ProgramProveFailed` with a
/// guest panic buried in it. The guest remains the authority: nothing here is
/// load-bearing for safety, and a check that drifts out of date costs a
/// confusing error, not a bad trade.
///
/// One guest assert is deliberately absent: that the buyer's collateral holding
/// is of `collateral_definition_id`. It cannot be answered from `PoolState`
/// alone - it needs the wallet's view of the shielded note - so it is checked in
/// [`LaunchpadClient::ensure_shielded_covers`], which the caller runs just
/// before this.
fn lbp_disposable_preflight(p: &PoolState, t_buy_ms: u64) -> Result<()> {
    if !matches!(p.status, LbpStatus::Open) {
        return Err("this pool is closed".into());
    }
    if p.paused {
        return Err("this pool is paused - wait for the creator to resume it".into());
    }
    if t_buy_ms < p.t_start_ms || t_buy_ms >= p.t_end_ms {
        return Err(format!(
            "a private buy is priced at the current time ({t_buy_ms}), which is outside this \
             pool's schedule [{}, {}) - it cannot be bought privately before it starts or after \
             it ends",
            p.t_start_ms, p.t_end_ms
        ));
    }
    if !lbp_core::is_open_allowlist(&p.allowlist_root) {
        return Err(
            "this pool is allowlist-gated: the private path carries no inclusion proof, so use \
             `lbp buy-gated` (public) with the proof for your own buyer leaf"
                .into(),
        );
    }
    if p.block_token_ceiling != 0 {
        return Err(
            "this pool sets a per-block token ceiling, which a private buy cannot enforce (a \
             proof carries no block id); buy publicly instead"
                .into(),
        );
    }
    if !p.fixed_price && p.w_start_q64 <= p.w_end_q64 {
        return Err(
            "this pool's token weight does not decline, so pricing it at a caller-chosen \
             timestamp is not sound; buy publicly instead"
                .into(),
        );
    }
    Ok(())
}

/// Lowercase hex of an account id, so an error can name a definition the
/// operator can paste straight back into the failing command.
fn hex_id(id: AccountId) -> String {
    id.to_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

/// Lowercase hex of a raw hash, so an error can name a transaction the operator
/// can look up on the chain it was sent to.
fn hex_bytes(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
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
mod network_choice {
    //! The named networks and their round-trip through the config form. All
    //! offline: nothing here touches a chain.

    use super::{Network, LOCAL_SEQUENCER, PARADOX_SEQUENCER, TESTNET_SEQUENCER};

    /// Whatever `Display` writes, `parse` must read back as the same network -
    /// that equality is the whole reason a CLI can remember the user's choice in
    /// a config file. A `Local` whose URL did not survive the trip would send
    /// later commands to a different chain than the one that was picked.
    #[test]
    fn every_network_round_trips_through_its_config_form() {
        for n in [
            Network::Testnet,
            Network::Paradox,
            Network::local(""),
            Network::local("http://192.168.1.7:3040"),
            Network::Custom("https://seq.example.com".to_owned()),
        ] {
            assert_eq!(Network::parse(&n.to_string()).unwrap(), n, "round-trip of {n}");
        }
    }

    #[test]
    fn aliases_and_urls_resolve_as_before() {
        assert_eq!(Network::parse("testnet").unwrap(), Network::Testnet);
        assert_eq!(Network::parse("logos").unwrap(), Network::Testnet);
        assert_eq!(Network::parse(" paradox ").unwrap(), Network::Paradox);
        assert_eq!(Network::default(), Network::Testnet);
        // A bare URL must keep meaning exactly what it meant before `local`
        // existed, or every existing --network invocation changes behaviour.
        assert_eq!(
            Network::parse("http://seq.example.com:3040").unwrap(),
            Network::Custom("http://seq.example.com:3040".to_owned())
        );
        assert_eq!(Network::parse("testnet").unwrap().url(), TESTNET_SEQUENCER);
        assert_eq!(Network::parse("paradox").unwrap().url(), PARADOX_SEQUENCER);
    }

    #[test]
    fn local_defaults_but_keeps_whatever_url_it_was_given() {
        assert_eq!(Network::parse("local").unwrap().url(), LOCAL_SEQUENCER);
        assert_eq!(Network::local("  ").url(), LOCAL_SEQUENCER);
        assert_eq!(Network::parse("local=http://10.0.0.2:3040").unwrap().url(), "http://10.0.0.2:3040");
        assert!(Network::parse("local").unwrap().is_local());
        assert!(!Network::parse(LOCAL_SEQUENCER).unwrap().is_local(), "a pasted URL is Custom, not Local");
    }

    #[test]
    fn unusable_networks_are_refused_with_the_options_spelled_out() {
        for bad in ["", "mainnet", "local=127.0.0.1:3040", "ftp://seq"] {
            let err = Network::parse(bad).unwrap_err();
            assert!(err.contains("local"), "{bad:?} error should name the options: {err}");
        }
    }

    #[test]
    fn the_picker_offers_exactly_the_three_named_networks() {
        let choices = Network::choices();
        assert_eq!(choices, [Network::Testnet, Network::Paradox, Network::local("")]);
        let names: Vec<&str> = choices.iter().map(Network::display_name).collect();
        assert_eq!(names, ["Logos testnet", "Paradox", "Local"]);
        for c in &choices {
            assert!(c.url().starts_with("http"), "{c} must offer a usable URL");
        }
    }
}

#[cfg(test)]
mod create_sale_accounts {
    //! The two `CreateSale` account lists, pinned by INDEX.
    //!
    //! Both guests destructure positionally and neither list is type-checked on
    //! the way out, so a role in the wrong slot is not a compile error and not a
    //! local failure: it is a signed, submitted transaction that comes back as
    //! an assert about a different account than the one that is actually wrong.
    //! These tests are the only place that mapping is checked without a chain.
    //!
    //! Each role gets a distinct marker id, so swapping any two entries - or
    //! dropping the treasury, which is what this build just added to both lists
    //! - fails here.

    use super::{
        bc_create_sale_accounts, lbp_create_sale_accounts, AccountId, BcCreateArgs, LbpCreateArgs,
    };

    fn id(b: u8) -> AccountId {
        AccountId::new([b; 32])
    }

    fn bc_args() -> BcCreateArgs {
        BcCreateArgs {
            program: [0u32; 8],
            collateral_def: id(5),
            treasury: id(3),
            creator_token_holding: id(6),
            creator: id(7),
            sale_quantity: 1,
            dex_seed: 1,
            virt_token: 2,
            virt_collateral: 1,
            fee_bps: 0,
            one_directional: false,
            end_timestamp_ms: 0,
            min_duration_ms: 0,
            nonce: 0,
        }
    }

    fn lbp_args() -> LbpCreateArgs {
        LbpCreateArgs {
            program: [0u32; 8],
            collateral_def: id(4),
            treasury: id(8),
            creator_token_holding: id(5),
            creator_collateral_holding: id(6),
            creator: id(7),
            token_deposit: 1,
            collateral_seed: 1,
            w_start_q64: 1,
            w_end_q64: 1,
            t_start_ms: 0,
            t_end_ms: 1,
            fee_bps: 0,
            block_token_ceiling: 0,
            allowlist_root: [0u8; 32],
            fixed_price: false,
            min_duration_ms: 0,
            nonce: 0,
        }
    }

    /// `[sale, token_vault, collateral_vault, treasury, token_def,
    /// collateral_def, creator_token_holding, creator, creator_index, clock]` -
    /// the order in the module doc of
    /// `programs/bonding_curve/src/create_sale.rs`.
    #[test]
    fn the_bonding_curve_list_is_ten_accounts_in_the_guest_order() {
        let a = bc_args();
        let accounts = bc_create_sale_accounts(id(0), id(1), id(2), id(4), &a);
        assert_eq!(
            accounts,
            vec![
                id(0),
                id(1),
                id(2),
                a.treasury,
                id(4),
                a.collateral_def,
                a.creator_token_holding,
                a.creator,
                bonding_curve_core::compute_creator_index_pda(a.program, a.creator),
                bonding_curve_core::CLOCK_01,
            ],
            "the bonding-curve CreateSale account list no longer matches the guest's destructure"
        );
        assert_eq!(accounts.len(), 10, "the guest destructures exactly ten accounts");
    }

    /// The index slot must carry the index PDA of the SIGNING creator, derived
    /// under the program the sale is being created on. Both halves matter: the
    /// guest pins this account against its own
    /// `compute_creator_index_pda(self_program_id, creator)`, so a vec built
    /// from another creator - or the same creator under the other launchpad -
    /// is a rejected transaction, and passing a creator's index while naming a
    /// different creator is precisely what that pin exists to stop.
    #[test]
    fn the_bonding_curve_index_slot_is_this_creator_index_pda_on_this_program() {
        let mut a = bc_args();
        let accounts = bc_create_sale_accounts(id(0), id(1), id(2), id(4), &a);
        assert_eq!(
            accounts[8],
            bonding_curve_core::compute_creator_index_pda(a.program, a.creator),
            "the index slot is not this creator's index PDA"
        );
        assert_ne!(
            accounts[8],
            bonding_curve_core::compute_creator_index_pda(a.program, id(99)),
            "a different creator must derive a different index account"
        );
        assert_ne!(
            accounts[8],
            lbp_core::compute_creator_index_pda(a.program, a.creator),
            "the two launchpads must not share one index account"
        );
        a.program = [7u32; 8];
        let rebuilt = bc_create_sale_accounts(id(0), id(1), id(2), id(4), &a);
        assert_ne!(rebuilt[8], accounts[8], "the index PDA must follow the program id");
    }

    /// The treasury is index 3 on the curve, the slot it also occupies in
    /// `Buy`/`Sell`/`BuyAta`/`SellAta`. Sending it anywhere else types some
    /// other account as the fee sink.
    #[test]
    fn the_bonding_curve_treasury_sits_where_every_trade_puts_it() {
        let a = bc_args();
        let accounts = bc_create_sale_accounts(id(0), id(1), id(2), id(4), &a);
        assert_eq!(accounts[3], a.treasury);
        assert_ne!(a.treasury, a.collateral_def, "the fixture must not alias the roles it pins");
    }

    /// `[pool, token_vault, collateral_vault, token_def, collateral_def,
    /// creator_token_holding, creator_collateral_holding, creator, treasury,
    /// creator_index, clock]` - the order in the module doc of
    /// `programs/lbp/src/create_sale.rs`. The treasury goes AFTER `creator`, not
    /// at index 3: that is what kept every pre-existing LBP index stable when it
    /// was added, and it is why the two lists must not be shared.
    #[test]
    fn the_lbp_list_is_eleven_accounts_in_the_guest_order() {
        let a = lbp_args();
        let accounts = lbp_create_sale_accounts(id(0), id(1), id(2), id(3), &a);
        assert_eq!(
            accounts,
            vec![
                id(0),
                id(1),
                id(2),
                id(3),
                a.collateral_def,
                a.creator_token_holding,
                a.creator_collateral_holding,
                a.creator,
                a.treasury,
                lbp_core::compute_creator_index_pda(a.program, a.creator),
                lbp_core::CLOCK_01,
            ],
            "the LBP CreateSale account list no longer matches the guest's destructure"
        );
        assert_eq!(accounts.len(), 11, "the guest destructures exactly eleven accounts");
        assert_eq!(accounts[8], a.treasury);
    }

    /// As for the curve: the LBP index slot is this creator's index PDA under
    /// the LBP program, and it sits at 9 - after the treasury, before the clock.
    #[test]
    fn the_lbp_index_slot_is_this_creator_index_pda_on_this_program() {
        let mut a = lbp_args();
        let accounts = lbp_create_sale_accounts(id(0), id(1), id(2), id(3), &a);
        assert_eq!(
            accounts[9],
            lbp_core::compute_creator_index_pda(a.program, a.creator),
            "the index slot is not this creator's index PDA"
        );
        assert_ne!(
            accounts[9],
            lbp_core::compute_creator_index_pda(a.program, id(99)),
            "a different creator must derive a different index account"
        );
        assert_ne!(
            accounts[9],
            bonding_curve_core::compute_creator_index_pda(a.program, a.creator),
            "the two launchpads must not share one index account"
        );
        a.program = [7u32; 8];
        let rebuilt = lbp_create_sale_accounts(id(0), id(1), id(2), id(3), &a);
        assert_ne!(rebuilt[9], accounts[9], "the index PDA must follow the program id");
    }

    /// The clock is appended by the guest's `echo_clock`, so it has to be LAST
    /// in both lists - the creator index went in directly BEFORE it for exactly
    /// that reason - and each list has to name every role exactly once: LEZ
    /// rejects a duplicated account id outright.
    #[test]
    fn both_lists_end_at_the_clock_and_repeat_no_account() {
        let bc = bc_create_sale_accounts(id(0), id(1), id(2), id(4), &bc_args());
        let lbp = lbp_create_sale_accounts(id(0), id(1), id(2), id(3), &lbp_args());
        assert_eq!(*bc.last().unwrap(), bonding_curve_core::CLOCK_01);
        assert_eq!(*lbp.last().unwrap(), lbp_core::CLOCK_01);
        for list in [&bc, &lbp] {
            let mut seen = list.clone();
            seen.sort_unstable();
            seen.dedup();
            assert_eq!(seen.len(), list.len(), "an account id appears twice in {list:?}");
        }
    }
}

#[cfg(test)]
mod deployment {
    //! The deploy-hash derivation, offline. The chain-facing half
    //! (`deploy_program` / `program_deployed`) needs a sequencer and is covered
    //! by scripts/bootstrap.sh against a real one.

    use super::{deploy_tx_hash, lpad_programs};
    use common::transaction::LeeTransaction;

    /// The hash lpad computes must be the hash the CHAIN indexes the deploy
    /// under - `LeeTransaction::hash()` is what `getTransaction` is keyed by. If
    /// these ever diverge, `program_deployed` answers "no" forever, every deploy
    /// polls its full 30-minute budget for an inclusion that will never come,
    /// and the idempotency this whole path is built on is gone.
    #[test]
    fn deploy_hash_matches_the_hash_the_chain_indexes() {
        for (name, program) in lpad_programs() {
            let tx = lee::ProgramDeploymentTransaction::new(
                lee::program_deployment_transaction::Message::new(program.elf().to_vec()),
            );
            assert_eq!(
                deploy_tx_hash(program.elf()),
                LeeTransaction::ProgramDeployment(tx).hash().0,
                "{name}'s deploy hash is not what the sequencer would index it under"
            );
        }
    }

    /// Pure function of the bytecode: same ELF, same hash, every time and on
    /// every network - that is what makes "already deployed?" answerable without
    /// a per-network id table. And distinct guests must not collide, or one
    /// program would be read as another's deploy.
    #[test]
    fn the_hash_is_a_pure_function_of_the_bytecode() {
        let [(_, bc), (_, lbp), (_, wlez)] = lpad_programs();
        assert_eq!(deploy_tx_hash(bc.elf()), deploy_tx_hash(bc.elf()));
        let hashes = [deploy_tx_hash(bc.elf()), deploy_tx_hash(lbp.elf()), deploy_tx_hash(wlez.elf())];
        for (i, a) in hashes.iter().enumerate() {
            for b in &hashes[i + 1..] {
                assert_ne!(a, b, "two lpad guests share a deploy transaction hash");
            }
        }
        // One flipped byte is a different program and must be a different deploy.
        let mut tweaked = bc.elf().to_vec();
        tweaked[0] ^= 0x01;
        assert_ne!(deploy_tx_hash(bc.elf()), deploy_tx_hash(&tweaked));
    }

    /// The deploy set is exactly the three programs whose ids are pinned, in a
    /// stable order. A program missing here is a program the CLI would never
    /// deploy to a fresh chain, and would only surface as timeouts later.
    #[test]
    fn the_deploy_set_is_the_three_pinned_programs() {
        let programs = lpad_programs();
        let names: Vec<&str> = programs.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, ["bonding_curve", "lbp", "wlez"]);
        assert_eq!(programs[0].1.id(), lpad_guests::deployed::BONDING_CURVE);
        assert_eq!(programs[1].1.id(), lpad_guests::deployed::LBP);
        assert_eq!(programs[2].1.id(), lpad_guests::deployed::WLEZ);
    }
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

#[cfg(test)]
mod discovery_pruning {
    //! The creator prune. All offline: it is a pure function of what the
    //! account scan already read, which is the reason it costs no extra RPC.

    use super::{creator_candidates, creator_ineligible_owners, public_owners, AccountId, ProgramId};
    use lee_core::{account::Account, program::DEFAULT_PROGRAM_ID};

    fn id(n: u8) -> AccountId {
        AccountId::new([n; 32])
    }

    fn owned(n: u8, owner: ProgramId) -> (AccountId, Option<ProgramId>) {
        (id(n), Some(owner))
    }

    /// The whole point of the prune: a wallet that has launched tokens is mostly
    /// token-program accounts (definition + supply holding + metadata per mint,
    /// plus one per `init-holding`), and none of them is an identity account a
    /// sale could name as its creator.
    #[test]
    fn token_and_ata_owned_accounts_are_not_creator_candidates() {
        let token = programs::token().id();
        let ata = programs::ata().id();
        let auth = programs::authenticated_transfer().id();
        let scan = vec![
            owned(1, auth),             // the bootstrap's creator
            owned(2, token),            // a token definition
            owned(3, token),            // its supply holding
            owned(4, token),            // its metadata account
            owned(5, ata),              // an associated token account
            owned(6, DEFAULT_PROGRAM_ID), // never initialised, still a creator
        ];
        let kept = creator_candidates(&scan, &creator_ineligible_owners());
        assert_eq!(kept, vec![id(1), id(6)]);
    }

    /// `bc_create_token_sale_private` mints a fresh, never-funded,
    /// never-initialised account as the unlinkable creator A and never touches it
    /// again, so it stays DEFAULT-owned forever. Pruning DEFAULT would hide every
    /// privately created sale - the exact "missed sale" this filter must not
    /// cause.
    #[test]
    fn a_default_owned_account_is_still_a_creator_candidate() {
        let kept = creator_candidates(
            &[owned(7, DEFAULT_PROGRAM_ID)],
            &creator_ineligible_owners(),
        );
        assert_eq!(kept, vec![id(7)]);
    }

    /// UNKNOWN is not RULED OUT. A read that failed (offline sequencer, account
    /// not yet in a block) must leave the candidate in, or a flaky network would
    /// silently shrink the search and drop sales from the listing.
    #[test]
    fn an_unreadable_account_is_never_pruned() {
        let kept = creator_candidates(&[(id(8), None)], &creator_ineligible_owners());
        assert_eq!(kept, vec![id(8)]);
    }

    /// A blocklist, not an allowlist: an owner this build has never heard of is
    /// kept. Programs are deployed permissionlessly on LEZ, so "unrecognised"
    /// will happen and must not mean "not a creator".
    #[test]
    fn an_unrecognised_owner_is_kept() {
        let stranger: ProgramId = [42; 8];
        let kept = creator_candidates(&[owned(9, stranger)], &creator_ineligible_owners());
        assert_eq!(kept, vec![id(9)]);
    }

    /// Order is preserved so a pruned scan probes ids in the same sequence the
    /// unpruned one did - which is what keeps `my-sales` printing in its old
    /// order.
    #[test]
    fn the_prune_preserves_order() {
        let token = programs::token().id();
        let scan = vec![owned(3, DEFAULT_PROGRAM_ID), owned(2, token), owned(1, DEFAULT_PROGRAM_ID)];
        assert_eq!(creator_candidates(&scan, &creator_ineligible_owners()), vec![id(3), id(1)]);
    }

    /// Shielded accounts are not creator candidates at all (a sale PDA is derived
    /// from a public creator id), and the owner is taken from the account that
    /// was already read.
    #[test]
    fn public_owners_drops_the_shielded_side_and_carries_the_owner() {
        let token = programs::token().id();
        let holding = Account { program_owner: token, ..Account::default() };
        let scan = vec![
            (id(1), false, Some(Account::default())),
            (id(2), false, Some(holding)),
            (id(3), false, None),
            (id(4), true, Some(Account::default())),
        ];
        assert_eq!(
            public_owners(&scan),
            vec![(id(1), Some(DEFAULT_PROGRAM_ID)), (id(2), Some(token)), (id(3), None)]
        );
    }
}

#[cfg(test)]
mod creator_index_discovery {
    //! The on-chain per-creator index: how one read of an index PDA is
    //! classified, and what that classification commits the search to. All
    //! offline - both functions under test are pure, which is the point: the
    //! decision table here is the difference between a four-read listing, a
    //! forty-minute one, and a listing that wrongly says you have no sales.

    use super::{
        classify_creator_index, plan_discovery, Account, AccountId, CreatorIndexLookup,
        DiscoveryPlan, ProgramId,
    };
    use lee_core::account::Data;

    const BC: ProgramId = [1, 2, 3, 4, 5, 6, 7, 8];
    const OTHER: ProgramId = [9; 8];

    fn id(n: u8) -> AccountId {
        AccountId::new([n; 32])
    }

    /// A bonding-curve index account exactly as the guest writes it: owned by
    /// the program, carrying `CREATOR_INDEX_MAGIC` and the creator it belongs
    /// to.
    fn bc_index_account(owner: ProgramId, creator: AccountId, ids: &[AccountId]) -> Account {
        let mut index = bonding_curve_core::CreatorIndex::new(creator);
        for &sale in ids {
            index.push(sale);
        }
        Account { program_owner: owner, data: Data::from(&index), ..Account::default() }
    }

    /// The bonding curve's own decode, i.e. what `bc_creator_index` passes in.
    fn bc_decode(data: &Data) -> Option<(AccountId, Vec<AccountId>)> {
        bonding_curve_core::CreatorIndex::try_from(data)
            .ok()
            .map(|index| (index.creator, index.sale_ids))
    }

    fn classify(read: super::Result<Account>) -> CreatorIndexLookup {
        classify_creator_index(BC, id(1), read, bc_decode)
    }

    /// The happy path, and the reason this exists: the index answers with the
    /// creator's whole sale list, in creation order.
    #[test]
    fn an_index_the_guest_wrote_is_authoritative() {
        let account = bc_index_account(BC, id(1), &[id(20), id(21)]);
        assert_eq!(classify(Ok(account)), CreatorIndexLookup::Indexed(vec![id(20), id(21)]));
    }

    /// An index PDA nobody has ever claimed reads back as `Account::default()`
    /// (LEZ fills the gap rather than erroring), and that IS the answer: this
    /// creator has made nothing under this program id. Downgrade it to `Unknown`
    /// and every listing pays for a full derivation again - the exact 40-minute
    /// behaviour the index was added to remove.
    #[test]
    fn a_never_touched_index_pda_is_absent() {
        assert_eq!(classify(Ok(Account::default())), CreatorIndexLookup::Absent);
    }

    /// ...and a read that FAILED is not that answer. Collapsing these two is
    /// how a sequencer blip turns into "you have no sales", which is worse than
    /// a slow listing: it is a wrong one.
    #[test]
    fn a_read_that_failed_is_unknown_not_absent() {
        assert_eq!(classify(Err("sequencer unreachable".to_owned())), CreatorIndexLookup::Unknown);
    }

    /// The address is a PDA only the launchpad guest can claim, so an account
    /// here owned by anything else is not our index however well it decodes.
    #[test]
    fn an_index_owned_by_another_program_is_unknown() {
        let account = bc_index_account(OTHER, id(1), &[id(20)]);
        assert_eq!(classify(Ok(account)), CreatorIndexLookup::Unknown);
    }

    /// The creator stored inside must be the creator we derived the PDA from -
    /// the same pin `create_sale` asserts before it appends.
    #[test]
    fn an_index_recorded_against_another_creator_is_unknown() {
        let account = bc_index_account(BC, id(2), &[id(20)]);
        assert_eq!(classify(Ok(account)), CreatorIndexLookup::Unknown);
    }

    /// Bytes we cannot explain are a reason to go and look, never a licence to
    /// report an empty listing - so non-empty data that is not an index is
    /// `Unknown`, not `Absent`.
    #[test]
    fn bytes_that_are_not_an_index_are_unknown() {
        for junk in [vec![0u8; 1], b"lpad/bci".to_vec(), vec![7u8; 64]] {
            let account = Account {
                program_owner: BC,
                data: Data::try_from(junk.clone()).expect("small"),
                ..Account::default()
            };
            assert_eq!(classify(Ok(account)), CreatorIndexLookup::Unknown, "junk {junk:?}");
        }
    }

    /// The two launchpads carry different magic (`lpad/bci` vs `lpad/lbi`), so
    /// even handed each other's bytes neither decodes the other's index. (Their
    /// PDAs differ too - the program ids differ - so this can only happen if a
    /// caller crosses the wires, which is what the test is for.)
    #[test]
    fn the_two_launchpads_do_not_read_each_others_index() {
        let bc = bc_index_account(BC, id(1), &[id(20)]);
        let lookup = classify_creator_index(BC, id(1), Ok(bc), |data| {
            lbp_core::CreatorIndex::try_from(data).ok().map(|i| (i.creator, i.pool_ids))
        });
        assert_eq!(lookup, CreatorIndexLookup::Unknown);
    }

    // --- the plan ---------------------------------------------------------

    fn plan(
        creators: &[AccountId],
        force: bool,
        answers: &[(AccountId, CreatorIndexLookup)],
    ) -> DiscoveryPlan {
        plan_discovery(creators, force, |creator| {
            answers
                .iter()
                .find(|(who, _)| *who == creator)
                .map_or(CreatorIndexLookup::Absent, |(_, a)| a.clone())
        })
    }

    /// The fast path in one assertion: a creator the index answered for is
    /// never handed to the derivation, because the index list is complete.
    #[test]
    fn an_indexed_creator_is_never_re_derived() {
        let p = plan(
            &[id(1)],
            false,
            &[(id(1), CreatorIndexLookup::Indexed(vec![id(20), id(21)]))],
        );
        assert_eq!(p.indexed, vec![id(20), id(21)]);
        assert!(p.derive.is_empty(), "an indexed creator must cost no derivation");
    }

    /// And the other half of the four-reads-not-four-thousand promise: a
    /// creator the chain says has created nothing costs nothing further. Most
    /// accounts in a real wallet are this case.
    #[test]
    fn a_creator_with_no_index_costs_no_derivation() {
        let p = plan(&[id(1), id(2)], false, &[(id(1), CreatorIndexLookup::Absent)]);
        assert!(p.indexed.is_empty());
        assert!(p.derive.is_empty(), "an absent index is an answer, not a reason to scan");
    }

    /// The fallback, and the reason the brute-force scan must stay: an index
    /// read that did not answer sends that creator - and only that creator - to
    /// the derivation.
    #[test]
    fn a_creator_whose_index_read_failed_falls_back_to_the_scan() {
        let p = plan(
            &[id(1), id(2), id(3)],
            false,
            &[
                (id(1), CreatorIndexLookup::Indexed(vec![id(20)])),
                (id(2), CreatorIndexLookup::Unknown),
                (id(3), CreatorIndexLookup::Absent),
            ],
        );
        assert_eq!(p.indexed, vec![id(20)]);
        assert_eq!(p.derive, vec![id(2)], "only the creator that did not answer is scanned");
    }

    /// `--refresh`/`--deep` hunt for a sale created before the index existed,
    /// and no index read can rule one out - so they derive for every creator
    /// while still taking whatever the index offers.
    #[test]
    fn refresh_derives_every_creator_and_still_reads_the_index() {
        let creators = [id(1), id(2), id(3)];
        let answers = [
            (id(1), CreatorIndexLookup::Indexed(vec![id(20)])),
            (id(2), CreatorIndexLookup::Unknown),
            (id(3), CreatorIndexLookup::Absent),
        ];
        let p = plan(&creators, true, &answers);
        assert_eq!(p.indexed, vec![id(20)]);
        assert_eq!(p.derive, creators.to_vec());
        // ...and each creator is listed once, so a re-derivation is not doubled.
        let mut once = p.derive.clone();
        once.dedup();
        assert_eq!(once, p.derive);
    }

    /// Ids keep the order the chain gave them (creator order, then the guest's
    /// append order, which is creation order) and are deduped, so a sale cannot
    /// be listed twice if two creator candidates resolve to one index.
    #[test]
    fn the_plan_keeps_creation_order_and_dedups() {
        let p = plan(
            &[id(1), id(2)],
            false,
            &[
                (id(1), CreatorIndexLookup::Indexed(vec![id(30), id(31)])),
                (id(2), CreatorIndexLookup::Indexed(vec![id(31), id(32)])),
            ],
        );
        assert_eq!(p.indexed, vec![id(30), id(31), id(32)]);
    }
}

#[cfg(test)]
mod owner_pinned_reads {
    //! Reading launchpad state authenticates the OWNER, not just the bytes.
    //!
    //! LEZ deployment is permissionless, so "these bytes Borsh-decode as a
    //! `SaleState`" is a statement anybody can arrange about an account they
    //! control. Offline: both functions under test are pure.

    use super::{decode_bc_sale, decode_lbp_pool, Account, PoolState, ProgramId, SaleState};
    use lee_core::account::Data;

    const BC: ProgramId = [1, 2, 3, 4, 5, 6, 7, 8];
    const LBP: ProgramId = [8, 7, 6, 5, 4, 3, 2, 1];
    /// Some stranger's program, deployed by anyone who wants one.
    const IMPOSTOR: ProgramId = [9; 8];

    /// A sale a stranger would be happy to have you read: plausible fee,
    /// plausible reserves, and the name `bc sale-info` prints first.
    fn sale_account(owner: ProgramId) -> Account {
        let state = SaleState {
            fee_bps: 30,
            token_name: "TOTALLY REAL".to_owned(),
            token_symbol: "REAL".to_owned(),
            sale_reserve: 1_000_000,
            ..Default::default()
        };
        Account { program_owner: owner, data: Data::from(&state), ..Account::default() }
    }

    fn pool_account(owner: ProgramId) -> Account {
        let state = PoolState {
            token_name: "TOTALLY REAL".to_owned(),
            token_symbol: "REAL".to_owned(),
            ..Default::default()
        };
        Account { program_owner: owner, data: Data::from(&state), ..Account::default() }
    }

    /// The ordinary read still works - the pin must not cost the real path.
    #[test]
    fn a_sale_owned_by_the_bonding_curve_program_decodes() {
        let s = decode_bc_sale(&sale_account(BC), BC).expect("a genuine sale must read");
        assert_eq!(s.token_symbol, "REAL");
        let p = decode_lbp_pool(&pool_account(LBP), LBP).expect("a genuine pool must read");
        assert_eq!(p.token_symbol, "REAL");
    }

    /// The fix. Identical bytes under someone else's program are not a sale,
    /// however cleanly they decode - and without the owner pin this is exactly
    /// what `bc sale-info` and `quote-live` would present as genuine.
    #[test]
    fn an_impostor_programs_account_is_not_a_sale_however_well_its_bytes_decode() {
        let account = sale_account(IMPOSTOR);
        assert!(
            SaleState::try_from(&account.data).is_ok(),
            "fixture must decode, or this test proves nothing about the OWNER check"
        );
        let err = decode_bc_sale(&account, BC).expect_err("a stranger's account is not a sale");
        assert!(err.contains("not owned by the bonding-curve program"), "{err}");
    }

    /// Same for the LBP, and the two do not accept each other's accounts
    /// either: the pin is against THIS build's program id.
    #[test]
    fn an_impostor_programs_account_is_not_a_pool_however_well_its_bytes_decode() {
        let err =
            decode_lbp_pool(&pool_account(IMPOSTOR), LBP).expect_err("a stranger's is not a pool");
        assert!(err.contains("not owned by the LBP program"), "{err}");
        assert!(decode_lbp_pool(&pool_account(BC), LBP).is_err(), "wrong launchpad's program id");
    }

    /// An account that does not exist reads back as `Account::default()`, and
    /// that is not a sale either - the pin catches it before the decode.
    #[test]
    fn an_empty_account_is_not_a_sale() {
        assert!(decode_bc_sale(&Account::default(), BC).is_err());
        assert!(decode_lbp_pool(&Account::default(), LBP).is_err());
    }
}

#[cfg(test)]
mod discovery_resolution {
    //! Which failed reads a listing is allowed to swallow.
    //!
    //! `read` returns the same `Err` for "this is not a sale" and "the
    //! sequencer did not answer", so the caller's own knowledge of WHERE the id
    //! came from is the only thing that can tell them apart. Offline.

    use std::collections::HashSet;

    use super::{
        discovery_noun, resolve_candidates, AccountId, DISCOVERY_KIND_BC, DISCOVERY_KIND_LBP,
    };

    fn id(n: u8) -> AccountId {
        AccountId::new([n; 32])
    }

    fn authoritative(ids: &[AccountId]) -> HashSet<AccountId> {
        ids.iter().copied().collect()
    }

    /// Reads that succeed for everything but `broken`.
    fn reader(broken: &'static [u8]) -> impl Fn(AccountId) -> super::Result<u8> {
        move |i: AccountId| {
            let n = i.to_bytes()[0];
            if broken.contains(&n) {
                Err(format!("read account {n}: sequencer 502"))
            } else {
                Ok(n)
            }
        }
    }

    /// The fix, and the whole point of the split. An id the on-chain index
    /// named is the chain's own statement that the sale EXISTS, so a read that
    /// fails on it is a failure to report - never a row to delete. Before this,
    /// a lagging sequencer silently shortened `my-sales` and exited 0.
    #[test]
    fn a_failed_read_on_an_indexed_id_is_reported_not_dropped() {
        let candidates = vec![id(1), id(2), id(3)];
        let (found, unreadable) =
            resolve_candidates(candidates, &authoritative(&[id(1), id(2), id(3)]), reader(&[2]));
        assert_eq!(found.iter().map(|&(i, _)| i).collect::<Vec<_>>(), vec![id(1), id(3)]);
        assert_eq!(
            unreadable.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
            vec![id(2)],
            "an index-listed sale that could not be read must surface, not vanish"
        );
        assert!(unreadable[0].1.contains("502"), "the read's own error has to survive");
    }

    /// And the other half: the derivation invents thousands of ids that are not
    /// sales, so its failures stay silent. A warning per speculative miss would
    /// bury the one that matters.
    #[test]
    fn a_failed_read_on_a_speculative_id_stays_silent() {
        let candidates = vec![id(1), id(2), id(3)];
        // Nothing is authoritative: every id here came from the derivation.
        let (found, unreadable) =
            resolve_candidates(candidates, &authoritative(&[]), reader(&[2, 3]));
        assert_eq!(found.iter().map(|&(i, _)| i).collect::<Vec<_>>(), vec![id(1)]);
        assert!(unreadable.is_empty(), "a speculative probe that misses is the expected answer");
    }

    /// The mixed run, which is what `--refresh` actually is: the same failing
    /// sequencer, and only the ids something vouched for are reported.
    #[test]
    fn only_the_vouched_for_ids_are_reported_when_both_paths_ran() {
        let candidates = vec![id(1), id(2), id(3), id(4)];
        // 1 and 2 came from the index or the cache; 3 and 4 were derived.
        let (found, unreadable) =
            resolve_candidates(candidates, &authoritative(&[id(1), id(2)]), reader(&[2, 4]));
        assert_eq!(found.iter().map(|&(i, _)| i).collect::<Vec<_>>(), vec![id(1), id(3)]);
        assert_eq!(unreadable.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![id(2)]);
    }

    /// A cached id is as authoritative as an indexed one: the cache is
    /// append-only and only ever holds ids that resolved once (or that the
    /// index named), so a failed read on one is a failure too.
    #[test]
    fn a_cached_id_that_stops_reading_is_reported() {
        let (_, unreadable) =
            resolve_candidates(vec![id(7)], &authoritative(&[id(7)]), reader(&[7]));
        assert_eq!(unreadable.len(), 1, "a remembered sale does not silently stop existing");
    }

    /// The split above is only worth making if it reaches the caller, and until
    /// now it did not: `discover` printed the unreadable ids to stderr and then
    /// returned `found` alone. A `--json` consumer sees no stderr, so it got a
    /// well-formed listing with a row missing and an exit code of 0 - the exact
    /// answer "that sale does not exist", which is what this whole split was
    /// built to stop saying.
    ///
    /// [`super::Listing`] carries both halves out, and `partial()` is the one
    /// bit a caller has to branch on.
    #[test]
    fn a_listing_that_missed_something_reports_it_as_partial() {
        let (found, unreadable) =
            resolve_candidates(vec![id(1), id(2)], &authoritative(&[id(1), id(2)]), reader(&[2]));
        let listing = super::Listing { found, unreadable };
        assert!(listing.partial(), "a listing missing an index-listed id must know it is short");
        assert_eq!(listing.found.len(), 1, "the rows that DID read are still delivered");
        assert_eq!(listing.unreadable[0].0, id(2), "and the id that did not is named");

        let (found, unreadable) =
            resolve_candidates(vec![id(1)], &authoritative(&[id(1)]), reader(&[]));
        let listing = super::Listing { found, unreadable };
        assert!(!listing.partial(), "a complete listing must not cry wolf");
    }

    /// The warning names a sale or a pool, and it takes the noun from the same
    /// `kind` the cache key uses so the two cannot drift.
    #[test]
    fn the_warning_calls_it_by_the_right_name() {
        assert_eq!(discovery_noun(DISCOVERY_KIND_BC), "sale");
        assert_eq!(discovery_noun(DISCOVERY_KIND_LBP), "pool");
    }
}

#[cfg(test)]
mod sweep_signers {
    //! Who must sign a `SweepTreasury`. Offline: the decision is a pure
    //! function of the treasury account, which is what lets it be an error
    //! instead of a transaction.

    use super::{sweep_treasury_signers_for, Account, AccountId, ProgramId, DEFAULT_PROGRAM_ID};
    use lee_core::account::Nonce;

    const TOKEN: ProgramId = [4; 8];

    fn treasury() -> AccountId {
        AccountId::new([7; 32])
    }

    /// The only case that reaches a real sweep: `CreateSale` pinned an
    /// initialised holding, so the treasury is token-program-owned and the
    /// instruction is permissionless.
    #[test]
    fn a_live_treasury_needs_no_signature() {
        let acc = Account { program_owner: TOKEN, ..Account::default() };
        assert_eq!(sweep_treasury_signers_for(treasury(), &acc), Ok(Vec::new()));
    }

    /// The fix. A pristine, unowned treasury used to be handed back as its own
    /// signer, on the theory that its signature could bootstrap the account.
    /// Both guests now `assert_ne!(treasury.program_owner, DEFAULT_PROGRAM_ID)`
    /// before they look at any authorization, so that signature only ever
    /// bought the operator a transaction they paid for and the guest reverted.
    #[test]
    fn a_pristine_unowned_treasury_is_an_error_not_a_signature() {
        let err = sweep_treasury_signers_for(treasury(), &Account::default())
            .expect_err("the guests reject this outright; signing for it cannot help");
        assert!(err.contains("unowned"), "{err}");
        assert!(
            err.contains("report this"),
            "the state is unreachable from CreateSale, so the remedy is a bug report: {err}"
        );
        assert!(
            !err.contains("run the sweep from the wallet"),
            "the old remedy sent the operator to another wallet, which cannot work either: {err}"
        );
    }

    /// A nonce-bumped or funded unowned treasury is the same verdict, and it
    /// always was - no program may claim an account that is not whole-default.
    #[test]
    fn a_non_pristine_unowned_treasury_is_an_error_too() {
        let acc =
            Account { program_owner: DEFAULT_PROGRAM_ID, nonce: Nonce(1), ..Account::default() };
        assert!(sweep_treasury_signers_for(treasury(), &acc).is_err());
    }
}

#[cfg(test)]
mod discovery_cache {
    //! The id cache: written next to the wallet, keyed by program id, and never
    //! fatal. All offline - it is a file, not a chain.

    use super::{
        account_from_hex, cached_ids, discovery_key, hex_id, load_discovery_cache, merged_ids,
        store_discovery_cache, AccountId, CacheEntry, DiscoveryCache, ProgramId,
        DISCOVERY_CACHE_VERSION, DISCOVERY_KIND_BC, DISCOVERY_KIND_LBP,
    };
    use std::path::PathBuf;

    fn id(n: u8) -> AccountId {
        AccountId::new([n; 32])
    }

    /// A private directory per test, so these can run in parallel with each other
    /// and with anything else on the machine.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "lpad-discovery-{}-{tag}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::create_dir_all(&dir).expect("temp dir");
            Self(dir)
        }
        fn cache(&self) -> PathBuf {
            self.0.join("lpad_discovery.json")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write(path: &std::path::Path, key: &str, entry: CacheEntry) {
        let mut cache = DiscoveryCache { version: DISCOVERY_CACHE_VERSION, ..Default::default() };
        cache.entries.insert(key.to_owned(), entry);
        store_discovery_cache(path, &cache);
    }

    const BC: ProgramId = [1, 2, 3, 4, 5, 6, 7, 8];
    const REBUILT: ProgramId = [1, 2, 3, 4, 5, 6, 7, 9];

    /// The round trip the fast path depends on: what a scan found comes back in
    /// the same order, still marked as a completed scan.
    #[test]
    fn ids_round_trip_through_the_file() {
        let dir = TempDir::new("roundtrip");
        let key = discovery_key(DISCOVERY_KIND_BC, BC);
        let ids = vec![hex_id(id(9)), hex_id(id(4)), hex_id(id(7))];
        write(&dir.cache(), &key, CacheEntry { ids: ids.clone() });

        let back = load_discovery_cache(&dir.cache());
        let entry = back.entries.get(&key).expect("entry survives the round trip");
        assert_eq!(cached_ids(entry), vec![id(9), id(4), id(7)], "order is the found order");
    }

    /// Rebuilding the guests moves every PDA. A cache keyed only by kind would
    /// hand back ids on a program that is no longer deployed; keying by program
    /// id makes that a plain MISS, which falls back to the full scan.
    #[test]
    fn a_cache_keyed_to_another_program_id_is_a_miss() {
        let dir = TempDir::new("progid");
        write(
            &dir.cache(),
            &discovery_key(DISCOVERY_KIND_BC, BC),
            CacheEntry { ids: vec![hex_id(id(1))] },
        );
        let back = load_discovery_cache(&dir.cache());
        assert!(
            !back.entries.contains_key(&discovery_key(DISCOVERY_KIND_BC, REBUILT)),
            "a rebuilt program must not read another program's ids"
        );
        // ...and the two launchpads never read each other's ids either.
        assert!(!back.entries.contains_key(&discovery_key(DISCOVERY_KIND_LBP, BC)));
        assert_ne!(discovery_key(DISCOVERY_KIND_BC, BC), discovery_key(DISCOVERY_KIND_BC, REBUILT));
    }

    /// A corrupt cache must cost a rescan, not a command. Truncated JSON, a JSON
    /// value of the wrong shape, and outright garbage all read as empty.
    #[test]
    fn a_corrupt_cache_is_a_miss_not_a_failure() {
        let dir = TempDir::new("corrupt");
        for junk in ["", "{", "not json at all", r#"{"version":1,"entries":[]}"#, "\0\0\0"] {
            std::fs::write(dir.cache(), junk).expect("write junk");
            let back = load_discovery_cache(&dir.cache());
            assert!(back.entries.is_empty(), "corrupt cache {junk:?} must read as empty");
        }
    }

    /// A file this build cannot interpret is a miss too - never a wrong answer.
    /// Both directions: a version from the future, and the v1 file (with its
    /// since-removed `scanned` flag) an upgrade leaves behind.
    #[test]
    fn a_version_this_build_does_not_write_is_a_miss() {
        let dir = TempDir::new("version");
        for version in [DISCOVERY_CACHE_VERSION + 1, 1] {
            std::fs::write(
                dir.cache(),
                format!(
                    r#"{{"version":{version},"entries":{{"bc:00":{{"scanned":true,"ids":["{}"]}}}}}}"#,
                    hex_id(id(1))
                ),
            )
            .expect("write");
            assert!(
                load_discovery_cache(&dir.cache()).entries.is_empty(),
                "version {version} must read as a miss"
            );
        }
    }

    /// The ordinary first run: no file yet.
    #[test]
    fn an_absent_cache_is_a_miss() {
        let dir = TempDir::new("absent");
        assert!(load_discovery_cache(&dir.cache()).entries.is_empty());
        assert!(load_discovery_cache(&dir.0.join("nope/deeper/none.json")).entries.is_empty());
    }

    /// One mangled id costs that id, not the file.
    #[test]
    fn a_single_unparseable_id_does_not_poison_the_entry() {
        let entry =
            CacheEntry { ids: vec![hex_id(id(1)), "zzzz".to_owned(), String::new(), hex_id(id(2))] };
        assert_eq!(cached_ids(&entry), vec![id(1), id(2)]);
        assert!(account_from_hex("zz".repeat(32).as_str()).is_none(), "64 chars but not hex");
        assert!(account_from_hex(&hex_id(id(3))[..63]).is_none(), "wrong length");
        assert_eq!(account_from_hex(&hex_id(id(3))), Some(id(3)));
    }

    /// Append-only. A probe that failed (flaky sequencer, not a vanished sale)
    /// must never evict a remembered id, and a re-derivation must never shrink
    /// the list.
    #[test]
    fn the_id_list_only_ever_grows() {
        let (a, b, c) = (hex_id(id(1)), hex_id(id(2)), hex_id(id(3)));
        // Nothing resolved this run - everything remembered survives.
        assert_eq!(merged_ids(&[], &[a.clone(), b.clone()]), vec![a.clone(), b.clone()]);
        // A new sale lands at the front (found order), the old one is kept.
        assert_eq!(merged_ids(&[c.clone(), a.clone()], &[a.clone(), b.clone()]), vec![c, a.clone(), b]);
        // No duplicates when the same id is found and remembered.
        assert_eq!(merged_ids(std::slice::from_ref(&a), std::slice::from_ref(&a)), vec![a]);
    }

    /// A write replaces the file atomically and leaves no `.tmp` behind, so a
    /// half-written cache can never be the thing the next run reads.
    #[test]
    fn writing_leaves_exactly_one_file() {
        let dir = TempDir::new("atomic");
        let key = discovery_key(DISCOVERY_KIND_BC, BC);
        write(&dir.cache(), &key, CacheEntry { ids: vec![hex_id(id(1))] });
        write(&dir.cache(), &key, CacheEntry { ids: vec![hex_id(id(2))] });
        let files: Vec<String> = std::fs::read_dir(&dir.0)
            .expect("read dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(files, vec!["lpad_discovery.json".to_owned()]);
        let entry = load_discovery_cache(&dir.cache()).entries.remove(&key).expect("entry");
        assert_eq!(cached_ids(&entry), vec![id(2)]);
    }
}

#[cfg(test)]
mod creator_settlement {
    //! Which creator accounts a sale can still be closed and withdrawn from.
    //!
    //! Offline, because the rule is not about the launchpad guests at all: it is
    //! `validate_execution` rule 7 plus the nonce bump `apply_state_diff` does to
    //! every signer, and both are decided from the creator's pre-state alone.
    //! The live consequence is the expensive part - a sale created from the
    //! wrong shape of account can never be closed and never be withdrawn from,
    //! and the D+R deposit plus every unit of collateral raised stays in the
    //! vaults for good.

    use super::{ensure_creator_settleable_for, hex_id, AccountId};
    use lee_core::{
        account::{Account, Nonce},
        program::DEFAULT_PROGRAM_ID,
    };

    fn id(n: u8) -> AccountId {
        AccountId::new([n; 32])
    }

    /// What scripts/bootstrap.sh makes: initialised under authenticated-transfer
    /// before it ever signs. Its nonce has moved on since, and that is fine -
    /// rule 7 only looks at accounts whose POST state is DEFAULT-owned.
    #[test]
    fn the_bootstrap_creator_is_settleable() {
        let acc = Account {
            program_owner: programs::authenticated_transfer().id(),
            nonce: Nonce(9),
            ..Account::default()
        };
        assert_eq!(ensure_creator_settleable_for(id(1), &acc), Ok(()));
    }

    /// The integration fixtures create their creator with
    /// `force_insert_account(i.creator, fungible(..))`, i.e. token-program-owned.
    /// Also settleable - which is precisely why the suite never saw the bug the
    /// private launch shipped with.
    #[test]
    fn a_token_program_owned_creator_is_settleable() {
        let acc = Account { program_owner: programs::token().id(), ..Account::default() };
        assert_eq!(ensure_creator_settleable_for(id(2), &acc), Ok(()));
    }

    /// The trap. A brand-new keypair is whole-default, so the create SUCCEEDS
    /// and the sale looks fine; the nonce bump that same transaction applies is
    /// what makes every later close/withdraw impossible. Refuse before the
    /// signature, and name the command that fixes it.
    #[test]
    fn a_never_initialised_creator_is_refused_before_it_signs() {
        let e = ensure_creator_settleable_for(id(3), &Account::default())
            .expect_err("a whole-default creator must not be allowed to sign a create");
        assert!(e.contains(&hex_id(id(3))), "the error must name the account: {e}");
        assert!(
            e.contains("wallet auth-transfer init"),
            "the error must name the command that fixes it: {e}"
        );
    }

    /// One transaction too late: DEFAULT-owned with a bumped nonce is what a
    /// fresh creator BECOMES by signing its own `CreateSale`. Nothing recovers
    /// it - `Initialize` claims only a whole-default account - so the remedy has
    /// to be a different account, not an init.
    #[test]
    fn a_default_owned_creator_that_has_signed_is_beyond_rescue() {
        let signed = Account { nonce: Nonce(1), ..Account::default() };
        assert_eq!(signed.program_owner, DEFAULT_PROGRAM_ID);
        let e = ensure_creator_settleable_for(id(4), &signed)
            .expect_err("a DEFAULT-owned account with a bumped nonce can never be echoed again");
        assert!(e.contains("wallet account new public"), "the remedy is a fresh account: {e}");

        // Paid but never initialised is the same verdict: not whole-default, so
        // the claim is refused and rule 7 keeps refusing it forever.
        let paid = Account { balance: 1, ..Account::default() };
        assert!(ensure_creator_settleable_for(id(5), &paid).is_err());
    }
}

#[cfg(test)]
mod unlinkable_launch {
    //! The two guarantees `bc_create_token_sale_private` sells and the two ways
    //! it lost them: a public bootstrap that publishes the real creator beside
    //! the project token, and a throwaway creator that could sign exactly once.
    //! Both decisions are pure, so both are pinned here without a chain.

    use super::{
        deanonymising_account, public_slot_id, wlez_bootstrap, wlez_initialize_accounts, AccountId,
        LaunchLinkage, WlezBootstrap,
    };
    use wallet::AccountIdentity;

    fn id(n: u8) -> AccountId {
        AccountId::new([n; 32])
    }

    const WLEZ: [u32; 8] = [3u32; 8];

    /// The join an observer makes: ONE transaction that names both the payer and
    /// the reference definition. Point it at the project's own definition - what
    /// the private launch used to do - and that definition is also the sale's
    /// public `token_definition_id`, so the pair resolves the sale's throwaway
    /// creator to the wallet behind it with no heuristics at all.
    #[test]
    fn a_wlez_initialize_publishes_its_payer_beside_the_reference_definition() {
        let accounts = wlez_initialize_accounts(WLEZ, id(7), id(8));
        assert_eq!(accounts.len(), 5, "the guest destructures exactly five accounts");
        assert_eq!(accounts[3], id(7), "the reference definition is published");
        assert_eq!(accounts[4], id(8), "so is the payer, in the same transaction");
    }

    /// So an unlinkable launch never sends it - with any payer, and whatever the
    /// probe's reason was. The refusal has to name the public command that does
    /// the bootstrap instead, because otherwise the operator's only obvious move
    /// is to launch publicly.
    #[test]
    fn an_unlinkable_launch_refuses_to_bootstrap_wlez() {
        let e = wlez_bootstrap(LaunchLinkage::Unlinkable, Err("no WLEZ definition".into()))
            .expect_err("a private launch must not initialise wlez");
        assert!(e.contains("lpad wrap"), "the refusal must name the public remedy: {e}");
        assert!(e.contains("no WLEZ definition"), "and must carry the probe's reason: {e}");
    }

    /// A public launch may pay for the bootstrap - its creator is on chain
    /// anyway - but only when the probe says the chain needs it. A wlez that is
    /// already initialised and canonically pinned costs no transaction now,
    /// where the client used to re-send `Initialize` on every single launch.
    #[test]
    fn a_ready_wlez_is_bootstrapped_by_nobody() {
        assert_eq!(wlez_bootstrap(LaunchLinkage::Public, Ok(())), Ok(WlezBootstrap::Done));
        assert_eq!(wlez_bootstrap(LaunchLinkage::Unlinkable, Ok(())), Ok(WlezBootstrap::Done));
        assert_eq!(
            wlez_bootstrap(LaunchLinkage::Public, Err("not initialised".into())),
            Ok(WlezBootstrap::Needed)
        );
    }

    /// The tripwire catches the real creator in ANY slot, not just the payer
    /// slot it leaked from - the next leak will be a different transaction.
    #[test]
    fn the_tripwire_spots_the_real_creator_anywhere_in_the_list() {
        let real = id(8);
        let leak = wlez_initialize_accounts(WLEZ, id(7), real);
        assert_eq!(deanonymising_account(&leak, Some(real)), Some(real));
        assert_eq!(deanonymising_account(&[real, id(1)], Some(real)), Some(real));
        assert_eq!(deanonymising_account(&[id(1), id(2)], Some(real)), None);
    }

    /// Disarmed is silent: every public command in this SDK submits through the
    /// same function, and none of them may pay for this check with a false
    /// refusal.
    #[test]
    fn the_tripwire_is_inert_outside_an_unlinkable_launch() {
        assert_eq!(deanonymising_account(&[id(1), id(8)], None), None);
    }

    /// A privacy transaction hides its private slots and publishes its public
    /// ones, so only the public ones are the tripwire's business - and all three
    /// public flavours count.
    #[test]
    fn only_the_public_slots_of_a_privacy_transaction_are_published() {
        assert_eq!(public_slot_id(&AccountIdentity::Public(id(1))), Some(id(1)));
        assert_eq!(public_slot_id(&AccountIdentity::PublicNoSign(id(2))), Some(id(2)));
        assert_eq!(
            public_slot_id(&AccountIdentity::PublicKeycard {
                account_id: id(3),
                key_path: String::new()
            }),
            Some(id(3))
        );
        assert_eq!(public_slot_id(&AccountIdentity::PrivateOwned(id(4))), None);
    }
}

#[cfg(test)]
mod private_sync {
    //! The bound on [`super::LaunchpadClient::sync_private`]. All offline: the
    //! decision is two block heights and a clock, so it lives in a pure function
    //! and is tested as one - no wallet, no chain, no sleeping test.

    use super::{secs_or, sync_step, SyncStep, DEFAULT_SYNC_MAX_SECS, DEFAULT_SYNC_STALL_SECS};
    use std::time::Duration;

    const BUDGET: Duration = Duration::from_secs(3_600);

    /// The 5h33m wedge, written down as a test. A window that scanned nothing
    /// is a dead stream, and the only correct answer is to stop. If this ever
    /// returns `Retry`, the loop is unbounded again and the bug is back.
    #[test]
    fn a_window_that_scanned_no_block_gives_up() {
        assert_eq!(
            sync_step(900, 900, Duration::from_secs(1), BUDGET),
            SyncStep::Stalled
        );
    }

    /// Progress buys another window. A first scan of a long chain is slow, not
    /// broken; a bound that killed it on wall clock alone would be a worse
    /// failure than the hang it replaced, because it would fail work that was
    /// about to succeed.
    #[test]
    fn progress_buys_another_window() {
        assert_eq!(
            sync_step(900, 901, Duration::from_secs(1), BUDGET),
            SyncStep::Retry
        );
        assert_eq!(
            sync_step(0, 250_000, Duration::from_secs(3_599), BUDGET),
            SyncStep::Retry
        );
    }

    /// The backstop. A chain that produces blocks faster than this wallet scans
    /// them makes progress in every window forever, so progress alone cannot
    /// terminate the loop and something has to.
    #[test]
    fn a_healthy_sync_still_ends_when_the_budget_does() {
        assert_eq!(sync_step(900, 950, BUDGET, BUDGET), SyncStep::Exhausted);
        assert_eq!(
            sync_step(900, 950, Duration::from_secs(3_601), BUDGET),
            SyncStep::Exhausted
        );
    }

    /// Both bounds tripped in the same window: "the stream is dead" is the
    /// diagnosis worth printing, and "this chain is fast" is the one that would
    /// send the reader after the wrong thing entirely.
    #[test]
    fn stalled_outranks_exhausted() {
        assert_eq!(sync_step(900, 900, BUDGET, BUDGET), SyncStep::Stalled);
    }

    /// `last_synced_block` cannot go backwards. A policy that answered `Retry`
    /// if it ever did would spin forever on the one state it must not, so the
    /// comparison is `<=`, not `==`.
    #[test]
    fn a_last_synced_block_that_went_backwards_is_not_progress() {
        assert_eq!(
            sync_step(900, 899, Duration::from_secs(1), BUDGET),
            SyncStep::Stalled
        );
    }

    /// A typo in the environment degrades to the default rather than bricking
    /// every private command - and `0`, which would fail every sync on its first
    /// tick, is a typo too.
    #[test]
    fn only_a_positive_number_overrides_the_default() {
        assert_eq!(secs_or(Some("30"), DEFAULT_SYNC_STALL_SECS), 30);
        assert_eq!(secs_or(Some("  30\n"), DEFAULT_SYNC_STALL_SECS), 30);
        assert_eq!(
            secs_or(None, DEFAULT_SYNC_STALL_SECS),
            DEFAULT_SYNC_STALL_SECS
        );
        for typo in ["5m", "", "0", "-1", "300s", "three hundred"] {
            assert_eq!(
                secs_or(Some(typo), DEFAULT_SYNC_MAX_SECS),
                DEFAULT_SYNC_MAX_SECS,
                "{typo:?} is not a seconds count"
            );
        }
    }

    /// One stall window has to fit inside the budget with room to spare, or no
    /// sync ever gets a second window and the retry path - the whole reason the
    /// bound is a stall bound rather than a timeout - is unreachable in the
    /// shipped defaults.
    #[test]
    fn the_default_budget_is_worth_more_than_one_window() {
        const {
            assert!(
                DEFAULT_SYNC_STALL_SECS < DEFAULT_SYNC_MAX_SECS,
                "one stall window must fit inside the sync budget"
            );
        }
        assert_eq!(
            sync_step(
                900,
                901,
                Duration::from_secs(DEFAULT_SYNC_STALL_SECS),
                Duration::from_secs(DEFAULT_SYNC_MAX_SECS)
            ),
            SyncStep::Retry
        );
    }
}
