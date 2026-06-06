//! Parsing + formatting helpers shared by the CLI commands.

use std::str::FromStr;

use nssa_core::{account::AccountId, program::ProgramId};

/// Parse an account id from either 64-char hex (optional `0x` prefix) or the
/// wallet's base58 form (`Public/<b58>`, `Private/<b58>`, or bare `<b58>`).
pub fn parse_account(s: &str) -> Result<AccountId, String> {
    let s = s.trim();
    let body = s
        .strip_prefix("Public/")
        .or_else(|| s.strip_prefix("Private/"))
        .unwrap_or(s);
    // Accept hex with or without a `0x` prefix (consistent with `parse_program`).
    let hex = body.strip_prefix("0x").unwrap_or(body);
    if hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(AccountId::new(parse_bytes32(hex)?));
    }
    AccountId::from_str(body).map_err(|e| format!("bad account id '{s}': {e:?}"))
}

/// Parse a 32-byte program id (RISC0 image id) from hex. Reconstructed as
/// `[u32; 8]` with native-endian lanes, matching `AccountId::for_public_pda`.
pub fn parse_program(s: &str) -> Result<ProgramId, String> {
    let b = parse_bytes32(s)?;
    let mut pid = [0u32; 8];
    for (i, lane) in pid.iter_mut().enumerate() {
        let mut w = [0u8; 4];
        w.copy_from_slice(&b[i * 4..i * 4 + 4]);
        *lane = u32::from_ne_bytes(w);
    }
    Ok(pid)
}

fn parse_bytes32(s: &str) -> Result<[u8; 32], String> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    if s.len() != 64 {
        return Err(format!("expected 64 hex chars (32 bytes), got {}", s.len()));
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
            .map_err(|_| format!("invalid hex at byte {i}"))?;
    }
    Ok(out)
}

/// Parse a 32-byte Merkle/allowlist root from hex; empty string → all-zero
/// (sentinel for "no allowlist gate").
pub fn parse_root(s: &str) -> Result<[u8; 32], String> {
    if s.trim().is_empty() {
        return Ok([0u8; 32]);
    }
    parse_bytes32(s)
}

/// Parse a weight into Q64.64. Accepts `"0.99"` (decimal fraction) or `"99/100"`.
pub fn parse_weight_q64(s: &str) -> Result<u128, String> {
    let one: u128 = 1u128 << 64;
    if let Some((num, den)) = s.split_once('/') {
        let num: u128 = num.trim().parse().map_err(|_| "bad weight numerator")?;
        let den: u128 = den.trim().parse().map_err(|_| "bad weight denominator")?;
        if den == 0 {
            return Err("weight denominator is zero".into());
        }
        // `num << 64` truncates silently for num >= 2^64 (host build, shift not
        // overflow-checked), which could sneak a wrong value past the q>one guard.
        if num >= one {
            return Err("weight numerator out of range (must be < 2^64)".into());
        }
        let q = (num << 64) / den;
        // Mirror the decimal branch's [0, 1] bound so e.g. `3/2` is a clear local
        // error, not a remote on-chain revert. (The on-chain (0,1)-exclusive
        // check still rejects the 0/1 endpoints at create time.)
        if q > one {
            return Err("weight must be in [0, 1]".into());
        }
        return Ok(q);
    }
    let f: f64 = s.trim().parse().map_err(|_| "weight must be a decimal or num/den")?;
    if !(0.0..=1.0).contains(&f) {
        return Err("weight must be in [0, 1]".into());
    }
    Ok((f * one as f64) as u128)
}

/// Q64.64 fixed-point → f64 for display.
pub fn q64_to_f64(x: u128) -> f64 {
    x as f64 / (1u128 << 64) as f64
}

/// Lowercase hex of a 32-byte account id.
pub fn hex_account(id: AccountId) -> String {
    to_hex(&id.to_bytes())
}

/// Lowercase hex of a program id (native-endian lanes flattened to 32 bytes).
pub fn hex_program(pid: ProgramId) -> String {
    let mut b = [0u8; 32];
    for (i, lane) in pid.iter().enumerate() {
        b[i * 4..i * 4 + 4].copy_from_slice(&lane.to_ne_bytes());
    }
    to_hex(&b)
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE: u128 = 1u128 << 64;

    #[test]
    fn parse_account_hex_forms_roundtrip() {
        let id = AccountId::new([9u8; 32]);
        let h = hex_account(id);
        assert_eq!(h.len(), 64);
        assert_eq!(parse_account(&h).unwrap(), id, "bare hex");
        assert_eq!(parse_account(&format!("0x{h}")).unwrap(), id, "0x-prefixed hex");
        assert_eq!(parse_account(&format!("Public/{h}")).unwrap(), id, "Public/ + hex");
        assert_eq!(parse_account(&format!("Private/{h}")).unwrap(), id, "Private/ + hex");
        assert_eq!(parse_account(&format!("  {h}\n")).unwrap(), id, "trimmed");
    }

    #[test]
    fn parse_account_rejects_invalid() {
        assert!(parse_account("____").is_err(), "'_' is neither hex nor base58");
        assert!(parse_account("Public/__bad__").is_err(), "prefix stripped then invalid");
        // 63 hex chars: not 64 -> falls to base58, but '0' is not a base58 char.
        assert!(parse_account(&"0".repeat(63)).is_err());
    }

    #[test]
    fn parse_program_roundtrips_native_endian() {
        // distinct bytes so a byte-order bug would be caught.
        let h: String = (0u8..32).map(|i| format!("{:02x}", i.wrapping_mul(7).wrapping_add(1))).collect();
        let pid = parse_program(&h).unwrap();
        assert_eq!(hex_program(pid), h, "parse_program/hex_program round-trip");
        assert_eq!(parse_program(&format!("0x{h}")).unwrap(), pid);
    }

    #[test]
    fn parse_program_rejects_bad_length() {
        assert!(parse_program("abcd").is_err());
        assert!(parse_program(&"a".repeat(63)).is_err());
        assert!(parse_program(&"zz".repeat(32)).is_err(), "64 chars but not hex");
    }

    #[test]
    fn parse_weight_decimal_forms() {
        assert_eq!(parse_weight_q64("0").unwrap(), 0);
        assert_eq!(parse_weight_q64("1").unwrap(), ONE);
        assert_eq!(parse_weight_q64("1.0").unwrap(), ONE);
        assert!((q64_to_f64(parse_weight_q64("0.5").unwrap()) - 0.5).abs() < 1e-9);
        assert!((q64_to_f64(parse_weight_q64("0.99").unwrap()) - 0.99).abs() < 1e-9);
    }

    #[test]
    fn parse_weight_fraction_forms_are_exact() {
        assert_eq!(parse_weight_q64("1/1").unwrap(), ONE);
        assert_eq!(parse_weight_q64("1/2").unwrap(), ONE / 2);
        assert_eq!(parse_weight_q64("99/100").unwrap(), (99u128 << 64) / 100);
        assert_eq!(parse_weight_q64(" 3 / 4 ").unwrap(), (3u128 << 64) / 4, "whitespace tolerated");
    }

    #[test]
    fn parse_weight_rejects_invalid() {
        assert!(parse_weight_q64("1.5").is_err(), "> 1");
        assert!(parse_weight_q64("-0.1").is_err(), "< 0");
        assert!(parse_weight_q64("1/0").is_err(), "zero denominator");
        assert!(parse_weight_q64("abc").is_err(), "not a number");
        assert!(parse_weight_q64("x/2").is_err(), "bad numerator");
        assert!(parse_weight_q64("1/y").is_err(), "bad denominator");
    }

    #[test]
    fn q64_to_f64_known_values() {
        assert_eq!(q64_to_f64(0), 0.0);
        assert_eq!(q64_to_f64(ONE), 1.0);
        assert!((q64_to_f64(ONE / 2) - 0.5).abs() < 1e-12);
    }

    #[test]
    fn hex_helpers_lowercase() {
        let id = AccountId::new([0xABu8; 32]);
        assert_eq!(hex_account(id), "ab".repeat(32));
        let pid = parse_program(&"cd".repeat(32)).unwrap();
        assert_eq!(hex_program(pid), "cd".repeat(32));
    }
}
