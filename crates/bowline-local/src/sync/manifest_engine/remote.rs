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

pub trait RemoteObjects {
    fn put_blob(&self, upload: BlobUpload<'_>) -> Result<(), TransportError>;
    fn put_blob_reader(&self, upload: BlobReaderUpload<'_>) -> Result<(), TransportError>;
    fn put_manifest(&self, upload: ManifestUpload<'_>) -> Result<(), TransportError>;
    fn get_blob(&self, key: &BlobKey) -> Result<Vec<u8>, TransportError>;
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
}

impl TransportError {
    pub fn new(operation: &'static str, detail: impl Into<String>) -> Self {
        Self {
            operation,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "transport {}: {}", self.operation, self.detail)
    }
}

impl std::error::Error for TransportError {}
