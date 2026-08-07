//! Clock helpers for the guest entrypoint in `src/main.rs`.
//!
//! ## Why there is no post-state filter here
//!
//! Under LEZ v0.2.0-rc4 SPEL's `#[lez_program]` macro generated this, including a
//! filter that dropped `(pre, post)` pairs whose account was `DEFAULT`-owned with
//! a non-default pre-state (the shape of a bare signer whose nonce an earlier
//! transaction had bumped), because `validate_execution` rule 7 rejects those.
//!
//! That filter is deliberately GONE as of the v0.2.4 port. v0.2.4 added
//! `DeclaredAccountMissingFromOutput`: every account in `message.account_ids`
//! must appear in the final state diff, and the commit that added it
//! (`7bc4c460`) names macro-generated dispatchers as the motivating case. Since
//! the top-level pre-states *are* `message.account_ids`, anything the filter
//! could drop is now a hard rejection - so filtering can never rescue a
//! transaction, it only swaps rule 7's precise error for a vaguer one.
//!
//! The real constraint is therefore on the *caller*: never declare an account
//! that is `DEFAULT`-owned with non-default state. In practice every account lpad
//! declares is owned by the token program, the authenticated-transfer program, or
//! is a PDA this program claims - so none of them trip rule 7. Verified by
//! running the full integration suite with the filter disabled: all 21 pass.

use lee_core::{account::AccountWithMetadata, program::AccountPostState};


/// On-chain ms timestamp from the threaded Clock account; 0 if absent/invalid,
/// in which case the end-timestamp guard never trips and analytics ts is 0.
///
/// SECURITY: the submitter chooses every non-signer account id, so the clock
/// slot must be pinned to the canonical `CLOCK_01` - otherwise an attacker
/// could substitute a non-decoding account, force the timestamp to 0, and
/// bypass the sale's `end_timestamp_ms` close guard (buy after close at the
/// cheap pre-close price). Mirrors the ldex AMM clock authentication.
#[must_use]
pub fn clock_ms(clock: &AccountWithMetadata) -> i64 {
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

/// Echo the read-only Clock account back unchanged so `post_states.len()` equals
/// the account count on both the public and privacy paths (the LEZ privacy
/// circuit asserts a 1:1 account<->state mapping).
#[must_use]
pub fn echo_clock(
    mut post_states: Vec<AccountPostState>,
    clock: AccountWithMetadata,
) -> Vec<AccountPostState> {
    post_states.push(AccountPostState::new(clock.account));
    post_states
}
