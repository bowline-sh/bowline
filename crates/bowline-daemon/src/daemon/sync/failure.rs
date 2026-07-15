use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::daemon) enum SyncFailureAction {
    Attention,
    Offline,
    Retry,
}

impl SyncFailureAction {
    pub(in crate::daemon) fn bounded_by_retry_budget(self, attempt_count: u32) -> Self {
        if self == Self::Retry && attempt_count >= super::MAX_SYNC_RETRY_ATTEMPTS {
            Self::Attention
        } else {
            self
        }
    }
}

#[derive(Debug)]
pub(in crate::daemon) enum SyncOnceError {
    InvalidOperationPayload(String),
    HostedConfigUnavailable,
    WorkspaceKeyMissing,
    WorkspaceKeyInvalid,
    CredentialsMissing,
    DeviceKeys(DeviceKeyError),
    Grant(grants::GrantError),
    ControlPlane(ControlPlaneError),
    Runner(SyncRunnerError),
}

impl SyncOnceError {
    pub(in crate::daemon) fn disposition(&self) -> SyncFailureAction {
        match self {
            Self::InvalidOperationPayload(_) => SyncFailureAction::Attention,
            Self::HostedConfigUnavailable
            | Self::WorkspaceKeyMissing
            | Self::WorkspaceKeyInvalid
            | Self::CredentialsMissing => SyncFailureAction::Attention,
            Self::DeviceKeys(_) | Self::Grant(_) => SyncFailureAction::Retry,
            Self::ControlPlane(error) => control_plane_disposition(error),
            Self::Runner(error) => runner_disposition(error),
        }
    }

    pub(in crate::daemon) fn network_state_label(&self) -> &'static str {
        match self.disposition() {
            SyncFailureAction::Offline => "offline",
            SyncFailureAction::Attention | SyncFailureAction::Retry => "degraded",
        }
    }

    /// Path-free classification for externally-visible daemon status JSON and
    /// hosted-readable telemetry. Callers building those payloads use this
    /// instead of `to_string()`, whose text can embed workspace paths.
    pub(in crate::daemon) fn external_failure_code(&self) -> SyncExternalFailureCode {
        match self {
            Self::HostedConfigUnavailable | Self::ControlPlane(_) => {
                SyncExternalFailureCode::ControlPlaneUnavailable
            }
            Self::Runner(error) => error.external_failure_code(),
            // Trust/auth blockers get their own codes so status can tell the user
            // *why* sync is blocked (approve the device / recover the workspace)
            // instead of an opaque "unknown". The codes name no path or secret.
            Self::WorkspaceKeyMissing => SyncExternalFailureCode::WorkspaceKeyMissing,
            Self::WorkspaceKeyInvalid => SyncExternalFailureCode::WorkspaceKeyInvalid,
            Self::InvalidOperationPayload(_)
            | Self::CredentialsMissing
            | Self::DeviceKeys(_)
            | Self::Grant(_) => SyncExternalFailureCode::Unknown,
        }
    }

    pub(in crate::daemon) fn is_stat_cache_divergence(&self) -> bool {
        matches!(
            self,
            Self::Runner(SyncRunnerError::StatCacheDivergence { .. })
        )
    }

    pub(in crate::daemon) fn remote_domain_committed(&self) -> bool {
        matches!(
            self,
            Self::Runner(SyncRunnerError::Upload(
                UploadError::RemoteCommitCheckpoint(_)
            ))
        )
    }
}

impl fmt::Display for SyncOnceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidOperationPayload(message) => {
                write!(formatter, "invalid sync operation payload: {message}")
            }
            Self::HostedConfigUnavailable => {
                formatter.write_str("CONVEX_URL is required for daemon sync")
            }
            Self::WorkspaceKeyMissing => formatter.write_str(
                "workspace key is missing; approve this device or recover the workspace before daemon sync",
            ),
            Self::WorkspaceKeyInvalid => {
                formatter.write_str("workspace key material must be exactly 32 bytes")
            }
            Self::CredentialsMissing => formatter.write_str(
                "daemon sync requires account session credentials, BOWLINE_CONTROL_PLANE_TOKEN, or a stored account session",
            ),
            Self::DeviceKeys(error) => error.fmt(formatter),
            Self::Grant(error) => error.fmt(formatter),
            Self::ControlPlane(error) => error.fmt(formatter),
            Self::Runner(error) => error.fmt(formatter),
        }
    }
}

impl Error for SyncOnceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidOperationPayload(_)
            | Self::HostedConfigUnavailable
            | Self::WorkspaceKeyMissing
            | Self::WorkspaceKeyInvalid
            | Self::CredentialsMissing => None,
            Self::DeviceKeys(error) => Some(error),
            Self::Grant(error) => Some(error),
            Self::ControlPlane(error) => Some(error),
            Self::Runner(error) => Some(error),
        }
    }
}

impl From<DeviceKeyError> for SyncOnceError {
    fn from(error: DeviceKeyError) -> Self {
        Self::DeviceKeys(error)
    }
}

impl From<ControlPlaneError> for SyncOnceError {
    fn from(error: ControlPlaneError) -> Self {
        Self::ControlPlane(error)
    }
}

impl From<SyncRunnerError> for SyncOnceError {
    fn from(error: SyncRunnerError) -> Self {
        Self::Runner(error)
    }
}

impl From<HostedSetupError> for SyncOnceError {
    fn from(error: HostedSetupError) -> Self {
        match error {
            HostedSetupError::HostedConfigUnavailable => Self::HostedConfigUnavailable,
            HostedSetupError::CredentialsMissing => Self::CredentialsMissing,
            HostedSetupError::DeviceKeys(error) => Self::DeviceKeys(error),
            HostedSetupError::Grant(error) => Self::Grant(error),
            HostedSetupError::Client(error) => Self::ControlPlane(error),
            HostedSetupError::CachePoisoned | HostedSetupError::ContextChangedDuringBuild => {
                Self::ControlPlane(ControlPlaneError::Storage(error.to_string()))
            }
        }
    }
}

fn control_plane_disposition(error: &ControlPlaneError) -> SyncFailureAction {
    match error {
        ControlPlaneError::Timeout { .. } | ControlPlaneError::Transport { .. } => {
            SyncFailureAction::Offline
        }
        ControlPlaneError::Rejected {
            code:
                RejectionCode::DeviceNotTrusted
                | RejectionCode::Unauthorized
                | RejectionCode::WorkspaceMembershipRequired
                | RejectionCode::WorkspaceOwnerRequired,
            ..
        } => SyncFailureAction::Attention,
        ControlPlaneError::Rejected {
            code: RejectionCode::InvalidRequest | RejectionCode::Unknown,
            ..
        }
        | ControlPlaneError::WorkspaceMissing { .. }
        | ControlPlaneError::WorkViewMissing { .. }
        | ControlPlaneError::LeaseMissing { .. }
        | ControlPlaneError::CompareAndSwap(_)
        | ControlPlaneError::InvalidObjectKey { .. }
        | ControlPlaneError::ObjectMissing { .. }
        | ControlPlaneError::DeviceRequestMissing { .. }
        | ControlPlaneError::Conflict { .. }
        | ControlPlaneError::Storage(_) => SyncFailureAction::Retry,
        ControlPlaneError::Limited { .. } | ControlPlaneError::Unsupported { .. } => {
            SyncFailureAction::Attention
        }
    }
}

fn byte_store_disposition(error: &ByteStoreError) -> SyncFailureAction {
    match error {
        ByteStoreError::MissingObject { .. } | ByteStoreError::Network { .. } => {
            SyncFailureAction::Offline
        }
        ByteStoreError::HttpStatus {
            operation: TransferOperation::Download,
            status: 404,
            ..
        } => SyncFailureAction::Offline,
        ByteStoreError::IntentFailed {
            kind: IntentFailureKind::Timeout | IntentFailureKind::Transport,
            ..
        } => SyncFailureAction::Offline,
        ByteStoreError::IntentFailed {
            kind: IntentFailureKind::DeviceNotTrusted,
            ..
        } => SyncFailureAction::Attention,
        ByteStoreError::HttpStatus {
            operation: TransferOperation::Upload | TransferOperation::Delete,
            ..
        }
        | ByteStoreError::HttpStatus {
            operation: TransferOperation::Download,
            ..
        }
        | ByteStoreError::IntentFailed {
            kind: IntentFailureKind::Other,
            ..
        }
        | ByteStoreError::Io(_)
        | ByteStoreError::InvalidObjectKey { .. }
        | ByteStoreError::ObjectAlreadyExists(_)
        | ByteStoreError::CorruptObject { .. }
        | ByteStoreError::CorruptJournal { .. }
        | ByteStoreError::RangeOutOfBounds { .. }
        | ByteStoreError::UnsupportedOperation(_) => SyncFailureAction::Retry,
    }
}

fn runner_disposition(error: &SyncRunnerError) -> SyncFailureAction {
    match error.failure_source() {
        SyncRunnerFailureSource::Upload(error) => upload_disposition(error),
        SyncRunnerFailureSource::Download(error) => download_disposition(error),
        SyncRunnerFailureSource::Cache(error) => cache_disposition(error),
        SyncRunnerFailureSource::WorkViewOverlay(error) => work_view_overlay_disposition(error),
        SyncRunnerFailureSource::ControlPlane(error) => control_plane_disposition(error),
        SyncRunnerFailureSource::InvalidImportedSnapshot => SyncFailureAction::Attention,
        SyncRunnerFailureSource::Retry => SyncFailureAction::Retry,
    }
}

fn upload_disposition(error: &UploadError) -> SyncFailureAction {
    match error.failure_source() {
        UploadFailureSource::ControlPlane(error) => control_plane_disposition(error),
        UploadFailureSource::ByteStore(error) => byte_store_disposition(error),
        UploadFailureSource::Download(error) => download_disposition(error),
        UploadFailureSource::Retry => SyncFailureAction::Retry,
    }
}

fn download_disposition(error: &DownloadError) -> SyncFailureAction {
    match error {
        DownloadError::ControlPlane(error) => control_plane_disposition(error),
        DownloadError::ByteStore(error) => byte_store_disposition(error),
        DownloadError::SnapshotManifestMissing(_) => SyncFailureAction::Offline,
        DownloadError::Manifest(_)
        | DownloadError::MetadataPage(_)
        | DownloadError::Namespace(_)
        | DownloadError::MissingBinding(_)
        | DownloadError::CancellationRequested
        | DownloadError::UnsafePath(_)
        | DownloadError::UnsafeManifest(_) => SyncFailureAction::Attention,
    }
}

fn cache_disposition(error: &CacheError) -> SyncFailureAction {
    match error {
        CacheError::Store(error) => byte_store_disposition(error),
        CacheError::Io(_)
        | CacheError::MissingCachedBytes(_)
        | CacheError::MissingPackedLocator(_)
        | CacheError::ContentIdMismatch { .. }
        | CacheError::InvalidCacheKey(_)
        | CacheError::InvalidCachedPackRange { .. }
        | CacheError::ShortCachedPackRead { .. }
        | CacheError::ShortFetchedRange { .. }
        | CacheError::MismatchedCachedPackReader { .. }
        | CacheError::Pack(_) => SyncFailureAction::Retry,
    }
}

fn work_view_overlay_disposition(error: &WorkViewOverlaySyncError) -> SyncFailureAction {
    match error {
        WorkViewOverlaySyncError::ControlPlane(error) => control_plane_disposition(error),
        WorkViewOverlaySyncError::ByteStore(error) => byte_store_disposition(error),
        WorkViewOverlaySyncError::Cache(error) => cache_disposition(error),
        WorkViewOverlaySyncError::Wire(_) => SyncFailureAction::Attention,
        WorkViewOverlaySyncError::WorkView(_)
        | WorkViewOverlaySyncError::Metadata(_)
        | WorkViewOverlaySyncError::WorkViewUpdate(_)
        | WorkViewOverlaySyncError::CommitCleanup { .. }
        | WorkViewOverlaySyncError::PublicationCleanup { .. }
        | WorkViewOverlaySyncError::Packfile(_)
        | WorkViewOverlaySyncError::Json(_)
        | WorkViewOverlaySyncError::MissingOverlayPack
        | WorkViewOverlaySyncError::MissingStateRoot
        | WorkViewOverlaySyncError::MissingStagedContent
        | WorkViewOverlaySyncError::CancellationRequested
        | WorkViewOverlaySyncError::ClaimOwnershipLost => SyncFailureAction::Retry,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bowline_control_plane::Capability;
    use bowline_core::ids::ContentId;
    use bowline_storage::{HydrationPlanError, ObjectKey};

    fn object_key(value: &str) -> ObjectKey {
        ObjectKey::new(value.to_string()).expect("valid object key")
    }

    // Canary paths from the Plan 06 Security/privacy contract.
    const CANARY_PATHS: &[&str] = &[".env", "secrets/prod.key", "client/acme-payroll/keys.json"];

    #[test]
    fn retry_budget_routes_poison_operations_to_attention() {
        assert_eq!(
            SyncFailureAction::Retry
                .bounded_by_retry_budget(super::super::MAX_SYNC_RETRY_ATTEMPTS - 1),
            SyncFailureAction::Retry
        );
        assert_eq!(
            SyncFailureAction::Retry.bounded_by_retry_budget(super::super::MAX_SYNC_RETRY_ATTEMPTS),
            SyncFailureAction::Attention
        );
        assert_eq!(
            SyncFailureAction::Offline
                .bounded_by_retry_budget(super::super::MAX_SYNC_RETRY_ATTEMPTS),
            SyncFailureAction::Offline
        );
    }

    #[test]
    fn external_failure_code_redacts_path_bearing_errors() {
        let cases = [
            (
                SyncOnceError::Runner(SyncRunnerError::MaterializationBlockedByDirectory(
                    "client/acme-payroll/keys.json".to_string(),
                )),
                "materialization-blocked",
            ),
            (
                SyncOnceError::Runner(SyncRunnerError::UnsafeMaterializationPath(
                    "secrets/prod.key".to_string(),
                )),
                "materialization-blocked",
            ),
            (
                SyncOnceError::ControlPlane(ControlPlaneError::Transport {
                    // Path-bearing raw detail is exactly what must not survive into
                    // the external code; a canary secret path (not a /home path, which
                    // the public-export gate independently forbids) proves redaction.
                    detail: "connection to secrets/prod.key store refused".to_string(),
                }),
                "control-plane-unavailable",
            ),
        ];
        for (error, expected_code) in cases {
            // The raw message is why redaction is required...
            let raw = error.to_string();
            let code = error.external_failure_code().as_code();
            assert_eq!(code, expected_code);
            // ...and the code that ships externally must never carry a path.
            for path in CANARY_PATHS {
                assert!(
                    !code.contains(path),
                    "external code `{code}` leaked `{path}` (raw was `{raw}`)"
                );
            }
        }
    }

    #[test]
    fn attention_dispositions() {
        assert_eq!(
            SyncOnceError::WorkspaceKeyMissing.disposition(),
            SyncFailureAction::Attention
        );
        assert_eq!(
            SyncOnceError::WorkspaceKeyInvalid.disposition(),
            SyncFailureAction::Attention
        );
        assert_eq!(
            SyncOnceError::ControlPlane(ControlPlaneError::Rejected {
                code: RejectionCode::DeviceNotTrusted,
                message: "device is not trusted".to_string(),
            })
            .disposition(),
            SyncFailureAction::Attention
        );
        assert_eq!(
            SyncOnceError::ControlPlane(ControlPlaneError::Rejected {
                code: RejectionCode::Unauthorized,
                message: "device cannot update this lease".to_string(),
            })
            .disposition(),
            SyncFailureAction::Attention
        );
        assert_eq!(
            SyncOnceError::Runner(SyncRunnerError::Upload(UploadError::ByteStore(
                ByteStoreError::IntentFailed {
                    operation: TransferOperation::Upload,
                    kind: IntentFailureKind::DeviceNotTrusted,
                    detail: "device is not trusted".to_string(),
                },
            )))
            .disposition(),
            SyncFailureAction::Attention
        );
        assert_eq!(
            SyncOnceError::HostedConfigUnavailable.disposition(),
            SyncFailureAction::Attention
        );
        assert_eq!(
            SyncOnceError::CredentialsMissing.disposition(),
            SyncFailureAction::Attention
        );
        assert_eq!(
            SyncOnceError::ControlPlane(ControlPlaneError::Limited {
                capability: Capability::ObjectMetadata,
                reason: "object metadata disabled",
            })
            .disposition(),
            SyncFailureAction::Attention
        );
        assert_eq!(
            SyncOnceError::ControlPlane(ControlPlaneError::Unsupported {
                capability: "storage-gc",
                reason: "retention deletes are not available",
            })
            .disposition(),
            SyncFailureAction::Attention
        );
        for error in [
            HydrationPlanError::MissingLocatorField("offset"),
            HydrationPlanError::ConflictingContentLocator {
                content_id: ContentId::new("cid_conflicting"),
            },
            HydrationPlanError::OverlappingRanges {
                previous_end: 20,
                next_offset: 10,
            },
            HydrationPlanError::RangeOutOfBounds {
                offset: 9,
                length: 2,
                pack_len: 10,
            },
            HydrationPlanError::RangeOverflow {
                offset: u64::MAX,
                length: 1,
            },
        ] {
            let error = SyncOnceError::Runner(SyncRunnerError::InvalidImportedSnapshot(error));
            assert_eq!(error.disposition(), SyncFailureAction::Attention);
            assert_eq!(
                error.external_failure_code(),
                SyncExternalFailureCode::ImportedSnapshotInvalid
            );
        }
    }

    #[test]
    fn offline_dispositions() {
        assert_eq!(
            SyncOnceError::Runner(SyncRunnerError::Download(
                DownloadError::SnapshotManifestMissing("snap_missing".to_string()),
            ))
            .disposition(),
            SyncFailureAction::Offline
        );
        assert_eq!(
            SyncOnceError::Runner(SyncRunnerError::Upload(UploadError::ByteStore(
                ByteStoreError::MissingObject {
                    key: object_key("packs_pk_0011223344556677"),
                    component: "metadata",
                },
            )))
            .disposition(),
            SyncFailureAction::Offline
        );
        assert_eq!(
            SyncOnceError::Runner(SyncRunnerError::Download(DownloadError::ByteStore(
                ByteStoreError::HttpStatus {
                    key: object_key("packs_pk_0011223344556677"),
                    operation: TransferOperation::Download,
                    status: 404,
                },
            )))
            .disposition(),
            SyncFailureAction::Offline
        );
        assert_eq!(
            SyncOnceError::ControlPlane(ControlPlaneError::Timeout {
                capability: "hosted Convex",
            })
            .disposition(),
            SyncFailureAction::Offline
        );
        assert_eq!(
            SyncOnceError::ControlPlane(ControlPlaneError::Transport {
                detail: "connection refused".to_string(),
            })
            .disposition(),
            SyncFailureAction::Offline
        );
        assert_eq!(
            SyncOnceError::Runner(SyncRunnerError::Cache(CacheError::Store(
                ByteStoreError::MissingObject {
                    key: object_key("packs_pk_0011223344556677"),
                    component: "object",
                },
            )))
            .disposition(),
            SyncFailureAction::Offline
        );
        assert_eq!(
            SyncOnceError::Runner(SyncRunnerError::WorkViewOverlay(
                WorkViewOverlaySyncError::ByteStore(ByteStoreError::Network {
                    operation: TransferOperation::Download,
                    detail: "connection refused".to_string(),
                }),
            ))
            .disposition(),
            SyncFailureAction::Offline
        );
        assert_eq!(
            SyncOnceError::Runner(SyncRunnerError::WorkViewOverlay(
                WorkViewOverlaySyncError::Cache(CacheError::Store(ByteStoreError::MissingObject {
                    key: object_key("packs_pk_1122334455667788"),
                    component: "object",
                })),
            ))
            .disposition(),
            SyncFailureAction::Offline
        );
    }

    #[test]
    fn retry_dispositions() {
        assert_eq!(
            SyncOnceError::Runner(SyncRunnerError::Cache(CacheError::ShortFetchedRange {
                expected: 10,
                actual: 9,
            }))
            .disposition(),
            SyncFailureAction::Retry
        );
        assert_eq!(
            SyncOnceError::Runner(SyncRunnerError::Upload(UploadError::ByteStore(
                ByteStoreError::CorruptObject {
                    key: object_key("packs_pk_8899aabbccddeeff"),
                    reason: "object bytes did not match metadata",
                },
            )))
            .disposition(),
            SyncFailureAction::Retry
        );
        assert_eq!(
            SyncOnceError::Runner(SyncRunnerError::Download(DownloadError::ByteStore(
                ByteStoreError::HttpStatus {
                    key: object_key("packs_pk_0123456789abcdef"),
                    operation: TransferOperation::Download,
                    status: 500,
                },
            )))
            .disposition(),
            SyncFailureAction::Retry
        );
        assert_eq!(
            SyncOnceError::Runner(SyncRunnerError::Upload(UploadError::ByteStore(
                ByteStoreError::HttpStatus {
                    key: object_key("packs_pk_fedcba9876543210"),
                    operation: TransferOperation::Upload,
                    status: 404,
                },
            )))
            .disposition(),
            SyncFailureAction::Retry
        );
        assert_eq!(
            SyncOnceError::ControlPlane(ControlPlaneError::Rejected {
                code: RejectionCode::InvalidRequest,
                message: "device trust has expired".to_string(),
            })
            .disposition(),
            SyncFailureAction::Retry
        );
        assert_eq!(
            SyncOnceError::Runner(SyncRunnerError::StateIo(io::Error::other(
                "state unavailable",
            )))
            .disposition(),
            SyncFailureAction::Retry
        );
        assert_eq!(
            SyncOnceError::Runner(SyncRunnerError::WorkViewOverlay(
                WorkViewOverlaySyncError::MissingOverlayPack,
            ))
            .disposition(),
            SyncFailureAction::Retry
        );
    }

    #[test]
    fn rewording_messages_do_not_change_disposition() {
        assert_eq!(
            SyncOnceError::ControlPlane(ControlPlaneError::Storage(
                "connection to network timed out while approving this trusted device".to_string(),
            ))
            .disposition(),
            SyncFailureAction::Retry
        );
        assert_eq!(
            SyncOnceError::Runner(SyncRunnerError::Upload(UploadError::ByteStore(
                ByteStoreError::CorruptObject {
                    key: object_key("packs_pk_8899aabbccddeeff"),
                    reason: "missing object network timed out",
                },
            )))
            .disposition(),
            SyncFailureAction::Retry
        );
        assert_eq!(
            SyncOnceError::ControlPlane(ControlPlaneError::Transport {
                detail: "everything is fine".to_string(),
            })
            .disposition(),
            SyncFailureAction::Offline
        );
    }
}
