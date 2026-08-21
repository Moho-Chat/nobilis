//! RFC 6238 time-based one-time passwords.
//!
//! Ported from sockchat-rs's `auth/totp.rs`
//! (<https://gitgud.io/jcmoon/sockchat-rs>). Verified against the RFC 6238
//! Appendix B test vectors. This is what makes 2FA login unattended: given
//! the base32 secret from the "can't scan the QR code?" link during 2FA
//! setup, the daemon derives the same six digits an authenticator app
//! would show.

use anyhow::{bail, Context, Result};
use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;

/// Seconds per code. RFC 6238 recommends 30 and every implementation uses it.
pub const STEP_SECS: u64 = 30;
/// Digits in a generated code. XenForo uses six.
pub const DIGITS: u32 = 6;

/// Generate the code for `unix_time`.
pub fn generate_at(secret: &[u8], unix_time: u64, digits: u32, step: u64) -> Result<String> {
    if secret.is_empty() {
        bail!("TOTP secret is empty");
    }
    if !(1..=9).contains(&digits) {
        bail!("TOTP digit count {digits} is out of range");
    }

    let counter = unix_time / step;
    let mut mac = <Hmac<Sha1> as KeyInit>::new_from_slice(secret).map_err(|e| anyhow::anyhow!("TOTP secret is not a valid HMAC key: {e}"))?;
    mac.update(&counter.to_be_bytes());
    let hash = mac.finalize().into_bytes();

    // Dynamic truncation (RFC 4226 §5.4): the low nibble of the last byte
    // picks where to read a 4-byte window, and the top bit is masked off so
    // the value is positive regardless of the reader's integer signedness.
    let offset = (hash[hash.len() - 1] & 0x0f) as usize;
    let binary = u32::from_be_bytes([hash[offset] & 0x7f, hash[offset + 1], hash[offset + 2], hash[offset + 3]]);

    let modulus = 10u32.pow(digits);
    Ok(format!("{:0width$}", binary % modulus, width = digits as usize))
}

/// Generate the code for right now.
pub fn generate_now(secret: &[u8]) -> Result<String> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).context("system clock is before the Unix epoch")?.as_secs();
    generate_at(secret, now, DIGITS, STEP_SECS)
}

/// Decode a secret as shown during 2FA setup. Spaces and lowercase are
/// accepted since that's how sites display them; padding is optional.
pub fn decode_secret(secret: &str) -> Result<Vec<u8>> {
    let cleaned: String = secret.chars().filter(|c| !c.is_whitespace() && *c != '-').collect::<String>().to_ascii_uppercase();
    if cleaned.is_empty() {
        bail!("TOTP secret is empty");
    }
    base32::decode(base32::Alphabet::Rfc4648 { padding: false }, &cleaned).filter(|b| !b.is_empty()).context("TOTP secret is not valid base32")
}

/// Seconds until the current code expires, for deciding whether a code
/// about to be generated is about to roll over.
pub fn seconds_remaining() -> u64 {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    STEP_SECS - (now % STEP_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RFC_SECRET: &[u8] = b"12345678901234567890";

    #[test]
    fn matches_rfc6238_test_vectors() {
        for (time, expected) in [
            (59u64, "94287082"),
            (1111111109, "07081804"),
            (1111111111, "14050471"),
            (1234567890, "89005924"),
            (2000000000, "69279037"),
            (20000000000, "65353130"),
        ] {
            assert_eq!(generate_at(RFC_SECRET, time, 8, STEP_SECS).unwrap(), expected, "vector at T={time}");
        }
    }

    #[test]
    fn six_digit_codes_are_the_low_digits_of_the_eight_digit_ones() {
        for (time, eight) in [(59u64, "94287082"), (1111111111, "14050471")] {
            let six = generate_at(RFC_SECRET, time, 6, STEP_SECS).unwrap();
            assert_eq!(six, &eight[2..]);
            assert_eq!(six.len(), 6);
        }
    }

    #[test]
    fn codes_are_stable_within_a_step_and_change_across_it() {
        let start = 999_999_990u64;
        assert_eq!(start % 30, 0, "test needs a window-aligned timestamp");
        let a = generate_at(RFC_SECRET, start, 6, 30).unwrap();
        let b = generate_at(RFC_SECRET, start + 29, 6, 30).unwrap();
        let c = generate_at(RFC_SECRET, start + 30, 6, 30).unwrap();
        assert_eq!(a, b, "same 30s window must give the same code");
        assert_ne!(a, c, "next window must differ");
    }

    #[test]
    fn codes_keep_leading_zeros() {
        let code = generate_at(RFC_SECRET, 1111111109, 8, STEP_SECS).unwrap();
        assert_eq!(code, "07081804");
        assert_eq!(code.len(), 8);
    }

    #[test]
    fn decodes_secrets_as_users_paste_them() {
        let canonical = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";
        assert_eq!(decode_secret(canonical).unwrap(), RFC_SECRET);
        for variant in ["gezdgnbvgy3tqojqgezdgnbvgy3tqojq", "GEZD GNBV GY3T QOJQ GEZD GNBV GY3T QOJQ", "GEZD-GNBV-GY3T-QOJQ-GEZD-GNBV-GY3T-QOJQ"] {
            assert_eq!(decode_secret(variant).unwrap(), RFC_SECRET, "{variant}");
        }
    }

    #[test]
    fn rejects_bad_secrets_rather_than_producing_wrong_codes() {
        assert!(decode_secret("").is_err());
        assert!(decode_secret("   ").is_err());
        assert!(decode_secret("18890!!!").is_err());
        assert!(generate_at(&[], 0, 6, 30).is_err());
        assert!(generate_at(RFC_SECRET, 0, 0, 30).is_err());
        assert!(generate_at(RFC_SECRET, 0, 20, 30).is_err());
    }

    #[test]
    fn end_to_end_from_a_pasted_secret() {
        let secret = decode_secret("gezd gnbv gy3t qojq gezd gnbv gy3t qojq").unwrap();
        assert_eq!(generate_at(&secret, 59, 8, STEP_SECS).unwrap(), "94287082");
        let now = generate_now(&secret).unwrap();
        assert_eq!(now.len(), 6);
        assert!(now.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn remaining_seconds_are_within_the_step() {
        let r = seconds_remaining();
        assert!(r > 0 && r <= STEP_SECS, "got {r}");
    }
}
