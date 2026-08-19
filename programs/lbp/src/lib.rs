//! Host (logic) implementation of the LPAD LBP program (RFP-016).
//!
//! Same composition discipline as the bonding-curve program: each function
//! returns `(Vec<AccountPostState>, Vec<ChainedCall>)`, the guest wraps it, and
//! token movements are chained calls. A leg that runs after an earlier leg
//! already moved one of its accounts must be built against the running state
//! diff with [`util::shift_balance`] - no LBP path needs that today, since
//! `Withdraw` stopped paying the treasury ahead of the creator (the fee is
//! escrowed for `SweepTreasury` instead), but the helper is what makes such a
//! leg correct if one is ever added back.

pub use lbp_core as core;

pub mod ata;
pub mod buy;
pub mod create_sale;
pub mod dispatch;
pub mod lifecycle;
pub mod util;

#[cfg(test)]
mod tests;
