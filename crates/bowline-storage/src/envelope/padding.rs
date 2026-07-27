//! Length padding for sealed objects.
//!
//! Without padding a sealed object's length sits within compression noise of its
//! plaintext length, so a server holding the workspace's objects learns a size
//! histogram of the tree — enough to fingerprint well-known files and to watch a
//! specific file grow edit by edit. KBFS pads for exactly this reason.
//!
//! Every payload is rounded up to the next rung of a ~1.1x ladder, so the
//! observable length reveals only which of ~O(log n) buckets the object fell in.
//! Expected cost is ~5% of stored bytes, bounded at 10% plus the length prefix.
//!
//! Padding runs *inside* the AEAD: the true payload length is a prefix of the
//! authenticated plaintext, never of the cleartext header, or the padding would
//! announce the very number it exists to hide.

use std::mem::size_of;

/// Every payload is at least this long once padded, so sub-64-byte objects —
/// where a 1.1x ladder still resolves single bytes — all look alike.
const MIN_PADDED_LEN: u64 = 64;

/// Width of the little-endian payload-length prefix inside the padded plaintext.
const PAYLOAD_LEN_PREFIX: usize = size_of::<u64>();

/// Smallest ladder rung at or above `payload_len`.
///
/// Integer-only by construction. A float `1.1f64.powi(k)` ladder would make the
/// padded length depend on the platform's rounding, and a sealed object's length
/// must be identical on every device or convergent sealing stops converging.
pub(crate) fn padded_len(payload_len: u64) -> u64 {
    let mut rung = MIN_PADDED_LEN;
    while rung < payload_len {
        let step = (rung / 10).max(1);
        match rung.checked_add(step) {
            Some(next) => rung = next,
            None => return u64::MAX,
        }
    }
    rung
}

/// Frame `payload` as `len_le_u64 || payload || zeros` up to the next rung.
///
/// Returns `None` only when the framed length overflows `usize`, which on a
/// 64-bit target means the payload was already unrepresentable.
pub(crate) fn pad(payload: &[u8]) -> Option<Vec<u8>> {
    let framed_len = (payload.len() as u64).checked_add(PAYLOAD_LEN_PREFIX as u64)?;
    let target = usize::try_from(padded_len(framed_len)).ok()?;

    let mut padded = Vec::with_capacity(target);
    padded.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    padded.extend_from_slice(payload);
    padded.resize(target, 0);
    Some(padded)
}

/// Recover the payload from an authenticated padded plaintext.
///
/// Only reachable after the AEAD tag verifies, so a malformed frame here means a
/// producer bug (a device that padded differently), not an attacker — and that
/// bug must fail loudly rather than convergence silently breaking.
pub(crate) fn unpad(padded: &[u8]) -> Option<&[u8]> {
    let prefix = padded.get(..PAYLOAD_LEN_PREFIX)?;
    let mut length_bytes = [0_u8; PAYLOAD_LEN_PREFIX];
    length_bytes.copy_from_slice(prefix);
    let payload_len = usize::try_from(u64::from_le_bytes(length_bytes)).ok()?;

    let end = PAYLOAD_LEN_PREFIX.checked_add(payload_len)?;
    let payload = padded.get(PAYLOAD_LEN_PREFIX..end)?;
    if padded.len() != usize::try_from(padded_len(end as u64)).ok()? {
        return None;
    }
    if padded[end..].iter().any(|byte| *byte != 0) {
        return None;
    }
    Some(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ladder_is_monotone_and_never_shrinks_a_payload() {
        let mut previous = 0;
        for length in [0_u64, 1, 63, 64, 65, 100, 1_000, 100_000, 10_000_000] {
            let rung = padded_len(length);
            assert!(rung >= length, "rung {rung} < length {length}");
            assert!(rung >= previous, "ladder went backwards at {length}");
            previous = rung;
        }
    }

    #[test]
    fn ladder_overhead_stays_under_eleven_percent_above_the_floor() {
        let mut length = MIN_PADDED_LEN;
        while length < 64 * 1024 * 1024 {
            let rung = padded_len(length);
            assert!(
                rung * 100 <= length * 111,
                "padding {rung} exceeded 11% over {length}"
            );
            length = length + (length / 7) + 1;
        }
    }

    #[test]
    fn ladder_collapses_distinct_lengths_onto_shared_rungs() {
        // The whole point: neighbouring plaintext sizes must become
        // indistinguishable by sealed length.
        assert_eq!(padded_len(1_000), padded_len(1_010));
        assert_eq!(padded_len(0), padded_len(64));
    }

    #[test]
    fn pad_round_trips_and_lands_on_a_rung() {
        for length in [0_usize, 1, 55, 56, 57, 1_024, 65_537] {
            let payload = vec![0xab_u8; length];
            let padded = pad(&payload).expect("pad");
            assert_eq!(
                padded.len() as u64,
                padded_len((length + PAYLOAD_LEN_PREFIX) as u64)
            );
            assert_eq!(unpad(&padded).expect("unpad"), payload.as_slice());
        }
    }

    #[test]
    fn pad_is_deterministic() {
        assert_eq!(pad(b"body").expect("a"), pad(b"body").expect("b"));
    }

    #[test]
    fn unpad_rejects_a_frame_that_is_not_zero_filled_to_its_rung() {
        let mut padded = pad(b"body").expect("pad");
        let last = padded.last_mut().expect("padded frame is non-empty");
        *last ^= 1;

        assert!(unpad(&padded).is_none());
    }

    #[test]
    fn unpad_rejects_a_declared_length_past_the_frame() {
        let mut padded = pad(b"body").expect("pad");
        padded[..PAYLOAD_LEN_PREFIX].copy_from_slice(&u64::MAX.to_le_bytes());

        assert!(unpad(&padded).is_none());
    }

    #[test]
    fn unpad_rejects_a_truncated_frame() {
        assert!(unpad(b"short").is_none());
    }
}
