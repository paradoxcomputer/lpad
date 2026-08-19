//! Guest entrypoint for the LPAD liquidity-bootstrapping-pool program (RFP-016).
//!
//! Replaces the SPEL `#[lez_program]` guest used under LEZ v0.2.0-rc4; SPEL was
//! deleted upstream, so the dispatch is written out by hand.
//!
//! Pre-states are emitted verbatim: v0.2.4 rejects a transaction that drops any
//! declared account (`DeclaredAccountMissingFromOutput`).
//!
//! The account order in each arm is part of the on-chain ABI: it must match the
//! order the SDK builds its account list in, and the doc comments on
//! `lbp_core::Instruction`.

use lbp_core::Instruction;
use lbp_program::buy::disposable_window;
use lbp_program::dispatch::{clock_block_id, clock_ms, echo_clock};
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

    // Every instruction carries a `deadline`, which becomes the window's exclusive
    // upper bound; the window is applied once, after the match. Arms also return a
    // LOWER bound as `Option<u64>`, which only `BuyDisposable` uses: it is priced
    // at a caller-supplied timestamp with no clock account to check it against, so
    // the window is the only thing tying that timestamp to real time. Every other
    // arm returns `None` and is bounded from above exactly as before.
    let (post_states, chained_calls, valid_from, valid_until) = match instruction {
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
                token_definition,
                collateral_definition,
                creator_token_holding,
                creator_collateral_holding,
                creator,
                treasury,
                creator_index,
                clock,
            ] = pre_states
                .try_into()
                .expect("CreateSale: 11 accounts");
            let clock_ts = clock_ms(&clock);
            let (post_states, chained_calls) = lbp_program::create_sale::create_sale(
                pool,
                token_vault,
                collateral_vault,
                token_definition,
                collateral_definition,
                creator_token_holding,
                creator_collateral_holding,
                creator,
                treasury,
                creator_index,
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
                pool,
                token_vault,
                collateral_vault,
                buyer_collateral_holding,
                buyer_token_holding,
                clock,
            ] = pre_states
                .try_into()
                .expect("Buy: 6 accounts");
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
            (
                echo_clock(post_states, clock),
                chained_calls,
                None,
                deadline,
            )
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
                .expect("BuyGated: 6 accounts");
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
                pool,
                token_vault,
                collateral_vault,
                owner,
                buyer_collateral_ata,
                buyer_token_ata,
                clock,
            ] = pre_states
                .try_into()
                .expect("BuyAta: 7 accounts");
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
            (
                echo_clock(post_states, clock),
                chained_calls,
                None,
                deadline,
            )
        }

        // `weight_obs` is the per-pool PDA the advanced weight is written into;
        // the pool account itself is echoed unchanged so a poke cannot invalidate
        // an in-flight BuyDisposable (see `lifecycle::poke`).
        Instruction::Poke { deadline } => {
            let [pool, weight_obs, clock] = pre_states
                .try_into()
                .expect("Poke: 3 accounts");
            let clock_ts = clock_ms(&clock);
            let (post_states, chained_calls) =
                lbp_program::lifecycle::poke(pool, weight_obs, self_program_id, clock_ts);
            (
                echo_clock(post_states, clock),
                chained_calls,
                None,
                deadline,
            )
        }

        // Pause/Resume take no clock: weight progression is lazy and continues
        // regardless, so there is no timestamp to read.
        Instruction::Pause { deadline } => {
            let [pool, creator] = pre_states
                .try_into()
                .expect("Pause: 2 accounts");
            let (post_states, chained_calls) =
                lbp_program::lifecycle::set_paused(pool, creator, true, self_program_id);
            (post_states, chained_calls, None, deadline)
        }

        Instruction::Resume { deadline } => {
            let [pool, creator] = pre_states
                .try_into()
                .expect("Resume: 2 accounts");
            let (post_states, chained_calls) =
                lbp_program::lifecycle::set_paused(pool, creator, false, self_program_id);
            (post_states, chained_calls, None, deadline)
        }

        Instruction::CloseSale { deadline } => {
            let [pool, creator, clock] = pre_states
                .try_into()
                .expect("CloseSale: 3 accounts");
            let clock_ts = clock_ms(&clock);
            let (post_states, chained_calls) =
                lbp_program::lifecycle::close_sale(pool, creator, self_program_id, clock_ts);
            (
                echo_clock(post_states, clock),
                chained_calls,
                None,
                deadline,
            )
        }

        // No clock account: withdraw is gated on the pool already being closed,
        // not on a timestamp, so there is nothing to echo. No treasury account
        // either - the escrowed at-close fee is settled by SweepTreasury, see
        // `lifecycle::withdraw`.
        Instruction::Withdraw { deadline } => {
            let [
                pool,
                token_vault,
                collateral_vault,
                creator_collateral_holding,
                creator_token_holding,
                creator,
            ] = pre_states
                .try_into()
                .expect("Withdraw: 6 accounts");
            let (post_states, chained_calls) = lbp_program::lifecycle::withdraw(
                pool,
                token_vault,
                collateral_vault,
                creator_collateral_holding,
                creator_token_holding,
                creator,
                self_program_id,
            );
            (post_states, chained_calls, None, deadline)
        }

        // No clock account: a privacy transaction cannot carry one (see the
        // `lbp_program::buy` module doc). The price is taken at `t_buy_ms` and
        // the validity window below is what binds it to real time.
        Instruction::BuyDisposable {
            collateral_in,
            min_tokens_out,
            t_buy_ms,
            deadline,
        } => {
            let [
                pool,
                token_vault,
                collateral_vault,
                buyer_collateral_holding,
                buyer_token_holding,
            ] = pre_states
                .try_into()
                .expect("BuyDisposable: 5 accounts");
            let (post_states, chained_calls, t_end_ms) = lbp_program::buy::buy_disposable(
                pool,
                token_vault,
                collateral_vault,
                buyer_collateral_holding,
                buyer_token_holding,
                collateral_in,
                min_tokens_out,
                t_buy_ms,
                self_program_id,
            );
            // `t_end_ms` comes from the PINNED pool state, not from the caller.
            // `disposable_window` clamps the caller's deadline down to the
            // earliest of it, the one-hour staleness cap, and the sale's end, and
            // reverts if nothing is left - never echo the deadline unclamped.
            let window = disposable_window(deadline, t_buy_ms, t_end_ms);
            (post_states, chained_calls, Some(window.start), window.end)
        }

        // No clock and no signer: the sweep is permissionless and time-invariant
        // (see `lifecycle::sweep_treasury`), so the only bound it carries is the
        // submitter's own deadline.
        Instruction::SweepTreasury { deadline } => {
            let [pool, collateral_vault, treasury] = pre_states
                .try_into()
                .expect("SweepTreasury: 3 accounts");
            let (post_states, chained_calls) = lbp_program::lifecycle::sweep_treasury(
                pool,
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

    // `..to` is infallible; `from..to` is not (LEZ has no `RangeInclusive`
    // conversion, and rejects an empty `from >= to` range), so the bounded form
    // goes through the fallible setter. `disposable_window` has already asserted
    // the range is non-empty, so this expect is unreachable - it is here because
    // the alternative is an unwrap with no story for the operator.
    let output = match valid_from {
        None => output.with_timestamp_validity_window(..valid_until),
        Some(from) => output
            .try_with_timestamp_validity_window(from..valid_until)
            .expect("empty validity window"),
    };
    output.write();
}
