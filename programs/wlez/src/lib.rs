//! The Wrapped LEZ (WLEZ) program implementation. See `wlez_core` for
//! the on-chain instruction enum and PDA derivations.

pub use wlez_core as core;

pub mod initialize;
pub mod unwrap;
pub mod wrap;

#[cfg(test)]
mod tests;
