use std::{error::Error, fmt};

use bowline_control_plane::ControlPlaneError;
use bowline_core::namespace_snapshot::NamespaceReadError;
use bowline_storage::{ByteStoreError, MetadataPageError};

#[derive(Debug)]
pub enum DownloadError {
    ControlPlane(ControlPlaneError),
    ByteStore(ByteStoreError),
    Manifest(bowline_storage::ManifestError),
    MetadataPage(MetadataPageError),
    Namespace(NamespaceReadError),
    UnsafePath(String),
    UnsafeManifest(&'static str),
    MissingBinding(String),
    SnapshotManifestMissing(String),
    CancellationRequested,
}

impl fmt::Display for DownloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ControlPlane(error) => error.fmt(formatter),
            Self::ByteStore(error) => error.fmt(formatter),
            Self::Manifest(error) => error.fmt(formatter),
            Self::MetadataPage(error) => error.fmt(formatter),
            Self::Namespace(error) => error.fmt(formatter),
            Self::UnsafePath(path) => write!(formatter, "remote namespace path `{path}` is unsafe"),
            Self::UnsafeManifest(reason) => {
                write!(formatter, "remote snapshot root is unsafe: {reason}")
            }
            Self::MissingBinding(logical_id) => {
                write!(formatter, "metadata binding `{logical_id}` was not found")
            }
            Self::SnapshotManifestMissing(snapshot_id) => {
                write!(formatter, "snapshot root `{snapshot_id}` was not found")
            }
            Self::CancellationRequested => {
                formatter.write_str("snapshot import cancellation was requested")
            }
        }
    }
}

impl Error for DownloadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ControlPlane(error) => Some(error),
            Self::ByteStore(error) => Some(error),
            Self::Manifest(error) => Some(error),
            Self::MetadataPage(error) => Some(error),
            Self::Namespace(error) => Some(error),
            Self::UnsafePath(_)
            | Self::UnsafeManifest(_)
            | Self::MissingBinding(_)
            | Self::SnapshotManifestMissing(_)
            | Self::CancellationRequested => None,
        }
    }
}

impl From<ControlPlaneError> for DownloadError {
    fn from(error: ControlPlaneError) -> Self {
        Self::ControlPlane(error)
    }
}

impl From<ByteStoreError> for DownloadError {
    fn from(error: ByteStoreError) -> Self {
        Self::ByteStore(error)
    }
}

impl From<bowline_storage::ManifestError> for DownloadError {
    fn from(error: bowline_storage::ManifestError) -> Self {
        Self::Manifest(error)
    }
}

impl From<MetadataPageError> for DownloadError {
    fn from(error: MetadataPageError) -> Self {
        Self::MetadataPage(error)
    }
}

impl From<NamespaceReadError> for DownloadError {
    fn from(error: NamespaceReadError) -> Self {
        Self::Namespace(error)
    }
}
