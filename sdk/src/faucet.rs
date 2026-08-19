//! # The pinata faucet - native LEZ for an empty wallet.
//!
//! LEZ's testnet faucet is the `pinata` program: an account holding native LEZ
//! that pays a fixed [`FAUCET_PRIZE`] to whoever submits a proof-of-work
//! solution against the challenge stored in its own public data. There is no
//! signature and no allowlist - the work IS the authorisation - so a wallet with
//! nothing in it can fund itself, which is the first step of lpad's
//! `faucet -> wrap -> create-token-sale -> buy` first-run story.
//!
//! lpad mines the solution itself. Upstream's `find_solution`/`compute_solution`
//! live in the wallet BINARY's cli module, not its library, so there is nothing
//! to call: the whole search is re-implemented here (and so, for want of a
//! `sha2` dependency reachable from this crate, is SHA-256 - see [`sha256`]).
//! That makes the byte layout of the hashed buffer lpad's problem, and it is the
//! one thing in this file that can go wrong silently: a wrong order or
//! endianness still mines "solutions" happily, at the right apparent cost, and
//! the chain rejects every one of them minutes later. The layout is therefore
//! pinned by digests computed outside this crate - see the `pow` tests.

use std::str::FromStr as _;

use lee::AccountId;
use lee_core::{account::Account, program::DEFAULT_PROGRAM_ID};
use wallet::program_facades::{
    native_token_transfer::NativeTokenTransfer, pinata::Pinata,
};
use wallet::AccountIdentity;

use crate::{hex_id, to_hash, LaunchpadClient, Result, TxHash};

// ---------------------------------------------------------------------------
// Initialising an account for native LEZ: what the chain will actually accept
// ---------------------------------------------------------------------------

/// What [`LaunchpadClient::initialize_native_account`] should do about an
/// account, given the account itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeInit {
    /// Owned by the authenticated-transfer program already: nothing to submit.
    AlreadyInitialised,
    /// Whole-default and signable by this wallet: the claim will be accepted.
    Claim,
}

/// Decide it offline, because every wrong answer here costs a transaction.
///
/// `authenticated_transfer::Initialize` asserts the claimed account equals
/// `Account::default()` - the WHOLE account, not just its owner. LEZ bumps the
/// nonce of every signer on inclusion, so an unowned account that has ever
/// signed anything (or that someone sent native balance to) can never be
/// initialised by anybody, ever. That is why `scripts/bootstrap.sh` initialises
/// the creator BEFORE it signs, and it is the same rule that makes an unowned
/// treasury unclaimable in [`crate::sweep_treasury_signers_for`].
///
/// Checking only `program_owner` therefore passed a doomed account straight
/// through to `register_account`: the user paid for a transaction the guest was
/// guaranteed to reject, and the error then prescribed
/// `wallet auth-transfer init`, which is the very instruction that just failed.
/// An account in that state does not need a different wallet, it needs a
/// different account - so say that, before spending anything.
fn classify_native_init(
    account: AccountId,
    acc: &Account,
    has_signing_key: bool,
) -> Result<NativeInit> {
    if acc.program_owner == programs::authenticated_transfer().id() {
        return Ok(NativeInit::AlreadyInitialised);
    }
    if acc.program_owner != DEFAULT_PROGRAM_ID {
        return Err(format!(
            "account {} is already owned by another program, and LEZ forbids changing an \
             account's program owner - so it can never be initialised for native LEZ. Token \
             holdings (what `lpad init-holding` makes) are the usual case: they hold a token, \
             not native LEZ. Use a plain account instead",
            hex_id(account)
        ));
    }
    if acc != &Account::default() {
        return Err(format!(
            "account {} is unowned but no longer pristine - a transaction it signed bumped its \
             nonce, or it already holds a native balance - and initialising claims an account \
             that must still be whole-default. No wallet and no retry can initialise it now, so \
             it can never receive native LEZ. Use a fresh account instead: make one with \
             `wallet account new public` and pass it as the recipient",
            hex_id(account)
        ));
    }
    if !has_signing_key {
        return Err(format!(
            "account {} is not initialised for native LEZ and this wallet holds no signing \
             key for it, so lpad cannot initialise it - the account must sign that itself. \
             Run `wallet auth-transfer init --account-id Public/<id>` from the wallet that \
             owns it, or pass an account this wallet owns",
            hex_id(account)
        ));
    }
    Ok(NativeInit::Claim)
}

// ---------------------------------------------------------------------------
// The faucet's fixed terms (from the pinata guest / system accounts)
// ---------------------------------------------------------------------------

/// Native LEZ paid by one successful claim.
///
/// A constant of the guest (`PRIZE` in `lez/programs/pinata/src/main.rs`), not
/// of the account, so it cannot be read off chain - which is also why
/// [`FaucetClaim::amount`] is measured rather than assumed.
pub const FAUCET_PRIZE: u128 = 150;

/// The base58 account id the pinata challenge lives at, as
/// `system_accounts::pinata_account_id()` spells it. Hardcoded because that
/// crate is not in this build's dependency graph; the `pow` tests pin the
/// decoded bytes.
const PINATA_ACCOUNT_B58: &str = "EfQhKQAkX2FJiwNii2WFQsGndjvF1Mzd7RuVe7QdPLw7";

/// The account holding the faucet's balance and its proof-of-work challenge.
///
/// The same id on every LEZ network: it is a constant of the genesis state, not
/// something derived per chain. A chain that never funded it simply has nothing
/// there, which is what [`LaunchpadClient::faucet_state`] reports as "this chain
/// does not run the pinata faucet".
#[must_use]
pub fn pinata_account_id() -> AccountId {
    AccountId::from_str(PINATA_ACCOUNT_B58).expect("the pinata account id is a valid base58 id")
}

/// Attempts one [`FaucetChallenge::mine`] will make before giving up, unless
/// `$LPAD_FAUCET_MAX_ATTEMPTS` says otherwise.
///
/// Sized against the difficulty the faucet actually runs at. Each difficulty
/// byte multiplies the expected search by 256: difficulty 3 - what LEZ's genesis
/// pinata account carries - expects 2^24 hashes, measured here at ~4.4M
/// hashes/s on one core, i.e. under ten seconds. 2^28 leaves a factor of 16 of
/// headroom, so a difficulty-3 challenge that exhausts this budget is a
/// ~1-in-10-million event rather than a slow machine.
///
/// Difficulty 4 expects 2^32 and will exhaust it, deliberately, after about a
/// minute of work: on an operator-raised difficulty the honest answer is an
/// error naming the knob, not a command that looks hung for an hour.
pub const DEFAULT_FAUCET_MAX_ATTEMPTS: u64 = 1 << 28;

/// How many attempts pass between progress reports. ~1M hashes is a fraction of
/// a second, so the phase label moves visibly without the callback costing
/// anything measurable.
const MINE_PROGRESS_EVERY: u64 = 1 << 20;

/// The attempt budget for this process: [`DEFAULT_FAUCET_MAX_ATTEMPTS`], or
/// `$LPAD_FAUCET_MAX_ATTEMPTS` when it parses to a non-zero number. A typo
/// degrades to the default rather than bricking the command.
#[must_use]
pub fn faucet_max_attempts() -> u64 {
    std::env::var("LPAD_FAUCET_MAX_ATTEMPTS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_FAUCET_MAX_ATTEMPTS)
}

// ---------------------------------------------------------------------------
// The proof of work (pure: no wallet, no network)
// ---------------------------------------------------------------------------

/// The faucet's proof-of-work challenge, as read from the pinata account's 33
/// public bytes.
///
/// A solution is any `u128` whose scoring digest starts with `difficulty` zero
/// BYTES (not bits) - see [`Self::digest`] for the buffer that is hashed. The
/// pinata guest re-hashes its own seed after every successful claim, so a
/// challenge is only good until the next claim lands: mine and submit, do not
/// cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaucetChallenge {
    /// Leading zero bytes required of the digest. Each one multiplies the
    /// expected search by 256. `<= 32`, or the guest itself would assert.
    pub difficulty: u8,
    /// The 32-byte seed the solution is hashed against.
    pub seed: [u8; 32],
}

impl FaucetChallenge {
    /// Read a challenge out of the pinata account's public data: byte 0 is the
    /// difficulty, bytes 1..33 the seed.
    ///
    /// Anything that is not exactly 33 bytes, or a difficulty above 32, is
    /// refused here rather than mined against: the guest asserts both, so such
    /// an account is not a pinata challenge at all and no solution exists.
    pub fn from_account_data(data: &[u8]) -> Result<Self> {
        let bytes: [u8; 33] = data.try_into().map_err(|_| {
            format!(
                "the pinata account holds {} bytes of data, not the 33 a faucet challenge is \
                 (1 difficulty byte + a 32-byte seed) - this chain is running something else at \
                 that address",
                data.len()
            )
        })?;
        let difficulty = bytes[0];
        if difficulty > 32 {
            return Err(format!(
                "the pinata account asks for {difficulty} leading zero bytes, but a SHA-256 \
                 digest is only 32 bytes long - no solution can exist, and the guest would \
                 reject the account too. Fund this wallet from an existing account instead"
            ));
        }
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&bytes[1..]);
        Ok(Self { difficulty, seed })
    }

    /// The digest a solution is scored by: `SHA-256(seed ‖ solution_le)`, over a
    /// 48-byte buffer of the 32-byte seed followed by the solution's 16
    /// little-endian bytes.
    ///
    /// THE ORDER AND THE ENDIANNESS ARE THE WHOLE CONTRACT. Get either wrong and
    /// mining still "succeeds" - the search space is just as full of digests
    /// with leading zeros - but every solution it finds scores differently
    /// inside the guest, so the claim is rejected on chain after the work is
    /// done. Nothing local catches it. The `pow` tests pin this against digests
    /// computed outside this crate for exactly that reason.
    #[must_use]
    pub fn digest(&self, solution: u128) -> [u8; 32] {
        let mut buf = [0u8; 32 + 16];
        buf[..32].copy_from_slice(&self.seed);
        buf[32..].copy_from_slice(&solution.to_le_bytes());
        sha256(&buf)
    }

    /// Whether `solution` wins this challenge - the same predicate the guest
    /// applies, so a `true` here is what the chain will agree with.
    #[must_use]
    pub fn satisfied_by(&self, solution: u128) -> bool {
        let d = usize::from(self.difficulty);
        self.digest(solution)[..d].iter().all(|&b| b == 0)
    }

    /// Search for a solution, scanning upward from 0, and return the smallest
    /// one - the same deterministic search upstream's wallet makes, so the same
    /// challenge yields the same solution here as there.
    ///
    /// `budget` bounds the number of attempts (see [`faucet_max_attempts`]);
    /// exhausting it is an error naming the knob, never a silent give-up and
    /// never an unbounded loop. `on_progress` is called every ~1M attempts with
    /// the running count, so a caller can show that a CPU-bound search of
    /// unknown length is alive.
    ///
    /// Single-threaded on purpose: at the difficulty the faucet runs, the search
    /// is seconds long, and a deterministic smallest-solution answer is worth
    /// more than the cores.
    pub fn mine(&self, budget: u64, on_progress: &mut dyn FnMut(u64)) -> Result<u128> {
        // The seed occupies the first 8 message words and never changes during
        // the search, so schedule it once and rewrite only the four words the
        // solution lives in. That is the difference between a search measured in
        // seconds and one measured in minutes.
        let mut block = [0u32; 16];
        for (w, chunk) in block.iter_mut().zip(self.seed.chunks_exact(4)) {
            *w = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        // 48 bytes of message: the 0x80 terminator, then the length in bits
        // (384) in the final word - the one-block padding SHA-256 specifies.
        block[12] = 0x8000_0000;
        block[15] = 384;
        let difficulty = usize::from(self.difficulty);

        for attempt in 0..budget {
            let solution = u128::from(attempt);
            let le = solution.to_le_bytes();
            for (i, chunk) in le.chunks_exact(4).enumerate() {
                block[8 + i] = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            }
            let h = compress(INITIAL_STATE, &block);
            if leading_zero_bytes(&h) >= difficulty {
                return Ok(solution);
            }
            if attempt > 0 && attempt % MINE_PROGRESS_EVERY == 0 {
                on_progress(attempt);
            }
        }
        Err(format!(
            "gave up mining the faucet after {budget} attempts at difficulty {} - each \
             difficulty byte costs 256x more work, so this chain's faucet is set harder than \
             lpad budgets for. Raise the budget with LPAD_FAUCET_MAX_ATTEMPTS=<attempts> and \
             re-run, or fund this wallet from an account that already holds LEZ",
            self.difficulty
        ))
    }
}

/// How many leading bytes of a digest are zero, read off the big-endian words
/// without materialising the 32 bytes - the miner's inner-loop test.
#[inline]
fn leading_zero_bytes(h: &[u32; 8]) -> usize {
    let mut n = 0;
    for word in h {
        let z = (word.leading_zeros() / 8) as usize;
        n += z;
        if z < 4 {
            break;
        }
    }
    n
}

// ---------------------------------------------------------------------------
// SHA-256
// ---------------------------------------------------------------------------

const INITIAL_STATE: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

#[rustfmt::skip]
const ROUND_CONSTANTS: [u32; 64] = [
    0x428a_2f98, 0x7137_4491, 0xb5c0_fbcf, 0xe9b5_dba5, 0x3956_c25b, 0x59f1_11f1, 0x923f_82a4, 0xab1c_5ed5,
    0xd807_aa98, 0x1283_5b01, 0x2431_85be, 0x550c_7dc3, 0x72be_5d74, 0x80de_b1fe, 0x9bdc_06a7, 0xc19b_f174,
    0xe49b_69c1, 0xefbe_4786, 0x0fc1_9dc6, 0x240c_a1cc, 0x2de9_2c6f, 0x4a74_84aa, 0x5cb0_a9dc, 0x76f9_88da,
    0x983e_5152, 0xa831_c66d, 0xb003_27c8, 0xbf59_7fc7, 0xc6e0_0bf3, 0xd5a7_9147, 0x06ca_6351, 0x1429_2967,
    0x27b7_0a85, 0x2e1b_2138, 0x4d2c_6dfc, 0x5338_0d13, 0x650a_7354, 0x766a_0abb, 0x81c2_c92e, 0x9272_2c85,
    0xa2bf_e8a1, 0xa81a_664b, 0xc24b_8b70, 0xc76c_51a3, 0xd192_e819, 0xd699_0624, 0xf40e_3585, 0x106a_a070,
    0x19a4_c116, 0x1e37_6c08, 0x2748_774c, 0x34b0_bcb5, 0x391c_0cb3, 0x4ed8_aa4a, 0x5b9c_ca4f, 0x682e_6ff3,
    0x748f_82ee, 0x78a5_636f, 0x84c8_7814, 0x8cc7_0208, 0x90be_fffa, 0xa450_6ceb, 0xbef9_a3f7, 0xc671_78f2,
];

/// One SHA-256 compression: fold a 16-word block into `state`.
///
/// Split out from [`sha256`] because the miner drives it directly - it rewrites
/// four words of one fixed block tens of millions of times and must not rebuild
/// the padding, or re-derive the seed's words, on every attempt.
#[inline]
fn compress(state: [u32; 8], block: &[u32; 16]) -> [u32; 8] {
    let mut w = [0u32; 64];
    w[..16].copy_from_slice(block);
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ (!e & g);
        let t1 = h
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(ROUND_CONSTANTS[i])
            .wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    [
        state[0].wrapping_add(a),
        state[1].wrapping_add(b),
        state[2].wrapping_add(c),
        state[3].wrapping_add(d),
        state[4].wrapping_add(e),
        state[5].wrapping_add(f),
        state[6].wrapping_add(g),
        state[7].wrapping_add(h),
    ]
}

/// SHA-256 of `msg`.
///
/// Written out here because no crate this build already depends on exposes one:
/// `common` hides its hasher, and `sha2` itself is a transitive dependency, not
/// a direct one. It is the ordinary FIPS 180-4 construction and it is pinned
/// against the published test vectors in the `pow` tests - which is the only
/// reason a hand-written hash belongs anywhere near consensus-visible bytes.
#[must_use]
pub fn sha256(msg: &[u8]) -> [u8; 32] {
    let mut state = INITIAL_STATE;
    let mut block = [0u8; 64];
    let mut chunks = msg.chunks_exact(64);
    for chunk in chunks.by_ref() {
        block.copy_from_slice(chunk);
        state = compress(state, &words(&block));
    }
    // Padding: the remainder, a 0x80 byte, zeros, and the bit length in the last
    // 8 bytes - spilling into a second block when the remainder leaves no room.
    let rest = chunks.remainder();
    block = [0u8; 64];
    block[..rest.len()].copy_from_slice(rest);
    block[rest.len()] = 0x80;
    let bits = (msg.len() as u64).wrapping_mul(8);
    if rest.len() >= 56 {
        state = compress(state, &words(&block));
        block = [0u8; 64];
    }
    block[56..].copy_from_slice(&bits.to_be_bytes());
    state = compress(state, &words(&block));

    let mut out = [0u8; 32];
    for (chunk, word) in out.chunks_exact_mut(4).zip(state) {
        chunk.copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// A 64-byte block as the 16 big-endian words SHA-256 schedules.
#[inline]
fn words(block: &[u8; 64]) -> [u32; 16] {
    let mut w = [0u32; 16];
    for (word, chunk) in w.iter_mut().zip(block.chunks_exact(4)) {
        *word = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
    }
    w
}

// ---------------------------------------------------------------------------
// The claim (wallet + network)
// ---------------------------------------------------------------------------

/// What the faucet looks like on the chain a client is pointed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaucetState {
    /// Always [`pinata_account_id`]; carried so a caller can name it.
    pub account: AccountId,
    /// Native LEZ the faucet still holds. Zero, or anything below
    /// [`FAUCET_PRIZE`], means drained - see [`Self::drained`].
    pub balance: u128,
    /// The challenge as it stands right now. It changes after every successful
    /// claim, including other people's.
    pub challenge: FaucetChallenge,
}

impl FaucetState {
    /// Whether the faucet can still pay one prize. A drained faucet is not a
    /// broken chain: it is a testnet that has been used, and the remedy (fund
    /// from another account) is different from the remedy for a chain that never
    /// ran pinata at all, which is why the two are separate errors.
    #[must_use]
    pub const fn drained(&self) -> bool {
        self.balance < FAUCET_PRIZE
    }

    /// How many more claims the faucet can pay at [`FAUCET_PRIZE`] each.
    #[must_use]
    pub const fn claims_left(&self) -> u128 {
        self.balance / FAUCET_PRIZE
    }
}

/// The outcome of [`LaunchpadClient::faucet_claim`]: the prize is in the
/// recipient's account, and that was VERIFIED by reading the balance on both
/// sides of the claim rather than inferred from the transaction being included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaucetClaim {
    /// The account that was paid.
    pub recipient: AccountId,
    /// Native LEZ the recipient gained: `balance_after - balance_before`, so it
    /// is measured, not assumed to be [`FAUCET_PRIZE`].
    pub amount: u128,
    /// The recipient's native balance after the claim.
    pub balance: u128,
    /// The block the claim landed in.
    pub block: u64,
    /// The claim transaction's hash.
    pub tx_hash: TxHash,
    /// The solution that was mined, and the difficulty it was mined at - worth
    /// reporting, since the difficulty is what the next claim's cost scales
    /// with.
    pub solution: u128,
    /// Leading zero bytes the challenge demanded.
    pub difficulty: u8,
    /// True when the recipient had to be initialised under the
    /// authenticated-transfer program first (one extra transaction). See
    /// [`LaunchpadClient::initialize_native_account`] for why it is mandatory.
    pub initialized_recipient: bool,
}

/// Re-reads of the recipient's balance after inclusion, and the pause between
/// them.
///
/// The claim's own transaction is already included when we look, so one read is
/// normally enough; these exist because a multi-sequencer client can answer a
/// state read from a node that has not applied the block yet, and reporting
/// "received 0" for a claim that in fact paid would send the user to look for a
/// bug that is not there.
const SETTLE_READS: u32 = 5;
const SETTLE_PAUSE_MS: u64 = 2_000;

impl LaunchpadClient {
    /// Read the faucet: its balance and its current proof-of-work challenge.
    ///
    /// Errors when this chain does not run pinata - a distinct condition from a
    /// drained faucet ([`FaucetState::drained`]), which is reported, not raised,
    /// because a caller may well want to say how empty it is.
    pub fn faucet_state(&self) -> Result<FaucetState> {
        let account = pinata_account_id();
        self.report("reading the faucet");
        let acc = self.read_account(account)?;
        // An untouched account and one owned by some other program are the same
        // verdict for a user - there is no faucet here - but they are worth
        // distinguishing in the message, because the second means the id has
        // been repurposed on this chain rather than never funded.
        if acc.program_owner == DEFAULT_PROGRAM_ID {
            return Err(format!(
                "this chain has no pinata faucet: account {} at {} is empty and unowned, so \
                 nothing there can pay a claim. Fund this wallet from an account that already \
                 holds LEZ (`lpad`'s scripts/bootstrap.sh takes LPAD_FUNDER for exactly this)",
                hex_id(account),
                self.wallet.helm_url()
            ));
        }
        if acc.program_owner != programs::pinata().id() {
            return Err(format!(
                "account {} on {} is not the pinata faucet on this chain - it is owned by another \
                 program, so this build's faucet cannot be claimed here. Fund this wallet from an \
                 account that already holds LEZ instead",
                hex_id(account),
                self.wallet.helm_url()
            ));
        }
        let challenge = FaucetChallenge::from_account_data(acc.data.as_ref())?;
        Ok(FaucetState { account, balance: acc.balance, challenge })
    }

    /// Whether `account` is initialised under the authenticated-transfer program
    /// - i.e. whether it can receive native LEZ at all.
    pub fn native_account_initialized(&self, account: AccountId) -> Result<bool> {
        Ok(self.read_account(account)?.program_owner == programs::authenticated_transfer().id())
    }

    /// The account `lpad faucet` claims into when the caller passes no `--to`.
    ///
    /// NOT [`LaunchpadClient::default_owner`]. That one picks the smallest
    /// signing-capable public id for ATA determinism, where any signer will do.
    /// A faucet claim pays NATIVE LEZ, which only an authenticated-transfer-owned
    /// account can hold - and in a wallet that has done any work, the smallest id
    /// is routinely a token holding, whose `program_owner` LEZ can never change.
    /// Reusing the ATA helper here therefore aimed the claim at an account no
    /// retry and no wallet could ever fix, and the live sweep failed on exactly
    /// that: the smallest id was the sale treasury.
    ///
    /// Prefers an account already initialised for native LEZ, then a whole-default
    /// one the claim path can still initialise itself. Deterministic within each
    /// tier (smallest id), so repeated claims top up the same account instead of
    /// scattering LEZ across the wallet.
    pub fn default_faucet_recipient(&self) -> Result<AccountId> {
        let mut ids: Vec<AccountId> = self
            .wallet_public_accounts()
            .into_iter()
            .filter(|id| self.wallet.get_account_public_signing_key(*id).is_some())
            .collect();
        ids.sort_unstable_by_key(AccountId::to_bytes);

        let mut claimable = None;
        // Why each rejected candidate was rejected. Kept because the failure a
        // user hits here is "my wallet has accounts, why is there nothing to
        // claim into" - and the per-account reason is the whole answer.
        let mut rejected: Vec<String> = Vec::new();
        for id in ids {
            let acc = self.read_account(id)?;
            match classify_native_init(id, &acc, true) {
                Ok(NativeInit::AlreadyInitialised) => return Ok(id),
                Ok(NativeInit::Claim) => claimable = claimable.or(Some(id)),
                Err(e) => rejected.push(e),
            }
        }
        claimable.ok_or_else(|| {
            if rejected.is_empty() {
                "this wallet has no signing-capable public account to claim into - make one with \
                 `wallet account new public`, or name one with `lpad faucet --to <account>`"
                    .to_owned()
            } else {
                format!(
                    "no account in this wallet can receive native LEZ, so there is nothing to \
                     claim into. Make a fresh one with `wallet account new public` and pass it as \
                     `lpad faucet --to <account>`. Why each existing account was skipped:\n  - {}",
                    rejected.join("\n  - ")
                )
            }
        })
    }

    /// Initialise `account` under the authenticated-transfer program, unless it
    /// already is. Returns the transaction hash, or `None` for the no-op.
    ///
    /// This is `wallet auth-transfer init`, and under v0.2.4 it is a
    /// PRECONDITION for receiving native LEZ, not a nicety. An account nobody has
    /// initialised is owned by the default program, and the state machine's rule
    /// 5 only lets the OWNING program decrease a balance - so native tokens paid
    /// into an uninitialised account can never be moved out of it again. The
    /// faucet claim is the first place a new wallet meets that trap, which is
    /// why the claim path refuses to proceed past it.
    ///
    /// Requires a signing key for `account`: initialising is the account's own
    /// signed transaction, so a wallet can only do this for its own accounts.
    pub fn initialize_native_account(&self, account: AccountId) -> Result<Option<TxHash>> {
        let acc = self.read_account(account)?;
        if classify_native_init(
            account,
            &acc,
            self.wallet.get_account_public_signing_key(account).is_some(),
        )? == NativeInit::AlreadyInitialised
        {
            return Ok(None);
        }
        self.report("initialising the account for native LEZ");
        let rt = &self.rt;
        let wallet = &self.wallet;
        // The wallet prints while it builds and polls; keep it off a possibly
        // --json stdout.
        let _silence = gag::Gag::stdout().ok();
        rt.block_on(async {
            let hash = NativeTokenTransfer(wallet)
                .register_account(AccountIdentity::Public(account))
                .await
                .map_err(|e| {
                    format!(
                        "initialise {} under authenticated-transfer: {e:?}",
                        hex_id(account)
                    )
                })?;
            wallet
                .poll_transaction(hash)
                .await
                .map_err(|e| format!("account initialisation not included: {e}"))?;
            Ok(Some(to_hash(hash)))
        })
    }

    /// Claim the faucet into `recipient`: read the challenge, mine a solution,
    /// submit it, and verify the money arrived.
    ///
    /// Initialises `recipient` first when it needs it (see
    /// [`Self::initialize_native_account`] for why an uninitialised recipient is
    /// a trap rather than an inconvenience), and refuses - naming the remedy -
    /// when it cannot.
    ///
    /// Mining is CPU-bound and takes seconds at the difficulty the faucet runs;
    /// progress goes to the client's progress sink, so nothing here prints.
    pub fn faucet_claim(&self, recipient: AccountId) -> Result<FaucetClaim> {
        let state = self.faucet_state()?;
        if state.drained() {
            return Err(format!(
                "the faucet is drained: it holds {} but one claim pays {FAUCET_PRIZE}. Nothing \
                 is wrong with this chain - the testnet faucet has simply been emptied. Wait for \
                 an operator to refill it, or fund this wallet from an account that already holds \
                 LEZ",
                state.balance
            ));
        }

        let initialized_recipient = self.initialize_native_account(recipient)?.is_some();
        let before = self.read_account(recipient)?.balance;

        let challenge = state.challenge;
        self.report(&format!(
            "mining the faucet challenge (difficulty {})",
            challenge.difficulty
        ));
        let solution = challenge.mine(faucet_max_attempts(), &mut |attempts| {
            self.report(&format!(
                "mining the faucet challenge (difficulty {}, {attempts} attempts)",
                challenge.difficulty
            ));
        })?;

        self.report("claiming the faucet");
        let rt = &self.rt;
        let wallet = &self.wallet;
        let (tx_hash, block) = {
            let _silence = gag::Gag::stdout().ok();
            rt.block_on(async {
                let hash = Pinata(wallet)
                    .claim(state.account, recipient, solution)
                    .await
                    .map_err(|e| {
                        format!(
                            "the faucet rejected the claim: {e:?} - the usual cause is that \
                             somebody else claimed first, which re-seeds the challenge and \
                             invalidates this solution. Re-run to mine against the new one"
                        )
                    })?;
                self.report("waiting for inclusion");
                let (_tx, block) = wallet
                    .poll_transaction(hash)
                    .await
                    .map_err(|e| format!("claim not included: {e}"))?;
                Ok::<_, String>((to_hash(hash), block))
            })?
        };

        // Inclusion is not payment. Read the balance back, because this is where
        // a wrong hashed-buffer layout would surface: a solution that scores
        // differently inside the guest costs exactly as much to mine and looks
        // exactly like this one right up to here.
        let mut balance = before;
        let mut last_read_error = None;
        for attempt in 0..SETTLE_READS {
            match self.read_account(recipient) {
                // A read that fails after the claim landed is a flaky sequencer,
                // not a failed claim - keep trying rather than reporting a
                // successful claim as an error.
                Err(e) => last_read_error = Some(e),
                Ok(acc) => {
                    balance = acc.balance;
                    if balance > before {
                        break;
                    }
                }
            }
            if attempt + 1 < SETTLE_READS {
                std::thread::sleep(std::time::Duration::from_millis(SETTLE_PAUSE_MS));
            }
        }
        if let Some(e) = last_read_error.filter(|_| balance <= before) {
            return Err(format!(
                "the claim was included in block {block}, but {}'s balance could not be read back \
                 to confirm it: {e}. The money is most likely there - check with `lpad my-balance` \
                 once the sequencer answers again",
                hex_id(recipient)
            ));
        }
        if balance <= before {
            return Err(format!(
                "the claim was included in block {block} but {}'s balance is still {before} - the \
                 faucet did not pay. Report this: it means the mined solution scored differently \
                 inside the pinata guest than it does here, which is a bug in lpad's miner, not \
                 something a re-run will fix",
                hex_id(recipient)
            ));
        }

        Ok(FaucetClaim {
            recipient,
            amount: balance - before,
            balance,
            block,
            tx_hash,
            solution,
            difficulty: challenge.difficulty,
            initialized_recipient,
        })
    }
}

#[cfg(test)]
mod native_init {
    //! Whether an account can be initialised for native LEZ - decided from the
    //! account, offline, because the alternative is finding out by paying for a
    //! transaction the guest was always going to reject.

    use lee_core::account::{Account, Nonce};

    use super::{classify_native_init, AccountId, NativeInit, DEFAULT_PROGRAM_ID};

    fn id() -> AccountId {
        AccountId::new([3; 32])
    }

    /// The happy path a new wallet is in: nothing has touched this account, and
    /// this wallet can sign for it.
    #[test]
    fn a_pristine_account_this_wallet_can_sign_for_is_claimable() {
        assert_eq!(classify_native_init(id(), &Account::default(), true), Ok(NativeInit::Claim));
    }

    /// Already done is not an error, and must stay a no-op rather than a
    /// second transaction.
    #[test]
    fn an_already_initialised_account_submits_nothing() {
        let acc = Account {
            program_owner: programs::authenticated_transfer().id(),
            ..Account::default()
        };
        assert_eq!(classify_native_init(id(), &acc, true), Ok(NativeInit::AlreadyInitialised));
    }

    /// The fix. LEZ bumps the nonce of every signer on inclusion, so any
    /// account that has ever signed is unowned AND non-default - and
    /// `Initialize` asserts the whole account equals `Account::default()`.
    /// Checking only `program_owner` sent this straight to `register_account`,
    /// where the user paid for a guaranteed revert; on any wallet that has done
    /// anything at all, `lpad faucet`'s default recipient is likely to be
    /// exactly this.
    #[test]
    fn an_account_that_has_already_signed_can_never_be_initialised() {
        let acc = Account { program_owner: DEFAULT_PROGRAM_ID, nonce: Nonce(1), ..Account::default() };
        let err = classify_native_init(id(), &acc, true)
            .expect_err("a nonce-bumped account is unclaimable; submitting only burns a fee");
        assert!(err.contains("no longer pristine"), "{err}");
        // The remedy has to be a different ACCOUNT. The old text sent the user
        // to `wallet auth-transfer init`, which is the instruction that fails.
        assert!(err.contains("wallet account new public"), "{err}");
        assert!(!err.contains("auth-transfer init"), "that remedy cannot work here: {err}");
    }

    /// Native balance sent to an uninitialised account is the same trap, and
    /// the same verdict: no program may claim it, so it can never be moved.
    #[test]
    fn an_unowned_account_holding_a_balance_can_never_be_initialised() {
        let acc = Account { program_owner: DEFAULT_PROGRAM_ID, balance: 1, ..Account::default() };
        assert!(classify_native_init(id(), &acc, true).is_err());
    }

    /// A token holding is owned by the token program, and LEZ forbids changing
    /// an owner - a different impossibility, with a different remedy.
    #[test]
    fn an_account_owned_by_another_program_names_that_reason_instead() {
        let acc = Account { program_owner: [4; 8], ..Account::default() };
        let err = classify_native_init(id(), &acc, true).expect_err("owner cannot change");
        assert!(err.contains("already owned by another program"), "{err}");
    }

    /// Pristine but not ours: only the account itself can sign its claim, so
    /// this one really does need the other wallet.
    #[test]
    fn a_pristine_account_without_a_key_asks_the_wallet_that_owns_it() {
        let err = classify_native_init(id(), &Account::default(), false)
            .expect_err("the account must sign its own claim");
        assert!(err.contains("auth-transfer init"), "{err}");
    }
}

#[cfg(test)]
mod pow {
    //! The proof of work, pinned against answers computed OUTSIDE this crate.
    //!
    //! Every digest and every solution below was produced by Python's
    //! `hashlib.sha256` (and, for the account id, an independent base58 decode),
    //! not by this file. That is the point: the miner and its hash agreeing with
    //! each other proves nothing, because a mis-ordered buffer mines perfectly
    //! good solutions to the wrong problem - solutions the guest rejects,
    //! minutes into a live run, with nothing local to blame.
    //!
    //! All offline: nothing here touches a chain or a wallet.

    use super::{sha256, FaucetChallenge, PINATA_ACCOUNT_B58};

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// A seed with every byte distinct in an obvious order, so a test failure
    /// that shifts or reverses the buffer is readable in the diff.
    const SEED: [u8; 32] = [
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
        25, 26, 27, 28, 29, 30, 31,
    ];

    const fn challenge(difficulty: u8) -> FaucetChallenge {
        FaucetChallenge { difficulty, seed: SEED }
    }

    /// FIPS 180-4 / NIST's own vectors for SHA-256, including one message long
    /// enough to need a second padding block. A hand-written hash is only
    /// defensible if it is checked against the published answers.
    #[test]
    fn the_hash_reproduces_the_published_sha256_vectors() {
        for (msg, want) in [
            ("", "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),
            ("abc", "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
            (
                "abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
                "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            ),
            (
                "abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmno\
                 ijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu",
                "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1",
            ),
        ] {
            assert_eq!(hex(&sha256(msg.as_bytes())), want, "sha256({msg:?})");
        }
        // 1M 'a' - the vector that catches a broken multi-block length field.
        let million = vec![b'a'; 1_000_000];
        assert_eq!(
            hex(&sha256(&million)),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    /// THE LAYOUT TEST. `SHA-256(seed ‖ solution.to_le_bytes())`, and nothing
    /// else, is what the pinata guest scores a solution by. The expected digest
    /// was computed elsewhere; the three near-misses are the mistakes that would
    /// otherwise cost a live run: putting the solution first, or writing it
    /// big-endian.
    #[test]
    fn a_solution_is_scored_by_the_seed_then_the_solution_little_endian() {
        assert_eq!(
            hex(&challenge(0).digest(42)),
            "8a9a5dde127acf9905248e956201894a7766f29c7424dcec6b475409ef53ec33"
        );

        let mut solution_first = Vec::new();
        solution_first.extend_from_slice(&42_u128.to_le_bytes());
        solution_first.extend_from_slice(&SEED);
        assert_eq!(
            hex(&sha256(&solution_first)),
            "5f9b69c1ad50d07e2fcc385ef17006a094154547238ae4a391aa06e48fb93ffb",
            "the reversed layout must still be a different digest, or this test proves nothing"
        );

        let mut big_endian = Vec::new();
        big_endian.extend_from_slice(&SEED);
        big_endian.extend_from_slice(&42_u128.to_be_bytes());
        assert_eq!(
            hex(&sha256(&big_endian)),
            "c38b6fc8d21efb5607c5e5a514ea8a163f38c7fbc22f6a2101a940afdec2dd33"
        );

        assert_ne!(challenge(0).digest(42), sha256(&solution_first));
        assert_ne!(challenge(0).digest(42), sha256(&big_endian));
    }

    /// The miner's fast path rebuilds the SHA-256 message schedule by hand, so
    /// it has to be shown to agree with the general hash it is an optimisation
    /// of - the digest it scores must be the digest [`FaucetChallenge::digest`]
    /// computes, for the mined value and for a value that was rejected on the
    /// way there.
    #[test]
    fn what_the_miner_scores_is_what_the_general_hash_computes() {
        let c = challenge(1);
        let solution = c.mine(10_000, &mut |_| {}).expect("difficulty 1 is cheap");
        assert_eq!(solution, 388, "the smallest difficulty-1 solution for this seed");
        assert_eq!(
            hex(&c.digest(solution)),
            "0083c132dd9aaaf11b4f60f0c85b74138216de1b75574cd0af76ef39ed6ee6b2"
        );
        assert!(c.satisfied_by(solution));
        // Everything below it must genuinely have failed, or "smallest" is a lie
        // and the miner is skipping candidates.
        for below in 0..solution {
            assert!(!c.satisfied_by(below), "solution {below} should not have been skipped");
        }
    }

    /// Difficulty 0 asks for nothing, so the first candidate wins. Worth pinning
    /// because it is the boundary where an off-by-one in the zero-byte count
    /// (`>=` vs `>`) would either loop forever or accept everything.
    #[test]
    fn difficulty_zero_is_satisfied_by_the_first_candidate() {
        let c = challenge(0);
        assert!(c.satisfied_by(0));
        assert!(c.satisfied_by(u128::MAX));
        assert_eq!(c.mine(1, &mut |_| {}).expect("no work required"), 0);
    }

    /// Each difficulty byte is another factor of 256 of expected work, and a
    /// harder challenge's answer is a subset of an easier one's - so the smallest
    /// difficulty-2 solution must be strictly beyond the difficulty-1 one, and
    /// the difficulty-1 answer must NOT satisfy difficulty 2. A miner that
    /// checked the wrong number of bytes would return the same value for both.
    #[test]
    fn a_harder_challenge_costs_strictly_more_work() {
        let easy = challenge(1).mine(10_000, &mut |_| {}).expect("difficulty 1");
        let hard = challenge(2).mine(100_000, &mut |_| {}).expect("difficulty 2");
        assert_eq!(hard, 35_787, "the smallest difficulty-2 solution for this seed");
        assert!(hard > easy, "difficulty 2 took {hard} attempts, difficulty 1 took {easy}");
        assert!(!challenge(2).satisfied_by(easy));
        assert!(challenge(1).satisfied_by(hard), "a difficulty-2 digest also has 1 leading zero");
    }

    /// A budget that cannot be met ends as an error that names the knob, not as
    /// a command that appears to hang - and the caller is told it is alive on the
    /// way there.
    #[test]
    fn mining_gives_up_with_a_remedy_instead_of_running_forever() {
        let mut reports = 0_u32;
        let err = challenge(8)
            .mine(2_100_000, &mut |_| reports += 1)
            .expect_err("8 zero bytes is not findable in 2.1M attempts");
        assert!(err.contains("LPAD_FAUCET_MAX_ATTEMPTS"), "{err}");
        assert!(err.contains("difficulty 8"), "{err}");
        assert!(reports >= 2, "a long search must report progress; got {reports} reports");
    }

    /// The 33 bytes, read the way the guest reads them - and refused, rather
    /// than mined against, when they are not a challenge at all.
    #[test]
    fn the_challenge_is_one_difficulty_byte_then_the_seed() {
        let mut data = [7u8; 33];
        data[0] = 3;
        let c = FaucetChallenge::from_account_data(&data).expect("33 bytes is a challenge");
        assert_eq!(c.difficulty, 3);
        assert_eq!(c.seed, [7u8; 32]);

        let short = FaucetChallenge::from_account_data(&data[..32]).expect_err("32 bytes is not");
        assert!(short.contains("32 bytes"), "{short}");
        assert!(FaucetChallenge::from_account_data(&[]).is_err());

        // A SHA-256 digest is 32 bytes, so 33 leading zero bytes is unsatisfiable
        // - and slicing a digest that far would panic rather than mine.
        data[0] = 33;
        let impossible = FaucetChallenge::from_account_data(&data).expect_err("33 > 32");
        assert!(impossible.contains("32 bytes long"), "{impossible}");
    }

    /// The faucet's address is a constant of LEZ's genesis state that this build
    /// hardcodes rather than importing, so pin the decoded bytes: a claim mined
    /// against the wrong account's data is work spent on a challenge nobody pays
    /// for.
    #[test]
    fn the_pinata_account_id_decodes_to_the_bytes_lez_seeds_it_at() {
        assert_eq!(
            hex(&super::pinata_account_id().to_bytes()),
            "cafecafecafecafecafecafecafecafecafecafecafecafecafecafecafecafe",
            "{PINATA_ACCOUNT_B58} is system_accounts::pinata_account_id()"
        );
    }
}
