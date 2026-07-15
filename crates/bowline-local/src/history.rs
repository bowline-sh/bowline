use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    error::Error,
    fmt, io,
    path::PathBuf,
};

use bowline_core::{
    commands::{
        CONTRACT_VERSION, CommandName, HistoryActor, HistoryActorKind, HistoryCause,
        HistoryChangeSummary, HistoryCommandOutput, HistoryEndpoint, HistoryScope,
        HistoryScopeKind, PathHistoryEntry, PathHistoryOperation, RestorePoint,
    },
    events::WorkspaceEvent,
    ids::{EventId, ProjectId, SnapshotId, WorkspaceId},
    namespace_snapshot::NamespaceReadError,
    policy::PathClassification,
    status::RepairCommand,
    workspace_graph::normalize_workspace_path,
};

use crate::{
    events::EventQuery,
    metadata::{
        DatabaseState, LocalWriteLogRecord, MetadataError, MetadataStore, ProjectRecord,
        WorkspaceRecord, default_database_path,
    },
};

#[path = "history_namespace.rs"]
mod history_namespace;

pub const DEFAULT_HISTORY_LIMIT: u32 = 50;
pub const MAX_HISTORY_LIMIT: u32 = 500;

#[derive(Debug, Clone)]
pub struct HistoryOptions {
    pub db_path: Option<PathBuf>,
    pub target_path: String,
    pub mode: HistoryMode,
    pub generated_at: String,
    pub limit: u32,
    pub cursor: Option<usize>,
    pub since: Option<String>,
    pub until: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryMode {
    Timeline,
    Path,
    Diff { from: String, to: String },
}

#[derive(Debug)]
pub enum HistoryError {
    Io(io::Error),
    Metadata(MetadataError),
    CachedSnapshot(crate::sync::CachedSnapshotError),
    NamespaceRead(NamespaceReadError),
    MetadataState(DatabaseState),
    MissingWorkspace,
    MissingWorkspaceRoot,
    MissingProject { path: String },
    UnknownSnapshot { selector: String },
}

pub fn compose_history(options: HistoryOptions) -> Result<HistoryCommandOutput, HistoryError> {
    let context = open_history_context(&options)?;
    let material = collect_history_material(&context, &options)?;
    let cursor = options.cursor.unwrap_or(0);
    let limit = bounded_limit(options.limit);
    let selection = select_history_output(
        &options.mode,
        &material,
        context.path_filter.as_deref(),
        cursor,
        limit,
    )?;

    Ok(HistoryCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::History,
        generated_at: options.generated_at,
        workspace_id: context.workspace.id.clone(),
        project_id: context.project.id.clone(),
        scope: HistoryScope {
            kind: if context.path_filter.is_some() {
                HistoryScopeKind::Path
            } else {
                HistoryScopeKind::Project
            },
            root: context.root,
            project_path: context.project.path,
            project_id: context.project.id,
            path: context.path_filter,
        },
        restore_points: selection.restore_points,
        path_entries: selection.path_entries,
        from: selection.from,
        to: selection.to,
        diff_summary: selection.diff_summary,
        next_cursor: selection.page.cursor,
        truncated: selection.page.has_more,
        next_actions: vec![RepairCommand::mutating(
            "Create a work view from a restore point".to_string(),
            Some("bowline work create <project> <name> --from <restore-point>".to_string()),
        )],
    })
}

struct HistoryContext {
    store: MetadataStore,
    workspace: WorkspaceRecord,
    root: String,
    project: ProjectRecord,
    path_filter: Option<String>,
}

struct HistoryMaterial {
    writes: Vec<LocalWriteLogRecord>,
    events_by_cause: BTreeMap<String, Vec<WorkspaceEvent>>,
    point_set: RestorePointSet,
}

struct HistorySelection {
    restore_points: Vec<RestorePoint>,
    path_entries: Vec<PathHistoryEntry>,
    from: Option<HistoryEndpoint>,
    to: Option<HistoryEndpoint>,
    diff_summary: Option<HistoryChangeSummary>,
    page: PageMeta,
}

struct PageMeta {
    has_more: bool,
    cursor: Option<String>,
}

fn open_history_context(options: &HistoryOptions) -> Result<HistoryContext, HistoryError> {
    let db_path = options
        .db_path
        .clone()
        .map(Ok)
        .unwrap_or_else(default_database_path)?;
    let inspection = MetadataStore::inspect(&db_path);
    match inspection.state {
        DatabaseState::Missing | DatabaseState::Empty => {
            return Err(HistoryError::MissingWorkspace);
        }
        DatabaseState::Corrupt
        | DatabaseState::FutureIncompatible { .. }
        | DatabaseState::UnsupportedSchema
        | DatabaseState::Locked
        | DatabaseState::PermissionDenied => {
            return Err(HistoryError::MetadataState(inspection.state));
        }
        DatabaseState::Current => {}
    }

    let store = MetadataStore::open(&db_path)?;
    let workspace = store
        .workspace_by_path(&options.target_path)?
        .or_else(|| store.current_workspace().ok().flatten())
        .ok_or(HistoryError::MissingWorkspace)?;
    let root = store
        .workspace_root(&workspace.id)?
        .ok_or(HistoryError::MissingWorkspaceRoot)?;
    let project = store
        .project_by_path(&workspace.id, &options.target_path)?
        .ok_or_else(|| HistoryError::MissingProject {
            path: options.target_path.clone(),
        })?;
    let path_filter = matches!(options.mode, HistoryMode::Path).then(|| {
        store
            .workspace_relative_path(&workspace.id, &options.target_path)
            .unwrap_or_else(|_| options.target_path.clone())
    });
    let path_filter = path_filter
        .as_deref()
        .map(normalize_workspace_path)
        .filter(|path| path != &project.path);

    Ok(HistoryContext {
        store,
        workspace,
        root,
        project,
        path_filter,
    })
}

fn collect_history_material(
    context: &HistoryContext,
    options: &HistoryOptions,
) -> Result<HistoryMaterial, HistoryError> {
    let events = context.store.list_events_scoped(EventQuery {
        workspace_id: Some(context.workspace.id.clone()),
        project_id: Some(context.project.id.clone()),
        path_prefix: None,
        limit: MAX_HISTORY_LIMIT,
    })?;
    let events_by_cause = events_by_causation(&events);
    let writes = project_writes(
        &context.store.local_writes_for_project(
            &context.workspace.id,
            &context.project.id,
            &context.project.path,
            options.since.as_deref(),
            Some(MAX_HISTORY_LIMIT as u64),
        )?,
        &context.project.id,
        &context.project.path,
    );
    let point_set = restore_points(
        &context.store,
        &context.workspace.id,
        &context.project.id,
        &context.project.path,
        &events_by_cause,
        options,
    )?;

    Ok(HistoryMaterial {
        writes,
        events_by_cause,
        point_set,
    })
}

fn select_history_output(
    mode: &HistoryMode,
    material: &HistoryMaterial,
    path_filter: Option<&str>,
    cursor: usize,
    limit: u32,
) -> Result<HistorySelection, HistoryError> {
    match mode {
        HistoryMode::Timeline => Ok(select_timeline_history(
            &material.point_set.points,
            cursor,
            limit,
        )),
        HistoryMode::Path => Ok(select_path_history(material, path_filter, cursor, limit)),
        HistoryMode::Diff { from, to } => select_diff_history(material, from, to, cursor, limit),
    }
}

fn select_timeline_history(points: &[RestorePoint], cursor: usize, limit: u32) -> HistorySelection {
    let restore_page = page(points.to_vec(), cursor, limit);
    let page = restore_page.meta();
    HistorySelection {
        restore_points: restore_page.items,
        path_entries: Vec::new(),
        from: None,
        to: None,
        diff_summary: None,
        page,
    }
}

fn select_path_history(
    material: &HistoryMaterial,
    path_filter: Option<&str>,
    cursor: usize,
    limit: u32,
) -> HistorySelection {
    let entries = path_history_entries(
        &material.writes,
        &material.point_set.points,
        &material.point_set.point_id_by_cause,
        &material.events_by_cause,
        path_filter,
    );
    let path_page = page(entries, cursor, limit);
    let restore_points = restore_points_for_entries(&material.point_set.points, &path_page.items);
    let page = path_page.meta();
    HistorySelection {
        restore_points,
        path_entries: path_page.items,
        from: None,
        to: None,
        diff_summary: None,
        page,
    }
}

fn select_diff_history(
    material: &HistoryMaterial,
    from: &str,
    to: &str,
    cursor: usize,
    limit: u32,
) -> Result<HistorySelection, HistoryError> {
    let restore_page = page(material.point_set.points.clone(), cursor, limit);
    let page = restore_page.meta();
    let from = resolve_endpoint(from, &material.point_set.points)?;
    let to = resolve_endpoint(to, &material.point_set.points)?;
    let summary = diff_summary_between(
        &material.writes,
        &material.point_set.points,
        &material.point_set.point_id_by_cause,
        &from,
        &to,
    );
    Ok(HistorySelection {
        restore_points: restore_page.items,
        path_entries: Vec::new(),
        from: Some(from),
        to: Some(to),
        diff_summary: Some(summary),
        page,
    })
}

pub fn render_history_human(output: &HistoryCommandOutput) -> String {
    if output.restore_points.is_empty() {
        return "No workspace history recorded.\n".to_string();
    }
    let mut lines = vec![format!("History  {}", output.scope.project_path)];
    for point in &output.restore_points {
        lines.push(format!(
            "  {}  {}  {}",
            point.occurred_at, point.id, point.label
        ));
    }
    if !output.path_entries.is_empty() {
        lines.push("Path changes".to_string());
        for entry in &output.path_entries {
            lines.push(format!(
                "  {}  {:?}  {}",
                entry.occurred_at, entry.operation, entry.restore_point_id
            ));
        }
    }
    lines.push(String::new());
    lines.join("\n")
}

impl fmt::Display for HistoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "history I/O failed: {error}"),
            Self::Metadata(error) => error.fmt(formatter),
            Self::CachedSnapshot(error) => error.fmt(formatter),
            Self::NamespaceRead(error) => error.fmt(formatter),
            Self::MetadataState(state) => write!(formatter, "metadata unavailable: {state:?}"),
            Self::MissingWorkspace => write!(formatter, "no bowline workspace is initialized"),
            Self::MissingWorkspaceRoot => write!(formatter, "workspace root is missing"),
            Self::MissingProject { path } => {
                write!(formatter, "no tracked project was found for `{path}`")
            }
            Self::UnknownSnapshot { selector } => {
                write!(formatter, "history snapshot `{selector}` was not found")
            }
        }
    }
}

impl Error for HistoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Metadata(error) => Some(error),
            Self::CachedSnapshot(error) => Some(error),
            Self::NamespaceRead(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for HistoryError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<MetadataError> for HistoryError {
    fn from(error: MetadataError) -> Self {
        Self::Metadata(error)
    }
}

impl From<crate::sync::CachedSnapshotError> for HistoryError {
    fn from(error: crate::sync::CachedSnapshotError) -> Self {
        Self::CachedSnapshot(error)
    }
}

impl From<NamespaceReadError> for HistoryError {
    fn from(error: NamespaceReadError) -> Self {
        Self::NamespaceRead(error)
    }
}

impl From<crate::events::LocalEventError> for HistoryError {
    fn from(error: crate::events::LocalEventError) -> Self {
        match error {
            crate::events::LocalEventError::Metadata(error) => Self::Metadata(error),
            other => Self::Metadata(MetadataError::InvalidStorageMetadata(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalWriteOperation {
    Create,
    Modify,
    Delete,
    Rename,
    Policy,
    Unknown,
}

impl LocalWriteOperation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Modify => "modify",
            Self::Delete => "delete",
            Self::Rename => "rename",
            Self::Policy => "policy",
            Self::Unknown => "unknown",
        }
    }

    fn from_str(value: &str) -> Self {
        match value {
            // Local write producers still live in agents/ and work_views/, so
            // history accepts both observed spellings until those writers own
            // this SQL enum too.
            "create" | "created" => Self::Create,
            "modify" | "modified" | "write" | "written" => Self::Modify,
            "delete" | "deleted" => Self::Delete,
            "rename" | "renamed" => Self::Rename,
            "policy" | "policy-change" => Self::Policy,
            _ => Self::Unknown,
        }
    }
}

fn restore_points(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    project_path: &str,
    events_by_cause: &BTreeMap<String, Vec<WorkspaceEvent>>,
    options: &HistoryOptions,
) -> Result<RestorePointSet, HistoryError> {
    let project_prefix = normalize_workspace_path(project_path);
    let mut point_id_by_cause = BTreeMap::new();
    let mut points = Vec::new();
    let mut before_updated_at: Option<String> = None;
    let mut before_id: Option<String> = None;
    while points.len() < MAX_HISTORY_LIMIT as usize {
        let operations = store.completed_sync_operations_page(
            workspace_id,
            options.since.as_deref(),
            options.until.as_deref(),
            before_updated_at.as_deref(),
            before_id.as_deref(),
            Some(MAX_HISTORY_LIMIT as u64),
        )?;
        if operations.is_empty() {
            break;
        }
        let operation_count = operations.len();
        let operation_ids = operations
            .iter()
            .map(|operation| operation.id.clone())
            .collect::<Vec<_>>();
        let page_writes = project_writes(
            &store.local_writes_for_causation_ids(workspace_id, &operation_ids)?,
            project_id,
            project_path,
        );
        let writes_by_cause = writes_by_causation(&page_writes);
        let snapshot_ids = operations
            .iter()
            .filter_map(|operation| operation.target_snapshot_id.as_ref())
            .map(|snapshot_id| SnapshotId::new(snapshot_id.clone()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let snapshot_project_ids = store.snapshot_project_ids(workspace_id, &snapshot_ids)?;
        for operation in operations {
            before_updated_at = Some(operation.updated_at.clone());
            before_id = Some(operation.id.clone());
            debug_assert_eq!(
                operation.state,
                crate::metadata::SyncOperationState::Completed
            );
            let Some(snapshot_id) = operation.target_snapshot_id.clone() else {
                continue;
            };
            let occurred_at = operation.updated_at.clone();
            let matching_writes = writes_by_cause
                .get(&operation.id)
                .cloned()
                .unwrap_or_default();
            let event_ids = events_by_cause
                .get(&operation.id)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|event| event.id)
                .collect::<Vec<_>>();
            let scoped_snapshot = snapshot_scoped_to_project(
                store,
                workspace_id,
                &SnapshotId::new(snapshot_id.clone()),
                snapshot_project_ids.get(&SnapshotId::new(snapshot_id.clone())),
                project_id,
                &project_prefix,
            )?;
            if matching_writes.is_empty() && event_ids.is_empty() && !scoped_snapshot {
                continue;
            }
            let point_id = restore_point_id(&snapshot_id);
            point_id_by_cause.insert(operation.id.clone(), point_id.clone());
            points.push(RestorePoint {
                id: point_id,
                snapshot_id: SnapshotId::new(snapshot_id),
                base_snapshot_id: operation.base_snapshot_id.map(SnapshotId::new),
                occurred_at,
                label: history_label(operation.kind.as_str()),
                cause: history_cause(operation.kind.as_str()),
                actor: operation.device_id.map(|device_id| HistoryActor {
                    kind: HistoryActorKind::Daemon,
                    display_name: None,
                    device_id: Some(device_id),
                }),
                summary: summarize_writes(&matching_writes),
                event_ids,
            });
            if points.len() >= MAX_HISTORY_LIMIT as usize {
                break;
            }
        }
        if operation_count < MAX_HISTORY_LIMIT as usize {
            break;
        }
    }
    points.sort_by(|left, right| {
        right
            .occurred_at
            .cmp(&left.occurred_at)
            .then(left.id.cmp(&right.id))
    });
    Ok(RestorePointSet {
        points,
        point_id_by_cause,
    })
}

fn snapshot_scoped_to_project(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    snapshot_id: &SnapshotId,
    snapshot_project_id: Option<&Option<ProjectId>>,
    project_id: &ProjectId,
    project_prefix: &str,
) -> Result<bool, HistoryError> {
    let Some(snapshot_project_id) = snapshot_project_id else {
        return Ok(false);
    };
    if snapshot_project_id.as_ref() == Some(project_id) {
        return Ok(true);
    }
    if snapshot_project_id.is_some() {
        return Ok(false);
    }
    history_namespace::snapshot_contains_prefix(store, workspace_id, snapshot_id, project_prefix)
}

struct RestorePointSet {
    points: Vec<RestorePoint>,
    point_id_by_cause: BTreeMap<String, String>,
}

struct Page<T> {
    items: Vec<T>,
    has_more: bool,
    cursor: Option<String>,
}

impl<T> Page<T> {
    fn meta(&self) -> PageMeta {
        PageMeta {
            has_more: self.has_more,
            cursor: self.cursor.clone(),
        }
    }
}

fn page<T>(items: Vec<T>, cursor: usize, limit: u32) -> Page<T> {
    let limit = limit as usize;
    let total = items.len();
    let start = cursor.min(total);
    let end = (start + limit).min(total);
    let has_more = end < total;
    let cursor = has_more.then(|| end.to_string());
    Page {
        items: items.into_iter().skip(start).take(limit).collect(),
        has_more,
        cursor,
    }
}

fn bounded_limit(limit: u32) -> u32 {
    limit.clamp(1, MAX_HISTORY_LIMIT)
}

fn project_writes(
    writes: &[LocalWriteLogRecord],
    project_id: &ProjectId,
    project_path: &str,
) -> Vec<LocalWriteLogRecord> {
    let project_prefix = normalize_workspace_path(project_path);
    writes
        .iter()
        .filter(|write| {
            write.project_id.as_ref() == Some(project_id)
                || normalize_workspace_path(&write.path) == project_prefix
                || normalize_workspace_path(&write.path).starts_with(&format!("{project_prefix}/"))
        })
        .cloned()
        .collect()
}

fn events_by_causation(events: &[WorkspaceEvent]) -> BTreeMap<String, Vec<WorkspaceEvent>> {
    let mut by_cause = BTreeMap::<String, Vec<WorkspaceEvent>>::new();
    for event in events {
        if let Some(cause) = &event.causation_id {
            by_cause
                .entry(cause.as_str().to_string())
                .or_default()
                .push(event.clone());
        }
    }
    by_cause
}

fn writes_by_causation(
    writes: &[LocalWriteLogRecord],
) -> BTreeMap<String, Vec<LocalWriteLogRecord>> {
    let mut by_cause = BTreeMap::<String, Vec<LocalWriteLogRecord>>::new();
    for write in writes {
        by_cause
            .entry(write.causation_id.clone())
            .or_default()
            .push(write.clone());
    }
    by_cause
}

fn summarize_writes(writes: &[LocalWriteLogRecord]) -> HistoryChangeSummary {
    let mut summary = HistoryChangeSummary {
        files_changed: writes.len() as u32,
        files_added: 0,
        files_modified: 0,
        files_deleted: 0,
        files_renamed: 0,
        binary_or_large_files_changed: 0,
        env_keys_changed: 0,
        paths_sample: Vec::new(),
    };
    for write in writes {
        let operation = LocalWriteOperation::from_str(&write.operation);
        debug_assert!(!operation.as_str().is_empty());
        match operation {
            LocalWriteOperation::Create => summary.files_added += 1,
            LocalWriteOperation::Delete => summary.files_deleted += 1,
            LocalWriteOperation::Rename => summary.files_renamed += 1,
            _ => summary.files_modified += 1,
        }
        if write.policy_classification == PathClassification::LargeFile {
            summary.binary_or_large_files_changed += 1;
        }
        if write
            .path
            .split('/')
            .next_back()
            .is_some_and(|name| name.starts_with(".env"))
        {
            summary.env_keys_changed += 1;
        }
        if summary.paths_sample.len() < 10 {
            summary.paths_sample.push(write.path.clone());
        }
    }
    summary
}

fn path_history_entries(
    writes: &[LocalWriteLogRecord],
    points: &[RestorePoint],
    point_id_by_cause: &BTreeMap<String, String>,
    events_by_cause: &BTreeMap<String, Vec<WorkspaceEvent>>,
    path_filter: Option<&str>,
) -> Vec<PathHistoryEntry> {
    let point_by_id = points
        .iter()
        .map(|point| (point.id.clone(), point))
        .collect::<BTreeMap<_, _>>();
    let event_id_sets = points
        .iter()
        .map(|point| {
            (
                point.id.clone(),
                point.event_ids.iter().cloned().collect::<HashSet<_>>(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    writes
        .iter()
        .filter(|write| {
            path_filter.is_none_or(|filter| {
                let path = normalize_workspace_path(&write.path);
                path == filter || path.starts_with(&format!("{filter}/"))
            })
        })
        .rev()
        .filter_map(|write| {
            let point = point_id_by_cause
                .get(&write.causation_id)
                .and_then(|point_id| point_by_id.get(point_id).copied())
                .or_else(|| {
                    point_by_id
                        .values()
                        .find(|point| {
                            let Some(point_event_ids) = event_id_sets.get(&point.id) else {
                                return false;
                            };
                            events_by_cause
                                .get(&write.causation_id)
                                .is_some_and(|events| {
                                    events
                                        .iter()
                                        .any(|event| point_event_ids.contains(&event.id))
                                })
                        })
                        .copied()
                })?;
            Some(PathHistoryEntry {
                restore_point_id: point.id.clone(),
                snapshot_id: point.snapshot_id.clone(),
                occurred_at: write.created_at.clone(),
                operation: path_operation(&write.operation),
                source_path: write.source_path.clone(),
                actor: point.actor.clone(),
                causation_id: Some(EventId::new(write.causation_id.clone())),
                event_ids: events_by_cause
                    .get(&write.causation_id)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|event| event.id)
                    .collect(),
            })
        })
        .collect()
}

fn restore_points_for_entries(
    points: &[RestorePoint],
    entries: &[PathHistoryEntry],
) -> Vec<RestorePoint> {
    let point_by_id = points
        .iter()
        .map(|point| (point.id.clone(), point))
        .collect::<BTreeMap<_, _>>();
    let mut selected = Vec::new();
    for entry in entries {
        if selected
            .iter()
            .any(|point: &RestorePoint| point.id == entry.restore_point_id)
        {
            continue;
        }
        if let Some(point) = point_by_id.get(&entry.restore_point_id) {
            selected.push((*point).clone());
        }
    }
    selected
}

fn resolve_endpoint(
    selector: &str,
    points: &[RestorePoint],
) -> Result<HistoryEndpoint, HistoryError> {
    let snapshot = snapshot_selector(selector);
    points
        .iter()
        .find(|point| point.id == selector || point.snapshot_id.as_str() == snapshot)
        .map(|point| HistoryEndpoint {
            restore_point_id: Some(point.id.clone()),
            snapshot_id: point.snapshot_id.clone(),
        })
        .ok_or_else(|| HistoryError::UnknownSnapshot {
            selector: selector.to_string(),
        })
}

fn diff_summary_between(
    writes: &[LocalWriteLogRecord],
    points: &[RestorePoint],
    point_id_by_cause: &BTreeMap<String, String>,
    from: &HistoryEndpoint,
    to: &HistoryEndpoint,
) -> HistoryChangeSummary {
    let point_index = |snapshot: &SnapshotId| {
        points
            .iter()
            .position(|point| &point.snapshot_id == snapshot)
            .unwrap_or(points.len())
    };
    let from_index = point_index(&from.snapshot_id);
    let to_index = point_index(&to.snapshot_id);
    let included_point_ids = points
        .iter()
        .enumerate()
        .filter(|(index, _)| {
            if from_index > to_index {
                *index >= to_index && *index < from_index
            } else {
                *index > from_index && *index <= to_index
            }
        })
        .map(|(_, point)| point.id.as_str())
        .collect::<Vec<_>>();
    let scoped = writes
        .iter()
        .filter(|write| {
            point_id_by_cause
                .get(&write.causation_id)
                .is_some_and(|point_id| included_point_ids.contains(&point_id.as_str()))
        })
        .cloned()
        .collect::<Vec<_>>();
    summarize_writes(&scoped)
}

fn restore_point_id(snapshot_id: &str) -> String {
    format!("rp_{snapshot_id}")
}

fn snapshot_selector(selector: &str) -> &str {
    selector.strip_prefix("rp_").unwrap_or(selector)
}

fn history_label(kind: &str) -> String {
    match kind {
        "daemon-reconcile" => "Workspace sync".to_string(),
        "work-accept" => "Work view accepted".to_string(),
        "conflict-resolution" => "Conflict resolution accepted".to_string(),
        "restore" => "Workspace restored".to_string(),
        other => other.replace('-', " "),
    }
}

fn history_cause(kind: &str) -> HistoryCause {
    match kind {
        "daemon-reconcile" | "sync" => HistoryCause::Sync,
        "work-accept" | "accept" => HistoryCause::Accept,
        "conflict-resolution" => HistoryCause::ConflictResolution,
        "restore" => HistoryCause::Restore,
        "archive" | "purge" => HistoryCause::Lifecycle,
        _ => HistoryCause::Unknown,
    }
}

fn path_operation(operation: &str) -> PathHistoryOperation {
    match LocalWriteOperation::from_str(operation) {
        LocalWriteOperation::Create => PathHistoryOperation::Create,
        LocalWriteOperation::Modify => PathHistoryOperation::Modify,
        LocalWriteOperation::Delete => PathHistoryOperation::Delete,
        LocalWriteOperation::Rename => PathHistoryOperation::Rename,
        LocalWriteOperation::Policy => PathHistoryOperation::Policy,
        LocalWriteOperation::Unknown => PathHistoryOperation::Unknown,
    }
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;
