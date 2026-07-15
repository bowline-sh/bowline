use super::ByteStoreError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub offset: u64,
    pub length: u64,
}

impl ByteRange {
    pub fn new(offset: u64, length: u64) -> Self {
        Self { offset, length }
    }

    pub(super) fn checked_end(self, byte_len: u64) -> Result<u64, ByteStoreError> {
        let end = self
            .offset
            .checked_add(self.length)
            .ok_or(ByteStoreError::RangeOutOfBounds {
                offset: self.offset,
                length: self.length,
                byte_len,
            })?;

        if end <= byte_len {
            Ok(end)
        } else {
            Err(ByteStoreError::RangeOutOfBounds {
                offset: self.offset,
                length: self.length,
                byte_len,
            })
        }
    }
}
