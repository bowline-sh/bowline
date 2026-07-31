//! Streaming identity and envelope boundary for segmented workspace files.

use std::io::{self, Read, Write};

use bowline_core::ids::ContentId;
use bowline_storage::{
    EnvelopeError, SegmentedOpenStats, SegmentedSealStats, is_segmented_envelope, open_segmented,
    seal_segmented,
};

use super::{
    ENVELOPE_FORMAT_VERSION, FILE_CONTENT_DOMAIN, KeyEpoch, ManifestError, WorkspaceCrypto,
};

pub fn is_segmented_file_envelope(prefix: &[u8]) -> bool {
    is_segmented_envelope(prefix)
}

pub fn seal_file_segmented(
    crypto: &WorkspaceCrypto,
    expected_content_id: &ContentId,
    reader: &mut dyn Read,
    writer: &mut dyn Write,
) -> Result<SegmentedSealStats, ManifestError> {
    let context = crypto.file_context(
        expected_content_id,
        crypto.key_epoch(),
        ENVELOPE_FORMAT_VERSION,
    );
    let key_bytes = crypto
        .key_bytes(crypto.key_epoch())
        .expect("write epoch key must exist in workspace crypto keyring");
    let mut hashing_reader = ContentHashingReader::new(reader, key_bytes);
    let stats = seal_segmented(&mut hashing_reader, writer, crypto.storage_key(), &context)
        .map_err(ManifestError::Envelope)?;
    if hashing_reader.content_id() != *expected_content_id {
        return Err(ManifestError::ContentIdMismatch);
    }
    Ok(stats)
}

pub fn open_file_segmented(
    crypto: &WorkspaceCrypto,
    key_epoch: KeyEpoch,
    expected_content_id: &ContentId,
    reader: &mut dyn Read,
    writer: &mut dyn Write,
    expected_size: u64,
    max_decoded_bytes: u64,
) -> Result<SegmentedOpenStats, ManifestError> {
    if expected_size > max_decoded_bytes {
        return Err(ManifestError::BoundExceeded {
            bound: "file-decoded-size-policy",
        });
    }
    let Some(storage_key) = crypto.storage_key_at(key_epoch) else {
        return Err(ManifestError::UnknownKeyEpoch { key_epoch });
    };
    let key_bytes = crypto
        .key_bytes(key_epoch)
        .ok_or(ManifestError::UnknownKeyEpoch { key_epoch })?;
    let context = crypto.file_context(expected_content_id, key_epoch, ENVELOPE_FORMAT_VERSION);
    let mut hashing_writer = ContentHashingWriter::new(writer, key_bytes);
    let stats = match open_segmented(
        reader,
        &mut hashing_writer,
        storage_key,
        &context,
        expected_size,
    ) {
        Err(EnvelopeError::DecodedSizeExceeded { .. }) => {
            return Err(ManifestError::DecodedSizeMismatch {
                expected: expected_size,
                actual: None,
            });
        }
        result => result.map_err(ManifestError::Envelope)?,
    };
    if stats.plaintext_bytes != expected_size {
        return Err(ManifestError::DecodedSizeMismatch {
            expected: expected_size,
            actual: Some(stats.plaintext_bytes),
        });
    }
    if hashing_writer.content_id() != *expected_content_id {
        return Err(ManifestError::ContentIdMismatch);
    }
    Ok(stats)
}

impl WorkspaceCrypto {
    pub fn content_id_reader(&self, reader: &mut dyn Read) -> io::Result<(ContentId, u64)> {
        self.content_id_reader_at(self.write_epoch, reader)?
            .ok_or_else(|| io::Error::other("write epoch key is missing"))
    }

    /// Hash a stream under a specific historical key epoch. A missing epoch is
    /// an ordinary unverifiable answer for recovery/proof callers, not a reason
    /// to hash under the current key and compare incomparable identifiers.
    pub fn content_id_reader_at(
        &self,
        key_epoch: KeyEpoch,
        reader: &mut dyn Read,
    ) -> io::Result<Option<(ContentId, u64)>> {
        let key_bytes = self.key_bytes(key_epoch);
        let Some(key_bytes) = key_bytes else {
            return Ok(None);
        };
        let mut hasher = file_content_hasher(key_bytes);
        let byte_len = io::copy(reader, &mut hasher)?;
        Ok(Some((finish_file_content_id(hasher), byte_len)))
    }
}

fn file_content_hasher(workspace_key: [u8; 32]) -> blake3::Hasher {
    let mut hasher = blake3::Hasher::new_keyed(&workspace_key);
    hasher.update(&(FILE_CONTENT_DOMAIN.len() as u64).to_le_bytes());
    hasher.update(FILE_CONTENT_DOMAIN);
    hasher
}

fn finish_file_content_id(hasher: blake3::Hasher) -> ContentId {
    ContentId::new(format!("cid_{}", hasher.finalize().to_hex()))
}

struct ContentHashingReader<'a> {
    inner: &'a mut dyn Read,
    hasher: blake3::Hasher,
}

impl<'a> ContentHashingReader<'a> {
    fn new(inner: &'a mut dyn Read, workspace_key: [u8; 32]) -> Self {
        Self {
            inner,
            hasher: file_content_hasher(workspace_key),
        }
    }

    fn content_id(&self) -> ContentId {
        finish_file_content_id(self.hasher.clone())
    }
}

impl Read for ContentHashingReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.hasher.update(&buffer[..read]);
        Ok(read)
    }
}

struct ContentHashingWriter<'a> {
    inner: &'a mut dyn Write,
    hasher: blake3::Hasher,
}

impl<'a> ContentHashingWriter<'a> {
    fn new(inner: &'a mut dyn Write, workspace_key: [u8; 32]) -> Self {
        Self {
            inner,
            hasher: file_content_hasher(workspace_key),
        }
    }

    fn content_id(&self) -> ContentId {
        finish_file_content_id(self.hasher.clone())
    }
}

impl Write for ContentHashingWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buffer)?;
        self.hasher.update(&buffer[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
