use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use bowline_control_plane::ControlPlaneError;
use bowline_local::sync::manifest_engine::TransportError;
use bowline_storage::{
    ByteStoreError, ObjectKey, ReopenableObjectSource, stable_object_hash_reader,
};

/// Reopens a sealed on-disk spool for each streamed upload attempt.
pub(super) struct SpoolSource {
    pub(super) path: PathBuf,
}

impl ReopenableObjectSource for SpoolSource {
    fn open(&self) -> std::io::Result<Box<dyn Read + Send>> {
        Ok(Box::new(File::open(&self.path)?))
    }
}

pub(super) fn hash_spool(path: &Path) -> Result<String, TransportError> {
    let mut file = File::open(path)
        .map_err(|error| TransportError::new("put-blob-reader", error.to_string()))?;
    stable_object_hash_reader(&mut file)
        .map_err(|error| TransportError::new("put-blob-reader", error.to_string()))
}

pub(super) fn parse_object_key(key: &str) -> Result<ObjectKey, TransportError> {
    ObjectKey::new(key).map_err(|error| byte_store_error("object-key", error))
}

pub(super) fn byte_store_error(operation: &'static str, error: ByteStoreError) -> TransportError {
    TransportError::new(operation, error.to_string())
}

pub(super) fn control_plane_error(
    operation: &'static str,
    error: ControlPlaneError,
) -> TransportError {
    TransportError::new(operation, error.to_string())
}

pub(super) fn committed_metadata_error(field: &'static str) -> TransportError {
    TransportError::new(
        "commit-metadata",
        format!("committed object metadata response failed validation: {field}"),
    )
}
