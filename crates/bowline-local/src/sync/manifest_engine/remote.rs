use std::collections::BTreeMap;
use std::fmt;
use std::io::Write;
use std::path::Path;

use bowline_core::ids::ContentId;

use super::manifest::{BlobKey, KeyEpoch, ManifestKey};

pub struct BlobUpload<'a> {
    pub key: &'a BlobKey,
    pub content_id: &'a ContentId,
    pub key_epoch: KeyEpoch,
    pub sealed: &'a [u8],
}

pub struct BlobReaderUpload<'a> {
    pub key: &'a BlobKey,
    pub content_id: &'a ContentId,
    pub key_epoch: KeyEpoch,
    pub spool_path: &'a Path,
    pub byte_len: u64,
}

pub struct ManifestUpload<'a> {
    pub key: &'a ManifestKey,
    pub content_id: &'a ContentId,
    pub key_epoch: KeyEpoch,
    pub sealed: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestBatchUpload {
    pub key: ManifestKey,
    pub content_id: ContentId,
    pub key_epoch: KeyEpoch,
    pub sealed: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct BlobPrefetchRequest {
    pub key: BlobKey,
    pub byte_len: u64,
}

pub type PrefetchedBlobs = BTreeMap<BlobKey, Vec<u8>>;

pub trait RemoteObjects {
    fn put_blob(&self, upload: BlobUpload<'_>) -> Result<(), TransportError>;
    fn put_blob_reader(&self, upload: BlobReaderUpload<'_>) -> Result<(), TransportError>;
    fn put_manifest(&self, upload: ManifestUpload<'_>) -> Result<(), TransportError>;

    /// Block until every blob accepted by `put_blob` is durably stored.
    ///
    /// A transport may queue uploads and drain them in parallel, in which case
    /// `put_blob` returning `Ok` means accepted, not stored. The engine records
    /// sealed blobs in a durable ledger whose whole contract is that each row
    /// names a completed PUT -- a later push consults it and skips re-uploading.
    /// Recording before the bytes exist would let a drain failure strand them:
    /// the rows survive, the retry skips the upload, and the manifest published
    /// afterwards names a blob no peer can ever fetch. Content is
    /// end-to-end encrypted, so no server-side check can catch that; this
    /// ordering is the entire guarantee.
    ///
    /// The default is exact for transports that store synchronously.
    fn ensure_uploads_settled(&self) -> Result<(), TransportError> {
        Ok(())
    }
    fn put_manifests(&self, uploads: &[ManifestBatchUpload]) -> Result<(), TransportError> {
        for upload in uploads {
            self.put_manifest(ManifestUpload {
                key: &upload.key,
                content_id: &upload.content_id,
                key_epoch: upload.key_epoch,
                sealed: &upload.sealed,
            })?;
        }
        Ok(())
    }
    fn get_blob(&self, key: &BlobKey) -> Result<Vec<u8>, TransportError>;
    fn prefetch_blobs(
        &self,
        requests: &[BlobPrefetchRequest],
    ) -> Result<PrefetchedBlobs, TransportError> {
        let mut blobs = BTreeMap::new();
        for request in requests {
            if !blobs.contains_key(&request.key) {
                blobs.insert(request.key.clone(), self.get_blob(&request.key)?);
            }
        }
        Ok(blobs)
    }
    fn get_blob_to_writer(
        &self,
        key: &BlobKey,
        writer: &mut dyn Write,
    ) -> Result<u64, TransportError> {
        let sealed = self.get_blob(key)?;
        writer
            .write_all(&sealed)
            .map_err(|error| TransportError::new("get-blob-to-writer", error.to_string()))?;
        Ok(sealed.len() as u64)
    }
    fn get_manifest(&self, key: &ManifestKey) -> Result<Vec<u8>, TransportError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefObservation {
    pub version: u64,
    pub manifest_key: ManifestKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CasOutcome {
    Advanced(RefObservation),
    Lost(RefObservation),
    Ambiguous,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefVersionLookup {
    Found(ManifestKey),
    NotAdvanced,
    Unknown,
}

pub trait RemoteRef {
    fn read_ref(&self) -> Result<Option<RefObservation>, TransportError>;
    fn lookup_ref_version(&self, version: u64) -> Result<RefVersionLookup, TransportError> {
        let Some(current) = self.read_ref()? else {
            return Ok(RefVersionLookup::NotAdvanced);
        };
        if current.version == version {
            return Ok(RefVersionLookup::Found(current.manifest_key));
        }
        if current.version < version {
            return Ok(RefVersionLookup::NotAdvanced);
        }
        Ok(RefVersionLookup::Unknown)
    }
    fn compare_and_swap(
        &self,
        expected_version: Option<u64>,
        new_manifest_key: &ManifestKey,
    ) -> Result<CasOutcome, TransportError>;
}

#[derive(Debug)]
pub struct TransportError {
    pub operation: &'static str,
    pub detail: String,
    class: TransportFailureClass,
}

impl TransportError {
    pub fn new(operation: &'static str, detail: impl Into<String>) -> Self {
        Self {
            operation,
            detail: detail.into(),
            class: TransportFailureClass::Retryable,
        }
    }

    pub fn integrity(operation: &'static str, detail: impl Into<String>) -> Self {
        Self {
            operation,
            detail: detail.into(),
            class: TransportFailureClass::Integrity,
        }
    }

    pub const fn failure_class(&self) -> TransportFailureClass {
        self.class
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportFailureClass {
    Retryable,
    Integrity,
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "transport {}: {}", self.operation, self.detail)
    }
}

impl std::error::Error for TransportError {}
