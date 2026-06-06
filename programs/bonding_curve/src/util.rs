//! Shared helpers for building chained-call pre-states.

use nssa_core::account::{AccountWithMetadata, Data};
use token_core::TokenHolding;

/// Return a clone of `awm` whose Fungible balance is shifted by `delta`
/// (`add` = true adds, false subtracts). Used to construct the pre-state of a
/// chained call that runs *after* a prior call already moved this account's
/// balance - the LEZ framework validates each chained call's pre-states against
/// the running state diff, not the proof-start snapshot.
#[must_use]
pub fn shift_balance(awm: &AccountWithMetadata, delta: u128, add: bool) -> AccountWithMetadata {
    let mut out = awm.clone();
    let mut holding = TokenHolding::try_from(&out.account.data)
        .expect("shift_balance: account must be an initialized Fungible token holding");
    match &mut holding {
        TokenHolding::Fungible { balance, .. } => {
            *balance = if add {
                balance.checked_add(delta).expect("balance overflow on shift")
            } else {
                balance.checked_sub(delta).expect("insufficient balance on shift")
            };
        }
        _ => panic!("shift_balance: account must be a Fungible token holding"),
    }
    out.account.data = Data::from(&holding);
    out
}

/// Mark an account authorized (used to authorize a PDA vault as the sender of a
/// chained `token::Transfer`, paired with `.with_pda_seeds`).
#[must_use]
pub fn authorized(awm: &AccountWithMetadata) -> AccountWithMetadata {
    let mut out = awm.clone();
    out.is_authorized = true;
    out
}
