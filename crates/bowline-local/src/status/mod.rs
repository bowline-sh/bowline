use std::{
    collections::{BTreeSet, HashSet},
    env,
    error::Error,
    fmt,
    path::{Path, PathBuf},
};

use bowline_control_plane::{
    StatusEventWatermarks, StatusIndexSnapshot, StatusItemSnapshot, StatusLimitSnapshot,
    StatusSyncQueueSnapshot, StatusWorkspaceSummarySnapshot, WorkspaceStatusSnapshot,
};
use bowline_core::{
    commands::{
        AgentLeaseExecutionState, AgentLeaseOutputState, CONTRACT_VERSION, CommandError,
        CommandErrorOutput, CommandErrorStatus, CommandName, CommandRecoverability,
        EventsCommandOutput, HydrationBudgetStatus, IndexDegradedReason, IndexSource, IndexState,
        IndexStatus, StatusCommandOutput, WatchFrame,
    },
    events::{EventName, EventSeverity, EventSubjectKind},
    ids::{DeviceId, ProjectId, WorkspaceId},
    policy::{MaterializationMode, PathClassification},
    status::{
        ComponentState, EventWatermarks, HydrationProgress, LimitedCapability, NetworkState,
        ObservedWorkspaceSummary, ProjectAttentionSummary, SafeAction, StatusItem, StatusItemKind,
        StatusLevel, StatusScope, StatusSubject, StatusSubjectKind, SyncQueueStatus,
        WorkspaceStatus, WorkspaceSummary,
    },
    work_views::{WorkViewLifecycle, WorkViewSyncState},
};

use crate::{
    agents::{AgentError, recover_provisional_agent_leases},
    events::EventQuery,
    hydration_budget::lease_budget_status,
    metadata::{
        DatabaseState, MetadataError, MetadataStore, SyncOperationCounts, default_database_path,
    },
    sync::conflicts::{ConflictBundleError, unresolved_conflict_paths},
    work_views::WorkViewError,
};

pub const MAX_EVENTS_LIMIT: u32 = 500;

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
            let query = scope.event_query(options.limit.min(MAX_EVENTS_LIMIT));
            (
                scope.workspace_id,
                scope.project_id,
                store.list_events_scoped(query.clone())?,
                store.scoped_event_watermarks(query)?,
            )
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

pub fn render_status_human(output: &StatusCommandOutput) -> String {
    let mut lines = vec![
        format!(
            "Workspace: {}",
            output.resolved_workspace_root.as_deref().unwrap_or("local")
        ),
        format!("Status: {}", status_level_label(output.status.level)),
    ];

    if output.status.attention_items.is_empty() {
        lines.push("Attention: none".to_string());
    } else {
        lines.push("Attention:".to_string());
        lines.extend(
            output
                .status
                .attention_items
                .iter()
                .map(|item| format!("  {item}")),
        );
    }

    if !output.items.is_empty() {
        lines.push("Details:".to_string());
        lines.extend(
            output
                .items
                .iter()
                .map(|item| format!("  {}", item.summary)),
        );
    }

    if let Some(index) = &output.index {
        lines.push(format!("Index: {:?} - {}", index.state, index.summary));
    }

    if !output.limits.is_empty() {
        lines.push("Limited capabilities:".to_string());
        lines.extend(output.limits.iter().map(|limit| {
            format!(
                "  {}: {}; still works: {}",
                limit.capability,
                limit.unavailable_because,
                limit.still_works.join(", ")
            )
        }));
    }

    if !output.next_actions.is_empty() {
        lines.push("Suggested actions:".to_string());
        lines.extend(
            output
                .next_actions
                .iter()
                .map(|action| match &action.command {
                    Some(command) => format!("  {}: {command}", action.label),
                    None => format!("  {}", action.label),
                }),
        );
    }

    lines.push(String::new());
    lines.join("\n")
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
            event_name_label(event.name),
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

fn resolve_db_path(path: Option<PathBuf>) -> Result<PathBuf, LocalStatusError> {
    match path {
        Some(path) => Ok(path),
        None => default_database_path().map_err(Into::into),
    }
}

mod common;
mod compose;
mod scope;
mod signals;
mod snapshot;
mod sync;
mod work;

use common::*;
use compose::*;
use scope::*;
use signals::*;
#[cfg(test)]
pub(super) use snapshot::redact_workspace_path;
pub use snapshot::{command_error_output, redacted_status_snapshot};
use sync::*;
use work::*;

#[cfg(test)]
mod tests;
