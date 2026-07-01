pub(super) use std::{collections::BTreeSet, error::Error, fmt, fs, io, path::PathBuf};

pub(super) use bowline_control_plane::WorkspaceRef;
pub(super) use bowline_core::{
    commands::AgentLease,
    ids::{
        ContentId, DeviceId, EnvRecordId, LeaseId, PackId, ProjectId, SnapshotId, WorkViewId,
        WorkspaceId,
    },
    policy::{AccessFlag, MaterializationMode, PathClassification},
    status::{ComponentState, EventWatermarks, NetworkState, ObservedWorkspaceSummary},
    work_views::{
        WorkView, WorkViewLifecycle, WorkViewRetention, WorkViewRetentionState, WorkViewSyncState,
        WorkViewVisibility,
    },
    workspace_graph::{
        ContentLocator, ContentStorage, HydrationState, NamespaceEntryKind,
        normalize_workspace_path,
    },
};
pub(super) use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};

pub(super) use super::schema::{
    CURRENT_SCHEMA_VERSION, SCHEMA_CORE, SCHEMA_ENV_SETUP_INDEXES, SCHEMA_INDEXING,
    SCHEMA_MATERIALIZATION, SCHEMA_WORK_VIEWS, TABLES,
};

mod agents_idempotency;
mod common;
mod env_setup;
mod indexing;
mod local_writes;
mod schema_open;
mod status;
mod sync;
#[cfg(test)]
mod tests;
mod work_views;
mod workspace_ops;

#[derive(Debug)]
pub struct MetadataStore {
    connection: Connection,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalPathRecord {
    pub path: String,
    pub classification: PathClassification,
    pub mode: MaterializationMode,
    pub access: Vec<AccessFlag>,
    pub matched_rule: String,
    pub rule_source: String,
    pub risk: String,
    pub summary: String,
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
    pub encrypted_locator_json: String,
    pub format_json: String,
    pub materialization_state: String,
    pub restriction_state: String,
    pub key_epoch: u32,
    pub metadata_json: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
    pub receipt_json: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedNodeRecord {
    pub workspace_id: WorkspaceId,
    pub node_id: String,
    pub project_id: Option<ProjectId>,
    pub parent_node_id: Option<String>,
    pub path: String,
    pub kind: NamespaceEntryKind,
    pub content_id: Option<ContentId>,
    pub hydration_state: HydrationState,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HydrationQueueRecord {
    pub id: String,
    pub workspace_id: WorkspaceId,
    pub project_id: Option<ProjectId>,
    pub path: String,
    pub content_id: Option<ContentId>,
    pub priority: String,
    pub state: String,
    pub cause: String,
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
pub struct WorkspaceSyncHeadRecord {
    pub workspace_ref: WorkspaceRef,
    pub observed_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncOperationRecord {
    pub id: String,
    pub workspace_id: WorkspaceId,
    pub kind: String,
    pub state: String,
    pub idempotency_key: String,
    pub base_version: Option<u64>,
    pub base_snapshot_id: Option<String>,
    pub target_snapshot_id: Option<String>,
    pub device_id: Option<DeviceId>,
    pub payload_json: String,
    pub attempt_count: u32,
    pub claimed_by: Option<String>,
    pub heartbeat_at: Option<String>,
    pub next_attempt_at: Option<String>,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandIdempotencyRecord {
    pub workspace_id: WorkspaceId,
    pub idempotency_key: String,
    pub command: String,
    pub request_hash: String,
    pub result_json: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SyncOperationCounts {
    pub queued: u64,
    pub claimed: u64,
    pub waiting_retry: u64,
    pub blocked_offline: u64,
    pub attention: u64,
    pub completed: u64,
}

pub type WorkViewRecord = WorkView;
pub type AgentLeaseRecord = AgentLease;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedLocalPath {
    pub project_id: Option<ProjectId>,
    pub path: String,
    pub classification: PathClassification,
    pub mode: MaterializationMode,
    pub access: Vec<AccessFlag>,
    pub matched_rule: String,
    pub rule_source: String,
    pub risk: String,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDocumentRecord {
    pub workspace_id: WorkspaceId,
    pub project_id: Option<ProjectId>,
    pub path: String,
    pub snapshot_id: Option<SnapshotId>,
    pub content_id: Option<ContentId>,
    pub classification: PathClassification,
    pub mode: MaterializationMode,
    pub access: Vec<AccessFlag>,
    pub policy_summary: String,
    pub body_text: String,
    pub hydration_state: HydrationState,
    pub indexed_bytes: u64,
    pub source_watermark: u64,
    pub indexed_watermark: u64,
    pub state: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolIndexRecord {
    pub id: String,
    pub workspace_id: WorkspaceId,
    pub project_id: Option<ProjectId>,
    pub path: String,
    pub snapshot_id: Option<SnapshotId>,
    pub name: String,
    pub kind: String,
    pub language: String,
    pub line_start: u64,
    pub line_end: u64,
    pub byte_start: u64,
    pub byte_end: u64,
    pub parser_status: String,
    pub access: Vec<AccessFlag>,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexWorkRecord {
    pub id: String,
    pub workspace_id: WorkspaceId,
    pub project_id: Option<ProjectId>,
    pub path: Option<String>,
    pub kind: String,
    pub source_watermark: u64,
    pub indexed_watermark: u64,
    pub state: String,
    pub reason: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexPackRecord {
    pub workspace_id: WorkspaceId,
    pub project_id: Option<ProjectId>,
    pub snapshot_id: Option<SnapshotId>,
    pub object_key: String,
    pub byte_len: u64,
    pub hash: String,
    pub state: String,
    pub updated_at: String,
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
    FutureIncompatible { found: u32, supported: u32 },
    UnsupportedSchema,
}

impl fmt::Display for MetadataError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "metadata I/O failed: {error}"),
            Self::Sqlite(error) => write!(formatter, "metadata SQLite failed: {error}"),
            Self::InvalidStorageMetadata(reason) => {
                write!(formatter, "invalid storage metadata: {reason}")
            }
            Self::FutureIncompatible { found, supported } => write!(
                formatter,
                "metadata schema version {found} is newer than supported version {supported}"
            ),
            Self::UnsupportedSchema => write!(
                formatter,
                "metadata database uses an unsupported schema; remove the local metadata database and re-run bowline login"
            ),
        }
    }
}

impl Error for MetadataError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Sqlite(error) => Some(error),
            Self::InvalidStorageMetadata(_) => None,
            Self::FutureIncompatible { .. } => None,
            Self::UnsupportedSchema => None,
        }
    }
}

impl From<io::Error> for MetadataError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for MetadataError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}
