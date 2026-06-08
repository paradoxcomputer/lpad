//! Wrapped LEZ (WLEZ) - shared types.
//!
//! The WLEZ program wraps the native LEZ gas token 1:1 into an SPL-style
//! token holding (the "WLEZ" token), so the AMM (which only speaks the
//! token program) can take native LEZ as one side of any pool.
//!
//! Architecture:
//!
//!   - **Vault**: a PDA-owned native account that custodies escrowed LEZ.
//!     ID = `for_public_pda(wlez_program_id, compute_wlez_vault_seed())`.
//!     program_owner = `wlez_program_id`. Its balance grows on `Wrap`
//!     and shrinks on `Unwrap`, matching `wlez_definition.total_supply`
//!     by construction.
//!
//!   - **WLEZ definition**: a PDA-owned token definition account. ID =
//!     `for_public_pda(wlez_program_id, compute_wlez_definition_seed())`.
//!     Mint authority = the WLEZ program (provides the PDA seed in the
//!     chained `token::Mint` call, mirroring how the AMM mints its LP
//!     tokens at `programs/amm/src/add.rs:181-190`).
//!
//!   - **User holdings**: a regular `token_program` holding for the WLEZ
//!     definition. Created via `token::InitializeAccount` (or an
//!     associated-token-account if the user prefers determinism).
//!
//! Invariant: `vault.balance == wlez_definition.total_supply` after
//! every Wrap/Unwrap. Both legs of each instruction move identical
//! amounts in opposite directions.

pub use nssa_core::program::PdaSeed;
use nssa_core::{account::AccountId, program::ProgramId};
use serde::{Deserialize, Serialize};

/// On-chain instruction enum. Matches the dispatcher in
/// `programs/wlez/methods/guest/src/bin/wlez.rs`.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum Instruction {
    /// One-shot setup at program-deployment time. Claims the vault
    /// PDA + creates the WLEZ token definition (chained
    /// `token::NewDefinition` with `wlez_program` as the authority via
    /// PDA seed). Idempotent: if the vault/definition are already
    /// initialised, returns a no-op echo of their post-states.
    ///
    /// Required accounts (3):
    ///   - Vault (PDA, default or already-claimed)
    ///   - WLEZ definition (PDA, default or already-initialised)
    ///   - A reference token-program definition account. Its
    ///     `program_owner` must equal `token_program_id` (the caller-
    ///     pinned canonical token program); this is the program the
    ///     WLEZ definition will be created under. Pinning the expected
    ///     id prevents a malicious reference definition from redirecting
    ///     the WLEZ definition's owning program at bootstrap.
    ///
    /// `native_program_id` is the canonical native/authenticated-transfer
    /// program; Initialize records it in the vault's `data` so that every
    /// later `Wrap` can pin the native-transfer leg to it (a submitter who
    /// supplied a no-op native program owning their `user_native` could
    /// otherwise skip the real escrow and mint unbacked WLEZ). Like
    /// `token_program_id`, it is trusted at bootstrap (the deployer pins
    /// it); the runtime defence is the per-Wrap check against this stored id.
    Initialize {
        token_program_id: ProgramId,
        native_program_id: ProgramId,
    },

    /// Lock `amount` native LEZ into the vault, mint `amount` WLEZ to
    /// the user's holding.
    ///
    /// Required accounts (4), in order:
    ///   0. `user_native` - the user's keypair public account (signs)
    ///   1. `vault` - the WLEZ vault PDA
    ///   2. `definition` - the WLEZ token definition PDA. Mint
    ///      authority is set to this program via `with_pda_seeds` on
    ///      the chained Mint.
    ///   3. `user_holding` - the user's WLEZ token holding. Must
    ///      already be initialised for the WLEZ def.
    Wrap { amount: u128 },

    /// Burn `amount` WLEZ from the user's holding, release `amount`
    /// native LEZ from the vault back to the user.
    ///
    /// Required accounts (4), in order:
    ///   0. `user_holding` - the user's WLEZ token holding. Signs the
    ///      burn via the user's tx authority.
    ///   1. `definition` - the WLEZ token definition.
    ///   2. `vault` - the WLEZ vault PDA. Authorised in the chained
    ///      native transfer via `with_pda_seeds`.
    ///   3. `user_native` - the destination native account.
    Unwrap { amount: u128 },
}

/// PDA seed for the WLEZ vault. Hardcoded byte tag rather than a
/// per-instance customisation, because there's exactly one vault per
/// WLEZ program deployment.
pub fn compute_wlez_vault_seed() -> PdaSeed {
    use risc0_zkvm::sha::{Impl, Sha256};
    PdaSeed::new(
        Impl::hash_bytes(b"wlez_vault\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0")
            .as_bytes()
            .try_into()
            .expect("Hash output must be exactly 32 bytes long"),
    )
}

/// PDA seed for the WLEZ token definition (one per program).
pub fn compute_wlez_definition_seed() -> PdaSeed {
    use risc0_zkvm::sha::{Impl, Sha256};
    PdaSeed::new(
        Impl::hash_bytes(b"wlez_definition\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0")
            .as_bytes()
            .try_into()
            .expect("Hash output must be exactly 32 bytes long"),
    )
}

/// Deterministic vault account id for a deployed WLEZ program.
pub fn get_wlez_vault_id(wlez_program_id: &ProgramId) -> AccountId {
    AccountId::for_public_pda(wlez_program_id, &compute_wlez_vault_seed())
}

/// Deterministic WLEZ token-definition account id for a deployed WLEZ
/// program. Bootstrap and FFI both call this to know where to send
/// `token::NewDefinition` outputs / mint requests.
pub fn get_wlez_definition_id(wlez_program_id: &ProgramId) -> AccountId {
    AccountId::for_public_pda(wlez_program_id, &compute_wlez_definition_seed())
}

/// PDA seed for the init-holding account that `Initialize` claims along
/// with the definition (the holding required by `token::NewFungibleDefinition`
/// for the initial supply, which is 0 for WLEZ - the holding stays
/// untouched after Initialize).
pub fn compute_wlez_init_holding_seed() -> PdaSeed {
    use risc0_zkvm::sha::{Impl, Sha256};
    PdaSeed::new(
        Impl::hash_bytes(b"wlez_init_holding\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0")
            .as_bytes()
            .try_into()
            .expect("Hash output must be exactly 32 bytes long"),
    )
}

/// Deterministic init-holding account id for a deployed WLEZ program.
pub fn get_wlez_init_holding_id(wlez_program_id: &ProgramId) -> AccountId {
    AccountId::for_public_pda(wlez_program_id, &compute_wlez_init_holding_seed())
}

// NOTE: the canonical native and token program ids are RISC0 guest image ids,
// which are host-computed (`Program::*::id()` -> `compute_image_id`) and not
// knowable inside the guest, and which differ across LEZ toolchains. So neither
// is pinned to a compile-time literal here (a literal would silently break a
// fresh clone built with a different toolchain, hard-failing `Initialize` at
// bootstrap). `Initialize` rejects only the zero/default no-op on-chain; the
// SDK pins both ids to the real canonical programs participant-side before any
// Wrap/Unwrap commits real LEZ (see `LaunchpadClient::wlez_programs_checked`).

/// Default WLEZ token symbol - used in the `token::NewDefinition` call
/// inside `Initialize`. Kept short so the symbol fits in a UI chip.
pub const WLEZ_NAME: &str = "WLEZ";

/// Encode a `ProgramId` (`[u32; 8]`) as 32 little-endian bytes for storage
/// in an account's `data`. `Initialize` records the trusted native program
/// id this way in the vault; `Wrap` reads it back with
/// [`decode_program_id`]. Dependency-free + byte-stable so the encoding is
/// identical across every build of the program.
#[must_use]
pub fn encode_program_id(id: &ProgramId) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, word) in id.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    out
}

/// Decode a `ProgramId` from exactly 32 little-endian bytes (the inverse of
/// [`encode_program_id`]). Returns `None` if the slice is not 32 bytes long
/// (e.g. a vault that predates the pinned-native-program field).
#[must_use]
pub fn decode_program_id(bytes: &[u8]) -> Option<ProgramId> {
    let arr: [u8; 32] = bytes.try_into().ok()?;
    let mut id = [0u32; 8];
    for (i, word) in id.iter_mut().enumerate() {
        *word = u32::from_le_bytes(
            arr[i * 4..i * 4 + 4]
                .try_into()
                .expect("4-byte chunk of a 32-byte array"),
        );
    }
    Some(id)
}
