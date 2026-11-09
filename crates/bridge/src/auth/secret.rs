//! Random values and the hex they travel as.

use protocol::auth::CROCKFORD_ALPHABET;
use protocol::{Challenge, PairingCode};
use sha2::{Digest, Sha256};

/// The OS random source failed. Nothing sensible can continue without it.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("the OS random number generator failed")]
pub struct RandomUnavailable;

/// `N` bytes from the OS CSPRNG.
pub(crate) fn random<const N: usize>() -> Result<[u8; N], RandomUnavailable> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).map_err(|_| RandomUnavailable)?;
    Ok(bytes)
}

/// Lowercase hex.
pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0xf)]));
    }
    out
}

/// Decodes lowercase hex; `None` for odd length or other characters.
pub(crate) fn unhex(text: &str) -> Option<Vec<u8>> {
    fn nibble(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            _ => None,
        }
    }
    let bytes = text.as_bytes();
    let (pairs, rest) = bytes.as_chunks::<2>();
    if !rest.is_empty() {
        return None;
    }
    pairs
        .iter()
        .map(|[hi, lo]| Some(nibble(*hi)? << 4 | nibble(*lo)?))
        .collect()
}

/// A fresh pairing code: 40 random bits as eight Crockford symbols.
pub(crate) fn pairing_code() -> Result<PairingCode, RandomUnavailable> {
    let bits = random::<5>()?
        .iter()
        .fold(0_u64, |acc, byte| acc << 8 | u64::from(*byte));
    let code: String = (0..8)
        .rev()
        .map(|i| {
            let index = usize::try_from((bits >> (i * 5)) & 0x1f).unwrap_or(0);
            char::from(CROCKFORD_ALPHABET[index])
        })
        .collect();
    // Eight symbols of the alphabet: always valid.
    PairingCode::new(code).map_err(|_| RandomUnavailable)
}

/// A fresh challenge.
pub(crate) fn challenge() -> Result<Challenge, RandomUnavailable> {
    Challenge::new(hex(&random::<{ protocol::auth::CHALLENGE_BYTES }>()?))
        .map_err(|_| RandomUnavailable)
}

/// What a session token is stored under: its SHA-256, so looking a token
/// up does not time a comparison against the secret itself.
pub(crate) type TokenDigest = [u8; 32];

/// The digest of `token`.
pub(crate) fn digest(token: &[u8]) -> TokenDigest {
    Sha256::digest(token).into()
}

/// Constant-time equality for equal-length secrets.
pub(crate) fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len()
        && std::hint::black_box(a.iter().zip(b).fold(0, |acc, (x, y)| acc | (x ^ y))) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() {
        let bytes = [0x00, 0x7f, 0xa5, 0xff];
        assert_eq!(hex(&bytes), "007fa5ff");
        assert_eq!(unhex("007fa5ff").expect("hex"), bytes);
        assert_eq!(unhex("0"), None);
        assert_eq!(unhex("0G"), None);
        assert_eq!(unhex("AB"), None);
    }

    #[test]
    fn random_values_are_well_formed_and_differ() {
        let a = pairing_code().expect("code");
        let b = pairing_code().expect("code");
        assert_ne!(a, b);
        assert!(challenge().is_ok());
        assert_ne!(random::<32>().expect("rng"), random::<32>().expect("rng"));
    }

    #[test]
    fn same_compares_exactly() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"ab"));
    }
}
