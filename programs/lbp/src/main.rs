//! Guest entrypoint for the LPAD liquidity-bootstrapping-pool program (RFP-016).
//!
//! Replaces the SPEL `#[lez_program]` guest used under LEZ v0.2.0-rc4; SPEL is
//! gone in v0.2.1, so the dispatch the macro generated is written out by hand.
//!
//! The account order in each arm is part of the on-chain ABI: it must match the
//! order the SDK builds its account list in, and the doc comments on
//! `lbp_core::Instruction`.

use lbp_core::Instruction;
use lbp_program::dispatch::{clock_block_id, clock_ms, echo_clock, filter_output};
use lee_core::program::{ProgramInput, ProgramOutput, read_lee_inputs};

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

    // Every instruction carries a `deadline`; the timestamp validity window is
    // applied once, after the match.
    let (post_states, chained_calls, deadline) = match instruction {
        Instruction::CreateSale {
            collateral_definition_id,
            treasury_id,
            token_name,
            token_symbol,
            token_deposit,
            collateral_seed,
            w_start_q64,
            w_end_q64,
            t_start_ms,
            t_end_ms,
            fee_bps,
            block_token_ceiling,
            allowlist_root,
            fixed_price,
            min_duration_ms,
            nonce,
            ata_program_id,
            deadline,
        } => {
            let [
                pool,
                token_vault,
                collateral_vault,
                creator_token_holding,
                creator_collateral_holding,
                creator,
                clock,
            ] = pre_states
                .try_into()
                .expect("CreateSale requires exactly seven accounts");
            let clock_ts = clock_ms(&clock);
            let (post_states, chained_calls) = lbp_program::create_sale::create_sale(
                pool,
                token_vault,
                collateral_vault,
                creator_token_holding,
                creator_collateral_holding,
                creator,
                collateral_definition_id,
                treasury_id,
                token_name,
                token_symbol,
                token_deposit,
                collateral_seed,
                w_start_q64,
                w_end_q64,
                t_start_ms,
                t_end_ms,
                fee_bps,
                block_token_ceiling,
                allowlist_root,
                fixed_price,
                min_duration_ms,
                nonce,
                ata_program_id,
                self_program_id,
                clock_ts,
            );
            (echo_clock(post_states, clock), chained_calls, deadline)
        }

        Instruction::Buy {
            collateral_in,
            min_tokens_out,
            deadline,
        } => {
            let [
                pool,
                token_vault,
                collateral_vault,
                buyer_collateral_holding,
                buyer_token_holding,
                clock,
            ] = pre_states
                .try_into()
                .expect("Buy requires exactly six accounts");
            let clock_ts = clock_ms(&clock);
            let block_id = clock_block_id(&clock);
            let (post_states, chained_calls) = lbp_program::buy::buy(
                pool,
                token_vault,
                collateral_vault,
                buyer_collateral_holding,
                buyer_token_holding,
                collateral_in,
                min_tokens_out,
                self_program_id,
                clock_ts,
                block_id,
            );
            (echo_clock(post_states, clock), chained_calls, deadline)
        }

        Instruction::BuyGated {
            collateral_in,
            min_tokens_out,
            leaf,
            proof,
            deadline,
        } => {
            let [
                pool,
                token_vault,
                collateral_vault,
                buyer_collateral_holding,
                buyer_token_holding,
                clock,
            ] = pre_states
                .try_into()
                .expect("BuyGated requires exactly six accounts");
            let clock_ts = clock_ms(&clock);
            let block_id = clock_block_id(&clock);
            let (post_states, chained_calls) = lbp_program::buy::buy_gated(
                pool,
                token_vault,
                collateral_vault,
                buyer_collateral_holding,
                buyer_token_holding,
                collateral_in,
                min_tokens_out,
                leaf,
                proof,
                self_program_id,
                clock_ts,
                block_id,
            );
            (echo_clock(post_states, clock), chained_calls, deadline)
        }

        Instruction::BuyAta {
            collateral_in,
            min_tokens_out,
            ata_program_id,
            deadline,
        } => {
            let [
                pool,
                token_vault,
                collateral_vault,
                owner,
                buyer_collateral_ata,
                buyer_token_ata,
                clock,
            ] = pre_states
                .try_into()
                .expect("BuyAta requires exactly seven accounts");
            let clock_ts = clock_ms(&clock);
            let block_id = clock_block_id(&clock);
            let (post_states, chained_calls) = lbp_program::ata::buy_ata(
                pool,
                token_vault,
                collateral_vault,
                owner,
                buyer_collateral_ata,
                buyer_token_ata,
                collateral_in,
                min_tokens_out,
                ata_program_id,
                self_program_id,
                clock_ts,
                block_id,
            );
            (echo_clock(post_states, clock), chained_calls, deadline)
        }

        Instruction::Poke { deadline } => {
            let [pool, clock] = pre_states
                .try_into()
                .expect("Poke requires exactly two accounts");
            let clock_ts = clock_ms(&clock);
            let (post_states, chained_calls) =
                lbp_program::lifecycle::poke(pool, self_program_id, clock_ts);
            (echo_clock(post_states, clock), chained_calls, deadline)
        }

        // Pause/Resume take no clock: weight progression is lazy and continues
        // regardless, so there is no timestamp to read.
        Instruction::Pause { deadline } => {
            let [pool, creator] = pre_states
                .try_into()
                .expect("Pause requires exactly two accounts");
            let (post_states, chained_calls) =
                lbp_program::lifecycle::set_paused(pool, creator, true, self_program_id);
            (post_states, chained_calls, deadline)
        }

        Instruction::Resume { deadline } => {
            let [pool, creator] = pre_states
                .try_into()
                .expect("Resume requires exactly two accounts");
            let (post_states, chained_calls) =
                lbp_program::lifecycle::set_paused(pool, creator, false, self_program_id);
            (post_states, chained_calls, deadline)
        }

        Instruction::CloseSale { deadline } => {
            let [pool, creator, clock] = pre_states
                .try_into()
                .expect("CloseSale requires exactly three accounts");
            let clock_ts = clock_ms(&clock);
            let (post_states, chained_calls) =
                lbp_program::lifecycle::close_sale(pool, creator, self_program_id, clock_ts);
            (echo_clock(post_states, clock), chained_calls, deadline)
        }

        // No clock: withdraw is gated on the pool already being closed.
        Instruction::Withdraw { deadline } => {
            let [
                pool,
                token_vault,
                collateral_vault,
                treasury,
                creator_collateral_holding,
                creator_token_holding,
                creator,
            ] = pre_states
                .try_into()
                .expect("Withdraw requires exactly seven accounts");
            let (post_states, chained_calls) = lbp_program::lifecycle::withdraw(
                pool,
                token_vault,
                collateral_vault,
                treasury,
                creator_collateral_holding,
                creator_token_holding,
                creator,
                self_program_id,
            );
            (post_states, chained_calls, deadline)
        }
    };

    let (filtered_pre, filtered_post) = filter_output(pre_states_clone, post_states);

    ProgramOutput::new(
        self_program_id,
        caller_program_id,
        instruction_words,
        filtered_pre,
        filtered_post,
    )
    .with_chained_calls(chained_calls)
    .with_timestamp_validity_window(..deadline)
    .write();
}
