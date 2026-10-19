//! Time-based one-time passwords (RFC 6238) for second factors: secrets, codes, the `otpauth://` URI authenticator
//! apps enroll from (with its QR code as SVG), and recovery codes.
//!
//! Codes are 6 digits over 30-second steps with HMAC-SHA1, the parameters every authenticator app supports. A code
//! is accepted one step early or late (clock drift), and never twice: the service records the step of the last
//! accepted code and refuses that step and earlier ones.

use data_encoding::BASE32_NOPAD;
use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;

use super::tokens::ct_eq;

/// Seconds per step.
pub const STEP_SECS: u64 = 30;
/// Digits per code.
pub const DIGITS: u32 = 6;
/// Steps before and after the current one whose codes are accepted.
pub const WINDOW: u64 = 1;
/// Bytes of a secret (160 bits, SHA1's output size, as RFC 4226 recommends).
pub const SECRET_LEN: usize = 20;
/// Recovery codes issued when a factor is confirmed or the codes are regenerated.
pub const RECOVERY_CODES: usize = 8;

/// A fresh random secret.
pub fn new_secret() -> Vec<u8> {
    rand::random::<[u8; SECRET_LEN]>().to_vec()
}

/// The secret as authenticator apps take it: base32 without padding.
pub fn encode_secret(secret: &[u8]) -> String {
    BASE32_NOPAD.encode(secret)
}

/// The code of `step` (`unix seconds / 30`).
pub fn code_at(secret: &[u8], step: u64) -> String {
    let mut mac = <Hmac<Sha1> as KeyInit>::new_from_slice(secret).expect("HMAC takes any key length");
    mac.update(&step.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = (digest[digest.len() - 1] & 0x0f) as usize;
    let binary =
        u32::from_be_bytes([digest[offset] & 0x7f, digest[offset + 1], digest[offset + 2], digest[offset + 3]]);
    format!("{:0width$}", binary % 10u32.pow(DIGITS), width = DIGITS as usize)
}

/// The current step at `now_s` (seconds since the epoch).
pub fn step_at(now_s: u64) -> u64 {
    now_s / STEP_SECS
}

/// A typed code with spaces and dashes removed, if it has the right shape (digits only).
pub fn normalize_code(code: &str) -> Option<String> {
    let code: String = code.chars().filter(|c| !c.is_whitespace() && *c != '-').collect();
    (code.len() == DIGITS as usize && code.bytes().all(|b| b.is_ascii_digit())).then_some(code)
}

/// The step within the window around `now_s` whose code is `code`, if any. Every candidate is compared (in
/// constant time), whichever matches, so timing reveals nothing about which step did.
pub fn matching_step(secret: &[u8], code: &str, now_s: u64) -> Option<u64> {
    let code = normalize_code(code)?;
    let current = step_at(now_s);
    let mut found = None;
    for step in current.saturating_sub(WINDOW)..=current + WINDOW {
        if ct_eq(&code_at(secret, step), &code) {
            found = Some(step);
        }
    }
    found
}

/// Percent-encode everything but RFC 3986 unreserved characters (for the URI's label and query values).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The `otpauth://totp/...` URI an authenticator app enrolls from: `issuer` names the app (shown in the
/// authenticator), `account` the user (their email). Colons in the label would split it, so they are replaced.
pub fn otpauth_uri(issuer: &str, account: &str, secret: &[u8]) -> String {
    let issuer = issuer.replace(':', " ");
    let account = account.replace(':', " ");
    format!(
        "otpauth://totp/{}:{}?secret={}&issuer={}&algorithm=SHA1&digits={DIGITS}&period={STEP_SECS}",
        percent_encode(&issuer),
        percent_encode(&account),
        encode_secret(secret),
        percent_encode(&issuer),
    )
}

/// The URI as a QR code: an inline-able `<svg>` (no XML prolog) that scales to its container.
pub fn qr_svg(uri: &str) -> crate::Result<String> {
    use qrcode::render::svg;
    let code = qrcode::QrCode::new(uri.as_bytes()).map_err(|e| anyhow::anyhow!("QR code: {e}"))?;
    let svg = code.render::<svg::Color>().min_dimensions(200, 200).build();
    let body = svg.find("<svg").map(|at| &svg[at..]).unwrap_or(&svg);
    Ok(body.to_string())
}

/// Recovery code alphabet: lowercase letters and digits without the look-alikes (`l`, `o`, `0`, `1`).
const RECOVERY_ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";

/// `n` fresh recovery codes, `xxxxx-xxxxx` (10 characters of a 32-letter alphabet: 50 bits each).
pub fn recovery_codes(n: usize) -> Vec<String> {
    (0..n)
        .map(|_| {
            let chars: Vec<u8> = (0..10).map(|_| RECOVERY_ALPHABET[rand::random::<u8>() as usize % 32]).collect();
            format!("{}-{}", String::from_utf8_lossy(&chars[..5]), String::from_utf8_lossy(&chars[5..]))
        })
        .collect()
}

/// A typed recovery code as it is hashed: lowercase, without spaces or dashes. `None` if it can't be one.
pub fn normalize_recovery_code(code: &str) -> Option<String> {
    let code: String = code.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_lowercase()).collect();
    (code.len() == 10 && code.bytes().all(|b| RECOVERY_ALPHABET.contains(&b))).then_some(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6238 appendix B (SHA1, secret `12345678901234567890`), truncated to 6 digits.
    #[test]
    fn rfc_6238_vectors() {
        let secret = b"12345678901234567890";
        for (time, expected) in [
            (59u64, "287082"),
            (1111111109, "081804"),
            (1111111111, "050471"),
            (1234567890, "005924"),
            (2000000000, "279037"),
            (20000000000, "353130"),
        ] {
            assert_eq!(code_at(secret, step_at(time)), expected, "t={time}");
        }
    }

    #[test]
    fn window_and_normalization() {
        let secret = new_secret();
        let now = 1_700_000_000;
        let step = step_at(now);
        assert_eq!(matching_step(&secret, &code_at(&secret, step), now), Some(step));
        assert_eq!(matching_step(&secret, &code_at(&secret, step - 1), now), Some(step - 1));
        assert_eq!(matching_step(&secret, &code_at(&secret, step + 1), now), Some(step + 1));
        assert_eq!(matching_step(&secret, &code_at(&secret, step + 2), now), None, "outside the window");
        let spaced = format!("{} {}", &code_at(&secret, step)[..3], &code_at(&secret, step)[3..]);
        assert_eq!(matching_step(&secret, &spaced, now), Some(step), "spaces are ignored");
        assert_eq!(matching_step(&secret, "12345", now), None);
        assert_eq!(matching_step(&secret, "abcdef", now), None);
    }

    #[test]
    fn uri_and_qr() {
        let secret = b"12345678901234567890";
        let uri = otpauth_uri("My App", "me@example.com", secret);
        assert_eq!(
            uri,
            "otpauth://totp/My%20App:me%40example.com?secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ&issuer=My%20App\
             &algorithm=SHA1&digits=6&period=30"
        );
        let svg = qr_svg(&uri).unwrap();
        assert!(svg.starts_with("<svg") && svg.ends_with("</svg>"), "{svg}");
        assert_eq!(otpauth_uri("a:b", "c:d", secret).split('?').next().unwrap(), "otpauth://totp/a%20b:c%20d");
    }

    #[test]
    fn recovery_codes_shape() {
        let codes = recovery_codes(RECOVERY_CODES);
        assert_eq!(codes.len(), RECOVERY_CODES);
        for c in &codes {
            assert_eq!(c.len(), 11, "{c}");
            assert_eq!(normalize_recovery_code(c).unwrap(), c.replace('-', ""));
            assert_eq!(normalize_recovery_code(&c.to_uppercase().replace('-', " ")).unwrap(), c.replace('-', ""));
        }
        assert_eq!(normalize_recovery_code("short"), None);
        assert_eq!(normalize_recovery_code("l0000-00000"), None, "look-alikes are not in the alphabet");
    }
}
