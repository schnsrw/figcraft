//! Time-based one-time passwords (RFC 6238, HMAC-SHA1, six digits, 30-second steps) for platform
//! admins. HMAC-SHA1 is what every authenticator app speaks; it is used here only as a MAC over a
//! counter, which is not affected by SHA-1's collision weakness.

use ring::hmac;
use ring::rand::SecureRandom as _;

/// Seconds per step.
pub(crate) const STEP_SECS: i64 = 30;
/// Digits in a code.
pub(crate) const DIGITS: u32 = 6;
/// Bytes of secret (160 bits, the RFC 4226 recommendation).
pub(crate) const SECRET_BYTES: usize = 20;
/// Steps accepted either side of the current one (clock skew).
const SKEW_STEPS: i64 = 1;

/// A fresh random secret.
pub(crate) fn generate_secret() -> Option<[u8; SECRET_BYTES]> {
    let mut secret = [0u8; SECRET_BYTES];
    ring::rand::SystemRandom::new().fill(&mut secret).ok()?;
    Some(secret)
}

/// RFC 4648 base32 without padding, as authenticator apps expect.
pub(crate) fn base32(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for byte in bytes {
        buffer = (buffer << 8) | u32::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            let index = usize::try_from((buffer >> bits) & 31).unwrap_or(0);
            out.push(char::from(ALPHABET.get(index).copied().unwrap_or(b'A')));
        }
        buffer &= (1 << bits) - 1;
    }
    if bits > 0 {
        let index = usize::try_from((buffer << (5 - bits)) & 31).unwrap_or(0);
        out.push(char::from(ALPHABET.get(index).copied().unwrap_or(b'A')));
    }
    out
}

/// The `otpauth://` URI authenticator apps import (the issuer and label are percent-encoded).
pub(crate) fn provisioning_uri(issuer: &str, label: &str, secret: &[u8]) -> String {
    format!(
        "otpauth://totp/{}:{}?secret={}&issuer={}&algorithm=SHA1&digits={DIGITS}&period={STEP_SECS}",
        percent(issuer),
        percent(label),
        base32(secret),
        percent(issuer)
    )
}

fn percent(value: &str) -> String {
    value.bytes().fold(String::new(), |mut out, b| {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
        out
    })
}

/// The code for time step `counter`.
pub(crate) fn code_at(secret: &[u8], counter: u64) -> u32 {
    let key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, secret);
    let tag = hmac::sign(&key, &counter.to_be_bytes());
    let digest = tag.as_ref();
    let offset = usize::from(digest.last().copied().unwrap_or(0) & 0x0f);
    let slice: [u8; 4] = digest.get(offset..offset + 4).and_then(|s| s.try_into().ok()).unwrap_or([0; 4]);
    (u32::from_be_bytes(slice) & 0x7fff_ffff) % 10u32.pow(DIGITS)
}

/// Check `submitted` (exactly six ASCII digits) against the steps around `now_unix`. Returns the
/// matching step so the caller can refuse a replay of the same or an earlier step.
pub(crate) fn verify(secret: &[u8], submitted: &str, now_unix: i64) -> Option<i64> {
    if submitted.len() != DIGITS as usize || !submitted.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let submitted: u32 = submitted.parse().ok()?;
    let current = now_unix.checked_div(STEP_SECS)?;
    let mut matched = None;
    // Every candidate is computed and compared without an early exit.
    for step in (current - SKEW_STEPS)..=(current + SKEW_STEPS) {
        let Ok(counter) = u64::try_from(step) else { continue };
        let expected = code_at(secret, counter);
        if constant_time_eq(&expected.to_be_bytes(), &submitted.to_be_bytes()) {
            matched = Some(step);
        }
    }
    matched
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && std::hint::black_box(a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y))) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const RFC_SECRET: &[u8] = b"12345678901234567890";

    #[test]
    fn matches_the_rfc_6238_sha1_vectors() {
        // RFC 6238 appendix B lists eight digits; the six-digit code is the same value modulo 10^6.
        for (time, eight) in [
            (59u64, 94_287_082u32),
            (1_111_111_109, 7_081_804),
            (1_111_111_111, 14_050_471),
            (1_234_567_890, 89_005_924),
            (2_000_000_000, 69_279_037),
        ] {
            assert_eq!(code_at(RFC_SECRET, time / 30), eight % 1_000_000, "t={time}");
        }
    }

    #[test]
    fn base32_matches_rfc_4648_vectors() {
        assert_eq!(base32(b""), "");
        assert_eq!(base32(b"f"), "MY");
        assert_eq!(base32(b"fo"), "MZXQ");
        assert_eq!(base32(b"foobar"), "MZXW6YTBOI");
        assert_eq!(base32(RFC_SECRET), "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ");
    }

    #[test]
    fn verify_accepts_one_step_of_skew_and_reports_the_step() {
        let now = 1_111_111_109;
        let code = format!("{:06}", code_at(RFC_SECRET, (now / 30) as u64));
        assert_eq!(verify(RFC_SECRET, &code, now), Some(now / 30));
        assert_eq!(verify(RFC_SECRET, &code, now + 30), Some(now / 30));
        assert_eq!(verify(RFC_SECRET, &code, now - 30), Some(now / 30));
        assert_eq!(verify(RFC_SECRET, &code, now + 90), None);
        for bad in ["", "12345", "1234567", "12345a", " 12345", "+12345"] {
            assert_eq!(verify(RFC_SECRET, bad, now), None, "{bad:?}");
        }
    }

    #[test]
    fn provisioning_uri_is_encoded() {
        let uri = provisioning_uri("FigCraft Platform", "ada@example.com", RFC_SECRET);
        assert!(uri.starts_with("otpauth://totp/FigCraft%20Platform:ada%40example.com?secret=GEZDGNBV"));
        assert!(uri.ends_with("&issuer=FigCraft%20Platform&algorithm=SHA1&digits=6&period=30"));
    }

    #[test]
    fn generated_secrets_differ() {
        assert_ne!(generate_secret(), generate_secret());
    }
}
