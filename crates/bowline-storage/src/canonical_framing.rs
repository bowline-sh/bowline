//! Canonical, domain-separated, length-prefixed byte framing.
//!
//! Every byte string a cryptographic primitive commits to is built here: AEAD
//! associated data, and the digests that name recovery-quarantine locators.
//!
//! Framing bytes are a permanent contract. A sealed object can only ever be
//! opened by reproducing its associated data byte for byte, so that encoding
//! must not depend on a Rust type's field order, its `#[serde]` attributes, or
//! a serializer's escaping and number-formatting rules. Deriving the bytes from
//! `serde_json::to_vec` makes every future edit to the struct a silent,
//! unrecoverable break; writing the frame out by hand makes such an edit a
//! visible one, caught by the pinned-byte tests next to each producer.

/// First byte of every frame. Bumped only when the framing layout itself
/// changes, so a future layout can be told apart from this one. It is not a
/// scheme selector on its own: the cleartext envelope header carries the
/// version that would choose between layouts.
const FRAMING_VERSION: u8 = 1;

/// Builder for one canonical frame.
///
/// Layout: `FRAMING_VERSION || segment(domain) || segment(label) segment(value) ...`
/// where `segment(x) = x.len() as u64 big-endian || x`. Every segment is
/// length-prefixed, so no concatenation of labels and values can be confused
/// with a different one.
pub(crate) struct CanonicalFrame {
    bytes: Vec<u8>,
}

impl CanonicalFrame {
    pub(crate) fn new(domain: &str) -> Self {
        let mut frame = Self {
            bytes: vec![FRAMING_VERSION],
        };
        frame.push_segment(domain.as_bytes());
        frame
    }

    pub(crate) fn bytes_field(mut self, label: &str, value: &[u8]) -> Self {
        self.push_segment(label.as_bytes());
        self.push_segment(value);
        self
    }

    pub(crate) fn str_field(self, label: &str, value: &str) -> Self {
        self.bytes_field(label, value.as_bytes())
    }

    pub(crate) fn u16_field(self, label: &str, value: u16) -> Self {
        self.bytes_field(label, &value.to_be_bytes())
    }

    pub(crate) fn u32_field(self, label: &str, value: u32) -> Self {
        self.bytes_field(label, &value.to_be_bytes())
    }

    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    pub(crate) fn digest_hex(self) -> String {
        blake3::hash(&self.bytes).to_hex().to_string()
    }

    fn push_segment(&mut self, segment: &[u8]) {
        self.bytes
            .extend_from_slice(&(segment.len() as u64).to_be_bytes());
        self.bytes.extend_from_slice(segment);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_starts_with_version_then_length_prefixed_domain() {
        let frame = CanonicalFrame::new("d").into_bytes();

        assert_eq!(frame, vec![1, 0, 0, 0, 0, 0, 0, 0, 1, b'd']);
    }

    #[test]
    fn frame_length_prefixes_every_label_and_value() {
        let frame = CanonicalFrame::new("d")
            .str_field("ab", "c")
            .u32_field("e", 1)
            .into_bytes();

        assert_eq!(
            frame,
            vec![
                1, // framing version
                0, 0, 0, 0, 0, 0, 0, 1, b'd', // domain
                0, 0, 0, 0, 0, 0, 0, 2, b'a', b'b', // label "ab"
                0, 0, 0, 0, 0, 0, 0, 1, b'c', // value "c"
                0, 0, 0, 0, 0, 0, 0, 1, b'e', // label "e"
                0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 1, // value 1u32 big-endian
            ]
        );
    }

    #[test]
    fn frame_cannot_be_confused_by_moving_bytes_across_a_field_boundary() {
        let split = CanonicalFrame::new("d")
            .str_field("a", "bc")
            .str_field("de", "f")
            .into_bytes();
        let shifted = CanonicalFrame::new("d")
            .str_field("a", "b")
            .str_field("cde", "f")
            .into_bytes();

        assert_ne!(split, shifted);
    }

    #[test]
    fn frame_domain_separates_identical_fields() {
        let first = CanonicalFrame::new("domain-a")
            .str_field("k", "v")
            .into_bytes();
        let second = CanonicalFrame::new("domain-b")
            .str_field("k", "v")
            .into_bytes();

        assert_ne!(first, second);
    }
}
