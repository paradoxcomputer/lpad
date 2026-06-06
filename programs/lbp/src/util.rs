//! Shared helpers for building chained-call pre-states (mirrors the bonding
//! curve program's `util`).

use nssa_core::account::{AccountWithMetadata, Data};
use token_core::TokenHolding;

/// Clone `awm` with its Fungible balance shifted by `delta` (add/subtract), so a
/// later chained call validates against the running state diff.
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

/// Mark an account authorized (to authorize a PDA vault as a chained-transfer sender).
#[must_use]
pub fn authorized(awm: &AccountWithMetadata) -> AccountWithMetadata {
    let mut out = awm.clone();
    out.is_authorized = true;
    out
}
