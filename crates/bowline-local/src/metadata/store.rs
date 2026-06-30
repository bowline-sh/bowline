use std::{collections::BTreeSet, error::Error, fmt, fs, io, path::PathBuf};

use bowline_control_plane::WorkspaceRef;
use bowline_core::{
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
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};

use super::schema::{
    CURRENT_SCHEMA_VERSION, SCHEMA_CORE, SCHEMA_ENV_SETUP_INDEXES, SCHEMA_INDEXING,
    SCHEMA_MATERIALIZATION, SCHEMA_WORK_VIEWS, TABLES,
};

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

impl MetadataStore {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, MetadataError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let connection = Connection::open(&path)?;
        configure_connection(&connection)?;
        initialize_schema(&connection)?;

        Ok(Self { connection })
    }

    pub fn inspect(path: impl Into<PathBuf>) -> DatabaseInspection {
        let path = path.into();
        if !path.exists() {
            return DatabaseInspection {
                state: DatabaseState::Missing,
                path,
            };
        }

        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return DatabaseInspection {
                    state: DatabaseState::Missing,
                    path,
                };
            }
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                return DatabaseInspection {
                    state: DatabaseState::PermissionDenied,
                    path,
                };
            }
            Err(_) => {
                return DatabaseInspection {
                    state: DatabaseState::Corrupt,
                    path,
                };
            }
        };

        if metadata.len() == 0 {
            return DatabaseInspection {
                state: DatabaseState::Empty,
                path,
            };
        }

        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let state = match Connection::open_with_flags(&path, flags) {
            Ok(connection) => inspect_open_connection(&connection),
            Err(error) => classify_open_error(&error),
        };

        DatabaseInspection { state, path }
    }

    pub fn journal_mode(&self) -> Result<String, MetadataError> {
        Ok(self
            .connection
            .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))?)
    }

    pub fn has_table(&self, table: &str) -> Result<bool, MetadataError> {
        Ok(self
            .connection
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn assert_schema_tables(&self) -> Result<(), MetadataError> {
        for table in TABLES {
            if !self.has_table(table)? {
                return Err(MetadataError::Sqlite(rusqlite::Error::InvalidQuery));
            }
        }
        Ok(())
    }

    pub fn insert_workspace(
        &self,
        id: &WorkspaceId,
        display_name: &str,
        now: &str,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "INSERT INTO workspaces (id, display_name, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(id) DO UPDATE SET
               display_name = excluded.display_name,
               updated_at = excluded.updated_at",
            params![id.as_str(), display_name, now],
        )?;
        Ok(())
    }

    pub fn insert_root(
        &self,
        id: &str,
        workspace_id: &WorkspaceId,
        accepted_path: &str,
        now: &str,
    ) -> Result<(), MetadataError> {
        let existing_workspace = self
            .connection
            .query_row(
                "SELECT workspace_id FROM roots WHERE id = ?1",
                [id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if existing_workspace
            .as_deref()
            .is_some_and(|owner| owner != workspace_id.as_str())
        {
            return Err(MetadataError::InvalidStorageMetadata(format!(
                "root id `{id}` already belongs to another workspace"
            )));
        }
        self.connection.execute(
            "INSERT INTO roots
             (id, workspace_id, accepted_path, state, materialization_state, created_at)
             VALUES (?1, ?2, ?3, 'accepted', 'ready', ?4)
             ON CONFLICT(id) DO UPDATE SET
               workspace_id = excluded.workspace_id,
               accepted_path = excluded.accepted_path,
               state = excluded.state,
               materialization_state = excluded.materialization_state",
            params![id, workspace_id.as_str(), accepted_path, now],
        )?;
        Ok(())
    }

    pub fn insert_project(
        &self,
        id: &ProjectId,
        workspace_id: &WorkspaceId,
        root_id: &str,
        path: &str,
        now: &str,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "INSERT INTO projects
             (id, workspace_id, root_id, path, hot_state, latest_snapshot_id, created_at)
             VALUES (?1, ?2, ?3, ?4, 'cold', NULL, ?5)
	             ON CONFLICT(id) DO UPDATE SET
	               workspace_id = excluded.workspace_id,
	               root_id = excluded.root_id,
	               path = excluded.path,
	               latest_snapshot_id = excluded.latest_snapshot_id",
            params![id.as_str(), workspace_id.as_str(), root_id, path, now],
        )?;
        Ok(())
    }

    pub fn replace_projects(
        &mut self,
        workspace_id: &WorkspaceId,
        root_id: &str,
        projects: &[(ProjectId, String)],
        now: &str,
    ) -> Result<(), MetadataError> {
        let transaction = self.connection.transaction()?;
        for (id, _) in projects {
            let existing_workspace = transaction
                .query_row(
                    "SELECT workspace_id FROM projects WHERE id = ?1",
                    [id.as_str()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if existing_workspace
                .as_deref()
                .is_some_and(|owner| owner != workspace_id.as_str())
            {
                return Err(MetadataError::InvalidStorageMetadata(format!(
                    "project id `{}` already belongs to another workspace",
                    id.as_str()
                )));
            }
        }
        let mut statement = transaction.prepare(
            "INSERT INTO projects
             (id, workspace_id, root_id, path, hot_state, latest_snapshot_id, created_at)
             VALUES (?1, ?2, ?3, ?4, 'cold', NULL, ?5)
	             ON CONFLICT(id) DO UPDATE SET
	               workspace_id = excluded.workspace_id,
	               root_id = excluded.root_id,
	               path = excluded.path",
        )?;
        for (id, path) in projects {
            statement.execute(params![
                id.as_str(),
                workspace_id.as_str(),
                root_id,
                path,
                now
            ])?;
        }
        drop(statement);

        let retained_ids = projects
            .iter()
            .map(|(id, _)| id.as_str().to_string())
            .collect::<BTreeSet<_>>();
        let mut statement = transaction.prepare(
            "SELECT id FROM projects
             WHERE workspace_id = ?1",
        )?;
        let stale_ids = statement
            .query_map([workspace_id.as_str()], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|id| !retained_ids.contains(id))
            .collect::<Vec<_>>();
        drop(statement);
        for id in stale_ids {
            transaction.execute("DELETE FROM namespace_entries WHERE project_id = ?1", [&id])?;
            transaction.execute("DELETE FROM index_documents WHERE project_id = ?1", [&id])?;
            transaction.execute("DELETE FROM index_packs WHERE project_id = ?1", [&id])?;
            transaction.execute("DELETE FROM index_work WHERE project_id = ?1", [&id])?;
            transaction.execute("DELETE FROM symbol_records WHERE project_id = ?1", [&id])?;
            transaction.execute("DELETE FROM work_views WHERE project_id = ?1", [&id])?;
            transaction.execute("DELETE FROM projects WHERE id = ?1", [id])?;
        }
        transaction.commit()?;

        Ok(())
    }

    pub fn current_workspace(&self) -> Result<Option<WorkspaceRecord>, MetadataError> {
        self.connection
            .query_row(
                "SELECT id, display_name FROM workspaces
                 ORDER BY (
                     SELECT MAX(created_at) FROM roots
                     WHERE roots.workspace_id = workspaces.id
                       AND roots.state = 'accepted'
                 ) IS NOT NULL DESC,
                 (
                     SELECT MAX(created_at) FROM roots
                     WHERE roots.workspace_id = workspaces.id
                       AND roots.state = 'accepted'
                 ) DESC,
                 created_at DESC,
                 id DESC
                 LIMIT 1",
                [],
                |row| {
                    Ok(WorkspaceRecord {
                        id: WorkspaceId::new(row.get::<_, String>(0)?),
                        display_name: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn current_project_by_path(
        &self,
        path: &str,
    ) -> Result<Option<ProjectRecord>, MetadataError> {
        let Some(workspace) = self.current_workspace()? else {
            return Ok(None);
        };
        let path = self.workspace_relative_path(&workspace.id, path)?;

        for candidate in project_path_candidates(&path) {
            let project = self
                .connection
                .query_row(
                    "SELECT id, path FROM projects
                     WHERE workspace_id = ?1 AND path = ?2
                     LIMIT 1",
                    params![workspace.id.as_str(), candidate],
                    |row| {
                        Ok(ProjectRecord {
                            id: ProjectId::new(row.get::<_, String>(0)?),
                            path: row.get(1)?,
                        })
                    },
                )
                .optional()?;
            if project.is_some() {
                return Ok(project);
            }
        }

        Ok(None)
    }

    pub fn project_by_id(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
    ) -> Result<Option<ProjectRecord>, MetadataError> {
        self.connection
            .query_row(
                "SELECT id, path FROM projects
                 WHERE workspace_id = ?1 AND id = ?2
                 LIMIT 1",
                params![workspace_id.as_str(), project_id.as_str()],
                |row| {
                    Ok(ProjectRecord {
                        id: ProjectId::new(row.get::<_, String>(0)?),
                        path: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn project_latest_snapshot_id(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
    ) -> Result<Option<SnapshotId>, MetadataError> {
        self.connection
            .query_row(
                "SELECT latest_snapshot_id FROM projects
                 WHERE workspace_id = ?1 AND id = ?2
                 LIMIT 1",
                params![workspace_id.as_str(), project_id.as_str()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map(|value| value.flatten().map(SnapshotId::new))
            .map_err(Into::into)
    }

    pub fn set_project_latest_snapshot_id(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
        snapshot_id: &SnapshotId,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "UPDATE projects
             SET latest_snapshot_id = ?3
             WHERE workspace_id = ?1 AND id = ?2",
            params![
                workspace_id.as_str(),
                project_id.as_str(),
                snapshot_id.as_str()
            ],
        )?;
        Ok(())
    }

    pub fn current_workspace_root(&self) -> Result<Option<String>, MetadataError> {
        let Some(workspace) = self.current_workspace()? else {
            return Ok(None);
        };

        self.connection
            .query_row(
                "SELECT accepted_path FROM roots
                 WHERE workspace_id = ?1 AND state = 'accepted'
                 ORDER BY created_at, id
                 LIMIT 1",
                [workspace.id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn projects(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<ProjectRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT id, path FROM projects
             WHERE workspace_id = ?1
             ORDER BY path, id",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], |row| {
            Ok(ProjectRecord {
                id: ProjectId::new(row.get::<_, String>(0)?),
                path: row.get(1)?,
            })
        })?;

        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn project_count(&self, workspace_id: &WorkspaceId) -> Result<u64, MetadataError> {
        self.connection
            .query_row(
                "SELECT count(*) FROM projects WHERE workspace_id = ?1",
                [workspace_id.as_str()],
                |row| row.get::<_, u64>(0),
            )
            .map_err(Into::into)
    }

    pub fn accepted_root_count(&self, workspace_id: &WorkspaceId) -> Result<u64, MetadataError> {
        self.connection
            .query_row(
                "SELECT count(*) FROM roots WHERE workspace_id = ?1 AND state = 'accepted'",
                [workspace_id.as_str()],
                |row| row.get::<_, u64>(0),
            )
            .map_err(Into::into)
    }

    pub fn replace_observed_paths(
        &mut self,
        workspace_id: &WorkspaceId,
        paths: &[ObservedLocalPath],
        now: &str,
    ) -> Result<(), MetadataError> {
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "DELETE FROM local_paths WHERE workspace_id = ?1",
            [workspace_id.as_str()],
        )?;

        let mut statement = transaction.prepare(
            "INSERT INTO local_paths
             (id, workspace_id, project_id, path, classification, mode, access_json,
              matched_rule, rule_source, risk, summary, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        )?;
        for path in paths {
            let id = local_path_id(workspace_id, &path.path);
            statement.execute(params![
                id,
                workspace_id.as_str(),
                path.project_id.as_ref().map(|id| id.as_str()),
                path.path,
                serialize_json_variant(&path.classification)?,
                serialize_json_variant(&path.mode)?,
                serde_json::to_string(&path.access)
                    .map_err(|error| MetadataError::Sqlite(json_to_sql_error(error)))?,
                path.matched_rule,
                path.rule_source,
                path.risk,
                path.summary,
                now,
            ])?;
        }
        drop(statement);
        transaction.commit()?;

        Ok(())
    }

    pub fn observed_path(
        &self,
        workspace_id: &WorkspaceId,
        path: &str,
    ) -> Result<Option<LocalPathRecord>, MetadataError> {
        let path = self.workspace_relative_path(workspace_id, path)?;
        let id = local_path_id(workspace_id, &path);
        self.connection
            .query_row(
                "SELECT path, classification, mode, access_json, matched_rule, rule_source,
                        risk, summary
                 FROM local_paths
                 WHERE id = ?1
                 LIMIT 1",
                params![id],
                |row| {
                    Ok(LocalPathRecord {
                        path: row.get(0)?,
                        classification: deserialize_json_variant(row.get::<_, String>(1)?)?,
                        mode: deserialize_json_variant(row.get::<_, String>(2)?)?,
                        access: serde_json::from_str(&row.get::<_, String>(3)?)
                            .map_err(json_to_sql_read_error)?,
                        matched_rule: row.get(4)?,
                        rule_source: row.get(5)?,
                        risk: row.get(6)?,
                        summary: row.get(7)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn set_observed_summary(
        &self,
        workspace_id: &WorkspaceId,
        summary: &ObservedWorkspaceSummary,
        now: &str,
    ) -> Result<(), MetadataError> {
        let summary_json = serde_json::to_string(summary)
            .map_err(|error| MetadataError::Sqlite(json_to_sql_error(error)))?;
        self.connection.execute(
            "INSERT INTO indexes
             (id, workspace_id, project_id, kind, state, watermark, updated_at)
             VALUES (?1, ?2, NULL, 'scan-summary', ?3, ?4, ?4)
             ON CONFLICT(id) DO UPDATE SET
               workspace_id = excluded.workspace_id,
               state = excluded.state,
               watermark = excluded.watermark,
               updated_at = excluded.updated_at",
            params![
                scan_summary_id(workspace_id),
                workspace_id.as_str(),
                summary_json,
                now,
            ],
        )?;
        Ok(())
    }

    pub fn observed_summary(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Option<ObservedWorkspaceSummary>, MetadataError> {
        let summary_json = self
            .connection
            .query_row(
                "SELECT state FROM indexes
                 WHERE workspace_id = ?1 AND kind = 'scan-summary'
                 ORDER BY updated_at DESC, id DESC
                 LIMIT 1",
                [workspace_id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;

        summary_json
            .map(|json| {
                serde_json::from_str(&json)
                    .map_err(|error| MetadataError::Sqlite(json_to_sql_error(error)))
            })
            .transpose()
    }

    pub fn upsert_index_document(&self, record: &IndexDocumentRecord) -> Result<(), MetadataError> {
        let path = normalize_workspace_path(&record.path);
        let project_id = record.project_id.as_ref().ok_or_else(|| {
            MetadataError::InvalidStorageMetadata("index document requires project_id".into())
        })?;
        self.connection.execute(
            "INSERT INTO index_documents
             (workspace_id, project_id, path, snapshot_id, content_id, classification, mode,
              access_json, policy_summary, body_text, hydration_state, indexed_bytes,
              source_watermark, indexed_watermark, state, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
             ON CONFLICT(workspace_id, project_id, path) DO UPDATE SET
               snapshot_id = excluded.snapshot_id,
               content_id = excluded.content_id,
               classification = excluded.classification,
               mode = excluded.mode,
               access_json = excluded.access_json,
               policy_summary = excluded.policy_summary,
               body_text = excluded.body_text,
               hydration_state = excluded.hydration_state,
               indexed_bytes = excluded.indexed_bytes,
               source_watermark = excluded.source_watermark,
               indexed_watermark = excluded.indexed_watermark,
               state = excluded.state,
               updated_at = excluded.updated_at",
            params![
                record.workspace_id.as_str(),
                project_id.as_str(),
                path,
                record.snapshot_id.as_ref().map(|id| id.as_str()),
                record.content_id.as_ref().map(|id| id.as_str()),
                serialize_json_variant(&record.classification)?,
                serialize_json_variant(&record.mode)?,
                serialize_access_flags(&record.access)?,
                record.policy_summary,
                record.body_text,
                serialize_json_variant(&record.hydration_state)?,
                record.indexed_bytes,
                record.source_watermark,
                record.indexed_watermark,
                record.state,
                record.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn index_documents_for_project(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
    ) -> Result<Vec<IndexDocumentRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT workspace_id, project_id, path, snapshot_id, content_id, classification, mode,
                    access_json, policy_summary, body_text, hydration_state, indexed_bytes,
                    source_watermark, indexed_watermark, state, updated_at
             FROM index_documents
             WHERE workspace_id = ?1 AND project_id = ?2 AND state = 'ready'
             ORDER BY path",
        )?;
        let rows = statement.query_map(
            params![workspace_id.as_str(), project_id.as_str()],
            index_document_from_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn purge_index_path(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
        path: &str,
    ) -> Result<usize, MetadataError> {
        let path = normalize_workspace_path(path);
        let documents = self.connection.execute(
            "DELETE FROM index_documents WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3",
            params![workspace_id.as_str(), project_id.as_str(), path],
        )?;
        let symbols = self.connection.execute(
            "DELETE FROM symbol_records WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3",
            params![workspace_id.as_str(), project_id.as_str(), path],
        )?;
        Ok(documents + symbols)
    }

    pub fn purge_index_paths_except(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
        keep_paths: &BTreeSet<String>,
        preserve_cold_paths: &BTreeSet<String>,
    ) -> Result<usize, MetadataError> {
        let existing = self.index_documents_for_project(workspace_id, project_id)?;
        let mut purged = 0_usize;
        for record in existing {
            let keep_cold = record.hydration_state == HydrationState::Cold
                && preserve_cold_paths.contains(&record.path);
            if !keep_paths.contains(&record.path) && !keep_cold {
                purged += self.purge_index_path(workspace_id, project_id, &record.path)?;
            }
        }
        Ok(purged)
    }

    pub fn purge_index_paths_under_prefix_except(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
        path_prefix: &str,
        keep_paths: &BTreeSet<String>,
        preserve_cold_paths: &BTreeSet<String>,
    ) -> Result<usize, MetadataError> {
        let path_prefix = normalize_workspace_path(path_prefix);
        let existing = self.index_documents_for_project(workspace_id, project_id)?;
        let mut purged = 0_usize;
        for record in existing {
            let keep_cold = record.hydration_state == HydrationState::Cold
                && preserve_cold_paths.contains(&record.path);
            if path_is_under_prefix(&record.path, &path_prefix)
                && !keep_paths.contains(&record.path)
                && !keep_cold
            {
                purged += self.purge_index_path(workspace_id, project_id, &record.path)?;
            }
        }
        Ok(purged)
    }

    pub fn purge_index_paths_for_snapshot_except(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
        snapshot_id: &SnapshotId,
        keep_paths: &BTreeSet<String>,
    ) -> Result<usize, MetadataError> {
        let existing = self.index_documents_for_project(workspace_id, project_id)?;
        let mut purged = 0_usize;
        for record in existing {
            if record.snapshot_id.as_ref() == Some(snapshot_id)
                && !keep_paths.contains(&record.path)
            {
                purged += self.purge_index_path(workspace_id, project_id, &record.path)?;
            }
        }
        Ok(purged)
    }

    pub fn replace_symbol_records_for_path(
        &mut self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
        path: &str,
        records: &[SymbolIndexRecord],
    ) -> Result<(), MetadataError> {
        let path = normalize_workspace_path(path);
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "DELETE FROM symbol_records WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3",
            params![workspace_id.as_str(), project_id.as_str(), path],
        )?;
        let mut statement = transaction.prepare(
            "INSERT INTO symbol_records
             (id, workspace_id, project_id, path, snapshot_id, name, kind, language, line_start,
              line_end, byte_start, byte_end, parser_status, access_json, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        )?;
        for record in records {
            let record_project_id = record.project_id.as_ref().ok_or_else(|| {
                rusqlite::Error::ToSqlConversionFailure(Box::new(
                    MetadataError::InvalidStorageMetadata(
                        "symbol record requires project_id".to_string(),
                    ),
                ))
            })?;
            statement.execute(params![
                record.id,
                record.workspace_id.as_str(),
                record_project_id.as_str(),
                path,
                record.snapshot_id.as_ref().map(|id| id.as_str()),
                record.name,
                record.kind,
                record.language,
                record.line_start,
                record.line_end,
                record.byte_start,
                record.byte_end,
                record.parser_status,
                serialize_access_flags(&record.access)?,
                record.updated_at,
            ])?;
        }
        drop(statement);
        transaction.commit()?;
        Ok(())
    }

    pub fn symbol_records_for_project(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
    ) -> Result<Vec<SymbolIndexRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT id, workspace_id, project_id, path, snapshot_id, name, kind, language,
                    line_start, line_end, byte_start, byte_end, parser_status, access_json,
                    updated_at
             FROM symbol_records
             WHERE workspace_id = ?1 AND project_id = ?2
             ORDER BY path, line_start, name, id",
        )?;
        let rows = statement.query_map(
            params![workspace_id.as_str(), project_id.as_str()],
            symbol_index_record_from_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn upsert_index_work(&self, record: &IndexWorkRecord) -> Result<(), MetadataError> {
        let path = record.path.as_deref().map(normalize_workspace_path);
        self.connection.execute(
            "INSERT INTO index_work
             (id, workspace_id, project_id, path, kind, source_watermark, indexed_watermark,
              state, reason, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(id) DO UPDATE SET
               workspace_id = excluded.workspace_id,
               project_id = excluded.project_id,
               path = excluded.path,
               kind = excluded.kind,
               source_watermark = excluded.source_watermark,
               indexed_watermark = excluded.indexed_watermark,
               state = excluded.state,
               reason = excluded.reason,
               updated_at = excluded.updated_at",
            params![
                record.id,
                record.workspace_id.as_str(),
                record.project_id.as_ref().map(|id| id.as_str()),
                path,
                record.kind,
                record.source_watermark,
                record.indexed_watermark,
                record.state,
                record.reason,
                record.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn mark_index_work_pending_for_path(
        &self,
        workspace_id: &WorkspaceId,
        project_id: Option<&ProjectId>,
        path: &str,
        reason: &str,
        updated_at: &str,
    ) -> Result<(), MetadataError> {
        let Some(project_id) = project_id else {
            return Ok(());
        };
        let path = self.workspace_relative_path(workspace_id, path)?;
        if is_work_namespace_path(&path) {
            return Ok(());
        }
        if reason == "local-write-log"
            && !project_index_initialized(self, workspace_id, project_id)?
        {
            return Ok(());
        }
        let index_path = self
            .project_by_id(workspace_id, project_id)?
            .map(|project| project_relative_index_path(&path, &project.path))
            .unwrap_or(path);
        let source_watermark = next_index_source_watermark(self, workspace_id, project_id)?;
        self.upsert_index_work(&IndexWorkRecord {
            id: format!(
                "index_work:{}:{}:path:{}",
                workspace_id.as_str(),
                project_id.as_str(),
                stable_store_token(&index_path)
            ),
            workspace_id: workspace_id.clone(),
            project_id: Some(project_id.clone()),
            path: Some(index_path),
            kind: "path".to_string(),
            source_watermark,
            indexed_watermark: 0,
            state: "pending".to_string(),
            reason: Some(reason.to_string()),
            updated_at: updated_at.to_string(),
        })
    }

    pub fn mark_index_work_ready_for_project(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
        updated_at: &str,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "UPDATE index_work
             SET state = 'ready',
                 indexed_watermark = source_watermark,
                 reason = NULL,
                 updated_at = ?3
             WHERE workspace_id = ?1 AND project_id = ?2 AND state != 'ready'",
            params![workspace_id.as_str(), project_id.as_str(), updated_at],
        )?;
        Ok(())
    }

    pub fn mark_index_work_ready_for_paths(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
        paths: &BTreeSet<String>,
        updated_at: &str,
    ) -> Result<(), MetadataError> {
        let work = self.index_work_for_project(workspace_id, project_id)?;
        for record in work {
            let Some(path) = record.path.as_deref() else {
                continue;
            };
            if paths.contains(path) {
                self.connection.execute(
                    "UPDATE index_work
                     SET indexed_watermark = source_watermark,
                         state = 'ready',
                         reason = NULL,
                         updated_at = ?1
                     WHERE id = ?2",
                    params![updated_at, record.id],
                )?;
            }
        }
        Ok(())
    }

    pub fn mark_local_write_index_work_ready_for_scope(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
        path_prefix: Option<&str>,
        updated_at: &str,
    ) -> Result<(), MetadataError> {
        let path_prefix = path_prefix.map(normalize_workspace_path);
        let work = self.index_work_for_project(workspace_id, project_id)?;
        for record in work {
            if record.reason.as_deref() != Some("local-write-log") {
                continue;
            }
            let Some(path) = record.path.as_deref() else {
                continue;
            };
            if path_prefix
                .as_deref()
                .is_none_or(|prefix| path_is_under_prefix(path, prefix))
            {
                self.connection.execute(
                    "UPDATE index_work
                     SET indexed_watermark = source_watermark,
                         state = 'ready',
                         reason = NULL,
                         updated_at = ?1
                     WHERE id = ?2",
                    params![updated_at, record.id],
                )?;
            }
        }
        Ok(())
    }

    pub fn mark_index_work_ready_under_prefix(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
        path_prefix: &str,
        updated_at: &str,
    ) -> Result<(), MetadataError> {
        let path_prefix = normalize_workspace_path(path_prefix);
        let work = self.index_work_for_project(workspace_id, project_id)?;
        for record in work {
            let Some(path) = record.path.as_deref() else {
                continue;
            };
            if path_is_under_prefix(path, &path_prefix) {
                self.connection.execute(
                    "UPDATE index_work
                     SET indexed_watermark = source_watermark,
                         state = 'ready',
                         reason = NULL,
                         updated_at = ?1
                     WHERE id = ?2",
                    params![updated_at, record.id],
                )?;
            }
        }
        Ok(())
    }

    pub fn upsert_index_pack(&self, record: &IndexPackRecord) -> Result<(), MetadataError> {
        self.connection.execute(
            "INSERT INTO index_packs
             (workspace_id, project_id, snapshot_id, object_key, byte_len, hash, state, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(workspace_id, object_key) DO UPDATE SET
               project_id = excluded.project_id,
               snapshot_id = excluded.snapshot_id,
               byte_len = excluded.byte_len,
               hash = excluded.hash,
               state = excluded.state,
               updated_at = excluded.updated_at",
            params![
                record.workspace_id.as_str(),
                record.project_id.as_ref().map(|id| id.as_str()),
                record.snapshot_id.as_ref().map(|id| id.as_str()),
                record.object_key,
                record.byte_len,
                record.hash,
                record.state,
                record.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn index_packs_for_project(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
    ) -> Result<Vec<IndexPackRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT workspace_id, project_id, snapshot_id, object_key, byte_len, hash, state,
                    updated_at
             FROM index_packs
             WHERE workspace_id = ?1 AND project_id = ?2
             ORDER BY updated_at DESC, object_key",
        )?;
        let rows = statement.query_map(
            params![workspace_id.as_str(), project_id.as_str()],
            index_pack_from_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn index_work_for_project(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
    ) -> Result<Vec<IndexWorkRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT id, workspace_id, project_id, path, kind, source_watermark, indexed_watermark,
                    state, reason, updated_at
             FROM index_work
             WHERE workspace_id = ?1 AND project_id = ?2
             ORDER BY updated_at, id",
        )?;
        let rows = statement.query_map(
            params![workspace_id.as_str(), project_id.as_str()],
            index_work_from_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn put_pack_record(
        &self,
        workspace_id: &WorkspaceId,
        pack_id: &PackId,
        kind: &str,
        byte_len: u64,
        state: &str,
        now: &str,
    ) -> Result<(), MetadataError> {
        self.put_pack_record_with_metadata(
            workspace_id,
            pack_id,
            kind,
            byte_len,
            "",
            1,
            state,
            None,
            now,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn put_pack_record_with_metadata(
        &self,
        workspace_id: &WorkspaceId,
        pack_id: &PackId,
        kind: &str,
        byte_len: u64,
        object_hash: &str,
        key_epoch: u32,
        state: &str,
        retain_until: Option<&str>,
        now: &str,
    ) -> Result<(), MetadataError> {
        validate_pack_kind(kind)?;
        validate_pack_state(state)?;
        if key_epoch == 0 {
            return Err(MetadataError::InvalidStorageMetadata(
                "pack key epoch must be non-zero".to_string(),
            ));
        }
        self.connection.execute(
            "INSERT INTO packs
             (id, workspace_id, kind, byte_len, object_hash, key_epoch, state, retain_until, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(workspace_id, id) DO UPDATE SET
               kind = excluded.kind,
               byte_len = excluded.byte_len,
               object_hash = excluded.object_hash,
               key_epoch = excluded.key_epoch,
               state = excluded.state,
               retain_until = excluded.retain_until",
            params![
                pack_id.as_str(),
                workspace_id.as_str(),
                kind,
                byte_len,
                object_hash,
                key_epoch,
                state,
                retain_until,
                now,
            ],
        )?;
        Ok(())
    }

    pub fn pack_records(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<PackRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT id, workspace_id, kind, byte_len, object_hash, key_epoch, state, retain_until
             FROM packs
             WHERE workspace_id = ?1
             ORDER BY id",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], |row| {
            Ok(PackRecord {
                id: PackId::new(row.get::<_, String>(0)?),
                workspace_id: WorkspaceId::new(row.get::<_, String>(1)?),
                kind: row.get(2)?,
                byte_len: row.get(3)?,
                object_hash: row.get(4)?,
                key_epoch: row.get(5)?,
                state: row.get(6)?,
                retain_until: row.get(7)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn put_content_locator(
        &self,
        workspace_id: &WorkspaceId,
        locator: &ContentLocator,
        now: &str,
    ) -> Result<(), MetadataError> {
        validate_locator_shape(locator)?;
        if let Some(pack_id) = &locator.pack_id {
            ensure_pack_exists(&self.connection, workspace_id, pack_id)?;
        }
        let locator_json = serde_json::to_string(locator)
            .map_err(|error| MetadataError::Sqlite(json_to_sql_error(error)))?;
        self.connection.execute(
            "INSERT INTO content_locators
             (content_id, workspace_id, storage, raw_size, pack_id, offset, length,
              locator_json, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(workspace_id, content_id) DO UPDATE SET
               storage = excluded.storage,
               raw_size = excluded.raw_size,
               pack_id = excluded.pack_id,
               offset = excluded.offset,
               length = excluded.length,
               locator_json = excluded.locator_json,
               updated_at = excluded.updated_at",
            params![
                locator.content_id.as_str(),
                workspace_id.as_str(),
                serialize_json_variant(&locator.storage)?,
                locator.raw_size,
                locator.pack_id.as_ref().map(|pack_id| pack_id.as_str()),
                locator.offset,
                locator.length,
                locator_json,
                now,
            ],
        )?;
        Ok(())
    }

    pub fn content_locator(
        &self,
        workspace_id: &WorkspaceId,
        content_id: &ContentId,
    ) -> Result<Option<StoredContentLocator>, MetadataError> {
        self.connection
            .query_row(
                "SELECT workspace_id, content_id, storage, raw_size, pack_id, offset, length, locator_json,
                        updated_at
                 FROM content_locators
                 WHERE workspace_id = ?1 AND content_id = ?2",
                params![workspace_id.as_str(), content_id.as_str()],
                |row| {
                    Ok(StoredContentLocator {
                        workspace_id: WorkspaceId::new(row.get::<_, String>(0)?),
                        locator: content_locator_from_row(row)?,
                        updated_at: row.get(8)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn content_locators(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<ContentLocator>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT workspace_id, content_id, storage, raw_size, pack_id, offset, length, locator_json
             FROM content_locators
             WHERE workspace_id = ?1
             ORDER BY content_id",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], content_locator_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn replace_env_records_for_source(
        &mut self,
        workspace_id: &WorkspaceId,
        source_path: &str,
        records: &[EnvRecord],
    ) -> Result<(), MetadataError> {
        let source_path = self.workspace_relative_path(workspace_id, source_path)?;
        let normalized_records = records
            .iter()
            .map(|record| {
                let mut record = record.clone();
                record.source_path =
                    self.workspace_relative_path(&record.workspace_id, &record.source_path)?;
                Ok(record)
            })
            .collect::<Result<Vec<_>, MetadataError>>()?;
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "DELETE FROM env_records
             WHERE workspace_id = ?1 AND source_path = ?2",
            params![workspace_id.as_str(), source_path],
        )?;
        for record in normalized_records {
            upsert_env_record_tx(&transaction, &record)?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn upsert_env_record(&self, record: &EnvRecord) -> Result<(), MetadataError> {
        let mut record = record.clone();
        record.source_path =
            self.workspace_relative_path(&record.workspace_id, &record.source_path)?;
        self.connection
            .execute(
                "INSERT INTO env_records
                 (id, workspace_id, project_id, source_path, key_name, access,
                  value_ciphertext_ref, updated_at, profile, occurrence_index, line_kind,
                  encrypted_locator_json, format_json, materialization_state, restriction_state,
                  key_epoch, metadata_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
                 ON CONFLICT(id) DO UPDATE SET
                   workspace_id = excluded.workspace_id,
                   project_id = excluded.project_id,
                   source_path = excluded.source_path,
                   key_name = excluded.key_name,
                   access = excluded.access,
                   value_ciphertext_ref = excluded.value_ciphertext_ref,
                   updated_at = excluded.updated_at,
                   profile = excluded.profile,
                   occurrence_index = excluded.occurrence_index,
                   line_kind = excluded.line_kind,
                   encrypted_locator_json = excluded.encrypted_locator_json,
                   format_json = excluded.format_json,
                   materialization_state = excluded.materialization_state,
                   restriction_state = excluded.restriction_state,
                   key_epoch = excluded.key_epoch,
                   metadata_json = excluded.metadata_json",
                params![
                    record.id.as_str(),
                    record.workspace_id.as_str(),
                    record.project_id.as_ref().map(|id| id.as_str()),
                    record.source_path.as_str(),
                    record.key_name.as_str(),
                    serialize_access_flags(&record.access)?,
                    record.updated_at.as_str(),
                    record.profile.as_str(),
                    record.occurrence_index,
                    record.line_kind.as_str(),
                    record.encrypted_locator_json.as_str(),
                    record.format_json.as_str(),
                    record.materialization_state.as_str(),
                    record.restriction_state.as_str(),
                    record.key_epoch,
                    record.metadata_json.as_str(),
                ],
            )
            .map(|_| ())
            .map_err(Into::into)
    }

    pub fn env_records(&self, workspace_id: &WorkspaceId) -> Result<Vec<EnvRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT id, workspace_id, project_id, source_path, key_name, access,
                    updated_at, profile, occurrence_index, line_kind, encrypted_locator_json,
                    format_json, materialization_state, restriction_state, key_epoch, metadata_json
             FROM env_records
             WHERE workspace_id = ?1
             ORDER BY source_path, occurrence_index, id",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], env_record_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn env_records_for_source(
        &self,
        workspace_id: &WorkspaceId,
        source_path: &str,
    ) -> Result<Vec<EnvRecord>, MetadataError> {
        let source_path = self.workspace_relative_path(workspace_id, source_path)?;
        let mut statement = self.connection.prepare(
            "SELECT id, workspace_id, project_id, source_path, key_name, access,
                    updated_at, profile, occurrence_index, line_kind, encrypted_locator_json,
                    format_json, materialization_state, restriction_state, key_epoch, metadata_json
             FROM env_records
             WHERE workspace_id = ?1 AND source_path = ?2
             ORDER BY occurrence_index, id",
        )?;
        let rows = statement.query_map(
            params![workspace_id.as_str(), source_path],
            env_record_from_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn upsert_setup_receipt(&self, record: &SetupReceiptRecord) -> Result<(), MetadataError> {
        let cwd = self.workspace_relative_path(&record.workspace_id, &record.cwd)?;
        self.connection.execute(
            "INSERT INTO setup_receipts
             (id, workspace_id, project_id, command, state, receipt_json, updated_at,
              recipe_hash, approval_state, trigger, cwd, os, arch, env_profile,
              output_path, redacted_summary)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
             ON CONFLICT(id) DO UPDATE SET
               workspace_id = excluded.workspace_id,
               project_id = excluded.project_id,
               command = excluded.command,
               state = excluded.state,
               receipt_json = excluded.receipt_json,
               updated_at = excluded.updated_at,
               recipe_hash = excluded.recipe_hash,
               approval_state = excluded.approval_state,
               trigger = excluded.trigger,
               cwd = excluded.cwd,
               os = excluded.os,
               arch = excluded.arch,
               env_profile = excluded.env_profile,
               output_path = excluded.output_path,
               redacted_summary = excluded.redacted_summary",
            params![
                record.id.as_str(),
                record.workspace_id.as_str(),
                record.project_id.as_ref().map(|id| id.as_str()),
                record.command.as_str(),
                record.state.as_str(),
                record.receipt_json.as_str(),
                record.updated_at.as_str(),
                record.recipe_hash.as_str(),
                record.approval_state.as_str(),
                record.trigger.as_str(),
                cwd,
                record.os.as_str(),
                record.arch.as_str(),
                record.env_profile.as_str(),
                record.output_path.as_deref(),
                record.redacted_summary.as_str(),
            ],
        )?;
        Ok(())
    }

    pub fn setup_receipts(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<SetupReceiptRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT id, workspace_id, project_id, command, state, receipt_json, updated_at,
                    recipe_hash, approval_state, trigger, cwd, os, arch, env_profile,
                    output_path, redacted_summary
             FROM setup_receipts
             WHERE workspace_id = ?1
             ORDER BY updated_at DESC, id",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], setup_receipt_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn command_idempotency_record(
        &self,
        workspace_id: &WorkspaceId,
        idempotency_key: &str,
    ) -> Result<Option<CommandIdempotencyRecord>, MetadataError> {
        self.connection
            .query_row(
                "SELECT workspace_id, idempotency_key, command, request_hash, result_json,
                        status, created_at, updated_at, expires_at
                 FROM command_idempotency_records
                 WHERE workspace_id = ?1 AND idempotency_key = ?2
                 LIMIT 1",
                params![workspace_id.as_str(), idempotency_key],
                command_idempotency_record_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn upsert_command_idempotency_record(
        &self,
        record: &CommandIdempotencyRecord,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "INSERT INTO command_idempotency_records
             (workspace_id, idempotency_key, command, request_hash, result_json, status,
              created_at, updated_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(workspace_id, idempotency_key) DO UPDATE SET
               command = excluded.command,
               result_json = excluded.result_json,
               status = excluded.status,
               updated_at = excluded.updated_at,
               expires_at = excluded.expires_at
             WHERE command_idempotency_records.request_hash = excluded.request_hash",
            params![
                record.workspace_id.as_str(),
                record.idempotency_key.as_str(),
                record.command.as_str(),
                record.request_hash.as_str(),
                record.result_json.as_str(),
                record.status.as_str(),
                record.created_at.as_str(),
                record.updated_at.as_str(),
                record.expires_at.as_str(),
            ],
        )?;
        Ok(())
    }

    pub fn try_insert_command_idempotency_record(
        &self,
        record: &CommandIdempotencyRecord,
    ) -> Result<bool, MetadataError> {
        let changed = self.connection.execute(
            "INSERT OR IGNORE INTO command_idempotency_records
             (workspace_id, idempotency_key, command, request_hash, result_json, status,
              created_at, updated_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                record.workspace_id.as_str(),
                record.idempotency_key.as_str(),
                record.command.as_str(),
                record.request_hash.as_str(),
                record.result_json.as_str(),
                record.status.as_str(),
                record.created_at.as_str(),
                record.updated_at.as_str(),
                record.expires_at.as_str(),
            ],
        )?;
        Ok(changed == 1)
    }

    pub fn finish_command_idempotency_record(
        &self,
        record: &CommandIdempotencyRecord,
    ) -> Result<(), MetadataError> {
        let changed = self.connection.execute(
            "UPDATE command_idempotency_records
             SET result_json = ?4,
                 status = ?5,
                 updated_at = ?6,
                 expires_at = ?7
             WHERE workspace_id = ?1
               AND idempotency_key = ?2
               AND request_hash = ?3",
            params![
                record.workspace_id.as_str(),
                record.idempotency_key.as_str(),
                record.request_hash.as_str(),
                record.result_json.as_str(),
                record.status.as_str(),
                record.updated_at.as_str(),
                record.expires_at.as_str(),
            ],
        )?;
        if changed == 1 {
            Ok(())
        } else {
            Err(MetadataError::InvalidStorageMetadata(
                "idempotency reservation changed before finish".to_string(),
            ))
        }
    }

    pub fn delete_command_idempotency_record(
        &self,
        workspace_id: &WorkspaceId,
        idempotency_key: &str,
        request_hash: &str,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "DELETE FROM command_idempotency_records
             WHERE workspace_id = ?1
               AND idempotency_key = ?2
               AND request_hash = ?3",
            params![workspace_id.as_str(), idempotency_key, request_hash],
        )?;
        Ok(())
    }

    pub fn upsert_agent_lease(&self, record: &AgentLeaseRecord) -> Result<(), MetadataError> {
        let lease_json = serde_json::to_string(record)
            .map_err(|error| MetadataError::Sqlite(json_to_sql_error(error)))?;
        self.connection.execute(
            "INSERT INTO leases (id, workspace_id, project_id, state, lease_json, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
               workspace_id = excluded.workspace_id,
               project_id = excluded.project_id,
               state = excluded.state,
               lease_json = excluded.lease_json,
               updated_at = excluded.updated_at",
            params![
                record.id.as_str(),
                record.workspace_id.as_str(),
                record.project_id.as_str(),
                serialize_json_variant(&record.execution_state)?,
                lease_json,
                record.updated_at.as_str(),
            ],
        )?;
        Ok(())
    }

    pub fn grant_agent_lease_budget_override(
        &mut self,
        lease: &AgentLeaseRecord,
        override_id: &str,
        added_bytes: u64,
        now: &str,
    ) -> Result<(), MetadataError> {
        let lease_json = serde_json::to_string(lease)
            .map_err(|error| MetadataError::Sqlite(json_to_sql_error(error)))?;
        let lease_state = serialize_json_variant(&lease.execution_state)?;
        self.with_transaction(|transaction| {
            transaction.execute(
                "INSERT INTO leases (id, workspace_id, project_id, state, lease_json, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(id) DO UPDATE SET
                   workspace_id = excluded.workspace_id,
                   project_id = excluded.project_id,
                   state = excluded.state,
                   lease_json = excluded.lease_json,
                   updated_at = excluded.updated_at",
                params![
                    lease.id.as_str(),
                    lease.workspace_id.as_str(),
                    lease.project_id.as_str(),
                    lease_state,
                    lease_json,
                    lease.updated_at.as_str(),
                ],
            )?;
            transaction.execute(
                "INSERT INTO hydration_budget_ledger
                 (id, workspace_id, project_id, lease_id, path, content_id, cause,
                  requested_bytes, reserved_bytes, committed_bytes, outcome, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, '.', NULL, 'human-override', ?5, 0, 0,
                         'override-granted', ?6, ?6)",
                params![
                    override_id,
                    lease.workspace_id.as_str(),
                    lease.project_id.as_str(),
                    lease.id.as_str(),
                    added_bytes,
                    now,
                ],
            )?;
            Ok(())
        })
    }

    pub fn agent_lease_by_id(
        &self,
        lease_id: &LeaseId,
    ) -> Result<Option<AgentLeaseRecord>, MetadataError> {
        self.connection
            .query_row(
                "SELECT lease_json FROM leases WHERE id = ?1 LIMIT 1",
                params![lease_id.as_str()],
                agent_lease_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn agent_leases(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<AgentLeaseRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT lease_json
             FROM leases
             WHERE workspace_id = ?1
             ORDER BY updated_at DESC, id",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], agent_lease_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn set_project_hot_state(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
        hot_state: &str,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "UPDATE projects
             SET hot_state = ?3
             WHERE workspace_id = ?1 AND id = ?2",
            params![workspace_id.as_str(), project_id.as_str(), hot_state],
        )?;
        Ok(())
    }

    pub fn project_hot_state(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
    ) -> Result<Option<String>, MetadataError> {
        self.connection
            .query_row(
                "SELECT hot_state FROM projects
                 WHERE workspace_id = ?1 AND id = ?2",
                params![workspace_id.as_str(), project_id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn upsert_projected_node(&self, record: &ProjectedNodeRecord) -> Result<(), MetadataError> {
        let path = self.workspace_relative_path(&record.workspace_id, &record.path)?;
        self.connection.execute(
            "INSERT INTO projected_nodes
             (workspace_id, node_id, project_id, parent_node_id, path, kind, content_id,
              hydration_state, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(workspace_id, node_id) DO UPDATE SET
               project_id = excluded.project_id,
               parent_node_id = excluded.parent_node_id,
               path = excluded.path,
               kind = excluded.kind,
               content_id = excluded.content_id,
               hydration_state = excluded.hydration_state,
               updated_at = excluded.updated_at",
            params![
                record.workspace_id.as_str(),
                record.node_id.as_str(),
                record.project_id.as_ref().map(|id| id.as_str()),
                record.parent_node_id.as_deref(),
                path,
                serialize_json_variant(&record.kind)?,
                record.content_id.as_ref().map(|id| id.as_str()),
                serialize_json_variant(&record.hydration_state)?,
                record.updated_at.as_str(),
            ],
        )?;
        if record.kind == NamespaceEntryKind::File {
            let index_target = match record.project_id.as_ref() {
                Some(project_id) => {
                    let index_path = self
                        .project_by_id(&record.workspace_id, project_id)?
                        .map(|project| project_relative_index_path(&path, &project.path))
                        .unwrap_or_else(|| path.clone());
                    Some((project_id.clone(), index_path))
                }
                None => self.current_project_by_path(&path)?.map(|project| {
                    let index_path = project_relative_index_path(&path, &project.path);
                    (project.id, index_path)
                }),
            };
            if let Some((project_id, index_path)) = index_target {
                self.mark_index_work_pending_for_path(
                    &record.workspace_id,
                    Some(&project_id),
                    &index_path,
                    "projected-node-updated",
                    &record.updated_at,
                )?;
            }
        }
        Ok(())
    }

    pub fn projected_node_by_path(
        &self,
        workspace_id: &WorkspaceId,
        path: &str,
    ) -> Result<Option<ProjectedNodeRecord>, MetadataError> {
        let path = self.workspace_relative_path(workspace_id, path)?;
        self.connection
            .query_row(
                "SELECT workspace_id, node_id, project_id, parent_node_id, path, kind, content_id,
                        hydration_state, updated_at
                 FROM projected_nodes
                 WHERE workspace_id = ?1 AND path = ?2
                 LIMIT 1",
                params![workspace_id.as_str(), path],
                projected_node_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn projected_nodes_for_project(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
    ) -> Result<Vec<ProjectedNodeRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT workspace_id, node_id, project_id, parent_node_id, path, kind, content_id,
                    hydration_state, updated_at
             FROM projected_nodes
             WHERE workspace_id = ?1 AND project_id = ?2
             ORDER BY path",
        )?;
        let rows = statement.query_map(
            params![workspace_id.as_str(), project_id.as_str()],
            projected_node_from_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn projected_nodes_for_workspace(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<ProjectedNodeRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT workspace_id, node_id, project_id, parent_node_id, path, kind, content_id,
                    hydration_state, updated_at
             FROM projected_nodes
             WHERE workspace_id = ?1
             ORDER BY path",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], projected_node_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn delete_unlisted_workspace_projected_nodes(
        &self,
        workspace_id: &WorkspaceId,
        retained_paths: &BTreeSet<String>,
    ) -> Result<(), MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT path FROM projected_nodes
             WHERE workspace_id = ?1 AND project_id IS NULL",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], |row| row.get::<_, String>(0))?;
        let stale_paths = rows
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|path| !retained_paths.contains(path))
            .collect::<Vec<_>>();
        drop(statement);
        for path in stale_paths {
            self.connection.execute(
                "DELETE FROM projected_nodes
                 WHERE workspace_id = ?1 AND project_id IS NULL AND path = ?2",
                params![workspace_id.as_str(), path],
            )?;
        }
        Ok(())
    }

    pub fn upsert_workspace_sync_head(
        &self,
        record: &WorkspaceSyncHeadRecord,
    ) -> Result<(), MetadataError> {
        self.insert_workspace(
            &WorkspaceId::new(record.workspace_ref.workspace_id.clone()),
            "Code",
            &record.observed_at,
        )?;
        self.connection.execute(
            "INSERT INTO workspace_sync_heads
             (workspace_id, version, snapshot_id, updated_at_tick, updated_by_device_id, observed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(workspace_id) DO UPDATE SET
               version = excluded.version,
               snapshot_id = excluded.snapshot_id,
               updated_at_tick = excluded.updated_at_tick,
               updated_by_device_id = excluded.updated_by_device_id,
               observed_at = excluded.observed_at",
            params![
                record.workspace_ref.workspace_id.as_str(),
                record.workspace_ref.version,
                record.workspace_ref.snapshot_id.as_str(),
                record.workspace_ref.updated_at.tick,
                record.workspace_ref.updated_by_device_id.as_deref(),
                record.observed_at.as_str(),
            ],
        )?;
        Ok(())
    }

    pub fn workspace_sync_head(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Option<WorkspaceSyncHeadRecord>, MetadataError> {
        self.connection
            .query_row(
                "SELECT workspace_id, version, snapshot_id, updated_at_tick, updated_by_device_id, observed_at
                 FROM workspace_sync_heads
                 WHERE workspace_id = ?1",
                [workspace_id.as_str()],
                |row| {
                    Ok(WorkspaceSyncHeadRecord {
                        workspace_ref: WorkspaceRef {
                            workspace_id: row.get(0)?,
                            version: row.get::<_, u64>(1)?,
                            snapshot_id: row.get(2)?,
                            updated_at: bowline_control_plane::ControlPlaneTimestamp {
                                tick: row.get::<_, u64>(3)?,
                            },
                            updated_by_device_id: row.get(4)?,
                        },
                        observed_at: row.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn enqueue_sync_operation(
        &self,
        record: &SyncOperationRecord,
    ) -> Result<(), MetadataError> {
        self.insert_workspace(&record.workspace_id, "Code", &record.updated_at)?;
        self.connection.execute(
            "INSERT INTO sync_operations
             (id, workspace_id, kind, state, idempotency_key, base_version, base_snapshot_id,
              target_snapshot_id, device_id, payload_json, attempt_count, claimed_by,
              heartbeat_at, next_attempt_at, last_error, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
             ON CONFLICT(workspace_id, idempotency_key) DO UPDATE SET
               kind = excluded.kind,
               state = CASE
                 WHEN sync_operations.state = 'completed' THEN sync_operations.state
                 ELSE excluded.state
               END,
               base_version = excluded.base_version,
               base_snapshot_id = excluded.base_snapshot_id,
               target_snapshot_id = excluded.target_snapshot_id,
               device_id = excluded.device_id,
               payload_json = excluded.payload_json,
               updated_at = excluded.updated_at",
            params![
                record.id.as_str(),
                record.workspace_id.as_str(),
                record.kind.as_str(),
                record.state.as_str(),
                record.idempotency_key.as_str(),
                record.base_version,
                record.base_snapshot_id.as_deref(),
                record.target_snapshot_id.as_deref(),
                record.device_id.as_ref().map(|id| id.as_str()),
                record.payload_json.as_str(),
                record.attempt_count,
                record.claimed_by.as_deref(),
                record.heartbeat_at.as_deref(),
                record.next_attempt_at.as_deref(),
                record.last_error.as_deref(),
                record.created_at.as_str(),
                record.updated_at.as_str(),
            ],
        )?;
        Ok(())
    }

    pub fn sync_operations(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<SyncOperationRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT id, workspace_id, kind, state, idempotency_key, base_version, base_snapshot_id,
                    target_snapshot_id, device_id, payload_json, attempt_count, claimed_by,
                    heartbeat_at, next_attempt_at, last_error, created_at, updated_at
             FROM sync_operations
             WHERE workspace_id = ?1
             ORDER BY created_at, id",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], sync_operation_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn active_sync_operation_for_device(
        &self,
        workspace_id: &WorkspaceId,
        kind: &str,
        device_id: &DeviceId,
    ) -> Result<Option<SyncOperationRecord>, MetadataError> {
        self.connection
            .query_row(
                "SELECT id, workspace_id, kind, state, idempotency_key, base_version, base_snapshot_id,
                        target_snapshot_id, device_id, payload_json, attempt_count, claimed_by,
                        heartbeat_at, next_attempt_at, last_error, created_at, updated_at
                 FROM sync_operations
                 WHERE workspace_id = ?1
                   AND kind = ?2
                   AND device_id = ?3
                   AND state != 'completed'
                 ORDER BY created_at, id
                 LIMIT 1",
                params![workspace_id.as_str(), kind, device_id.as_str()],
                sync_operation_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn claim_next_sync_operation(
        &self,
        workspace_id: &WorkspaceId,
        claimant: &str,
        now: &str,
    ) -> Result<Option<SyncOperationRecord>, MetadataError> {
        let Some(id) = self
            .connection
            .query_row(
                "SELECT id FROM sync_operations
                 WHERE workspace_id = ?1
                   AND (
                     state = 'queued'
                     OR (state = 'waiting_retry' AND (next_attempt_at IS NULL OR next_attempt_at <= ?2))
                     OR (state = 'blocked_offline' AND (next_attempt_at IS NULL OR next_attempt_at <= ?2))
                   )
                 ORDER BY created_at, id
                 LIMIT 1",
                params![workspace_id.as_str(), now],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        else {
            return Ok(None);
        };

        let changed = self.connection.execute(
            "UPDATE sync_operations
             SET state = 'claimed',
                 claimed_by = ?1,
                 heartbeat_at = ?2,
                 attempt_count = attempt_count + 1,
                 updated_at = ?2
             WHERE id = ?3
               AND workspace_id = ?4
               AND (
                 state = 'queued'
                 OR (state = 'waiting_retry' AND (next_attempt_at IS NULL OR next_attempt_at <= ?2))
                 OR (state = 'blocked_offline' AND (next_attempt_at IS NULL OR next_attempt_at <= ?2))
               )",
            params![claimant, now, id, workspace_id.as_str()],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.sync_operation_by_id(&id)
    }

    pub fn refresh_sync_operation_heartbeat(
        &self,
        id: &str,
        claimant: &str,
        now: &str,
    ) -> Result<bool, MetadataError> {
        Ok(self.connection.execute(
            "UPDATE sync_operations
             SET heartbeat_at = ?3, updated_at = ?3
             WHERE id = ?1 AND state = 'claimed' AND claimed_by = ?2",
            params![id, claimant, now],
        )? > 0)
    }

    pub fn complete_sync_operation(
        &self,
        id: &str,
        completion_payload_json: &str,
        now: &str,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "UPDATE sync_operations
             SET state = 'completed',
                 payload_json = ?2,
                 claimed_by = NULL,
                 heartbeat_at = NULL,
                 next_attempt_at = NULL,
                 last_error = NULL,
                 updated_at = ?3
             WHERE id = ?1",
            params![id, completion_payload_json, now],
        )?;
        Ok(())
    }

    pub fn fail_sync_operation_for_retry(
        &self,
        id: &str,
        message: &str,
        next_attempt_at: &str,
        now: &str,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "UPDATE sync_operations
             SET state = 'waiting_retry',
                 claimed_by = NULL,
                 heartbeat_at = NULL,
                 next_attempt_at = ?3,
                 last_error = ?2,
                 updated_at = ?4
             WHERE id = ?1",
            params![id, message, next_attempt_at, now],
        )?;
        Ok(())
    }

    pub fn block_sync_operation_offline(
        &self,
        id: &str,
        message: &str,
        next_attempt_at: &str,
        now: &str,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "UPDATE sync_operations
             SET state = 'blocked_offline',
                 claimed_by = NULL,
                 heartbeat_at = NULL,
                 next_attempt_at = ?3,
                 last_error = ?2,
                 updated_at = ?4
             WHERE id = ?1",
            params![id, message, next_attempt_at, now],
        )?;
        Ok(())
    }

    pub fn mark_sync_operation_attention(
        &self,
        id: &str,
        message: &str,
        now: &str,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "UPDATE sync_operations
             SET state = 'attention',
                 claimed_by = NULL,
                 heartbeat_at = NULL,
                 last_error = ?2,
                 updated_at = ?3
             WHERE id = ?1",
            params![id, message, now],
        )?;
        Ok(())
    }

    pub fn complete_obsolete_daemon_reconciles_for_device(
        &self,
        workspace_id: &WorkspaceId,
        device_id: &DeviceId,
        completion_payload_json: &str,
        now: &str,
    ) -> Result<u64, MetadataError> {
        let changed = self.connection.execute(
            "UPDATE sync_operations
             SET state = 'completed',
                 payload_json = ?3,
                 claimed_by = NULL,
                 heartbeat_at = NULL,
                 next_attempt_at = NULL,
                 last_error = NULL,
                 updated_at = ?4
             WHERE workspace_id = ?1
               AND device_id = ?2
               AND kind = 'daemon-reconcile'
               AND state IN ('waiting_retry', 'blocked_offline', 'attention')",
            params![
                workspace_id.as_str(),
                device_id.as_str(),
                completion_payload_json,
                now,
            ],
        )?;
        Ok(changed as u64)
    }

    pub fn append_sync_operation_checkpoint(
        &self,
        record: &SyncOperationCheckpointRecord,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "INSERT INTO sync_operation_checkpoints
             (id, workspace_id, operation_id, step, state, payload_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET
               state = excluded.state,
               payload_json = excluded.payload_json,
               updated_at = excluded.updated_at",
            params![
                record.id.as_str(),
                record.workspace_id.as_str(),
                record.operation_id.as_str(),
                record.step.as_str(),
                record.state.as_str(),
                record.payload_json.as_str(),
                record.created_at.as_str(),
                record.updated_at.as_str(),
            ],
        )?;
        Ok(())
    }

    pub fn sync_operation_checkpoints(
        &self,
        operation_id: &str,
    ) -> Result<Vec<SyncOperationCheckpointRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT id, workspace_id, operation_id, step, state, payload_json, created_at, updated_at
             FROM sync_operation_checkpoints
             WHERE operation_id = ?1
             ORDER BY created_at, id",
        )?;
        let rows = statement.query_map([operation_id], sync_operation_checkpoint_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn requeue_expired_sync_claims(
        &self,
        workspace_id: &WorkspaceId,
        expired_before: &str,
        now: &str,
    ) -> Result<u64, MetadataError> {
        let changed = self.connection.execute(
            "UPDATE sync_operations
             SET state = 'queued',
                 claimed_by = NULL,
                 heartbeat_at = NULL,
                 updated_at = ?3
             WHERE workspace_id = ?1
               AND state = 'claimed'
               AND heartbeat_at < ?2",
            params![workspace_id.as_str(), expired_before, now],
        )?;
        Ok(changed as u64)
    }

    pub fn requeue_claimed_sync_operations_for_device_kind(
        &self,
        workspace_id: &WorkspaceId,
        kind: &str,
        device_id: &DeviceId,
        now: &str,
    ) -> Result<u64, MetadataError> {
        let changed = self.connection.execute(
            "UPDATE sync_operations
             SET state = 'queued',
                 claimed_by = NULL,
                 heartbeat_at = NULL,
                 updated_at = ?4
             WHERE workspace_id = ?1
               AND kind = ?2
               AND device_id = ?3
               AND state = 'claimed'",
            params![workspace_id.as_str(), kind, device_id.as_str(), now],
        )?;
        Ok(changed as u64)
    }

    pub fn requeue_waiting_retry_sync_operations_for_device_kind(
        &self,
        workspace_id: &WorkspaceId,
        kind: &str,
        device_id: &DeviceId,
        now: &str,
    ) -> Result<u64, MetadataError> {
        let changed = self.connection.execute(
            "UPDATE sync_operations
             SET state = 'queued',
                 claimed_by = NULL,
                 heartbeat_at = NULL,
                 next_attempt_at = NULL,
                 updated_at = ?4
             WHERE workspace_id = ?1
               AND kind = ?2
               AND device_id = ?3
               AND state = 'waiting_retry'",
            params![workspace_id.as_str(), kind, device_id.as_str(), now],
        )?;
        Ok(changed as u64)
    }

    pub fn requeue_attention_sync_operations_for_device_kind_with_error(
        &self,
        workspace_id: &WorkspaceId,
        kind: &str,
        device_id: &DeviceId,
        error_substring: &str,
        now: &str,
    ) -> Result<u64, MetadataError> {
        let changed = self.connection.execute(
            "UPDATE sync_operations
             SET state = 'queued',
                 claimed_by = NULL,
                 heartbeat_at = NULL,
                 next_attempt_at = NULL,
                 last_error = NULL,
                 updated_at = ?5
             WHERE workspace_id = ?1
               AND kind = ?2
               AND device_id = ?3
               AND state = 'attention'
               AND last_error LIKE ?4",
            params![
                workspace_id.as_str(),
                kind,
                device_id.as_str(),
                format!("%{error_substring}%"),
                now,
            ],
        )?;
        Ok(changed as u64)
    }

    pub fn put_remote_ref_cursor(
        &self,
        record: &RemoteRefCursorRecord,
    ) -> Result<(), MetadataError> {
        self.insert_workspace(&record.workspace_id, "Code", &record.updated_at)?;
        self.connection.execute(
            "INSERT INTO sync_remote_cursors
             (workspace_id, cursor, last_observed_version, last_observed_snapshot_id, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(workspace_id) DO UPDATE SET
               cursor = excluded.cursor,
               last_observed_version = excluded.last_observed_version,
               last_observed_snapshot_id = excluded.last_observed_snapshot_id,
               updated_at = excluded.updated_at",
            params![
                record.workspace_id.as_str(),
                record.cursor.as_deref(),
                record.last_observed_version,
                record.last_observed_snapshot_id.as_deref(),
                record.updated_at.as_str(),
            ],
        )?;
        Ok(())
    }

    pub fn remote_ref_cursor(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Option<RemoteRefCursorRecord>, MetadataError> {
        self.connection
            .query_row(
                "SELECT workspace_id, cursor, last_observed_version, last_observed_snapshot_id, updated_at
                 FROM sync_remote_cursors
                 WHERE workspace_id = ?1",
                [workspace_id.as_str()],
                |row| {
                    Ok(RemoteRefCursorRecord {
                        workspace_id: WorkspaceId::new(row.get::<_, String>(0)?),
                        cursor: row.get(1)?,
                        last_observed_version: row.get(2)?,
                        last_observed_snapshot_id: row.get(3)?,
                        updated_at: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn sync_operation_counts(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<SyncOperationCounts, MetadataError> {
        let mut counts = SyncOperationCounts::default();
        let mut statement = self.connection.prepare(
            "SELECT state, COUNT(*)
             FROM sync_operations
             WHERE workspace_id = ?1
             GROUP BY state",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
        })?;
        for row in rows {
            let (state, count) = row?;
            match state.as_str() {
                "queued" => counts.queued = count,
                "claimed" => counts.claimed = count,
                "waiting_retry" => counts.waiting_retry = count,
                "blocked_offline" => counts.blocked_offline = count,
                "attention" => counts.attention = count,
                "completed" => counts.completed = count,
                _ => {}
            }
        }
        Ok(counts)
    }

    pub fn sync_operation_counts_for_device(
        &self,
        workspace_id: &WorkspaceId,
        device_id: &DeviceId,
    ) -> Result<SyncOperationCounts, MetadataError> {
        let mut counts = SyncOperationCounts::default();
        let mut statement = self.connection.prepare(
            "SELECT state, COUNT(*)
             FROM sync_operations
             WHERE workspace_id = ?1 AND device_id = ?2
             GROUP BY state",
        )?;
        let rows = statement
            .query_map(params![workspace_id.as_str(), device_id.as_str()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
            })?;
        for row in rows {
            let (state, count) = row?;
            match state.as_str() {
                "queued" => counts.queued = count,
                "claimed" => counts.claimed = count,
                "waiting_retry" => counts.waiting_retry = count,
                "blocked_offline" => counts.blocked_offline = count,
                "attention" => counts.attention = count,
                "completed" => counts.completed = count,
                _ => {}
            }
        }
        Ok(counts)
    }

    pub fn sync_operation_by_id(
        &self,
        id: &str,
    ) -> Result<Option<SyncOperationRecord>, MetadataError> {
        self.connection
            .query_row(
                "SELECT id, workspace_id, kind, state, idempotency_key, base_version, base_snapshot_id,
                        target_snapshot_id, device_id, payload_json, attempt_count, claimed_by,
                        heartbeat_at, next_attempt_at, last_error, created_at, updated_at
                 FROM sync_operations
                 WHERE id = ?1",
                [id],
                sync_operation_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn enqueue_hydration(&self, record: &HydrationQueueRecord) -> Result<(), MetadataError> {
        let path = self.workspace_relative_path(&record.workspace_id, &record.path)?;
        self.connection.execute(
            "INSERT INTO hydration_queue
             (id, workspace_id, project_id, path, content_id, priority, state, cause, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(workspace_id, path, cause) DO UPDATE SET
               id = excluded.id,
               project_id = excluded.project_id,
               content_id = excluded.content_id,
               priority = excluded.priority,
               state = excluded.state,
               updated_at = excluded.updated_at",
            params![
                record.id.as_str(),
                record.workspace_id.as_str(),
                record.project_id.as_ref().map(|id| id.as_str()),
                path,
                record.content_id.as_ref().map(|id| id.as_str()),
                record.priority.as_str(),
                record.state.as_str(),
                record.cause.as_str(),
                record.updated_at.as_str(),
            ],
        )?;
        Ok(())
    }

    pub fn hydration_queue(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<HydrationQueueRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT id, workspace_id, project_id, path, content_id, priority, state, cause, updated_at
             FROM hydration_queue
             WHERE workspace_id = ?1
             ORDER BY CASE priority
               WHEN 'active-read' THEN 0
               WHEN 'explicit-pin' THEN 1
               WHEN 'agent-lease' THEN 2
               WHEN 'hot-project-prefetch' THEN 3
               WHEN 'index-request' THEN 4
               WHEN 'background' THEN 5
               ELSE 6
             END, updated_at, id",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], |row| {
            Ok(HydrationQueueRecord {
                id: row.get(0)?,
                workspace_id: WorkspaceId::new(row.get::<_, String>(1)?),
                project_id: row.get::<_, Option<String>>(2)?.map(ProjectId::new),
                path: row.get(3)?,
                content_id: row.get::<_, Option<String>>(4)?.map(ContentId::new),
                priority: row.get(5)?,
                state: row.get(6)?,
                cause: row.get(7)?,
                updated_at: row.get(8)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn append_local_write_log(
        &self,
        record: &LocalWriteLogRecord,
    ) -> Result<(), MetadataError> {
        let path = self.workspace_relative_path(&record.workspace_id, &record.path)?;
        let source_path = record
            .source_path
            .as_deref()
            .map(|path| self.workspace_relative_path(&record.workspace_id, path))
            .transpose()?;
        self.connection.execute(
            "INSERT INTO local_write_log
             (id, workspace_id, device_id, project_id, path, source_path, operation,
              staged_content_id, policy_classification, causation_id, settled_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                record.id.as_str(),
                record.workspace_id.as_str(),
                record.device_id.as_str(),
                record.project_id.as_ref().map(|id| id.as_str()),
                path,
                source_path,
                record.operation.as_str(),
                record.staged_content_id.as_ref().map(|id| id.as_str()),
                serialize_json_variant(&record.policy_classification)?,
                record.causation_id.as_str(),
                record.settled_at.as_str(),
                record.created_at.as_str(),
            ],
        )?;
        self.mark_index_work_pending_for_path(
            &record.workspace_id,
            record.project_id.as_ref(),
            &path,
            "local-write-log",
            &record.created_at,
        )?;
        Ok(())
    }

    pub fn local_write_log(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<LocalWriteLogRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT id, workspace_id, device_id, project_id, path, source_path, operation,
                    staged_content_id, policy_classification, causation_id, settled_at, created_at
             FROM local_write_log
             WHERE workspace_id = ?1
             ORDER BY created_at, id",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], |row| {
            Ok(LocalWriteLogRecord {
                id: row.get(0)?,
                workspace_id: WorkspaceId::new(row.get::<_, String>(1)?),
                device_id: DeviceId::new(row.get::<_, String>(2)?),
                project_id: row.get::<_, Option<String>>(3)?.map(ProjectId::new),
                path: row.get(4)?,
                source_path: row.get(5)?,
                operation: row.get(6)?,
                staged_content_id: row.get::<_, Option<String>>(7)?.map(ContentId::new),
                policy_classification: deserialize_json_variant(row.get::<_, String>(8)?)?,
                causation_id: row.get(9)?,
                settled_at: row.get(10)?,
                created_at: row.get(11)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn upsert_work_view(&self, record: &WorkViewRecord) -> Result<(), MetadataError> {
        let project_path =
            self.workspace_relative_path(&record.workspace_id, &record.project_path)?;
        self.connection.execute(
            "INSERT INTO work_views
             (id, workspace_id, project_id, project_path, name, visible_path, base_snapshot_id,
              overlay_head, overlay_version, env_profile, lifecycle, visibility, sync_state, retention_state,
              retain_until, restorable, owner_device_id, followed_by_json,
              host_materializations_json, attention_json, audit_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                     ?16, ?17, ?18, ?19, ?20, '{}', ?21, ?22)
             ON CONFLICT(id) DO UPDATE SET
               workspace_id = excluded.workspace_id,
               project_id = excluded.project_id,
               project_path = excluded.project_path,
               name = excluded.name,
               visible_path = excluded.visible_path,
               base_snapshot_id = excluded.base_snapshot_id,
               overlay_head = excluded.overlay_head,
               overlay_version = excluded.overlay_version,
               env_profile = excluded.env_profile,
               lifecycle = excluded.lifecycle,
               visibility = excluded.visibility,
               sync_state = excluded.sync_state,
               retention_state = excluded.retention_state,
               retain_until = excluded.retain_until,
               restorable = excluded.restorable,
               owner_device_id = excluded.owner_device_id,
               followed_by_json = excluded.followed_by_json,
               host_materializations_json = excluded.host_materializations_json,
               attention_json = excluded.attention_json,
               updated_at = excluded.updated_at",
            params![
                record.id.as_str(),
                record.workspace_id.as_str(),
                record.project_id.as_str(),
                project_path,
                record.name.as_str(),
                record.visible_path.as_str(),
                record.base_snapshot_id.as_str(),
                record.overlay_head.as_str(),
                record.overlay_version,
                record.env_profile.as_str(),
                serialize_json_variant(&record.lifecycle)?,
                serialize_json_variant(&record.visibility)?,
                serialize_json_variant(&record.sync_state)?,
                serialize_json_variant(&record.retention.state)?,
                record.retention.retain_until.as_deref(),
                if record.retention.restorable {
                    1_i64
                } else {
                    0_i64
                },
                record.owner_device_id.as_ref().map(|id| id.as_str()),
                serde_json::to_string(&record.followed_by)
                    .map_err(|error| MetadataError::Sqlite(json_to_sql_error(error)))?,
                serde_json::to_string(&record.host_materializations)
                    .map_err(|error| MetadataError::Sqlite(json_to_sql_error(error)))?,
                serde_json::to_string(&record.attention)
                    .map_err(|error| MetadataError::Sqlite(json_to_sql_error(error)))?,
                record.created_at.as_str(),
                record.updated_at.as_str(),
            ],
        )?;
        Ok(())
    }

    pub fn replace_work_view_base_files(
        &self,
        workspace_id: &WorkspaceId,
        work_view_id: &WorkViewId,
        files: &[(String, String)],
        captured_at: &str,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "DELETE FROM work_view_base_files
             WHERE workspace_id = ?1 AND work_view_id = ?2",
            params![workspace_id.as_str(), work_view_id.as_str()],
        )?;
        let mut statement = self.connection.prepare(
            "INSERT INTO work_view_base_files
             (workspace_id, work_view_id, path, hash, captured_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for (path, hash) in files {
            statement.execute(params![
                workspace_id.as_str(),
                work_view_id.as_str(),
                path,
                hash,
                captured_at,
            ])?;
        }
        Ok(())
    }

    pub fn work_view_base_hash(
        &self,
        workspace_id: &WorkspaceId,
        work_view_id: &WorkViewId,
        path: &str,
    ) -> Result<Option<String>, MetadataError> {
        self.connection
            .query_row(
                "SELECT hash
                 FROM work_view_base_files
                 WHERE workspace_id = ?1 AND work_view_id = ?2 AND path = ?3
                 LIMIT 1",
                params![workspace_id.as_str(), work_view_id.as_str(), path],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn work_view_base_files(
        &self,
        workspace_id: &WorkspaceId,
        work_view_id: &WorkViewId,
    ) -> Result<Vec<(String, String)>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT path, hash
             FROM work_view_base_files
             WHERE workspace_id = ?1 AND work_view_id = ?2
             ORDER BY path",
        )?;
        let rows = statement.query_map(
            params![workspace_id.as_str(), work_view_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn work_views(
        &self,
        workspace_id: &WorkspaceId,
        include_hidden: bool,
        current_device_id: Option<&DeviceId>,
    ) -> Result<Vec<WorkViewRecord>, MetadataError> {
        let base_query = "SELECT id, workspace_id, project_id, project_path, name, visible_path,
                                base_snapshot_id, overlay_head, overlay_version, env_profile, lifecycle, visibility,
                                sync_state, retention_state, retain_until, restorable, owner_device_id,
                                followed_by_json, host_materializations_json, attention_json,
                                created_at, updated_at
                         FROM work_views";
        let order_by = "ORDER BY updated_at DESC, project_path, name";
        let mut rows = Vec::new();
        if include_hidden {
            let mut statement = self.connection.prepare(&format!(
                "{base_query}
                 WHERE workspace_id = ?1
                 {order_by}",
            ))?;
            let mapped = statement.query_map([workspace_id.as_str()], work_view_from_row)?;
            for row in mapped {
                rows.push(row?);
            }
            return Ok(rows);
        }

        let mut statement = self.connection.prepare(&format!(
            "{base_query}
             WHERE workspace_id = ?1
               AND visibility != 'hidden'
               AND lifecycle IN ('active', 'review-ready')
               AND (
                 visibility IN ('pinned', 'followed')
                 OR lifecycle = 'review-ready'
                 OR owner_device_id IS NULL
                 OR owner_device_id = ?2
                 OR followed_by_json LIKE ?3
               )
             {order_by}",
        ))?;
        let current_device_id = current_device_id
            .map(DeviceId::as_str)
            .unwrap_or_default()
            .to_string();
        let followed_token = format!("%\"{current_device_id}\"%");
        let mapped = statement.query_map(
            params![workspace_id.as_str(), current_device_id, followed_token],
            work_view_from_row,
        )?;
        for row in mapped {
            rows.push(row?);
        }
        Ok(rows)
    }

    pub fn work_view_by_id(
        &self,
        workspace_id: &WorkspaceId,
        id: &WorkViewId,
    ) -> Result<Option<WorkViewRecord>, MetadataError> {
        self.connection
            .query_row(
                "SELECT id, workspace_id, project_id, project_path, name, visible_path,
                        base_snapshot_id, overlay_head, overlay_version, env_profile, lifecycle, visibility,
                        sync_state, retention_state, retain_until, restorable, owner_device_id,
                        followed_by_json, host_materializations_json, attention_json,
                        created_at, updated_at
                 FROM work_views
                 WHERE workspace_id = ?1 AND id = ?2
                 LIMIT 1",
                params![workspace_id.as_str(), id.as_str()],
                work_view_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn work_views_by_name(
        &self,
        workspace_id: &WorkspaceId,
        project_id: Option<&ProjectId>,
        name: &str,
    ) -> Result<Vec<WorkViewRecord>, MetadataError> {
        if let Some(project_id) = project_id {
            let mut statement = self.connection.prepare(
                "SELECT id, workspace_id, project_id, project_path, name, visible_path,
                        base_snapshot_id, overlay_head, overlay_version, env_profile, lifecycle, visibility,
                        sync_state, retention_state, retain_until, restorable, owner_device_id,
                        followed_by_json, host_materializations_json, attention_json,
                        created_at, updated_at
                 FROM work_views
                 WHERE workspace_id = ?1 AND project_id = ?2 AND name = ?3 COLLATE NOCASE
                 ORDER BY updated_at DESC",
            )?;
            let rows = statement.query_map(
                params![workspace_id.as_str(), project_id.as_str(), name],
                work_view_from_row,
            )?;
            return rows.collect::<Result<Vec<_>, _>>().map_err(Into::into);
        }

        let mut statement = self.connection.prepare(
            "SELECT id, workspace_id, project_id, project_path, name, visible_path,
                    base_snapshot_id, overlay_head, overlay_version, env_profile, lifecycle, visibility,
                    sync_state, retention_state, retain_until, restorable, owner_device_id,
                    followed_by_json, host_materializations_json, attention_json,
                    created_at, updated_at
             FROM work_views
             WHERE workspace_id = ?1 AND name = ?2 COLLATE NOCASE
             ORDER BY updated_at DESC",
        )?;
        let rows = statement.query_map(params![workspace_id.as_str(), name], work_view_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub(crate) fn workspace_relative_path(
        &self,
        workspace_id: &WorkspaceId,
        path: &str,
    ) -> Result<String, MetadataError> {
        let path = normalize_path_for_matching(path);
        for root in self.accepted_roots(workspace_id)? {
            let root = normalize_path_for_matching(&root);
            if let Some(relative) = strip_root_prefix(&path, &root) {
                return Ok(normalize_workspace_path(relative));
            }
        }

        Ok(normalize_workspace_path(&path))
    }

    pub fn set_component_state(
        &self,
        component: &str,
        state: &str,
        updated_at: &str,
    ) -> Result<(), MetadataError> {
        self.connection.execute(
            "INSERT OR REPLACE INTO component_states (component, state, updated_at)
             VALUES (?1, ?2, ?3)",
            params![component, state, updated_at],
        )?;
        Ok(())
    }

    pub fn event_watermarks(&self) -> Result<EventWatermarks, MetadataError> {
        let last_event_id = self
            .connection
            .query_row(
                "SELECT id FROM events ORDER BY occurred_at DESC, id DESC LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;

        let sync_state = self.component_state("sync")?;
        let watcher_state = self.component_state("watcher")?;
        let network_state = match self
            .component_state_raw("network")?
            .as_deref()
            .unwrap_or("online")
        {
            "offline" => Some(NetworkState::Offline),
            "degraded" => Some(NetworkState::Degraded),
            "online" => Some(NetworkState::Online),
            _ => None,
        };

        Ok(EventWatermarks {
            last_scan_at: self.last_scan_at()?,
            last_event_id: last_event_id.map(bowline_core::ids::EventId::new),
            event_lag_ms: Some(0),
            sync_state,
            watcher_state,
            network_state,
        })
    }

    pub fn with_transaction<T>(
        &mut self,
        f: impl FnOnce(&Transaction<'_>) -> rusqlite::Result<T>,
    ) -> Result<T, MetadataError> {
        let transaction = self.connection.transaction()?;
        let result = f(&transaction)?;
        transaction.commit()?;
        Ok(result)
    }

    pub(crate) fn connection(&self) -> &Connection {
        &self.connection
    }

    fn component_state(&self, component: &str) -> Result<Option<ComponentState>, MetadataError> {
        Ok(match self.component_state_raw(component)?.as_deref() {
            Some("ready") => Some(ComponentState::Ready),
            Some("degraded") => Some(ComponentState::Degraded),
            Some("unavailable") => Some(ComponentState::Unavailable),
            _ => None,
        })
    }

    fn component_state_raw(&self, component: &str) -> Result<Option<String>, MetadataError> {
        self.connection
            .query_row(
                "SELECT state FROM component_states WHERE component = ?1",
                [component],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into)
    }

    fn last_scan_at(&self) -> Result<Option<String>, MetadataError> {
        self.connection
            .query_row(
                "SELECT watermark FROM indexes
                 WHERE kind = 'scan-summary'
                 ORDER BY updated_at DESC, id DESC
                 LIMIT 1",
                [],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map(|value| value.flatten())
            .map_err(Into::into)
    }

    pub fn accepted_roots(&self, workspace_id: &WorkspaceId) -> Result<Vec<String>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT accepted_path FROM roots
             WHERE workspace_id = ?1 AND state = 'accepted'
             ORDER BY length(accepted_path) DESC",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn accepted_root_id_for_path(
        &self,
        workspace_id: &WorkspaceId,
        accepted_path: &str,
    ) -> Result<Option<String>, MetadataError> {
        self.connection
            .query_row(
                "SELECT id FROM roots
                 WHERE workspace_id = ?1 AND accepted_path = ?2 AND state = 'accepted'",
                params![workspace_id.as_str(), accepted_path],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into)
    }
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

fn configure_connection(connection: &Connection) -> Result<(), MetadataError> {
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

fn initialize_schema(connection: &Connection) -> Result<(), MetadataError> {
    let existing_version = current_schema_version(connection)?;
    if existing_version > CURRENT_SCHEMA_VERSION {
        return Err(MetadataError::FutureIncompatible {
            found: existing_version,
            supported: CURRENT_SCHEMA_VERSION,
        });
    }
    if existing_version == 0 && user_schema_has_tables(connection)? {
        return Err(MetadataError::UnsupportedSchema);
    }
    if existing_version > 0 && existing_version < CURRENT_SCHEMA_VERSION {
        reinitialize_known_bowline_tables(connection)?;
    }

    connection.execute_batch(SCHEMA_CORE)?;
    connection.execute_batch(SCHEMA_MATERIALIZATION)?;
    connection.execute_batch(SCHEMA_ENV_SETUP_INDEXES)?;
    connection.execute_batch(SCHEMA_WORK_VIEWS)?;
    repair_work_view_tables(connection)?;
    repair_derived_index_tables(connection)?;
    connection.execute_batch(SCHEMA_INDEXING)?;
    connection.pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION)?;
    Ok(())
}

fn reinitialize_known_bowline_tables(connection: &Connection) -> Result<(), MetadataError> {
    connection.pragma_update(None, "foreign_keys", "OFF")?;
    for table in TABLES.iter().rev() {
        connection.execute(&format!("DROP TABLE IF EXISTS {table}"), [])?;
    }
    connection.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

fn repair_work_view_tables(connection: &Connection) -> Result<(), MetadataError> {
    if table_exists(connection, "work_views")?
        && !table_has_column(connection, "work_views", "overlay_version")?
    {
        connection.execute(
            "ALTER TABLE work_views ADD COLUMN overlay_version INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    Ok(())
}

fn repair_derived_index_tables(connection: &Connection) -> Result<(), MetadataError> {
    let index_documents_stale = table_exists(connection, "index_documents")?
        && !table_has_column(connection, "index_documents", "access_json")?;
    let symbol_records_stale = table_exists(connection, "symbol_records")?
        && (!table_has_column(connection, "symbol_records", "project_id")?
            || !table_has_column(connection, "symbol_records", "access_json")?);
    if index_documents_stale || symbol_records_stale {
        connection.execute_batch(
            "DROP TABLE IF EXISTS index_documents;
             DROP TABLE IF EXISTS symbol_records;
             DROP TABLE IF EXISTS index_packs;
             DROP TABLE IF EXISTS index_work;",
        )?;
    }
    Ok(())
}

fn table_exists(connection: &Connection, table: &str) -> Result<bool, MetadataError> {
    connection
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sqlite_master
                WHERE type = 'table'
                  AND name = ?1
            )",
            [table],
            |row| row.get::<_, bool>(0),
        )
        .map_err(Into::into)
}

fn table_has_column(
    connection: &Connection,
    table: &str,
    column: &str,
) -> Result<bool, MetadataError> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(columns.iter().any(|name| name == column))
}

fn current_schema_version(connection: &Connection) -> Result<u32, MetadataError> {
    connection
        .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
        .map_err(Into::into)
}

fn user_schema_has_tables(connection: &Connection) -> Result<bool, MetadataError> {
    connection
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sqlite_master
                WHERE type = 'table'
                  AND name NOT LIKE 'sqlite_%'
            )",
            [],
            |row| row.get::<_, bool>(0),
        )
        .map_err(Into::into)
}

fn inspect_open_connection(connection: &Connection) -> DatabaseState {
    match current_schema_version(connection) {
        Ok(0) => match user_schema_has_tables(connection) {
            Ok(true) => DatabaseState::UnsupportedSchema,
            Ok(false) => DatabaseState::Empty,
            Err(MetadataError::Sqlite(error)) => classify_open_error(&error),
            Err(_) => DatabaseState::Corrupt,
        },
        Ok(version) if version > CURRENT_SCHEMA_VERSION => DatabaseState::FutureIncompatible {
            found: version,
            supported: CURRENT_SCHEMA_VERSION,
        },
        Ok(version) if version < CURRENT_SCHEMA_VERSION => DatabaseState::Current,
        Ok(_) => DatabaseState::Current,
        Err(MetadataError::Sqlite(error)) => classify_open_error(&error),
        Err(_) => DatabaseState::Corrupt,
    }
}

fn classify_open_error(error: &rusqlite::Error) -> DatabaseState {
    match error {
        rusqlite::Error::SqliteFailure(sqlite_error, _) => {
            use rusqlite::ffi;
            match sqlite_error.code {
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked => {
                    DatabaseState::Locked
                }
                rusqlite::ErrorCode::CannotOpen
                    if sqlite_error.extended_code == ffi::SQLITE_CANTOPEN =>
                {
                    DatabaseState::PermissionDenied
                }
                rusqlite::ErrorCode::NotADatabase | rusqlite::ErrorCode::DatabaseCorrupt => {
                    DatabaseState::Corrupt
                }
                _ => DatabaseState::Corrupt,
            }
        }
        _ => DatabaseState::Corrupt,
    }
}

fn normalize_path_for_matching(path: &str) -> String {
    let mut normalized = expand_tilde_for_matching(path).replace('\\', "/");
    while normalized.contains("//") {
        normalized = normalized.replace("//", "/");
    }
    normalized.trim_end_matches('/').to_string()
}

fn expand_tilde_for_matching(path: &str) -> String {
    let Some(rest) = path.strip_prefix('~') else {
        return path.to_string();
    };
    if !(rest.is_empty() || rest.starts_with('/')) {
        return path.to_string();
    }
    let Some(home) = std::env::var_os("HOME") else {
        return path.to_string();
    };

    format!("{}{}", home.to_string_lossy(), rest)
}

fn upsert_env_record_tx(
    transaction: &Transaction<'_>,
    record: &EnvRecord,
) -> Result<(), rusqlite::Error> {
    transaction.execute(
        "INSERT INTO env_records
         (id, workspace_id, project_id, source_path, key_name, access,
          value_ciphertext_ref, updated_at, profile, occurrence_index, line_kind,
          encrypted_locator_json, format_json, materialization_state, restriction_state,
          key_epoch, metadata_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
         ON CONFLICT(id) DO UPDATE SET
           workspace_id = excluded.workspace_id,
           project_id = excluded.project_id,
           source_path = excluded.source_path,
           key_name = excluded.key_name,
           access = excluded.access,
           value_ciphertext_ref = excluded.value_ciphertext_ref,
           updated_at = excluded.updated_at,
           profile = excluded.profile,
           occurrence_index = excluded.occurrence_index,
           line_kind = excluded.line_kind,
           encrypted_locator_json = excluded.encrypted_locator_json,
           format_json = excluded.format_json,
           materialization_state = excluded.materialization_state,
           restriction_state = excluded.restriction_state,
           key_epoch = excluded.key_epoch,
           metadata_json = excluded.metadata_json",
        params![
            record.id.as_str(),
            record.workspace_id.as_str(),
            record.project_id.as_ref().map(|id| id.as_str()),
            record.source_path.as_str(),
            record.key_name.as_str(),
            serialize_access_flags(&record.access)?,
            record.updated_at.as_str(),
            record.profile.as_str(),
            record.occurrence_index,
            record.line_kind.as_str(),
            record.encrypted_locator_json.as_str(),
            record.format_json.as_str(),
            record.materialization_state.as_str(),
            record.restriction_state.as_str(),
            record.key_epoch,
            record.metadata_json.as_str(),
        ],
    )?;
    Ok(())
}

fn env_record_from_row(row: &rusqlite::Row<'_>) -> Result<EnvRecord, rusqlite::Error> {
    let occurrence_index = row.get::<_, i64>(8)?;
    let key_epoch = row.get::<_, i64>(14)?;
    Ok(EnvRecord {
        id: EnvRecordId::new(row.get::<_, String>(0)?),
        workspace_id: WorkspaceId::new(row.get::<_, String>(1)?),
        project_id: row.get::<_, Option<String>>(2)?.map(ProjectId::new),
        source_path: row.get(3)?,
        key_name: row.get(4)?,
        access: deserialize_access_flags(row.get::<_, String>(5)?)?,
        updated_at: row.get(6)?,
        profile: row.get(7)?,
        occurrence_index: u32::try_from(occurrence_index).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                8,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        line_kind: row.get(9)?,
        encrypted_locator_json: row.get(10)?,
        format_json: row.get(11)?,
        materialization_state: row.get(12)?,
        restriction_state: row.get(13)?,
        key_epoch: u32::try_from(key_epoch).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                14,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        metadata_json: row.get(15)?,
    })
}

fn index_document_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<IndexDocumentRecord, rusqlite::Error> {
    Ok(IndexDocumentRecord {
        workspace_id: WorkspaceId::new(row.get::<_, String>(0)?),
        project_id: row.get::<_, Option<String>>(1)?.map(ProjectId::new),
        path: row.get(2)?,
        snapshot_id: row.get::<_, Option<String>>(3)?.map(SnapshotId::new),
        content_id: row.get::<_, Option<String>>(4)?.map(ContentId::new),
        classification: deserialize_json_variant(row.get::<_, String>(5)?)?,
        mode: deserialize_json_variant(row.get::<_, String>(6)?)?,
        access: deserialize_access_flags(row.get::<_, String>(7)?)?,
        policy_summary: row.get(8)?,
        body_text: row.get(9)?,
        hydration_state: deserialize_json_variant(row.get::<_, String>(10)?)?,
        indexed_bytes: row.get(11)?,
        source_watermark: row.get(12)?,
        indexed_watermark: row.get(13)?,
        state: row.get(14)?,
        updated_at: row.get(15)?,
    })
}

fn symbol_index_record_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<SymbolIndexRecord, rusqlite::Error> {
    Ok(SymbolIndexRecord {
        id: row.get(0)?,
        workspace_id: WorkspaceId::new(row.get::<_, String>(1)?),
        project_id: row.get::<_, Option<String>>(2)?.map(ProjectId::new),
        path: row.get(3)?,
        snapshot_id: row.get::<_, Option<String>>(4)?.map(SnapshotId::new),
        name: row.get(5)?,
        kind: row.get(6)?,
        language: row.get(7)?,
        line_start: row.get(8)?,
        line_end: row.get(9)?,
        byte_start: row.get(10)?,
        byte_end: row.get(11)?,
        parser_status: row.get(12)?,
        access: deserialize_access_flags(row.get::<_, String>(13)?)?,
        updated_at: row.get(14)?,
    })
}

fn index_work_from_row(row: &rusqlite::Row<'_>) -> Result<IndexWorkRecord, rusqlite::Error> {
    Ok(IndexWorkRecord {
        id: row.get(0)?,
        workspace_id: WorkspaceId::new(row.get::<_, String>(1)?),
        project_id: row.get::<_, Option<String>>(2)?.map(ProjectId::new),
        path: row.get(3)?,
        kind: row.get(4)?,
        source_watermark: row.get(5)?,
        indexed_watermark: row.get(6)?,
        state: row.get(7)?,
        reason: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

fn index_pack_from_row(row: &rusqlite::Row<'_>) -> Result<IndexPackRecord, rusqlite::Error> {
    let byte_len: i64 = row.get(4)?;
    Ok(IndexPackRecord {
        workspace_id: WorkspaceId::new(row.get::<_, String>(0)?),
        project_id: row.get::<_, Option<String>>(1)?.map(ProjectId::new),
        snapshot_id: row.get::<_, Option<String>>(2)?.map(SnapshotId::new),
        object_key: row.get(3)?,
        byte_len: byte_len.try_into().unwrap_or(0),
        hash: row.get(5)?,
        state: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

fn next_index_source_watermark(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
) -> Result<u64, MetadataError> {
    let value: i64 = store.connection.query_row(
        "SELECT COALESCE(MAX(source_watermark), 0) + 1
         FROM index_work
         WHERE workspace_id = ?1 AND project_id = ?2",
        params![workspace_id.as_str(), project_id.as_str()],
        |row| row.get(0),
    )?;
    Ok(value.try_into().unwrap_or(1))
}

fn project_index_initialized(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
) -> Result<bool, MetadataError> {
    let count: i64 = store.connection.query_row(
        "SELECT
           (SELECT COUNT(*) FROM index_work WHERE workspace_id = ?1 AND project_id = ?2) +
           (SELECT COUNT(*) FROM index_documents WHERE workspace_id = ?1 AND project_id = ?2)",
        params![workspace_id.as_str(), project_id.as_str()],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn stable_store_token(value: &str) -> String {
    blake3::hash(value.as_bytes())
        .to_hex()
        .chars()
        .take(16)
        .collect()
}

fn is_work_namespace_path(path: &str) -> bool {
    let normalized = normalize_workspace_path(path);
    normalized == ".work" || normalized.starts_with(".work/") || normalized.contains("/.work/")
}

fn path_is_under_prefix(path: &str, prefix: &str) -> bool {
    let path = normalize_workspace_path(path);
    let prefix = normalize_workspace_path(prefix);
    !prefix.is_empty() && (path == prefix || path.starts_with(&format!("{prefix}/")))
}

fn project_relative_index_path(workspace_relative_path: &str, project_path: &str) -> String {
    let path = normalize_workspace_path(workspace_relative_path);
    let project_path = normalize_workspace_path(project_path);
    if project_path.is_empty() {
        return path;
    }
    path.strip_prefix(&format!("{project_path}/"))
        .map(str::to_string)
        .unwrap_or(path)
}

fn setup_receipt_from_row(row: &rusqlite::Row<'_>) -> Result<SetupReceiptRecord, rusqlite::Error> {
    Ok(SetupReceiptRecord {
        id: row.get(0)?,
        workspace_id: WorkspaceId::new(row.get::<_, String>(1)?),
        project_id: row.get::<_, Option<String>>(2)?.map(ProjectId::new),
        command: row.get(3)?,
        state: row.get(4)?,
        receipt_json: row.get(5)?,
        updated_at: row.get(6)?,
        recipe_hash: row.get(7)?,
        approval_state: row.get(8)?,
        trigger: row.get(9)?,
        cwd: row.get(10)?,
        os: row.get(11)?,
        arch: row.get(12)?,
        env_profile: row.get(13)?,
        output_path: row.get(14)?,
        redacted_summary: row.get(15)?,
    })
}

fn command_idempotency_record_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<CommandIdempotencyRecord, rusqlite::Error> {
    Ok(CommandIdempotencyRecord {
        workspace_id: WorkspaceId::new(row.get::<_, String>(0)?),
        idempotency_key: row.get(1)?,
        command: row.get(2)?,
        request_hash: row.get(3)?,
        result_json: row.get(4)?,
        status: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
        expires_at: row.get(8)?,
    })
}

fn agent_lease_from_row(row: &rusqlite::Row<'_>) -> Result<AgentLeaseRecord, rusqlite::Error> {
    serde_json::from_str(&row.get::<_, String>(0)?).map_err(json_to_sql_read_error)
}

fn work_view_from_row(row: &rusqlite::Row<'_>) -> Result<WorkViewRecord, rusqlite::Error> {
    Ok(WorkViewRecord {
        id: WorkViewId::new(row.get::<_, String>(0)?),
        workspace_id: WorkspaceId::new(row.get::<_, String>(1)?),
        project_id: ProjectId::new(row.get::<_, String>(2)?),
        project_path: row.get(3)?,
        name: row.get(4)?,
        visible_path: row.get(5)?,
        base_snapshot_id: SnapshotId::new(row.get::<_, String>(6)?),
        overlay_head: row.get(7)?,
        overlay_version: row.get(8)?,
        env_profile: row.get(9)?,
        lifecycle: deserialize_json_variant::<WorkViewLifecycle>(row.get::<_, String>(10)?)?,
        visibility: deserialize_json_variant::<WorkViewVisibility>(row.get::<_, String>(11)?)?,
        sync_state: deserialize_json_variant::<WorkViewSyncState>(row.get::<_, String>(12)?)?,
        retention: WorkViewRetention {
            state: deserialize_json_variant::<WorkViewRetentionState>(row.get::<_, String>(13)?)?,
            retain_until: row.get(14)?,
            restorable: row.get::<_, i64>(15)? != 0,
        },
        owner_device_id: row.get::<_, Option<String>>(16)?.map(DeviceId::new),
        followed_by: serde_json::from_str(&row.get::<_, String>(17)?)
            .map_err(json_to_sql_read_error)?,
        host_materializations: serde_json::from_str(&row.get::<_, String>(18)?)
            .map_err(json_to_sql_read_error)?,
        attention: serde_json::from_str(&row.get::<_, String>(19)?)
            .map_err(json_to_sql_read_error)?,
        created_at: row.get(20)?,
        updated_at: row.get(21)?,
    })
}

fn serialize_access_flags(access: &[AccessFlag]) -> Result<String, rusqlite::Error> {
    serde_json::to_string(access).map_err(json_to_sql_error)
}

fn deserialize_access_flags(value: String) -> Result<Vec<AccessFlag>, rusqlite::Error> {
    serde_json::from_str(&value).or_else(|_| {
        serde_json::from_str::<AccessFlag>(&value)
            .map(|flag| vec![flag])
            .map_err(json_to_sql_read_error)
    })
}

fn strip_root_prefix<'a>(path: &'a str, root: &str) -> Option<&'a str> {
    if path == root {
        return Some("");
    }

    path.strip_prefix(&format!("{root}/"))
}

fn scan_summary_id(workspace_id: &WorkspaceId) -> String {
    format!("scan-summary:{}", workspace_id.as_str())
}

fn local_path_id(workspace_id: &WorkspaceId, path: &str) -> String {
    format!("{}:{path}", workspace_id.as_str())
}

fn serialize_json_variant<T>(value: &T) -> Result<String, rusqlite::Error>
where
    T: serde::Serialize,
{
    serde_json::to_value(value)
        .map_err(json_to_sql_error)?
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| rusqlite::Error::ToSqlConversionFailure("expected string enum".into()))
}

fn validate_pack_kind(kind: &str) -> Result<(), MetadataError> {
    if matches!(
        kind,
        "source-pack"
            | "large-chunk"
            | "snapshot-manifest"
            | "locator-index"
            | "index-pack"
            | "overlay-pack"
            | "agent-overlay"
    ) {
        Ok(())
    } else {
        Err(MetadataError::InvalidStorageMetadata(format!(
            "unsupported pack kind `{kind}`"
        )))
    }
}

fn validate_pack_state(state: &str) -> Result<(), MetadataError> {
    if matches!(
        state,
        "pending" | "current" | "orphan-candidate" | "retained" | "delete-eligible"
    ) {
        Ok(())
    } else {
        Err(MetadataError::InvalidStorageMetadata(format!(
            "unsupported pack state `{state}`"
        )))
    }
}

fn validate_locator_shape(locator: &ContentLocator) -> Result<(), MetadataError> {
    match locator.storage {
        ContentStorage::Packed => {
            if locator.pack_id.is_some() && locator.offset.is_some() && locator.length.is_some() {
                Ok(())
            } else {
                Err(MetadataError::InvalidStorageMetadata(
                    "packed locators require pack_id, offset, and length".to_string(),
                ))
            }
        }
        ContentStorage::Inline | ContentStorage::Chunked => {
            if locator.pack_id.is_none() && locator.offset.is_none() && locator.length.is_none() {
                Ok(())
            } else {
                Err(MetadataError::InvalidStorageMetadata(
                    "non-packed locators must not carry pack ranges".to_string(),
                ))
            }
        }
    }
}

fn ensure_pack_exists(
    connection: &Connection,
    workspace_id: &WorkspaceId,
    pack_id: &PackId,
) -> Result<(), MetadataError> {
    let exists = connection
        .query_row(
            "SELECT 1 FROM packs WHERE workspace_id = ?1 AND id = ?2",
            params![workspace_id.as_str(), pack_id.as_str()],
            |_| Ok(()),
        )
        .optional()?
        .is_some();

    if exists {
        Ok(())
    } else {
        Err(MetadataError::InvalidStorageMetadata(format!(
            "packed locator references missing pack `{}`",
            pack_id.as_str()
        )))
    }
}

fn deserialize_json_variant<T>(value: String) -> Result<T, rusqlite::Error>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value(serde_json::Value::String(value)).map_err(json_to_sql_error)
}

fn content_locator_from_row(row: &rusqlite::Row<'_>) -> Result<ContentLocator, rusqlite::Error> {
    let scalar = ContentLocator {
        content_id: ContentId::new(row.get::<_, String>(1)?),
        storage: deserialize_json_variant::<ContentStorage>(row.get::<_, String>(2)?)?,
        raw_size: row.get(3)?,
        pack_id: row.get::<_, Option<String>>(4)?.map(PackId::new),
        offset: row.get(5)?,
        length: row.get(6)?,
        chunk_ids: Vec::new(),
    };
    let canonical: ContentLocator =
        serde_json::from_str(&row.get::<_, String>(7)?).map_err(json_to_sql_read_error)?;

    if canonical.content_id != scalar.content_id
        || canonical.storage != scalar.storage
        || canonical.raw_size != scalar.raw_size
        || canonical.pack_id != scalar.pack_id
        || canonical.offset != scalar.offset
        || canonical.length != scalar.length
    {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            7,
            rusqlite::types::Type::Text,
            Box::new(io::Error::new(
                io::ErrorKind::InvalidData,
                "locator_json drifted from indexed locator columns",
            )),
        ));
    }

    Ok(canonical)
}

fn projected_node_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<ProjectedNodeRecord, rusqlite::Error> {
    Ok(ProjectedNodeRecord {
        workspace_id: WorkspaceId::new(row.get::<_, String>(0)?),
        node_id: row.get(1)?,
        project_id: row.get::<_, Option<String>>(2)?.map(ProjectId::new),
        parent_node_id: row.get(3)?,
        path: row.get(4)?,
        kind: deserialize_json_variant(row.get::<_, String>(5)?)?,
        content_id: row.get::<_, Option<String>>(6)?.map(ContentId::new),
        hydration_state: deserialize_json_variant(row.get::<_, String>(7)?)?,
        updated_at: row.get(8)?,
    })
}

fn sync_operation_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<SyncOperationRecord, rusqlite::Error> {
    Ok(SyncOperationRecord {
        id: row.get(0)?,
        workspace_id: WorkspaceId::new(row.get::<_, String>(1)?),
        kind: row.get(2)?,
        state: row.get(3)?,
        idempotency_key: row.get(4)?,
        base_version: row.get(5)?,
        base_snapshot_id: row.get(6)?,
        target_snapshot_id: row.get(7)?,
        device_id: row.get::<_, Option<String>>(8)?.map(DeviceId::new),
        payload_json: row.get(9)?,
        attempt_count: row.get(10)?,
        claimed_by: row.get(11)?,
        heartbeat_at: row.get(12)?,
        next_attempt_at: row.get(13)?,
        last_error: row.get(14)?,
        created_at: row.get(15)?,
        updated_at: row.get(16)?,
    })
}

fn sync_operation_checkpoint_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<SyncOperationCheckpointRecord, rusqlite::Error> {
    Ok(SyncOperationCheckpointRecord {
        id: row.get(0)?,
        workspace_id: WorkspaceId::new(row.get::<_, String>(1)?),
        operation_id: row.get(2)?,
        step: row.get(3)?,
        state: row.get(4)?,
        payload_json: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

fn json_to_sql_error(error: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(error))
}

fn json_to_sql_read_error(error: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}

fn project_path_candidates(path: &str) -> Vec<String> {
    let path = normalize_workspace_path(path);
    if path.is_empty() {
        return vec![String::new()];
    }

    let mut candidates = Vec::new();
    let mut parts = path.split('/').collect::<Vec<_>>();
    while !parts.is_empty() {
        candidates.push(parts.join("/"));
        parts.pop();
    }
    candidates
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use bowline_core::{
        ids::{ContentId, DeviceId, PackId, ProjectId, SnapshotId, WorkspaceId},
        policy::{AccessFlag, MaterializationMode, PathClassification},
        workspace_graph::{ContentLocator, ContentStorage, HydrationState, NamespaceEntryKind},
    };

    use crate::{metadata::schema::CURRENT_SCHEMA_VERSION, workspace::TempWorkspace};
    use rusqlite::Connection;

    use bowline_core::ids::EnvRecordId;

    use super::{
        CommandIdempotencyRecord, DatabaseState, EnvRecord, HydrationQueueRecord,
        IndexDocumentRecord, IndexPackRecord, IndexWorkRecord, LocalWriteLogRecord, MetadataError,
        MetadataStore, ProjectedNodeRecord, SetupReceiptRecord,
    };

    #[test]
    fn schema_initialization_is_idempotent_and_enables_wal() {
        let temp = TempWorkspace::new("metadata-idempotent").expect("temp workspace");
        let db_path = temp.root().join(".state").join("local.sqlite3");

        let store = MetadataStore::open(&db_path).expect("metadata opens");
        assert_eq!(store.journal_mode().expect("journal mode"), "wal");
        store.assert_schema_tables().expect("schema tables exist");
        drop(store);

        let reopened = MetadataStore::open(&db_path).expect("metadata reopens");
        reopened
            .assert_schema_tables()
            .expect("schema tables exist");
    }

    #[test]
    fn command_idempotency_reservation_does_not_overwrite_conflicts() {
        let temp = TempWorkspace::new("metadata-command-idempotency").expect("temp workspace");
        let db_path = temp.root().join(".state").join("local.sqlite3");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        let workspace_id = WorkspaceId::new("ws_idem");
        store
            .insert_workspace(&workspace_id, "Idempotency", "2026-06-29T12:00:00Z")
            .expect("workspace");

        let mut record = CommandIdempotencyRecord {
            workspace_id: workspace_id.clone(),
            idempotency_key: "key-1".to_string(),
            command: "workon".to_string(),
            request_hash: "hash-a".to_string(),
            result_json: "{}".to_string(),
            status: "pending".to_string(),
            created_at: "2026-06-29T12:00:00Z".to_string(),
            updated_at: "2026-06-29T12:00:00Z".to_string(),
            expires_at: "2026-07-06T12:00:00Z".to_string(),
        };
        assert!(
            store
                .try_insert_command_idempotency_record(&record)
                .expect("reservation insert")
        );

        let mut conflicting = record.clone();
        conflicting.request_hash = "hash-b".to_string();
        assert!(
            !store
                .try_insert_command_idempotency_record(&conflicting)
                .expect("conflicting reservation ignored")
        );

        record.result_json = "{\"ok\":true}".to_string();
        record.status = "success".to_string();
        store
            .finish_command_idempotency_record(&record)
            .expect("finish reservation");
        conflicting.result_json = "{\"ok\":false}".to_string();
        conflicting.status = "success".to_string();
        store
            .upsert_command_idempotency_record(&conflicting)
            .expect("conflicting upsert is ignored");

        let stored = store
            .command_idempotency_record(&workspace_id, "key-1")
            .expect("stored record")
            .expect("record exists");
        assert_eq!(stored.request_hash, "hash-a");
        assert_eq!(stored.result_json, "{\"ok\":true}");
    }

    #[test]
    fn older_versioned_schema_is_reinitialized_with_current_schema() {
        let temp = TempWorkspace::new("metadata-version-reinitialized").expect("temp workspace");
        let db_path = temp.root().join(".state").join("local.sqlite3");
        fs::create_dir_all(db_path.parent().expect("db parent")).expect("state dir");
        let connection = Connection::open(&db_path).expect("old db");
        connection
            .execute_batch(
                "PRAGMA user_version = 1;
                 CREATE TABLE projects (
                   id TEXT PRIMARY KEY,
                   path TEXT NOT NULL
                 );
                 INSERT INTO projects (id, path) VALUES ('old-project', 'old');",
            )
            .expect("old schema version");
        drop(connection);

        let store = MetadataStore::open(&db_path).expect("old version opens and reinitializes");
        store.assert_schema_tables().expect("schema tables exist");
        assert_eq!(
            store
                .project_count(&WorkspaceId::new("ws_code"))
                .unwrap_or(0),
            0
        );
        drop(store);
        assert_eq!(
            MetadataStore::inspect(&db_path).state,
            DatabaseState::Current
        );
        let connection = Connection::open(&db_path).expect("inspect db");
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .expect("schema version"),
            CURRENT_SCHEMA_VERSION
        );
    }

    #[test]
    fn current_version_schema_rebuilds_stale_derived_index_tables() {
        let temp = TempWorkspace::new("metadata-index-cache-repaired").expect("temp workspace");
        let db_path = temp.root().join(".state").join("local.sqlite3");
        fs::create_dir_all(db_path.parent().expect("db parent")).expect("state dir");
        let connection = Connection::open(&db_path).expect("old db");
        connection
            .execute_batch(&format!(
                "PRAGMA user_version = {CURRENT_SCHEMA_VERSION};
                 CREATE TABLE index_documents (
                   workspace_id TEXT NOT NULL,
                   project_id TEXT,
                   path TEXT NOT NULL,
                   body_text TEXT NOT NULL,
                   state TEXT NOT NULL,
                   PRIMARY KEY (workspace_id, path)
                 );
                 CREATE TABLE symbol_records (
                   id TEXT PRIMARY KEY,
                   workspace_id TEXT NOT NULL,
                   project_id TEXT NOT NULL,
                   path TEXT NOT NULL,
                   name TEXT NOT NULL
                 );"
            ))
            .expect("old index table");
        drop(connection);

        let store = MetadataStore::open(&db_path).expect("old version opens and repairs");
        store.assert_schema_tables().expect("schema tables exist");
        drop(store);

        let connection = Connection::open(&db_path).expect("inspect db");
        let has_access_json: bool = connection
            .prepare("PRAGMA table_info(index_documents)")
            .expect("table info")
            .query_map([], |row| row.get::<_, String>(1))
            .expect("columns")
            .collect::<Result<Vec<_>, _>>()
            .expect("column names")
            .iter()
            .any(|name| name == "access_json");
        assert!(has_access_json);
        let symbol_has_access_json: bool = connection
            .prepare("PRAGMA table_info(symbol_records)")
            .expect("symbol table info")
            .query_map([], |row| row.get::<_, String>(1))
            .expect("symbol columns")
            .collect::<Result<Vec<_>, _>>()
            .expect("symbol column names")
            .iter()
            .any(|name| name == "access_json");
        assert!(symbol_has_access_json);
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .expect("schema version"),
            CURRENT_SCHEMA_VERSION
        );
    }

    #[test]
    fn current_version_schema_repairs_missing_work_view_overlay_version() {
        let temp = TempWorkspace::new("metadata-work-view-overlay-version-repaired")
            .expect("temp workspace");
        let db_path = temp.root().join(".state").join("local.sqlite3");
        fs::create_dir_all(db_path.parent().expect("db parent")).expect("state dir");
        let connection = Connection::open(&db_path).expect("old db");
        connection
            .execute_batch(&format!(
                "PRAGMA user_version = {CURRENT_SCHEMA_VERSION};
                 CREATE TABLE work_views (
                   id TEXT PRIMARY KEY,
                   workspace_id TEXT NOT NULL,
                   project_id TEXT NOT NULL,
                   project_path TEXT NOT NULL,
                   name TEXT NOT NULL,
                   visible_path TEXT NOT NULL,
                   base_snapshot_id TEXT NOT NULL,
                   overlay_head TEXT NOT NULL,
                   env_profile TEXT NOT NULL DEFAULT 'default',
                   lifecycle TEXT NOT NULL,
                   visibility TEXT NOT NULL,
                   sync_state TEXT NOT NULL,
                   retention_state TEXT NOT NULL,
                   retain_until TEXT,
                   restorable INTEGER NOT NULL DEFAULT 1,
                   owner_device_id TEXT,
                   followed_by_json TEXT NOT NULL DEFAULT '[]',
                   host_materializations_json TEXT NOT NULL DEFAULT '[]',
                   attention_json TEXT NOT NULL DEFAULT '[]',
                   audit_json TEXT NOT NULL DEFAULT '{{}}',
                   created_at TEXT NOT NULL,
                   updated_at TEXT NOT NULL
                 );"
            ))
            .expect("old work views table");
        drop(connection);

        let store = MetadataStore::open(&db_path).expect("old version opens and repairs");
        store.assert_schema_tables().expect("schema tables exist");
        drop(store);

        let connection = Connection::open(&db_path).expect("inspect db");
        let has_overlay_version: bool = connection
            .prepare("PRAGMA table_info(work_views)")
            .expect("table info")
            .query_map([], |row| row.get::<_, String>(1))
            .expect("columns")
            .collect::<Result<Vec<_>, _>>()
            .expect("column names")
            .iter()
            .any(|name| name == "overlay_version");
        assert!(has_overlay_version);
    }

    #[test]
    fn unversioned_existing_schema_is_refused_without_stamping_current() {
        let temp = TempWorkspace::new("metadata-unversioned-refused").expect("temp workspace");
        let db_path = temp.root().join(".state").join("local.sqlite3");
        fs::create_dir_all(db_path.parent().expect("db parent")).expect("state dir");
        let connection = Connection::open(&db_path).expect("unversioned db");
        connection
            .execute_batch(
                "CREATE TABLE packs (
                    id TEXT PRIMARY KEY,
                    byte_len INTEGER NOT NULL
                );",
            )
            .expect("unversioned schema");
        drop(connection);

        let error = MetadataStore::open(&db_path).expect_err("unversioned store is refused");
        assert!(matches!(error, MetadataError::UnsupportedSchema));
        assert_eq!(
            MetadataStore::inspect(&db_path).state,
            DatabaseState::UnsupportedSchema
        );
        let connection = Connection::open(&db_path).expect("inspect db");
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .expect("schema version"),
            0
        );
    }

    #[test]
    fn phase8_env_records_and_setup_receipts_round_trip_without_plaintext_values() {
        let temp = TempWorkspace::new("metadata-phase8").expect("temp workspace");
        let db_path = temp.root().join(".state").join("local.sqlite3");
        let mut store = MetadataStore::open(&db_path).expect("metadata opens");
        let workspace_id = WorkspaceId::new("ws_phase8");
        let project_id = ProjectId::new("proj_web");
        store
            .insert_workspace(&workspace_id, "User Code", "2026-06-25T00:00:00Z")
            .expect("workspace");
        store
            .insert_root(
                "root_phase8",
                &workspace_id,
                &temp.root().to_string_lossy(),
                "2026-06-25T00:00:00Z",
            )
            .expect("root");
        store
            .insert_project(
                &project_id,
                &workspace_id,
                "root_phase8",
                "apps/web",
                "2026-06-25T00:00:00Z",
            )
            .expect("project");

        let records = [
            EnvRecord {
                id: EnvRecordId::new("env_api_url_env"),
                workspace_id: workspace_id.clone(),
                project_id: Some(project_id.clone()),
                source_path: "apps/web/.env".to_string(),
                profile: "default".to_string(),
                key_name: "API_URL".to_string(),
                occurrence_index: 0,
                line_kind: "key-value".to_string(),
                access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
                encrypted_locator_json: "{\"contentId\":\"cid_env_1\",\"storage\":\"packed\"}"
                    .to_string(),
                format_json: "{\"quote\":\"none\"}".to_string(),
                materialization_state: "materialized".to_string(),
                restriction_state: "unrestricted".to_string(),
                key_epoch: 1,
                metadata_json: "{\"redacted\":true}".to_string(),
                updated_at: "2026-06-25T00:00:01Z".to_string(),
            },
            EnvRecord {
                id: EnvRecordId::new("env_api_url_local"),
                workspace_id: workspace_id.clone(),
                project_id: Some(project_id.clone()),
                source_path: "apps/web/.env.local".to_string(),
                profile: "local".to_string(),
                key_name: "API_URL".to_string(),
                occurrence_index: 0,
                line_kind: "key-value".to_string(),
                access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
                encrypted_locator_json: "{\"contentId\":\"cid_env_2\",\"storage\":\"packed\"}"
                    .to_string(),
                format_json: "{\"quote\":\"double\"}".to_string(),
                materialization_state: "pending".to_string(),
                restriction_state: "unrestricted".to_string(),
                key_epoch: 1,
                metadata_json: "{\"redacted\":true}".to_string(),
                updated_at: "2026-06-25T00:00:01Z".to_string(),
            },
        ];
        store
            .replace_env_records_for_source(&workspace_id, "apps/web/.env", &records[0..1])
            .expect("replace env");
        store
            .upsert_env_record(&records[1])
            .expect("upsert second env");

        let stored = store.env_records(&workspace_id).expect("env records");
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].key_name, "API_URL");
        assert_ne!(stored[0].source_path, stored[1].source_path);
        let env_rows = format!("{stored:?}");
        assert!(!env_rows.contains("super-secret"));

        store
            .upsert_setup_receipt(&SetupReceiptRecord {
                id: "receipt_web_setup".to_string(),
                workspace_id: workspace_id.clone(),
                project_id: Some(project_id),
                command: "pnpm install --ignore-scripts".to_string(),
                state: "completed".to_string(),
                recipe_hash: "blake3:recipe".to_string(),
                approval_state: "approved".to_string(),
                trigger: "setup".to_string(),
                cwd: "apps/web".to_string(),
                os: "macos".to_string(),
                arch: "arm64".to_string(),
                env_profile: "default".to_string(),
                output_path: Some(".bowline/logs/setup.log".to_string()),
                redacted_summary: "installed dependencies with [redacted]".to_string(),
                receipt_json: "{\"command\":\"pnpm install --ignore-scripts\"}".to_string(),
                updated_at: "2026-06-25T00:00:02Z".to_string(),
            })
            .expect("receipt");

        let receipts = store.setup_receipts(&workspace_id).expect("setup receipts");
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].state, "completed");
        assert!(receipts[0].redacted_summary.contains("[redacted]"));
    }

    #[test]
    fn metadata_lives_outside_workspace_root() {
        let temp = TempWorkspace::new("metadata-outside-root").expect("temp workspace");
        let workspace_root = temp.root().join("Code");
        fs::create_dir_all(&workspace_root).expect("workspace root");
        let db_path = temp.root().join("state").join("local.sqlite3");

        assert!(!is_below(&db_path, &workspace_root));
        MetadataStore::open(&db_path).expect("metadata opens");
        assert!(!workspace_root.join("local.sqlite3").exists());
    }

    #[test]
    fn corrupt_database_can_be_inspected_without_panic() {
        let temp = TempWorkspace::new("metadata-corrupt").expect("temp workspace");
        let db_path = temp.root().join("local.sqlite3");
        fs::write(&db_path, b"not sqlite").expect("write corrupt db");

        let inspection = MetadataStore::inspect(&db_path);
        assert_eq!(inspection.state, DatabaseState::Corrupt);
    }

    #[test]
    fn product_shaped_queries_find_workspace_and_project_by_path() {
        let temp = TempWorkspace::new("metadata-query").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        let workspace_id = WorkspaceId::new("ws_code");
        let project_id = ProjectId::new("proj_acme_web");

        store
            .insert_workspace(&workspace_id, "User Code", "2026-06-23T12:00:00Z")
            .expect("workspace insert");
        store
            .insert_root("root_code", &workspace_id, "~/Code", "2026-06-23T12:00:00Z")
            .expect("root insert");
        store
            .insert_project(
                &project_id,
                &workspace_id,
                "root_code",
                "acme/web",
                "2026-06-23T12:00:00Z",
            )
            .expect("project insert");

        assert_eq!(
            store
                .current_workspace()
                .expect("current workspace")
                .unwrap()
                .id,
            workspace_id
        );
        assert_eq!(
            store
                .current_project_by_path("acme/web/src/index.ts")
                .expect("project by path")
                .unwrap()
                .id,
            project_id
        );
        let home = std::env::var("HOME").expect("HOME should be set for tilde root matching");
        assert_eq!(
            store
                .current_project_by_path(&format!("{home}/Code/acme/web/src/index.ts"))
                .expect("project by absolute path under tilde root")
                .unwrap()
                .id,
            project_id
        );
    }

    #[test]
    fn current_workspace_ignores_stale_workspace_without_root() {
        let temp = TempWorkspace::new("metadata-current-workspace").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        let stale_workspace_id = WorkspaceId::new("ws_code");
        let active_workspace_id = WorkspaceId::new("ws_code_account");

        store
            .insert_workspace(&stale_workspace_id, "User Code", "2026-06-23T12:00:00Z")
            .expect("stale workspace insert");
        store
            .insert_workspace(&active_workspace_id, "User Code", "2026-06-23T12:01:00Z")
            .expect("active workspace insert");
        store
            .insert_root(
                "root_code",
                &active_workspace_id,
                "~/Code",
                "2026-06-23T12:01:00Z",
            )
            .expect("active root insert");

        assert_eq!(
            store
                .current_workspace()
                .expect("current workspace")
                .unwrap()
                .id,
            active_workspace_id
        );
    }

    #[test]
    fn current_workspace_prefers_newest_accepted_root() {
        let temp =
            TempWorkspace::new("metadata-current-workspace-newest-root").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        let old_workspace_id = WorkspaceId::new("ws_code");
        let account_workspace_id = WorkspaceId::new("ws_code_account");

        store
            .insert_workspace(&old_workspace_id, "User Code", "2026-06-23T12:00:00Z")
            .expect("old workspace insert");
        store
            .insert_root(
                "root_ws_code",
                &old_workspace_id,
                "~/Code",
                "2026-06-23T12:00:00Z",
            )
            .expect("old root insert");
        store
            .insert_workspace(&account_workspace_id, "User Code", "2026-06-23T12:01:00Z")
            .expect("account workspace insert");
        store
            .insert_root(
                "root_ws_code_account",
                &account_workspace_id,
                "~/Code",
                "2026-06-23T12:01:00Z",
            )
            .expect("account root insert");

        assert_eq!(
            store
                .current_workspace()
                .expect("current workspace")
                .unwrap()
                .id,
            account_workspace_id
        );
    }

    #[test]
    fn replace_projects_removes_stale_projects_for_workspace() {
        let temp = TempWorkspace::new("metadata-replace-projects").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let mut store = MetadataStore::open(&db_path).expect("metadata opens");
        let workspace_id = WorkspaceId::new("ws_code");

        store
            .insert_workspace(&workspace_id, "User Code", "2026-06-23T12:00:00Z")
            .expect("workspace insert");
        store
            .insert_root("root_code", &workspace_id, "~/Code", "2026-06-23T12:00:00Z")
            .expect("root insert");
        store
            .replace_projects(
                &workspace_id,
                "root_code",
                &[
                    (ProjectId::new("proj_old"), "old".to_string()),
                    (ProjectId::new("proj_web"), "apps/web".to_string()),
                ],
                "2026-06-23T12:00:00Z",
            )
            .expect("first project set");
        store
            .connection()
            .execute(
                "INSERT INTO namespace_entries
                 (id, workspace_id, project_id, path, kind, classification, mode,
                  hydration_state, updated_at)
                 VALUES (?1, ?2, ?3, ?4, 'file', 'workspace-sync', 'workspace-sync',
                         'local', ?5)",
                rusqlite::params![
                    "entry-web",
                    workspace_id.as_str(),
                    "proj_web",
                    "apps/web/src/index.ts",
                    "2026-06-23T12:00:00Z",
                ],
            )
            .expect("namespace insert");
        store
            .connection()
            .execute(
                "INSERT INTO namespace_entries
                 (id, workspace_id, project_id, path, kind, classification, mode,
                  hydration_state, updated_at)
                 VALUES (?1, ?2, ?3, ?4, 'file', 'workspace-sync', 'workspace-sync',
                         'local', ?5)",
                rusqlite::params![
                    "entry-old",
                    workspace_id.as_str(),
                    "proj_old",
                    "old/src/lib.rs",
                    "2026-06-23T12:00:00Z",
                ],
            )
            .expect("old namespace insert");
        let old_project_id = ProjectId::new("proj_old");
        store
            .upsert_index_document(&IndexDocumentRecord {
                workspace_id: workspace_id.clone(),
                project_id: Some(old_project_id.clone()),
                path: "src/lib.rs".to_string(),
                snapshot_id: Some(SnapshotId::new("snap_old")),
                content_id: Some(ContentId::new("cid_old")),
                classification: PathClassification::WorkspaceSync,
                mode: MaterializationMode::WorkspaceSync,
                access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
                policy_summary: "source".to_string(),
                body_text: "pub fn stale_secret_text() {}".to_string(),
                hydration_state: HydrationState::Cold,
                indexed_bytes: 29,
                source_watermark: 1,
                indexed_watermark: 1,
                state: "ready".to_string(),
                updated_at: "2026-06-23T12:00:00Z".to_string(),
            })
            .expect("old index document");
        store
            .upsert_index_pack(&IndexPackRecord {
                workspace_id: workspace_id.clone(),
                project_id: Some(old_project_id.clone()),
                snapshot_id: Some(SnapshotId::new("snap_old")),
                object_key: "indexes/old.bowlinei".to_string(),
                byte_len: 128,
                hash: "hash-old-index-pack".to_string(),
                state: "ready".to_string(),
                updated_at: "2026-06-23T12:00:00Z".to_string(),
            })
            .expect("old index pack");
        store
            .upsert_index_work(&IndexWorkRecord {
                id: "index_work:ws_code:proj_old:path:lib".to_string(),
                workspace_id: workspace_id.clone(),
                project_id: Some(old_project_id),
                path: Some("src/lib.rs".to_string()),
                kind: "path".to_string(),
                source_watermark: 1,
                indexed_watermark: 0,
                state: "pending".to_string(),
                reason: Some("projected-node-updated".to_string()),
                updated_at: "2026-06-23T12:00:00Z".to_string(),
            })
            .expect("old index work");
        store
            .replace_projects(
                &workspace_id,
                "root_code",
                &[(ProjectId::new("proj_web"), "apps/web".to_string())],
                "2026-06-23T12:01:00Z",
            )
            .expect("second project set");

        assert_eq!(
            store.project_count(&workspace_id).expect("project count"),
            1
        );
        assert!(
            store
                .current_project_by_path("old/src/lib.rs")
                .expect("old project lookup")
                .is_none()
        );
        assert_eq!(
            store
                .current_project_by_path("apps/web/src/index.ts")
                .expect("current project lookup")
                .unwrap()
                .id,
            ProjectId::new("proj_web")
        );
        assert_eq!(
            store
                .connection()
                .query_row(
                    "SELECT project_id FROM namespace_entries WHERE id = 'entry-web'",
                    [],
                    |row| row.get::<_, Option<String>>(0),
                )
                .expect("namespace project link"),
            Some("proj_web".to_string())
        );
        let old_entry_count: i64 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM namespace_entries WHERE id = 'entry-old'",
                [],
                |row| row.get(0),
            )
            .expect("old namespace count");
        assert_eq!(old_entry_count, 0);
        for (table, label) in [
            ("index_documents", "old index documents"),
            ("index_packs", "old index packs"),
            ("index_work", "old index work"),
        ] {
            let count: i64 = store
                .connection()
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE project_id = 'proj_old'"),
                    [],
                    |row| row.get(0),
                )
                .expect(label);
            assert_eq!(count, 0, "{label} should be removed");
        }
    }

    #[test]
    fn replace_projects_rejects_project_id_owned_by_another_workspace() {
        let temp =
            TempWorkspace::new("metadata-replace-projects-cross-workspace").expect("workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let mut store = MetadataStore::open(&db_path).expect("metadata opens");
        let workspace_a = WorkspaceId::new("ws_a");
        let workspace_b = WorkspaceId::new("ws_b");
        store
            .insert_workspace(&workspace_a, "A", "2026-06-29T04:00:00Z")
            .expect("workspace a");
        store
            .insert_workspace(&workspace_b, "B", "2026-06-29T04:00:00Z")
            .expect("workspace b");
        store
            .insert_root("root_a", &workspace_a, "/tmp/a", "2026-06-29T04:00:00Z")
            .expect("root a");
        store
            .insert_root("root_b", &workspace_b, "/tmp/b", "2026-06-29T04:00:00Z")
            .expect("root b");
        store
            .replace_projects(
                &workspace_a,
                "root_a",
                &[(ProjectId::new("proj_app"), "app".to_string())],
                "2026-06-29T04:00:00Z",
            )
            .expect("workspace a projects");

        let error = store
            .replace_projects(
                &workspace_b,
                "root_b",
                &[(ProjectId::new("proj_app"), "app".to_string())],
                "2026-06-29T04:00:01Z",
            )
            .expect_err("cross-workspace project id is rejected");

        assert!(error.to_string().contains("another workspace"));
        assert_eq!(
            store
                .connection()
                .query_row(
                    "SELECT workspace_id FROM projects WHERE id = 'proj_app'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("project owner"),
            workspace_a.as_str()
        );
    }

    #[test]
    fn insert_root_rejects_root_id_owned_by_another_workspace() {
        let temp = TempWorkspace::new("metadata-root-cross-workspace").expect("workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        let workspace_a = WorkspaceId::new("ws_a");
        let workspace_b = WorkspaceId::new("ws_b");
        store
            .insert_workspace(&workspace_a, "A", "2026-06-29T04:00:00Z")
            .expect("workspace a");
        store
            .insert_workspace(&workspace_b, "B", "2026-06-29T04:00:00Z")
            .expect("workspace b");
        store
            .insert_root("root_code", &workspace_a, "/tmp/a", "2026-06-29T04:00:00Z")
            .expect("workspace a root");

        let error = store
            .insert_root("root_code", &workspace_b, "/tmp/b", "2026-06-29T04:00:01Z")
            .expect_err("cross-workspace root id is rejected");

        assert!(error.to_string().contains("another workspace"));
        assert_eq!(
            store
                .accepted_roots(&workspace_a)
                .expect("workspace a roots")
                .len(),
            1
        );
        assert!(
            store
                .accepted_roots(&workspace_b)
                .expect("workspace b roots")
                .is_empty()
        );
    }

    #[test]
    fn root_matching_is_scoped_to_the_current_workspace() {
        let temp = TempWorkspace::new("metadata-root-scope").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let current_root = temp.root().join("Code");
        let other_root = temp.root().join("OtherCode");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        let current_workspace_id = WorkspaceId::new("ws_current");
        let other_workspace_id = WorkspaceId::new("ws_other");
        let project_id = ProjectId::new("proj_acme_web");

        store
            .insert_workspace(
                &current_workspace_id,
                "Current Code",
                "2026-06-23T12:01:00Z",
            )
            .expect("current workspace insert");
        store
            .insert_workspace(&other_workspace_id, "Other Code", "2026-06-23T12:00:00Z")
            .expect("other workspace insert");
        store
            .insert_root(
                "root_current",
                &current_workspace_id,
                &current_root.display().to_string(),
                "2026-06-23T12:01:00Z",
            )
            .expect("current root insert");
        store
            .insert_root(
                "root_other",
                &other_workspace_id,
                &other_root.display().to_string(),
                "2026-06-23T12:00:00Z",
            )
            .expect("other root insert");
        store
            .insert_project(
                &project_id,
                &current_workspace_id,
                "root_current",
                "acme/web",
                "2026-06-23T12:00:00Z",
            )
            .expect("project insert");

        assert_eq!(
            store
                .accepted_root_count(&current_workspace_id)
                .expect("current accepted roots"),
            1
        );
        assert_eq!(
            store
                .project_count(&other_workspace_id)
                .expect("other project count"),
            0
        );
        assert_eq!(
            store
                .current_project_by_path(&format!(
                    "{}/acme/web/src/index.ts",
                    current_root.display()
                ))
                .expect("project by current root")
                .unwrap()
                .id,
            project_id
        );
        assert!(
            store
                .current_project_by_path(&format!("{}/acme/web/src/index.ts", other_root.display()))
                .expect("project by other root")
                .is_none()
        );
    }

    #[test]
    fn packs_and_content_locators_round_trip_through_reserved_tables() {
        let temp = TempWorkspace::new("metadata-storage").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        let workspace_id = WorkspaceId::new("ws_code");
        let content_id = ContentId::new("cid_source");
        let first_pack_id = PackId::new("pk_source_00000001");
        let second_pack_id = PackId::new("pk_source_00000002");

        store
            .insert_workspace(&workspace_id, "User Code", "2026-06-24T12:00:00Z")
            .expect("workspace insert");
        store
            .put_pack_record_with_metadata(
                &workspace_id,
                &first_pack_id,
                "source-pack",
                4096,
                "b3_first",
                3,
                "pending",
                Some("2026-06-25T12:00:00Z"),
                "2026-06-24T12:01:00Z",
            )
            .expect("pack insert");
        store
            .put_pack_record(
                &workspace_id,
                &second_pack_id,
                "source-pack",
                8192,
                "pending",
                "2026-06-24T12:01:30Z",
            )
            .expect("second pack insert");

        let first_locator = ContentLocator {
            content_id: content_id.clone(),
            storage: ContentStorage::Packed,
            raw_size: 18,
            pack_id: Some(first_pack_id.clone()),
            offset: Some(100),
            length: Some(64),
            chunk_ids: Vec::new(),
        };
        store
            .put_content_locator(&workspace_id, &first_locator, "2026-06-24T12:02:00Z")
            .expect("locator insert");

        let packs = store.pack_records(&workspace_id).expect("packs");
        assert_eq!(packs.len(), 2);
        assert_eq!(packs[0].object_hash, "b3_first");
        assert_eq!(packs[0].key_epoch, 3);
        assert_eq!(
            packs[0].retain_until.as_deref(),
            Some("2026-06-25T12:00:00Z")
        );
        assert_eq!(
            store
                .content_locator(&workspace_id, &content_id)
                .expect("locator query")
                .expect("locator exists")
                .locator,
            first_locator
        );

        let remapped_locator = ContentLocator {
            content_id: content_id.clone(),
            storage: ContentStorage::Packed,
            raw_size: 18,
            pack_id: Some(second_pack_id),
            offset: Some(2048),
            length: Some(64),
            chunk_ids: Vec::new(),
        };
        store
            .put_content_locator(&workspace_id, &remapped_locator, "2026-06-24T12:03:00Z")
            .expect("locator remap");

        let stored = store
            .content_locator(&workspace_id, &content_id)
            .expect("locator query")
            .expect("locator exists");
        assert_eq!(stored.workspace_id, workspace_id);
        assert_eq!(stored.locator.content_id, content_id);
        assert_eq!(stored.locator, remapped_locator);
        assert_eq!(stored.updated_at, "2026-06-24T12:03:00Z");

        drop(store);
        let reopened = MetadataStore::open(&db_path).expect("metadata reopens");
        let reopened_locator = reopened
            .content_locator(&workspace_id, &content_id)
            .expect("locator query after reopen")
            .expect("locator exists after reopen");
        assert_eq!(
            reopened.pack_records(&workspace_id).expect("packs").len(),
            2
        );
        assert_eq!(reopened_locator.locator, remapped_locator);
        assert_eq!(reopened_locator.updated_at, "2026-06-24T12:03:00Z");
    }

    #[test]
    fn storage_metadata_is_workspace_scoped() {
        let temp = TempWorkspace::new("metadata-storage-workspaces").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        let first_workspace = WorkspaceId::new("ws_first");
        let second_workspace = WorkspaceId::new("ws_second");
        let shared_pack_id = PackId::new("pk_0011223344556677");
        let shared_content_id = ContentId::new("cid_shared");

        store
            .insert_workspace(&first_workspace, "First", "2026-06-24T12:00:00Z")
            .expect("first workspace");
        store
            .insert_workspace(&second_workspace, "Second", "2026-06-24T12:00:00Z")
            .expect("second workspace");
        for workspace_id in [&first_workspace, &second_workspace] {
            store
                .put_pack_record(
                    workspace_id,
                    &shared_pack_id,
                    "source-pack",
                    4096,
                    "pending",
                    "2026-06-24T12:01:00Z",
                )
                .expect("pack insert");
        }

        let first_locator = ContentLocator {
            content_id: shared_content_id.clone(),
            storage: ContentStorage::Packed,
            raw_size: 18,
            pack_id: Some(shared_pack_id.clone()),
            offset: Some(100),
            length: Some(64),
            chunk_ids: Vec::new(),
        };
        let second_locator = ContentLocator {
            offset: Some(2048),
            ..first_locator.clone()
        };
        store
            .put_content_locator(&first_workspace, &first_locator, "2026-06-24T12:02:00Z")
            .expect("first locator");
        store
            .put_content_locator(&second_workspace, &second_locator, "2026-06-24T12:03:00Z")
            .expect("second locator");

        assert_eq!(
            store
                .content_locator(&first_workspace, &shared_content_id)
                .expect("first lookup")
                .expect("first locator")
                .locator,
            first_locator
        );
        assert_eq!(
            store
                .content_locator(&second_workspace, &shared_content_id)
                .expect("second lookup")
                .expect("second locator")
                .locator,
            second_locator
        );
        assert_eq!(store.pack_records(&first_workspace).unwrap().len(), 1);
        assert_eq!(store.pack_records(&second_workspace).unwrap().len(), 1);
    }

    #[test]
    fn packed_locators_must_reference_existing_pack_in_same_workspace() {
        let temp = TempWorkspace::new("metadata-storage-missing-pack").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        let workspace_id = WorkspaceId::new("ws_code");
        store
            .insert_workspace(&workspace_id, "User Code", "2026-06-24T12:00:00Z")
            .expect("workspace insert");

        let locator = ContentLocator {
            content_id: ContentId::new("cid_source"),
            storage: ContentStorage::Packed,
            raw_size: 18,
            pack_id: Some(PackId::new("pk_0011223344556677")),
            offset: Some(100),
            length: Some(64),
            chunk_ids: Vec::new(),
        };
        let error = store
            .put_content_locator(&workspace_id, &locator, "2026-06-24T12:02:00Z")
            .expect_err("missing pack rejected");

        assert!(matches!(error, MetadataError::InvalidStorageMetadata(_)));
    }

    #[test]
    fn locator_json_drift_is_rejected_on_read() {
        let temp = TempWorkspace::new("metadata-storage-drift").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        let workspace_id = WorkspaceId::new("ws_code");
        let pack_id = PackId::new("pk_0011223344556677");
        let content_id = ContentId::new("cid_source");
        store
            .insert_workspace(&workspace_id, "User Code", "2026-06-24T12:00:00Z")
            .expect("workspace insert");
        store
            .put_pack_record(
                &workspace_id,
                &pack_id,
                "source-pack",
                4096,
                "pending",
                "2026-06-24T12:01:00Z",
            )
            .expect("pack insert");

        let drifted_json = serde_json::to_string(&ContentLocator {
            content_id: content_id.clone(),
            storage: ContentStorage::Packed,
            raw_size: 18,
            pack_id: Some(pack_id.clone()),
            offset: Some(200),
            length: Some(64),
            chunk_ids: Vec::new(),
        })
        .expect("locator json");
        store
            .connection()
            .execute(
                "INSERT INTO content_locators
                 (content_id, workspace_id, storage, raw_size, pack_id, offset, length,
                  locator_json, updated_at)
                 VALUES (?1, ?2, 'packed', 18, ?3, 100, 64, ?4, '2026-06-24T12:02:00Z')",
                rusqlite::params![
                    content_id.as_str(),
                    workspace_id.as_str(),
                    pack_id.as_str(),
                    drifted_json,
                ],
            )
            .expect("drifted row insert");

        assert!(
            store
                .content_locator(&workspace_id, &content_id)
                .expect_err("drift rejected")
                .to_string()
                .contains("locator_json drifted")
        );
    }

    #[test]
    fn materialization_metadata_round_trips() {
        let temp = TempWorkspace::new("metadata-materialization").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let code_root = temp.root().join("Code");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        let workspace_id = WorkspaceId::new("ws_code");
        let project_id = ProjectId::new("proj_acme_web");
        let content_id = ContentId::new("cid_source");
        let code_root_string = code_root.display().to_string();

        store
            .insert_workspace(&workspace_id, "User Code", "2026-06-24T12:00:00Z")
            .expect("workspace insert");
        store
            .insert_root(
                "root_code",
                &workspace_id,
                &code_root_string,
                "2026-06-24T12:00:00Z",
            )
            .expect("root insert");
        store
            .insert_project(
                &project_id,
                &workspace_id,
                "root_code",
                "acme/web",
                "2026-06-24T12:00:00Z",
            )
            .expect("project insert");

        let projected_path = code_root.join("acme/web/src/main.rs").display().to_string();
        store
            .upsert_projected_node(&ProjectedNodeRecord {
                workspace_id: workspace_id.clone(),
                node_id: "node_src".to_string(),
                project_id: Some(project_id.clone()),
                parent_node_id: Some("node_web".to_string()),
                path: projected_path.clone(),
                kind: NamespaceEntryKind::File,
                content_id: Some(content_id.clone()),
                hydration_state: HydrationState::Cold,
                updated_at: "2026-06-24T12:02:00Z".to_string(),
            })
            .expect("node upsert");

        store
            .enqueue_hydration(&HydrationQueueRecord {
                id: "hydrate_src".to_string(),
                workspace_id: workspace_id.clone(),
                project_id: Some(project_id.clone()),
                path: projected_path.clone(),
                content_id: Some(content_id.clone()),
                priority: "active-read".to_string(),
                state: "queued".to_string(),
                cause: "open-read".to_string(),
                updated_at: "2026-06-24T12:03:00Z".to_string(),
            })
            .expect("hydration enqueue");

        let write = LocalWriteLogRecord {
            id: "write_src".to_string(),
            workspace_id: workspace_id.clone(),
            device_id: DeviceId::new("dev_mac"),
            project_id: Some(project_id),
            path: "acme/web/src/main.rs".to_string(),
            source_path: None,
            operation: "update".to_string(),
            staged_content_id: Some(ContentId::new("cid_staged")),
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: "event_edit".to_string(),
            settled_at: "2026-06-24T12:04:00Z".to_string(),
            created_at: "2026-06-24T12:04:01Z".to_string(),
        };
        store
            .append_local_write_log(&write)
            .expect("write log append");

        let node = store
            .projected_node_by_path(&workspace_id, "acme/web/src/main.rs")
            .expect("node lookup")
            .expect("node exists");
        assert_eq!(node.node_id, "node_src");
        assert_eq!(node.path, "acme/web/src/main.rs");
        assert_eq!(node.hydration_state, HydrationState::Cold);
        assert_eq!(
            store.hydration_queue(&workspace_id).expect("queue")[0].path,
            "acme/web/src/main.rs"
        );
        assert_eq!(
            store.local_write_log(&workspace_id).expect("write log"),
            vec![write]
        );
    }

    fn is_below(path: &Path, root: &Path) -> bool {
        path.starts_with(root)
    }
}
