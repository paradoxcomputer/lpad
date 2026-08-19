//! `lpad` - command-line client for the LPAD launchpad platform.
//!
//! Offline commands compute quotes, costs, weights, and PDA addresses for both
//! the bonding-curve (RFP-015) and LBP (RFP-016) programs directly from the
//! on-chain pricing libraries (byte-identical to the programs). Online commands
//! open the LEZ wallet to read sale/pool state and build/sign/submit
//! transactions (public path). See `online.rs`.

use clap::{Parser, Subcommand};
use lee_core::account::AccountId;

use lpad_cli::fmt::{hex_account, parse_account, parse_program, parse_proof, parse_root, parse_weight_q64, q64_to_f64};
use lpad_cli::online::{self, WalletPaths};
use lpad_cli::ui;

#[derive(Parser, Debug)]
#[command(name = "lpad", version, about = "Logos Privacy-Preserving Token Launchpad CLI")]
struct Cli {
    /// Emit machine-readable JSON instead of human-readable text.
    #[arg(long, global = true)]
    json: bool,
    /// Wallet config path (default: $LEE_WALLET_HOME_DIR/wallet_config.json).
    #[arg(long, global = true)]
    config: Option<String>,
    /// Wallet storage path (default: $LEE_WALLET_HOME_DIR/storage.json).
    #[arg(long, global = true)]
    storage: Option<String>,
    /// Sequencer to target: `testnet`, `paradox`, `local`, `local=<url>`, or an
    /// http(s) URL. Omitted, the network remembered for this wallet is used (and
    /// on first run, `lpad` asks - see `lpad network`). Overrides the remembered
    /// choice for this invocation ONLY - it is never written back - and only the
    /// sequencer, not the wallet home. lpad bundles no local sequencer: `local`
    /// is a LEZ node you run yourself.
    #[arg(long, global = true)]
    network: Option<String>,
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
    /// Choose which sequencer this wallet talks to, and remember it.
    ///
    /// Re-opens the first-run picker and writes the answer next to the wallet,
    /// so later commands are silent. With `--json` it prints the current
    /// selection instead of asking. Needs a terminal: a run with no TTY reports
    /// how to set the choice non-interactively rather than waiting on stdin.
    ///
    /// Picking a sequencer you run yourself also checks whether lpad's three
    /// programs are on that chain, and offers to deploy them if not.
    Network {
        /// Ask whether this build's three programs are on the chain, without
        /// picking anything and without a terminal.
        ///
        /// The same check the picker runs, reachable from a script. It exits 1
        /// when a program has no record, because a transaction against a
        /// program that is not deployed is dropped from the mempool and reads
        /// as a timeout, not an error - so a script that carries on burns its
        /// whole poll budget to learn nothing. "No record" is not proof of
        /// absence, though: a chain that was reset can have forgotten the
        /// deploy transaction while still running the program, so the message
        /// says so and `scripts/verify-deployment.sh` is the stronger answer.
        #[arg(long)]
        check: bool,
    },
    /// Create a brand-new wallet: fresh keys, and the recovery phrase printed once.
    ///
    /// The first command a new install can run. Everything else needs a wallet to
    /// already exist, and until this landed nothing in lpad could make one - only
    /// the LEZ `wallet` binary could, which meant a LEZ checkout for anyone who
    /// had installed the CLI on its own.
    ///
    /// Refuses to run if a wallet is already there. It asks for no password,
    /// because the one upstream accepts is discarded (`Storage::new` is a
    /// `TODO: Use password for storage encryption`) - `storage.json` is plain
    /// JSON holding raw keys, so it is created `0600` and the output says so.
    InitWallet,
    /// Print a program's deployed id (RISC0 image id) from its guest ELF.
    ProgramId {
        /// Which program: `bc`, `lbp`, `ata`, or `wlez`.
        which: String,
    },
    /// [online] Your token balances across every wallet account (public + shielded).
    MyBalance,
    /// [online] Bonding-curve sales your wallet created.
    ///
    /// The bonding-curve program records every sale it creates in a per-creator
    /// index account on chain, so this is one read per account of yours - not a
    /// search. It sees a sale created on another machine holding the same key,
    /// or in a wallet home restored from backup, with no flag needed. The state
    /// printed is always read live.
    ///
    /// (There is still no event indexer (LP-0012), so listing OTHER people's
    /// sales is not possible - only your own.)
    MySales {
        /// Also re-derive and probe every candidate id. Needed only for a sale
        /// created by an older lpad build, from before the on-chain index
        /// existed: nothing recorded those, so finding one means guessing every
        /// PDA your wallet could have made. Very slow (thousands of reads), and
        /// a one-off - whatever it finds is remembered for later runs. Only
        /// adds to what is listed, never drops.
        #[arg(long)]
        refresh: bool,
        /// Treat EVERY public account as a possible creator, not just identity
        /// accounts (implies --refresh). Needed only for a sale created with
        /// `--creator <a token holding>`; the index half of that is cheap and
        /// usually answers on its own.
        #[arg(long)]
        deep: bool,
    },
    /// [online] LBP pools your wallet created.
    ///
    /// Same discovery and flags as `my-sales`, against the LBP program's own
    /// per-creator index.
    MyPools {
        /// Also re-derive and probe every candidate id - see `my-sales
        /// --refresh`. Needed only for a pool created before the on-chain index
        /// existed. Very slow, and only ever adds to what is listed.
        #[arg(long)]
        refresh: bool,
        /// Treat EVERY public account as a possible creator, not just identity
        /// accounts (implies --refresh). Needed only for a pool created with
        /// `--creator <a token holding>`.
        #[arg(long)]
        deep: bool,
    },
    /// [online] Claim native LEZ from this chain's pinata faucet (mines its proof-of-work).
    ///
    /// The faucet checks no signature and no allowlist: it pays whoever submits
    /// a proof-of-work solution for the challenge in its own account, so a
    /// wallet with nothing in it can fund itself. lpad solves that puzzle
    /// locally - one core, seconds at the difficulty LEZ ships, and each extra
    /// difficulty byte costs 256x more work (`LPAD_FAUCET_MAX_ATTEMPTS=<n>`
    /// raises the give-up budget). This is the first step of the first-run
    /// story: `faucet` -> `wrap` -> `bc create-token-sale` -> `bc buy`.
    ///
    /// The chain sets no cooldown and keeps no record of who was paid, so
    /// running it again claims again. What can fail is a race: every successful
    /// claim re-seeds the challenge, so a claim that lands while lpad is mining
    /// invalidates the solution being searched for - and the remedy is just to
    /// re-run.
    ///
    /// `--to` names the recipient; omitted, the wallet's default signing account
    /// is used. An account that has never been initialised under the
    /// authenticated-transfer program cannot receive native LEZ at all, so it is
    /// initialised first (one extra transaction) when this wallet holds its key.
    Faucet {
        #[arg(
            long,
            help = "recipient account (default: this wallet's default signing account); \
                    initialised for native LEZ first if it never was"
        )]
        to: Option<String>,
    },
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
    /// [online] Create + initialise a fresh public token holding for a token definition.
    ///
    /// Prints the new holding's id. A transfer to a never-initialised holding is
    /// rejected (the token program claims the recipient as Authorized, which the
    /// runtime only grants to a signer), and the LEZ wallet has no equivalent
    /// command, so use this to make a recipient before sending to it.
    ///
    /// It is also the required FIRST step of `bc create-sale` / `lbp
    /// create-sale`: both programs reject a create whose `--treasury` is not
    /// already an initialised holding of the sale's collateral definition. Run
    /// it against `--collateral-def` and pass the id it prints as `--treasury`.
    InitHolding {
        #[arg(long = "token-def")]
        token_def: String,
    },
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
    ///
    /// Prints the sale id, the token definition, the sale's CREATOR and the
    /// creator's token holding. Record the last two: `bc close --creator` and
    /// `bc withdraw --creator --creator-token` accept no other accounts.
    ///
    /// With `--private` they are the only record. That path funds the deposit
    /// from a shielded holding through a fresh, unlinkable creator account that
    /// this command mints - so the wallet holds its key without knowing which
    /// account it is, and nothing on chain ties it back to you. `bc sale-info
    /// --sale <id>` can recover the creator from the sale id later; the
    /// creator's token holding it cannot. Keep both to yourself: they are
    /// unlinkable on chain, and publishing them beside the sale id is what
    /// undoes that.
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
    ///
    /// `--treasury` has to EXIST FIRST. The program requires an initialised
    /// fungible holding of `--collateral-def` and rejects the create otherwise:
    /// run `lpad init-holding --token-def <collateral-def>` and pass the id it
    /// prints. It is pinned into the sale permanently - nothing can change it
    /// afterwards, and it is where every protocol fee this sale ever charges is
    /// paid. (`bc create-token-sale` creates one for you instead.)
    CreateSale {
        #[arg(long)] program: Option<String>,
        #[arg(long = "collateral-def")] collateral_def: String,
        #[arg(
            long,
            help = "fee sink: an EXISTING fungible holding of --collateral-def (make one with \
                    `lpad init-holding --token-def <collateral-def>`); pinned permanently"
        )]
        treasury: String,
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
    /// [online] Private buy in ONE atomic transaction (no public intermediate state).
    ///
    /// Both of your holdings - the collateral you pay with and the tokens you
    /// receive - are private account slots of a single transaction, so the debit
    /// and the credit are private notes inside one proof. There is no ephemeral
    /// account, no deshield leg and no re-shield leg: either the whole buy lands
    /// or nothing happened, so a crash cannot leave a trade publicly visible the
    /// way `buy-private` can.
    ///
    /// Slower per operation than any public path: this proves the buy locally as
    /// a real recursive STARK, which takes minutes, not seconds.
    BuyDisposable { #[arg(long)] program: Option<String>, #[arg(long)] sale: String, #[arg(long = "user-collateral")] user_collateral: Option<String>, #[arg(long = "user-token")] user_token: Option<String>, #[arg(long = "in")] collateral_in: u128, #[arg(long = "min-out")] min_out: Option<u128>, #[arg(long = "slippage-bps", default_value_t = 100)] slippage_bps: u128 },
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
    /// [online] Settle the escrowed private-buy fee to the sale's treasury (anyone).
    ///
    /// A private (disposable) buy cannot pay the treasury as part of the buy - a
    /// privacy proof pins every public account it touches byte-for-byte, and a
    /// shared treasury changes constantly - so its protocol fee is escrowed in
    /// the sale's collateral vault instead. This hands that escrow over to the
    /// treasury pinned into the sale when it was created.
    ///
    /// Permissionless: no creator flag, because there is no authority to prove.
    /// The fee can only ever reach that pinned treasury, so anyone may send
    /// this; a stranger who does simply pays the gas to move someone else's fee
    /// to its rightful owner. Works while the sale is still open as well as
    /// after it closes, and errors when nothing is owed. `bc sale-info` reports
    /// the outstanding amount.
    ///
    /// No signer at all, and no exception to it: nothing signs for the treasury.
    /// There is nothing a signature could authorise - `create-sale` refuses a
    /// treasury that is not already an initialised holding of the collateral
    /// definition, and the guest rejects an unowned treasury before it looks at
    /// anything else. So "the treasury is unowned" is not a wallet problem and
    /// no retry fixes it: the sale's pinned treasury disagrees with chain state,
    /// and it wants reporting.
    SweepTreasury { #[arg(long)] program: Option<String>, #[arg(long)] sale: String },
}

#[derive(Subcommand, Debug)]
enum LbpCmd {
    /// Token weight (and collateral weight) at a point in time.
    Weight { #[arg(long = "w-start")] w_start: String, #[arg(long = "w-end")] w_end: String, #[arg(long = "t-start")] t_start: u64, #[arg(long = "t-end")] t_end: u64, #[arg(long)] at: u64 },
    /// Quote a buy at a point in time (weight-shifting price).
    Quote { #[arg(long = "reserve-token")] reserve_token: u128, #[arg(long = "reserve-collateral")] reserve_collateral: u128, #[arg(long = "w-start")] w_start: String, #[arg(long = "w-end")] w_end: String, #[arg(long = "t-start")] t_start: u64, #[arg(long = "t-end")] t_end: u64, #[arg(long)] at: u64, #[arg(long = "in")] collateral_in: u128 },
    /// Derive the pool + vault PDA addresses for a sale.
    Ids { #[arg(long)] program: Option<String>, #[arg(long = "token-def")] token_def: String, #[arg(long = "collateral-def")] collateral_def: String, #[arg(long)] creator: String, #[arg(long, default_value_t = 0)] nonce: u64 },
    /// [offline] Allowlist Merkle leaf for an account's collateral holding. For a
    /// single-member allowlist this hex is the `--allowlist-root` to pass to `lbp
    /// create-sale`; `lbp buy-gated` derives the same leaf from the buyer.
    AllowlistLeaf { #[arg(long)] account: String },
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
    /// [online] Private buy in ONE atomic transaction (no public intermediate state).
    ///
    /// Both of your holdings - the collateral you pay with and the tokens you
    /// receive - are private account slots of a single transaction, so the debit
    /// and the credit are private notes inside one proof. There is no ephemeral
    /// account, no deshield leg and no re-shield leg: either the whole buy lands
    /// or nothing happened, so a crash cannot leave a trade publicly visible the
    /// way `buy-private` can.
    ///
    /// A privacy transaction cannot carry the on-chain clock, so the buy is
    /// priced at the timestamp it was built at and its slippage floor is quoted
    /// at that same timestamp. The sale's own end time still binds it: the
    /// transaction's validity window is clamped to it.
    ///
    /// Slower per operation than any public path: this proves the buy locally as
    /// a real recursive STARK, which takes minutes, not seconds.
    BuyDisposable { #[arg(long)] program: Option<String>, #[arg(long)] pool: String, #[arg(long = "user-collateral")] user_collateral: Option<String>, #[arg(long = "user-token")] user_token: Option<String>, #[arg(long = "in")] collateral_in: u128, #[arg(long = "min-out")] min_out: Option<u128>, #[arg(long = "slippage-bps", default_value_t = 100)] slippage_bps: u128 },
    /// [online] Create an LBP sale (deposits project tokens; weights in [0,1] as `0.99` or `99/100`).
    ///
    /// `--treasury` has to EXIST FIRST, exactly as on the bonding curve: the
    /// program requires an initialised fungible holding of `--collateral-def`
    /// and rejects the create otherwise, so run
    /// `lpad init-holding --token-def <collateral-def>` and pass the id it
    /// prints. It is pinned into the pool permanently, and it is the only
    /// account `lbp sweep-treasury` can ever pay - including at `--fee-bps 0`,
    /// which is checked the same way.
    CreateSale {
        #[arg(long)] program: Option<String>,
        #[arg(long = "collateral-def")] collateral_def: String,
        #[arg(
            long,
            help = "fee sink: an EXISTING fungible holding of --collateral-def (make one with \
                    `lpad init-holding --token-def <collateral-def>`); pinned permanently"
        )]
        treasury: String,
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
    /// [online] Settle the escrowed at-close fee to the pool's treasury (anyone).
    ///
    /// `lbp withdraw` does not pay the treasury: it escrows the at-close fee in
    /// the collateral vault and hands the creator the rest. Paying both in one
    /// transaction meant a treasury that cannot receive - uninitialised, wrong
    /// token definition, wrong token program - reverted the creator's payout with
    /// it, locking the entire raise and every unsold token in a closed pool
    /// forever. This settles that escrow to the treasury pinned into the pool
    /// when it was created.
    ///
    /// Permissionless: no creator flag, because there is no authority to prove.
    /// The fee can only ever reach that pinned treasury, so anyone may send this
    /// and a stranger who does just pays the gas. Errors when nothing is owed -
    /// the fee accrues at withdrawal, so close and withdraw first. `lbp
    /// pool-info` reports the outstanding amount.
    ///
    /// Same absence of exceptions as `bc sweep-treasury`: nothing signs for the
    /// treasury, an uninitialised one cannot be bootstrapped by sweeping to it,
    /// and `create-sale` is where a treasury that could never receive is caught.
    SweepTreasury { #[arg(long)] program: Option<String>, #[arg(long)] pool: String },
}

/// Whether this invocation is one the wordmark belongs above: no arguments at
/// all, or a help request.
///
/// Pure and iterator-taking so the rule is testable without a terminal or a
/// child process. `--json` vetoes, and it vetoes even when combined with
/// `--help`, because the machine-readable flag is the more specific statement of
/// intent.
fn wants_banner(args: impl Iterator<Item = std::ffi::OsString>) -> bool {
    let args: Vec<String> = args.map(|a| a.to_string_lossy().into_owned()).collect();
    if args.iter().any(|a| a == "--json") {
        return false;
    }
    args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h" || a == "help")
}

fn main() {
    // Before anything else can panic. A raw Rust backtrace is the one output
    // that tells a user nothing they can act on, and `lpad` is the sort of tool
    // people run once, under time pressure, against real funds.
    ui::install_panic_hook();
    // The wordmark goes above the help screen, and only there: a bare `lpad`, or
    // any `--help`. Read from the raw argv BEFORE clap runs, because clap prints
    // help and exits from inside `parse()` - there is no later hook to attach to.
    //
    // `--json` suppresses it. The banner is on stderr and so cannot corrupt a
    // `--json` stdout anyway, but a caller who asked for machine output has said
    // what they want from this process, and a logo is not it.
    if wants_banner(std::env::args_os().skip(1)) {
        ui::banner();
    }
    let Cli { json, config, storage, network, cmd } = Cli::parse();
    let paths = || WalletPaths::resolve(config.clone(), storage.clone(), network.clone(), json);
    let result = match cmd {
        Cmd::Status => paths().and_then(|p| online::status(&p, json)),
        Cmd::Balance { account } => {
            parse_account(&account).and_then(|a| paths().and_then(|p| online::balance(&p, a, json)))
        }
        // `resolve_no_prompt`: this command IS the picker, so the first-run
        // prompt inside `resolve` would ask the same question twice.
        Cmd::Network { check } => {
            WalletPaths::resolve_no_prompt(config.clone(), storage.clone(), network.clone())
                .and_then(|p| online::network(&p, network.is_some(), check, json))
        }
        // Deliberately not `paths()`: that resolves a wallet that must already
        // exist, which is precisely what this command is for creating.
        Cmd::InitWallet => {
            WalletPaths::resolve_for_create(config.clone(), storage.clone(), network.clone())
                .and_then(|p| online::init_wallet(&p, json))
        }
        Cmd::ProgramId { which } => online::program_id(&which, json),
        Cmd::MyBalance => paths().and_then(|p| online::my_balance(&p, json)),
        Cmd::MySales { refresh, deep } => {
            paths().and_then(|p| online::my_sales(&p, refresh, deep, json))
        }
        Cmd::MyPools { refresh, deep } => {
            paths().and_then(|p| online::my_pools(&p, refresh, deep, json))
        }
        // `--to` is parsed BEFORE the wallet is opened, like `balance`: a typo'd
        // account should not cost a sequencer round trip to hear about.
        Cmd::Faucet { to } => to.as_deref().map(parse_account).transpose()
            .and_then(|to| paths().and_then(|p| online::faucet(&p, to, json))),
        Cmd::Wrap { amount, from } => paths().and_then(|p| online::wrap(&p, amount, from, json)),
        Cmd::Unwrap { amount } => paths().and_then(|p| online::unwrap(&p, amount, json)),
        Cmd::Shield { token_def, amount } => parse_account(&token_def).and_then(|d| paths().and_then(|p| online::shield(&p, d, amount, json))),
        Cmd::Deshield { token_def, amount } => parse_account(&token_def).and_then(|d| paths().and_then(|p| online::deshield(&p, d, amount, json))),
        Cmd::ShieldLez { amount, from } => paths().and_then(|p| online::shield_lez(&p, amount, from, json)),
        Cmd::DeshieldLez { amount } => paths().and_then(|p| online::deshield_lez(&p, amount, json)),
        Cmd::InitHolding { token_def } => {
            parse_account(&token_def).and_then(|d| paths().and_then(|p| online::init_holding(&p, d, json)))
        }
        Cmd::CreateAta { token_def, owner, ata_program } => {
            let ata = match ata_program { Some(s) => parse_program(&s), None => lpad_sdk::ata_program_id() };
            ata.and_then(|ata| parse_account(&token_def).and_then(|d| paths().and_then(|p| online::create_ata(&p, ata, owner, d, json))))
        }
        Cmd::Bc(c) => run_bc(c, json, &config, &storage, &network),
        Cmd::Lbp(c) => run_lbp(c, json, &config, &storage, &network),
    };
    if let Err(e) = result {
        // Sanitised like every other printed line: an error can quote text the
        // chain or the sequencer supplied - a guest's revert string, a rejected
        // account's on-chain name. `ui::error` sanitises, wraps and reddens it
        // on a TTY, so this no longer calls `safe` itself - doing both would
        // escape the escapes.
        ui::error(&e);
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

fn run_bc(cmd: BcCmd, json: bool, config: &Option<String>, storage: &Option<String>, network: &Option<String>) -> Result<(), String> {
    use bonding_curve_core as bc;
    let paths = || WalletPaths::resolve(config.clone(), storage.clone(), network.clone(), json);
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
        BcCmd::BuyDisposable { program, sale, user_collateral, user_token, collateral_in, min_out, slippage_bps } => {
            return online::bc_buy_disposable(&paths()?, prog_id(program)?, parse_account(&sale)?, user_collateral, user_token, collateral_in, min_out, slippage_bps, json);
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
        BcCmd::SweepTreasury { program, sale } => {
            return online::bc_sweep_treasury(&paths()?, prog_id(program)?, parse_account(&sale)?, json);
        }
    }
    Ok(())
}

fn run_lbp(cmd: LbpCmd, json: bool, config: &Option<String>, storage: &Option<String>, network: &Option<String>) -> Result<(), String> {
    use lbp_core as lbp;
    let paths = || WalletPaths::resolve(config.clone(), storage.clone(), network.clone(), json);
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
        LbpCmd::AllowlistLeaf { account } => {
            let acc = parse_account(&account)?;
            let hex: String = lbp::allowlist_leaf(&acc).iter().map(|b| format!("{b:02x}")).collect();
            out(json, serde_json::json!({ "leaf": &hex }), || {
                ui::header("LBP allowlist leaf");
                ui::kv("leaf (single-member root)", &hex);
            });
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
        LbpCmd::BuyDisposable { program, pool, user_collateral, user_token, collateral_in, min_out, slippage_bps } => {
            return online::lbp_buy_disposable(&paths()?, prog_id(program)?, parse_account(&pool)?, user_collateral, user_token, collateral_in, min_out, slippage_bps, json);
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
        LbpCmd::SweepTreasury { program, pool } => {
            return online::lbp_sweep_treasury(&paths()?, prog_id(program)?, parse_account(&pool)?, json);
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

    /// The wordmark belongs above a help screen and nowhere else. In particular
    /// it must never precede real work: `lpad bc buy` printing a logo before
    /// moving funds is noise at the worst possible moment.
    #[test]
    fn the_banner_is_for_help_screens_only() {
        let a = |v: &[&str]| {
            super::wants_banner(v.iter().map(|s| std::ffi::OsString::from(*s)))
        };
        assert!(a(&[]), "bare `lpad` is the help screen");
        assert!(a(&["--help"]));
        assert!(a(&["-h"]));
        assert!(a(&["help"]));
        assert!(a(&["bc", "--help"]), "a subcommand's help is still a help screen");
        assert!(!a(&["status"]));
        assert!(!a(&["bc", "buy", "--sale", "x", "--in", "1"]));
        assert!(!a(&["--network", "testnet", "faucet"]));
    }

    /// `--json` vetoes it, and vetoes it even alongside `--help`: a caller who
    /// asked for machine output has said what they want from this process. The
    /// banner is on stderr and so could not corrupt a `--json` stdout anyway -
    /// this is about not making them ask twice.
    #[test]
    fn json_suppresses_the_banner_even_with_help() {
        let a = |v: &[&str]| {
            super::wants_banner(v.iter().map(|s| std::ffi::OsString::from(*s)))
        };
        assert!(!a(&["--json"]));
        assert!(!a(&["--json", "--help"]));
        assert!(!a(&["--help", "--json"]));
    }
    use super::Cli;
    use clap::CommandFactory;

    /// Catches clap mis-configuration (duplicate args, bad value parsers, etc.).
    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
