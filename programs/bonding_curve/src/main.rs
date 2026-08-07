//! Guest entrypoint for the LPAD bonding-curve program (RFP-015).
//!
//! Replaces the SPEL `#[lez_program]` guest that LEZ v0.2.0-rc4 used. SPEL was
//! deleted upstream, so the dispatch the macro generated - match the instruction,
//! destructure `pre_states` into the handler's account arguments, write the
//! `ProgramOutput` - is written out by hand here.
//!
//! Pre-states are emitted verbatim: v0.2.4 rejects a transaction that drops any
//! declared account (`DeclaredAccountMissingFromOutput`). See `dispatch.rs`.
//!
//! The account order in each arm is part of the on-chain ABI: it must match the
//! order the SDK builds its account list in, and the doc comments on
//! `bonding_curve_core::Instruction`.

use bonding_curve_core::Instruction;
use bonding_curve_program::dispatch::{clock_ms, echo_clock};
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
            sale_quantity,
            dex_seed_quantity,
            virt_token,
            virt_collateral,
            fee_bps,
            one_directional,
            end_timestamp_ms,
            min_duration_ms,
            nonce,
            ata_program_id,
            deadline,
        } => {
            let [
                sale,
                token_vault,
                collateral_vault,
                collateral_definition,
                creator_token_holding,
                creator,
                clock,
            ] = pre_states
                .try_into()
                .expect("CreateSale requires exactly seven accounts");
            let clock_ts = clock_ms(&clock);
            let (post_states, chained_calls) = bonding_curve_program::create_sale::create_sale(
                sale,
                token_vault,
                collateral_vault,
                collateral_definition,
                creator_token_holding,
                creator,
                collateral_definition_id,
                treasury_id,
                token_name,
                token_symbol,
                sale_quantity,
                dex_seed_quantity,
                virt_token,
                virt_collateral,
                fee_bps,
                one_directional,
                end_timestamp_ms,
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
                sale,
                token_vault,
                collateral_vault,
                treasury,
                buyer_collateral_holding,
                buyer_token_holding,
                clock,
            ] = pre_states
                .try_into()
                .expect("Buy requires exactly seven accounts");
            let clock_ts = clock_ms(&clock);
            let (post_states, chained_calls) = bonding_curve_program::buy::buy(
                sale,
                token_vault,
                collateral_vault,
                treasury,
                buyer_collateral_holding,
                buyer_token_holding,
                collateral_in,
                min_tokens_out,
                self_program_id,
                clock_ts,
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
                sale,
                token_vault,
                collateral_vault,
                treasury,
                owner,
                buyer_collateral_ata,
                buyer_token_ata,
                clock,
            ] = pre_states
                .try_into()
                .expect("BuyAta requires exactly eight accounts");
            let clock_ts = clock_ms(&clock);
            let (post_states, chained_calls) = bonding_curve_program::ata::buy_ata(
                sale,
                token_vault,
                collateral_vault,
                treasury,
                owner,
                buyer_collateral_ata,
                buyer_token_ata,
                collateral_in,
                min_tokens_out,
                ata_program_id,
                self_program_id,
                clock_ts,
            );
            (echo_clock(post_states, clock), chained_calls, deadline)
        }

        Instruction::Sell {
            tokens_in,
            min_collateral_out,
            deadline,
        } => {
            let [
                sale,
                token_vault,
                collateral_vault,
                treasury,
                seller_token_holding,
                seller_collateral_holding,
                clock,
            ] = pre_states
                .try_into()
                .expect("Sell requires exactly seven accounts");
            let clock_ts = clock_ms(&clock);
            let (post_states, chained_calls) = bonding_curve_program::sell::sell(
                sale,
                token_vault,
                collateral_vault,
                treasury,
                seller_token_holding,
                seller_collateral_holding,
                tokens_in,
                min_collateral_out,
                self_program_id,
                clock_ts,
            );
            (echo_clock(post_states, clock), chained_calls, deadline)
        }

        Instruction::SellAta {
            tokens_in,
            min_collateral_out,
            ata_program_id,
            deadline,
        } => {
            let [
                sale,
                token_vault,
                collateral_vault,
                treasury,
                owner,
                seller_token_ata,
                seller_collateral_ata,
                clock,
            ] = pre_states
                .try_into()
                .expect("SellAta requires exactly eight accounts");
            let clock_ts = clock_ms(&clock);
            let (post_states, chained_calls) = bonding_curve_program::ata::sell_ata(
                sale,
                token_vault,
                collateral_vault,
                treasury,
                owner,
                seller_token_ata,
                seller_collateral_ata,
                tokens_in,
                min_collateral_out,
                ata_program_id,
                self_program_id,
                clock_ts,
            );
            (echo_clock(post_states, clock), chained_calls, deadline)
        }

        Instruction::CloseSale { deadline } => {
            let [sale, creator, clock] = pre_states
                .try_into()
                .expect("CloseSale requires exactly three accounts");
            let clock_ts = clock_ms(&clock);
            let (post_states, chained_calls) =
                bonding_curve_program::lifecycle::close_sale(sale, creator, self_program_id, clock_ts);
            (echo_clock(post_states, clock), chained_calls, deadline)
        }

        // No clock account: withdraw is gated on the sale already being closed,
        // not on a timestamp, so there is nothing to echo.
        Instruction::Withdraw { deadline } => {
            let [
                sale,
                token_vault,
                collateral_vault,
                creator_collateral_holding,
                creator_token_holding,
                creator,
            ] = pre_states
                .try_into()
                .expect("Withdraw requires exactly six accounts");
            let (post_states, chained_calls) = bonding_curve_program::lifecycle::withdraw(
                sale,
                token_vault,
                collateral_vault,
                creator_collateral_holding,
                creator_token_holding,
                creator,
                self_program_id,
            );
            (post_states, chained_calls, deadline)
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
    .with_timestamp_validity_window(..deadline)
    .write();
}
