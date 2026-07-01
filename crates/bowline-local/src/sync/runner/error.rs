use super::*;

#[derive(Debug)]
pub enum SyncRunnerError {
    Coalesce(CoalesceError),
    Upload(UploadError),
    Download(DownloadError),
    Cache(CacheError),
    Merge(MergeError),
    ConflictBundle(ConflictBundleError),
    WorkViewOverlay(WorkViewOverlaySyncError),
    ControlPlane(ControlPlaneError),
    Metadata(MetadataError),
    StateIo(io::Error),
    StateJson(serde_json::Error),
    UnsafeMaterializationPath(String),
    MissingPackedLocator(&'static str),
}

impl fmt::Display for SyncRunnerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Coalesce(error) => error.fmt(formatter),
            Self::Upload(error) => error.fmt(formatter),
            Self::Download(error) => error.fmt(formatter),
            Self::Cache(error) => error.fmt(formatter),
            Self::Merge(error) => error.fmt(formatter),
            Self::ConflictBundle(error) => error.fmt(formatter),
            Self::WorkViewOverlay(error) => error.fmt(formatter),
            Self::ControlPlane(error) => error.fmt(formatter),
            Self::Metadata(error) => error.fmt(formatter),
            Self::StateIo(error) => write!(formatter, "sync state I/O failed: {error}"),
            Self::StateJson(error) => write!(formatter, "sync state JSON failed: {error}"),
            Self::UnsafeMaterializationPath(path) => {
                write!(formatter, "unsafe materialization path: {path}")
            }
            Self::MissingPackedLocator(field) => {
                write!(formatter, "packed locator is missing {field}")
            }
        }
    }
}

impl Error for SyncRunnerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Coalesce(error) => Some(error),
            Self::Upload(error) => Some(error),
            Self::Download(error) => Some(error),
            Self::Cache(error) => Some(error),
            Self::Merge(error) => Some(error),
            Self::ConflictBundle(error) => Some(error),
            Self::WorkViewOverlay(error) => Some(error),
            Self::ControlPlane(error) => Some(error),
            Self::Metadata(error) => Some(error),
            Self::StateIo(error) => Some(error),
            Self::StateJson(error) => Some(error),
            Self::UnsafeMaterializationPath(_) => None,
            Self::MissingPackedLocator(_) => None,
        }
    }
}

impl From<CoalesceError> for SyncRunnerError {
    fn from(error: CoalesceError) -> Self {
        Self::Coalesce(error)
    }
}

impl From<UploadError> for SyncRunnerError {
    fn from(error: UploadError) -> Self {
        Self::Upload(error)
    }
}

impl From<DownloadError> for SyncRunnerError {
    fn from(error: DownloadError) -> Self {
        Self::Download(error)
    }
}

impl From<CacheError> for SyncRunnerError {
    fn from(error: CacheError) -> Self {
        Self::Cache(error)
    }
}

impl From<MergeError> for SyncRunnerError {
    fn from(error: MergeError) -> Self {
        Self::Merge(error)
    }
}

impl From<ConflictBundleError> for SyncRunnerError {
    fn from(error: ConflictBundleError) -> Self {
        Self::ConflictBundle(error)
    }
}

impl From<WorkViewOverlaySyncError> for SyncRunnerError {
    fn from(error: WorkViewOverlaySyncError) -> Self {
        Self::WorkViewOverlay(error)
    }
}

impl From<EnvImportError> for SyncRunnerError {
    fn from(error: EnvImportError) -> Self {
        Self::Metadata(MetadataError::InvalidStorageMetadata(error.to_string()))
    }
}

impl From<ControlPlaneError> for SyncRunnerError {
    fn from(error: ControlPlaneError) -> Self {
        Self::ControlPlane(error)
    }
}

impl From<bowline_storage::ByteStoreError> for SyncRunnerError {
    fn from(error: bowline_storage::ByteStoreError) -> Self {
        Self::Cache(CacheError::Store(error))
    }
}

impl From<MetadataError> for SyncRunnerError {
    fn from(error: MetadataError) -> Self {
        Self::Metadata(error)
    }
}

impl From<serde_json::Error> for SyncRunnerError {
    fn from(error: serde_json::Error) -> Self {
        Self::StateJson(error)
    }
}
