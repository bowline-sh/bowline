use std::{
    collections::{BTreeSet, HashSet},
    env,
    error::Error,
    fmt,
    path::{Path, PathBuf},
    time::Duration,
};

use bowline_control_plane::{
    StatusEventWatermarks, StatusItemSnapshot, StatusLimitSnapshot, StatusSyncQueueSnapshot,
    StatusWorkspaceSummarySnapshot, WorkspaceStatusSnapshot,
};
use bowline_core::{
    commands::{
        AgentSessionState, CONTRACT_VERSION, CommandError, CommandErrorOutput, CommandErrorStatus,
        CommandName, CommandRecoverability, EventsCommandOutput, StatusCommandOutput, WatchFrame,
    },
    events::{EventName, EventSeverity, EventSubjectKind},
    ids::{DeviceId, ProjectId, WorkspaceId},
    policy::{MaterializationMode, PathClassification},
    status::{
        ComponentState, EventWatermarks, FreshnessVerdict, GitObserverState, LimitedCapability,
        NetworkState, ObservedWorkspaceSummary, ProjectAttentionSummary, ProjectSetupReadiness,
        ProjectSetupReadinessState, RepairCommand, SetupReceiptState, StaleBaseStatus,
        StatusAttention, StatusAvailability, StatusFact, StatusFactAvailabilityImpact,
        StatusFactScope, StatusItem, StatusItemKind, StatusLevel, StatusScope,
        StatusSnapshotFreshness, StatusSubject, StatusSubjectKind, SyncQueueStatus,
        WorkspaceStatus, WorkspaceSummary, reduce_status_facts, status_fact_policy,
    },
    work_views::{WorkViewLifecycle, WorkViewSyncState},
};

use crate::{
    agents::{AgentError, recover_provisional_agent_leases},
    events::EventQuery,
    metadata::{
        AgentLeaseRecord, DatabaseState, MaterializationTaskState, MetadataError, MetadataStore,
        ObservedLocalPath, ProjectLifecycleState, ProjectLocalMaterializationState, ProjectRecord,
        SyncOperationCounts, WorkViewRecord, WorkspaceRecord, default_database_path,
    },
    sync::conflicts::{
        ConflictBundleError, ConflictStatusRevision, conflict_status_revision,
        unresolved_conflict_paths,
    },
    work_views::WorkViewError,
};

pub const MAX_EVENTS_LIMIT: u32 = 500;
pub const STATUS_SAFETY_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
pub use collector::{
    LocalStatusCollection, LocalStatusFactCollector, LocalStatusFacts, LocalStatusRevision,
    LocalStatusSourceRevision, RevisionedStatus, RevisionedStatusComposer, StatusComposerMetrics,
    StatusSourceRevision,
};
pub(crate) use common::event_name_label;

#[derive(Debug, Clone)]
pub struct StatusOptions {
    pub db_path: Option<PathBuf>,
    pub requested_path: Option<String>,
    pub workspace_scope: bool,
    pub generated_at: String,
}

#[derive(Debug, Clone)]
pub struct EventsOptions {
    pub db_path: Option<PathBuf>,
    pub requested_path: Option<String>,
    pub workspace_scope: bool,
    pub generated_at: String,
    pub limit: u32,
}

#[derive(Debug)]
pub enum LocalStatusError {
    Metadata(MetadataError),
    MetadataState(DatabaseState),
    Path(std::io::Error),
    Events(crate::events::LocalEventError),
    ConflictBundle(ConflictBundleError),
}

impl LocalStatusError {
    pub fn is_recoverable(&self) -> bool {
        match self {
            Self::Metadata(error) => metadata_error_is_recoverable(error),
            Self::MetadataState(DatabaseState::Locked) => true,
            Self::MetadataState(_) => false,
            Self::Path(error) => io_error_is_recoverable(error),
            Self::Events(error) => event_error_is_recoverable(error),
            Self::ConflictBundle(error) => conflict_bundle_error_is_recoverable(error),
        }
    }
}

pub fn compose_status(options: StatusOptions) -> Result<StatusCommandOutput, LocalStatusError> {
    let db_path = resolve_db_path(options.db_path.clone())?;
    let inspection = MetadataStore::inspect(&db_path);

    match inspection.state {
        DatabaseState::Missing => Ok(missing_metadata_status(&options)),
        DatabaseState::Corrupt
        | DatabaseState::FutureIncompatible { .. }
        | DatabaseState::UnsupportedSchema
        | DatabaseState::Locked
        | DatabaseState::PermissionDenied => {
            Ok(limited_metadata_status(&options, &inspection.state))
        }
        DatabaseState::Empty => Ok(missing_metadata_status(&options)),
        DatabaseState::Current => {
            let store = MetadataStore::open(&db_path)?;
            let state_root = db_path
                .parent()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            compose_from_store(&store, options, state_root)
        }
    }
}

pub fn compose_events(options: EventsOptions) -> Result<EventsCommandOutput, LocalStatusError> {
    let db_path = resolve_db_path(options.db_path.clone())?;
    let inspection = MetadataStore::inspect(&db_path);
    let (workspace_id, project_id, events, watermarks) = match inspection.state {
        DatabaseState::Missing | DatabaseState::Empty => {
            (None, None, Vec::new(), empty_watermarks())
        }
        DatabaseState::Corrupt
        | DatabaseState::FutureIncompatible { .. }
        | DatabaseState::UnsupportedSchema
        | DatabaseState::Locked
        | DatabaseState::PermissionDenied => {
            return Err(LocalStatusError::MetadataState(inspection.state));
        }
        DatabaseState::Current => {
            let store = MetadataStore::open(&db_path)?;
            let scope = resolve_scope(
                &store,
                options.requested_path.as_deref(),
                options.workspace_scope,
            )?;
            if scope.workspace_id.is_none() {
                (None, None, Vec::new(), empty_watermarks())
            } else {
                let query = scope.event_query(options.limit.min(MAX_EVENTS_LIMIT));
                (
                    scope.workspace_id,
                    scope.project_id,
                    store.list_events_scoped(query.clone())?,
                    store.scoped_event_watermarks(query)?,
                )
            }
        }
    };

    let scope = Some(if options.workspace_scope || project_id.is_none() {
        StatusScope::Workspace
    } else {
        StatusScope::Project
    });
    let requested_path = options.requested_path;

    Ok(EventsCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Events,
        generated_at: options.generated_at,
        workspace_id,
        project_id: project_id.clone(),
        scope,
        requested_path,
        events,
        event_watermarks: watermarks,
    })
}

pub fn initial_watch_frame(status: StatusCommandOutput) -> WatchFrame {
    WatchFrame::Status {
        contract_version: CONTRACT_VERSION,
        sequence: 1,
        generated_at: status.generated_at.clone(),
        workspace_id: status.workspace_id.clone(),
        project_id: status.project_id.clone(),
        last_event_id: status.event_watermarks.last_event_id.clone(),
        watermark: status.event_watermarks.clone(),
        status: Box::new(status),
    }
}

pub fn render_events_human(output: &EventsCommandOutput) -> String {
    if output.events.is_empty() {
        return "No local bowline events recorded.\n".to_string();
    }

    let mut lines = Vec::new();
    for event in &output.events {
        lines.push(format!(
            "{} {} {}",
            event.occurred_at,
            event_name_label(&event.name),
            event.summary
        ));
    }
    lines.push(String::new());
    lines.join("\n")
}

impl fmt::Display for LocalStatusError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Metadata(error) => error.fmt(formatter),
            Self::MetadataState(state) => write!(formatter, "metadata unavailable: {state:?}"),
            Self::Path(error) => write!(formatter, "metadata path failed: {error}"),
            Self::Events(error) => error.fmt(formatter),
            Self::ConflictBundle(error) => error.fmt(formatter),
        }
    }
}

impl Error for LocalStatusError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Metadata(error) => Some(error),
            Self::MetadataState(_) => None,
            Self::Path(error) => Some(error),
            Self::Events(error) => Some(error),
            Self::ConflictBundle(error) => Some(error),
        }
    }
}

impl From<MetadataError> for LocalStatusError {
    fn from(error: MetadataError) -> Self {
        Self::Metadata(error)
    }
}

impl From<std::io::Error> for LocalStatusError {
    fn from(error: std::io::Error) -> Self {
        Self::Path(error)
    }
}

impl From<crate::events::LocalEventError> for LocalStatusError {
    fn from(error: crate::events::LocalEventError) -> Self {
        Self::Events(error)
    }
}

impl From<ConflictBundleError> for LocalStatusError {
    fn from(error: ConflictBundleError) -> Self {
        Self::ConflictBundle(error)
    }
}

fn metadata_error_is_recoverable(error: &MetadataError) -> bool {
    match error {
        MetadataError::Io(error) => io_error_is_recoverable(error),
        MetadataError::Sqlite(error) => sqlite_error_is_recoverable(error),
        MetadataError::InvalidStorageMetadata(_)
        | MetadataError::InvalidCurrentNamespaceProjection { .. }
        | MetadataError::ImmutableBindingConflict { .. }
        | MetadataError::IncompleteSnapshotRoot { .. }
        | MetadataError::FutureIncompatible { .. }
        | MetadataError::UnsupportedSchema => false,
    }
}

fn event_error_is_recoverable(error: &crate::events::LocalEventError) -> bool {
    match error {
        crate::events::LocalEventError::Metadata(error) => metadata_error_is_recoverable(error),
        crate::events::LocalEventError::Sqlite(error) => sqlite_error_is_recoverable(error),
        crate::events::LocalEventError::Json(_)
        | crate::events::LocalEventError::DuplicateEventId(_) => false,
    }
}

fn conflict_bundle_error_is_recoverable(error: &ConflictBundleError) -> bool {
    match error {
        ConflictBundleError::Io(error) => io_error_is_recoverable(error),
        ConflictBundleError::RecordLockTimeout { .. } => true,
        ConflictBundleError::Json(_)
        | ConflictBundleError::MissingOccurrenceField { .. }
        | ConflictBundleError::OccurrenceSuperseded { .. }
        | ConflictBundleError::InvalidOccurrenceVersion { .. }
        | ConflictBundleError::UnsafePath(_) => false,
    }
}

fn sqlite_error_is_recoverable(error: &rusqlite::Error) -> bool {
    match error {
        rusqlite::Error::SqliteFailure(sqlite_error, _) => matches!(
            sqlite_error.code,
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
        ),
        _ => false,
    }
}

fn io_error_is_recoverable(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::WouldBlock
    )
}

fn resolve_db_path(path: Option<PathBuf>) -> Result<PathBuf, LocalStatusError> {
    match path {
        Some(path) => Ok(path),
        None => default_database_path().map_err(Into::into),
    }
}

mod accumulator;
mod collector;
mod common;
mod compose;
mod materialization;
mod scope;
mod setup;
mod signals;
mod snapshot;
mod sync;
mod work;

use accumulator::StatusAccumulator;

use common::*;
use compose::*;
use scope::*;
use signals::*;
#[cfg(test)]
pub(super) use snapshot::redact_workspace_path;
pub use snapshot::{command_error_output, redacted_status_snapshot};
use sync::*;
pub(crate) use sync::{freshness_for_stale_bases, snapshot_stale_bases};
use work::*;

#[cfg(test)]
mod tests;
