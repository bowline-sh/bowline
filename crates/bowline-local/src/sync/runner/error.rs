use super::*;

#[derive(Debug)]
pub enum SyncRunnerError {
    Coalesce(CoalesceError),
    Upload(UploadError),
    Download(DownloadError),
    Cache(CacheError),
    InvalidImportedSnapshot(bowline_storage::HydrationPlanError),
    Merge(MergeError),
    MergePluginConfig(MergePluginConfigError),
    ConflictBundle(ConflictBundleError),
    WorkViewOverlay(WorkViewOverlaySyncError),
    ControlPlane(ControlPlaneError),
    Metadata(MetadataError),
    NamespaceRead(bowline_core::namespace_snapshot::NamespaceReadError),
    StateIo(io::Error),
    StateJson(serde_json::Error),
    UnsafeMaterializationPath(String),
    MaterializationBlockedByDirectory(String),
    MissingMaterializationContent(String),
    MaterializationTaskFenceLost(String),
    MaterializationRetryPending,
    ImportedContentIdMismatch {
        path: String,
        expected: ContentId,
        actual: ContentId,
    },
    SupersededMaterializationSnapshot(String),
    SyncClaimOwnershipLost,
    SyncOperationCancellationRequested,
    MissingPackedLocator(&'static str),
    #[cfg(feature = "fault-injection")]
    Fault(crate::sync::fault::FaultError),
    StatCacheDivergence {
        path: String,
        cached_content_id: ContentId,
        observed_content_id: ContentId,
    },
}

impl fmt::Display for SyncRunnerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Coalesce(error) => error.fmt(formatter),
            Self::Upload(error) => error.fmt(formatter),
            Self::Download(error) => error.fmt(formatter),
            Self::Cache(error) => error.fmt(formatter),
            Self::InvalidImportedSnapshot(error) => error.fmt(formatter),
            Self::Merge(error) => error.fmt(formatter),
            Self::MergePluginConfig(error) => error.fmt(formatter),
            Self::ConflictBundle(error) => error.fmt(formatter),
            Self::WorkViewOverlay(error) => error.fmt(formatter),
            Self::ControlPlane(error) => error.fmt(formatter),
            Self::Metadata(error) => error.fmt(formatter),
            Self::NamespaceRead(error) => error.fmt(formatter),
            Self::StateIo(error) => write!(formatter, "sync state I/O failed: {error}"),
            Self::StateJson(error) => write!(formatter, "sync state JSON failed: {error}"),
            Self::UnsafeMaterializationPath(path) => {
                write!(formatter, "unsafe materialization path: {path}")
            }
            Self::MaterializationBlockedByDirectory(path) => write!(
                formatter,
                "cannot replace non-empty directory {path} with a file; move or remove its contents (or mark the path local-only) and sync again"
            ),
            Self::MissingMaterializationContent(path) => write!(
                formatter,
                "required materialization content is unavailable for `{path}`"
            ),
            Self::MaterializationTaskFenceLost(path) => write!(
                formatter,
                "materialization authority for `{path}` is no longer current"
            ),
            Self::MaterializationRetryPending => {
                formatter.write_str("materialization retry backoff has not elapsed")
            }
            Self::ImportedContentIdMismatch {
                path,
                expected,
                actual,
            } => write!(
                formatter,
                "imported content for `{path}` hashed as {} instead of {}",
                actual.as_str(),
                expected.as_str()
            ),
            Self::SupersededMaterializationSnapshot(snapshot_id) => write!(
                formatter,
                "materialization snapshot `{snapshot_id}` was superseded before commit"
            ),
            Self::SyncClaimOwnershipLost => {
                write!(formatter, "sync operation claim ownership was lost")
            }
            Self::SyncOperationCancellationRequested => {
                formatter.write_str("sync operation cancellation was requested")
            }
            Self::MissingPackedLocator(field) => {
                write!(formatter, "packed locator is missing {field}")
            }
            #[cfg(feature = "fault-injection")]
            Self::Fault(error) => error.fmt(formatter),
            Self::StatCacheDivergence {
                path,
                cached_content_id,
                observed_content_id,
            } => write!(
                formatter,
                "stat cache divergence for `{path}`: cached {} but observed {}",
                cached_content_id.as_str(),
                observed_content_id.as_str()
            ),
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
            Self::InvalidImportedSnapshot(error) => Some(error),
            Self::Merge(error) => Some(error),
            Self::MergePluginConfig(error) => Some(error),
            Self::ConflictBundle(error) => Some(error),
            Self::WorkViewOverlay(error) => Some(error),
            Self::ControlPlane(error) => Some(error),
            Self::Metadata(error) => Some(error),
            Self::NamespaceRead(error) => Some(error),
            Self::StateIo(error) => Some(error),
            Self::StateJson(error) => Some(error),
            Self::UnsafeMaterializationPath(_) => None,
            Self::MaterializationBlockedByDirectory(_) => None,
            Self::MissingMaterializationContent(_) => None,
            Self::MaterializationTaskFenceLost(_) => None,
            Self::MaterializationRetryPending => None,
            Self::ImportedContentIdMismatch { .. } => None,
            Self::SupersededMaterializationSnapshot(_) => None,
            Self::SyncClaimOwnershipLost => None,
            Self::SyncOperationCancellationRequested => None,
            Self::MissingPackedLocator(_) => None,
            #[cfg(feature = "fault-injection")]
            Self::Fault(error) => Some(error),
            Self::StatCacheDivergence { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum SyncRunnerFailureSource<'a> {
    Upload(&'a UploadError),
    Download(&'a DownloadError),
    Cache(&'a CacheError),
    WorkViewOverlay(&'a WorkViewOverlaySyncError),
    ControlPlane(&'a ControlPlaneError),
    InvalidImportedSnapshot,
    Retry,
}

impl SyncRunnerError {
    /// Path-free classification for externally-visible surfaces (daemon status
    /// JSON, hosted-readable telemetry). The `Display` impl above intentionally
    /// carries paths for local diagnostics; this code must never do so, so
    /// callers building external payloads use this instead of `to_string()`.
    pub fn external_failure_code(&self) -> SyncExternalFailureCode {
        match self {
            Self::StatCacheDivergence { .. } => SyncExternalFailureCode::StatCacheDivergence,
            Self::MaterializationBlockedByDirectory(_)
            | Self::MissingMaterializationContent(_)
            | Self::MaterializationTaskFenceLost(_)
            | Self::MaterializationRetryPending
            | Self::ImportedContentIdMismatch { .. }
            | Self::SupersededMaterializationSnapshot(_)
            | Self::UnsafeMaterializationPath(_) => SyncExternalFailureCode::MaterializationBlocked,
            Self::Coalesce(_) => SyncExternalFailureCode::ScannerReadFailed,
            Self::Upload(_)
            | Self::Download(_)
            | Self::Cache(_)
            | Self::WorkViewOverlay(_)
            | Self::ControlPlane(_) => SyncExternalFailureCode::ControlPlaneUnavailable,
            Self::InvalidImportedSnapshot(_) => SyncExternalFailureCode::ImportedSnapshotInvalid,
            Self::Merge(_)
            | Self::MergePluginConfig(_)
            | Self::ConflictBundle(_)
            | Self::Metadata(_)
            | Self::NamespaceRead(_)
            | Self::StateIo(_)
            | Self::StateJson(_)
            | Self::SyncClaimOwnershipLost
            | Self::SyncOperationCancellationRequested
            | Self::MissingPackedLocator(_) => SyncExternalFailureCode::Unknown,
            #[cfg(feature = "fault-injection")]
            Self::Fault(_) => SyncExternalFailureCode::Unknown,
        }
    }

    pub fn failure_source(&self) -> SyncRunnerFailureSource<'_> {
        match self {
            Self::Upload(error) => SyncRunnerFailureSource::Upload(error),
            Self::Download(error) => SyncRunnerFailureSource::Download(error),
            Self::Cache(error) => SyncRunnerFailureSource::Cache(error),
            Self::WorkViewOverlay(error) => SyncRunnerFailureSource::WorkViewOverlay(error),
            Self::ControlPlane(error) => SyncRunnerFailureSource::ControlPlane(error),
            Self::InvalidImportedSnapshot(_) => SyncRunnerFailureSource::InvalidImportedSnapshot,
            Self::Coalesce(_)
            | Self::Merge(_)
            | Self::MergePluginConfig(_)
            | Self::ConflictBundle(_)
            | Self::Metadata(_)
            | Self::NamespaceRead(_)
            | Self::StateIo(_)
            | Self::StateJson(_)
            | Self::UnsafeMaterializationPath(_)
            | Self::MaterializationBlockedByDirectory(_)
            | Self::MissingMaterializationContent(_)
            | Self::MaterializationTaskFenceLost(_)
            | Self::MaterializationRetryPending
            | Self::ImportedContentIdMismatch { .. }
            | Self::SupersededMaterializationSnapshot(_)
            | Self::SyncClaimOwnershipLost
            | Self::SyncOperationCancellationRequested
            | Self::MissingPackedLocator(_)
            | Self::StatCacheDivergence { .. } => SyncRunnerFailureSource::Retry,
            #[cfg(feature = "fault-injection")]
            Self::Fault(_) => SyncRunnerFailureSource::Retry,
        }
    }

    pub fn is_cancellation_requested(&self) -> bool {
        matches!(self, Self::SyncOperationCancellationRequested)
    }
}

impl From<CoalesceError> for SyncRunnerError {
    fn from(error: CoalesceError) -> Self {
        Self::Coalesce(error)
    }
}

impl From<UploadError> for SyncRunnerError {
    fn from(error: UploadError) -> Self {
        match error {
            UploadError::ClaimOwnershipLost => Self::SyncClaimOwnershipLost,
            UploadError::CancellationRequested => Self::SyncOperationCancellationRequested,
            error => Self::Upload(error),
        }
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

impl From<bowline_core::namespace_snapshot::NamespaceReadError> for SyncRunnerError {
    fn from(error: bowline_core::namespace_snapshot::NamespaceReadError) -> Self {
        Self::NamespaceRead(error)
    }
}

impl From<bowline_storage::HydrationPlanError> for SyncRunnerError {
    fn from(error: bowline_storage::HydrationPlanError) -> Self {
        Self::InvalidImportedSnapshot(error)
    }
}

impl From<MergeError> for SyncRunnerError {
    fn from(error: MergeError) -> Self {
        Self::Merge(error)
    }
}

impl From<MergePluginConfigError> for SyncRunnerError {
    fn from(error: MergePluginConfigError) -> Self {
        Self::MergePluginConfig(error)
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

#[cfg(feature = "fault-injection")]
impl From<crate::sync::fault::FaultError> for SyncRunnerError {
    fn from(error: crate::sync::fault::FaultError) -> Self {
        Self::Fault(error)
    }
}
