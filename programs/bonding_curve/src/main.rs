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

use bonding_curve_core::{Instruction, SaleState, private_window_hi};
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

    // Every instruction carries a `deadline` - the exclusive upper bound of the
    // timestamp validity window, applied once after the match. Only the
    // disposable buy also needs a LOWER bound (it carries no clock, so the window
    // is the only thing that can pin it in time), so each arm yields
    // `(post_states, chained_calls, window_lo, window_hi)`. Every public arm
    // yields `None` for the lower bound, which is exactly the unbounded-below
    // `..deadline` they carried before.
    let (post_states, chained_calls, window_lo, window_hi) = match instruction {
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
                treasury,
                token_definition,
                collateral_definition,
                creator_token_holding,
                creator,
                creator_index,
                clock,
            ] = pre_states
                .try_into()
                .expect("CreateSale requires exactly ten accounts");
            let clock_ts = clock_ms(&clock);
            let (post_states, chained_calls) = bonding_curve_program::create_sale::create_sale(
                sale,
                token_vault,
                collateral_vault,
                treasury,
                token_definition,
                collateral_definition,
                creator_token_holding,
                creator,
                creator_index,
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
            (
                echo_clock(post_states, clock),
                chained_calls,
                None,
                deadline,
            )
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
            (
                echo_clock(post_states, clock),
                chained_calls,
                None,
                deadline,
            )
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
            (
                echo_clock(post_states, clock),
                chained_calls,
                None,
                deadline,
            )
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
            (
                echo_clock(post_states, clock),
                chained_calls,
                None,
                deadline,
            )
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
            (
                echo_clock(post_states, clock),
                chained_calls,
                None,
                deadline,
            )
        }

        Instruction::CloseSale { deadline } => {
            let [sale, creator, clock] = pre_states
                .try_into()
                .expect("CloseSale requires exactly three accounts");
            let clock_ts = clock_ms(&clock);
            let (post_states, chained_calls) = bonding_curve_program::lifecycle::close_sale(
                sale,
                creator,
                self_program_id,
                clock_ts,
            );
            (
                echo_clock(post_states, clock),
                chained_calls,
                None,
                deadline,
            )
        }

        // No clock account: withdraw is gated on the sale already being closed,
        // not on a timestamp, so there is nothing to echo. No treasury account
        // either - the escrowed disposable-buy fee is settled by SweepTreasury,
        // see `lifecycle::withdraw`.
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
            (post_states, chained_calls, None, deadline)
        }

        Instruction::BuyDisposable {
            collateral_in,
            min_tokens_out,
            not_before_ms,
            deadline,
        } => {
            let [
                sale,
                token_vault,
                collateral_vault,
                buyer_collateral_holding,
                buyer_token_holding,
            ] = pre_states
                .try_into()
                .expect("BuyDisposable requires exactly five accounts");
            // The sale's end timestamp comes from the PINNED pre-state, never
            // from the instruction: the submitter chooses the instruction bytes,
            // so reading it there would let a buyer widen their own window past
            // the end of the sale.
            let state =
                SaleState::try_from(&sale.account.data).expect("invalid sale state account");
            let end_or_max = if state.end_timestamp_ms == 0 {
                u64::MAX
            } else {
                state.end_timestamp_ms
            };
            let hi = private_window_hi(deadline, not_before_ms, end_or_max);
            assert!(
                not_before_ms < hi,
                "BuyDisposable: empty timestamp validity window - not_before_ms is at or past \
                 the tightest upper bound (the deadline, not_before_ms + MAX_PRIVATE_WINDOW_MS, \
                 or the sale's end_timestamp_ms). Re-submit with a later deadline, an earlier \
                 not_before_ms, or - if the sale has ended - not at all."
            );
            let (post_states, chained_calls) = bonding_curve_program::buy::buy_disposable(
                sale,
                token_vault,
                collateral_vault,
                buyer_collateral_holding,
                buyer_token_holding,
                collateral_in,
                min_tokens_out,
                self_program_id,
            );
            // No clock to echo: a privacy transaction pins every public account
            // byte-for-byte at proving time, and the clock's data is rewritten
            // every block. `hi` is what enforces `end_timestamp_ms` instead.
            (post_states, chained_calls, Some(not_before_ms), hi)
        }

        // No clock and no signer: the sweep is permissionless and time-invariant
        // (see `lifecycle::sweep_treasury`), so the only bound it carries is the
        // submitter's own deadline.
        Instruction::SweepTreasury { deadline } => {
            let [sale, collateral_vault, treasury] = pre_states
                .try_into()
                .expect("SweepTreasury requires exactly three accounts");
            let (post_states, chained_calls) = bonding_curve_program::lifecycle::sweep_treasury(
                sale,
                collateral_vault,
                treasury,
                self_program_id,
            );
            (post_states, chained_calls, None, deadline)
        }
    };

    let output = ProgramOutput::new(
        self_program_id,
        caller_program_id,
        instruction_words,
        pre_states_clone,
        post_states,
    )
    .with_chained_calls(chained_calls);

    // Half-open in both shapes: `..hi` when there is no lower bound (admitted iff
    // `ts < hi`), `lo..hi` when there is (`ts >= lo && ts < hi`). The two-bound
    // form is fallible only because an empty range is rejected, and the
    // BuyDisposable arm has already asserted `lo < hi` - so this expect is
    // unreachable short of a bug in that arm's bound arithmetic.
    let output = match window_lo {
        None => output.with_timestamp_validity_window(..window_hi),
        Some(lo) => output
            .try_with_timestamp_validity_window(lo..window_hi)
            .expect("empty timestamp validity window: window_lo is at or past window_hi"),
    };
    output.write();
}
