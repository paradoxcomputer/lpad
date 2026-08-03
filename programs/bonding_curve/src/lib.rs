//! Host (logic) implementation of the LPAD bonding-curve program (RFP-015).
//!
//! Each public function returns `(Vec<AccountPostState>, Vec<ChainedCall>)` and
//! is wrapped by the guest entrypoint in `src/main.rs`. The convention mirrors
//! the ldex AMM: the top-level program lists every account it touches, echoes
//! them as post-states (claiming the PDAs it creates), and emits the token
//! movements as chained calls whose pre-states are adjusted with
//! [`util::shift_balance`] so each call validates against the running state diff.

pub use bonding_curve_core as core;

pub mod ata;
pub mod buy;
pub mod create_sale;
pub mod dispatch;
pub mod lifecycle;
pub mod sell;
pub mod util;

#[cfg(test)]
mod tests;
