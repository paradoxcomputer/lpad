//! The LPAD programs, packaged as [`Program`]s over their committed guest ELFs.
//!
//! Mirrors upstream's `programs` crate. Each `.bin` under `artifacts/lpad/` is a
//! byte-reproducible output of `cargo risczero build` (a pinned Docker
//! toolchain), so the image ids below are identical on every machine. That is
//! what makes them safe to use as trust anchors: unlike the old in-process
//! `risc0-build` guests, a fresh clone on a different host derives the *same*
//! program id, so pinned ids no longer break a clone+bootstrap.
//!
//! Only compiled with the `artifacts` feature - see the note in `Cargo.toml` on
//! why it is not a default.

#[cfg(feature = "artifacts")]
pub use inner::*;

#[cfg(feature = "artifacts")]
mod inner {
    use std::borrow::Cow;

    use guests::{BONDING_CURVE_ELF, BONDING_CURVE_ID, LBP_ELF, LBP_ID, WLEZ_ELF, WLEZ_ID};
    use lee::program::Program;

    mod guests {
        include!(concat!(env!("OUT_DIR"), "/lpad/mod.rs"));
    }

    /// The bonding-curve launchpad program (RFP-015).
    #[must_use]
    #[inline]
    pub const fn bonding_curve() -> Program {
        Program::new_unchecked(BONDING_CURVE_ID, Cow::Borrowed(BONDING_CURVE_ELF))
    }

    /// The liquidity-bootstrapping-pool program (RFP-016).
    #[must_use]
    #[inline]
    pub const fn lbp() -> Program {
        Program::new_unchecked(LBP_ID, Cow::Borrowed(LBP_ELF))
    }

    /// The wrapped-native-LEZ program, which turns the native account balance
    /// into a token so it can be used as curve/pool collateral.
    #[must_use]
    #[inline]
    pub const fn wlez() -> Program {
        Program::new_unchecked(WLEZ_ID, Cow::Borrowed(WLEZ_ELF))
    }
}
