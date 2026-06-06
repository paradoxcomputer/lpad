#![no_main]

use nssa_core::{
    account::{AccountId, AccountWithMetadata},
    program::{AccountPostState, ProgramId},
};
use spel_framework::context::ProgramContext;
use spel_framework::prelude::*;

risc0_zkvm::guest::entry!(main);

#[lez_program(instruction = "bonding_curve_core::Instruction")]
mod bonding_curve {
    #[allow(unused_imports)]
    use super::*;

    /// On-chain ms timestamp from the threaded Clock account; 0 if absent/invalid,
    /// in which case the end-timestamp guard never trips and analytics ts is 0.
    ///
    /// SECURITY: the submitter chooses every non-signer account id, so the clock
    /// slot must be pinned to the canonical `CLOCK_01` - otherwise an attacker
    /// could substitute a non-decoding account, force the timestamp to 0, and
    /// bypass the sale's `end_timestamp_ms` close guard (buy after close at the
    /// cheap pre-close price). Mirrors the ldex AMM clock authentication.
    fn clock_ms(clock: &AccountWithMetadata) -> i64 {
        assert_eq!(
            clock.account_id,
            bonding_curve_core::CLOCK_01,
            "clock account must be the canonical CLOCK_01"
        );
        <bonding_curve_core::ClockData as borsh::BorshDeserialize>::try_from_slice(
            clock.account.data.as_ref(),
        )
        .map(|c| c.timestamp)
        .unwrap_or(0)
    }

    /// Echo the read-only Clock account back unchanged so post_states.len()
    /// equals the account count on both the public and privacy paths (the LEZ
    /// privacy circuit asserts a 1:1 account↔state mapping).
    fn echo_clock(
        mut post_states: Vec<AccountPostState>,
        clock: AccountWithMetadata,
    ) -> Vec<AccountPostState> {
        post_states.push(AccountPostState::new(clock.account));
        post_states
    }

    /// Create a new bonding-curve sale.
    #[expect(clippy::too_many_arguments, reason = "fixed instruction shape")]
    #[instruction]
    pub fn create_sale(
        ctx: ProgramContext,
        sale: AccountWithMetadata,
        token_vault: AccountWithMetadata,
        collateral_vault: AccountWithMetadata,
        collateral_definition: AccountWithMetadata,
        creator_token_holding: AccountWithMetadata,
        creator: AccountWithMetadata,
        clock: AccountWithMetadata,
        collateral_definition_id: AccountId,
        treasury_id: AccountId,
        token_name: String,
        token_symbol: String,
        sale_quantity: u128,
        dex_seed_quantity: u128,
        virt_token: u128,
        virt_collateral: u128,
        fee_bps: u128,
        one_directional: bool,
        end_timestamp_ms: u64,
        min_duration_ms: u64,
        nonce: u64,
        ata_program_id: ProgramId,
        deadline: u64,
    ) -> SpelResult {
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
            ctx.self_program_id,
            clock_ts,
        );
        Ok(spel_framework::SpelOutput::execute(echo_clock(post_states, clock), chained_calls)
            .with_timestamp_validity_window(..deadline))
    }

    /// Public buy with keypair token holdings.
    #[expect(clippy::too_many_arguments, reason = "fixed instruction shape")]
    #[instruction]
    pub fn buy(
        ctx: ProgramContext,
        sale: AccountWithMetadata,
        token_vault: AccountWithMetadata,
        collateral_vault: AccountWithMetadata,
        treasury: AccountWithMetadata,
        buyer_collateral_holding: AccountWithMetadata,
        buyer_token_holding: AccountWithMetadata,
        clock: AccountWithMetadata,
        collateral_in: u128,
        min_tokens_out: u128,
        deadline: u64,
    ) -> SpelResult {
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
            ctx.self_program_id,
            clock_ts,
        );
        Ok(spel_framework::SpelOutput::execute(echo_clock(post_states, clock), chained_calls)
            .with_timestamp_validity_window(..deadline))
    }

    /// Public buy with the user side using ATAs (RFP-015 Func #7).
    #[expect(clippy::too_many_arguments, reason = "fixed instruction shape")]
    #[instruction]
    pub fn buy_ata(
        ctx: ProgramContext,
        sale: AccountWithMetadata,
        token_vault: AccountWithMetadata,
        collateral_vault: AccountWithMetadata,
        treasury: AccountWithMetadata,
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
            ctx.self_program_id,
            clock_ts,
        );
        Ok(spel_framework::SpelOutput::execute(echo_clock(post_states, clock), chained_calls)
            .with_timestamp_validity_window(..deadline))
    }

    /// Public sell with the user side using ATAs (RFP-015 Func #7).
    #[expect(clippy::too_many_arguments, reason = "fixed instruction shape")]
    #[instruction]
    pub fn sell_ata(
        ctx: ProgramContext,
        sale: AccountWithMetadata,
        token_vault: AccountWithMetadata,
        collateral_vault: AccountWithMetadata,
        treasury: AccountWithMetadata,
        owner: AccountWithMetadata,
        seller_token_ata: AccountWithMetadata,
        seller_collateral_ata: AccountWithMetadata,
        clock: AccountWithMetadata,
        tokens_in: u128,
        min_collateral_out: u128,
        ata_program_id: ProgramId,
        deadline: u64,
    ) -> SpelResult {
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
            ctx.self_program_id,
            clock_ts,
        );
        Ok(spel_framework::SpelOutput::execute(echo_clock(post_states, clock), chained_calls)
            .with_timestamp_validity_window(..deadline))
    }

    /// Public sell back into the curve.
    #[expect(clippy::too_many_arguments, reason = "fixed instruction shape")]
    #[instruction]
    pub fn sell(
        ctx: ProgramContext,
        sale: AccountWithMetadata,
        token_vault: AccountWithMetadata,
        collateral_vault: AccountWithMetadata,
        treasury: AccountWithMetadata,
        seller_token_holding: AccountWithMetadata,
        seller_collateral_holding: AccountWithMetadata,
        clock: AccountWithMetadata,
        tokens_in: u128,
        min_collateral_out: u128,
        deadline: u64,
    ) -> SpelResult {
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
            ctx.self_program_id,
            clock_ts,
        );
        Ok(spel_framework::SpelOutput::execute(echo_clock(post_states, clock), chained_calls)
            .with_timestamp_validity_window(..deadline))
    }

    /// Manual close by the creator.
    #[instruction]
    pub fn close_sale(
        ctx: ProgramContext,
        sale: AccountWithMetadata,
        creator: AccountWithMetadata,
        clock: AccountWithMetadata,
        deadline: u64,
    ) -> SpelResult {
        let clock_ts = clock_ms(&clock);
        let (post_states, chained_calls) =
            bonding_curve_program::lifecycle::close_sale(sale, creator, ctx.self_program_id, clock_ts);
        Ok(spel_framework::SpelOutput::execute(echo_clock(post_states, clock), chained_calls)
            .with_timestamp_validity_window(..deadline))
    }

    /// Creator withdrawal of raised collateral + remaining tokens.
    #[expect(clippy::too_many_arguments, reason = "fixed instruction shape")]
    #[instruction]
    pub fn withdraw(
        ctx: ProgramContext,
        sale: AccountWithMetadata,
        token_vault: AccountWithMetadata,
        collateral_vault: AccountWithMetadata,
        creator_collateral_holding: AccountWithMetadata,
        creator_token_holding: AccountWithMetadata,
        creator: AccountWithMetadata,
        deadline: u64,
    ) -> SpelResult {
        let (post_states, chained_calls) = bonding_curve_program::lifecycle::withdraw(
            sale,
            token_vault,
            collateral_vault,
            creator_collateral_holding,
            creator_token_holding,
            creator,
            ctx.self_program_id,
        );
        Ok(spel_framework::SpelOutput::execute(post_states, chained_calls)
            .with_timestamp_validity_window(..deadline))
    }
}
