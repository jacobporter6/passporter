//! Idempotency keys without `crypto.randomUUID`.
//!
//! The MVP is served over plain HTTP inside `WireGuard`, which is not a secure
//! context, so `crypto.randomUUID()` is missing. `crypto.getRandomValues()`
//! is available in every context; [`uuid_v4`] turns 16 of its bytes into
//! the canonical lowercase v4 UUID that [`IdempotencyKey`] requires.

use core::fmt::Write as _;

use protocol::IdempotencyKey;

/// Formats 16 random bytes as a lowercase RFC 9562 version-4 UUID
/// (`xxxxxxxx-xxxx-4xxx-Nxxx-xxxxxxxxxxxx`, `N` in `8..=b`).
#[must_use]
pub fn uuid_v4(mut bytes: [u8; 16]) -> IdempotencyKey {
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut out = String::with_capacity(36);
    for (i, byte) in bytes.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        let _ = write!(out, "{byte:02x}");
    }
    // Always 36 lowercase hex-and-hyphen characters in 8-4-4-4-12 shape.
    IdempotencyKey::new(out).unwrap_or_else(|_| unreachable!("uuid_v4 output is canonical"))
}

/// A fresh random idempotency key, or `None` if the browser has no
/// `crypto.getRandomValues` (it always does in supported browsers).
#[must_use]
pub fn random_key() -> Option<IdempotencyKey> {
    let crypto = web_sys::window()?.crypto().ok()?;
    let mut bytes = [0_u8; 16];
    crypto.get_random_values_with_u8_array(&mut bytes).ok()?;
    Some(uuid_v4(bytes))
}

/// A key derived from `from`, the same wherever it is computed: when two
/// tabs requeue the same entry under a new key, they must pick the same
/// one, so the bridge still dedupes. Two FNV-1a hashes of the key make
/// the 16 bytes; not random, but as unlikely to collide with a random v4
/// key as two random ones are.
#[must_use]
pub fn derived_key(from: &IdempotencyKey) -> IdempotencyKey {
    let fnv = |seed: u64| {
        from.as_str().bytes().fold(seed, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        })
    };
    let mut bytes = [0_u8; 16];
    bytes[..8].copy_from_slice(&fnv(0xcbf2_9ce4_8422_2325).to_le_bytes());
    bytes[8..].copy_from_slice(&fnv(0x8422_2325_cbf2_9ce4).to_le_bytes());
    uuid_v4(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_keys_are_stable_and_distinct() {
        let a = IdempotencyKey::new("9ca09978-ef89-46fd-9a85-118b84bee7b4").unwrap();
        let b = IdempotencyKey::new("0ca09978-ef89-46fd-9a85-118b84bee7b4").unwrap();
        assert_eq!(derived_key(&a), derived_key(&a));
        assert_ne!(derived_key(&a), a);
        assert_ne!(derived_key(&a), derived_key(&b));
        // Deriving again (a rebased prompt rebased once more) moves on.
        assert_ne!(derived_key(&derived_key(&a)), derived_key(&a));
        assert_ne!(derived_key(&derived_key(&a)), a);
    }

    #[test]
    fn formats_a_canonical_v4() {
        let key = uuid_v4([0xff; 16]);
        assert_eq!(key.to_string(), "ffffffff-ffff-4fff-bfff-ffffffffffff");
        let key = uuid_v4([0; 16]);
        assert_eq!(key.to_string(), "00000000-0000-4000-8000-000000000000");
    }

    #[test]
    fn keeps_the_random_bits() {
        let bytes = [
            0x9c, 0xa0, 0x99, 0x78, 0xef, 0x89, 0x16, 0xfd, 0x1a, 0x85, 0x11, 0x8b, 0x84, 0xbe,
            0xe7, 0xb4,
        ];
        assert_eq!(
            uuid_v4(bytes).to_string(),
            "9ca09978-ef89-46fd-9a85-118b84bee7b4"
        );
    }

    #[test]
    fn every_byte_value_yields_a_valid_key() {
        for b in 0..=255_u8 {
            let key = uuid_v4([b; 16]).to_string();
            assert_eq!(key.len(), 36);
            assert_eq!(&key[14..15], "4");
            assert!(matches!(&key[19..20], "8" | "9" | "a" | "b"), "{key}");
        }
    }
}
