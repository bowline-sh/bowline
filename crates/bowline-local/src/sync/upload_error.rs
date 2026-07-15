use std::{error::Error, fmt};

use bowline_control_plane::{CompareAndSwapError, ControlPlaneError};
use bowline_core::ids::PackId;
use bowline_storage::{ByteStoreError, EnvelopeError, PackfileError};

use super::DownloadError;

#[derive(Debug)]
pub enum UploadError {
    ControlPlane(ControlPlaneError),
    ByteStore(ByteStoreError),
    Download(DownloadError),
    Packfile(PackfileError),
    Manifest(bowline_storage::ManifestError),
    MetadataPage(bowline_storage::MetadataPageError),
    Envelope(EnvelopeError),
    Namespace(bowline_core::namespace_snapshot::NamespaceReadError),
    CompareAndSwap(CompareAndSwapError),
    ReusedPackMissing {
        pack_id: PackId,
    },
    ClaimOwnershipLost,
    CancellationRequested,
    Checkpoint(String),
    RemoteCommitCheckpoint(String),
    Json(serde_json::Error),
    #[cfg(feature = "fault-injection")]
    Fault(crate::sync::fault::FaultError),
}

impl fmt::Display for UploadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ControlPlane(error) => error.fmt(formatter),
            Self::ByteStore(error) => error.fmt(formatter),
            Self::Download(error) => error.fmt(formatter),
            Self::Packfile(error) => error.fmt(formatter),
            Self::Manifest(error) => error.fmt(formatter),
            Self::MetadataPage(error) => error.fmt(formatter),
            Self::Envelope(error) => error.fmt(formatter),
            Self::Namespace(error) => error.fmt(formatter),
            Self::CompareAndSwap(error) => error.fmt(formatter),
            Self::ReusedPackMissing { pack_id } => {
                write!(
                    formatter,
                    "reused source pack `{}` is missing",
                    pack_id.as_str()
                )
            }
            Self::ClaimOwnershipLost => {
                formatter.write_str("sync operation claim ownership was lost")
            }
            Self::CancellationRequested => {
                formatter.write_str("sync operation cancellation was requested")
            }
            Self::Checkpoint(error) => write!(formatter, "sync checkpoint failed: {error}"),
            Self::RemoteCommitCheckpoint(error) => write!(
                formatter,
                "sync checkpoint failed after the remote ref committed: {error}"
            ),
            Self::Json(error) => write!(formatter, "upload JSON failed: {error}"),
            #[cfg(feature = "fault-injection")]
            Self::Fault(error) => error.fmt(formatter),
        }
    }
}

impl Error for UploadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ControlPlane(error) => Some(error),
            Self::ByteStore(error) => Some(error),
            Self::Download(error) => Some(error),
            Self::Packfile(error) => Some(error),
            Self::Manifest(error) => Some(error),
            Self::MetadataPage(error) => Some(error),
            Self::Envelope(error) => Some(error),
            Self::Namespace(error) => Some(error),
            Self::CompareAndSwap(error) => Some(error),
            Self::ReusedPackMissing { .. } => None,
            Self::ClaimOwnershipLost | Self::CancellationRequested => None,
            Self::Checkpoint(_) | Self::RemoteCommitCheckpoint(_) => None,
            Self::Json(error) => Some(error),
            #[cfg(feature = "fault-injection")]
            Self::Fault(error) => Some(error),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum UploadFailureSource<'a> {
    ControlPlane(&'a ControlPlaneError),
    ByteStore(&'a ByteStoreError),
    Download(&'a DownloadError),
    Retry,
}

impl UploadError {
    pub fn failure_source(&self) -> UploadFailureSource<'_> {
        match self {
            Self::ControlPlane(error) => UploadFailureSource::ControlPlane(error),
            Self::ByteStore(error) => UploadFailureSource::ByteStore(error),
            Self::Download(error) => UploadFailureSource::Download(error),
            Self::Packfile(_)
            | Self::Manifest(_)
            | Self::MetadataPage(_)
            | Self::Envelope(_)
            | Self::Namespace(_)
            | Self::CompareAndSwap(_)
            | Self::ReusedPackMissing { .. }
            | Self::ClaimOwnershipLost
            | Self::CancellationRequested
            | Self::Checkpoint(_)
            | Self::RemoteCommitCheckpoint(_)
            | Self::Json(_) => UploadFailureSource::Retry,
            #[cfg(feature = "fault-injection")]
            Self::Fault(_) => UploadFailureSource::Retry,
        }
    }
}

impl From<ControlPlaneError> for UploadError {
    fn from(error: ControlPlaneError) -> Self {
        Self::ControlPlane(error)
    }
}

impl From<ByteStoreError> for UploadError {
    fn from(error: ByteStoreError) -> Self {
        Self::ByteStore(error)
    }
}

impl From<DownloadError> for UploadError {
    fn from(error: DownloadError) -> Self {
        Self::Download(error)
    }
}

impl From<PackfileError> for UploadError {
    fn from(error: PackfileError) -> Self {
        Self::Packfile(error)
    }
}

impl From<bowline_storage::ManifestError> for UploadError {
    fn from(error: bowline_storage::ManifestError) -> Self {
        Self::Manifest(error)
    }
}

impl From<bowline_storage::MetadataPageError> for UploadError {
    fn from(error: bowline_storage::MetadataPageError) -> Self {
        Self::MetadataPage(error)
    }
}

impl From<bowline_core::namespace_snapshot::NamespaceBuildError> for UploadError {
    fn from(error: bowline_core::namespace_snapshot::NamespaceBuildError) -> Self {
        match error {
            bowline_core::namespace_snapshot::NamespaceBuildError::Read(error) => {
                Self::Namespace(error)
            }
        }
    }
}

impl From<EnvelopeError> for UploadError {
    fn from(error: EnvelopeError) -> Self {
        Self::Envelope(error)
    }
}

impl From<bowline_core::namespace_snapshot::NamespaceReadError> for UploadError {
    fn from(error: bowline_core::namespace_snapshot::NamespaceReadError) -> Self {
        Self::Namespace(error)
    }
}

impl From<serde_json::Error> for UploadError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

#[cfg(feature = "fault-injection")]
impl From<crate::sync::fault::FaultError> for UploadError {
    fn from(error: crate::sync::fault::FaultError) -> Self {
        Self::Fault(error)
    }
}
