//! Host (logic) implementation of the LPAD LBP program (RFP-016).
//!
//! Same composition discipline as the bonding-curve program: each function
//! returns `(Vec<AccountPostState>, Vec<ChainedCall>)`, the guest wraps it, and
//! token movements are chained calls with `shift_balance`-adjusted pre-states.

pub use lbp_core as core;

pub mod ata;
pub mod buy;
pub mod create_sale;
pub mod lifecycle;
pub mod util;

#[cfg(test)]
mod tests;
