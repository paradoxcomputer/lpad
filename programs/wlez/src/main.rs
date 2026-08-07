//! Guest entrypoint for the WLEZ (wrapped native LEZ) program.
//!
//! Replaces the SPEL `#[lez_program]` guest used under LEZ v0.2.0-rc4; SPEL was
//! deleted upstream, so the dispatch is written out by hand.
//!
//! WLEZ instructions carry no deadline and touch no clock account, so no
//! validity window is applied.

use lee_core::program::{ProgramInput, ProgramOutput, read_lee_inputs};
use wlez_core::Instruction;

fn main() {
    let (
        ProgramInput {
            self_program_id,
            caller_program_id,
            pre_states,
            instruction,
        },
        instruction_words,
    ) = read_lee_inputs::<Instruction>();

    let pre_states_clone = pre_states.clone();

    let (post_states, chained_calls) = match instruction {
        // One-shot setup: claim the WLEZ vault PDA + create the WLEZ token
        // definition via a chained `token::NewFungibleDefinition`. Idempotent -
        // re-running is a no-op (vault already claimed, definition already owned
        // by the token program).
        Instruction::Initialize {
            token_program_id,
            native_program_id,
        } => {
            let [vault, definition, init_holding, reference_token_def, payer] = pre_states
                .try_into()
                .expect("Initialize requires exactly five accounts");
            wlez_program::initialize::initialize(
                vault,
                definition,
                init_holding,
                reference_token_def,
                payer,
                self_program_id,
                token_program_id,
                native_program_id,
            )
        }

        // Lock `amount` native LEZ into the vault and mint `amount` WLEZ to the
        // user's holding. The native side is authorised by the user's tx
        // signature on `user_native`; the mint authority is supplied by this
        // program via `with_pda_seeds(wlez_definition_seed)` on the chained
        // `token::Mint`.
        Instruction::Wrap { amount } => {
            let [user_native, vault, definition, user_holding] = pre_states
                .try_into()
                .expect("Wrap requires exactly four accounts");
            wlez_program::wrap::wrap(
                user_native,
                vault,
                definition,
                user_holding,
                amount,
                self_program_id,
            )
        }

        // Burn `amount` WLEZ from the user's holding and release `amount` native
        // LEZ from the vault. The burn is authorised by the user's tx signature
        // on `user_holding`; the native release is done by WLEZ mutating the
        // vault and user_native balances directly in its own post-states (WLEZ is
        // both the executing program and the vault owner, permitted by
        // validate_execution rule 5) - no chained native transfer.
        Instruction::Unwrap { amount } => {
            let [user_holding, definition, vault, user_native] = pre_states
                .try_into()
                .expect("Unwrap requires exactly four accounts");
            wlez_program::unwrap::unwrap(
                user_holding,
                definition,
                vault,
                user_native,
                amount,
                self_program_id,
            )
        }
    };

    ProgramOutput::new(
        self_program_id,
        caller_program_id,
        instruction_words,
        pre_states_clone,
        post_states,
    )
    .with_chained_calls(chained_calls)
    .write();
}
