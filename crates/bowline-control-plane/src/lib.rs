#![deny(unsafe_code)]

use std::{
    error::Error,
    fmt,
    sync::{Arc, Mutex},
};

mod fake;

#[cfg(feature = "hosted-convex")]
pub mod hosted;
#[cfg(feature = "hosted-convex")]
pub mod transfer;

pub use fake::FakeControlPlaneClient;

#[cfg(feature = "hosted-convex")]
pub use hosted::{HostedControlPlaneClient, HostedFunctionCallCount, hosted_function_call_counts};
#[cfg(feature = "hosted-convex")]
pub use transfer::SignedUrlByteStore;

pub type ControlPlaneResult<T> = Result<T, ControlPlaneError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ControlPlaneTimestamp {
    pub tick: u64,
}

impl fmt::Display for ControlPlaneTimestamp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "t{:012}", self.tick)
    }
}

#[derive(Debug, Clone)]
pub struct DeterministicClock {
    next_tick: Arc<Mutex<u64>>,
}

impl DeterministicClock {
    pub fn new(start_tick: u64) -> Self {
        Self {
            next_tick: Arc::new(Mutex::new(start_tick)),
        }
    }

    pub fn now(&self) -> ControlPlaneTimestamp {
        let mut next_tick = self.next_tick.lock().expect("deterministic clock poisoned");
        let timestamp = ControlPlaneTimestamp { tick: *next_tick };
        *next_tick += 1;
        timestamp
    }

    pub fn peek(&self) -> ControlPlaneTimestamp {
        let next_tick = self.next_tick.lock().expect("deterministic clock poisoned");
        ControlPlaneTimestamp { tick: *next_tick }
    }
}

impl Default for DeterministicClock {
    fn default() -> Self {
        Self::new(0)
    }
}

#[derive(Debug, Clone)]
pub struct DeterministicIdGenerator {
    prefix: String,
    next_id: Arc<Mutex<u64>>,
}

impl DeterministicIdGenerator {
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: sanitize_id_part(&prefix.into()),
            next_id: Arc::new(Mutex::new(1)),
        }
    }

    pub fn next_id(&self, kind: &str) -> String {
        let mut next_id = self
            .next_id
            .lock()
            .expect("deterministic ID generator poisoned");
        let id = format!("{}-{}-{:08}", self.prefix, sanitize_id_part(kind), *next_id);
        *next_id += 1;
        id
    }
}

impl Default for DeterministicIdGenerator {
    fn default() -> Self {
        Self::new("bowline")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRef {
    pub workspace_id: String,
    pub version: u64,
    pub snapshot_id: String,
    pub updated_at: ControlPlaneTimestamp,
    pub updated_by_device_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleWorkspaceRef {
    pub expected_version: u64,
    pub current: WorkspaceRef,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleWorkViewOverlayHead {
    pub expected_overlay_version: u64,
    pub current: WorkViewRecord,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompareAndSwapError {
    WorkspaceMissing {
        workspace_id: String,
    },
    StaleRef(StaleWorkspaceRef),
    Storage(String),
    Unsupported {
        capability: &'static str,
        reason: &'static str,
    },
}

impl fmt::Display for CompareAndSwapError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkspaceMissing { workspace_id } => {
                write!(formatter, "workspace `{workspace_id}` does not exist")
            }
            Self::StaleRef(stale) => write!(
                formatter,
                "workspace `{}` is at version {}, not expected version {}",
                stale.current.workspace_id, stale.current.version, stale.expected_version
            ),
            Self::Storage(error) => write!(formatter, "control-plane storage failed: {error}"),
            Self::Unsupported { capability, reason } => {
                write!(formatter, "{capability} is unsupported: {reason}")
            }
        }
    }
}

impl Error for CompareAndSwapError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlPlaneError {
    WorkspaceMissing {
        workspace_id: String,
    },
    WorkViewMissing {
        work_view_id: String,
    },
    LeaseMissing {
        lease_id: String,
    },
    CompareAndSwap(CompareAndSwapError),
    InvalidObjectKey {
        reason: &'static str,
    },
    ObjectMissing {
        object_key: String,
    },
    DeviceRequestMissing {
        request_id: String,
    },
    Limited {
        capability: &'static str,
        reason: &'static str,
    },
    Unsupported {
        capability: &'static str,
        reason: &'static str,
    },
    Conflict {
        resource: &'static str,
        reason: &'static str,
    },
    Storage(String),
}

impl fmt::Display for ControlPlaneError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkspaceMissing { workspace_id } => {
                write!(formatter, "workspace `{workspace_id}` does not exist")
            }
            Self::WorkViewMissing { work_view_id } => {
                write!(formatter, "work view `{work_view_id}` does not exist")
            }
            Self::LeaseMissing { lease_id } => {
                write!(formatter, "lease `{lease_id}` does not exist")
            }
            Self::CompareAndSwap(error) => error.fmt(formatter),
            Self::InvalidObjectKey { reason } => {
                write!(formatter, "object key is invalid: {reason}")
            }
            Self::ObjectMissing { object_key } => {
                write!(formatter, "object `{object_key}` does not exist")
            }
            Self::DeviceRequestMissing { request_id } => {
                write!(formatter, "device request `{request_id}` does not exist")
            }
            Self::Limited { capability, reason } => {
                write!(formatter, "{capability} is limited in this phase: {reason}")
            }
            Self::Unsupported { capability, reason } => {
                write!(formatter, "{capability} is unsupported: {reason}")
            }
            Self::Conflict { resource, reason } => {
                write!(
                    formatter,
                    "{resource} conflicts with existing metadata: {reason}"
                )
            }
            Self::Storage(error) => write!(formatter, "storage failed: {error}"),
        }
    }
}

impl Error for ControlPlaneError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CompareAndSwap(error) => Some(error),
            _ => None,
        }
    }
}

impl From<CompareAndSwapError> for ControlPlaneError {
    fn from(error: CompareAndSwapError) -> Self {
        match error {
            CompareAndSwapError::WorkspaceMissing { workspace_id } => {
                Self::WorkspaceMissing { workspace_id }
            }
            error => Self::CompareAndSwap(error),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkViewUpdateError {
    WorkViewMissing {
        work_view_id: String,
    },
    StaleOverlayHead(Box<StaleWorkViewOverlayHead>),
    Storage(String),
    Unsupported {
        capability: &'static str,
        reason: &'static str,
    },
}

impl fmt::Display for WorkViewUpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkViewMissing { work_view_id } => {
                write!(formatter, "work view `{work_view_id}` does not exist")
            }
            Self::StaleOverlayHead(stale) => write!(
                formatter,
                "work view `{}` overlay is at version {}, not expected version {}",
                stale.current.work_view_id,
                stale.current.overlay_version,
                stale.expected_overlay_version
            ),
            Self::Storage(error) => write!(formatter, "control-plane storage failed: {error}"),
            Self::Unsupported { capability, reason } => {
                write!(formatter, "{capability} is unsupported: {reason}")
            }
        }
    }
}

impl Error for WorkViewUpdateError {}

impl From<ControlPlaneError> for WorkViewUpdateError {
    fn from(error: ControlPlaneError) -> Self {
        match error {
            ControlPlaneError::WorkViewMissing { work_view_id } => {
                Self::WorkViewMissing { work_view_id }
            }
            error => Self::Storage(error.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactEvent {
    pub event_id: String,
    pub workspace_id: String,
    pub at: ControlPlaneTimestamp,
    pub kind: CompactEventKind,
    pub subject: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactEventKind {
    DeviceHarnessApproved,
    DeviceApprovalRequested,
    DeviceApproved,
    DeviceDenied,
    DeviceRevoked,
    DeviceRequested,
    RecoveryKeyCreated,
    RecoveryKeyVerified,
    RecoveryKeyRotated,
    RecoveryKeyRevoked,
    AuthLoginStarted,
    AuthLoginCompleted,
    ConflictDetected,
    ConflictResolved,
    LeaseBlocked,
    LeaseCleanupCompleted,
    LeaseCompleted,
    LeaseCreated,
    LeaseExpired,
    LeaseHydrationRequested,
    LeaseRevoked,
    LeaseReviewReady,
    LeaseToolDenied,
    LeaseToolInvoked,
    LeaseUpdated,
    ObjectManifestCommitted,
    ObjectPointerAdded,
    OverlayChanged,
    PublishRequested,
    WorkAccepted,
    WorkArchived,
    WorkCleanupCompleted,
    WorkCleanupPreviewed,
    WorkCreated,
    WorkDiscarded,
    WorkExpired,
    WorkRestored,
    WorkReviewReady,
    WorkUpdated,
    WorkspaceCreated,
    WorkspaceRefAdvanced,
}

impl CompactEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DeviceHarnessApproved => "device.harness_approved",
            Self::DeviceApprovalRequested => "device.approval_requested",
            Self::DeviceApproved => "device.approved",
            Self::DeviceDenied => "device.denied",
            Self::DeviceRevoked => "device.revoked",
            Self::DeviceRequested => "device.requested",
            Self::RecoveryKeyCreated => "recovery_key.created",
            Self::RecoveryKeyVerified => "recovery_key.verified",
            Self::RecoveryKeyRotated => "recovery_key.rotated",
            Self::RecoveryKeyRevoked => "recovery_key.revoked",
            Self::AuthLoginStarted => "auth.login_started",
            Self::AuthLoginCompleted => "auth.login_completed",
            Self::ConflictDetected => "conflict.detected",
            Self::ConflictResolved => "conflict.resolved",
            Self::LeaseBlocked => "lease.blocked",
            Self::LeaseCleanupCompleted => "lease.cleanup_completed",
            Self::LeaseCompleted => "lease.completed",
            Self::LeaseCreated => "lease.created",
            Self::LeaseExpired => "lease.expired",
            Self::LeaseHydrationRequested => "lease.hydration_requested",
            Self::LeaseRevoked => "lease.revoked",
            Self::LeaseReviewReady => "lease.review_ready",
            Self::LeaseToolDenied => "lease.tool_denied",
            Self::LeaseToolInvoked => "lease.tool_invoked",
            Self::LeaseUpdated => "lease.updated",
            Self::ObjectManifestCommitted => "object_manifest.committed",
            Self::ObjectPointerAdded => "object_pointer.added",
            Self::OverlayChanged => "overlay.changed",
            Self::PublishRequested => "publish.requested",
            Self::WorkAccepted => "work.accepted",
            Self::WorkArchived => "work.archived",
            Self::WorkCleanupCompleted => "work.cleanup_completed",
            Self::WorkCleanupPreviewed => "work.cleanup_previewed",
            Self::WorkCreated => "work.created",
            Self::WorkDiscarded => "work.discarded",
            Self::WorkExpired => "work.expired",
            Self::WorkRestored => "work.restored",
            Self::WorkReviewReady => "work.review_ready",
            Self::WorkUpdated => "work.updated",
            Self::WorkspaceCreated => "workspace.created",
            Self::WorkspaceRefAdvanced => "workspace_ref.advanced",
        }
    }
}

impl fmt::Display for CompactEventKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    SourcePack,
    IndexPack,
    LocatorIndex,
    SnapshotManifest,
    AgentOverlay,
}

impl ObjectKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SourcePack => "source-pack",
            Self::IndexPack => "index-pack",
            Self::LocatorIndex => "locator-index",
            Self::SnapshotManifest => "snapshot-manifest",
            Self::AgentOverlay => "overlay-pack",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectPointer {
    pub object_key: String,
    pub content_id: String,
    pub byte_len: u64,
    pub hash: String,
    pub key_epoch: u32,
    pub kind: ObjectKind,
    pub created_at: ControlPlaneTimestamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictMetadataPublish {
    pub workspace_id: String,
    pub conflict_id: String,
    pub conflict_kind: String,
    pub paths: Vec<String>,
    pub contains_secrets: bool,
    pub base_snapshot_id: String,
    pub remote_snapshot_id: String,
    pub detected_by_device_id: String,
    pub bundle_object: Option<ObjectPointer>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictResolutionMark {
    pub workspace_id: String,
    pub conflict_id: String,
    pub resolved_by_device_id: String,
    pub resolution: ConflictResolutionState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictResolutionState {
    Accepted,
    Rejected,
}

impl ConflictResolutionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictMetadataRecord {
    pub workspace_id: String,
    pub conflict_id: String,
    pub conflict_kind: String,
    pub paths: Vec<String>,
    pub contains_secrets: bool,
    pub state: String,
    pub base_snapshot_id: String,
    pub remote_snapshot_id: String,
    pub detected_by_device_id: String,
    pub bundle_object: Option<ObjectPointer>,
    pub detected_at: ControlPlaneTimestamp,
    pub resolved_by_device_id: Option<String>,
    pub resolved_at: Option<ControlPlaneTimestamp>,
}

/// Redacted live workspace status snapshot published by a trusted device (the
/// daemon) to the control plane so the dashboard can show sync/index/watcher
/// posture. Paths are workspace-relative and secrets are never included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceStatusSnapshot {
    pub workspace_id: String,
    pub snapshot_id: String,
    /// One of "healthy" | "attention" | "limited".
    pub status_level: String,
    pub attention_items: Vec<String>,
    pub generated_at: String,
    pub event_watermarks: StatusEventWatermarks,
    pub sync_queue: Option<StatusSyncQueueSnapshot>,
    pub index: Option<StatusIndexSnapshot>,
    pub workspace_summary: Option<StatusWorkspaceSummarySnapshot>,
    pub items: Vec<StatusItemSnapshot>,
    pub limits: Vec<StatusLimitSnapshot>,
    pub published_by_device_id: String,
}

impl WorkspaceStatusSnapshot {
    /// Canonical proof subject the daemon signs for the
    /// `status:publishWorkspaceStatus` mutation. Must stay byte-for-byte in sync
    /// with `statusPublishProofSubject` on the Convex side.
    pub fn proof_subject(&self) -> String {
        format!(
            "workspaceId={}\nsnapshotId={}\nstatusLevel={}\ngeneratedAt={}",
            self.workspace_id, self.snapshot_id, self.status_level, self.generated_at
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusEventWatermarks {
    pub last_event_id: Option<String>,
    pub last_scan_at: Option<String>,
    pub sync_state: Option<String>,
    pub watcher_state: Option<String>,
    pub network_state: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusSyncQueueSnapshot {
    pub queued: u64,
    pub claimed: u64,
    pub waiting_retry: u64,
    pub blocked_offline: u64,
    pub attention: u64,
    pub completed: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusIndexSnapshot {
    pub state: String,
    pub file_count: u64,
    pub path_count: u64,
    pub summary: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatusWorkspaceSummarySnapshot {
    pub total_projects: Option<u64>,
    pub repo_count: Option<u64>,
    pub env_file_count: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusItemSnapshot {
    pub kind: String,
    pub summary: String,
    pub path: Option<String>,
    pub event_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusLimitSnapshot {
    pub capability: String,
    pub unavailable_because: String,
    pub path: Option<String>,
    pub still_works: Vec<String>,
}

pub type ByteRange = bowline_storage::ByteRange;

#[derive(Clone, PartialEq, Eq)]
pub struct SignedUrlIntent {
    pub url: String,
    pub expires_at: ControlPlaneTimestamp,
}

impl fmt::Debug for SignedUrlIntent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SignedUrlIntent")
            .field("url", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadIntentRequest {
    pub workspace_id: String,
    pub object_kind: ObjectKind,
    pub byte_len: u64,
    pub content_id: Option<String>,
    pub object_key: Option<String>,
}

impl UploadIntentRequest {
    pub fn new(workspace_id: impl Into<String>, object_kind: ObjectKind, byte_len: u64) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            object_kind,
            byte_len,
            content_id: None,
            object_key: None,
        }
    }

    pub fn with_content_id(mut self, content_id: impl Into<String>) -> Self {
        self.content_id = Some(content_id.into());
        self
    }

    pub fn with_object_key(mut self, object_key: impl Into<String>) -> Self {
        self.object_key = Some(object_key.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadIntent {
    pub workspace_id: String,
    pub object_key: String,
    pub object_kind: ObjectKind,
    pub byte_len: u64,
    pub signed_url: SignedUrlIntent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadVerificationIntentRequest {
    pub workspace_id: String,
    pub object_key: String,
    pub byte_len: u64,
    pub content_id: Option<String>,
}

impl UploadVerificationIntentRequest {
    pub fn new(
        workspace_id: impl Into<String>,
        object_key: impl Into<String>,
        byte_len: u64,
    ) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            object_key: object_key.into(),
            byte_len,
            content_id: None,
        }
    }

    pub fn with_content_id(mut self, content_id: impl Into<String>) -> Self {
        self.content_id = Some(content_id.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadIntentRequest {
    pub workspace_id: String,
    pub object_key: String,
    pub range: Option<ByteRange>,
}

impl DownloadIntentRequest {
    pub fn full(workspace_id: impl Into<String>, object_key: impl Into<String>) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            object_key: object_key.into(),
            range: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadIntent {
    pub workspace_id: String,
    pub object_key: String,
    pub range: Option<ByteRange>,
    pub signed_url: SignedUrlIntent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectRetentionStateUpdate {
    pub workspace_id: String,
    pub object_key: String,
    pub retention_state: bowline_storage::RetentionState,
}

impl ObjectRetentionStateUpdate {
    pub fn new(
        workspace_id: impl Into<String>,
        object_key: impl Into<String>,
        retention_state: bowline_storage::RetentionState,
    ) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            object_key: object_key.into(),
            retention_state,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteIntentRequest {
    pub workspace_id: String,
    pub object_key: String,
    pub object_kind: Option<ObjectKind>,
    pub key_epoch: Option<u32>,
}

impl DeleteIntentRequest {
    pub fn new(workspace_id: impl Into<String>, object_key: impl Into<String>) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            object_key: object_key.into(),
            object_kind: None,
            key_epoch: None,
        }
    }

    pub fn with_object_kind(mut self, object_kind: ObjectKind) -> Self {
        self.object_kind = Some(object_kind);
        self
    }

    pub fn with_key_epoch(mut self, key_epoch: u32) -> Self {
        self.key_epoch = Some(key_epoch);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteIntent {
    pub workspace_id: String,
    pub object_key: String,
    pub object_kind: ObjectKind,
    pub key_epoch: u32,
    pub signed_url: SignedUrlIntent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectManifestCommit {
    pub workspace_id: String,
    pub snapshot_id: String,
    pub manifest_id: String,
    pub manifest_object: ObjectPointer,
    pub pack_objects: Vec<ObjectPointer>,
    pub committed_by_device_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMetadataCommit {
    pub workspace_id: String,
    pub object: ObjectPointer,
    pub committed_by_device_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectManifestRecord {
    pub workspace_id: String,
    pub snapshot_id: String,
    pub manifest_id: String,
    pub manifest_object: ObjectPointer,
    pub pack_objects: Vec<ObjectPointer>,
    pub committed_by_device_id: String,
    pub committed_at: ControlPlaneTimestamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkViewLifecycleState {
    Active,
    ReviewReady,
    Accepted,
    Discarded,
    Expired,
    Archived,
}

impl WorkViewLifecycleState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::ReviewReady => "review-ready",
            Self::Accepted => "accepted",
            Self::Discarded => "discarded",
            Self::Expired => "expired",
            Self::Archived => "archived",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkViewCreate {
    pub workspace_id: String,
    pub work_view_id: String,
    pub project_id: String,
    pub name: String,
    pub visible_path: String,
    pub base_snapshot_id: String,
    pub base_workspace_version: u64,
    pub created_by_device_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkViewLifecycleUpdate {
    pub workspace_id: String,
    pub work_view_id: String,
    pub lifecycle: WorkViewLifecycleState,
    pub updated_by_device_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkViewOverlayCommit {
    pub workspace_id: String,
    pub work_view_id: String,
    pub expected_overlay_version: u64,
    pub overlay_object: ObjectPointer,
    pub committed_by_device_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkViewRecord {
    pub workspace_id: String,
    pub work_view_id: String,
    pub project_id: String,
    pub name: String,
    pub visible_path: String,
    pub base_snapshot_id: String,
    pub base_workspace_version: u64,
    pub overlay_head: Option<ObjectPointer>,
    pub overlay_version: u64,
    pub lifecycle: WorkViewLifecycleState,
    pub created_by_device_id: String,
    pub updated_by_device_id: String,
    pub created_at: ControlPlaneTimestamp,
    pub updated_at: ControlPlaneTimestamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRequestInput {
    pub workspace_id: String,
    pub device_id: String,
    pub device_name: String,
    pub platform: String,
    pub device_public_key: String,
    pub device_fingerprint: String,
    pub device_authorization_proof_verifier: String,
    pub matching_code: String,
    pub account_id: Option<String>,
    pub host: Option<String>,
    pub root: Option<String>,
    pub expires_in_ticks: u64,
}

impl DeviceRequestInput {
    pub fn new(
        workspace_id: impl Into<String>,
        device_id: impl Into<String>,
        device_name: impl Into<String>,
        device_public_key: impl Into<String>,
        device_fingerprint: impl Into<String>,
        matching_code: impl Into<String>,
    ) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            device_id: device_id.into(),
            device_name: device_name.into(),
            platform: std::env::consts::OS.to_string(),
            device_public_key: device_public_key.into(),
            device_fingerprint: device_fingerprint.into(),
            device_authorization_proof_verifier: String::new(),
            matching_code: matching_code.into(),
            account_id: None,
            host: None,
            root: None,
            expires_in_ticks: 600,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapSessionInput {
    pub workspace_id: String,
    pub host: Option<String>,
    pub root: Option<String>,
    pub expires_in_ticks: u64,
}

impl BootstrapSessionInput {
    pub fn new(workspace_id: impl Into<String>) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            host: None,
            root: None,
            expires_in_ticks: 600,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapSession {
    pub session_id: String,
    pub workspace_id: String,
    pub token: String,
    pub expires_at: ControlPlaneTimestamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRequest {
    pub request_id: String,
    pub workspace_id: String,
    pub device_id: String,
    pub device_name: String,
    pub platform: String,
    pub device_public_key: String,
    pub device_fingerprint: String,
    pub matching_code: String,
    pub account_id: Option<String>,
    pub host: Option<String>,
    pub root: Option<String>,
    pub requested_at: ControlPlaneTimestamp,
    pub expires_at: ControlPlaneTimestamp,
    pub state: DeviceRequestState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceRequestState {
    Pending,
    Approved,
    Denied,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizedDeviceRecord {
    pub workspace_id: String,
    pub device_id: String,
    pub device_name: String,
    pub platform: String,
    pub device_fingerprint: String,
    pub authorized_at: ControlPlaneTimestamp,
    pub authorized_by_device_id: Option<String>,
    pub revoked_at: Option<ControlPlaneTimestamp>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirstAuthorizedDeviceInput {
    pub workspace_id: String,
    pub device_id: String,
    pub device_name: String,
    pub platform: String,
    pub device_fingerprint: String,
    pub device_authorization_proof_verifier: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceApprovalRequestList {
    pub pending_requests: Vec<DeviceRequest>,
    pub authorized_devices: Vec<AuthorizedDeviceRecord>,
    pub revoked_devices: Vec<RevokedDeviceRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceApprovalInput {
    pub request_id: String,
    pub approved_by_device_id: String,
    pub approved_by_device_proof: String,
    pub encrypted_grant_ciphertext: String,
    pub grant_acceptance_proof_verifier: String,
    pub key_epoch: u32,
    pub expires_in_ticks: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceApproval {
    pub grant_id: String,
    pub request_id: String,
    pub workspace_id: String,
    pub device_id: String,
    pub device_name: String,
    pub platform: String,
    pub device_fingerprint: String,
    pub approved_by_device_id: String,
    pub encrypted_grant_ciphertext: String,
    pub key_epoch: u32,
    pub granted_at: ControlPlaneTimestamp,
    pub expires_at: ControlPlaneTimestamp,
    pub accepted_at: Option<ControlPlaneTimestamp>,
    pub harness_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceDenialInput {
    pub request_id: String,
    pub denied_by_device_id: String,
    pub denied_by_device_proof: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceDenial {
    pub request_id: String,
    pub workspace_id: String,
    pub device_id: String,
    pub denied_by_device_id: String,
    pub denied_at: ControlPlaneTimestamp,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRevocationInput {
    pub workspace_id: String,
    pub device_id: String,
    pub revoked_by_device_id: String,
    pub revoked_by_device_proof: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevokedDeviceRecord {
    pub workspace_id: String,
    pub device_id: String,
    pub device_name: String,
    pub platform: String,
    pub device_fingerprint: String,
    pub revoked_at: ControlPlaneTimestamp,
    pub revoked_by_device_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantAcceptanceInput {
    pub request_id: String,
    pub device_id: String,
    pub grant_acceptance_proof: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryEnvelopeInput {
    pub workspace_id: String,
    pub envelope_id: String,
    pub created_by_device_id: String,
    pub created_by_device_proof: String,
    pub ciphertext: String,
    pub fingerprint: String,
    pub recovery_proof_verifier: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryDeviceAuthorizationInput {
    pub workspace_id: String,
    pub envelope_id: String,
    pub request_id: String,
    pub encrypted_grant_ciphertext: String,
    pub grant_acceptance_proof_verifier: String,
    pub key_epoch: u32,
    pub recovery_proof: String,
    pub expires_in_ticks: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryEnvelopeState {
    GeneratedUnverified,
    Active,
    Rotated,
    Revoked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryEnvelopeRecord {
    pub workspace_id: String,
    pub envelope_id: String,
    pub created_by_device_id: String,
    pub ciphertext: String,
    pub fingerprint: String,
    pub state: RecoveryEnvelopeState,
    pub created_at: ControlPlaneTimestamp,
    pub verified_at: Option<ControlPlaneTimestamp>,
    pub rotated_at: Option<ControlPlaneTimestamp>,
    pub revoked_at: Option<ControlPlaneTimestamp>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub lease_id: String,
    pub workspace_id: String,
    pub project_id: String,
    pub device_id: String,
    pub write_target_mode: LeaseWriteTargetMode,
    pub work_view_id: Option<String>,
    pub base_snapshot_id: String,
    pub version: u64,
    pub execution_state: LeaseExecutionState,
    pub output_state: LeaseOutputState,
    pub status_code: String,
    pub output_object: Option<ObjectPointer>,
    pub audit_object: Option<ObjectPointer>,
    pub created_at: ControlPlaneTimestamp,
    pub updated_at: ControlPlaneTimestamp,
    pub expires_at: ControlPlaneTimestamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseExecutionState {
    Active,
    Blocked,
    Completed,
    Expired,
    Revoked,
}

impl LeaseExecutionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Blocked => "blocked",
            Self::Completed => "completed",
            Self::Expired => "expired",
            Self::Revoked => "revoked",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseOutputState {
    Empty,
    Dirty,
    ReviewReady,
    Accepted,
    Discarded,
    Conflicted,
    Retained,
}

impl LeaseOutputState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Dirty => "dirty",
            Self::ReviewReady => "review-ready",
            Self::Accepted => "accepted",
            Self::Discarded => "discarded",
            Self::Conflicted => "conflicted",
            Self::Retained => "retained",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseWriteTargetMode {
    Direct,
    WorkView,
}

impl LeaseWriteTargetMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::WorkView => "work-view",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseCreate {
    pub workspace_id: String,
    pub lease_id: String,
    pub project_id: String,
    pub device_id: String,
    pub write_target_mode: LeaseWriteTargetMode,
    pub work_view_id: Option<String>,
    pub base_snapshot_id: String,
    pub execution_state: LeaseExecutionState,
    pub output_state: LeaseOutputState,
    pub status_code: String,
    pub output_object: Option<ObjectPointer>,
    pub audit_object: Option<ObjectPointer>,
    pub expires_at: ControlPlaneTimestamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseUpdate {
    pub workspace_id: String,
    pub lease_id: String,
    pub expected_version: u64,
    pub updated_by_device_id: String,
    pub execution_state: Option<LeaseExecutionState>,
    pub output_state: Option<LeaseOutputState>,
    pub status_code: Option<String>,
    pub output_object: Option<ObjectPointer>,
    pub audit_object: Option<ObjectPointer>,
    pub event_kind: Option<CompactEventKind>,
}

pub trait ControlPlaneClient {
    fn create_workspace_ref(&self, workspace_id: &str) -> ControlPlaneResult<WorkspaceRef>;

    fn get_workspace_ref(&self, workspace_id: &str) -> ControlPlaneResult<Option<WorkspaceRef>>;

    fn observe_workspace_ref(
        &self,
        workspace_id: &str,
    ) -> ControlPlaneResult<Option<WorkspaceRef>> {
        self.get_workspace_ref(workspace_id)
    }

    fn compare_and_swap_workspace_ref(
        &self,
        workspace_id: &str,
        expected_version: u64,
        new_snapshot_id: &str,
        writer_device_id: &str,
    ) -> Result<WorkspaceRef, CompareAndSwapError>;

    fn list_events(&self, workspace_id: &str) -> ControlPlaneResult<Vec<CompactEvent>>;

    fn publish_conflict_metadata(
        &self,
        input: ConflictMetadataPublish,
    ) -> ControlPlaneResult<ConflictMetadataRecord>;

    fn list_workspace_conflicts(
        &self,
        workspace_id: &str,
        requested_by_device_id: &str,
    ) -> ControlPlaneResult<Vec<ConflictMetadataRecord>>;

    fn mark_conflict_resolved(
        &self,
        input: ConflictResolutionMark,
    ) -> ControlPlaneResult<ConflictMetadataRecord>;

    /// Publish a redacted live status snapshot for the workspace. In-memory and
    /// offline control planes treat this as a no-op; the hosted client forwards
    /// it to the `status:publishWorkspaceStatus` mutation.
    fn publish_workspace_status(
        &self,
        _snapshot: &WorkspaceStatusSnapshot,
    ) -> ControlPlaneResult<()> {
        Ok(())
    }

    fn create_upload_intent(
        &self,
        request: UploadIntentRequest,
    ) -> ControlPlaneResult<UploadIntent>;

    fn create_download_intent(
        &self,
        request: DownloadIntentRequest,
    ) -> ControlPlaneResult<DownloadIntent>;

    fn create_upload_verification_intent(
        &self,
        request: UploadVerificationIntentRequest,
    ) -> ControlPlaneResult<DownloadIntent>;

    fn mark_object_retention_state(
        &self,
        update: ObjectRetentionStateUpdate,
    ) -> ControlPlaneResult<bowline_storage::ObjectMetadata>;

    fn create_delete_intent(
        &self,
        request: DeleteIntentRequest,
    ) -> ControlPlaneResult<DeleteIntent>;

    fn head_object_metadata(
        &self,
        workspace_id: &str,
        object_key: &str,
    ) -> ControlPlaneResult<bowline_storage::ObjectMetadata>;

    fn commit_uploaded_object_metadata(
        &self,
        _commit: ObjectMetadataCommit,
    ) -> ControlPlaneResult<bowline_storage::ObjectMetadata> {
        Err(ControlPlaneError::Limited {
            capability: "object-metadata",
            reason: "committing uploaded object metadata requires a hosted control-plane implementation.",
        })
    }

    fn commit_object_manifest(
        &self,
        commit: ObjectManifestCommit,
    ) -> ControlPlaneResult<ObjectManifestRecord>;

    fn get_snapshot_manifest_pointer(
        &self,
        workspace_id: &str,
        snapshot_id: &str,
    ) -> ControlPlaneResult<Option<ObjectManifestRecord>>;

    fn create_work_view(&self, _input: WorkViewCreate) -> ControlPlaneResult<WorkViewRecord> {
        Err(ControlPlaneError::Limited {
            capability: "work-views",
            reason: "work views require the Phase 9 control-plane implementation.",
        })
    }

    fn list_work_views(
        &self,
        _workspace_id: &str,
        _include_all: bool,
    ) -> ControlPlaneResult<Vec<WorkViewRecord>> {
        Err(ControlPlaneError::Limited {
            capability: "work-views",
            reason: "work view listing requires the Phase 9 control-plane implementation.",
        })
    }

    fn update_work_view_lifecycle(
        &self,
        _input: WorkViewLifecycleUpdate,
    ) -> ControlPlaneResult<WorkViewRecord> {
        Err(ControlPlaneError::Limited {
            capability: "work-views",
            reason: "work view lifecycle updates require the Phase 9 control-plane implementation.",
        })
    }

    fn restore_work_view(
        &self,
        _workspace_id: &str,
        _work_view_id: &str,
        _restored_by_device_id: &str,
    ) -> ControlPlaneResult<WorkViewRecord> {
        Err(ControlPlaneError::Limited {
            capability: "work-views",
            reason: "work view restore requires the Phase 9 control-plane implementation.",
        })
    }

    fn commit_work_view_overlay(
        &self,
        _input: WorkViewOverlayCommit,
    ) -> Result<WorkViewRecord, WorkViewUpdateError> {
        Err(WorkViewUpdateError::Unsupported {
            capability: "work-views",
            reason: "work view overlay commits require the Phase 9 control-plane implementation.",
        })
    }

    fn create_lease(&self, _input: LeaseCreate) -> ControlPlaneResult<Lease> {
        Err(ControlPlaneError::Limited {
            capability: "agent-leases",
            reason: "agent lease metadata requires the Phase 10 control-plane implementation.",
        })
    }

    fn update_lease(&self, _input: LeaseUpdate) -> ControlPlaneResult<Lease> {
        Err(ControlPlaneError::Limited {
            capability: "agent-leases",
            reason: "agent lease metadata updates require the Phase 10 control-plane implementation.",
        })
    }

    fn list_leases(&self, _workspace_id: &str) -> ControlPlaneResult<Vec<Lease>> {
        Err(ControlPlaneError::Limited {
            capability: "agent-leases",
            reason: "agent lease listing requires the Phase 10 control-plane implementation.",
        })
    }

    fn create_device_request(&self, input: DeviceRequestInput)
    -> ControlPlaneResult<DeviceRequest>;

    fn create_bootstrap_session(
        &self,
        _input: BootstrapSessionInput,
    ) -> ControlPlaneResult<BootstrapSession> {
        Err(ControlPlaneError::Limited {
            capability: "device-bootstrap",
            reason: "remote bootstrap sessions require the hosted Phase 5 control plane.",
        })
    }

    fn create_first_authorized_device(
        &self,
        _input: FirstAuthorizedDeviceInput,
    ) -> ControlPlaneResult<AuthorizedDeviceRecord> {
        Err(ControlPlaneError::Limited {
            capability: "device-trust",
            reason: "first-device trust roots require the Phase 5 control-plane implementation.",
        })
    }

    fn list_device_trust(
        &self,
        _workspace_id: &str,
    ) -> ControlPlaneResult<DeviceApprovalRequestList> {
        Err(ControlPlaneError::Limited {
            capability: "device-trust",
            reason: "device trust listing requires the Phase 5 control-plane implementation.",
        })
    }

    fn approve_device_request(
        &self,
        _input: DeviceApprovalInput,
    ) -> ControlPlaneResult<DeviceApproval> {
        Err(ControlPlaneError::Limited {
            capability: "device-trust",
            reason: "Phase 4 records pending devices only; real decrypt authority waits for Phase 5.",
        })
    }

    fn deny_device_request(&self, _input: DeviceDenialInput) -> ControlPlaneResult<DeviceDenial> {
        Err(ControlPlaneError::Limited {
            capability: "device-trust",
            reason: "device denial requires the Phase 5 control-plane implementation.",
        })
    }

    fn revoke_device(
        &self,
        _input: DeviceRevocationInput,
    ) -> ControlPlaneResult<RevokedDeviceRecord> {
        Err(ControlPlaneError::Limited {
            capability: "device-trust",
            reason: "device revocation requires the Phase 5 control-plane implementation.",
        })
    }

    fn get_encrypted_device_grant(
        &self,
        _request_id: &str,
        _device_id: &str,
    ) -> ControlPlaneResult<Option<DeviceApproval>> {
        Err(ControlPlaneError::Limited {
            capability: "device-trust",
            reason: "grant fetching requires the Phase 5 control-plane implementation.",
        })
    }

    fn confirm_device_grant_accepted(
        &self,
        _input: GrantAcceptanceInput,
    ) -> ControlPlaneResult<DeviceApproval> {
        Err(ControlPlaneError::Limited {
            capability: "device-trust",
            reason: "grant acceptance requires the Phase 5 control-plane implementation.",
        })
    }

    fn create_recovery_envelope(
        &self,
        _input: RecoveryEnvelopeInput,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        Err(ControlPlaneError::Limited {
            capability: "recovery-key",
            reason: "recovery envelopes require the Phase 5 control-plane implementation.",
        })
    }

    fn verify_recovery_envelope(
        &self,
        _workspace_id: &str,
        _envelope_id: &str,
        _verified_by_device_id: &str,
        _verified_by_device_proof: &str,
        _recovery_proof: &str,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        Err(ControlPlaneError::Limited {
            capability: "recovery-key",
            reason: "recovery verification requires the Phase 5 control-plane implementation.",
        })
    }

    fn rotate_recovery_envelope(
        &self,
        _input: RecoveryEnvelopeInput,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        Err(ControlPlaneError::Limited {
            capability: "recovery-key",
            reason: "recovery rotation requires the Phase 5 control-plane implementation.",
        })
    }

    fn revoke_recovery_envelope(
        &self,
        _workspace_id: &str,
        _envelope_id: &str,
        _revoked_by_device_id: &str,
        _revoked_by_device_proof: &str,
    ) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
        Err(ControlPlaneError::Limited {
            capability: "recovery-key",
            reason: "recovery revocation requires the Phase 5 control-plane implementation.",
        })
    }

    fn list_recovery_envelopes(
        &self,
        _workspace_id: &str,
    ) -> ControlPlaneResult<Vec<RecoveryEnvelopeRecord>> {
        Err(ControlPlaneError::Limited {
            capability: "recovery-key",
            reason: "recovery listing requires the Phase 5 control-plane implementation.",
        })
    }

    fn authorize_device_with_recovery(
        &self,
        _input: RecoveryDeviceAuthorizationInput,
    ) -> ControlPlaneResult<DeviceApproval> {
        Err(ControlPlaneError::Limited {
            capability: "recovery-key",
            reason: "recovery device authorization requires the Phase 5 control-plane implementation.",
        })
    }
}

pub fn is_opaque_object_key(object_key: &str) -> bool {
    bowline_storage::ObjectKey::new(object_key).is_ok()
}

fn validate_object_key(object_key: &str) -> ControlPlaneResult<()> {
    match bowline_storage::ObjectKey::new(object_key) {
        Ok(_) => Ok(()),
        Err(_) => Err(ControlPlaneError::InvalidObjectKey {
            reason: "object keys must be generated opaque pack, manifest, or overlay keys",
        }),
    }
}

fn sanitize_id_part(value: &str) -> String {
    let mut sanitized = String::new();
    let mut last_was_dash = false;

    for character in value.chars() {
        let next = if character.is_ascii_alphanumeric() {
            character.to_ascii_lowercase()
        } else {
            '-'
        };

        if next == '-' {
            if !last_was_dash {
                sanitized.push(next);
            }
            last_was_dash = true;
        } else {
            sanitized.push(next);
            last_was_dash = false;
        }
    }

    sanitized = sanitized.trim_matches('-').to_string();

    if sanitized.is_empty() {
        "id".to_string()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_snapshot() -> WorkspaceStatusSnapshot {
        WorkspaceStatusSnapshot {
            workspace_id: "ws_code".to_string(),
            snapshot_id: "snap_abc123".to_string(),
            status_level: "attention".to_string(),
            attention_items: vec!["device approval pending".to_string()],
            generated_at: "2026-06-29T12:00:00Z".to_string(),
            event_watermarks: StatusEventWatermarks::default(),
            sync_queue: None,
            index: None,
            workspace_summary: None,
            items: Vec::new(),
            limits: Vec::new(),
            published_by_device_id: "device-daemon".to_string(),
        }
    }

    #[test]
    fn status_publish_proof_subject_matches_convex_contract() {
        let snapshot = sample_snapshot();
        assert_eq!(
            snapshot.proof_subject(),
            "workspaceId=ws_code\nsnapshotId=snap_abc123\nstatusLevel=attention\ngeneratedAt=2026-06-29T12:00:00Z"
        );
    }

    #[test]
    fn publish_workspace_status_is_noop_for_in_memory_client() {
        let client = FakeControlPlaneClient::default();
        assert!(client.publish_workspace_status(&sample_snapshot()).is_ok());
    }
}
