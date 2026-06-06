#![no_main]

use nssa_core::{
    account::{AccountId, AccountWithMetadata},
    program::{AccountPostState, ProgramId},
};
use spel_framework::context::ProgramContext;
use spel_framework::prelude::*;

risc0_zkvm::guest::entry!(main);

#[lez_program(instruction = "lbp_core::Instruction")]
mod lbp {
    #[allow(unused_imports)]
    use super::*;

    /// SECURITY: pin the clock slot to the canonical `CLOCK_01`. The submitter
    /// chooses non-signer account ids, so without this an attacker could
    /// substitute a non-decoding account, force the time to 0, and buy after the
    /// sale's `t_end_ms` (priced at the opening weight). Mirrors the ldex AMM.
    fn assert_clock(clock: &AccountWithMetadata) {
        assert_eq!(
            clock.account_id,
            lbp_core::CLOCK_01,
            "clock account must be the canonical CLOCK_01"
        );
    }

    fn clock_ms(clock: &AccountWithMetadata) -> i64 {
        assert_clock(clock);
        <lbp_core::ClockData as borsh::BorshDeserialize>::try_from_slice(clock.account.data.as_ref())
            .map(|c| c.timestamp)
            .unwrap_or(0)
    }

    fn clock_block_id(clock: &AccountWithMetadata) -> u64 {
        assert_clock(clock);
        <lbp_core::ClockData as borsh::BorshDeserialize>::try_from_slice(clock.account.data.as_ref())
            .map(|c| c.block_id)
            .unwrap_or(0)
    }

    fn echo_clock(
        mut post_states: Vec<AccountPostState>,
        clock: AccountWithMetadata,
    ) -> Vec<AccountPostState> {
        post_states.push(AccountPostState::new(clock.account));
        post_states
    }

    /// Create a new LBP sale.
    #[expect(clippy::too_many_arguments, reason = "fixed instruction shape")]
    #[instruction]
    pub fn create_sale(
        ctx: ProgramContext,
        pool: AccountWithMetadata,
        token_vault: AccountWithMetadata,
        collateral_vault: AccountWithMetadata,
        creator_token_holding: AccountWithMetadata,
        creator_collateral_holding: AccountWithMetadata,
        creator: AccountWithMetadata,
        clock: AccountWithMetadata,
        collateral_definition_id: AccountId,
        treasury_id: AccountId,
        token_name: String,
        token_symbol: String,
        token_deposit: u128,
        collateral_seed: u128,
        w_start_q64: u128,
        w_end_q64: u128,
        t_start_ms: u64,
        t_end_ms: u64,
        fee_bps: u128,
        block_token_ceiling: u128,
        allowlist_root: [u8; 32],
        fixed_price: bool,
        min_duration_ms: u64,
        nonce: u64,
        ata_program_id: ProgramId,
        deadline: u64,
    ) -> SpelResult {
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
            ctx.self_program_id,
            clock_ts,
        );
        Ok(spel_framework::SpelOutput::execute(echo_clock(post_states, clock), chained_calls)
            .with_timestamp_validity_window(..deadline))
    }

    /// Public buy, priced at the on-chain clock.
    #[expect(clippy::too_many_arguments, reason = "fixed instruction shape")]
    #[instruction]
    pub fn buy(
        ctx: ProgramContext,
        pool: AccountWithMetadata,
        token_vault: AccountWithMetadata,
        collateral_vault: AccountWithMetadata,
        buyer_collateral_holding: AccountWithMetadata,
        buyer_token_holding: AccountWithMetadata,
        clock: AccountWithMetadata,
        collateral_in: u128,
        min_tokens_out: u128,
        deadline: u64,
    ) -> SpelResult {
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
            ctx.self_program_id,
            clock_ts,
            block_id,
        );
        Ok(spel_framework::SpelOutput::execute(echo_clock(post_states, clock), chained_calls)
            .with_timestamp_validity_window(..deadline))
    }

    /// Public buy with the buyer side using ATAs (RFP-016 Func #9).
    #[expect(clippy::too_many_arguments, reason = "fixed instruction shape")]
    #[instruction]
    pub fn buy_ata(
        ctx: ProgramContext,
        pool: AccountWithMetadata,
        token_vault: AccountWithMetadata,
        collateral_vault: AccountWithMetadata,
        owner: AccountWithMetadata,
        buyer_collateral_ata: AccountWithMetadata,
        buyer_token_ata: AccountWithMetadata,
        clock: AccountWithMetadata,
        collateral_in: u128,
        min_tokens_out: u128,
        ata_program_id: ProgramId,
        deadline: u64,
    ) -> SpelResult {
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
            ctx.self_program_id,
            clock_ts,
            block_id,
        );
        Ok(spel_framework::SpelOutput::execute(echo_clock(post_states, clock), chained_calls)
            .with_timestamp_validity_window(..deadline))
    }

    /// Allowlist-gated public buy.
    #[expect(clippy::too_many_arguments, reason = "fixed instruction shape")]
    #[instruction]
    pub fn buy_gated(
        ctx: ProgramContext,
        pool: AccountWithMetadata,
        token_vault: AccountWithMetadata,
        collateral_vault: AccountWithMetadata,
        buyer_collateral_holding: AccountWithMetadata,
        buyer_token_holding: AccountWithMetadata,
        clock: AccountWithMetadata,
        collateral_in: u128,
        min_tokens_out: u128,
        leaf: [u8; 32],
        proof: Vec<[u8; 32]>,
        deadline: u64,
    ) -> SpelResult {
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
            ctx.self_program_id,
            clock_ts,
            block_id,
        );
        Ok(spel_framework::SpelOutput::execute(echo_clock(post_states, clock), chained_calls)
            .with_timestamp_validity_window(..deadline))
    }

    /// Advance the stored weight (off-chain convenience).
    #[instruction]
    pub fn poke(
        ctx: ProgramContext,
        pool: AccountWithMetadata,
        clock: AccountWithMetadata,
        deadline: u64,
    ) -> SpelResult {
        let clock_ts = clock_ms(&clock);
        let (post_states, chained_calls) = lbp_program::lifecycle::poke(pool, ctx.self_program_id, clock_ts);
        Ok(spel_framework::SpelOutput::execute(echo_clock(post_states, clock), chained_calls)
            .with_timestamp_validity_window(..deadline))
    }

    /// Pause buying (emergency stop). Weight progression continues.
    #[instruction]
    pub fn pause(
        ctx: ProgramContext,
        pool: AccountWithMetadata,
        creator: AccountWithMetadata,
        deadline: u64,
    ) -> SpelResult {
        let (post_states, chained_calls) =
            lbp_program::lifecycle::set_paused(pool, creator, true, ctx.self_program_id);
        Ok(spel_framework::SpelOutput::execute(post_states, chained_calls)
            .with_timestamp_validity_window(..deadline))
    }

    /// Resume buying.
    #[instruction]
    pub fn resume(
        ctx: ProgramContext,
        pool: AccountWithMetadata,
        creator: AccountWithMetadata,
        deadline: u64,
    ) -> SpelResult {
        let (post_states, chained_calls) =
            lbp_program::lifecycle::set_paused(pool, creator, false, ctx.self_program_id);
        Ok(spel_framework::SpelOutput::execute(post_states, chained_calls)
            .with_timestamp_validity_window(..deadline))
    }

    /// Close after the end timestamp.
    #[instruction]
    pub fn close_sale(
        ctx: ProgramContext,
        pool: AccountWithMetadata,
        creator: AccountWithMetadata,
        clock: AccountWithMetadata,
        deadline: u64,
    ) -> SpelResult {
        let clock_ts = clock_ms(&clock);
        let (post_states, chained_calls) =
            lbp_program::lifecycle::close_sale(pool, creator, ctx.self_program_id, clock_ts);
        Ok(spel_framework::SpelOutput::execute(echo_clock(post_states, clock), chained_calls)
            .with_timestamp_validity_window(..deadline))
    }

    /// Creator withdrawal: collateral net of the at-close fee + unsold tokens.
    #[expect(clippy::too_many_arguments, reason = "fixed instruction shape")]
    #[instruction]
    pub fn withdraw(
        ctx: ProgramContext,
        pool: AccountWithMetadata,
        token_vault: AccountWithMetadata,
        collateral_vault: AccountWithMetadata,
        treasury: AccountWithMetadata,
        creator_collateral_holding: AccountWithMetadata,
        creator_token_holding: AccountWithMetadata,
        creator: AccountWithMetadata,
        deadline: u64,
    ) -> SpelResult {
        let (post_states, chained_calls) = lbp_program::lifecycle::withdraw(
            pool,
            token_vault,
            collateral_vault,
            treasury,
            creator_collateral_holding,
            creator_token_holding,
            creator,
            ctx.self_program_id,
        );
        Ok(spel_framework::SpelOutput::execute(post_states, chained_calls)
            .with_timestamp_validity_window(..deadline))
    }
}
