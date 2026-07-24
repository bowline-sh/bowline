pub(super) use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::PathBuf,
};

pub(super) use bowline_core::{
    ids::{DeviceId, EnvRecordId, ProjectId, SnapshotId, WorkViewId, WorkspaceId},
    policy::{AccessFlag, MaterializationMode, PathClassification},
    status::{EventWatermarks, GitObserverState, ObservedWorkspaceSummary},
    work_views::{
        WorkView, WorkViewLifecycle, WorkViewRetention, WorkViewRetentionState, WorkViewSyncState,
        WorkViewVisibility,
    },
    workspace_graph::normalize_workspace_path,
};
pub(super) use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

pub(super) use super::schema::{CURRENT_SCHEMA_BATCHES, CURRENT_SCHEMA_VERSION, TABLES};

mod common;
mod env_setup;
mod error;
mod observed_paths;
mod schema_open;
mod status;
#[cfg(test)]
pub(crate) mod tests;
mod work_views;
mod workspace_ops;

pub(crate) use env_setup::EnvRecordSourceReplacement;

#[derive(Debug)]
pub struct MetadataStore {
    connection: Connection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MetadataReadRole {
    SchemaInspection,
    StatusProjection,
}

pub(crate) enum ClassifiedTransactionError<E> {
    BeforeCommit(E),
    CommitAcknowledgement(MetadataError),
}

impl MetadataStore {
    pub(crate) const STATUS_PROJECTION_READER: MetadataReadRole =
        MetadataReadRole::StatusProjection;

    pub(crate) fn open_read_only(
        path: impl Into<PathBuf>,
        role: MetadataReadRole,
    ) -> Result<Self, MetadataError> {
        let connection = Connection::open_with_flags(
            path.into(),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        common::configure_read_only_connection(&connection, role)?;
        Ok(Self { connection })
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

pub type WorkViewRecord = WorkView;

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
