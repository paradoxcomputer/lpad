//! Q64.64 fixed-point math for the LBP weighted-pool formula (RFP-016).
//!
//! Implements the Balancer "power" via `pow(b, e) = exp2(e * log2(b))` in
//! integer-only arithmetic:
//! - [`log2_q64`] uses the exact bit-by-bit squaring method (64 fractional bits).
//! - [`exp2_q64`] uses a 64-entry table of `2^(2^-i)` (see `EXP2_TBL`, generated
//!   to 80-digit precision) so the fractional part is reconstructed with one
//!   multiply per set bit.
//! - [`mul_shr_64`] is an exact 128×128→256-bit multiply shifted right 64,
//!   so intermediates never overflow.
//!
//! Domain assumption (documented, mirrors the ldex AMM oracle): pool reserves
//! and weight numerators fit in 64 bits, so `x << 64` ratios do not overflow.
//! `pow` rounds toward zero; callers floor the final token output against the
//! trader.

/// 1.0 in Q64.64.
pub const ONE: u128 = 1u128 << 64;
const MASK64: u128 = u64::MAX as u128;

/// Exact `(a * b) >> 64`, computed via the full 256-bit product so neither the
/// product nor the shift overflows. The caller must guarantee the result fits
/// in `u128` (true for every use here: ratios < 1, or `reserve * fraction`).
#[must_use]
pub fn mul_shr_64(a: u128, b: u128) -> u128 {
    let (ah, al) = (a >> 64, a & MASK64);
    let (bh, bl) = (b >> 64, b & MASK64);
    let ll = al * bl;
    let lh = al * bh;
    let hl = ah * bl;
    let hh = ah * bh;
    // Sum the columns that land at bit offset 64, carrying into the high word.
    let mid = (ll >> 64) + (lh & MASK64) + (hl & MASK64);
    let hi = hh + (lh >> 64) + (hl >> 64) + (mid >> 64);
    // Result is bits [64, 192) of the 256-bit product.
    let lo_part = mid & MASK64;
    (hi << 64) | lo_part
}

/// Like [`mul_shr_64`] but returns `None` when the `(a*b)>>64` result does NOT
/// fit in `u128` (i.e. bits ≥ 128 of the product are set) instead of silently
/// wrapping. Used where the caller cannot guarantee the magnitude bound (e.g.
/// the spot-price oracle with extreme reserve/weight ratios) and wants to
/// saturate rather than record a wrapped value.
#[must_use]
pub fn mul_shr_64_checked(a: u128, b: u128) -> Option<u128> {
    let (ah, al) = (a >> 64, a & MASK64);
    let (bh, bl) = (b >> 64, b & MASK64);
    let ll = al * bl;
    let lh = al * bh;
    let hl = ah * bl;
    let hh = ah * bh;
    let mid = (ll >> 64) + (lh & MASK64) + (hl & MASK64);
    let hi = hh + (lh >> 64) + (hl >> 64) + (mid >> 64);
    // The u128 result is `(hi << 64) | lo_part`; it overflows iff `hi >= 2^64`.
    if hi >> 64 != 0 {
        return None;
    }
    Some((hi << 64) | (mid & MASK64))
}

/// `floor((a << 64) / b)` - Q64.64 division of an integer `a` by integer `b`.
/// `a` must be < 2^64: otherwise `a << 64` overflows `u128` and silently wraps.
/// The bound is a hard runtime `assert!` (a clean revert), NOT a `debug_assert!`,
/// because the guest is built in release with overflow-checks OFF - a wrap here
/// would corrupt the price and let a buyer drain the vault.
#[must_use]
pub fn div_to_q64(a: u128, b: u128) -> u128 {
    assert!(a < (1u128 << 64), "div_to_q64: numerator out of the 64-bit Q64.64 domain");
    assert!(b != 0, "div_to_q64 by zero");
    (a << 64) / b
}

/// `floor((a * 2^64) / b)` for Q64.64 values `a, b` with `a < 2^64`. The domain
/// bound is a hard runtime `assert!` for the same reason as [`div_to_q64`].
#[must_use]
pub fn div_q64(a: u128, b: u128) -> u128 {
    assert!(b != 0, "div_q64 by zero");
    assert!(a < (1u128 << 64), "div_q64: numerator out of the 64-bit Q64.64 domain");
    (a << 64) / b
}

/// `log2(x)` for a Q64.64 value `x > 0`, returned as a signed Q64.64.
#[must_use]
pub fn log2_q64(x: u128) -> i128 {
    assert!(x > 0, "log2 of zero");
    let msb = 127 - x.leading_zeros() as i32;
    let c = msb - 64; // characteristic (integer part of log2)
    let mut y = if c >= 0 { x >> c } else { x << (-c) }; // normalize to [ONE, 2*ONE)
    let mut result: i128 = (c as i128) << 64;
    let mut bit: u128 = ONE >> 1; // weight 2^-1 in Q64.64
    for _ in 0..64 {
        y = mul_shr_64(y, y);
        if y >= (ONE << 1) {
            y >>= 1;
            result += bit as i128;
        }
        bit >>= 1;
        if bit == 0 {
            break;
        }
    }
    result
}

/// `2^y` for a signed Q64.64 `y`, returned as Q64.64. Saturates to 0 when the
/// result underflows the representable range.
#[must_use]
pub fn exp2_q64(y: i128) -> u128 {
    let i = y >> 64; // floor of integer part (arithmetic shift)
    let f = (y - (i << 64)) as u128; // fractional part in [0, ONE)
    let mut r = ONE;
    for (k, &tbl) in EXP2_TBL.iter().enumerate() {
        // bit at position 63-k of f has weight 2^-(k+1)
        if (f >> (63 - k)) & 1 == 1 {
            r = mul_shr_64(r, tbl);
        }
    }
    if i >= 0 {
        let sh = i as u32;
        if sh >= 128 {
            return u128::MAX; // overflow - not reachable for base<=1 powers
        }
        r << sh
    } else {
        let sh = (-i) as u32;
        if sh >= 128 {
            return 0;
        }
        r >> sh
    }
}

/// `base^exp` for Q64.64 `base` in `(0, 1]` and Q64.64 `exp >= 0`, returned as
/// Q64.64 in `(0, 1]`. Used for the Balancer out-given-in ratio.
#[must_use]
pub fn pow_q64(base: u128, exp: u128) -> u128 {
    if exp == 0 || base == ONE {
        return ONE;
    }
    assert!(base > 0, "pow of zero base");
    let l = log2_q64(base); // <= 0 for base <= ONE
    let neg = l < 0;
    // `|log2(base)| * exp` (Q64.64) can exceed u128 / the positive i128 range for a
    // base near 0 with a huge exponent (a thin token weight near sale end). Use the
    // checked multiply and require the magnitude to fit in the *signed* domain: an
    // `as i128` of a value with bit 127 set would flip the sign, turning the
    // intended-negative `p` (base < 1) into a large POSITIVE exponent and making
    // `exp2_q64` return >= ONE - corrupting `1 - powv` into a wrap (vault-drain /
    // buy-bricking class bug). For base in (0,1] an effectively infinite negative
    // exponent has the limit 0, which is also the safe floor against the trader.
    let prod = match mul_shr_64_checked(l.unsigned_abs(), exp) {
        Some(m) if m < (1u128 << 127) => m as i128,
        _ => return if neg { 0 } else { u128::MAX },
    };
    let p = if neg { -prod } else { prod };
    exp2_q64(p)
}

/// `EXP2_TBL[k] = round(2^(2^-(k+1)) * 2^64)` for k = 0..63 (Q64.64), generated
/// to 80-decimal precision.
pub(crate) const EXP2_TBL: [u128; 64] = [
    0x00000000000000016a09e667f3bcc909,
    0x0000000000000001306fe0a31b7152df,
    0x0000000000000001172b83c7d517adce,
    0x00000000000000010b5586cf9890f62a,
    0x0000000000000001059b0d31585743ae,
    0x000000000000000102c9a3e778060ee7,
    0x00000000000000010163da9fb33356d8,
    0x000000000000000100b1afa5abcbed61,
    0x00000000000000010058c86da1c09ea2,
    0x0000000000000001002c605e2e8cec50,
    0x000000000000000100162f3904051fa1,
    0x0000000000000001000b175effdc76ba,
    0x000000000000000100058ba01fb9f96d,
    0x00000000000000010002c5cc37da9492,
    0x0000000000000001000162e525ee0547,
    0x00000000000000010000b17255775c04,
    0x0000000000000001000058b91b5bc9ae,
    0x000000000000000100002c5c89d5ec6d,
    0x00000000000000010000162e43f4f831,
    0x000000000000000100000b1721bcfc9a,
    0x00000000000000010000058b90cf1e6e,
    0x0000000000000001000002c5c863b73f,
    0x000000000000000100000162e430e5a2,
    0x0000000000000001000000b172183551,
    0x000000000000000100000058b90c0b49,
    0x00000000000000010000002c5c8601cc,
    0x0000000000000001000000162e42fff0,
    0x00000000000000010000000b17217fbb,
    0x0000000000000001000000058b90bfce,
    0x000000000000000100000002c5c85fe3,
    0x00000000000000010000000162e42ff1,
    0x000000000000000100000000b17217f8,
    0x00000000000000010000000058b90bfc,
    0x0000000000000001000000002c5c85fe,
    0x000000000000000100000000162e42ff,
    0x0000000000000001000000000b17217f,
    0x000000000000000100000000058b90c0,
    0x00000000000000010000000002c5c860,
    0x0000000000000001000000000162e430,
    0x00000000000000010000000000b17218,
    0x0000000000000001000000000058b90c,
    0x000000000000000100000000002c5c86,
    0x00000000000000010000000000162e43,
    0x000000000000000100000000000b1721,
    0x00000000000000010000000000058b91,
    0x0000000000000001000000000002c5c8,
    0x000000000000000100000000000162e4,
    0x0000000000000001000000000000b172,
    0x000000000000000100000000000058b9,
    0x00000000000000010000000000002c5d,
    0x0000000000000001000000000000162e,
    0x00000000000000010000000000000b17,
    0x0000000000000001000000000000058c,
    0x000000000000000100000000000002c6,
    0x00000000000000010000000000000163,
    0x000000000000000100000000000000b1,
    0x00000000000000010000000000000059,
    0x0000000000000001000000000000002c,
    0x00000000000000010000000000000016,
    0x0000000000000001000000000000000b,
    0x00000000000000010000000000000006,
    0x00000000000000010000000000000003,
    0x00000000000000010000000000000001,
    0x00000000000000010000000000000001,
];

#[cfg(test)]
mod tests {
    use super::*;

    fn q64_to_f64(x: u128) -> f64 {
        x as f64 / ONE as f64
    }

    #[test]
    fn mul_shr_64_matches_widening() {
        // Operands < ONE so the direct product fits in u128 for the reference.
        for (a, b) in [(ONE / 2, ONE / 4), (ONE / 3, ONE / 7), (ONE - 1, ONE - 1)] {
            assert_eq!(mul_shr_64(a, b), (a * b) >> 64);
        }
        // ONE is the multiplicative identity even where the direct product overflows.
        assert_eq!(mul_shr_64(ONE, 12345 * ONE), 12345 * ONE);
        assert_eq!(mul_shr_64(ONE, ONE), ONE);
        assert_eq!(mul_shr_64(2 * ONE, 3 * ONE), 6 * ONE);
    }

    #[test]
    fn log2_exp2_round_trip() {
        for &x in &[ONE, 2 * ONE, ONE / 2, ONE / 10, 3 * ONE, (ONE / 100) * 99] {
            let l = log2_q64(x);
            let back = exp2_q64(l);
            let rel = (back as f64 - x as f64).abs() / x as f64;
            assert!(rel < 1e-12, "log2/exp2 round trip x={x} back={back} rel={rel}");
        }
    }

    /// Compare pow_q64 against Python-computed exact vectors (Decimal, prec=80).
    #[test]
    fn pow_matches_reference_vectors() {
        // (base_q64, exp_q64, expect_q64)
        let vectors: &[(u128, u128, u128)] = &[
            (0x8000000000000000, 0x10000000000000000, 0x8000000000000000), // 0.5^1
            (0x8000000000000000, 0x20000000000000000, 0x4000000000000000), // 0.5^2
            (0xe666666666666800, 0x630000000000000000, 0x1ef23eecec32b),   // 0.9^99
            (0xfd70a3d70a3d7000, 0x295fad4093b99e0, 0xfff958e34e136a01),   // 0.99^0.0101
            (0x8000000000000000, 0x8000000000000000, 0xb504f333f9de6484),  // 0.5^0.5
            (0x1f7ced916872b000, 0x3b333333333334000, 0x1c209a1e4e5846),   // 0.123^3.7
            (0xc000000000000000, 0x5555555530aed400, 0xe897685796a8556e),  // 0.75^0.333..
        ];
        for &(base, exp, expect) in vectors {
            let got = pow_q64(base, exp);
            let rel = (got as f64 - expect as f64).abs() / (expect as f64).max(1.0);
            assert!(
                rel < 1e-6,
                "pow base=0x{base:x} exp=0x{exp:x}: got=0x{got:x} expect=0x{expect:x} rel={rel:.3e}",
            );
        }
    }

    #[test]
    fn pow_edge_cases() {
        assert_eq!(pow_q64(ONE / 3, 0), ONE, "x^0 == 1");
        assert_eq!(pow_q64(ONE, 7 * ONE), ONE, "1^x == 1");
        assert_eq!(pow_q64(ONE / 2, ONE), ONE / 2, "x^1 == x");
    }

    #[test]
    fn pow_is_monotonic_in_exponent_for_base_lt_1() {
        // base < 1: larger exponent => smaller result
        let base = (ONE / 10) * 7; // 0.7
        let mut last = u128::MAX;
        for e in 1..=20u128 {
            let v = pow_q64(base, e * ONE);
            assert!(v < last, "0.7^{e} should decrease");
            last = v;
        }
    }

    #[test]
    fn pow_never_exceeds_one_for_base_in_unit_interval() {
        // Invariant for the Balancer out-given-in ratio: base in (0, ONE], exp >= 0
        // => result in (0, ONE]. A result > ONE makes `ONE - powv` underflow/wrap in
        // buy_tokens_out (overflow-checks off in the release guest), draining/bricking
        // the reserve. Cover the pathological corner: base near 0 with a huge exponent
        // (a thin token weight near sale end) that previously sign-flipped to powv > ONE.
        let bases = [
            1u128,                // ~2^-64, the smallest representable base
            ONE / 1_000_000,
            ONE / 2,
            (ONE / 100) * 99,
            ONE - 1,
            ONE,
        ];
        let exps = [
            0u128,
            1,
            ONE,
            99 * ONE,
            1u128 << 96,
            1u128 << 120,
            1u128 << 127,
            u128::MAX, // exponent ~2^64 (Q64.64), the worst case from a w_end=1 weight
        ];
        for &base in &bases {
            for &exp in &exps {
                let v = pow_q64(base, exp);
                assert!(v <= ONE, "pow base=0x{base:x} exp=0x{exp:x} => 0x{v:x} > ONE");
            }
        }
    }

    #[test]
    fn weighted_out_given_in_sanity() {
        // Equal weights (0.5/0.5) reduce to the constant-product swap:
        // out = rt * (1 - (rc/(rc+cin))^1).
        let rc = 1_000_000u128;
        let rt = 1_000_000u128;
        let cin = 100_000u128;
        let base = div_to_q64(rc, rc + cin);
        let ratio = div_q64(ONE / 2, ONE / 2); // = ONE
        let powv = pow_q64(base, ratio);
        let out = mul_shr_64(rt, ONE - powv);
        let cp = rt * cin / (rc + cin);
        let diff = out.abs_diff(cp);
        assert!(diff <= 2, "weighted (0.5/0.5) ~ constant product: out={out} cp={cp}");
        assert_eq!(q64_to_f64(ONE), 1.0);
    }

    #[test]
    #[should_panic(expected = "out of the 64-bit Q64.64 domain")]
    fn div_to_q64_reverts_above_domain() {
        // A numerator >= 2^64 would overflow `a << 64` and silently wrap in a
        // release guest (overflow-checks off), corrupting the Balancer price and
        // letting a dust buy drain the vault. The hard assert must revert instead.
        let _ = div_to_q64(1u128 << 64, (1u128 << 64) + 5_000);
    }
}
