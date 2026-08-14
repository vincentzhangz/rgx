//! 128-bit content fingerprints for change detection.
//!
//! The fingerprint is two independent FNV-1a-64 passes over the bytes with
//! distinct offset bases, combined into a `u128`. It is used to decide
//! whether a file's n-gram set can be reused during an incremental update.
//!
//! This is *not* a cryptographic hash: there is no adversarial requirement.
//! The failure mode of a collision is a stale index entry (a missed match),
//! and at 128 bits the accidental-collision probability is negligible for
//! any realistic file count.

/// First FNV-1a offset basis (standard).
const FNV_OFFSET: u64 = 0xcbf29ce484222325;
/// Second, independent offset basis.
const FNV_ALT_OFFSET: u64 = 0x9e3779b97f4a7c15;
/// FNV-1a prime.
const FNV_PRIME: u64 = 0x100000001b3;

/// Compute a 128-bit fingerprint of `bytes`.
pub fn content_hash(bytes: &[u8]) -> u128 {
    let mut a = FNV_OFFSET;
    let mut b = FNV_ALT_OFFSET;
    for &byte in bytes {
        a ^= byte as u64;
        a = a.wrapping_mul(FNV_PRIME);
        b ^= byte as u64;
        b = b.wrapping_mul(FNV_PRIME);
    }
    ((a as u128) << 64) | (b as u128)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_content_same_hash() {
        assert_eq!(
            content_hash(b"the quick brown fox"),
            content_hash(b"the quick brown fox")
        );
    }

    #[test]
    fn differing_content_differs() {
        assert_ne!(
            content_hash(b"the quick brown fox"),
            content_hash(b"the quick brown fyox")
        );
    }

    #[test]
    fn empty_input_stable() {
        assert_eq!(content_hash(b""), content_hash(b""));
    }
}
