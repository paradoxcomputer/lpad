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
    /// Updating these is a deliberate act: rebuild the guests, redeploy every
    /// program whose id moved, and expect existing sales/pools to be unreachable.
    ///
    /// Which ids moved is a fact to be *read off the rebuild*, never inferred
    /// from the source diff. This file has been wrong in both directions:
    ///
    /// * All three moved when `BuyDisposable` was added, `wlez` included - even
    ///   though no wlez source was touched. An image id covers the whole
    ///   dependency closure, not just the crate you edited, so
    ///   `programs/Cargo.lock` changing (commit 9a25825, which landed after the
    ///   artifacts were last built at 62deea9) is enough to move it on its own.
    ///   "I did not edit wlez, so its id is safe" is how a program quietly stops
    ///   being the one that is deployed.
    /// * The buy-path program-substitution fix changed only `bonding_curve` and
    ///   `lbp` and left the lockfile alone, so `wlez.bin` came back
    ///   byte-identical from the container and its id did not move. An id that
    ///   did not move is a deployment that does not need to move: redeploying
    ///   "to keep the set consistent" buys nothing, and ids out of lockstep are
    ///   not evidence that a rebuild was partial.
    ///
    /// The **treasury pin** came first: `create_sale` takes the treasury as an
    /// account on both programs and requires it to already be an initialised
    /// Fungible holding of the sale's collateral definition, which is what makes
    /// the unsettleable-escrow branch in `SweepTreasury` unreachable instead of
    /// merely unlikely. That build deployed `bonding_curve` (block 12076) and
    /// `wlez` (block 12241); `lbp` was rejected at submission for being 618,012
    /// bytes against a sequencer that refuses anything over
    /// `max_block_size - 200` = 614,200. Shortening only its assert MESSAGES,
    /// with the prose moved into `//` comments (which cost nothing in the ELF),
    /// brought `lbp.bin` back to 611,856 and made it deployable.
    ///
    /// The ids below are the **guest-build-profile** build, and they carry two
    /// changes at once. First, `create_sale` writes a per-creator index account
    /// recording the ids it creates (bc 9 -> 10 accounts, lbp 10 -> 11), so
    /// `my-sales`/`my-pools` cost one read per creator instead of thousands of
    /// speculative PDA probes. Second, `programs/Cargo.toml` gained its first
    /// `[profile.release]` section.
    ///
    /// The profile is why **all three moved, `wlez` included**, with no wlez
    /// source change - the same lesson as the `BuyDisposable` build, from a
    /// different direction. A profile change alters `-C metadata` for every
    /// crate, which permutes codegen-unit and string-merge ordering, so even
    /// the strip-only step relaid `.text` and `.rodata` while leaving the
    /// instruction histogram byte-for-byte identical (74,007 instructions, 52
    /// distinct opcodes, unchanged). And an image id covers the loaded image,
    /// which includes the ELF header - the first LOAD segment maps it at
    /// `0x00010000` - so `e_shoff` alone moves the id. There is no such thing
    /// as a profile change that spares a guest.
    ///
    /// The creator index had put both launchpads over the deploy wall: the
    /// sequencer refuses any transaction over `max_block_size - 200` = 614,200
    /// bytes, and a deploy tx is the ELF plus a 5-byte envelope. The profile is
    /// what got them back under it, with room this time:
    ///
    /// | guest | ELF before | ELF now | deploy tx | margin |
    /// |---|---|---|---|---|
    /// | `bonding_curve` | 619,484 (5,289 OVER) | 335,656 | 335,661 | 278,539 under |
    /// | `lbp` | 622,996 (8,801 OVER) | 347,356 | 347,361 | 266,839 under |
    /// | `wlez` | 449,480 | 236,692 | 236,697 | 377,503 under |
    ///
    /// Both launchpad rows are current rather than the profile's own effect,
    /// because two later builds (below) moved them again: the profile took
    /// `lbp` to 345,812, the assert-message restoration added 1,488, and the
    /// dusting correction another 56; `bonding_curve` sat at 335,304 until
    /// that same correction added 352. `wlez` is untouched by either and is
    /// still exactly the profile's number. The table in `Cargo.toml` measures
    /// the profile alone, so it still reads 345,812 and 335,304 - the two are
    /// not in conflict, they are measuring different things.
    ///
    /// Most of that was `.symtab` + `.strtab` - 235,990 bytes on
    /// `bonding_curve`, ~40% of the file - which is in no PT_LOAD segment and
    /// so was never mapped, executed or proven, but was paid for in full on
    /// every deploy. Trimming assert prose, which bought 6 KB last time, was
    /// never going to reach this and is now beside the point. The guests also
    /// prove ~8% faster than before; see the profile comment in `Cargo.toml`
    /// for the per-instruction cycle measurements and the gates it must keep
    /// passing.
    ///
    /// `lbp` then moved once more, on its own. Its assert MESSAGES had been
    /// cut to telegraphic fragments (`"treasury: wrong program"`) under the
    /// old wall, behind comments claiming ~2 KiB of headroom; that figure was
    /// wrong by two orders of magnitude even when written, and the reason for
    /// the cuts - a guest that would not fit in a deploy transaction - is
    /// gone. Restoring the bonding curve's full wording is ~1.3 KB of string
    /// data and cost 1,488 ELF bytes. In that rebuild `bonding_curve.bin` and
    /// `wlez.bin` came back **byte-identical** from the container - neither
    /// their sources nor `programs/Cargo.lock` changed - so only `lbp` moved.
    /// That is the second bullet above holding for a third time.
    ///
    /// The ids below are one build further on: the **dusting correction**.
    /// Several shipped assert messages told creators that a stranger could
    /// dust a treasury or a vault PDA and lock them out. That is not true on
    /// LEZ, and it is not true by construction rather than by luck:
    /// `validated_state_diff` rejects any transaction that leaves an account
    /// whose pre-state owner is DEFAULT modified without a claim, for balance
    /// as well as for data, and the privacy circuit enforces the same rule -
    /// so only the deriving program can ever write such an account. Replacing
    /// the false rationale with the true one is string data in the ELF, and
    /// `bonding_curve` also picked up the two asserts whose LBP twins already
    /// carried the longer wording. `bonding_curve` grew 352 bytes and `lbp`
    /// 56; both ids moved. **No assert CONDITION changed in this build** - the
    /// comments, docs and the one new unit test that came with it cost nothing
    /// in the guest, and the executed logic is bit-for-bit the same decision
    /// it was before.
    ///
    /// `wlez.bin` came back byte-identical for the **second consecutive
    /// rebuild**, so `WLEZ` below is still the profile build's id. Worth
    /// recording precisely, because it looks at first glance like the first
    /// bullet above should have fired: `programs/Cargo.lock` *did* change this
    /// round. It gained exactly one line - `authenticated_transfer_core` in
    /// the `integration_tests` dependency list, so the F14 settlement walk can
    /// submit the real `Initialize` instead of a hand-encoded copy - and no
    /// version resolution moved. A test-only crate is in no guest's dependency
    /// closure, so nothing reached `-C metadata` for any of the three binaries.
    /// That does not weaken the first bullet, which is about a lockfile change
    /// that moved actual dependency versions; it only says this particular one
    /// did not, and that too was read off the rebuild rather than argued from
    /// the diff.
    ///
    /// **These three ids are deployed on Logos testnet** - `bonding_curve` in
    /// block 12609, `lbp` in 12610, `wlez` in 12611 - and
    /// `scripts/verify-deployment.sh` confirms the bootstrap's sale and pool
    /// are on chain and owned by them, so nothing there needs redeploying.
    /// **Paradox has none of them**: that sequencer reports no record of any of
    /// the three and every lpad PDA on it reads as unclaimed, so it needs a
    /// deploy and a fresh bootstrap.
    ///
    /// This paragraph used to say the opposite, and said it long after it had
    /// stopped being true - it was written when the ids had just moved and was
    /// never revisited once they were deployed. Do not trust it, or any prose,
    /// for this: `lpad --network <net> network --check` asks the chain, and
    /// `scripts/verify-deployment.sh` asks the stronger question (is lpad's
    /// state still there, and does its owner match this build's id). A comment
    /// cannot know what an operator did yesterday. `CHANGELOG.md` records what
    /// a release needs redeployed; this comment only records how the artifacts
    /// got here.
    pub mod deployed {
        /// `bonding_curve.bin`, built against LEZ v0.2.4.
        pub const BONDING_CURVE: [u32; 8] = [
            261875743, 1441171480, 1539516234, 1521366316, 3631831148, 2226588926, 870522051,
            859683103,
        ];
        /// `lbp.bin`, built against LEZ v0.2.4.
        pub const LBP: [u32; 8] = [
            1384462869, 3374538765, 1173073368, 43224037, 2322440826, 2784768058, 2174255388,
            685374320,
        ];
        /// `wlez.bin`, built against LEZ v0.2.4.
        pub const WLEZ: [u32; 8] = [
            1299021779, 1777363431, 3488413557, 1503696036, 1803863082, 2970560090, 656622716,
            2217751921,
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
            assert_eq!(
                lbp().id(),
                deployed::LBP,
                "lbp.bin no longer hashes to the deployed program id"
            );
            assert_eq!(
                wlez().id(),
                deployed::WLEZ,
                "wlez.bin no longer hashes to the deployed program id"
            );
        }
    }
}
