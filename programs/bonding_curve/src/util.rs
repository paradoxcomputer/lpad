//! Shared helpers for building chained-call pre-states.

use lee_core::account::{AccountWithMetadata, Data};
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

/// Re-asserts the `ata::Transfer` recipient contract on behalf of the caller.
///
/// lpad used to vendor a hardened fork of the ATA program which enforced this
/// itself. v0.2.1 ships the ATA program as a committed reproducible artifact
/// with a machine-independent image id, so lpad now uses the canonical upstream
/// build - and upstream deliberately does NOT enforce a recipient contract: its
/// doc reads "recipient token holding (any account; auto-created if default)",
/// so a default recipient is silently materialized by the downstream
/// `token::Transfer`'s `new_claimed_if_default(.., Claim::Authorized)` rather than
/// rejected.
///
/// The three checks below are therefore no longer made anywhere else, and are
/// the reason a mis-addressed leg fails loudly here instead of quietly minting a
/// fresh holding. Call it for the **recipient of every `ata::Transfer` leg**.
///
/// `sender_definition_id` is the definition the sender ATA holds, which the
/// caller has already established.
pub fn assert_ata_recipient(
    recipient: &AccountWithMetadata,
    sender_ata: &AccountWithMetadata,
    sender_definition_id: lee_core::account::AccountId,
    context: &str,
) {
    assert_ne!(
        recipient.account,
        lee_core::account::Account::default(),
        "{context}: recipient token holding must be initialized"
    );
    assert_eq!(
        recipient.account.program_owner,
        sender_ata.account.program_owner,
        "{context}: recipient must be owned by the same token program as the sender ATA"
    );
    let recipient_definition_id = match TokenHolding::try_from(&recipient.account.data) {
        Ok(TokenHolding::Fungible { definition_id, .. }) => definition_id,
        _ => panic!("{context}: recipient must be a Fungible token holding"),
    };
    assert_eq!(
        recipient_definition_id, sender_definition_id,
        "{context}: recipient and sender token definitions do not match"
    );
}
