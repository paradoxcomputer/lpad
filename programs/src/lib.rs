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

    /// The program ids that are DEPLOYED on the Logos and Paradox testnets.
    ///
    /// These are pinned deliberately. Under LEZ v0.2.0-rc4 a hardcoded image id
    /// was a bug - guests were built in-process, so the id varied with the local
    /// risc0 toolchain and a literal would break a fresh clone (which is why
    /// commits cf68c55/3825bab removed the ones that existed). That reasoning no
    /// longer applies: guests are now built in a pinned container and the ELFs
    /// are committed, so there is exactly one possible id per program.
    ///
    /// Their job here is to be a **drift guard**. The ids are what an lpad build
    /// will submit against, and they are baked into on-chain state - every sale
    /// and pool PDA is derived from them. So an accidental artifact change is a
    /// consensus break: it orphans every existing sale and points the CLI at a
    /// program nobody deployed. `pinned_ids_match_artifacts` below turns that into
    /// a failing test instead of a silent production incident.
    ///
    /// Updating these is a deliberate act: rebuild the guests, redeploy all three
    /// programs, and expect existing sales/pools to be unreachable.
    pub mod deployed {
        /// `bonding_curve.bin`, built against LEZ v0.2.4.
        pub const BONDING_CURVE: [u32; 8] = [
            2565909590, 1089191793, 3096775970, 3601414694, 2551522959, 1119295349, 390623576,
            571803788,
        ];
        /// `lbp.bin`, built against LEZ v0.2.4.
        pub const LBP: [u32; 8] = [
            1887109823, 1038855689, 1807287308, 1616265736, 549739187, 1066856073, 3271529204,
            2612835954,
        ];
        /// `wlez.bin`, built against LEZ v0.2.4.
        pub const WLEZ: [u32; 8] = [
            2256988620, 4234924746, 2824019580, 1455987179, 2844105107, 818985403, 4189727749,
            654478936,
        ];
    }

    #[cfg(test)]
    mod tests {
        use super::{bonding_curve, deployed, lbp, wlez};

        /// The committed artifacts must hash to the pinned ids.
        ///
        /// If this fails, `programs/artifacts/lpad/*.bin` changed. That is a
        /// consensus-level change - see the note on [`deployed`]. Either restore
        /// the artifacts, or (if the change is intended) redeploy the programs and
        /// update the constants.
        #[test]
        fn pinned_ids_match_artifacts() {
            assert_eq!(
                bonding_curve().id(),
                deployed::BONDING_CURVE,
                "bonding_curve.bin no longer hashes to the deployed program id"
            );
            assert_eq!(lbp().id(), deployed::LBP, "lbp.bin no longer hashes to the deployed program id");
            assert_eq!(
                wlez().id(),
                deployed::WLEZ,
                "wlez.bin no longer hashes to the deployed program id"
            );
        }
    }
}
