//! Deterministic envelope nonce derivation.
//!
//! Sealing is convergent: the nonce is a pseudorandom function of the workspace
//! key, the associated data, and the plaintext, so identical plaintext sealed
//! under identical context always produces byte-identical sealed output. That is
//! what makes `blake3(sealed)` a real content address — a rename, an A→B→A edit,
//! a branch round trip, and a second device pushing bytes the first already
//! uploaded all collapse onto one stored object instead of re-uploading.
//!
//! Security trade, recorded deliberately: this is single-tenant convergent
//! encryption. The server learns which objects *within one workspace* are
//! byte-identical, and nothing else — it holds no workspace key, and it already
//! sees each object's (padded) size. Cross-workspace equality stays invisible
//! because the key is per workspace. See `docs/trust-contract.md` for the full
//! "what the server can see" table.
//!
//! Nonce reuse safety: XChaCha20-Poly1305 is catastrophically broken by reusing
//! one nonce across *different* plaintexts under the same key. This derivation
//! makes that impossible by construction rather than by convention — the nonce
//! is derived from the plaintext itself, so equal nonces imply equal plaintext
//! and equal associated data, which is exactly the case where reuse is harmless
//! (the ciphertexts are also equal). No caller can pass a mismatched identifier
//! and silently destroy the cipher.

use crate::canonical_framing::CanonicalFrame;

/// Domain separator for the nonce PRF. These bytes are a permanent contract:
/// changing them reseals every object under a new key, orphaning everything
/// already stored.
const NONCE_DOMAIN: &str = "bowline/blob-nonce/v1";

/// BLAKE3 in keyed mode is the PRF here rather than HMAC-SHA256/HKDF: it is
/// already a workspace dependency, keyed BLAKE3 is a PRF with the same
/// extract-then-expand security argument, and one primitive fewer is one fewer
/// thing to get wrong. The 32-byte PRF output is truncated to the 24-byte
/// XChaCha nonce, which is the standard HKDF-Expand truncation.
pub(crate) fn derive_nonce<const NONCE_LEN: usize>(
    key: &[u8; 32],
    associated_data: &[u8],
    plaintext: &[u8],
) -> [u8; NONCE_LEN] {
    let plaintext_digest = blake3::hash(plaintext);
    let material = CanonicalFrame::new(NONCE_DOMAIN)
        .bytes_field("associated-data", associated_data)
        .bytes_field("plaintext-digest", plaintext_digest.as_bytes())
        .into_bytes();
    let expanded = blake3::keyed_hash(key, &material);

    let mut nonce = [0_u8; NONCE_LEN];
    nonce.copy_from_slice(&expanded.as_bytes()[..NONCE_LEN]);
    nonce
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [7; 32];
    const OTHER_KEY: [u8; 32] = [9; 32];

    fn nonce(key: &[u8; 32], aad: &[u8], plaintext: &[u8]) -> [u8; 24] {
        derive_nonce::<24>(key, aad, plaintext)
    }

    #[test]
    fn nonce_is_a_pure_function_of_key_context_and_plaintext() {
        assert_eq!(nonce(&KEY, b"aad", b"body"), nonce(&KEY, b"aad", b"body"));
    }

    #[test]
    fn nonce_changes_with_key_context_or_plaintext() {
        let baseline = nonce(&KEY, b"aad", b"body");

        assert_ne!(baseline, nonce(&OTHER_KEY, b"aad", b"body"));
        assert_ne!(baseline, nonce(&KEY, b"aae", b"body"));
        assert_ne!(baseline, nonce(&KEY, b"aad", b"bodz"));
    }

    /// The framing is length-prefixed, so moving a byte across the
    /// associated-data/plaintext boundary must not collide.
    #[test]
    fn nonce_cannot_be_confused_across_the_field_boundary() {
        assert_ne!(nonce(&KEY, b"ab", b"c"), nonce(&KEY, b"a", b"bc"));
    }
}
