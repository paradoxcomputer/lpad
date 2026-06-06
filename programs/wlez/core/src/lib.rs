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
    ///   - A reference token-program definition account, used only to
    ///     pull `token_program_id = program_owner`. Any existing
    ///     token-program-owned definition works (e.g. a TokenA def
    ///     from the bootstrap).
    Initialize,

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

/// Default WLEZ token symbol - used in the `token::NewDefinition` call
/// inside `Initialize`. Kept short so the symbol fits in a UI chip.
pub const WLEZ_NAME: &str = "WLEZ";
