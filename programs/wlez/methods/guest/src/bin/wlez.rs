#![no_main]

use spel_framework::prelude::*;
use spel_framework::context::ProgramContext;
use nssa_core::account::AccountWithMetadata;

risc0_zkvm::guest::entry!(main);

#[lez_program(instruction = "wlez_core::Instruction")]
mod wlez {
    #[allow(unused_imports)]
    use super::*;

    /// One-shot setup: claim the WLEZ vault PDA + create the WLEZ token
    /// definition via a chained `token::NewFungibleDefinition`. Idempotent
    /// - re-running is a no-op (vault already claimed + definition already
    /// owned by the token program).
    #[instruction]
    pub fn initialize(
        ctx: ProgramContext,
        vault: AccountWithMetadata,
        definition: AccountWithMetadata,
        init_holding: AccountWithMetadata,
        reference_token_def: AccountWithMetadata,
        payer: AccountWithMetadata,
        token_program_id: nssa_core::program::ProgramId,
        native_program_id: nssa_core::program::ProgramId,
    ) -> SpelResult {
        let (post_states, chained_calls) = wlez_program::initialize::initialize(
            vault,
            definition,
            init_holding,
            reference_token_def,
            payer,
            ctx.self_program_id,
            token_program_id,
            native_program_id,
        );
        Ok(spel_framework::SpelOutput::execute(post_states, chained_calls))
    }

    /// Lock `amount` native LEZ into the vault and mint `amount` WLEZ
    /// to the user's holding. The native side is authorised by the user's
    /// tx signature on `user_native`; the mint authority is supplied by
    /// this program via `with_pda_seeds(wlez_definition_seed)` in the
    /// chained `token::Mint` call.
    #[instruction]
    pub fn wrap(
        ctx: ProgramContext,
        user_native: AccountWithMetadata,
        vault: AccountWithMetadata,
        definition: AccountWithMetadata,
        user_holding: AccountWithMetadata,
        amount: u128,
    ) -> SpelResult {
        let (post_states, chained_calls) = wlez_program::wrap::wrap(
            user_native,
            vault,
            definition,
            user_holding,
            amount,
            ctx.self_program_id,
        );
        Ok(spel_framework::SpelOutput::execute(post_states, chained_calls))
    }

    /// Burn `amount` WLEZ from the user's holding and release `amount`
    /// native LEZ from the vault back to the user's native account.
    /// The burn is authorised by the user's tx signature on
    /// `user_holding`; the native release is performed by WLEZ mutating
    /// the vault and user_native balances directly in its own
    /// post-states (WLEZ is both the executing program and the vault
    /// owner, so the vault balance decrease is permitted by
    /// validate_execution rule 5) - no chained native transfer is used.
    #[instruction]
    pub fn unwrap(
        ctx: ProgramContext,
        user_holding: AccountWithMetadata,
        definition: AccountWithMetadata,
        vault: AccountWithMetadata,
        user_native: AccountWithMetadata,
        amount: u128,
    ) -> SpelResult {
        let (post_states, chained_calls) = wlez_program::unwrap::unwrap(
            user_holding,
            definition,
            vault,
            user_native,
            amount,
            ctx.self_program_id,
        );
        Ok(spel_framework::SpelOutput::execute(post_states, chained_calls))
    }
}
