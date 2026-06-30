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

fn compose_from_store(
    store: &MetadataStore,
    options: StatusOptions,
    state_root: PathBuf,
) -> Result<StatusCommandOutput, LocalStatusError> {
    let Some(workspace) = store.current_workspace()? else {
        return Ok(missing_metadata_status(&options));
    };
    if store.accepted_root_count(&workspace.id)? == 0 {
        return Ok(missing_metadata_status(&options));
    }

    let resolved = resolve_scope(
        store,
        options.requested_path.as_deref(),
        options.workspace_scope,
    )?;
    let workspace_id = resolved
        .workspace_id
        .clone()
        .unwrap_or_else(|| WorkspaceId::new("ws_local_uninitialized"));
    let project_id = resolved.project_id.clone();
    let scope = if options.workspace_scope {
        StatusScope::Workspace
    } else if project_id.is_some() {
        StatusScope::Project
    } else {
        StatusScope::Workspace
    };
    let query = resolved.event_query(50);
    let watermarks = store.scoped_event_watermarks(query)?;
    let recent_events = store.list_events_scoped(resolved.event_query(20))?;
    let status_events = store.list_status_signal_events_scoped(resolved.event_query(0))?;
    let unresolved_conflict_paths = unresolved_conflict_paths(&state_root)?
        .into_iter()
        .filter(|path| !status_path_is_source_control_metadata(path))
        .collect::<BTreeSet<_>>();
    let mut items = Vec::new();
    let mut limits = Vec::new();
    let mut attention_items = Vec::new();
    let mut next_actions = Vec::new();
    let mut level = StatusLevel::Healthy;

    apply_watermark_status(
        &watermarks,
        &mut items,
        &mut limits,
        &mut attention_items,
        &mut level,
    );
    apply_status_signal_events(
        &status_events,
        &watermarks,
        &unresolved_conflict_paths,
        &mut items,
        &mut attention_items,
        &mut level,
    );
    let sync_counts = sync_operation_counts_for_local_device(store, &workspace_id, &recent_events)?;
    apply_sync_operation_status(
        &workspace_id,
        &sync_counts,
        &mut items,
        &mut limits,
        &mut attention_items,
        &mut level,
    );
    apply_unresolved_conflict_status(
        &unresolved_conflict_paths,
        &workspace_id,
        &mut items,
        &mut limits,
        &mut attention_items,
        &mut next_actions,
        &mut level,
    )?;

    let total_projects = store.project_count(&workspace_id)?;
    let observed = store.observed_summary(&workspace_id)?;
    let projects_needing_attention = project_attention_summaries(
        store,
        &workspace_id,
        project_id.as_ref(),
        &watermarks,
        &unresolved_conflict_paths,
    )?;
    if !projects_needing_attention.is_empty() && level == StatusLevel::Healthy {
        level = StatusLevel::Attention;
        attention_items.push("Other projects need attention.".to_string());
    }
    let resolved_workspace_root = store
        .current_workspace_root()?
        .map(|path| display_root_path(&path))
        .or_else(|| Some("~/Code".to_string()));
    if total_projects == 0 && items.is_empty() {
        let mut item = base_status_item(
            StatusItemKind::Continuity,
            "Accepted workspace metadata is current; no projects have been observed yet.",
        );
        item.subject = Some(StatusSubject {
            kind: StatusSubjectKind::Workspace,
            id: workspace_id.as_str().to_string(),
            path: None,
        });
        items.push(item);
    }
    if let Some(summary) = observed.as_ref() {
        apply_observed_summary(
            &workspace_id,
            summary,
            watermarks.sync_state == Some(ComponentState::Ready),
            &mut items,
            &mut attention_items,
            &mut level,
        );
    }
    apply_env_setup_metadata(
        store,
        &workspace_id,
        project_id.as_ref(),
        &mut items,
        &mut attention_items,
        &mut level,
    )?;
    apply_work_view_metadata(
        store,
        &workspace_id,
        project_id.as_ref(),
        &mut items,
        &mut attention_items,
        &mut level,
    )?;
    apply_agent_lease_metadata(
        store,
        &workspace_id,
        project_id.as_ref(),
        &options.generated_at,
        &mut items,
        &mut attention_items,
        &mut level,
    )?;
    let index = durable_index_status(store, &workspace_id, project_id.as_ref())?;
    apply_index_status(
        index.as_ref(),
        &mut items,
        &mut limits,
        &mut attention_items,
        &mut level,
    );
    let hydration_budget =
        durable_hydration_budget_status(store, &workspace_id, project_id.as_ref())?;
    let hydration_progress = hydration_progress_from_events(&recent_events);
    let sync_queue = sync_queue_status(&sync_counts);

    Ok(StatusCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Status,
        generated_at: options.generated_at,
        workspace_id,
        project_id,
        scope: Some(scope),
        requested_path: options.requested_path,
        resolved_workspace_root,
        workspace_summary: Some(WorkspaceSummary {
            projects_needing_attention,
            total_projects: Some(total_projects),
            observed,
        }),
        index,
        hydration_budget,
        hydration_progress,
        sync_queue,
        status: WorkspaceStatus {
            level,
            attention_items,
        },
        items,
        limits,
        event_watermarks: watermarks,
        next_actions: if level == StatusLevel::Healthy {
            next_actions
        } else {
            if next_actions.is_empty() {
                next_actions.push(recent_events_action());
            }
            next_actions
        },
    })
}

fn conflict_resolution_action() -> SafeAction {
    SafeAction {
        label: "Resolve conflicts".to_string(),
        command: Some("bowline resolve ~/Code".to_string()),
    }
}

fn status_path_is_source_control_metadata(path: &str) -> bool {
    path.split('/')
        .any(|component| matches!(component, ".git" | ".jj" | ".hg" | ".svn"))
}

fn recent_events_action() -> SafeAction {
    SafeAction {
        label: "Inspect recent events".to_string(),
        command: Some("bowline status --watch".to_string()),
    }
}

fn missing_metadata_status(options: &StatusOptions) -> StatusCommandOutput {
    StatusCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Status,
        generated_at: options.generated_at.clone(),
        workspace_id: WorkspaceId::new("ws_local_uninitialized"),
        project_id: None,
        scope: Some(StatusScope::Workspace),
        requested_path: options.requested_path.clone(),
        resolved_workspace_root: Some("~/Code".to_string()),
        workspace_summary: Some(WorkspaceSummary::empty()),
        index: None,
        hydration_budget: None,
        hydration_progress: Vec::new(),
        sync_queue: None,
        status: WorkspaceStatus {
            level: StatusLevel::Attention,
            attention_items: vec!["bowline has not initialized local metadata yet.".to_string()],
        },
        items: vec![metadata_item(
            "Local metadata is missing; status is observational and did not create files.",
            None,
        )],
        limits: Vec::new(),
        event_watermarks: empty_watermarks(),
        next_actions: vec![SafeAction {
            label: "Initialize ~/Code when ready".to_string(),
            command: None,
        }],
    }
}

fn apply_observed_summary(
    workspace_id: &WorkspaceId,
    summary: &ObservedWorkspaceSummary,
    sync_ready: bool,
    items: &mut Vec<StatusItem>,
    attention_items: &mut Vec<String>,
    level: &mut StatusLevel,
) {
    if !sync_ready && *level == StatusLevel::Healthy {
        *level = StatusLevel::Attention;
        attention_items
            .push("Workspace has been observed locally; sync has not started yet.".to_string());
    }

    let mut item = base_status_item(
        StatusItemKind::Continuity,
        if sync_ready {
            format!(
                "Observed {} repos, {} workspace-sync paths, {} env files, {} generated/dependency paths; sync is active.",
                summary.repo_count,
                summary.workspace_sync_path_count,
                summary.env_file_count,
                summary.generated_path_count + summary.dependency_path_count,
            )
        } else {
            format!(
                "Observed {} repos, {} workspace-sync paths, {} env files, {} generated/dependency paths; no bytes have been uploaded.",
            summary.repo_count,
            summary.workspace_sync_path_count,
            summary.env_file_count,
            summary.generated_path_count + summary.dependency_path_count,
            )
        }
        .as_str(),
    );
    item.subject = Some(StatusSubject {
        kind: StatusSubjectKind::Workspace,
        id: workspace_id.as_str().to_string(),
        path: None,
    });
    items.push(item);

    if summary.repo_count > 0 {
        let mut item = base_status_item(
            StatusItemKind::Source,
            &format!(
                "Git observer is advisory for {} repo(s); bowline reads local metadata only and never fetches, commits, or uses Git as sync.",
                summary.repo_count
            ),
        );
        item.subject = Some(StatusSubject {
            kind: StatusSubjectKind::Workspace,
            id: workspace_id.as_str().to_string(),
            path: None,
        });
        items.push(item);
    }

    if summary.no_remote_repo_count > 0 {
        let mut item = base_status_item(
            StatusItemKind::Source,
            &format!(
                "{} repo(s) have no remote; bowline will still treat their files as syncable workspace state.",
                summary.no_remote_repo_count
            ),
        );
        item.subject = Some(StatusSubject {
            kind: StatusSubjectKind::Workspace,
            id: workspace_id.as_str().to_string(),
            path: None,
        });
        items.push(item);
    }

    if summary.stale_remote_tracking_repo_count > 0 {
        let mut item = base_status_item(
            StatusItemKind::Source,
            &format!(
                "{} repo(s) have local branch refs that differ from local remote-tracking refs; this is advisory only and bowline will not fetch or repair Git.",
                summary.stale_remote_tracking_repo_count
            ),
        );
        item.subject = Some(StatusSubject {
            kind: StatusSubjectKind::Workspace,
            id: workspace_id.as_str().to_string(),
            path: None,
        });
        items.push(item);
    }

    if summary.untracked_file_count > 0 {
        let mut item = base_status_item(
            StatusItemKind::Source,
            &format!(
                "{} untracked file(s) were observed as workspace continuity state.",
                summary.untracked_file_count
            ),
        );
        item.subject = Some(StatusSubject {
            kind: StatusSubjectKind::Workspace,
            id: workspace_id.as_str().to_string(),
            path: None,
        });
        items.push(item);
    }
}

fn apply_env_setup_metadata(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: Option<&ProjectId>,
    items: &mut Vec<StatusItem>,
    attention_items: &mut Vec<String>,
    level: &mut StatusLevel,
) -> Result<(), LocalStatusError> {
    let env_records = store.env_records(workspace_id)?;
    let visible_env_records = env_records
        .iter()
        .filter(|record| project_id.is_none() || record.project_id.as_ref() == project_id)
        .collect::<Vec<_>>();
    if !visible_env_records.is_empty() {
        let source_count = visible_env_records
            .iter()
            .map(|record| record.source_path.as_str())
            .collect::<HashSet<_>>()
            .len();
        let stale_count = visible_env_records
            .iter()
            .filter(|record| record.materialization_state == "stale")
            .count();
        let mut item = base_status_item(
            StatusItemKind::Env,
            &format!(
                "{} project env record(s) across {} file(s) are tracked; values are redacted.",
                visible_env_records.len(),
                source_count
            ),
        );
        item.subject = Some(StatusSubject {
            kind: StatusSubjectKind::EnvRecord,
            id: visible_env_records
                .first()
                .map(|record| record.id.as_str().to_string())
                .unwrap_or_else(|| "env-records".to_string()),
            path: visible_env_records
                .first()
                .map(|record| record.source_path.clone()),
        });
        item.path = visible_env_records
            .first()
            .map(|record| record.source_path.clone());
        item.classification = Some(PathClassification::ProjectEnv);
        item.mode = Some(MaterializationMode::ProjectEnv);
        item.access = visible_env_records
            .first()
            .map(|record| record.access.clone())
            .unwrap_or_default();
        item.project_id = visible_env_records
            .first()
            .and_then(|record| record.project_id.clone());
        item.env_record_id = visible_env_records.first().map(|record| record.id.clone());
        items.push(item);

        if stale_count > 0 {
            if *level == StatusLevel::Healthy {
                *level = StatusLevel::Attention;
            }
            attention_items.push(format!(
                "{stale_count} materialized env record(s) are stale; values remain redacted."
            ));
        }
    }

    let setup_receipts = store.setup_receipts(workspace_id)?;
    let visible_receipts = setup_receipts
        .iter()
        .filter(|record| project_id.is_none() || record.project_id.as_ref() == project_id)
        .collect::<Vec<_>>();
    for receipt in &visible_receipts {
        if setup_receipt_needs_current_attention(store, workspace_id, receipt)? {
            if *level == StatusLevel::Healthy {
                *level = StatusLevel::Attention;
            }
            attention_items.push(format!(
                "Setup for {} needs attention: {}.",
                receipt.cwd, receipt.state
            ));
        }
    }
    for receipt in visible_receipts.iter().take(3) {
        let mut item = base_status_item(
            StatusItemKind::Setup,
            &format!(
                "Setup {} via {}; {}",
                receipt.state,
                receipt.trigger,
                if receipt.redacted_summary.is_empty() {
                    "output is redacted.".to_string()
                } else {
                    receipt.redacted_summary.clone()
                }
            ),
        );
        item.subject = Some(StatusSubject {
            kind: StatusSubjectKind::SetupReceipt,
            id: receipt.id.clone(),
            path: Some(receipt.cwd.clone()),
        });
        item.path = Some(receipt.cwd.clone());
        item.project_id = receipt.project_id.clone();
        items.push(item);
    }

    Ok(())
}

fn setup_receipt_needs_current_attention(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    receipt: &crate::metadata::SetupReceiptRecord,
) -> Result<bool, LocalStatusError> {
    if !matches!(
        receipt.state.as_str(),
        "blocked" | "failed" | "approval-required"
    ) {
        return Ok(false);
    }
    let Some(project_id) = receipt.project_id.as_ref() else {
        return Ok(true);
    };
    Ok(store
        .project_hot_state(workspace_id, project_id)?
        .is_none_or(|state| state == "setup.blocked"))
}

fn project_attention_summaries(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    current_project_id: Option<&ProjectId>,
    watermarks: &EventWatermarks,
    unresolved_conflict_paths: &BTreeSet<String>,
) -> Result<Vec<ProjectAttentionSummary>, LocalStatusError> {
    let mut summaries = Vec::new();

    for project in store.projects(workspace_id)? {
        if current_project_id == Some(&project.id) {
            continue;
        }

        let events = store.list_status_signal_events_scoped(EventQuery {
            workspace_id: Some(workspace_id.clone()),
            project_id: Some(project.id.clone()),
            path_prefix: Some(project.path.clone()),
            limit: 0,
        })?;
        let mut items = Vec::new();
        let mut attention_items = Vec::new();
        let mut level = StatusLevel::Healthy;
        apply_status_signal_events(
            &events,
            watermarks,
            unresolved_conflict_paths,
            &mut items,
            &mut attention_items,
            &mut level,
        );
        if level != StatusLevel::Healthy
            && items
                .iter()
                .all(|item| item.kind == StatusItemKind::Conflict)
            && !unresolved_conflict_paths.iter().any(|path| {
                path == &project.path || path.starts_with(&format!("{}/", project.path))
            })
        {
            continue;
        }

        if level != StatusLevel::Healthy {
            let summary = attention_items
                .first()
                .cloned()
                .or_else(|| items.first().map(|item| item.summary.clone()))
                .unwrap_or_else(|| "Project needs attention.".to_string());
            summaries.push(ProjectAttentionSummary {
                project_id: project.id,
                path: project.path,
                level,
                summary,
            });
        }
    }

    Ok(summaries)
}

fn display_root_path(path: &str) -> String {
    if path == "~" || path.starts_with("~/") {
        return path.to_string();
    }

    let path_buf = PathBuf::from(path);
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return path.to_string();
    };
    let Ok(relative) = path_buf.strip_prefix(home) else {
        return path.to_string();
    };

    if relative.as_os_str().is_empty() {
        "~".to_string()
    } else {
        format!("~/{}", relative.display())
    }
}

fn limited_metadata_status(options: &StatusOptions, state: &DatabaseState) -> StatusCommandOutput {
    let reason = match state {
        DatabaseState::FutureIncompatible { found, supported } => {
            format!("metadata schema version {found} is newer than supported version {supported}")
        }
        DatabaseState::Corrupt => "metadata database is corrupt".to_string(),
        DatabaseState::UnsupportedSchema => {
            "metadata database uses an unsupported schema".to_string()
        }
        DatabaseState::Locked => "metadata database is locked".to_string(),
        DatabaseState::PermissionDenied => "metadata database cannot be opened".to_string(),
        DatabaseState::Missing | DatabaseState::Empty | DatabaseState::Current => {
            "metadata database is unavailable".to_string()
        }
    };

    StatusCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Status,
        generated_at: options.generated_at.clone(),
        workspace_id: WorkspaceId::new("ws_local_limited"),
        project_id: None,
        scope: Some(StatusScope::Workspace),
        requested_path: options.requested_path.clone(),
        resolved_workspace_root: Some("~/Code".to_string()),
        workspace_summary: Some(WorkspaceSummary::empty()),
        index: None,
        hydration_budget: None,
        hydration_progress: Vec::new(),
        sync_queue: None,
        status: WorkspaceStatus {
            level: StatusLevel::Limited,
            attention_items: vec![format!("Local metadata is limited: {reason}.")],
        },
        items: vec![metadata_item(
            "Local metadata could not be opened; source files were not modified.",
            Some(EventName::MetadataCorrupt),
        )],
        limits: vec![LimitedCapability {
            capability: "local metadata".to_string(),
            unavailable_because: reason,
            still_works: vec![
                "source files stay readable".to_string(),
                "status can report recovery guidance".to_string(),
            ],
            path: None,
        }],
        event_watermarks: empty_watermarks(),
        next_actions: vec![SafeAction {
            label: "Check local metadata".to_string(),
            command: None,
        }],
    }
}

fn empty_watermarks() -> EventWatermarks {
    EventWatermarks {
        last_scan_at: None,
        last_event_id: None,
        event_lag_ms: Some(0),
        sync_state: None,
        watcher_state: None,
        network_state: None,
    }
}

fn durable_index_status(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: Option<&ProjectId>,
) -> Result<Option<IndexStatus>, MetadataError> {
    let (count, ready_count, source_watermark, indexed_watermark, updated_at): (
        i64,
        i64,
        i64,
        i64,
        Option<String>,
    ) = store.connection().query_row(
        "SELECT COUNT(*),
                COALESCE(SUM(CASE WHEN state = 'ready' THEN 1 ELSE 0 END), 0),
                COALESCE(MAX(source_watermark), 0),
                COALESCE(MAX(indexed_watermark), 0),
                MAX(updated_at)
         FROM index_work
         WHERE workspace_id = ?1 AND (?2 IS NULL OR project_id = ?2)",
        rusqlite::params![workspace_id.as_str(), project_id.map(|id| id.as_str())],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        },
    )?;
    if count == 0 {
        return Ok(None);
    }
    let state = if ready_count == count && indexed_watermark >= source_watermark {
        IndexState::Ready
    } else if indexed_watermark < source_watermark {
        IndexState::Stale
    } else {
        IndexState::Degraded
    };
    Ok(Some(IndexStatus {
        state,
        source: IndexSource::Local,
        indexed_at: (state == IndexState::Ready)
            .then(|| updated_at.clone())
            .flatten(),
        updated_at,
        snapshot_id: None,
        index_pack_object_key: None,
        path_count: 0,
        file_count: 0,
        indexed_bytes: 0,
        pending_path_count: Some((count - ready_count).max(0) as u64),
        degraded_reason: (state != IndexState::Ready).then_some(IndexDegradedReason::Missing),
        summary: if state == IndexState::Ready {
            "Index metadata is current.".to_string()
        } else {
            "Index metadata has pending or stale work.".to_string()
        },
        next_action: None,
    }))
}

fn apply_index_status(
    index: Option<&IndexStatus>,
    items: &mut Vec<StatusItem>,
    limits: &mut Vec<LimitedCapability>,
    attention_items: &mut Vec<String>,
    level: &mut StatusLevel,
) {
    let Some(index) = index else {
        return;
    };
    match index.state {
        IndexState::Ready => {}
        IndexState::Stale | IndexState::Rebuilding => {
            if *level == StatusLevel::Healthy {
                *level = StatusLevel::Attention;
            }
            attention_items.push(if index.state == IndexState::Rebuilding {
                "Index is rebuilding.".to_string()
            } else {
                "Index metadata is stale.".to_string()
            });
            let mut item = base_status_item(StatusItemKind::Index, &index.summary);
            item.subject = Some(StatusSubject {
                kind: StatusSubjectKind::Index,
                id: "index-local".to_string(),
                path: None,
            });
            item.event_name = Some(EventName::IndexDegraded);
            items.push(item);
        }
        IndexState::Degraded => {
            *level = StatusLevel::Limited;
            attention_items.push("Index is degraded.".to_string());
            limits.push(LimitedCapability {
                capability: "index".to_string(),
                unavailable_because: index.summary.clone(),
                still_works: vec![
                    "status".to_string(),
                    "local file access".to_string(),
                    "bounded hydration".to_string(),
                ],
                path: None,
            });
            let mut item = base_status_item(StatusItemKind::Index, &index.summary);
            item.subject = Some(StatusSubject {
                kind: StatusSubjectKind::Index,
                id: "index-local".to_string(),
                path: None,
            });
            item.event_name = Some(EventName::IndexDegraded);
            items.push(item);
        }
    }
}

fn durable_hydration_budget_status(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: Option<&ProjectId>,
) -> Result<Option<HydrationBudgetStatus>, MetadataError> {
    let active_leases = store
        .agent_leases(workspace_id)?
        .into_iter()
        .filter(|lease| match project_id {
            Some(project_id) => lease.project_id.as_str() == project_id.as_str(),
            None => true,
        })
        .filter(|lease| {
            matches!(
                lease.execution_state,
                AgentLeaseExecutionState::Active | AgentLeaseExecutionState::Blocked
            )
        })
        .collect::<Vec<_>>();

    match active_leases.as_slice() {
        [lease] => lease_budget_status(
            store,
            workspace_id,
            &lease.project_id,
            &lease.id,
            lease.hydrate_budget_bytes,
        )
        .map(Some),
        _ => Ok(None),
    }
}

fn metadata_item(summary: &str, event_name: Option<EventName>) -> StatusItem {
    let mut item = base_status_item(StatusItemKind::Metadata, summary);
    item.subject = Some(StatusSubject {
        kind: StatusSubjectKind::Metadata,
        id: "metadata-local".to_string(),
        path: None,
    });
    item.event_name = event_name;
    item
}

fn apply_watermark_status(
    watermarks: &EventWatermarks,
    items: &mut Vec<StatusItem>,
    limits: &mut Vec<LimitedCapability>,
    attention_items: &mut Vec<String>,
    level: &mut StatusLevel,
) {
    if matches!(
        watermarks.sync_state,
        Some(ComponentState::Degraded | ComponentState::Unavailable)
    ) {
        *level = StatusLevel::Limited;
        attention_items.push("Sync is degraded.".to_string());
        limits.push(LimitedCapability {
            capability: "sync".to_string(),
            unavailable_because: "sync degraded".to_string(),
            still_works: vec![
                "local files".to_string(),
                "status".to_string(),
                "local metadata inspection".to_string(),
            ],
            path: None,
        });
        items.push(component_item(
            StatusItemKind::Materialization,
            "Sync is degraded; local files and status still work.",
            EventName::SyncDegraded,
        ));
    }

    if matches!(
        watermarks.watcher_state,
        Some(ComponentState::Degraded | ComponentState::Unavailable)
    ) {
        *level = StatusLevel::Limited;
        attention_items.push("Native file watching is degraded.".to_string());
        limits.push(LimitedCapability {
            capability: "watch".to_string(),
            unavailable_because: "native watcher unavailable".to_string(),
            still_works: vec![
                "manual status".to_string(),
                "scheduled reconciliation".to_string(),
            ],
            path: None,
        });
        items.push(component_item(
            StatusItemKind::Watcher,
            "The watcher is degraded, so bowline is using reconciliation.",
            EventName::WatcherDegraded,
        ));
    }

    if matches!(
        watermarks.network_state,
        Some(NetworkState::Offline | NetworkState::Degraded)
    ) {
        *level = StatusLevel::Limited;
        let unavailable_because = if matches!(watermarks.network_state, Some(NetworkState::Offline))
        {
            "network offline"
        } else {
            "network degraded"
        };
        attention_items.push("Network is unavailable.".to_string());
        limits.push(LimitedCapability {
            capability: "hydrate".to_string(),
            unavailable_because: unavailable_because.to_string(),
            still_works: vec![
                "project structure".to_string(),
                "local cached reads".to_string(),
            ],
            path: None,
        });
        items.push(component_item(
            StatusItemKind::Network,
            "Network is offline; local cached state remains available.",
            EventName::NetworkOffline,
        ));
    }
}

fn apply_sync_operation_status(
    workspace_id: &WorkspaceId,
    counts: &SyncOperationCounts,
    items: &mut Vec<StatusItem>,
    limits: &mut Vec<LimitedCapability>,
    attention_items: &mut Vec<String>,
    level: &mut StatusLevel,
) {
    let pending = counts.queued
        + counts.claimed
        + counts.waiting_retry
        + counts.blocked_offline
        + counts.attention;
    if pending == 0 {
        return;
    }

    let summary = sync_operation_summary(counts);
    let mut item = base_status_item(StatusItemKind::Materialization, &summary);
    item.subject = Some(StatusSubject {
        kind: StatusSubjectKind::Workspace,
        id: workspace_id.as_str().to_string(),
        path: None,
    });
    items.push(item);

    if counts.attention > 0 {
        *level = StatusLevel::Attention;
        attention_items.push("Sync queue needs attention.".to_string());
        limits.push(LimitedCapability {
            capability: "sync".to_string(),
            unavailable_because: "sync queue needs attention".to_string(),
            still_works: vec!["local files".to_string(), "status".to_string()],
            path: None,
        });
    } else if counts.blocked_offline > 0 {
        *level = StatusLevel::Limited;
        attention_items.push("Sync queue is waiting for offline recovery.".to_string());
        limits.push(LimitedCapability {
            capability: "sync".to_string(),
            unavailable_because: "sync queue is waiting for offline recovery".to_string(),
            still_works: sync_queue_wait_still_works(),
            path: None,
        });
    } else if counts.waiting_retry > 0 {
        *level = StatusLevel::Limited;
        attention_items.push("Sync queue is waiting for retry.".to_string());
        limits.push(LimitedCapability {
            capability: "sync".to_string(),
            unavailable_because: "sync queue is waiting for retry".to_string(),
            still_works: sync_queue_wait_still_works(),
            path: None,
        });
    }
}

fn sync_queue_status(counts: &SyncOperationCounts) -> Option<SyncQueueStatus> {
    let status = SyncQueueStatus {
        queued: counts.queued,
        claimed: counts.claimed,
        waiting_retry: counts.waiting_retry,
        blocked_offline: counts.blocked_offline,
        attention: counts.attention,
        completed: counts.completed,
    };
    status.has_pending_work().then_some(status)
}

fn apply_unresolved_conflict_status(
    paths: &BTreeSet<String>,
    workspace_id: &WorkspaceId,
    items: &mut Vec<StatusItem>,
    limits: &mut Vec<LimitedCapability>,
    attention_items: &mut Vec<String>,
    next_actions: &mut Vec<SafeAction>,
    level: &mut StatusLevel,
) -> Result<(), LocalStatusError> {
    if paths.is_empty() {
        return Ok(());
    }

    *level = StatusLevel::Attention;
    let summary = if paths.len() == 1 {
        format!(
            "1 unresolved conflict needs attention: {}.",
            paths.iter().next().expect("path exists")
        )
    } else {
        format!("{} unresolved conflicts need attention.", paths.len())
    };
    attention_items.push(summary.clone());

    let mut item = base_status_item(StatusItemKind::Conflict, &summary);
    item.subject = Some(StatusSubject {
        kind: StatusSubjectKind::Workspace,
        id: workspace_id.as_str().to_string(),
        path: None,
    });
    item.path = paths.iter().next().cloned();
    item.event_name = Some(EventName::ConflictBundleCreated);
    items.push(item);

    limits.push(LimitedCapability {
        capability: "sync".to_string(),
        unavailable_because: "unresolved conflict".to_string(),
        still_works: vec![
            "local files".to_string(),
            "status".to_string(),
            "conflict resolution".to_string(),
        ],
        path: None,
    });
    next_actions.push(conflict_resolution_action());
    Ok(())
}

fn sync_operation_counts_for_local_device(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    recent_events: &[bowline_core::events::WorkspaceEvent],
) -> Result<SyncOperationCounts, MetadataError> {
    match env::var("BOWLINE_DEVICE_ID") {
        Ok(device_id) if !device_id.trim().is_empty() => {
            store.sync_operation_counts_for_device(workspace_id, &DeviceId::new(device_id))
        }
        _ => {
            if let Some(device_id) = recent_sync_device_id(recent_events) {
                store.sync_operation_counts_for_device(workspace_id, &device_id)
            } else {
                store.sync_operation_counts(workspace_id)
            }
        }
    }
}

fn recent_sync_device_id(events: &[bowline_core::events::WorkspaceEvent]) -> Option<DeviceId> {
    events
        .iter()
        .find(|event| {
            matches!(
                event.name,
                EventName::SyncStarted
                    | EventName::SyncCompleted
                    | EventName::SyncLimited
                    | EventName::SyncDegraded
                    | EventName::SyncRecovered
            ) && event.device_id.is_some()
        })
        .and_then(|event| event.device_id.clone())
}

fn sync_queue_wait_still_works() -> Vec<String> {
    vec![
        "local files".to_string(),
        "status".to_string(),
        "scheduled retry".to_string(),
    ]
}

fn sync_operation_summary(counts: &SyncOperationCounts) -> String {
    format!(
        "Sync queue: {} queued, {} running, {} waiting retry, {} offline, {} attention.",
        counts.queued,
        counts.claimed,
        counts.waiting_retry,
        counts.blocked_offline,
        counts.attention
    )
}

fn hydration_progress_from_events(
    events: &[bowline_core::events::WorkspaceEvent],
) -> Vec<HydrationProgress> {
    let Some(event) = events
        .iter()
        .find(|event| event.name == EventName::HydrationCompleted)
        .or_else(|| {
            events
                .iter()
                .find(|event| event.name == EventName::HydrationBlocked)
        })
        .or_else(|| {
            events
                .iter()
                .find(|event| event.name == EventName::HydrationStarted)
        })
    else {
        return Vec::new();
    };
    let bytes = payload_u64(event, "bytes");
    let (bytes_done, bytes_remaining) = match event.name {
        EventName::HydrationCompleted => (bytes, 0),
        _ => (0, bytes),
    };
    let cause = payload_str(event, "cause").unwrap_or_else(|| event_name_label(event.name));
    vec![HydrationProgress {
        project_id: event.project_id.clone(),
        bytes_done,
        bytes_remaining,
        cause,
    }]
}

fn payload_u64(event: &bowline_core::events::WorkspaceEvent, key: &str) -> u64 {
    event
        .payload
        .get(key)
        .and_then(|value| value.as_u64())
        .unwrap_or(0)
}

fn payload_str(event: &bowline_core::events::WorkspaceEvent, key: &str) -> Option<String> {
    event
        .payload
        .get(key)
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned)
}

fn apply_event_status(
    event: &bowline_core::events::WorkspaceEvent,
    items: &mut Vec<StatusItem>,
    attention_items: &mut Vec<String>,
    level: &mut StatusLevel,
) {
    match event.severity {
        EventSeverity::Info => {}
        EventSeverity::Attention => {
            if *level == StatusLevel::Healthy {
                *level = StatusLevel::Attention;
            }
            attention_items.push(event.summary.clone());
        }
        EventSeverity::Limited => {
            *level = StatusLevel::Limited;
            attention_items.push(event.summary.clone());
        }
    }

    if event.severity != EventSeverity::Info {
        let mut item = base_status_item(status_item_kind_for_event(event.name), &event.summary);
        item.subject = event.subject.as_ref().map(|subject| StatusSubject {
            kind: status_subject_kind(subject.kind),
            id: subject.id.clone(),
            path: subject.path.clone(),
        });
        item.path = event.path.clone();
        item.event_id = Some(event.id.clone());
        item.event_name = Some(event.name);
        item.device_id = event.device_id.clone();
        item.lease_id = event.lease_id.clone();
        item.project_id = event.project_id.clone();
        items.push(item);
    }
}

fn apply_status_signal_events(
    events: &[bowline_core::events::WorkspaceEvent],
    watermarks: &EventWatermarks,
    unresolved_conflict_paths: &BTreeSet<String>,
    items: &mut Vec<StatusItem>,
    attention_items: &mut Vec<String>,
    level: &mut StatusLevel,
) {
    let mut cleared = HashSet::new();
    let mut applied = HashSet::new();

    for event in events {
        for key in status_clear_keys(event) {
            cleared.insert(key);
        }

        let Some(key) = status_signal_key(event) else {
            continue;
        };
        if cleared.contains(&key) || applied.contains(&key) {
            continue;
        }
        if is_conflict_signal(event)
            && !conflict_signal_is_unresolved(event, unresolved_conflict_paths)
        {
            continue;
        }
        if should_apply_event_status(event, watermarks) {
            apply_event_status(event, items, attention_items, level);
            applied.insert(key);
        }
    }
}

fn is_conflict_signal(event: &bowline_core::events::WorkspaceEvent) -> bool {
    matches!(
        event.name,
        EventName::ConflictCreated
            | EventName::ConflictBundleCreated
            | EventName::ConflictResolutionProposed
    )
}

fn conflict_signal_is_unresolved(
    event: &bowline_core::events::WorkspaceEvent,
    unresolved_conflict_paths: &BTreeSet<String>,
) -> bool {
    if unresolved_conflict_paths.is_empty() {
        return false;
    }
    event
        .path
        .as_deref()
        .or_else(|| {
            event
                .subject
                .as_ref()
                .and_then(|subject| subject.path.as_deref())
        })
        .is_none_or(|path| unresolved_conflict_paths.contains(path))
}

fn apply_work_view_metadata(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: Option<&ProjectId>,
    items: &mut Vec<StatusItem>,
    attention_items: &mut Vec<String>,
    level: &mut StatusLevel,
) -> Result<(), LocalStatusError> {
    for view in store.work_views(workspace_id, true, None)? {
        if let Some(project_id) = project_id
            && &view.project_id != project_id
        {
            continue;
        }
        let needs_attention = matches!(view.lifecycle, WorkViewLifecycle::ReviewReady)
            || matches!(
                view.sync_state,
                WorkViewSyncState::Attention | WorkViewSyncState::Conflicted
            );
        if !needs_attention {
            continue;
        }
        if items.iter().any(|item| {
            item.kind == StatusItemKind::WorkView
                && item
                    .subject
                    .as_ref()
                    .is_some_and(|subject| subject.id == view.id.as_str())
        }) {
            continue;
        }
        if *level == StatusLevel::Healthy {
            *level = StatusLevel::Attention;
        }
        let summary = format!("{} is review-ready; workspace remains usable.", view.name);
        attention_items.push(summary.clone());
        let mut item = base_status_item(StatusItemKind::WorkView, &summary);
        item.subject = Some(StatusSubject {
            kind: StatusSubjectKind::WorkView,
            id: view.id.as_str().to_string(),
            path: Some(view.visible_path.clone()),
        });
        item.path = Some(view.visible_path);
        item.project_id = Some(view.project_id);
        item.event_name = Some(EventName::WorkReviewReady);
        items.push(item);
    }
    Ok(())
}

fn apply_agent_lease_metadata(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: Option<&ProjectId>,
    generated_at: &str,
    items: &mut Vec<StatusItem>,
    attention_items: &mut Vec<String>,
    level: &mut StatusLevel,
) -> Result<(), LocalStatusError> {
    recover_provisional_agent_leases(store, workspace_id, generated_at)
        .map_err(agent_recovery_status_error)?;
    for lease in store.agent_leases(workspace_id)? {
        if let Some(project_id) = project_id
            && &lease.project_id != project_id
        {
            continue;
        }
        let visible = matches!(
            lease.execution_state,
            AgentLeaseExecutionState::Active | AgentLeaseExecutionState::Blocked
        ) || matches!(
            lease.output_state,
            AgentLeaseOutputState::ReviewReady | AgentLeaseOutputState::Conflicted
        );
        if !visible {
            continue;
        }
        let needs_attention =
            matches!(
                lease.output_state,
                AgentLeaseOutputState::ReviewReady | AgentLeaseOutputState::Conflicted
            ) || matches!(lease.execution_state, AgentLeaseExecutionState::Blocked);
        if needs_attention && *level == StatusLevel::Healthy {
            *level = StatusLevel::Attention;
        }
        let summary = match lease.output_state {
            AgentLeaseOutputState::ReviewReady => {
                format!("Agent lease {} is ready for review.", lease.id.as_str())
            }
            AgentLeaseOutputState::Conflicted => {
                format!("Agent lease {} has conflicted output.", lease.id.as_str())
            }
            _ if lease.execution_state == AgentLeaseExecutionState::Blocked => {
                format!("Agent lease {} needs human attention.", lease.id.as_str())
            }
            _ => format!("Agent lease {} is active.", lease.id.as_str()),
        };
        if needs_attention {
            attention_items.push(summary.clone());
        }
        let mut item = base_status_item(StatusItemKind::Lease, &summary);
        item.subject = Some(StatusSubject {
            kind: StatusSubjectKind::Lease,
            id: lease.id.as_str().to_string(),
            path: Some(lease.work_view_path.clone()),
        });
        item.path = Some(lease.work_view_path);
        item.project_id = Some(lease.project_id);
        item.lease_id = Some(lease.id);
        item.event_name = Some(match lease.output_state {
            AgentLeaseOutputState::ReviewReady => EventName::LeaseReviewReady,
            AgentLeaseOutputState::Conflicted => EventName::LeaseBlocked,
            _ if lease.execution_state == AgentLeaseExecutionState::Blocked => {
                EventName::LeaseBlocked
            }
            _ => EventName::LeaseUpdated,
        });
        items.push(item);
    }
    Ok(())
}

fn agent_recovery_status_error(error: AgentError) -> LocalStatusError {
    match error {
        AgentError::Metadata(error) => LocalStatusError::Metadata(error),
        AgentError::Event(error) => LocalStatusError::Events(error),
        AgentError::Io(error) => LocalStatusError::Path(error),
        AgentError::WorkView(WorkViewError::Metadata(error)) => LocalStatusError::Metadata(error),
        AgentError::WorkView(WorkViewError::Io(error)) => LocalStatusError::Path(error),
        other => LocalStatusError::Metadata(MetadataError::InvalidStorageMetadata(format!(
            "agent lease recovery failed: {other}"
        ))),
    }
}

fn status_clear_keys(event: &bowline_core::events::WorkspaceEvent) -> Vec<String> {
    let categories: &[&str] = match event.name {
        EventName::ConflictResolutionAccepted | EventName::ConflictResolutionRejected => {
            &["conflict"]
        }
        EventName::DeviceApproved | EventName::DeviceRevoked => &["device-approval"],
        EventName::SetupCompleted => &["setup"],
        EventName::HydrationCompleted
        | EventName::HydrationBudgetCommitted
        | EventName::HydrationBudgetReleased
        | EventName::HydrationBudgetOverrideGranted => &["hydration"],
        EventName::PolicyChanged => &["policy"],
        EventName::LeaseCreated
        | EventName::LeaseUpdated
        | EventName::LeaseCompleted
        | EventName::LeaseRevoked
        | EventName::LeaseCleanupCompleted => &["lease"],
        EventName::DaemonRecovered => &["daemon"],
        EventName::IndexUpdated => &["index"],
        EventName::SyncCompleted | EventName::SyncRecovered => &["sync"],
        EventName::WatcherRecovered => &["watcher"],
        EventName::NetworkRecovered => &["network"],
        EventName::WorkAccepted
        | EventName::WorkArchived
        | EventName::WorkCleanupCompleted
        | EventName::WorkDiscarded
        | EventName::WorkRestored => &["work-view"],
        _ => &[],
    };

    categories
        .iter()
        .map(|category| status_key(category, event))
        .collect()
}

fn status_signal_key(event: &bowline_core::events::WorkspaceEvent) -> Option<String> {
    if event.severity == EventSeverity::Info {
        return None;
    }

    let category = match event.name {
        EventName::ConflictCreated
        | EventName::ConflictBundleCreated
        | EventName::ConflictResolutionProposed => "conflict".to_string(),
        EventName::DeviceApprovalRequested => "device-approval".to_string(),
        EventName::SetupBlocked => "setup".to_string(),
        EventName::HydrationBlocked | EventName::HydrationBudgetDenied => "hydration".to_string(),
        EventName::PolicyNeedsApproval => "policy".to_string(),
        EventName::LeaseExpired => "lease".to_string(),
        EventName::DaemonDegraded => "daemon".to_string(),
        EventName::IndexDegraded => "index".to_string(),
        EventName::SyncLimited | EventName::SyncDegraded => "sync".to_string(),
        EventName::WatcherDegraded => "watcher".to_string(),
        EventName::NetworkOffline => "network".to_string(),
        EventName::WorkReviewReady => "work-view".to_string(),
        _ => event_name_label(event.name),
    };

    Some(status_key(&category, event))
}

fn status_key(category: &str, event: &bowline_core::events::WorkspaceEvent) -> String {
    let identity = if category == "setup" {
        status_path_or_project_identity(event)
    } else {
        status_identity(event)
    };
    format!("{category}:{identity}")
}

fn status_path_or_project_identity(event: &bowline_core::events::WorkspaceEvent) -> String {
    if let Some(path) = &event.path {
        return format!("path:{path}");
    }
    if let Some(subject) = &event.subject
        && let Some(path) = &subject.path
    {
        return format!("path:{path}");
    }
    if let Some(project_id) = &event.project_id {
        return format!("project:{}", project_id.as_str());
    }
    status_identity(event)
}

fn status_identity(event: &bowline_core::events::WorkspaceEvent) -> String {
    if let Some(subject) = &event.subject {
        if !subject.id.is_empty() {
            return format!("subject:{}", subject.id);
        }
        if let Some(path) = &subject.path {
            return format!("path:{path}");
        }
    }
    if let Some(path) = &event.path {
        return format!("path:{path}");
    }
    if let Some(lease_id) = &event.lease_id {
        return format!("lease:{}", lease_id.as_str());
    }
    if let Some(device_id) = &event.device_id {
        return format!("device:{}", device_id.as_str());
    }
    if let Some(project_id) = &event.project_id {
        return format!("project:{}", project_id.as_str());
    }
    format!("workspace:{}", event.workspace_id.as_str())
}

fn should_apply_event_status(
    event: &bowline_core::events::WorkspaceEvent,
    watermarks: &EventWatermarks,
) -> bool {
    match event.name {
        EventName::SyncLimited | EventName::SyncDegraded => matches!(
            watermarks.sync_state,
            Some(ComponentState::Degraded | ComponentState::Unavailable)
        ),
        EventName::WatcherDegraded => matches!(
            watermarks.watcher_state,
            Some(ComponentState::Degraded | ComponentState::Unavailable)
        ),
        EventName::NetworkOffline => matches!(
            watermarks.network_state,
            Some(NetworkState::Offline | NetworkState::Degraded)
        ),
        _ => true,
    }
}

#[derive(Debug, Clone)]
struct ResolvedScope {
    workspace_id: Option<WorkspaceId>,
    project_id: Option<ProjectId>,
    project_path: Option<String>,
}

impl ResolvedScope {
    fn event_query(&self, limit: u32) -> EventQuery {
        EventQuery {
            workspace_id: self.workspace_id.clone(),
            project_id: self.project_id.clone(),
            path_prefix: self.project_path.clone(),
            limit,
        }
    }
}

fn resolve_scope(
    store: &MetadataStore,
    requested_path: Option<&str>,
    workspace_scope: bool,
) -> Result<ResolvedScope, LocalStatusError> {
    let workspace_id = store.current_workspace()?.map(|record| record.id);
    let project = if workspace_scope {
        None
    } else if let Some(path) = requested_path {
        store.current_project_by_path(path)?
    } else {
        None
    };

    Ok(ResolvedScope {
        workspace_id,
        project_id: project.as_ref().map(|record| record.id.clone()),
        project_path: project.map(|record| record.path),
    })
}

fn status_subject_kind(kind: EventSubjectKind) -> StatusSubjectKind {
    match kind {
        EventSubjectKind::Workspace => StatusSubjectKind::Workspace,
        EventSubjectKind::Root => StatusSubjectKind::Root,
        EventSubjectKind::Project => StatusSubjectKind::Project,
        EventSubjectKind::Path | EventSubjectKind::Content | EventSubjectKind::Pack => {
            StatusSubjectKind::Path
        }
        EventSubjectKind::Snapshot => StatusSubjectKind::Snapshot,
        EventSubjectKind::Policy => StatusSubjectKind::Policy,
        EventSubjectKind::EnvRecord => StatusSubjectKind::EnvRecord,
        EventSubjectKind::SetupReceipt => StatusSubjectKind::SetupReceipt,
        EventSubjectKind::Conflict => StatusSubjectKind::Conflict,
        EventSubjectKind::WorkView => StatusSubjectKind::WorkView,
        EventSubjectKind::Lease => StatusSubjectKind::Lease,
        EventSubjectKind::Overlay => StatusSubjectKind::Overlay,
        EventSubjectKind::Index => StatusSubjectKind::Index,
        EventSubjectKind::Device => StatusSubjectKind::Device,
        EventSubjectKind::Metadata => StatusSubjectKind::Metadata,
        EventSubjectKind::Component => StatusSubjectKind::Component,
    }
}

fn status_item_kind_for_event(name: EventName) -> StatusItemKind {
    match name {
        EventName::PolicyClassified | EventName::PolicyNeedsApproval | EventName::PolicyChanged => {
            StatusItemKind::Policy
        }
        EventName::DeviceApprovalRequested
        | EventName::DeviceApproved
        | EventName::DeviceDenied
        | EventName::DeviceRevoked => StatusItemKind::Device,
        EventName::ConflictCreated
        | EventName::ConflictBundleCreated
        | EventName::ConflictResolutionProposed
        | EventName::ConflictResolutionAccepted
        | EventName::ConflictResolutionRejected => StatusItemKind::Conflict,
        EventName::LeaseCreated
        | EventName::LeaseUpdated
        | EventName::LeaseExpired
        | EventName::LeaseCompleted
        | EventName::LeaseBlocked
        | EventName::LeaseRevoked
        | EventName::LeaseReviewReady
        | EventName::LeaseToolInvoked
        | EventName::LeaseToolDenied
        | EventName::LeaseHydrationRequested
        | EventName::LeaseCleanupCompleted => StatusItemKind::Lease,
        EventName::WorkCreated
        | EventName::WorkUpdated
        | EventName::WorkReviewReady
        | EventName::WorkAccepted
        | EventName::WorkDiscarded
        | EventName::WorkRestored
        | EventName::WorkExpired
        | EventName::WorkArchived
        | EventName::WorkCleanupPreviewed
        | EventName::WorkCleanupCompleted => StatusItemKind::WorkView,
        EventName::WatcherDegraded | EventName::WatcherRecovered => StatusItemKind::Watcher,
        EventName::EnvImported | EventName::EnvMaterialized | EventName::EnvRevoked => {
            StatusItemKind::Env
        }
        EventName::HydrationStarted
        | EventName::HydrationCompleted
        | EventName::HydrationBlocked
        | EventName::HydrationBudgetReserved
        | EventName::HydrationBudgetCommitted
        | EventName::HydrationBudgetReleased
        | EventName::HydrationBudgetDenied
        | EventName::HydrationBudgetOverrideGranted => StatusItemKind::Hydration,
        EventName::SourceStale
        | EventName::NamespaceCreated
        | EventName::NamespaceMoved
        | EventName::NamespaceDeletedOrArchived => StatusItemKind::Source,
        EventName::SetupStarted | EventName::SetupCompleted | EventName::SetupBlocked => {
            StatusItemKind::Setup
        }
        EventName::SyncStarted
        | EventName::SyncCompleted
        | EventName::SyncLimited
        | EventName::SyncDegraded
        | EventName::SyncRecovered => StatusItemKind::Materialization,
        EventName::NetworkOffline | EventName::NetworkRecovered => StatusItemKind::Network,
        EventName::IndexUpdated | EventName::IndexDegraded => StatusItemKind::Index,
        EventName::MetadataCorrupt
        | EventName::DaemonDegraded
        | EventName::DaemonRecovered
        | EventName::RecoveryKeyCreated
        | EventName::RecoveryKeyVerified
        | EventName::RecoveryKeyRotated
        | EventName::RecoveryKeyRevoked
        | EventName::AuthLoginStarted
        | EventName::AuthLoginCompleted
        | EventName::OverlayChanged
        | EventName::PublishRequested => StatusItemKind::Metadata,
    }
}

pub fn command_error_output(
    command: CommandName,
    generated_at: String,
    code: impl Into<String>,
    message: impl Into<String>,
    recoverability: CommandRecoverability,
) -> CommandErrorOutput {
    CommandErrorOutput {
        contract_version: CONTRACT_VERSION,
        command,
        generated_at,
        status: CommandErrorStatus::Failed,
        error: CommandError {
            code: code.into(),
            message: message.into(),
            recoverability,
            remediation: None,
            details: None,
            retry_after_seconds: None,
            correlation_id: None,
        },
        next_actions: Vec::new(),
    }
}

/// Map a fully composed [`StatusCommandOutput`] into the redacted snapshot the
/// daemon publishes to the control plane. Counts, state enums, and timestamps
/// are preserved; every `path` is reduced to a workspace-relative form, and
/// anything that still looks like an absolute filesystem path or an env file is
/// dropped so secrets and local layout never leave the device.
///
/// `syncState`/`watcherState`/`networkState` reflect whatever component states
/// `compose_status` observed; the daemon may overwrite them with its live
/// in-memory states before publishing.
pub fn redacted_status_snapshot(
    output: &StatusCommandOutput,
    device_id: &str,
) -> WorkspaceStatusSnapshot {
    let workspace_root = output.resolved_workspace_root.as_deref();

    let event_watermarks = StatusEventWatermarks {
        last_event_id: output
            .event_watermarks
            .last_event_id
            .as_ref()
            .map(|id| id.as_str().to_string()),
        last_scan_at: output.event_watermarks.last_scan_at.clone(),
        sync_state: output
            .event_watermarks
            .sync_state
            .map(|state| component_state_label(state).to_string()),
        watcher_state: output
            .event_watermarks
            .watcher_state
            .map(|state| component_state_label(state).to_string()),
        network_state: output
            .event_watermarks
            .network_state
            .map(|state| network_state_label(state).to_string()),
    };

    let sync_queue = output
        .sync_queue
        .as_ref()
        .map(|queue| StatusSyncQueueSnapshot {
            queued: queue.queued,
            claimed: queue.claimed,
            waiting_retry: queue.waiting_retry,
            blocked_offline: queue.blocked_offline,
            attention: queue.attention,
            completed: queue.completed,
        });

    let index = output.index.as_ref().map(|index| StatusIndexSnapshot {
        state: index_state_label(index.state).to_string(),
        file_count: index.file_count,
        path_count: index.path_count,
        summary: redact_status_text(&index.summary, workspace_root),
    });

    let workspace_summary = output.workspace_summary.as_ref().map(|summary| {
        let observed = summary.observed.as_ref();
        StatusWorkspaceSummarySnapshot {
            total_projects: summary.total_projects,
            repo_count: observed.map(|observed| observed.repo_count),
            env_file_count: observed.map(|observed| observed.env_file_count),
        }
    });

    let items = output
        .items
        .iter()
        .map(|item| StatusItemSnapshot {
            kind: status_item_kind_label(item.kind),
            summary: redact_status_text(&item.summary, workspace_root),
            path: item
                .path
                .as_deref()
                .and_then(|path| redact_workspace_path(path, workspace_root)),
            event_name: item.event_name.map(event_name_label),
        })
        .collect();

    let limits = output
        .limits
        .iter()
        .map(|limit| StatusLimitSnapshot {
            capability: limit.capability.clone(),
            unavailable_because: redact_status_text(&limit.unavailable_because, workspace_root),
            path: limit
                .path
                .as_deref()
                .and_then(|path| redact_workspace_path(path, workspace_root)),
            still_works: limit
                .still_works
                .iter()
                .map(|text| redact_status_text(text, workspace_root))
                .collect(),
        })
        .collect();

    WorkspaceStatusSnapshot {
        snapshot_id: status_snapshot_id(output.workspace_id.as_str(), &output.generated_at),
        workspace_id: output.workspace_id.as_str().to_string(),
        status_level: status_level_label(output.status.level).to_string(),
        attention_items: output
            .status
            .attention_items
            .iter()
            .map(|text| redact_status_text(text, workspace_root))
            .collect(),
        generated_at: output.generated_at.clone(),
        event_watermarks,
        sync_queue,
        index,
        workspace_summary,
        items,
        limits,
        published_by_device_id: device_id.to_string(),
    }
}

fn status_snapshot_id(workspace_id: &str, generated_at: &str) -> String {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    workspace_id.hash(&mut hasher);
    generated_at.hash(&mut hasher);
    format!("wss_{:016x}", hasher.finish())
}

fn component_state_label(state: ComponentState) -> &'static str {
    match state {
        ComponentState::Ready => "ready",
        ComponentState::Degraded => "degraded",
        ComponentState::Unavailable => "unavailable",
    }
}

fn network_state_label(state: NetworkState) -> &'static str {
    match state {
        NetworkState::Online => "online",
        NetworkState::Degraded => "degraded",
        NetworkState::Offline => "offline",
    }
}

fn index_state_label(state: IndexState) -> &'static str {
    match state {
        IndexState::Ready => "ready",
        IndexState::Stale => "stale",
        IndexState::Rebuilding => "rebuilding",
        IndexState::Degraded => "degraded",
    }
}

fn status_item_kind_label(kind: StatusItemKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| format!("{kind:?}").to_ascii_lowercase())
}

/// Reduce a status path to a safe, workspace-relative form, or drop it entirely
/// when it still looks like an absolute filesystem path or an env file.
fn redact_workspace_path(path: &str, workspace_root: Option<&str>) -> Option<String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return None;
    }
    let relative = strip_workspace_root(trimmed, workspace_root);
    if path_is_absolute_like(&relative) || path_basename_is_env(&relative) {
        return None;
    }
    Some(relative)
}

fn strip_workspace_root(path: &str, workspace_root: Option<&str>) -> String {
    let Some(root) = workspace_root else {
        return path.to_string();
    };
    let root = root.trim_end_matches('/');
    if root.is_empty() {
        return path.to_string();
    }
    match Path::new(path).strip_prefix(Path::new(root)) {
        Ok(rest) => {
            let rest = rest.to_string_lossy();
            if rest.is_empty() {
                ".".to_string()
            } else {
                rest.to_string()
            }
        }
        Err(_) => path.to_string(),
    }
}

fn redact_status_text(text: &str, workspace_root: Option<&str>) -> String {
    if text
        .split_whitespace()
        .map(status_text_token)
        .any(|token| status_token_needs_redaction(token, workspace_root))
    {
        "Sensitive local path redacted.".to_string()
    } else {
        text.to_string()
    }
}

fn status_text_token(token: &str) -> &str {
    token.trim_matches(|ch: char| {
        matches!(
            ch,
            '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | '.' | '!'
        )
    })
}

fn status_token_needs_redaction(token: &str, workspace_root: Option<&str>) -> bool {
    if token.is_empty() {
        return false;
    }
    if !(token.contains('/') || token.contains('\\') || token.contains(".env")) {
        return false;
    }
    redact_workspace_path(token, workspace_root).is_none()
}

fn path_is_absolute_like(path: &str) -> bool {
    path.starts_with('/')
        || path.starts_with('~')
        || path.starts_with('\\')
        || has_windows_drive_prefix(path)
}

fn has_windows_drive_prefix(path: &str) -> bool {
    let mut chars = path.chars();
    matches!(
        (chars.next(), chars.next(), chars.next()),
        (Some(drive), Some(':'), Some('/' | '\\')) if drive.is_ascii_alphabetic()
    )
}

fn path_basename_is_env(path: &str) -> bool {
    let basename = path.rsplit(['/', '\\']).next().unwrap_or(path);
    basename == ".env" || basename.starts_with(".env.")
}

fn component_item(kind: StatusItemKind, summary: &str, event_name: EventName) -> StatusItem {
    let mut item = base_status_item(kind, summary);
    item.subject = Some(StatusSubject {
        kind: StatusSubjectKind::Component,
        id: format!("{kind:?}").to_ascii_lowercase(),
        path: None,
    });
    item.event_name = Some(event_name);
    item
}

fn base_status_item(kind: StatusItemKind, summary: &str) -> StatusItem {
    StatusItem {
        kind,
        summary: summary.to_string(),
        subject: None,
        path: None,
        classification: None,
        mode: None,
        access: Vec::new(),
        event_id: None,
        event_name: None,
        device_id: None,
        lease_id: None,
        project_id: None,
        snapshot_id: None,
        policy_version: None,
        env_record_id: None,
    }
}

fn status_level_label(level: StatusLevel) -> &'static str {
    match level {
        StatusLevel::Healthy => "healthy",
        StatusLevel::Attention => "attention",
        StatusLevel::Limited => "limited",
    }
}

fn event_name_label(name: EventName) -> String {
    serde_json::to_value(name)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| format!("{name:?}"))
}

#[cfg(test)]
mod tests {
    use bowline_core::{
        commands::{AgentLeaseBase, HydrationBudgetState, IndexState},
        events::{EventName, EventSeverity, EventSubject, EventSubjectKind, WorkspaceEvent},
        ids::{DeviceId, EventId, ProjectId, SnapshotId, WorkspaceId},
        status::{LimitedCapability, StatusItemKind, StatusLevel},
    };

    use crate::{
        agents::{AgentLeaseCreateOptions, create_agent_lease},
        metadata::{MetadataStore, SyncOperationRecord},
        status::StatusOptions,
        sync::conflicts::{ConflictFile, ConflictRecord, create_conflict_bundle},
        workspace::TempWorkspace,
    };

    use super::{
        EventsOptions, base_status_item, compose_events, compose_status, initial_watch_frame,
        redact_workspace_path, redacted_status_snapshot, render_status_human,
    };

    #[test]
    fn missing_metadata_returns_non_mutating_attention_status() {
        let temp = TempWorkspace::new("status-missing").expect("temp workspace");
        let db_path = temp.root().join("missing").join("local.sqlite3");

        let output = compose_status(StatusOptions {
            db_path: Some(db_path.clone()),
            requested_path: Some("acme/web".to_string()),
            workspace_scope: false,
            generated_at: "2026-06-23T12:00:00Z".to_string(),
        })
        .expect("status composes");

        assert_eq!(output.status.level, StatusLevel::Attention);
        assert!(!db_path.exists());
        assert_eq!(output.next_actions[0].label, "Initialize ~/Code when ready");
        assert!(output.next_actions[0].command.is_none());
    }

    #[test]
    fn corrupt_metadata_returns_limited_status() {
        let temp = TempWorkspace::new("status-corrupt").expect("temp workspace");
        let db_path = temp.root().join("local.sqlite3");
        std::fs::write(&db_path, b"not sqlite").expect("corrupt db");

        let output = compose_status(StatusOptions {
            db_path: Some(db_path),
            requested_path: None,
            workspace_scope: true,
            generated_at: "2026-06-23T12:00:00Z".to_string(),
        })
        .expect("status composes");

        assert_eq!(output.status.level, StatusLevel::Limited);
        assert_eq!(output.limits[0].capability, "local metadata");
    }

    #[test]
    fn redact_workspace_path_strips_root_and_drops_sensitive_paths() {
        let root = Some("~/Code");
        assert_eq!(
            redact_workspace_path("~/Code/apps/web/src/index.ts", root),
            Some("apps/web/src/index.ts".to_string())
        );
        // Already-relative paths are kept untouched.
        assert_eq!(
            redact_workspace_path("apps/api/main.rs", root),
            Some("apps/api/main.rs".to_string())
        );
        // Absolute paths outside the workspace root are dropped entirely.
        assert_eq!(
            redact_workspace_path("/workspace/user/secret.txt", root),
            None
        );
        assert_eq!(
            redact_workspace_path("~/CodeSecrets/private.txt", root),
            None
        );
        assert_eq!(redact_workspace_path("~/.ssh/id_ed25519", root), None);
        assert_eq!(redact_workspace_path("C:\\Users\\user\\app", root), None);
        // Env files are dropped even when workspace-relative.
        assert_eq!(redact_workspace_path("apps/web/.env.local", root), None);
        assert_eq!(redact_workspace_path("~/Code/api/.env", root), None);
        // Empty / whitespace yields nothing.
        assert_eq!(redact_workspace_path("   ", root), None);
    }

    #[test]
    fn redacted_status_snapshot_maps_states_and_redacts_paths() {
        let temp = TempWorkspace::new("status-redacted").expect("temp workspace");
        let db_path = temp.root().join("local.sqlite3");
        std::fs::write(&db_path, b"not sqlite").expect("corrupt db");

        let mut output = compose_status(StatusOptions {
            db_path: Some(db_path),
            requested_path: None,
            workspace_scope: true,
            generated_at: "2026-06-29T12:00:00Z".to_string(),
        })
        .expect("status composes");
        output.resolved_workspace_root = Some("~/Code".to_string());
        output
            .status
            .attention_items
            .push("1 unresolved conflict needs attention: ~/Code/apps/web/.env.local.".to_string());

        let mut visible_item = base_status_item(StatusItemKind::Source, "edited file");
        visible_item.path = Some("~/Code/apps/web/src/index.ts".to_string());
        let mut secret_item = base_status_item(StatusItemKind::Env, "env file changed");
        secret_item.path = Some("~/Code/apps/web/.env.local".to_string());
        let mut absolute_item = base_status_item(StatusItemKind::Device, "external path");
        absolute_item.summary = "external path: /workspace/user/secret".to_string();
        absolute_item.path = Some("/workspace/user/secret".to_string());
        output.items = vec![visible_item, secret_item, absolute_item];
        output.limits = vec![LimitedCapability {
            capability: "search".to_string(),
            unavailable_because: "index degraded".to_string(),
            still_works: vec!["status".to_string()],
            path: Some("~/Code/apps/api".to_string()),
        }];

        let snapshot = redacted_status_snapshot(&output, "device-daemon");

        assert_eq!(snapshot.status_level, "limited");
        assert_eq!(snapshot.published_by_device_id, "device-daemon");
        assert_eq!(snapshot.generated_at, "2026-06-29T12:00:00Z");
        assert_eq!(
            snapshot.attention_items.last().map(String::as_str),
            Some("Sensitive local path redacted.")
        );
        assert!(snapshot.snapshot_id.starts_with("wss_"));
        // Snapshot id is stable for a given (workspace, generatedAt).
        assert_eq!(
            snapshot.snapshot_id,
            redacted_status_snapshot(&output, "device-daemon").snapshot_id
        );
        assert_eq!(snapshot.items.len(), 3);
        assert_eq!(
            snapshot.items[0].path.as_deref(),
            Some("apps/web/src/index.ts")
        );
        assert_eq!(snapshot.items[0].kind, "source");
        assert!(snapshot.items[1].path.is_none(), "env path must be dropped");
        assert!(
            snapshot.items[2].path.is_none(),
            "absolute path must be dropped"
        );
        assert_eq!(snapshot.items[2].summary, "Sensitive local path redacted.");
        assert_eq!(snapshot.limits.len(), 1);
        assert_eq!(snapshot.limits[0].path.as_deref(), Some("apps/api"));
        assert_eq!(snapshot.limits[0].capability, "search");
    }

    #[test]
    fn zero_byte_metadata_is_observational_attention_without_mutation() {
        let temp = TempWorkspace::new("status-empty-file").expect("temp workspace");
        let db_path = temp.root().join("local.sqlite3");
        std::fs::write(&db_path, []).expect("empty db");

        let output = compose_status(StatusOptions {
            db_path: Some(db_path.clone()),
            requested_path: None,
            workspace_scope: true,
            generated_at: "2026-06-23T12:00:00Z".to_string(),
        })
        .expect("status composes");

        assert_eq!(output.status.level, StatusLevel::Attention);
        assert_eq!(std::fs::metadata(&db_path).expect("metadata").len(), 0);
        assert!(!db_path.with_extension("sqlite3-wal").exists());
    }

    #[test]
    fn empty_accepted_workspace_is_healthy() {
        let temp = TempWorkspace::new("status-empty").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let workspace_id = WorkspaceId::new("ws_code");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        store
            .insert_workspace(&workspace_id, "User Code", "2026-06-23T12:00:00Z")
            .expect("workspace insert");
        store
            .insert_root("root_code", &workspace_id, "~/Code", "2026-06-23T12:00:00Z")
            .expect("root insert");

        let output = compose_status(StatusOptions {
            db_path: Some(db_path),
            requested_path: None,
            workspace_scope: true,
            generated_at: "2026-06-23T12:00:00Z".to_string(),
        })
        .expect("status composes");

        assert_eq!(
            output.status.level,
            StatusLevel::Healthy,
            "{:?}",
            output.status.attention_items
        );
        assert!(render_status_human(&output).contains("Status: healthy"));
    }

    #[test]
    fn observed_workspace_with_ready_sync_is_healthy() {
        let temp = TempWorkspace::new("status-observed-sync-ready").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let workspace_id = WorkspaceId::new("ws_code");
        let project_id = ProjectId::new("proj_web");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        seed_workspace_root(&store, &workspace_id);
        seed_project(&store, &project_id, &workspace_id, "root_code", "apps/web");
        store
            .set_observed_summary(
                &workspace_id,
                &bowline_core::status::ObservedWorkspaceSummary {
                    repo_count: 1,
                    no_remote_repo_count: 1,
                    workspace_sync_path_count: 12,
                    env_file_count: 1,
                    ..Default::default()
                },
                "2026-06-23T12:00:00Z",
            )
            .expect("observed summary");
        store
            .append_event(WorkspaceEvent::new(
                EventId::new("evt_sync_ready"),
                EventName::SyncCompleted,
                "2026-06-23T12:00:01Z",
                EventSeverity::Info,
                "Sync completed.",
                workspace_id.clone(),
            ))
            .expect("sync event append");
        store
            .set_component_state("sync", "ready", "2026-06-23T12:00:01Z")
            .expect("sync component");
        store
            .set_component_state("watcher", "ready", "2026-06-23T12:00:01Z")
            .expect("watcher component");
        store
            .set_component_state("network", "online", "2026-06-23T12:00:01Z")
            .expect("network component");

        let output = compose_status(StatusOptions {
            db_path: Some(db_path),
            requested_path: None,
            workspace_scope: true,
            generated_at: "2026-06-23T12:00:02Z".to_string(),
        })
        .expect("status composes");

        assert_eq!(
            output.status.level,
            StatusLevel::Healthy,
            "{:?}",
            output.status.attention_items
        );
        assert!(output.status.attention_items.is_empty());
        assert!(
            output
                .items
                .iter()
                .any(|item| item.summary.contains("sync is active"))
        );
    }

    #[test]
    fn status_reports_accepted_workspace_root_from_metadata() {
        let temp = TempWorkspace::new("status-root").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let root_path = temp.root().join("CustomCode").display().to_string();
        let workspace_id = WorkspaceId::new("ws_code");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        store
            .insert_workspace(&workspace_id, "User Code", "2026-06-23T12:00:00Z")
            .expect("workspace insert");
        store
            .insert_root(
                "root_custom",
                &workspace_id,
                &root_path,
                "2026-06-23T12:00:00Z",
            )
            .expect("root insert");

        let output = compose_status(StatusOptions {
            db_path: Some(db_path),
            requested_path: None,
            workspace_scope: true,
            generated_at: "2026-06-23T12:00:00Z".to_string(),
        })
        .expect("status composes");

        assert_eq!(
            output.resolved_workspace_root.as_deref(),
            Some(root_path.as_str())
        );
    }

    #[test]
    fn status_reports_durable_index_state_without_project_scan() {
        let temp = TempWorkspace::new("status-index-state").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let workspace_id = WorkspaceId::new("ws_code");
        let project_id = ProjectId::new("proj_web");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        seed_workspace_root(&store, &workspace_id);
        seed_project(&store, &project_id, &workspace_id, "root_code", "apps/web");
        store
            .connection()
            .execute(
                "INSERT INTO index_work
                 (id, workspace_id, project_id, path, kind, source_watermark, indexed_watermark, state, reason, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                rusqlite::params![
                    "ix_proj_web",
                    workspace_id.as_str(),
                    project_id.as_str(),
                    "apps/web",
                    "project",
                    42_i64,
                    42_i64,
                    "ready",
                    Option::<&str>::None,
                    "2026-06-23T12:00:00Z",
                ],
            )
            .expect("index work insert");

        let output = compose_status(StatusOptions {
            db_path: Some(db_path),
            requested_path: Some("apps/web".to_string()),
            workspace_scope: false,
            generated_at: "2026-06-23T12:00:01Z".to_string(),
        })
        .expect("status composes");
        let index = output.index.expect("index status");
        assert_eq!(index.state, IndexState::Ready);
        assert_eq!(index.indexed_at.as_deref(), Some("2026-06-23T12:00:00Z"));
        assert_eq!(index.pending_path_count, Some(0));
    }

    #[test]
    fn stale_index_metadata_promotes_top_level_status_attention() {
        let temp = TempWorkspace::new("status-index-stale").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let workspace_id = WorkspaceId::new("ws_code");
        let project_id = ProjectId::new("proj_web");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        seed_workspace_root(&store, &workspace_id);
        seed_project(&store, &project_id, &workspace_id, "root_code", "apps/web");
        store
            .connection()
            .execute(
                "INSERT INTO index_work
                 (id, workspace_id, project_id, path, kind, source_watermark, indexed_watermark, state, reason, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                rusqlite::params![
                    "ix_proj_web_stale",
                    workspace_id.as_str(),
                    project_id.as_str(),
                    "apps/web",
                    "project",
                    42_i64,
                    1_i64,
                    "ready",
                    Option::<&str>::None,
                    "2026-06-23T12:00:00Z",
                ],
            )
            .expect("index work insert");

        let output = compose_status(StatusOptions {
            db_path: Some(db_path),
            requested_path: Some("apps/web".to_string()),
            workspace_scope: false,
            generated_at: "2026-06-23T12:00:01Z".to_string(),
        })
        .expect("status composes");

        assert_eq!(output.index.expect("index").state, IndexState::Stale);
        assert_eq!(output.status.level, StatusLevel::Attention);
        assert!(
            output
                .items
                .iter()
                .any(|item| item.kind == StatusItemKind::Index)
        );
    }

    #[test]
    fn status_reports_single_active_lease_hydration_budget() {
        let temp = TempWorkspace::new("status-hydration-budget").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let workspace_id = WorkspaceId::new("ws_code");
        let project_id = ProjectId::new("proj_web");
        let root_path = temp.root().display().to_string();
        std::fs::create_dir_all(temp.root().join("apps/web")).expect("project directory");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        store
            .insert_workspace(&workspace_id, "User Code", "2026-06-23T12:00:00Z")
            .expect("workspace insert");
        store
            .insert_root(
                "root_code",
                &workspace_id,
                &root_path,
                "2026-06-23T12:00:00Z",
            )
            .expect("root insert");
        seed_project(&store, &project_id, &workspace_id, "root_code", "apps/web");
        drop(store);

        let lease = create_agent_lease(AgentLeaseCreateOptions {
            db_path: Some(db_path.clone()),
            project_path: "apps/web".to_string(),
            task: "hydrate cold files".to_string(),
            base: AgentLeaseBase::LatestWorkspace,
            hydrate_budget_bytes: 2048,
            work_view: true,
            device_id: DeviceId::new("device_user_mac"),
            generated_at: "2026-06-23T12:00:01Z".to_string(),
        })
        .expect("lease created")
        .lease;

        let output = compose_status(StatusOptions {
            db_path: Some(db_path),
            requested_path: Some("apps/web".to_string()),
            workspace_scope: false,
            generated_at: "2026-06-23T12:00:02Z".to_string(),
        })
        .expect("status composes");
        let budget = output.hydration_budget.expect("hydration budget");
        assert_eq!(budget.state, HydrationBudgetState::Available);
        assert_eq!(budget.limit_bytes, 2048);
        assert_eq!(budget.lease_id.as_ref(), Some(&lease.id));
    }

    #[test]
    fn limited_event_makes_status_limited_and_events_command_lists_it() {
        let temp = TempWorkspace::new("status-events").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let workspace_id = WorkspaceId::new("ws_code");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        store
            .insert_workspace(&workspace_id, "User Code", "2026-06-23T12:00:00Z")
            .expect("workspace insert");
        store
            .insert_root("root_code", &workspace_id, "~/Code", "2026-06-23T12:00:00Z")
            .expect("root insert");
        store
            .append_event(WorkspaceEvent::new(
                EventId::new("evt_status_001"),
                EventName::MetadataCorrupt,
                "2026-06-23T12:00:00Z",
                EventSeverity::Limited,
                "Local metadata needs inspection.",
                workspace_id,
            ))
            .expect("event append");

        let output = compose_status(StatusOptions {
            db_path: Some(db_path.clone()),
            requested_path: None,
            workspace_scope: true,
            generated_at: "2026-06-23T12:00:00Z".to_string(),
        })
        .expect("status composes");
        assert_eq!(output.status.level, StatusLevel::Limited);
        assert_eq!(
            output.event_watermarks.last_event_id.unwrap().as_str(),
            "evt_status_001"
        );

        let events = compose_events(EventsOptions {
            db_path: Some(db_path),
            requested_path: None,
            workspace_scope: true,
            generated_at: "2026-06-23T12:00:00Z".to_string(),
            limit: 10,
        })
        .expect("events compose");
        assert_eq!(events.events.len(), 1);
    }

    #[test]
    fn human_events_render_serialized_event_names() {
        let temp = TempWorkspace::new("events-human-label").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let workspace_id = WorkspaceId::new("ws_code");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        seed_workspace_root(&store, &workspace_id);
        store
            .append_event(WorkspaceEvent::new(
                EventId::new("evt_source_stale"),
                EventName::SourceStale,
                "2026-06-23T12:00:00Z",
                EventSeverity::Attention,
                "Source is stale.",
                workspace_id,
            ))
            .expect("event append");

        let events = compose_events(EventsOptions {
            db_path: Some(db_path),
            requested_path: None,
            workspace_scope: true,
            generated_at: "2026-06-23T12:00:00Z".to_string(),
            limit: 10,
        })
        .expect("events compose");
        let rendered = super::render_events_human(&events);

        assert!(rendered.contains("source.stale"));
        assert!(!rendered.contains(" event Source is stale."));
    }

    #[test]
    fn project_events_are_scoped_unless_workspace_is_requested() {
        let temp = TempWorkspace::new("status-events-scoped").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let workspace_id = WorkspaceId::new("ws_code");
        let project_a = ProjectId::new("proj_a");
        let project_b = ProjectId::new("proj_b");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        seed_workspace_root(&store, &workspace_id);
        seed_project(&store, &project_a, &workspace_id, "root_code", "apps/web");
        seed_project(
            &store,
            &project_b,
            &workspace_id,
            "root_code",
            "apps/backend",
        );
        store
            .append_event(project_event(
                "evt_a",
                &workspace_id,
                &project_a,
                "apps/web/src/index.ts",
                EventSeverity::Attention,
                "Web needs attention.",
            ))
            .expect("event append");
        let mut path_only_event = WorkspaceEvent::new(
            EventId::new("evt_a_path_only"),
            EventName::SourceStale,
            "2026-06-23T12:00:01Z",
            EventSeverity::Attention,
            "Web path-only event.",
            workspace_id.clone(),
        );
        path_only_event.path = Some("apps/web/src/button.ts".to_string());
        store
            .append_event(path_only_event)
            .expect("path-only event append");
        store
            .append_event(project_event(
                "evt_b",
                &workspace_id,
                &project_b,
                "apps/backend/src/main.rs",
                EventSeverity::Attention,
                "Backend needs attention.",
            ))
            .expect("event append");

        let project_events = compose_events(EventsOptions {
            db_path: Some(db_path.clone()),
            requested_path: Some("apps/web/src/index.ts".to_string()),
            workspace_scope: false,
            generated_at: "2026-06-23T12:00:00Z".to_string(),
            limit: 10,
        })
        .expect("events compose");
        assert_eq!(project_events.project_id, Some(project_a));
        assert_eq!(project_events.events.len(), 2);
        assert_eq!(project_events.events[0].id.as_str(), "evt_a_path_only");
        assert_eq!(project_events.events[1].id.as_str(), "evt_a");

        let workspace_events = compose_events(EventsOptions {
            db_path: Some(db_path),
            requested_path: None,
            workspace_scope: true,
            generated_at: "2026-06-23T12:00:00Z".to_string(),
            limit: 10,
        })
        .expect("events compose");
        assert_eq!(workspace_events.events.len(), 3);
    }

    #[test]
    fn project_path_prefixes_are_matched_as_literals() {
        let temp = TempWorkspace::new("status-events-literal-prefix").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let workspace_id = WorkspaceId::new("ws_code");
        let project_web_app = ProjectId::new("proj_web_app");
        let project_webxapp = ProjectId::new("proj_webxapp");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        seed_workspace_root(&store, &workspace_id);
        seed_project(
            &store,
            &project_web_app,
            &workspace_id,
            "root_code",
            "apps/web_app",
        );
        seed_project(
            &store,
            &project_webxapp,
            &workspace_id,
            "root_code",
            "apps/webXapp",
        );

        let mut web_app_event = WorkspaceEvent::new(
            EventId::new("evt_web_app_path"),
            EventName::SourceStale,
            "2026-06-23T12:00:00Z",
            EventSeverity::Attention,
            "Web app path-only event.",
            workspace_id.clone(),
        );
        web_app_event.path = Some("apps/web_app/src/index.ts".to_string());
        store
            .append_event(web_app_event)
            .expect("web app event append");

        let mut webxapp_event = WorkspaceEvent::new(
            EventId::new("evt_webxapp_path"),
            EventName::SourceStale,
            "2026-06-23T12:00:01Z",
            EventSeverity::Attention,
            "Sibling path-only event.",
            workspace_id,
        );
        webxapp_event.path = Some("apps/webXapp/src/index.ts".to_string());
        store
            .append_event(webxapp_event)
            .expect("webXapp event append");

        let project_events = compose_events(EventsOptions {
            db_path: Some(db_path),
            requested_path: Some("apps/web_app/src/main.ts".to_string()),
            workspace_scope: false,
            generated_at: "2026-06-23T12:00:00Z".to_string(),
            limit: 10,
        })
        .expect("events compose");

        assert_eq!(project_events.project_id, Some(project_web_app));
        assert_eq!(project_events.events.len(), 1);
        assert_eq!(project_events.events[0].id.as_str(), "evt_web_app_path");
    }

    #[test]
    fn project_status_summarizes_attention_in_other_projects() {
        let temp = TempWorkspace::new("status-project-summary").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let workspace_id = WorkspaceId::new("ws_code");
        let project_web = ProjectId::new("proj_web");
        let project_backend = ProjectId::new("proj_backend");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        seed_workspace_root(&store, &workspace_id);
        seed_project(&store, &project_web, &workspace_id, "root_code", "apps/web");
        seed_project(
            &store,
            &project_backend,
            &workspace_id,
            "root_code",
            "apps/backend",
        );
        let mut backend_event = WorkspaceEvent::new(
            EventId::new("evt_backend_attention"),
            EventName::SourceStale,
            "2026-06-23T12:00:00Z",
            EventSeverity::Attention,
            "Backend needs attention.",
            workspace_id.clone(),
        );
        backend_event.path = Some("apps/backend/src/main.rs".to_string());
        store
            .append_event(backend_event)
            .expect("backend event append");

        let output = compose_status(StatusOptions {
            db_path: Some(db_path),
            requested_path: Some("apps/web/src/index.ts".to_string()),
            workspace_scope: false,
            generated_at: "2026-06-23T12:00:00Z".to_string(),
        })
        .expect("status composes");

        assert_eq!(output.project_id, Some(project_web));
        assert_eq!(output.status.level, StatusLevel::Attention);
        let summary = output.workspace_summary.expect("workspace summary");
        assert_eq!(summary.projects_needing_attention.len(), 1);
        assert_eq!(
            summary.projects_needing_attention[0].project_id,
            project_backend
        );
        assert_eq!(summary.projects_needing_attention[0].path, "apps/backend");
        assert_eq!(
            summary.projects_needing_attention[0].summary,
            "Backend needs attention."
        );
    }

    #[test]
    fn status_uses_recent_actionable_events_for_attention() {
        let temp = TempWorkspace::new("status-recent-events").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let workspace_id = WorkspaceId::new("ws_code");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        seed_workspace_root(&store, &workspace_id);

        store
            .append_event(WorkspaceEvent::new(
                EventId::new("evt_device_approval"),
                EventName::DeviceApprovalRequested,
                "2026-06-23T11:59:00Z",
                EventSeverity::Attention,
                "Device approval requested.",
                workspace_id.clone(),
            ))
            .expect("device approval event append");
        for index in 0..51 {
            store
                .append_event(WorkspaceEvent::new(
                    EventId::new(format!("evt_info_{index:03}")),
                    EventName::IndexUpdated,
                    format!("2026-06-23T12:{index:02}:00Z"),
                    EventSeverity::Info,
                    "Informational event.",
                    workspace_id.clone(),
                ))
                .expect("event append");
        }

        let output = compose_status(StatusOptions {
            db_path: Some(db_path),
            requested_path: None,
            workspace_scope: true,
            generated_at: "2026-06-23T12:00:00Z".to_string(),
        })
        .expect("status composes");

        assert_eq!(output.status.level, StatusLevel::Attention);
        assert!(
            output
                .status
                .attention_items
                .iter()
                .any(|item| item == "Device approval requested.")
        );
    }

    #[test]
    fn resolved_actionable_events_do_not_keep_status_unhealthy() {
        let temp = TempWorkspace::new("status-resolved-events").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let workspace_id = WorkspaceId::new("ws_code");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        seed_workspace_root(&store, &workspace_id);
        store
            .append_event(WorkspaceEvent::new(
                EventId::new("evt_conflict_created"),
                EventName::ConflictCreated,
                "2026-06-23T12:00:00Z",
                EventSeverity::Attention,
                "Merge conflict detected.",
                workspace_id.clone(),
            ))
            .expect("conflict event append");
        store
            .append_event(WorkspaceEvent::new(
                EventId::new("evt_conflict_resolved"),
                EventName::ConflictResolutionAccepted,
                "2026-06-23T12:01:00Z",
                EventSeverity::Info,
                "Merge conflict resolved.",
                workspace_id,
            ))
            .expect("resolution event append");

        let output = compose_status(StatusOptions {
            db_path: Some(db_path),
            requested_path: None,
            workspace_scope: true,
            generated_at: "2026-06-23T12:00:00Z".to_string(),
        })
        .expect("status composes");

        assert_eq!(output.status.level, StatusLevel::Healthy);
        assert!(
            output
                .status
                .attention_items
                .iter()
                .all(|item| item != "Merge conflict detected.")
        );
    }

    #[test]
    fn rejected_resolution_event_clears_conflict_attention() {
        let temp = TempWorkspace::new("status-rejected-resolution").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let workspace_id = WorkspaceId::new("ws_code");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        seed_workspace_root(&store, &workspace_id);
        let mut created = WorkspaceEvent::new(
            EventId::new("evt_conflict_created"),
            EventName::ConflictCreated,
            "2026-06-23T12:00:00Z",
            EventSeverity::Attention,
            "Merge conflict detected.",
            workspace_id.clone(),
        );
        created.subject = Some(EventSubject {
            kind: EventSubjectKind::Conflict,
            id: "conflict-1".to_string(),
            path: None,
        });
        store.append_event(created).expect("conflict event append");
        let mut rejected = WorkspaceEvent::new(
            EventId::new("evt_conflict_rejected"),
            EventName::ConflictResolutionRejected,
            "2026-06-23T12:01:00Z",
            EventSeverity::Info,
            "Merge conflict resolved by remote version.",
            workspace_id,
        );
        rejected.subject = Some(EventSubject {
            kind: EventSubjectKind::Conflict,
            id: "conflict-1".to_string(),
            path: None,
        });
        store
            .append_event(rejected)
            .expect("resolution event append");

        let output = compose_status(StatusOptions {
            db_path: Some(db_path),
            requested_path: None,
            workspace_scope: true,
            generated_at: "2026-06-23T12:00:00Z".to_string(),
        })
        .expect("status composes");

        assert_eq!(output.status.level, StatusLevel::Healthy);
        assert!(
            output
                .status
                .attention_items
                .iter()
                .all(|item| item != "Merge conflict detected.")
        );
    }

    #[test]
    fn recovered_component_events_do_not_keep_status_unhealthy() {
        let temp = TempWorkspace::new("status-recovered-events").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let workspace_id = WorkspaceId::new("ws_code");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        seed_workspace_root(&store, &workspace_id);
        store
            .append_event(WorkspaceEvent::new(
                EventId::new("evt_network_offline"),
                EventName::NetworkOffline,
                "2026-06-23T12:00:00Z",
                EventSeverity::Limited,
                "Network went offline.",
                workspace_id.clone(),
            ))
            .expect("offline event append");
        store
            .append_event(WorkspaceEvent::new(
                EventId::new("evt_network_recovered"),
                EventName::NetworkRecovered,
                "2026-06-23T12:01:00Z",
                EventSeverity::Info,
                "Network recovered.",
                workspace_id,
            ))
            .expect("recovered event append");

        let output = compose_status(StatusOptions {
            db_path: Some(db_path),
            requested_path: None,
            workspace_scope: true,
            generated_at: "2026-06-23T12:00:00Z".to_string(),
        })
        .expect("status composes");

        assert_eq!(output.status.level, StatusLevel::Healthy);
        assert!(
            output
                .status
                .attention_items
                .iter()
                .all(|item| item != "Network went offline.")
        );
    }

    #[test]
    fn component_degradation_always_reports_limited_capability() {
        let temp = TempWorkspace::new("status-components").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let workspace_id = WorkspaceId::new("ws_code");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        seed_workspace_root(&store, &workspace_id);
        store
            .set_component_state("sync", "degraded", "2026-06-23T12:00:00Z")
            .expect("sync state");
        store
            .set_component_state("watcher", "unavailable", "2026-06-23T12:00:00Z")
            .expect("watcher state");
        store
            .set_component_state("network", "offline", "2026-06-23T12:00:00Z")
            .expect("network state");

        let output = compose_status(StatusOptions {
            db_path: Some(db_path),
            requested_path: None,
            workspace_scope: true,
            generated_at: "2026-06-23T12:00:00Z".to_string(),
        })
        .expect("status composes");

        assert_eq!(output.status.level, StatusLevel::Limited);
        assert!(!output.limits.is_empty());
        assert!(
            output
                .limits
                .iter()
                .all(|limit| !limit.still_works.is_empty())
        );
    }

    #[test]
    fn pending_sync_operations_are_visible_in_status() {
        let temp = TempWorkspace::new("status-sync-operations").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let workspace_id = WorkspaceId::new("ws_code");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        seed_workspace_root(&store, &workspace_id);
        store
            .enqueue_sync_operation(&sync_operation_record(
                "op_queued",
                &workspace_id,
                "queued",
                "queued-key",
            ))
            .expect("queued operation");
        store
            .enqueue_sync_operation(&sync_operation_record(
                "op_retry",
                &workspace_id,
                "waiting_retry",
                "retry-key",
            ))
            .expect("retry operation");

        let output = compose_status(StatusOptions {
            db_path: Some(db_path),
            requested_path: None,
            workspace_scope: true,
            generated_at: "2026-06-23T12:00:00Z".to_string(),
        })
        .expect("status composes");

        assert_eq!(output.status.level, StatusLevel::Limited);
        let sync_queue = output.sync_queue.expect("sync queue is reported");
        assert_eq!(sync_queue.queued, 1);
        assert_eq!(sync_queue.waiting_retry, 1);
        assert!(
            output
                .status
                .attention_items
                .contains(&"Sync queue is waiting for retry.".to_string())
        );
        assert!(output.items.iter().any(|item| {
            item.kind == StatusItemKind::Materialization
                && item.summary.contains("1 queued")
                && item.summary.contains("1 waiting retry")
        }));
    }

    #[test]
    fn offline_sync_operations_report_recovery_wait_in_status() {
        let temp = TempWorkspace::new("status-sync-offline").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let workspace_id = WorkspaceId::new("ws_code");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        seed_workspace_root(&store, &workspace_id);
        store
            .enqueue_sync_operation(&sync_operation_record(
                "op_offline",
                &workspace_id,
                "blocked_offline",
                "offline-key",
            ))
            .expect("offline operation");

        let output = compose_status(StatusOptions {
            db_path: Some(db_path),
            requested_path: None,
            workspace_scope: true,
            generated_at: "2026-06-23T12:00:00Z".to_string(),
        })
        .expect("status composes");

        assert_eq!(output.status.level, StatusLevel::Limited);
        let sync_queue = output.sync_queue.expect("sync queue is reported");
        assert_eq!(sync_queue.blocked_offline, 1);
        assert!(
            output
                .status
                .attention_items
                .contains(&"Sync queue is waiting for offline recovery.".to_string())
        );
        assert!(output.limits.iter().any(|limit| {
            limit.capability == "sync"
                && limit.unavailable_because == "sync queue is waiting for offline recovery"
        }));
    }

    #[test]
    fn attention_sync_operations_report_attention_in_status() {
        let temp = TempWorkspace::new("status-sync-attention").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let workspace_id = WorkspaceId::new("ws_code");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        seed_workspace_root(&store, &workspace_id);
        store
            .enqueue_sync_operation(&sync_operation_record(
                "op_attention",
                &workspace_id,
                "attention",
                "attention-key",
            ))
            .expect("attention operation");

        let output = compose_status(StatusOptions {
            db_path: Some(db_path),
            requested_path: None,
            workspace_scope: true,
            generated_at: "2026-06-23T12:00:00Z".to_string(),
        })
        .expect("status composes");

        assert_eq!(output.status.level, StatusLevel::Attention);
        let sync_queue = output.sync_queue.expect("sync queue is reported");
        assert_eq!(sync_queue.attention, 1);
        assert!(
            output
                .status
                .attention_items
                .contains(&"Sync queue needs attention.".to_string())
        );
        assert!(output.limits.iter().any(|limit| {
            limit.capability == "sync" && limit.unavailable_because == "sync queue needs attention"
        }));
    }

    #[test]
    fn status_scopes_sync_queue_to_recent_daemon_device() {
        let temp = TempWorkspace::new("status-sync-current-device").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let workspace_id = WorkspaceId::new("ws_code");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        seed_workspace_root(&store, &workspace_id);

        let mut stale_other_device = sync_operation_record(
            "op_other_attention",
            &workspace_id,
            "attention",
            "other-key",
        );
        stale_other_device.device_id = Some(DeviceId::new("device_other"));
        store
            .enqueue_sync_operation(&stale_other_device)
            .expect("other device attention operation");

        let mut current_device_completed =
            sync_operation_record("op_current_done", &workspace_id, "completed", "current-key");
        current_device_completed.device_id = Some(DeviceId::new("device_current"));
        store
            .enqueue_sync_operation(&current_device_completed)
            .expect("current device completed operation");

        let mut event = WorkspaceEvent::new(
            EventId::new("evt_current_sync"),
            EventName::SyncCompleted,
            "2026-06-23T12:00:01Z",
            EventSeverity::Info,
            "Current device sync completed.",
            workspace_id.clone(),
        );
        event.device_id = Some(DeviceId::new("device_current"));
        store.append_event(event).expect("sync event append");

        let output = compose_status(StatusOptions {
            db_path: Some(db_path),
            requested_path: None,
            workspace_scope: true,
            generated_at: "2026-06-23T12:00:00Z".to_string(),
        })
        .expect("status composes");

        assert_eq!(output.status.level, StatusLevel::Healthy);
        assert!(
            output
                .status
                .attention_items
                .iter()
                .all(|item| item != "Sync queue needs attention.")
        );
        assert_eq!(output.sync_queue, None);
    }

    #[test]
    fn status_reports_state_root_unresolved_conflict_bundles() {
        let temp = TempWorkspace::new("status-state-root-conflict").expect("temp workspace");
        let state_root = temp.root().join("state");
        let db_path = state_root.join("local.sqlite3");
        let workspace_id = WorkspaceId::new("ws_code");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        seed_workspace_root(&store, &workspace_id);
        create_conflict_bundle(
            &state_root,
            ConflictRecord::same_path("apps/web/src/index.ts"),
            &[ConflictFile {
                relative_path: "apps/web/src/index.ts".to_string(),
                base: Some(b"base\n".to_vec()),
                local: Some(b"local\n".to_vec()),
                remote: Some(b"remote\n".to_vec()),
            }],
        )
        .expect("conflict bundle created");

        let output = compose_status(StatusOptions {
            db_path: Some(db_path),
            requested_path: None,
            workspace_scope: true,
            generated_at: "2026-06-23T12:00:00Z".to_string(),
        })
        .expect("status composes");

        assert_eq!(output.status.level, StatusLevel::Attention);
        assert!(output.status.attention_items.iter().any(|item| {
            item == "1 unresolved conflict needs attention: apps/web/src/index.ts."
        }));
        assert!(output.items.iter().any(|item| {
            item.kind == StatusItemKind::Conflict
                && item.path.as_deref() == Some("apps/web/src/index.ts")
        }));
        assert!(output.limits.iter().any(|limit| {
            limit.capability == "sync" && limit.unavailable_because == "unresolved conflict"
        }));
        assert!(
            output
                .next_actions
                .iter()
                .any(|action| { action.command.as_deref() == Some("bowline resolve ~/Code") })
        );
    }

    #[test]
    fn status_ignores_stale_git_index_conflict_bundles() {
        let temp = TempWorkspace::new("status-git-index-conflict").expect("temp workspace");
        let state_root = temp.root().join("state");
        let db_path = state_root.join("local.sqlite3");
        let workspace_id = WorkspaceId::new("ws_code");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        seed_workspace_root(&store, &workspace_id);
        create_conflict_bundle(
            &state_root,
            ConflictRecord::opaque_git("apps/web/.git/index"),
            &[ConflictFile {
                relative_path: "apps/web/.git/index".to_string(),
                base: Some(b"base-index".to_vec()),
                local: Some(b"local-index".to_vec()),
                remote: Some(b"remote-index".to_vec()),
            }],
        )
        .expect("conflict bundle created");

        let output = compose_status(StatusOptions {
            db_path: Some(db_path),
            requested_path: None,
            workspace_scope: true,
            generated_at: "2026-06-23T12:00:00Z".to_string(),
        })
        .expect("status composes");

        assert_eq!(output.status.level, StatusLevel::Healthy);
        assert!(
            output
                .next_actions
                .iter()
                .all(|action| action.command.as_deref() != Some("bowline resolve ~/Code"))
        );
    }

    #[test]
    fn stale_conflict_event_without_unresolved_bundle_does_not_keep_project_attention() {
        let temp = TempWorkspace::new("status-stale-conflict-event").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let workspace_id = WorkspaceId::new("ws_code");
        let backend_id = ProjectId::new("proj_backend");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        seed_workspace_root(&store, &workspace_id);
        seed_project(
            &store,
            &backend_id,
            &workspace_id,
            "root_code",
            "apps/backend",
        );
        let mut conflict = WorkspaceEvent::new(
            EventId::new("evt_backend_conflict"),
            EventName::ConflictCreated,
            "2026-06-23T12:00:00Z",
            EventSeverity::Attention,
            "Continuous sync detected a conflict in 1 path(s).",
            workspace_id.clone(),
        );
        conflict.project_id = Some(backend_id);
        conflict.path = Some("apps/backend/src/index.ts".to_string());
        conflict.subject = Some(EventSubject {
            kind: EventSubjectKind::Conflict,
            id: "conflict_backend".to_string(),
            path: Some("apps/backend/src/index.ts".to_string()),
        });
        store.append_event(conflict).expect("conflict event append");

        let output = compose_status(StatusOptions {
            db_path: Some(db_path),
            requested_path: None,
            workspace_scope: true,
            generated_at: "2026-06-23T12:00:00Z".to_string(),
        })
        .expect("status composes");

        assert_eq!(output.status.level, StatusLevel::Healthy);
        assert!(
            output
                .workspace_summary
                .expect("summary")
                .projects_needing_attention
                .is_empty()
        );
    }

    #[test]
    fn event_subjects_map_to_status_domains() {
        let temp = TempWorkspace::new("status-event-domains").expect("temp workspace");
        let db_path = temp.root().join("state").join("local.sqlite3");
        let state_root = temp.root().join("state");
        let workspace_id = WorkspaceId::new("ws_code");
        let store = MetadataStore::open(&db_path).expect("metadata opens");
        seed_workspace_root(&store, &workspace_id);
        create_conflict_bundle(
            &state_root,
            ConflictRecord::same_path("apps/web/src/index.ts"),
            &[ConflictFile {
                relative_path: "apps/web/src/index.ts".to_string(),
                base: Some(b"base\n".to_vec()),
                local: Some(b"local\n".to_vec()),
                remote: Some(b"remote\n".to_vec()),
            }],
        )
        .expect("conflict bundle created");
        let mut event = WorkspaceEvent::new(
            EventId::new("evt_conflict"),
            EventName::ConflictCreated,
            "2026-06-23T12:00:00Z",
            EventSeverity::Attention,
            "Merge conflict detected.",
            workspace_id,
        );
        event.subject = Some(EventSubject {
            kind: EventSubjectKind::Conflict,
            id: "conflict-1".to_string(),
            path: Some("apps/web/src/index.ts".to_string()),
        });
        store.append_event(event).expect("event append");

        let output = compose_status(StatusOptions {
            db_path: Some(db_path),
            requested_path: None,
            workspace_scope: true,
            generated_at: "2026-06-23T12:00:00Z".to_string(),
        })
        .expect("status composes");

        let conflict_item = output
            .items
            .iter()
            .find(|item| {
                item.event_id
                    .as_ref()
                    .is_some_and(|id| id.as_str() == "evt_conflict")
            })
            .expect("conflict status item");
        assert_eq!(conflict_item.kind, StatusItemKind::Conflict);
    }

    #[test]
    fn corrupt_metadata_events_return_error_instead_of_empty_history() {
        let temp = TempWorkspace::new("events-corrupt").expect("temp workspace");
        let db_path = temp.root().join("local.sqlite3");
        std::fs::write(&db_path, b"not sqlite").expect("corrupt db");

        let error = compose_events(EventsOptions {
            db_path: Some(db_path),
            requested_path: None,
            workspace_scope: true,
            generated_at: "2026-06-23T12:00:00Z".to_string(),
            limit: 10,
        })
        .expect_err("corrupt events fail");

        assert!(matches!(error, super::LocalStatusError::MetadataState(_)));
    }

    #[test]
    fn watch_frame_starts_with_current_status() {
        let status = super::missing_metadata_status(&StatusOptions {
            db_path: None,
            requested_path: None,
            workspace_scope: true,
            generated_at: "2026-06-23T12:00:00Z".to_string(),
        });

        match initial_watch_frame(status) {
            bowline_core::commands::WatchFrame::Status { sequence, .. } => {
                assert_eq!(sequence, 1)
            }
            _ => panic!("expected status frame"),
        }
    }

    fn seed_workspace_root(store: &MetadataStore, workspace_id: &WorkspaceId) {
        store
            .insert_workspace(workspace_id, "User Code", "2026-06-23T12:00:00Z")
            .expect("workspace insert");
        store
            .insert_root("root_code", workspace_id, "~/Code", "2026-06-23T12:00:00Z")
            .expect("root insert");
    }

    fn sync_operation_record(
        id: &str,
        workspace_id: &WorkspaceId,
        state: &str,
        idempotency_key: &str,
    ) -> SyncOperationRecord {
        SyncOperationRecord {
            id: id.to_string(),
            workspace_id: workspace_id.clone(),
            kind: "daemon_tick".to_string(),
            state: state.to_string(),
            idempotency_key: idempotency_key.to_string(),
            base_version: Some(1),
            base_snapshot_id: Some("snap_base".to_string()),
            target_snapshot_id: Some("snap_target".to_string()),
            device_id: Some(DeviceId::new("device-test")),
            payload_json: "{}".to_string(),
            attempt_count: 0,
            claimed_by: None,
            heartbeat_at: None,
            next_attempt_at: None,
            last_error: None,
            created_at: "2026-06-23T12:00:00Z".to_string(),
            updated_at: "2026-06-23T12:00:00Z".to_string(),
        }
    }

    fn seed_project(
        store: &MetadataStore,
        project_id: &ProjectId,
        workspace_id: &WorkspaceId,
        root_id: &str,
        path: &str,
    ) {
        store
            .insert_project(
                project_id,
                workspace_id,
                root_id,
                path,
                "2026-06-23T12:00:00Z",
            )
            .expect("project insert");
        store
            .set_project_latest_snapshot_id(
                workspace_id,
                project_id,
                &SnapshotId::new(format!("snap_{}", project_id.as_str())),
            )
            .expect("project latest snapshot");
    }

    fn project_event(
        id: &str,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
        path: &str,
        severity: EventSeverity,
        summary: &str,
    ) -> WorkspaceEvent {
        let mut event = WorkspaceEvent::new(
            EventId::new(id),
            EventName::SourceStale,
            "2026-06-23T12:00:00Z",
            severity,
            summary,
            workspace_id.clone(),
        );
        event.project_id = Some(project_id.clone());
        event.path = Some(path.to_string());
        event
    }
}
