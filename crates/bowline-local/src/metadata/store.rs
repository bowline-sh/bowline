pub(super) use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::PathBuf,
};

pub(super) use bowline_control_plane::WorkspaceRef as ControlPlaneWorkspaceRef;
pub(super) use bowline_core::{
    commands::AgentLease,
    ids::{
        ContentId, ContentLayoutId, DeviceId, EnvRecordId, LeaseId, ManifestDigest,
        NamespacePageId, PackId, ProjectId, SnapshotId, WorkViewId, WorkspaceId,
    },
    policy::{AccessFlag, MaterializationMode, PathClassification},
    status::{
        ComponentState, EventWatermarks, GitObserverState, NetworkState, ObservedWorkspaceSummary,
    },
    work_views::{
        WorkView, WorkViewLifecycle, WorkViewRetention, WorkViewRetentionState, WorkViewSyncState,
        WorkViewVisibility,
    },
    workspace_graph::{
        ContentLocator, ContentStorage, FileExecutability, HydrationState, NamespaceEntryKind,
        SnapshotKind, WorkspaceRef, WorkspaceRelativePath, normalize_workspace_path,
    },
};
pub(super) use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};

pub(super) use super::schema::{CURRENT_SCHEMA_BATCHES, CURRENT_SCHEMA_VERSION, TABLES};

mod agents_idempotency;
mod common;
mod current_namespace;
mod env_setup;
mod error;
mod indexing;
mod local_writes;
mod materialization;
mod materialization_model;
mod mcp_tokens;
mod merge_plugins;
mod metadata_gc;
mod metadata_objects;
mod operations;
mod preparation;
mod schema_open;
mod snapshot_pins;
mod snapshot_retention;
mod snapshot_roots;
mod stat_cache;
mod status;
mod sync;
#[cfg(test)]
mod tests;
mod work_view_accept_model;
mod work_view_accept_operations;
mod work_views;
mod workspace_ops;

pub use current_namespace::*;
pub use materialization_model::{
    MATERIALIZATION_TASK_HEARTBEAT_SECONDS, MATERIALIZATION_TASK_LEASE_SECONDS,
    MaterializationFailureKind,
};
pub use metadata_gc::*;
pub use metadata_objects::*;
pub use operations::{SyncOperationKind, SyncResourceKey};
pub use preparation::{
    OwnedStagedPath, PreparationLeaseId, PreparationLeaseRecord, PreparationLeaseState,
    PreparationOrphanRecord, PreparationOwnerMarker, PreparedStagedContentRecord,
    SourceFingerprint,
};
pub use snapshot_pins::*;
pub use snapshot_retention::*;
pub use snapshot_roots::*;
pub use sync::{LocalMetadataPruneReport, LocalMetadataRetentionPolicy};
pub use work_view_accept_model::*;

#[derive(Debug)]
pub struct MetadataStore {
    connection: Connection,
}

impl MetadataStore {
    pub(crate) fn open_read_only(path: impl Into<PathBuf>) -> Result<Self, MetadataError> {
        let connection = Connection::open_with_flags(
            path.into(),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        Ok(Self { connection })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostCommitSyncComponent {
    ConflictResolutionPublication,
    WorkViewOverlaySync,
}

impl PostCommitSyncComponent {
    pub(crate) const ALL: [Self; 2] = [
        Self::ConflictResolutionPublication,
        Self::WorkViewOverlaySync,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ConflictResolutionPublication => "sync.post_commit.conflicts",
            Self::WorkViewOverlaySync => "sync.post_commit.overlays",
        }
    }
}

pub fn all_accepted_roots(store: &MetadataStore) -> Result<Vec<String>, MetadataError> {
    let mut statement = store.connection.prepare(
        "SELECT DISTINCT accepted_path FROM roots
         WHERE state = 'accepted'
         ORDER BY length(accepted_path) DESC, accepted_path",
    )?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRecord {
    pub id: WorkspaceId,
    pub display_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRecord {
    pub id: ProjectId,
    pub path: String,
    pub lifecycle_state: ProjectLifecycleState,
    pub local_materialization_state: ProjectLocalMaterializationState,
    pub purge_after: Option<String>,
    pub git_observer_state: GitObserverState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProjectLifecycleState {
    Active,
    Archived,
    PurgePending,
    Purged,
}

impl ProjectLifecycleState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Archived => "archived",
            Self::PurgePending => "purge-pending",
            Self::Purged => "purged",
        }
    }

    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "archived" => Some(Self::Archived),
            "purge-pending" => Some(Self::PurgePending),
            "purged" => Some(Self::Purged),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProjectLocalMaterializationState {
    Materialized,
    Forgotten,
}

impl ProjectLocalMaterializationState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Materialized => "materialized",
            Self::Forgotten => "forgotten",
        }
    }

    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "materialized" => Some(Self::Materialized),
            "forgotten" => Some(Self::Forgotten),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectUpsert {
    pub id: ProjectId,
    pub path: String,
    pub git_observer_state: GitObserverState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalPathRecord {
    pub path: String,
    pub classification: PathClassification,
    pub mode: MaterializationMode,
    pub access: Vec<AccessFlag>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackRecord {
    pub id: PackId,
    pub workspace_id: WorkspaceId,
    pub kind: String,
    pub byte_len: u64,
    pub object_hash: String,
    pub key_epoch: u32,
    pub state: String,
    pub retain_until: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredContentLocator {
    pub workspace_id: WorkspaceId,
    pub locator: ContentLocator,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotRecord {
    pub id: SnapshotId,
    pub workspace_id: WorkspaceId,
    pub project_id: Option<ProjectId>,
    pub kind: SnapshotKind,
    pub base_snapshot_id: Option<SnapshotId>,
    pub root_id: NamespacePageId,
    pub semantic_manifest_digest: ManifestDigest,
    pub entry_count: u64,
    pub refs: Vec<WorkspaceRef>,
    pub created_at: String,
}

impl SnapshotRecord {
    pub(crate) fn has_same_immutable_binding(&self, other: &Self) -> bool {
        self.id == other.id
            && self.workspace_id == other.workspace_id
            && self.project_id == other.project_id
            && self.kind == other.kind
            && self.base_snapshot_id == other.base_snapshot_id
            && self.root_id == other.root_id
            && self.semantic_manifest_digest == other.semantic_manifest_digest
            && self.entry_count == other.entry_count
            && self.refs == other.refs
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvRecord {
    pub id: EnvRecordId,
    pub workspace_id: WorkspaceId,
    pub project_id: Option<ProjectId>,
    pub source_path: String,
    pub profile: String,
    pub key_name: String,
    pub occurrence_index: u32,
    pub line_kind: String,
    pub access: Vec<AccessFlag>,
    pub value_ciphertext_ref: Option<String>,
    pub encrypted_locator_json: String,
    pub format_json: String,
    pub materialization_state: String,
    pub restriction_state: String,
    pub key_epoch: u32,
    pub metadata_json: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SetupReceiptRecord {
    pub id: String,
    pub workspace_id: WorkspaceId,
    pub project_id: Option<ProjectId>,
    pub command: String,
    pub state: String,
    pub recipe_hash: String,
    pub approval_state: String,
    pub trigger: String,
    pub cwd: String,
    pub os: String,
    pub arch: String,
    pub env_profile: String,
    pub output_path: Option<String>,
    pub redacted_summary: String,
    pub setup_identity_hash: String,
    pub readiness_state: String,
    pub readiness_reason: String,
    pub readiness_remedy: String,
    pub receipt_json: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalWriteLogRecord {
    pub id: String,
    pub workspace_id: WorkspaceId,
    pub device_id: DeviceId,
    pub project_id: Option<ProjectId>,
    pub path: String,
    pub source_path: Option<String>,
    pub operation: String,
    pub staged_content_id: Option<ContentId>,
    pub policy_classification: PathClassification,
    pub causation_id: String,
    pub settled_at: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMcpTokenRecord {
    pub id: String,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub lease_id: LeaseId,
    pub token_hash: String,
    pub token_file: String,
    pub grants_json: String,
    pub expires_at: String,
    pub revoked_at: Option<String>,
    pub created_at: String,
    pub last_used_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSyncHeadRecord {
    pub workspace_ref: ControlPlaneWorkspaceRef,
    pub observed_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncOperationRecord {
    pub id: String,
    pub workspace_id: WorkspaceId,
    pub kind: SyncOperationKind,
    pub resource_key: SyncResourceKey,
    pub state: SyncOperationState,
    pub idempotency_key: String,
    pub base_version: Option<u64>,
    pub base_snapshot_id: Option<String>,
    pub target_snapshot_id: Option<String>,
    pub device_id: Option<DeviceId>,
    pub payload_json: String,
    pub attempt_count: u32,
    pub claimed_by: Option<String>,
    pub claim_generation: u64,
    pub heartbeat_at: Option<String>,
    pub lease_expires_at: Option<String>,
    pub cancellation_requested_at: Option<String>,
    pub next_attempt_at: Option<String>,
    pub result_json: Option<String>,
    pub last_error_code: Option<String>,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncClaimToken(String);

impl SyncClaimToken {
    fn random() -> Result<Self, MetadataError> {
        Ok(Self(common::random_hex_token("sync claim")?))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncClaimHandle {
    operation_id: String,
    owner: String,
    token: SyncClaimToken,
    generation: u64,
    claimed_from_state: SyncOperationState,
}

impl SyncClaimHandle {
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn token(&self) -> &SyncClaimToken {
        &self.token
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn claimed_from_state(&self) -> SyncOperationState {
        self.claimed_from_state
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedSyncOperation {
    pub operation: SyncOperationRecord,
    pub claim: SyncClaimHandle,
    pub lease_expires_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncClaimTransition {
    Applied,
    OwnershipLost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncClaimCheck {
    Owned,
    CancellationRequested,
    OwnershipLost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncCancellationOutcome {
    Requested,
    Cancelled,
    AlreadyCompleted,
    AlreadyCancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncOperationEnqueueOutcome {
    Inserted(SyncOperationRecord),
    Existing(SyncOperationRecord),
}

impl SyncOperationEnqueueOutcome {
    pub fn operation(&self) -> &SyncOperationRecord {
        match self {
            Self::Inserted(operation) | Self::Existing(operation) => operation,
        }
    }

    pub fn inserted(&self) -> bool {
        matches!(self, Self::Inserted(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncCommittedCancelledLateResult {
    outcome: &'static str,
    operation_kind: SyncOperationKind,
    committed_result: serde_json::Value,
}

impl SyncCommittedCancelledLateResult {
    pub fn new(operation_kind: SyncOperationKind, committed_result: serde_json::Value) -> Self {
        Self {
            outcome: "committed-cancelled-late",
            operation_kind,
            committed_result,
        }
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct MaterializationTaskId(String);

impl MaterializationTaskId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum MaterializationPriorityClass {
    CorrectnessCritical,
    ActiveProject,
    RequestedPath,
    RecentProject,
    SmallFile,
    BackgroundLarge,
    Cleanup,
}

impl MaterializationPriorityClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CorrectnessCritical => "correctness-critical",
            Self::ActiveProject => "active-project",
            Self::RequestedPath => "requested-path",
            Self::RecentProject => "recent-project",
            Self::SmallFile => "small-file",
            Self::BackgroundLarge => "background-large",
            Self::Cleanup => "cleanup",
        }
    }

    fn from_wire(value: &str) -> Option<Self> {
        match value {
            "correctness-critical" => Some(Self::CorrectnessCritical),
            "active-project" => Some(Self::ActiveProject),
            "requested-path" => Some(Self::RequestedPath),
            "recent-project" => Some(Self::RecentProject),
            "small-file" => Some(Self::SmallFile),
            "background-large" => Some(Self::BackgroundLarge),
            "cleanup" => Some(Self::Cleanup),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MaterializationTaskState {
    Queued,
    Claimed,
    Staged,
    WaitingRetry,
    BlockedOffline,
    BlockedMissing,
    BlockedConflict,
    Attention,
    Ready,
    Cancelled,
}

impl MaterializationTaskState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Claimed => "claimed",
            Self::Staged => "staged",
            Self::WaitingRetry => "waiting-retry",
            Self::BlockedOffline => "blocked-offline",
            Self::BlockedMissing => "blocked-missing",
            Self::BlockedConflict => "blocked-conflict",
            Self::Attention => "attention",
            Self::Ready => "ready",
            Self::Cancelled => "cancelled",
        }
    }

    fn from_wire(value: &str) -> Option<Self> {
        match value {
            "queued" => Some(Self::Queued),
            "claimed" => Some(Self::Claimed),
            "staged" => Some(Self::Staged),
            "waiting-retry" => Some(Self::WaitingRetry),
            "blocked-offline" => Some(Self::BlockedOffline),
            "blocked-missing" => Some(Self::BlockedMissing),
            "blocked-conflict" => Some(Self::BlockedConflict),
            "attention" => Some(Self::Attention),
            "ready" => Some(Self::Ready),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializationTaskRecord {
    pub id: MaterializationTaskId,
    pub workspace_id: WorkspaceId,
    pub project_id: Option<ProjectId>,
    pub snapshot_id: SnapshotId,
    pub path: String,
    pub expected_kind: NamespaceEntryKind,
    pub expected_content_id: Option<ContentId>,
    pub expected_byte_len: u64,
    pub expected_executable: bool,
    pub priority_class: MaterializationPriorityClass,
    pub state: MaterializationTaskState,
    pub attempt_count: u32,
    pub claim_generation: u64,
    pub not_before: Option<String>,
    pub claim_token: Option<String>,
    pub claimed_by: Option<String>,
    pub claimed_at: Option<String>,
    pub lease_expires_at: Option<String>,
    pub last_error_kind: Option<MaterializationFailureKind>,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug)]
pub struct MaterializationTaskFinish<'a> {
    pub id: &'a MaterializationTaskId,
    pub claim_token: &'a str,
    pub claim_generation: u64,
    pub state: MaterializationTaskState,
    pub error_kind: Option<MaterializationFailureKind>,
    pub error: Option<&'a str>,
    pub not_before: Option<&'a str>,
    pub now: &'a str,
}

#[derive(Debug, Clone, Copy)]
pub struct MaterializationTaskFence<'a> {
    pub id: &'a MaterializationTaskId,
    pub claim_token: &'a str,
    pub claim_generation: u64,
    pub snapshot_id: &'a SnapshotId,
    pub path: &'a str,
    pub expected_kind: NamespaceEntryKind,
    pub expected_content_id: Option<&'a ContentId>,
    pub settled_write_matches_base: bool,
    pub unresolved_conflict_paths: &'a BTreeSet<String>,
    pub now: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MaterializationPathState {
    NeedsObservation,
    Queued,
    Materializing,
    BlockedOffline,
    BlockedMissing,
    BlockedConflict,
    Attention,
    Ready,
    Excluded,
}

impl MaterializationPathState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NeedsObservation => "needs-observation",
            Self::Queued => "queued",
            Self::Materializing => "materializing",
            Self::BlockedOffline => "blocked-offline",
            Self::BlockedMissing => "blocked-missing",
            Self::BlockedConflict => "blocked-conflict",
            Self::Attention => "attention",
            Self::Ready => "ready",
            Self::Excluded => "excluded",
        }
    }

    fn from_wire(value: &str) -> Option<Self> {
        match value {
            "needs-observation" => Some(Self::NeedsObservation),
            "queued" => Some(Self::Queued),
            "materializing" => Some(Self::Materializing),
            "blocked-offline" => Some(Self::BlockedOffline),
            "blocked-missing" => Some(Self::BlockedMissing),
            "blocked-conflict" => Some(Self::BlockedConflict),
            "attention" => Some(Self::Attention),
            "ready" => Some(Self::Ready),
            "excluded" => Some(Self::Excluded),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializationPathStateRecord {
    pub workspace_id: WorkspaceId,
    pub project_id: Option<ProjectId>,
    pub path: String,
    pub snapshot_id: Option<SnapshotId>,
    pub expected_content_id: Option<ContentId>,
    pub state: MaterializationPathState,
    pub observed_content_id: Option<ContentId>,
    pub observed_byte_len: Option<u64>,
    pub source_hydration_state: Option<String>,
    pub verified_at: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MaterializationReconcileReport {
    pub inserted: u64,
    pub reactivated: u64,
    pub reprioritized: u64,
    pub cancelled: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncOperationState {
    Queued,
    Claimed,
    WaitingRetry,
    BlockedOffline,
    ReconciliationRequired,
    Attention,
    Completed,
    Cancelled,
}

impl SyncOperationState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Claimed => "claimed",
            Self::WaitingRetry => "waiting_retry",
            Self::BlockedOffline => "blocked_offline",
            Self::ReconciliationRequired => "reconciliation_required",
            Self::Attention => "attention",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncOperationCheckpointRecord {
    pub id: String,
    pub workspace_id: WorkspaceId,
    pub operation_id: String,
    pub step: String,
    pub state: String,
    pub payload_json: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRefCursorRecord {
    pub workspace_id: WorkspaceId,
    pub cursor: Option<String>,
    pub last_observed_version: Option<u64>,
    pub last_observed_snapshot_id: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SyncOperationCounts {
    pub queued: u64,
    pub claimed: u64,
    pub waiting_retry: u64,
    pub blocked_offline: u64,
    pub reconciliation_required: u64,
    pub attention: u64,
    pub completed: u64,
    pub cancelled: u64,
}

pub type WorkViewRecord = WorkView;
pub type AgentLeaseRecord = AgentLease;

pub const WORK_VIEW_BASE_DESCRIPTOR_VERSION: u16 = 2;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkViewBaseDescriptor {
    pub format_version: u16,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub work_view_id: WorkViewId,
    pub base_snapshot_id: SnapshotId,
    pub project_prefix: String,
    pub policy_fingerprint: String,
    pub exposed_snapshot_id: SnapshotId,
    pub exposed_namespace_root_id: NamespacePageId,
    pub exposed_semantic_manifest_digest: ManifestDigest,
    pub exposed_entry_count: u64,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkViewBaseState {
    Authoritative {
        descriptor: Box<WorkViewBaseDescriptor>,
    },
    LegacyUnverifiable,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedLocalPath {
    pub project_id: Option<ProjectId>,
    pub path: String,
    pub classification: PathClassification,
    pub mode: MaterializationMode,
    pub access: Vec<AccessFlag>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseInspection {
    pub state: DatabaseState,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DatabaseState {
    Missing,
    Empty,
    Current,
    FutureIncompatible { found: u32, supported: u32 },
    UnsupportedSchema,
    Corrupt,
    Locked,
    PermissionDenied,
}

#[derive(Debug)]
pub enum MetadataError {
    Io(io::Error),
    Sqlite(rusqlite::Error),
    InvalidStorageMetadata(String),
    InvalidCurrentNamespaceProjection {
        field: &'static str,
        reason: &'static str,
    },
    ImmutableBindingConflict {
        logical_id: String,
        field: &'static str,
    },
    IncompleteSnapshotRoot {
        snapshot_id: SnapshotId,
        logical_id: String,
    },
    FutureIncompatible {
        found: u32,
        supported: u32,
    },
    UnsupportedSchema,
}
