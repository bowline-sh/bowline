use std::io::{self, Read};

use super::*;

/// Immutable prepared content that can open a fresh reader for every attempt.
/// Implementations must not reopen mutable worktree paths.
pub trait ContentSourceReader: Send + Sync {
    /// Exact plaintext length the next reader will produce.
    fn logical_len(&self) -> u64;

    /// Opens a fresh reader positioned at the first plaintext byte.
    fn open(&self) -> Result<Box<dyn Read + Send + '_>, PackfileError>;

    /// Opens exactly `length` plaintext bytes starting at `offset`.
    fn open_range(
        &self,
        offset: u64,
        length: u64,
    ) -> Result<Box<dyn Read + Send + '_>, PackfileError>;
}

/// One logical current-format record backed by a reopenable content source.
#[derive(Clone, Copy)]
pub struct PackRecordReader<'a> {
    pub content_id: &'a ContentId,
    pub source: &'a dyn ContentSourceReader,
}

pub(super) struct SliceContentSource<'a> {
    pub(super) bytes: &'a [u8],
}

impl ContentSourceReader for SliceContentSource<'_> {
    fn logical_len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn open(&self) -> Result<Box<dyn Read + Send + '_>, PackfileError> {
        Ok(Box::new(io::Cursor::new(self.bytes)))
    }

    fn open_range(
        &self,
        offset: u64,
        length: u64,
    ) -> Result<Box<dyn Read + Send + '_>, PackfileError> {
        let start = usize::try_from(offset).map_err(|_| PackfileError::InvalidRecordRange)?;
        let length = usize::try_from(length).map_err(|_| PackfileError::InvalidRecordRange)?;
        let end = start
            .checked_add(length)
            .filter(|end| *end <= self.bytes.len())
            .ok_or(PackfileError::InvalidRecordRange)?;
        Ok(Box::new(io::Cursor::new(&self.bytes[start..end])))
    }
}

pub(super) fn read_source_record(
    source: &dyn ContentSourceReader,
) -> Result<Vec<u8>, PackfileError> {
    let logical_len = source.logical_len();
    let capacity = usize::try_from(logical_len).map_err(|_| PackfileError::PackTooLarge)?;
    let read_limit = logical_len
        .checked_add(1)
        .ok_or(PackfileError::PackTooLarge)?;
    let mut bytes = Vec::with_capacity(capacity);
    let mut reader = source.open()?.take(read_limit);
    reader.read_to_end(&mut bytes)?;
    let actual = bytes.len() as u64;
    if actual != logical_len {
        return Err(PackfileError::ContentSourceLengthMismatch {
            expected: logical_len,
            actual,
        });
    }
    Ok(bytes)
}
