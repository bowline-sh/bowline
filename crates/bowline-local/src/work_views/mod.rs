use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt, fs, io,
    path::{Component, Path, PathBuf},
};

use bowline_control_plane::{
    ControlPlaneClient, ControlPlaneError, ControlPlaneTimestamp, ObjectKind, ObjectMetadataCommit,
    ObjectPointer, ObjectRetentionStateUpdate, UploadIntentRequest, WorkViewCreate,
    WorkViewOverlayCommit, WorkViewUpdateError, WorkspaceRef,
};
use bowline_core::{
    commands::{
        AgentLeaseOutputState, CONTRACT_VERSION, CommandName, WorkCleanupCommandOutput,
        WorkDiffCommandOutput, WorkLifecycleCommandOutput, WorkListCommandOutput,
        WorkonCommandOutput,
    },
    events::{EventName, EventSeverity, EventSubject, EventSubjectKind, WorkspaceEvent},
    ids::{ContentId, DeviceId, EventId, ProjectId, WorkViewId},
    policy::{MaterializationMode, PathClassification},
    status::{SafeAction, StatusLevel, WorkspaceStatus},
    work_views::{
        WorkCommandAction, WorkDiffChangeKind, WorkDiffEntry, WorkView, WorkViewLifecycle,
        WorkViewRetention, WorkViewRetentionState, WorkViewSyncState, WorkViewVisibility,
    },
    workspace_graph::normalize_workspace_path,
};
use bowline_storage::{
    ByteStore, ByteStoreError, ObjectKind as StorageObjectKind, PackRecordInput, PackWriteOutput,
    PackfileError, RetentionState as StorageRetentionState, StorageKey, write_source_packs,
};
use serde::Serialize;

use crate::{
    metadata::{MetadataError, MetadataStore, default_database_path},
    policy::{PathFacts, UserPolicy, classify_path},
};

mod materialize;
mod overlay;
pub mod overlay_resolution;

#[derive(Debug, Clone)]
pub struct WorkonOptions {
    pub db_path: Option<PathBuf>,
    pub project_path: String,
    pub name: String,
    pub owner_device_id: Option<DeviceId>,
    pub generated_at: String,
}

#[derive(Debug, Clone)]
pub struct WorkListOptions {
    pub db_path: Option<PathBuf>,
    pub include_hidden: bool,
    pub current_device_id: Option<DeviceId>,
    pub generated_at: String,
}

#[derive(Debug, Clone)]
pub struct WorkSelectorOptions {
    pub db_path: Option<PathBuf>,
    pub selector: String,
    pub generated_at: String,
}

#[derive(Debug, Clone)]
pub struct WorkCleanupOptions {
    pub db_path: Option<PathBuf>,
    pub apply: bool,
    pub generated_at: String,
}

#[derive(Debug)]
pub enum WorkViewError {
    MissingMetadataDb,
    MissingWorkspace,
    MissingWorkspaceRoot,
    MissingProject {
        path: String,
    },
    MissingBaseSnapshot {
        path: String,
    },
    DirtyProject {
        path: String,
    },
    InvalidName {
        name: String,
        reason: &'static str,
    },
    NameCollision {
        name: String,
        project_path: String,
    },
    AmbiguousSelector {
        selector: String,
        matches: Vec<String>,
    },
    MissingWorkView {
        selector: String,
    },
    InactiveWorkView {
        name: String,
    },
    UnrestorableWorkView {
        name: String,
    },
    UnsafeWorkViewPath {
        path: String,
        reason: &'static str,
    },
    Metadata(MetadataError),
    Io(io::Error),
}

impl fmt::Display for WorkViewError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingMetadataDb => {
                write!(
                    formatter,
                    "metadata database path could not be resolved for work-view commands"
                )
            }
            Self::MissingWorkspace => write!(formatter, "no bowline workspace is initialized"),
            Self::MissingWorkspaceRoot => write!(formatter, "workspace root is missing"),
            Self::MissingProject { path } => {
                write!(formatter, "no tracked project was found for `{path}`")
            }
            Self::MissingBaseSnapshot { path } => write!(
                formatter,
                "work view for `{path}` needs a fresh project snapshot before it can be created"
            ),
            Self::DirtyProject { path } => write!(
                formatter,
                "work view for `{path}` needs the current project changes to sync before it can be created"
            ),
            Self::InvalidName { name, reason } => {
                write!(formatter, "work view name `{name}` is invalid: {reason}")
            }
            Self::NameCollision { name, project_path } => write!(
                formatter,
                "work view `{name}` already exists for project `{project_path}`"
            ),
            Self::AmbiguousSelector { selector, matches } => write!(
                formatter,
                "work view selector `{selector}` is ambiguous: {}",
                matches.join(", ")
            ),
            Self::MissingWorkView { selector } => {
                write!(formatter, "work view `{selector}` was not found")
            }
            Self::InactiveWorkView { name } => {
                write!(
                    formatter,
                    "work view `{name}` must be restored before it can be accepted"
                )
            }
            Self::UnrestorableWorkView { name } => {
                write!(formatter, "work view `{name}` is not restorable")
            }
            Self::UnsafeWorkViewPath { path, reason } => {
                write!(formatter, "unsafe work-view path `{path}`: {reason}")
            }
            Self::Metadata(error) => error.fmt(formatter),
            Self::Io(error) => write!(formatter, "work-view file operation failed: {error}"),
        }
    }
}

impl Error for WorkViewError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Metadata(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<MetadataError> for WorkViewError {
    fn from(error: MetadataError) -> Self {
        Self::Metadata(error)
    }
}

impl From<io::Error> for WorkViewError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Debug)]
pub enum WorkViewOverlaySyncError {
    WorkView(WorkViewError),
    Metadata(MetadataError),
    ControlPlane(ControlPlaneError),
    WorkViewUpdate(WorkViewUpdateError),
    Packfile(PackfileError),
    ByteStore(ByteStoreError),
    Json(serde_json::Error),
    MissingOverlayPack,
}

impl fmt::Display for WorkViewOverlaySyncError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkView(error) => error.fmt(formatter),
            Self::Metadata(error) => error.fmt(formatter),
            Self::ControlPlane(error) => error.fmt(formatter),
            Self::WorkViewUpdate(error) => error.fmt(formatter),
            Self::Packfile(error) => error.fmt(formatter),
            Self::ByteStore(error) => error.fmt(formatter),
            Self::Json(error) => error.fmt(formatter),
            Self::MissingOverlayPack => write!(formatter, "overlay pack writer produced no pack"),
        }
    }
}

impl Error for WorkViewOverlaySyncError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::WorkView(error) => Some(error),
            Self::Metadata(error) => Some(error),
            Self::ControlPlane(error) => Some(error),
            Self::WorkViewUpdate(error) => Some(error),
            Self::Packfile(error) => Some(error),
            Self::ByteStore(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::MissingOverlayPack => None,
        }
    }
}

impl From<WorkViewError> for WorkViewOverlaySyncError {
    fn from(error: WorkViewError) -> Self {
        Self::WorkView(error)
    }
}

impl From<MetadataError> for WorkViewOverlaySyncError {
    fn from(error: MetadataError) -> Self {
        Self::Metadata(error)
    }
}

impl From<ControlPlaneError> for WorkViewOverlaySyncError {
    fn from(error: ControlPlaneError) -> Self {
        Self::ControlPlane(error)
    }
}

impl From<WorkViewUpdateError> for WorkViewOverlaySyncError {
    fn from(error: WorkViewUpdateError) -> Self {
        Self::WorkViewUpdate(error)
    }
}

impl From<PackfileError> for WorkViewOverlaySyncError {
    fn from(error: PackfileError) -> Self {
        Self::Packfile(error)
    }
}

impl From<ByteStoreError> for WorkViewOverlaySyncError {
    fn from(error: ByteStoreError) -> Self {
        Self::ByteStore(error)
    }
}

impl From<serde_json::Error> for WorkViewOverlaySyncError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

pub fn create_work_view(options: WorkonOptions) -> Result<WorkonCommandOutput, WorkViewError> {
    validate_work_view_name(&options.name)?;
    let store = open_store(options.db_path.as_deref())?;
    let workspace = store
        .current_workspace()?
        .ok_or(WorkViewError::MissingWorkspace)?;
    let root = store
        .current_workspace_root()?
        .ok_or(WorkViewError::MissingWorkspaceRoot)?;
    let project = store
        .current_project_by_path(&options.project_path)?
        .ok_or_else(|| WorkViewError::MissingProject {
            path: options.project_path.clone(),
        })?;
    if !store
        .work_views_by_name(&workspace.id, Some(&project.id), &options.name)?
        .is_empty()
    {
        return Err(WorkViewError::NameCollision {
            name: options.name,
            project_path: project.path,
        });
    }
    let visible_path = visible_path(&root, &project.path, &options.name);
    ensure_no_symlink_ancestors(
        &visible_path,
        &expand_display_path(&root),
        "work view materialization escapes workspace",
    )?;
    let base_snapshot_id = store
        .project_latest_snapshot_id(&workspace.id, &project.id)?
        .ok_or_else(|| WorkViewError::MissingBaseSnapshot {
            path: project.path.clone(),
        })?;
    if project_has_pending_local_writes(&store, &workspace.id, &project.id, &project.path)? {
        return Err(WorkViewError::DirtyProject { path: project.path });
    }
    let work_view = WorkView {
        id: work_view_id(workspace.id.as_str(), project.id.as_str(), &options.name),
        workspace_id: workspace.id.clone(),
        project_id: project.id,
        project_path: project.path,
        name: options.name,
        visible_path: display_path(&visible_path),
        base_snapshot_id,
        overlay_head: "overlay_empty".to_string(),
        overlay_version: 0,
        env_profile: "default".to_string(),
        lifecycle: WorkViewLifecycle::Active,
        visibility: WorkViewVisibility::DefaultVisible,
        sync_state: WorkViewSyncState::LocalOnly,
        retention: WorkViewRetention {
            state: WorkViewRetentionState::Current,
            retain_until: None,
            restorable: false,
        },
        owner_device_id: options.owner_device_id,
        followed_by: Vec::new(),
        host_materializations: vec![display_path(&visible_path)],
        attention: Vec::new(),
        created_at: options.generated_at.clone(),
        updated_at: options.generated_at.clone(),
    };
    let base_files = collect_work_view_base_files(&store, &work_view)?;
    ensure_fresh_materialization_path(&visible_path)?;
    fs::create_dir_all(&visible_path)?;
    if let Some(main_root) = main_project_root(&store, &work_view)? {
        materialize::materialize_base_files(&main_root, &visible_path)?;
    }
    let metadata_result =
        persist_new_work_view(&store, &work_view, &base_files, &options.generated_at);
    if let Err(error) = metadata_result {
        remove_materialization_tree(&visible_path);
        return Err(error);
    }
    append_work_event(
        &store,
        EventName::WorkCreated,
        &work_view,
        &options.generated_at,
    );
    Ok(WorkonCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Workon,
        generated_at: options.generated_at,
        action: WorkCommandAction::Created,
        work_view,
        status: WorkspaceStatus::healthy(),
        next_actions: vec![SafeAction {
            label: "Open the work view".to_string(),
            command: Some("cd .work/<project>/<name>".to_string()),
        }],
    })
}

fn persist_new_work_view(
    store: &MetadataStore,
    work_view: &WorkView,
    base_files: &[(String, String)],
    captured_at: &str,
) -> Result<(), WorkViewError> {
    store.upsert_work_view(work_view)?;
    store.replace_work_view_base_files(
        &work_view.workspace_id,
        &work_view.id,
        base_files,
        captured_at,
    )?;
    Ok(())
}

pub fn list_work_views(options: WorkListOptions) -> Result<WorkListCommandOutput, WorkViewError> {
    let store = open_store(options.db_path.as_deref())?;
    let workspace = store
        .current_workspace()?
        .ok_or(WorkViewError::MissingWorkspace)?;
    let work_views = store.work_views(
        &workspace.id,
        options.include_hidden,
        options.current_device_id.as_ref(),
    )?;
    let status = status_for_work_views(&work_views);
    Ok(WorkListCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Work,
        generated_at: options.generated_at,
        action: WorkCommandAction::Listed,
        workspace_id: workspace.id,
        work_views,
        include_hidden: options.include_hidden,
        status,
        next_actions: vec![SafeAction {
            label: "Start a work view".to_string(),
            command: Some("bowline workon <name>".to_string()),
        }],
    })
}

#[derive(Debug, Clone)]
pub struct WorkViewOverlaySyncOptions {
    pub db_path: PathBuf,
    pub device_id: DeviceId,
    pub storage_key: StorageKey,
    pub key_epoch: u32,
    pub generated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkViewOverlaySyncReport {
    pub uploaded: usize,
    pub attention: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OverlayPayload {
    schema_version: u32,
    work_view_id: String,
    base_snapshot_id: String,
    entries: Vec<OverlayPayloadEntry>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OverlayPayloadEntry {
    path: String,
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<String>,
    contains_secrets: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes: Option<Vec<u8>>,
}

pub fn sync_local_work_view_overlays(
    options: WorkViewOverlaySyncOptions,
    control_plane: &dyn ControlPlaneClient,
    byte_store: &dyn ByteStore,
    workspace_ref: &WorkspaceRef,
) -> Result<WorkViewOverlaySyncReport, WorkViewOverlaySyncError> {
    let store = MetadataStore::open(&options.db_path)?;
    let workspace = store
        .current_workspace()?
        .ok_or(WorkViewError::MissingWorkspace)?;
    let mut report = WorkViewOverlaySyncReport {
        uploaded: 0,
        attention: 0,
    };
    let mut pending = Vec::new();

    for work_view in store.work_views(&workspace.id, true, None)? {
        if work_view.lifecycle != WorkViewLifecycle::Active {
            continue;
        }
        if matches!(
            work_view.sync_state,
            WorkViewSyncState::Attention | WorkViewSyncState::Conflicted
        ) {
            continue;
        }
        let deltas = match overlay_deltas_for_upload(&store, &work_view) {
            Ok(deltas) => deltas,
            Err(error) => {
                let mut work_view = work_view;
                work_view.sync_state = WorkViewSyncState::Attention;
                work_view.attention = vec![format!(
                    "Work view overlay needs review before sync: {error}"
                )];
                work_view.updated_at = options.generated_at.clone();
                store.upsert_work_view(&work_view)?;
                report.attention += 1;
                continue;
            }
        };
        if deltas.is_empty()
            && work_view.sync_state != WorkViewSyncState::LocalOnly
            && work_view.overlay_head == "overlay_empty"
        {
            continue;
        }
        pending.push((work_view, deltas));
    }
    if pending.is_empty() {
        return Ok(report);
    }

    let mut remote_views = control_plane.list_work_views(workspace.id.as_str(), true)?;
    for (mut work_view, deltas) in pending {
        if deltas
            .iter()
            .any(|delta| delta.kind.requires_review() || delta.contains_secrets)
        {
            work_view.sync_state = WorkViewSyncState::Attention;
            work_view.attention =
                vec!["Work view has changes that need review before overlay sync.".into()];
            work_view.updated_at = options.generated_at.clone();
            store.upsert_work_view(&work_view)?;
            report.attention += 1;
            continue;
        }

        let payload_bytes = overlay_payload_bytes(&work_view, &deltas)?;
        let overlay_digest = format!("b3_{}", blake3::hash(&payload_bytes).to_hex());
        let overlay_pack = derive_overlay_payload_pack(
            &workspace.id,
            &payload_bytes,
            options.storage_key,
            options.key_epoch,
        )?;

        let remote_record = match remote_views
            .iter()
            .find(|remote| remote.work_view_id == work_view.id.as_str())
            .cloned()
        {
            Some(record) => record,
            None => {
                let base_workspace_version =
                    if workspace_ref.snapshot_id == work_view.base_snapshot_id.as_str() {
                        workspace_ref.version
                    } else {
                        0
                    };
                let created = control_plane.create_work_view(WorkViewCreate {
                    workspace_id: workspace.id.as_str().to_string(),
                    work_view_id: work_view.id.as_str().to_string(),
                    project_id: work_view.project_id.as_str().to_string(),
                    name: work_view.id.as_str().to_string(),
                    visible_path: format!(".work/{}", work_view.id.as_str()),
                    base_snapshot_id: work_view.base_snapshot_id.as_str().to_string(),
                    base_workspace_version,
                    created_by_device_id: options.device_id.as_str().to_string(),
                })?;
                remote_views.push(created.clone());
                created
            }
        };

        if deltas.is_empty() && work_view.overlay_head == "overlay_empty" {
            if remote_record.overlay_head.is_some() || remote_record.overlay_version > 0 {
                work_view.sync_state = WorkViewSyncState::Attention;
                work_view.attention =
                    vec!["Remote work view overlay changed; review before syncing.".to_string()];
                report.attention += 1;
            } else {
                work_view.sync_state = WorkViewSyncState::Synced;
                work_view.attention.clear();
                work_view.overlay_version = remote_record.overlay_version;
            }
            work_view.updated_at = options.generated_at.clone();
            store.upsert_work_view(&work_view)?;
            continue;
        }

        if remote_record.overlay_version > work_view.overlay_version {
            if remote_record
                .overlay_head
                .as_ref()
                .is_some_and(|remote| overlay_pointer_matches_pack(remote, &overlay_pack))
            {
                work_view.overlay_head = overlay_digest;
                work_view.overlay_version = remote_record.overlay_version;
                work_view.sync_state = WorkViewSyncState::Synced;
                work_view.attention.clear();
                work_view.updated_at = options.generated_at.clone();
                store.upsert_work_view(&work_view)?;
                continue;
            }
            work_view.sync_state = WorkViewSyncState::Attention;
            work_view.attention = vec![format!(
                "Remote work view overlay is at version {}, but this device last synced version {}.",
                remote_record.overlay_version, work_view.overlay_version
            )];
            work_view.updated_at = options.generated_at.clone();
            store.upsert_work_view(&work_view)?;
            report.attention += 1;
            continue;
        }
        if work_view.overlay_head == overlay_digest {
            continue;
        }

        let overlay_object = upload_overlay_payload(
            &workspace.id,
            &options.device_id,
            overlay_pack,
            control_plane,
            byte_store,
            options.key_epoch,
        )?;
        match control_plane.commit_work_view_overlay(WorkViewOverlayCommit {
            workspace_id: workspace.id.as_str().to_string(),
            work_view_id: work_view.id.as_str().to_string(),
            expected_overlay_version: work_view.overlay_version,
            overlay_object: overlay_object.clone(),
            committed_by_device_id: options.device_id.as_str().to_string(),
        }) {
            Ok(updated) => {
                let updated_overlay_version = updated.overlay_version;
                if let Some(remote) = remote_views
                    .iter_mut()
                    .find(|remote| remote.work_view_id == updated.work_view_id)
                {
                    *remote = updated;
                }
                work_view.overlay_head = overlay_digest;
                work_view.overlay_version = updated_overlay_version;
                work_view.sync_state = WorkViewSyncState::Synced;
                work_view.attention.clear();
                work_view.updated_at = options.generated_at.clone();
                store.upsert_work_view(&work_view)?;
                append_work_event(
                    &store,
                    EventName::OverlayChanged,
                    &work_view,
                    &options.generated_at,
                );
                report.uploaded += 1;
            }
            Err(WorkViewUpdateError::StaleOverlayHead(stale)) => {
                if stale
                    .current
                    .overlay_head
                    .as_ref()
                    .is_some_and(|current| current.object_key == overlay_object.object_key)
                {
                    let current_overlay_version = stale.current.overlay_version;
                    if let Some(remote) = remote_views
                        .iter_mut()
                        .find(|remote| remote.work_view_id == stale.current.work_view_id)
                    {
                        *remote = stale.current;
                    }
                    work_view.overlay_head = overlay_digest;
                    work_view.overlay_version = current_overlay_version;
                    work_view.sync_state = WorkViewSyncState::Synced;
                    work_view.attention.clear();
                    work_view.updated_at = options.generated_at.clone();
                    store.upsert_work_view(&work_view)?;
                    report.uploaded += 1;
                    continue;
                }
                control_plane.mark_object_retention_state(ObjectRetentionStateUpdate::new(
                    workspace.id.as_str(),
                    overlay_object.object_key,
                    StorageRetentionState::OrphanCandidate,
                ))?;
                work_view.sync_state = WorkViewSyncState::Attention;
                work_view.attention = vec![format!(
                    "Remote work view overlay is at version {}, not expected version {}.",
                    stale.current.overlay_version, stale.expected_overlay_version
                )];
                work_view.updated_at = options.generated_at.clone();
                store.upsert_work_view(&work_view)?;
                report.attention += 1;
            }
            Err(error) => return Err(error.into()),
        }
    }

    Ok(report)
}

fn overlay_deltas_for_upload(
    store: &MetadataStore,
    work_view: &WorkView,
) -> Result<Vec<overlay::OverlayDelta>, WorkViewError> {
    let mut deltas = filesystem_overlay_deltas(store, work_view)?;
    deltas.extend(
        overlay::logged_overlay_deltas(store, work_view)?
            .into_iter()
            .filter(|delta| delta.kind.requires_review()),
    );
    let workspace_root = expand_display_path(
        store
            .current_workspace_root()?
            .ok_or(WorkViewError::MissingWorkspaceRoot)?,
    );
    let work_root = expand_display_path(&work_view.visible_path);
    let mut policy_filtered = Vec::with_capacity(deltas.len());
    for delta in deltas {
        if overlay_delta_is_ignored_by_policy(
            store,
            work_view,
            &workspace_root,
            &work_root,
            &delta,
        )? {
            continue;
        }
        policy_filtered.push(delta);
    }
    let mut deltas = policy_filtered;
    deltas.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then(overlay_delta_kind_name(&left.kind).cmp(overlay_delta_kind_name(&right.kind)))
    });
    deltas.dedup_by(|left, right| left.path == right.path && left.kind == right.kind);
    Ok(deltas)
}

fn overlay_delta_is_ignored_by_policy(
    store: &MetadataStore,
    work_view: &WorkView,
    workspace_root: &Path,
    work_root: &Path,
    delta: &overlay::OverlayDelta,
) -> Result<bool, WorkViewError> {
    let destination_workspace_path = workspace_path_for_project_file(work_view, &delta.path);
    let work_path = work_root.join(&delta.path);
    let source = work_path.exists().then_some(work_path.as_path());
    let policy = clean_accept_policy(
        store,
        workspace_root,
        &work_view.workspace_id,
        &destination_workspace_path,
        source,
    )?;
    if is_ignored_clean_accept_policy(policy.classification, policy.mode) {
        return Ok(true);
    }
    if delta.contains_secrets {
        return Ok(false);
    }
    if is_clean_accept_policy_eligible(policy.classification, policy.mode) {
        return Ok(false);
    }
    Err(WorkViewError::UnsafeWorkViewPath {
        path: normalize_workspace_path(&delta.path.display().to_string()),
        reason: "work view path policy requires review",
    })
}

fn overlay_payload_bytes(
    work_view: &WorkView,
    deltas: &[overlay::OverlayDelta],
) -> Result<Vec<u8>, WorkViewOverlaySyncError> {
    let work_root = expand_display_path(&work_view.visible_path);
    let mut entries = Vec::with_capacity(deltas.len());
    for delta in deltas {
        let file_path = work_root.join(&delta.path);
        let bytes = if matches!(
            delta.kind,
            overlay::OverlayDeltaKind::Create
                | overlay::OverlayDeltaKind::Modify
                | overlay::OverlayDeltaKind::Rename { .. }
        ) {
            Some(fs::read(&file_path).map_err(WorkViewError::from)?)
        } else {
            None
        };
        let content_hash = bytes
            .as_ref()
            .map(|bytes| format!("b3_{}", blake3::hash(bytes).to_hex()));
        entries.push(OverlayPayloadEntry {
            path: normalize_workspace_path(&delta.path.display().to_string()),
            kind: overlay_delta_kind_name(&delta.kind),
            from: overlay_delta_rename_from(&delta.kind),
            contains_secrets: delta.contains_secrets,
            content_hash,
            bytes,
        });
    }
    let payload = OverlayPayload {
        schema_version: 1,
        work_view_id: work_view.id.as_str().to_string(),
        base_snapshot_id: work_view.base_snapshot_id.as_str().to_string(),
        entries,
    };
    Ok(serde_json::to_vec(&payload)?)
}

fn overlay_delta_kind_name(kind: &overlay::OverlayDeltaKind) -> &'static str {
    match kind {
        overlay::OverlayDeltaKind::Create => "create",
        overlay::OverlayDeltaKind::Modify => "modify",
        overlay::OverlayDeltaKind::Delete => "delete",
        overlay::OverlayDeltaKind::Rename { .. } => "rename",
        overlay::OverlayDeltaKind::Symlink => "symlink",
        overlay::OverlayDeltaKind::Chmod => "chmod",
        overlay::OverlayDeltaKind::Unsupported { .. } => "unsupported",
    }
}

fn overlay_delta_rename_from(kind: &overlay::OverlayDeltaKind) -> Option<String> {
    match kind {
        overlay::OverlayDeltaKind::Rename { from } => {
            Some(normalize_workspace_path(&from.display().to_string()))
        }
        _ => None,
    }
}

fn project_has_pending_local_writes(
    store: &MetadataStore,
    workspace_id: &bowline_core::ids::WorkspaceId,
    project_id: &ProjectId,
    project_path: &str,
) -> Result<bool, WorkViewError> {
    let project_path = normalize_workspace_path(project_path);
    let synced_at = store
        .workspace_sync_head(workspace_id)?
        .map(|head| head.observed_at);
    for write in store.local_write_log(workspace_id)? {
        if synced_at
            .as_deref()
            .is_some_and(|synced_at| write.created_at.as_str() <= synced_at)
        {
            continue;
        }
        let Ok(relative_path) = store.workspace_relative_path(workspace_id, &write.path) else {
            if write.project_id.as_ref() == Some(project_id) {
                return Ok(true);
            }
            continue;
        };
        let relative_path = normalize_workspace_path(&relative_path);
        if relative_path == project_path && write.operation == "modify" {
            continue;
        }
        if relative_path == ".work"
            || relative_path
                .strip_prefix(".work")
                .is_some_and(|suffix| suffix.starts_with('/'))
        {
            continue;
        }
        if write.project_id.as_ref() == Some(project_id) {
            return Ok(true);
        }
        if relative_path == project_path
            || relative_path
                .strip_prefix(&project_path)
                .is_some_and(|suffix| suffix.starts_with('/'))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn derive_overlay_payload_pack(
    workspace_id: &bowline_core::ids::WorkspaceId,
    payload_bytes: &[u8],
    storage_key: StorageKey,
    key_epoch: u32,
) -> Result<PackWriteOutput, WorkViewOverlaySyncError> {
    let payload_content_id =
        ContentId::new(format!("overlay_{}", blake3::hash(payload_bytes).to_hex()));
    let packs = write_source_packs(
        workspace_id.clone(),
        &[PackRecordInput {
            content_id: payload_content_id,
            bytes: payload_bytes.to_vec(),
        }],
        payload_bytes.len().max(1),
        storage_key,
        key_epoch,
    )?;
    let pack = packs
        .into_iter()
        .next()
        .ok_or(WorkViewOverlaySyncError::MissingOverlayPack)?;
    Ok(pack)
}

fn overlay_pointer_matches_pack(pointer: &ObjectPointer, pack: &PackWriteOutput) -> bool {
    pointer.kind == ObjectKind::AgentOverlay
        && overlay_pack_payload_content_id(pack)
            .is_some_and(|content_id| pointer.content_id == content_id)
}

fn overlay_pack_payload_content_id(pack: &PackWriteOutput) -> Option<&str> {
    pack.locators
        .first()
        .map(|locator| locator.content_id.as_str())
}

fn upload_overlay_payload(
    workspace_id: &bowline_core::ids::WorkspaceId,
    device_id: &DeviceId,
    pack: PackWriteOutput,
    control_plane: &dyn ControlPlaneClient,
    byte_store: &dyn ByteStore,
    key_epoch: u32,
) -> Result<ObjectPointer, WorkViewOverlaySyncError> {
    match control_plane.head_object_metadata(workspace_id.as_str(), pack.object_key.as_str()) {
        Ok(metadata) => {
            validate_overlay_object_metadata(&metadata, &pack.object_key, &pack.bytes, key_epoch)?;
            return Ok(ObjectPointer {
                object_key: pack.object_key.as_str().to_string(),
                content_id: overlay_pack_payload_content_id(&pack)
                    .ok_or(WorkViewOverlaySyncError::MissingOverlayPack)?
                    .to_string(),
                byte_len: metadata.byte_len,
                hash: metadata.hash,
                key_epoch: metadata.key_epoch,
                kind: ObjectKind::AgentOverlay,
                created_at: ControlPlaneTimestamp {
                    tick: metadata.created_at_unix_ms,
                },
            });
        }
        Err(ControlPlaneError::ObjectMissing { .. }) => {}
        Err(error) => return Err(error.into()),
    }
    control_plane.create_upload_intent(
        UploadIntentRequest::new(
            workspace_id.as_str(),
            ObjectKind::AgentOverlay,
            pack.bytes.len() as u64,
        )
        .with_object_key(pack.object_key.as_str())
        .with_content_id(
            overlay_pack_payload_content_id(&pack)
                .ok_or(WorkViewOverlaySyncError::MissingOverlayPack)?,
        ),
    )?;

    let metadata = match byte_store.put_object_with_content_id_at_epoch(
        pack.object_key.clone(),
        StorageObjectKind::AgentOverlay,
        overlay_pack_payload_content_id(&pack)
            .ok_or(WorkViewOverlaySyncError::MissingOverlayPack)?,
        &pack.bytes,
        key_epoch,
        Some(device_id),
    ) {
        Ok(metadata) => metadata,
        Err(ByteStoreError::ObjectAlreadyExists(existing_key))
            if existing_key == pack.object_key =>
        {
            byte_store.head_object(&pack.object_key)?
        }
        Err(error) => return Err(error.into()),
    };
    validate_overlay_object_metadata(&metadata, &pack.object_key, &pack.bytes, key_epoch)?;
    let pointer = ObjectPointer {
        object_key: pack.object_key.as_str().to_string(),
        content_id: overlay_pack_payload_content_id(&pack)
            .ok_or(WorkViewOverlaySyncError::MissingOverlayPack)?
            .to_string(),
        byte_len: metadata.byte_len,
        hash: metadata.hash,
        key_epoch: metadata.key_epoch,
        kind: ObjectKind::AgentOverlay,
        created_at: ControlPlaneTimestamp {
            tick: metadata.created_at_unix_ms,
        },
    };
    control_plane.commit_uploaded_object_metadata(ObjectMetadataCommit {
        workspace_id: workspace_id.as_str().to_string(),
        object: pointer.clone(),
        committed_by_device_id: device_id.as_str().to_string(),
    })?;
    Ok(pointer)
}

fn validate_overlay_object_metadata(
    metadata: &bowline_storage::ObjectMetadata,
    object_key: &bowline_storage::ObjectKey,
    bytes: &[u8],
    key_epoch: u32,
) -> Result<(), WorkViewOverlaySyncError> {
    let expected_hash = format!("b3_{}", blake3::hash(bytes).to_hex());
    if metadata.key != *object_key
        || metadata.kind != StorageObjectKind::AgentOverlay
        || metadata.byte_len != bytes.len() as u64
        || metadata.hash != expected_hash
        || metadata.key_epoch != key_epoch
    {
        return Err(ControlPlaneError::Conflict {
            resource: "overlay object metadata",
            reason: "stored overlay object metadata does not match encrypted pack bytes",
        }
        .into());
    }
    Ok(())
}

pub fn diff_work_view(
    options: WorkSelectorOptions,
) -> Result<WorkDiffCommandOutput, WorkViewError> {
    let store = open_store(options.db_path.as_deref())?;
    let work_view = resolve_work_view(&store, &options.selector)?;
    let changes = diff_entries(&store, &work_view)?;
    let status = status_for_changes(&changes);
    Ok(WorkDiffCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Diff,
        generated_at: options.generated_at,
        action: WorkCommandAction::Diffed,
        work_view,
        changes,
        status,
        next_actions: vec![SafeAction {
            label: "Accept work view".to_string(),
            command: Some(format!("bowline accept {}", options.selector)),
        }],
    })
}

pub fn accept_work_view(
    options: WorkSelectorOptions,
) -> Result<WorkLifecycleCommandOutput, WorkViewError> {
    let store = open_store(options.db_path.as_deref())?;
    let mut work_view = resolve_work_view(&store, &options.selector)?;
    if !matches!(
        work_view.lifecycle,
        WorkViewLifecycle::Active | WorkViewLifecycle::ReviewReady
    ) {
        return Err(WorkViewError::InactiveWorkView {
            name: work_view.name,
        });
    }
    let conflicts = apply_clean_work_view_files(&store, &work_view)?;
    if !conflicts.is_empty() {
        work_view.lifecycle = WorkViewLifecycle::ReviewReady;
        work_view.sync_state = WorkViewSyncState::Attention;
        work_view.attention = conflicts
            .iter()
            .map(|path| format!("Manual review needed before accepting {path}."))
            .collect();
        work_view.updated_at = options.generated_at.clone();
        store.upsert_work_view(&work_view)?;
        append_work_event(
            &store,
            EventName::WorkReviewReady,
            &work_view,
            &options.generated_at,
        );
        return Ok(WorkLifecycleCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::Accept,
            generated_at: options.generated_at,
            action: WorkCommandAction::ReviewReady,
            work_view,
            status: WorkspaceStatus {
                level: StatusLevel::Attention,
                attention_items: vec![
                    "Accept needs review before touching the main view.".to_string(),
                ],
            },
            next_actions: vec![SafeAction {
                label: "Inspect work-view diff".to_string(),
                command: Some(format!("bowline review {}", options.selector)),
            }],
        });
    }

    work_view.lifecycle = WorkViewLifecycle::Accepted;
    work_view.visibility = WorkViewVisibility::Hidden;
    work_view.sync_state = WorkViewSyncState::Synced;
    work_view.attention.clear();
    work_view.retention = WorkViewRetention {
        state: WorkViewRetentionState::Retained,
        retain_until: None,
        restorable: true,
    };
    work_view.updated_at = options.generated_at.clone();
    store.upsert_work_view(&work_view)?;
    mark_matching_agent_leases_accepted(&store, &work_view, &options.generated_at)?;
    append_work_event(
        &store,
        EventName::WorkAccepted,
        &work_view,
        &options.generated_at,
    );
    Ok(WorkLifecycleCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Accept,
        generated_at: options.generated_at,
        action: WorkCommandAction::Accepted,
        work_view,
        status: WorkspaceStatus::healthy(),
        next_actions: vec![SafeAction {
            label: "Inspect workspace status".to_string(),
            command: Some("bowline status --all".to_string()),
        }],
    })
}

fn mark_matching_agent_leases_accepted(
    store: &MetadataStore,
    work_view: &WorkView,
    generated_at: &str,
) -> Result<(), WorkViewError> {
    mark_matching_agent_leases_output_state(
        store,
        work_view,
        AgentLeaseOutputState::Accepted,
        "accepted",
        generated_at,
    )
}

fn mark_matching_agent_leases_discarded(
    store: &MetadataStore,
    work_view: &WorkView,
    generated_at: &str,
) -> Result<(), WorkViewError> {
    mark_matching_agent_leases_output_state(
        store,
        work_view,
        AgentLeaseOutputState::Discarded,
        "discarded",
        generated_at,
    )
}

fn mark_matching_agent_leases_output_state(
    store: &MetadataStore,
    work_view: &WorkView,
    output_state: AgentLeaseOutputState,
    status_summary: &str,
    generated_at: &str,
) -> Result<(), WorkViewError> {
    for mut lease in store.agent_leases(&work_view.workspace_id)? {
        if lease.work_view_id != work_view.id {
            continue;
        }
        if matches!(
            lease.output_state,
            AgentLeaseOutputState::Accepted | AgentLeaseOutputState::Discarded
        ) {
            continue;
        }
        lease.output_state = output_state;
        lease.status_summary = status_summary.to_string();
        lease.updated_at = generated_at.to_string();
        store.upsert_agent_lease(&lease)?;
    }
    Ok(())
}

pub fn discard_work_view(
    options: WorkSelectorOptions,
) -> Result<WorkLifecycleCommandOutput, WorkViewError> {
    transition_work_view(
        options,
        CommandName::Discard,
        WorkCommandAction::Discarded,
        WorkViewLifecycle::Discarded,
        WorkViewVisibility::Hidden,
        WorkViewRetention {
            state: WorkViewRetentionState::Retained,
            retain_until: None,
            restorable: true,
        },
        EventName::WorkDiscarded,
    )
}

pub fn restore_work_view(
    options: WorkSelectorOptions,
) -> Result<WorkLifecycleCommandOutput, WorkViewError> {
    let store = open_store(options.db_path.as_deref())?;
    let work_view = resolve_work_view(&store, &options.selector)?;
    ensure_restorable_work_view(&work_view)?;
    ensure_restorable_materialization(&store, &work_view)?;
    transition_work_view_with_store(
        store,
        work_view,
        options.generated_at,
        WorkViewTransition {
            command: CommandName::Restore,
            action: WorkCommandAction::Restored,
            lifecycle: WorkViewLifecycle::Active,
            visibility: WorkViewVisibility::DefaultVisible,
            retention: WorkViewRetention {
                state: WorkViewRetentionState::Current,
                retain_until: None,
                restorable: false,
            },
            event_name: EventName::WorkRestored,
        },
    )
}

pub fn cleanup_work_views(
    options: WorkCleanupOptions,
) -> Result<WorkCleanupCommandOutput, WorkViewError> {
    let store = open_store(options.db_path.as_deref())?;
    let workspace = store
        .current_workspace()?
        .ok_or(WorkViewError::MissingWorkspace)?;
    let candidates = store
        .work_views(&workspace.id, true, None)?
        .into_iter()
        .filter(|view| {
            matches!(
                view.lifecycle,
                WorkViewLifecycle::Accepted
                    | WorkViewLifecycle::Discarded
                    | WorkViewLifecycle::Expired
                    | WorkViewLifecycle::Archived
            )
        })
        .collect::<Vec<_>>();
    let previewed_paths = candidates
        .iter()
        .flat_map(|view| view.host_materializations.iter().cloned())
        .collect::<Vec<_>>();
    let mut deleted_paths = Vec::new();
    if options.apply {
        for mut view in candidates {
            let namespace_root =
                work_namespace_root(&store, &view)?.ok_or(WorkViewError::MissingWorkspaceRoot)?;
            let workspace_root = expand_display_path(
                store
                    .current_workspace_root()?
                    .ok_or(WorkViewError::MissingWorkspaceRoot)?,
            );
            ensure_no_symlink_ancestors(
                &namespace_root,
                &workspace_root,
                "cleanup namespace escapes .work",
            )?;
            for path in &view.host_materializations {
                let path = expand_display_path(path);
                ensure_path_inside(&path, &namespace_root, "cleanup is limited to .work")?;
                ensure_no_symlink_ancestors(
                    &path,
                    &namespace_root,
                    "cleanup target escapes .work",
                )?;
                if path.exists() {
                    ensure_existing_path_inside_real(
                        &path,
                        &namespace_root,
                        "cleanup target escapes .work",
                    )?;
                    fs::remove_dir_all(&path)?;
                    deleted_paths.push(display_path(&path));
                }
            }
            view.lifecycle = WorkViewLifecycle::Archived;
            view.visibility = WorkViewVisibility::Hidden;
            view.retention.state = WorkViewRetentionState::DeleteEligible;
            view.retention.retain_until = None;
            view.retention.restorable = false;
            view.updated_at = options.generated_at.clone();
            store.upsert_work_view(&view)?;
        }
        append_workspace_event(
            &store,
            EventName::WorkCleanupCompleted,
            &workspace.id,
            &options.generated_at,
            "Cleaned up retained work views",
        );
    } else {
        append_workspace_event(
            &store,
            EventName::WorkCleanupPreviewed,
            &workspace.id,
            &options.generated_at,
            "Previewed retained work-view cleanup",
        );
    }

    Ok(WorkCleanupCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Cleanup,
        generated_at: options.generated_at,
        action: if options.apply {
            WorkCommandAction::CleanupApplied
        } else {
            WorkCommandAction::CleanupPreviewed
        },
        workspace_id: workspace.id,
        previewed_paths,
        deleted_paths,
        status: WorkspaceStatus::healthy(),
        next_actions: vec![SafeAction {
            label: "List retained work views".to_string(),
            command: Some("bowline status --all".to_string()),
        }],
    })
}

fn transition_work_view(
    options: WorkSelectorOptions,
    command: CommandName,
    action: WorkCommandAction,
    lifecycle: WorkViewLifecycle,
    visibility: WorkViewVisibility,
    retention: WorkViewRetention,
    event_name: EventName,
) -> Result<WorkLifecycleCommandOutput, WorkViewError> {
    let store = open_store(options.db_path.as_deref())?;
    let work_view = resolve_work_view(&store, &options.selector)?;
    transition_work_view_with_store(
        store,
        work_view,
        options.generated_at,
        WorkViewTransition {
            command,
            action,
            lifecycle,
            visibility,
            retention,
            event_name,
        },
    )
}

struct WorkViewTransition {
    command: CommandName,
    action: WorkCommandAction,
    lifecycle: WorkViewLifecycle,
    visibility: WorkViewVisibility,
    retention: WorkViewRetention,
    event_name: EventName,
}

fn transition_work_view_with_store(
    store: MetadataStore,
    mut work_view: WorkView,
    generated_at: String,
    transition: WorkViewTransition,
) -> Result<WorkLifecycleCommandOutput, WorkViewError> {
    work_view.lifecycle = transition.lifecycle;
    work_view.visibility = transition.visibility;
    work_view.sync_state = WorkViewSyncState::LocalOnly;
    work_view.attention.clear();
    work_view.retention = transition.retention;
    work_view.updated_at = generated_at.clone();
    store.upsert_work_view(&work_view)?;
    if transition.action == WorkCommandAction::Discarded {
        mark_matching_agent_leases_discarded(&store, &work_view, &generated_at)?;
    }
    append_work_event(&store, transition.event_name, &work_view, &generated_at);
    Ok(WorkLifecycleCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: transition.command,
        generated_at,
        action: transition.action,
        work_view,
        status: WorkspaceStatus::healthy(),
        next_actions: vec![SafeAction {
            label: "List work views".to_string(),
            command: Some("bowline status --all".to_string()),
        }],
    })
}

fn resolve_work_view(store: &MetadataStore, selector: &str) -> Result<WorkView, WorkViewError> {
    let workspace = store
        .current_workspace()?
        .ok_or(WorkViewError::MissingWorkspace)?;
    if let Some(work_view) =
        store.work_view_by_id(&workspace.id, &WorkViewId::new(selector.to_string()))?
    {
        return Ok(work_view);
    }

    let matches = store.work_views_by_name(&workspace.id, None, selector)?;
    match matches.as_slice() {
        [work_view] => Ok(work_view.clone()),
        [] => resolve_work_view_by_visible_path(store, &workspace.id, selector)?.ok_or(
            WorkViewError::MissingWorkView {
                selector: selector.to_string(),
            },
        ),
        _ => Err(WorkViewError::AmbiguousSelector {
            selector: selector.to_string(),
            matches: matches
                .iter()
                .map(|view| format!("{} ({})", view.id.as_str(), view.project_path))
                .collect(),
        }),
    }
}

fn resolve_work_view_by_visible_path(
    store: &MetadataStore,
    workspace_id: &bowline_core::ids::WorkspaceId,
    selector: &str,
) -> Result<Option<WorkView>, WorkViewError> {
    let selector_path = normalize_lexical_path(expand_display_path(selector));
    Ok(store
        .work_views(workspace_id, true, None)?
        .into_iter()
        .filter(|view| {
            path_is_at_or_under(
                &selector_path,
                &normalize_lexical_path(expand_display_path(&view.visible_path)),
            )
        })
        .max_by_key(|view| {
            normalize_lexical_path(expand_display_path(&view.visible_path))
                .components()
                .count()
        }))
}

fn path_is_at_or_under(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}

fn normalize_lexical_path(path: PathBuf) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn ensure_restorable_work_view(work_view: &WorkView) -> Result<(), WorkViewError> {
    if work_view.retention.restorable
        && matches!(work_view.retention.state, WorkViewRetentionState::Retained)
    {
        return Ok(());
    }
    Err(WorkViewError::UnrestorableWorkView {
        name: work_view.name.clone(),
    })
}

fn ensure_restorable_materialization(
    store: &MetadataStore,
    work_view: &WorkView,
) -> Result<(), WorkViewError> {
    let work_root = expand_display_path(&work_view.visible_path);
    let namespace_root =
        work_namespace_root(store, work_view)?.ok_or(WorkViewError::MissingWorkspaceRoot)?;
    let workspace_root = expand_display_path(
        store
            .current_workspace_root()?
            .ok_or(WorkViewError::MissingWorkspaceRoot)?,
    );
    ensure_path_inside(
        &work_root,
        &namespace_root,
        "work view must live under .work",
    )?;
    ensure_no_symlink_ancestors(
        &namespace_root,
        &workspace_root,
        "work view namespace escapes .work",
    )?;
    ensure_no_symlink_ancestors(&work_root, &namespace_root, "work view root escapes .work")?;
    match fs::symlink_metadata(&work_root) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(WorkViewError::UnsafeWorkViewPath {
            path: work_root.display().to_string(),
            reason: "work view materialization path already exists",
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(&work_root)?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn diff_entries(
    store: &MetadataStore,
    work_view: &WorkView,
) -> Result<Vec<WorkDiffEntry>, WorkViewError> {
    let mut deltas = overlay::logged_overlay_deltas(store, work_view)?;
    let logged_paths = deltas
        .iter()
        .map(|delta| delta.path.clone())
        .collect::<BTreeSet<_>>();
    deltas.extend(
        filesystem_overlay_deltas(store, work_view)?
            .into_iter()
            .filter(|delta| !logged_paths.contains(&delta.path)),
    );
    if deltas.is_empty() {
        return Ok(Vec::new());
    }
    Ok(overlay::diff_entries_from_deltas(work_view, &deltas))
}

fn filesystem_overlay_deltas(
    store: &MetadataStore,
    work_view: &WorkView,
) -> Result<Vec<overlay::OverlayDelta>, WorkViewError> {
    let work_root = expand_display_path(&work_view.visible_path);
    let Some(namespace_root) = work_namespace_root(store, work_view)? else {
        return Ok(Vec::new());
    };
    ensure_path_inside(
        &work_root,
        &namespace_root,
        "work view must live under .work",
    )?;
    let workspace_root = expand_display_path(
        store
            .current_workspace_root()?
            .ok_or(WorkViewError::MissingWorkspaceRoot)?,
    );
    ensure_no_symlink_ancestors(
        &namespace_root,
        &workspace_root,
        "work view namespace escapes .work",
    )?;
    ensure_no_symlink_ancestors(&work_root, &namespace_root, "work view root escapes .work")?;
    overlay::filesystem_overlay_deltas(store, work_view, &work_root)
}

fn apply_clean_work_view_files(
    store: &MetadataStore,
    work_view: &WorkView,
) -> Result<Vec<String>, WorkViewError> {
    let Some(main_root) = main_project_root(store, work_view)? else {
        return Ok(Vec::new());
    };
    let work_root = expand_display_path(&work_view.visible_path);
    let Some(namespace_root) = work_namespace_root(store, work_view)? else {
        return Ok(Vec::new());
    };
    ensure_path_inside(
        &work_root,
        &namespace_root,
        "work view must live under .work",
    )?;
    let workspace_root = expand_display_path(
        store
            .current_workspace_root()?
            .ok_or(WorkViewError::MissingWorkspaceRoot)?,
    );
    ensure_no_symlink_ancestors(
        &namespace_root,
        &workspace_root,
        "work view namespace escapes .work",
    )?;
    ensure_no_symlink_ancestors(&work_root, &namespace_root, "work view root escapes .work")?;
    if !work_root.exists() {
        return Ok(Vec::new());
    }

    let mut conflicts = overlay_review_paths(store, work_view)?;
    let deletes = work_view_deletions(store, work_view)?;
    let deleted_relative_paths = deletes.iter().cloned().collect::<BTreeSet<_>>();
    let mut deleted_files = Vec::new();
    for delete in deletes {
        let main_file = main_root.join(&delete);
        ensure_path_inside(
            &main_file,
            &main_root,
            "accepted deletions must stay inside the main project",
        )?;
        let destination_workspace_path = workspace_path_for_project_file(work_view, &delete);
        let policy_source = main_file.exists().then_some(main_file.as_path());
        let policy = clean_accept_policy(
            store,
            &workspace_root,
            &work_view.workspace_id,
            &destination_workspace_path,
            policy_source,
        )?;
        if is_ignored_clean_accept_policy(policy.classification, policy.mode) {
            continue;
        }
        if is_secret_bearing_work_path(&delete)
            || is_source_control_metadata_path(&delete)
            || !is_clean_accept_policy_eligible(policy.classification, policy.mode)
            || (main_file.exists()
                && !main_matches_work_view_base(store, work_view, &delete, &main_file)?)
        {
            conflicts.push(normalize_workspace_path(&delete.display().to_string()));
        }
        deleted_files.push(main_file);
    }

    let mut files = Vec::new();
    for file in files_under(&work_root)? {
        let relative = file
            .strip_prefix(&work_root)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
            .to_path_buf();
        if deleted_relative_paths.contains(&relative) {
            continue;
        }
        let main_file = main_root.join(&relative);
        ensure_path_inside(
            &main_file,
            &main_root,
            "accepted files must stay inside the main project",
        )?;
        let destination_workspace_path = workspace_path_for_project_file(work_view, &relative);
        let policy = clean_accept_policy(
            store,
            &workspace_root,
            &work_view.workspace_id,
            &destination_workspace_path,
            Some(&file),
        )?;
        if is_ignored_clean_accept_policy(policy.classification, policy.mode) {
            continue;
        }
        if is_secret_bearing_work_path(&relative)
            || is_source_control_metadata_path(&relative)
            || !is_clean_accept_policy_eligible(policy.classification, policy.mode)
            || (!main_file.exists() && work_view_base_has_path(store, work_view, &relative)?)
            || (main_file.exists()
                && fs::read(&file)? != fs::read(&main_file)?
                && !main_matches_work_view_base(store, work_view, &relative, &main_file)?)
        {
            conflicts.push(normalize_workspace_path(&relative.display().to_string()));
            continue;
        }
        files.push((file, main_file));
    }
    if !conflicts.is_empty() {
        return Ok(conflicts);
    }

    for deleted_file in deleted_files {
        ensure_no_symlink_ancestors(
            &deleted_file,
            &main_root,
            "accepted deletion escapes project",
        )?;
        if let Ok(metadata) = fs::symlink_metadata(&deleted_file) {
            if metadata.file_type().is_symlink() {
                return Err(WorkViewError::UnsafeWorkViewPath {
                    path: deleted_file.display().to_string(),
                    reason: "accepted deletion refuses symlink targets",
                });
            }
            if metadata.is_dir() {
                fs::remove_dir_all(&deleted_file)?;
            } else {
                fs::remove_file(&deleted_file)?;
            }
        }
    }

    for (source, destination) in files {
        if let Some(parent) = destination.parent() {
            ensure_no_symlink_ancestors(parent, &main_root, "destination parent escapes project")?;
            fs::create_dir_all(parent)?;
        }
        ensure_no_symlink_ancestors(&destination, &main_root, "destination escapes project")?;
        fs::copy(source, destination)?;
    }
    Ok(Vec::new())
}

fn overlay_review_paths(
    store: &MetadataStore,
    work_view: &WorkView,
) -> Result<Vec<String>, WorkViewError> {
    Ok(overlay::logged_overlay_deltas(store, work_view)?
        .into_iter()
        .filter(|delta| delta.kind.requires_review())
        .map(|delta| normalize_workspace_path(&delta.path.display().to_string()))
        .collect())
}

fn workspace_path_for_project_file(work_view: &WorkView, relative: &Path) -> String {
    normalize_workspace_path(
        &PathBuf::from(normalize_workspace_path(&work_view.project_path))
            .join(relative)
            .display()
            .to_string(),
    )
}

fn clean_accept_policy(
    store: &MetadataStore,
    workspace_root: &Path,
    workspace_id: &bowline_core::ids::WorkspaceId,
    workspace_path: &str,
    source: Option<&Path>,
) -> Result<crate::policy::PathPolicyDecision, WorkViewError> {
    if let Some(observed) = store.observed_path(workspace_id, workspace_path)? {
        return Ok(crate::policy::PathPolicyDecision {
            classification: observed.classification,
            mode: observed.mode,
            access: observed.access,
            matched_rule: observed.matched_rule,
            rule_source: observed.rule_source,
            risk: observed.risk,
            summary: observed.summary,
        });
    }
    let policy = UserPolicy::load_for_path(workspace_root, workspace_path)?;
    let byte_len = source
        .map(fs::metadata)
        .transpose()?
        .map(|metadata| metadata.len());
    Ok(classify_path(
        &PathFacts {
            relative_path: workspace_path.to_string(),
            is_dir: false,
            byte_len,
        },
        &policy,
    ))
}

fn is_clean_accept_policy_eligible(
    classification: PathClassification,
    mode: MaterializationMode,
) -> bool {
    matches!(
        (classification, mode),
        (PathClassification::WorkspaceSync, _)
            | (PathClassification::LargeFile, MaterializationMode::Lazy)
    )
}

fn is_ignored_clean_accept_policy(
    classification: PathClassification,
    mode: MaterializationMode,
) -> bool {
    matches!(
        (classification, mode),
        (
            PathClassification::Generated
                | PathClassification::Dependency
                | PathClassification::Cache
                | PathClassification::LocalOnly,
            MaterializationMode::LocalRegenerate
                | MaterializationMode::LocalCache
                | MaterializationMode::Ignore
                | MaterializationMode::LocalOnly
        )
    )
}

fn work_view_deletions(
    store: &MetadataStore,
    work_view: &WorkView,
) -> Result<Vec<PathBuf>, WorkViewError> {
    let visible_prefix = normalize_workspace_path(
        &store.workspace_relative_path(&work_view.workspace_id, &work_view.visible_path)?,
    );
    let work_root = expand_display_path(&work_view.visible_path);
    let mut final_delete_state = BTreeMap::<PathBuf, bool>::new();
    for write in store.local_write_log(&work_view.workspace_id)? {
        let path = normalize_workspace_path(
            &store.workspace_relative_path(&work_view.workspace_id, &write.path)?,
        );
        let Some(relative) = relative_to_work_view(&path, &visible_prefix) else {
            continue;
        };
        if relative.is_empty() {
            continue;
        }
        let relative = PathBuf::from(relative);
        if is_source_control_metadata_path(&relative) {
            continue;
        }
        if matches!(write.operation.as_str(), "rename" | "renamed")
            && let Some(source_path) = write.source_path.as_deref()
        {
            let source_path = normalize_workspace_path(
                &store.workspace_relative_path(&work_view.workspace_id, source_path)?,
            );
            let source_relative = relative_to_work_view(&source_path, &visible_prefix)
                .filter(|relative| !relative.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(source_path));
            if !is_source_control_metadata_path(&source_relative) {
                final_delete_state.insert(source_relative, true);
            }
        }
        if matches!(write.operation.as_str(), "delete" | "deleted") {
            final_delete_state.insert(relative, true);
        } else {
            final_delete_state.insert(relative, false);
        }
    }
    for (relative, _hash) in store.work_view_base_files(&work_view.workspace_id, &work_view.id)? {
        let relative = PathBuf::from(relative);
        if is_source_control_metadata_path(&relative) {
            continue;
        }
        if !work_root.join(&relative).exists() {
            final_delete_state.insert(relative, true);
        }
    }
    let mut deletes = final_delete_state
        .into_iter()
        .filter_map(|(path, is_deleted)| is_deleted.then_some(path))
        .collect::<Vec<_>>();
    deletes.sort();
    deletes.dedup();
    Ok(deletes)
}

fn relative_to_work_view<'a>(path: &'a str, visible_prefix: &str) -> Option<&'a str> {
    if path == visible_prefix {
        return Some("");
    }
    path.strip_prefix(visible_prefix)
        .and_then(|relative| relative.strip_prefix('/'))
}

fn work_view_base_has_path(
    store: &MetadataStore,
    work_view: &WorkView,
    relative: &Path,
) -> Result<bool, WorkViewError> {
    let relative_path = normalize_workspace_path(&relative.display().to_string());
    Ok(store
        .work_view_base_hash(&work_view.workspace_id, &work_view.id, &relative_path)?
        .is_some())
}

fn main_matches_work_view_base(
    store: &MetadataStore,
    work_view: &WorkView,
    relative: &Path,
    main_file: &Path,
) -> Result<bool, WorkViewError> {
    let relative_path = normalize_workspace_path(&relative.display().to_string());
    let Some(base_hash) =
        store.work_view_base_hash(&work_view.workspace_id, &work_view.id, &relative_path)?
    else {
        return Ok(false);
    };
    if !main_file.is_file() {
        return Ok(false);
    }
    Ok(file_content_hash(main_file)? == base_hash)
}

fn collect_work_view_base_files(
    store: &MetadataStore,
    work_view: &WorkView,
) -> Result<Vec<(String, String)>, WorkViewError> {
    let Some(main_root) = main_project_root(store, work_view)? else {
        return Ok(Vec::new());
    };
    let mut files = Vec::new();
    collect_base_file_hashes(&main_root, &main_root, &mut files)?;
    files.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(files)
}

fn collect_base_file_hashes(
    root: &Path,
    path: &Path,
    files: &mut Vec<(String, String)>,
) -> Result<(), WorkViewError> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if is_bowline_owned_namespace(relative) {
            continue;
        }
        if is_secret_bearing_work_path(relative) || is_source_control_metadata_path(relative) {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            collect_base_file_hashes(root, &path, files)?;
        } else if metadata.is_file() {
            files.push((
                normalize_workspace_path(&relative.display().to_string()),
                file_content_hash(&path)?,
            ));
        }
    }
    Ok(())
}

fn is_bowline_owned_namespace(relative: &Path) -> bool {
    matches!(
        relative.components().next(),
        Some(Component::Normal(name)) if name.to_str() == Some(".work")
    )
}

fn file_content_hash(path: &Path) -> Result<String, WorkViewError> {
    Ok(format!("b3_{}", blake3::hash(&fs::read(path)?).to_hex()))
}

fn is_secret_bearing_work_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(".env"))
}

fn is_source_control_metadata_path(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component,
            Component::Normal(name)
                if matches!(name.to_str(), Some(".git" | ".jj" | ".hg" | ".svn"))
        )
    })
}

fn main_project_root(
    store: &MetadataStore,
    work_view: &WorkView,
) -> Result<Option<PathBuf>, WorkViewError> {
    let Some(root) = store.current_workspace_root()? else {
        return Ok(None);
    };
    Ok(Some(
        expand_display_path(root).join(normalize_workspace_path(&work_view.project_path)),
    ))
}

fn work_namespace_root(
    store: &MetadataStore,
    work_view: &WorkView,
) -> Result<Option<PathBuf>, WorkViewError> {
    let Some(root) = store.current_workspace_root()? else {
        return Ok(None);
    };
    Ok(Some(
        expand_display_path(root)
            .join(".work")
            .join(normalize_workspace_path(&work_view.project_path)),
    ))
}

fn ensure_path_inside(path: &Path, root: &Path, reason: &'static str) -> Result<(), WorkViewError> {
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(WorkViewError::UnsafeWorkViewPath {
            path: path.display().to_string(),
            reason,
        });
    }
    if path.starts_with(root) {
        return Ok(());
    }
    Err(WorkViewError::UnsafeWorkViewPath {
        path: path.display().to_string(),
        reason,
    })
}

fn ensure_existing_path_inside_real(
    path: &Path,
    root: &Path,
    reason: &'static str,
) -> Result<(), WorkViewError> {
    let canonical_path = fs::canonicalize(path)?;
    let canonical_root = fs::canonicalize(root)?;
    if canonical_path.starts_with(&canonical_root) {
        return Ok(());
    }
    Err(WorkViewError::UnsafeWorkViewPath {
        path: path.display().to_string(),
        reason,
    })
}

fn ensure_no_symlink_ancestors(
    path: &Path,
    root: &Path,
    reason: &'static str,
) -> Result<(), WorkViewError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| WorkViewError::UnsafeWorkViewPath {
            path: path.display().to_string(),
            reason,
        })?;
    let mut current = root.to_path_buf();
    for component in relative {
        current.push(component);
        if let Ok(metadata) = fs::symlink_metadata(&current)
            && metadata.file_type().is_symlink()
        {
            return Err(WorkViewError::UnsafeWorkViewPath {
                path: current.display().to_string(),
                reason,
            });
        }
    }
    Ok(())
}

fn files_under(root: &Path) -> Result<Vec<PathBuf>, WorkViewError> {
    let mut files = Vec::new();
    collect_files(root, root, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_files(root: &Path, path: &Path, files: &mut Vec<PathBuf>) -> Result<(), WorkViewError> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if is_source_control_metadata_path(relative) {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(WorkViewError::UnsafeWorkViewPath {
                path: path.display().to_string(),
                reason: "symlinks are not followed in work views",
            });
        }
        if metadata.is_dir() {
            collect_files(root, &path, files)?;
        } else if metadata.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

fn ensure_fresh_materialization_path(path: &Path) -> Result<(), WorkViewError> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(WorkViewError::UnsafeWorkViewPath {
            path: path.display().to_string(),
            reason: "work view materialization path already exists",
        });
    }
    if fs::read_dir(path)?.next().is_some() {
        return Err(WorkViewError::UnsafeWorkViewPath {
            path: path.display().to_string(),
            reason: "work view materialization path is not empty",
        });
    }
    Ok(())
}

fn remove_materialization_tree(path: &Path) {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && metadata.is_dir()
        && !metadata.file_type().is_symlink()
    {
        let _ = fs::remove_dir_all(path);
    }
}

fn status_for_changes(changes: &[WorkDiffEntry]) -> WorkspaceStatus {
    if changes.iter().any(|change| {
        matches!(
            change.kind,
            WorkDiffChangeKind::Conflict | WorkDiffChangeKind::PolicyReview
        )
    }) {
        return WorkspaceStatus {
            level: StatusLevel::Attention,
            attention_items: vec!["Work view has changes that need review.".to_string()],
        };
    }
    WorkspaceStatus::healthy()
}

fn status_for_work_views(work_views: &[WorkView]) -> WorkspaceStatus {
    let attention_items = work_views
        .iter()
        .filter(|work_view| {
            matches!(work_view.lifecycle, WorkViewLifecycle::ReviewReady)
                || matches!(
                    work_view.sync_state,
                    WorkViewSyncState::Attention | WorkViewSyncState::Conflicted
                )
                || !work_view.attention.is_empty()
        })
        .map(|work_view| {
            format!(
                "{} is {}; review before accepting.",
                work_view.name,
                serde_json::to_value(work_view.lifecycle)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_string))
                    .unwrap_or_else(|| "attention".to_string())
            )
        })
        .collect::<Vec<_>>();
    if attention_items.is_empty() {
        WorkspaceStatus::healthy()
    } else {
        WorkspaceStatus {
            level: StatusLevel::Attention,
            attention_items,
        }
    }
}

fn open_store(db_path: Option<&Path>) -> Result<MetadataStore, WorkViewError> {
    let path = match db_path {
        Some(path) => path.to_path_buf(),
        None => default_database_path().map_err(|_| WorkViewError::MissingMetadataDb)?,
    };
    MetadataStore::open(path).map_err(Into::into)
}

fn validate_work_view_name(name: &str) -> Result<(), WorkViewError> {
    let invalid = |reason| WorkViewError::InvalidName {
        name: name.to_string(),
        reason,
    };
    if name.is_empty() {
        return Err(invalid("name must not be empty"));
    }
    if name == "." || name == ".." || name == ".work" {
        return Err(invalid("reserved name"));
    }
    if name.starts_with('.') {
        return Err(invalid("hidden names are reserved for bowline metadata"));
    }
    if name.contains('/') || name.contains('\\') {
        return Err(invalid(
            "use a short branch-like name without path separators",
        ));
    }
    if name
        .chars()
        .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(invalid(
            "use hyphens instead of whitespace or control characters",
        ));
    }
    Ok(())
}

fn visible_path(root: &str, project_path: &str, name: &str) -> PathBuf {
    expand_display_path(root)
        .join(".work")
        .join(normalize_workspace_path(project_path))
        .join(name)
}

fn display_path(path: &Path) -> String {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return path.display().to_string();
    };
    let Ok(relative) = path.strip_prefix(&home) else {
        return path.display().to_string();
    };
    if relative.as_os_str().is_empty() {
        "~".to_string()
    } else {
        format!("~/{}", relative.display())
    }
}

pub(crate) fn expand_display_path(path: impl AsRef<str>) -> PathBuf {
    let path = path.as_ref();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        if path == "~" {
            return home;
        }
        if let Some(relative) = path.strip_prefix("~/") {
            return home.join(relative);
        }
    }
    PathBuf::from(path)
}

fn work_view_id(workspace_id: &str, project_id: &str, name: &str) -> WorkViewId {
    let input = format!("{workspace_id}:{project_id}:{name}");
    WorkViewId::new(format!(
        "work_{}",
        &blake3::hash(input.as_bytes()).to_hex()[..16]
    ))
}

fn append_work_event(
    store: &MetadataStore,
    name: EventName,
    work_view: &WorkView,
    generated_at: &str,
) {
    let mut event = WorkspaceEvent::new(
        event_id(name, work_view.id.as_str(), generated_at),
        name,
        generated_at,
        work_event_severity(name),
        format!("Work view {} {}", work_view.name, event_verb(name)),
        work_view.workspace_id.clone(),
    );
    event.project_id = Some(work_view.project_id.clone());
    event.path = Some(work_view.visible_path.clone());
    event.subject = Some(EventSubject {
        kind: EventSubjectKind::WorkView,
        id: work_view.id.as_str().to_string(),
        path: Some(work_view.visible_path.clone()),
    });
    event.payload.insert(
        "name".to_string(),
        serde_json::Value::String(work_view.name.clone()),
    );
    let _ = store.append_event(event);
}

fn append_workspace_event(
    store: &MetadataStore,
    name: EventName,
    workspace_id: &bowline_core::ids::WorkspaceId,
    generated_at: &str,
    summary: &str,
) {
    let event = WorkspaceEvent::new(
        event_id(name, workspace_id.as_str(), generated_at),
        name,
        generated_at,
        EventSeverity::Info,
        summary,
        workspace_id.clone(),
    );
    let _ = store.append_event(event);
}

fn event_id(name: EventName, subject: &str, generated_at: &str) -> EventId {
    let input = format!("{name:?}:{subject}:{generated_at}");
    EventId::new(format!(
        "evt_work_{}",
        &blake3::hash(input.as_bytes()).to_hex()[..16]
    ))
}

fn event_verb(name: EventName) -> &'static str {
    match name {
        EventName::WorkCreated => "created",
        EventName::WorkAccepted => "accepted",
        EventName::WorkDiscarded => "discarded",
        EventName::WorkRestored => "restored",
        _ => "updated",
    }
}

fn work_event_severity(name: EventName) -> EventSeverity {
    match name {
        EventName::WorkReviewReady => EventSeverity::Attention,
        _ => EventSeverity::Info,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use bowline_control_plane::{
        ControlPlaneClient, ControlPlaneTimestamp, FakeControlPlaneClient,
        ObjectKind as ControlObjectKind, ObjectManifestCommit, ObjectPointer, UploadIntentRequest,
        WorkViewCreate, WorkViewOverlayCommit, WorkspaceRef,
    };
    use bowline_core::{
        commands::{AgentLeaseBase, AgentLeaseOutputState},
        ids::{DeviceId, ProjectId, SnapshotId, WorkspaceId},
        policy::PathClassification,
        status::StatusLevel,
        work_views::{
            WorkDiffChangeKind, WorkViewLifecycle, WorkViewSyncState, WorkViewVisibility,
        },
    };

    use crate::{
        agents::{AgentLeaseCreateOptions, create_agent_lease},
        metadata::{LocalWriteLogRecord, MetadataStore, WorkspaceSyncHeadRecord},
        status::{StatusOptions, compose_status},
        workspace::TempWorkspace,
    };

    use super::{
        WorkCleanupOptions, WorkListOptions, WorkSelectorOptions, WorkViewError,
        WorkViewOverlaySyncOptions, WorkonOptions, accept_work_view, cleanup_work_views,
        create_work_view, diff_work_view, discard_work_view, list_work_views,
        overlay_delta_kind_name, overlay_deltas_for_upload, restore_work_view,
        sync_local_work_view_overlays,
    };
    use bowline_storage::{
        LocalByteStore, ObjectKind as StorageObjectKind, RetentionState as StorageRetentionState,
        StorageKey,
    };

    #[test]
    fn workon_materializes_project_files_without_secrets_or_source_control_metadata() {
        let (temp, db_path) = seeded_store("phase9-workon");
        let project_path = temp.root().join("Code").join("apps/web");
        fs::create_dir_all(project_path.join("src")).expect("project src");
        fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("source file");
        fs::write(project_path.join(".env.local"), "TOKEN=secret").expect("env file");
        fs::create_dir_all(project_path.join(".git")).expect("git dir");
        fs::write(project_path.join(".git/config"), "[core]\n").expect("git config");

        let output = create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "auth-fix".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("work view");

        assert_eq!(output.work_view.name, "auth-fix");
        let materialized = temp.root().join("Code/.work/apps/web/auth-fix");
        assert!(materialized.is_dir());
        assert_eq!(
            fs::read_to_string(materialized.join("src/index.ts")).expect("copied source"),
            "console.log('base')"
        );
        assert!(!materialized.join(".env.local").exists());
        assert!(!materialized.join(".git/config").exists());
        assert_eq!(
            output.work_view.host_materializations,
            vec![display(&materialized)]
        );

        let diff = diff_work_view(WorkSelectorOptions {
            db_path: Some(db_path.clone()),
            selector: materialized.join("src").display().to_string(),
            generated_at: now(),
        })
        .expect("diff from inside work view");
        assert_eq!(diff.work_view.id, output.work_view.id);

        let sibling = temp.root().join("Code/.work/apps/web/auth-fix-old");
        fs::create_dir_all(&sibling).expect("sibling prefix path");
        let error = diff_work_view(WorkSelectorOptions {
            db_path: Some(db_path.clone()),
            selector: sibling.display().to_string(),
            generated_at: now(),
        })
        .expect_err("sibling prefix is not inside work view");
        assert!(matches!(error, WorkViewError::MissingWorkView { .. }));

        let escaped_sibling = materialized.join("../auth-fix-old");
        let error = diff_work_view(WorkSelectorOptions {
            db_path: Some(db_path),
            selector: escaped_sibling.display().to_string(),
            generated_at: now(),
        })
        .expect_err("parent traversal selector is not inside work view");
        assert!(matches!(error, WorkViewError::MissingWorkView { .. }));
    }

    #[test]
    fn workon_requires_latest_project_snapshot_before_materializing() {
        let (temp, db_path) = seeded_store_without_snapshot("phase9-workon-empty-base");
        let project_path = temp.root().join("Code").join("apps/web");

        let error = create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "first-work".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect_err("missing base should block work view");

        assert!(matches!(error, WorkViewError::MissingBaseSnapshot { .. }));
        assert!(!temp.root().join("Code/.work/apps/web/first-work").exists());

        let store = MetadataStore::open(&db_path).expect("metadata");
        assert!(
            store
                .work_views(&WorkspaceId::new("ws_code"), true, None)
                .expect("work views")
                .is_empty()
        );
    }

    #[test]
    fn workon_refuses_project_with_pending_local_writes() {
        let (temp, db_path) = seeded_store("phase9-workon-dirty-project");
        let project_path = temp.root().join("Code").join("apps/web");
        fs::create_dir_all(project_path.join("src")).expect("project src");
        fs::write(project_path.join("src/index.ts"), "console.log('dirty')").expect("dirty file");
        let store = MetadataStore::open(&db_path).expect("metadata");
        store
            .append_local_write_log(&LocalWriteLogRecord {
                id: "write-dirty-project".to_string(),
                workspace_id: WorkspaceId::new("ws_code"),
                device_id: DeviceId::new("device-1"),
                project_id: Some(ProjectId::new("proj_web")),
                path: display(&project_path.join("src/index.ts")),
                source_path: None,
                operation: "modify".to_string(),
                staged_content_id: None,
                policy_classification: PathClassification::WorkspaceSync,
                causation_id: "test".to_string(),
                settled_at: now(),
                created_at: now(),
            })
            .expect("write log");
        drop(store);

        let error = create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "dirty-base".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect_err("dirty project should not become work-view base");

        assert!(matches!(error, WorkViewError::DirtyProject { .. }));
        assert!(!temp.root().join("Code/.work/apps/web/dirty-base").exists());
        let store = MetadataStore::open(&db_path).expect("metadata");
        assert!(
            store
                .work_views(&WorkspaceId::new("ws_code"), true, None)
                .expect("work views")
                .is_empty()
        );
    }

    #[test]
    fn workon_allows_historical_writes_before_synced_head() {
        let (temp, db_path) = seeded_store("phase9-workon-historical-write");
        let project_path = temp.root().join("Code").join("apps/web");
        fs::create_dir_all(project_path.join("src")).expect("project src");
        fs::write(project_path.join("src/index.ts"), "console.log('synced')").expect("synced file");
        let store = MetadataStore::open(&db_path).expect("metadata");
        store
            .append_local_write_log(&LocalWriteLogRecord {
                id: "write-synced-project".to_string(),
                workspace_id: WorkspaceId::new("ws_code"),
                device_id: DeviceId::new("device-1"),
                project_id: Some(ProjectId::new("proj_web")),
                path: display(&project_path.join("src/index.ts")),
                source_path: None,
                operation: "modify".to_string(),
                staged_content_id: None,
                policy_classification: PathClassification::WorkspaceSync,
                causation_id: "test".to_string(),
                settled_at: "2026-06-25T01:00:00Z".to_string(),
                created_at: "2026-06-25T01:00:00Z".to_string(),
            })
            .expect("write log");
        store
            .upsert_workspace_sync_head(&WorkspaceSyncHeadRecord {
                workspace_ref: WorkspaceRef {
                    workspace_id: "ws_code".to_string(),
                    version: 1,
                    snapshot_id: "snap_project_base".to_string(),
                    updated_at: ControlPlaneTimestamp { tick: 1 },
                    updated_by_device_id: Some("device-1".to_string()),
                },
                observed_at: "2026-06-25T01:01:00Z".to_string(),
            })
            .expect("synced head");
        drop(store);

        let output = create_work_view(WorkonOptions {
            db_path: Some(db_path),
            project_path: project_path.display().to_string(),
            name: "after-sync".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("historical write should not block work view");

        assert_eq!(output.work_view.name, "after-sync");
        assert!(temp.root().join("Code/.work/apps/web/after-sync").exists());
    }

    #[test]
    fn workon_ignores_project_root_modify_noise_after_synced_head() {
        let (temp, db_path) = seeded_store("phase9-workon-project-root-noise");
        let project_path = temp.root().join("Code").join("apps/web");
        fs::create_dir_all(project_path.join("src")).expect("project src");
        fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");
        let store = MetadataStore::open(&db_path).expect("metadata");
        store
            .append_local_write_log(&LocalWriteLogRecord {
                id: "write-project-root-noise".to_string(),
                workspace_id: WorkspaceId::new("ws_code"),
                device_id: DeviceId::new("device-1"),
                project_id: None,
                path: "apps/web".to_string(),
                source_path: None,
                operation: "modify".to_string(),
                staged_content_id: None,
                policy_classification: PathClassification::WorkspaceSync,
                causation_id: "test".to_string(),
                settled_at: "2026-06-25T01:02:00Z".to_string(),
                created_at: "2026-06-25T01:02:00Z".to_string(),
            })
            .expect("write log");
        store
            .upsert_workspace_sync_head(&WorkspaceSyncHeadRecord {
                workspace_ref: WorkspaceRef {
                    workspace_id: "ws_code".to_string(),
                    version: 1,
                    snapshot_id: "snap_project_base".to_string(),
                    updated_at: ControlPlaneTimestamp { tick: 1 },
                    updated_by_device_id: Some("device-1".to_string()),
                },
                observed_at: "2026-06-25T01:01:00Z".to_string(),
            })
            .expect("synced head");
        drop(store);

        let output = create_work_view(WorkonOptions {
            db_path: Some(db_path),
            project_path: project_path.display().to_string(),
            name: "directory-noise".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("project root directory noise should not block work view");

        assert_eq!(output.work_view.name, "directory-noise");
        assert!(
            temp.root()
                .join("Code/.work/apps/web/directory-noise")
                .exists()
        );
    }

    #[test]
    fn workon_ignores_pending_writes_inside_other_work_views() {
        let (temp, db_path) = seeded_store("phase9-workon-work-namespace-write");
        let project_path = temp.root().join("Code").join("apps/web");
        fs::create_dir_all(project_path.join("src")).expect("project src");
        fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "first".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("first work view");
        let first_work_file = temp.root().join("Code/.work/apps/web/first/src/index.ts");
        fs::write(&first_work_file, "console.log('overlay')").expect("overlay edit");
        let store = MetadataStore::open(&db_path).expect("metadata");
        store
            .append_local_write_log(&LocalWriteLogRecord {
                id: "write-first-work-view".to_string(),
                workspace_id: WorkspaceId::new("ws_code"),
                device_id: DeviceId::new("device-1"),
                project_id: Some(ProjectId::new("proj_web")),
                path: display(&first_work_file),
                source_path: None,
                operation: "modify".to_string(),
                staged_content_id: None,
                policy_classification: PathClassification::WorkspaceSync,
                causation_id: "test".to_string(),
                settled_at: now(),
                created_at: now(),
            })
            .expect("work-view write log");
        drop(store);

        let second = create_work_view(WorkonOptions {
            db_path: Some(db_path),
            project_path: project_path.display().to_string(),
            name: "second".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("work-view overlay writes should not dirty main project");

        assert_eq!(second.work_view.name, "second");
        assert!(temp.root().join("Code/.work/apps/web/second").exists());
    }

    #[test]
    fn sync_uploads_changed_work_view_overlay_once() {
        let (temp, db_path) = seeded_store("phase9-work-overlay-sync");
        let project_path = temp.root().join("Code").join("apps/web");
        fs::create_dir_all(project_path.join("src")).expect("project src");
        fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

        let output = create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "remote-edit".to_string(),
            owner_device_id: Some(DeviceId::new("device-1")),
            generated_at: now(),
        })
        .expect("work view");
        let materialized = temp.root().join("Code/.work/apps/web/remote-edit");
        fs::write(materialized.join("src/index.ts"), "console.log('overlay')")
            .expect("overlay edit");

        let control_plane = FakeControlPlaneClient::default();
        control_plane.create_workspace("ws_code");
        let workspace_ref = control_plane
            .compare_and_swap_workspace_ref("ws_code", 0, "snap_project_base", "device-1")
            .expect("base ref");
        let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 91)
            .expect("byte store");
        let storage_key = StorageKey::deterministic(91);

        let report = sync_local_work_view_overlays(
            WorkViewOverlaySyncOptions {
                db_path: db_path.clone(),
                device_id: DeviceId::new("device-1"),
                storage_key,
                key_epoch: 1,
                generated_at: now(),
            },
            &control_plane,
            &byte_store,
            &workspace_ref,
        )
        .expect("overlay sync");

        assert_eq!(report.uploaded, 1);
        assert_eq!(report.attention, 0);
        let remote = control_plane
            .list_work_views("ws_code", true)
            .expect("remote work views")
            .into_iter()
            .find(|view| view.work_view_id == output.work_view.id.as_str())
            .expect("remote work view");
        assert_eq!(remote.name, output.work_view.id.as_str());
        assert_eq!(
            remote.visible_path,
            format!(".work/{}", output.work_view.id.as_str())
        );
        let overlay = remote.overlay_head.expect("overlay head");
        assert_eq!(overlay.kind, ControlObjectKind::AgentOverlay);
        assert!(overlay.object_key.starts_with("packs_pk_"));
        assert!(!overlay.object_key.contains("apps"));
        assert!(!overlay.object_key.contains("remote-edit"));
        assert_eq!(
            control_plane
                .head_object_metadata("ws_code", &overlay.object_key)
                .expect("overlay metadata")
                .kind,
            StorageObjectKind::AgentOverlay
        );

        let store = MetadataStore::open(&db_path).expect("metadata");
        let mut synced = store
            .work_view_by_id(&WorkspaceId::new("ws_code"), &output.work_view.id)
            .expect("work view lookup")
            .expect("local work view");
        assert!(synced.overlay_head.starts_with("b3_"));
        assert_eq!(synced.overlay_version, 1);
        assert_eq!(synced.sync_state, WorkViewSyncState::Synced);
        synced.overlay_head = "overlay_empty".to_string();
        synced.overlay_version = 0;
        store
            .upsert_work_view(&synced)
            .expect("simulate crash before local overlay state persisted");
        drop(store);

        let retry = sync_local_work_view_overlays(
            WorkViewOverlaySyncOptions {
                db_path: db_path.clone(),
                device_id: DeviceId::new("device-1"),
                storage_key,
                key_epoch: 1,
                generated_at: now(),
            },
            &control_plane,
            &byte_store,
            &workspace_ref,
        )
        .expect("retry reconciles already committed overlay object metadata");
        assert_eq!(retry.uploaded, 0);
        assert_eq!(retry.attention, 0);

        let idle = sync_local_work_view_overlays(
            WorkViewOverlaySyncOptions {
                db_path: db_path.clone(),
                device_id: DeviceId::new("device-1"),
                storage_key,
                key_epoch: 1,
                generated_at: now(),
            },
            &control_plane,
            &byte_store,
            &workspace_ref,
        )
        .expect("second overlay sync");
        assert_eq!(idle.uploaded, 0);
        assert_eq!(idle.attention, 0);

        let store = MetadataStore::open(&db_path).expect("metadata");
        let synced_before_revert = store
            .work_view_by_id(&WorkspaceId::new("ws_code"), &output.work_view.id)
            .expect("work view lookup")
            .expect("local work view");
        let previous_overlay_head = synced_before_revert.overlay_head.clone();
        drop(store);
        fs::write(materialized.join("src/index.ts"), "console.log('base')")
            .expect("revert overlay to base");
        let reverted = sync_local_work_view_overlays(
            WorkViewOverlaySyncOptions {
                db_path: db_path.clone(),
                device_id: DeviceId::new("device-1"),
                storage_key,
                key_epoch: 1,
                generated_at: now(),
            },
            &control_plane,
            &byte_store,
            &workspace_ref,
        )
        .expect("revert uploads empty overlay");
        assert_eq!(reverted.uploaded, 1);
        assert_eq!(reverted.attention, 0);
        let store = MetadataStore::open(&db_path).expect("metadata");
        let reverted_local = store
            .work_view_by_id(&WorkspaceId::new("ws_code"), &output.work_view.id)
            .expect("work view lookup")
            .expect("local work view");
        assert_ne!(reverted_local.overlay_head, previous_overlay_head);
        assert_eq!(reverted_local.sync_state, WorkViewSyncState::Synced);
    }

    #[test]
    fn sync_publishes_empty_work_view_metadata_without_uploading_bytes() {
        let (temp, db_path) = seeded_store("phase9-empty-work-view-publish");
        let project_path = temp.root().join("Code").join("apps/web");
        fs::create_dir_all(project_path.join("src")).expect("project src");
        fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

        let output = create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "empty-remote".to_string(),
            owner_device_id: Some(DeviceId::new("device-1")),
            generated_at: now(),
        })
        .expect("work view");

        let control_plane = FakeControlPlaneClient::default();
        control_plane.create_workspace("ws_code");
        let workspace_ref = control_plane
            .compare_and_swap_workspace_ref("ws_code", 0, "snap_project_base", "device-1")
            .expect("base ref");
        let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 97)
            .expect("byte store");

        let report = sync_local_work_view_overlays(
            WorkViewOverlaySyncOptions {
                db_path: db_path.clone(),
                device_id: DeviceId::new("device-1"),
                storage_key: StorageKey::deterministic(97),
                key_epoch: 1,
                generated_at: now(),
            },
            &control_plane,
            &byte_store,
            &workspace_ref,
        )
        .expect("empty work view metadata sync");

        assert_eq!(report.uploaded, 0);
        assert_eq!(report.attention, 0);
        assert!(
            control_plane.object_pointers("ws_code").is_empty(),
            "metadata-only publication must not upload an overlay pack"
        );
        let remote = control_plane
            .list_work_views("ws_code", true)
            .expect("remote work views")
            .into_iter()
            .find(|view| view.work_view_id == output.work_view.id.as_str())
            .expect("empty work view should be remotely visible");
        assert!(remote.overlay_head.is_none());
        assert_eq!(remote.name, output.work_view.id.as_str());
        assert_eq!(
            remote.visible_path,
            format!(".work/{}", output.work_view.id.as_str())
        );

        let store = MetadataStore::open(&db_path).expect("metadata");
        let synced = store
            .work_view_by_id(&WorkspaceId::new("ws_code"), &output.work_view.id)
            .expect("work view lookup")
            .expect("local work view");
        assert_eq!(synced.sync_state, WorkViewSyncState::Synced);
        drop(store);

        let idle = sync_local_work_view_overlays(
            WorkViewOverlaySyncOptions {
                db_path,
                device_id: DeviceId::new("device-1"),
                storage_key: StorageKey::deterministic(97),
                key_epoch: 1,
                generated_at: now(),
            },
            &control_plane,
            &byte_store,
            &workspace_ref,
        )
        .expect("idle empty work view stays quiet");
        assert_eq!(idle.uploaded, 0);
        assert_eq!(idle.attention, 0);
    }

    #[test]
    fn sync_ignores_dependency_artifacts_in_work_view_overlay() {
        let (temp, db_path) = seeded_store("phase9-work-overlay-policy-ignore");
        let project_path = temp.root().join("Code").join("apps/web");
        fs::create_dir_all(project_path.join("src")).expect("project src");
        fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

        let output = create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "deps-only".to_string(),
            owner_device_id: Some(DeviceId::new("device-1")),
            generated_at: now(),
        })
        .expect("work view");
        let dependency_file = temp
            .root()
            .join("Code/.work/apps/web/deps-only/node_modules/lodash/index.js");
        fs::create_dir_all(dependency_file.parent().expect("dependency parent"))
            .expect("dependency dir");
        fs::write(&dependency_file, "module.exports = {}\n").expect("dependency artifact");

        let control_plane = FakeControlPlaneClient::default();
        control_plane.create_workspace("ws_code");
        let workspace_ref = control_plane
            .compare_and_swap_workspace_ref("ws_code", 0, "snap_project_base", "device-1")
            .expect("base ref");
        let byte_store =
            LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 102)
                .expect("byte store");

        let report = sync_local_work_view_overlays(
            WorkViewOverlaySyncOptions {
                db_path: db_path.clone(),
                device_id: DeviceId::new("device-1"),
                storage_key: StorageKey::deterministic(102),
                key_epoch: 1,
                generated_at: now(),
            },
            &control_plane,
            &byte_store,
            &workspace_ref,
        )
        .expect("dependency-only work view sync");

        assert_eq!(report.uploaded, 0);
        assert_eq!(report.attention, 0);
        assert!(
            control_plane.object_pointers("ws_code").is_empty(),
            "dependency artifacts must not be packaged into overlay packs"
        );
        let remote = control_plane
            .list_work_views("ws_code", true)
            .expect("remote work views")
            .into_iter()
            .find(|view| view.work_view_id == output.work_view.id.as_str())
            .expect("dependency-only work view should still publish metadata");
        assert!(remote.overlay_head.is_none());
    }

    #[test]
    fn sync_marks_empty_local_work_view_attention_when_remote_overlay_exists() {
        let (temp, db_path) = seeded_store("phase9-empty-local-remote-overlay");
        let project_path = temp.root().join("Code").join("apps/web");
        fs::create_dir_all(project_path.join("src")).expect("project src");
        fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

        let output = create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "remote-has-work".to_string(),
            owner_device_id: Some(DeviceId::new("device-1")),
            generated_at: now(),
        })
        .expect("work view");

        let control_plane = FakeControlPlaneClient::default();
        control_plane.create_workspace("ws_code");
        let workspace_ref = control_plane
            .compare_and_swap_workspace_ref("ws_code", 0, "snap_project_base", "device-1")
            .expect("base ref");
        control_plane
            .create_work_view(WorkViewCreate {
                workspace_id: "ws_code".to_string(),
                work_view_id: output.work_view.id.as_str().to_string(),
                project_id: "proj_web".to_string(),
                name: output.work_view.id.as_str().to_string(),
                visible_path: format!(".work/{}", output.work_view.id.as_str()),
                base_snapshot_id: "snap_project_base".to_string(),
                base_workspace_version: workspace_ref.version,
                created_by_device_id: "device-1".to_string(),
            })
            .expect("remote work view");
        let remote_overlay = ObjectPointer {
            object_key: "packs_pk_0011223344556677".to_string(),
            content_id: "pack_remote_overlay".to_string(),
            byte_len: 16,
            hash: "b3_remote_overlay".to_string(),
            key_epoch: 1,
            kind: ControlObjectKind::AgentOverlay,
            created_at: ControlPlaneTimestamp { tick: 7 },
        };
        control_plane.put_object_pointer("ws_code", remote_overlay.clone());
        control_plane
            .commit_work_view_overlay(WorkViewOverlayCommit {
                workspace_id: "ws_code".to_string(),
                work_view_id: output.work_view.id.as_str().to_string(),
                expected_overlay_version: 0,
                overlay_object: remote_overlay,
                committed_by_device_id: "device-1".to_string(),
            })
            .expect("remote overlay commit");
        let byte_store =
            LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 101)
                .expect("byte store");

        let report = sync_local_work_view_overlays(
            WorkViewOverlaySyncOptions {
                db_path: db_path.clone(),
                device_id: DeviceId::new("device-1"),
                storage_key: StorageKey::deterministic(101),
                key_epoch: 1,
                generated_at: now(),
            },
            &control_plane,
            &byte_store,
            &workspace_ref,
        )
        .expect("empty local detects remote overlay");

        assert_eq!(report.uploaded, 0);
        assert_eq!(report.attention, 1);
        let store = MetadataStore::open(&db_path).expect("metadata");
        let blocked = store
            .work_view_by_id(&WorkspaceId::new("ws_code"), &output.work_view.id)
            .expect("work view lookup")
            .expect("local work view");
        assert_eq!(blocked.sync_state, WorkViewSyncState::Attention);
        assert!(
            blocked
                .attention
                .iter()
                .any(|item| item.contains("Remote work view overlay changed"))
        );
    }

    #[test]
    fn sync_refuses_to_overwrite_unseen_remote_work_view_overlay() {
        let (temp, db_path) = seeded_store("phase9-work-overlay-stale-local");
        let project_path = temp.root().join("Code").join("apps/web");
        fs::create_dir_all(project_path.join("src")).expect("project src");
        fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

        let output = create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "stale-local".to_string(),
            owner_device_id: Some(DeviceId::new("device-1")),
            generated_at: now(),
        })
        .expect("work view");
        let local_file = temp
            .root()
            .join("Code/.work/apps/web/stale-local/src/index.ts");
        fs::write(&local_file, "console.log('local edit')").expect("local overlay edit");

        let control_plane = FakeControlPlaneClient::default();
        control_plane.create_workspace("ws_code");
        let workspace_ref = control_plane
            .compare_and_swap_workspace_ref("ws_code", 0, "snap_project_base", "device-1")
            .expect("base ref");
        control_plane
            .create_work_view(WorkViewCreate {
                workspace_id: "ws_code".to_string(),
                work_view_id: output.work_view.id.as_str().to_string(),
                project_id: "proj_web".to_string(),
                name: output.work_view.id.as_str().to_string(),
                visible_path: format!(".work/{}", output.work_view.id.as_str()),
                base_snapshot_id: "snap_project_base".to_string(),
                base_workspace_version: workspace_ref.version,
                created_by_device_id: "device-2".to_string(),
            })
            .expect("remote work view");
        let remote_overlay =
            reserve_test_overlay_object(&control_plane, "ws_code", "overlay-remote", 8);
        control_plane
            .commit_work_view_overlay(WorkViewOverlayCommit {
                workspace_id: "ws_code".to_string(),
                work_view_id: output.work_view.id.as_str().to_string(),
                expected_overlay_version: 0,
                overlay_object: remote_overlay,
                committed_by_device_id: "device-2".to_string(),
            })
            .expect("remote overlay commit");
        let object_count_before = control_plane.object_pointers("ws_code").len();
        let byte_store =
            LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 103)
                .expect("byte store");

        let report = sync_local_work_view_overlays(
            WorkViewOverlaySyncOptions {
                db_path: db_path.clone(),
                device_id: DeviceId::new("device-1"),
                storage_key: StorageKey::deterministic(103),
                key_epoch: 1,
                generated_at: now(),
            },
            &control_plane,
            &byte_store,
            &workspace_ref,
        )
        .expect("stale local overlay should not overwrite remote");

        assert_eq!(report.uploaded, 0);
        assert_eq!(report.attention, 1);
        assert_eq!(
            control_plane.object_pointers("ws_code").len(),
            object_count_before,
            "stale local overlay should not upload a replacement object"
        );
        let store = MetadataStore::open(&db_path).expect("metadata");
        let blocked = store
            .work_view_by_id(&WorkspaceId::new("ws_code"), &output.work_view.id)
            .expect("work view lookup")
            .expect("local work view");
        assert_eq!(blocked.sync_state, WorkViewSyncState::Attention);
        assert!(
            blocked
                .attention
                .iter()
                .any(|item| item.contains("last synced version 0"))
        );
    }

    #[test]
    fn sync_detects_remote_overlay_advance_even_when_local_digest_matches() {
        let (temp, db_path) = seeded_store("phase9-work-overlay-remote-advance");
        let project_path = temp.root().join("Code").join("apps/web");
        fs::create_dir_all(project_path.join("src")).expect("project src");
        fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

        let output = create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "remote-advance".to_string(),
            owner_device_id: Some(DeviceId::new("device-1")),
            generated_at: now(),
        })
        .expect("work view");
        let local_file = temp
            .root()
            .join("Code/.work/apps/web/remote-advance/src/index.ts");
        fs::write(&local_file, "console.log('local edit')").expect("local overlay edit");

        let control_plane = FakeControlPlaneClient::default();
        control_plane.create_workspace("ws_code");
        let workspace_ref = control_plane
            .compare_and_swap_workspace_ref("ws_code", 0, "snap_project_base", "device-1")
            .expect("base ref");
        let byte_store =
            LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 104)
                .expect("byte store");
        let storage_key = StorageKey::deterministic(104);
        let initial = sync_local_work_view_overlays(
            WorkViewOverlaySyncOptions {
                db_path: db_path.clone(),
                device_id: DeviceId::new("device-1"),
                storage_key,
                key_epoch: 1,
                generated_at: now(),
            },
            &control_plane,
            &byte_store,
            &workspace_ref,
        )
        .expect("initial overlay upload");
        assert_eq!(initial.uploaded, 1);

        let remote_overlay =
            reserve_test_overlay_object(&control_plane, "ws_code", "overlay-peer", 9);
        control_plane
            .commit_work_view_overlay(WorkViewOverlayCommit {
                workspace_id: "ws_code".to_string(),
                work_view_id: output.work_view.id.as_str().to_string(),
                expected_overlay_version: 1,
                overlay_object: remote_overlay,
                committed_by_device_id: "device-2".to_string(),
            })
            .expect("peer overlay commit");

        let report = sync_local_work_view_overlays(
            WorkViewOverlaySyncOptions {
                db_path: db_path.clone(),
                device_id: DeviceId::new("device-1"),
                storage_key,
                key_epoch: 1,
                generated_at: now(),
            },
            &control_plane,
            &byte_store,
            &workspace_ref,
        )
        .expect("remote overlay advance should be noticed");

        assert_eq!(report.uploaded, 0);
        assert_eq!(report.attention, 1);
        let store = MetadataStore::open(&db_path).expect("metadata");
        let blocked = store
            .work_view_by_id(&WorkspaceId::new("ws_code"), &output.work_view.id)
            .expect("work view lookup")
            .expect("local work view");
        assert_eq!(blocked.sync_state, WorkViewSyncState::Attention);
        assert_eq!(blocked.overlay_version, 1);
        assert!(
            blocked
                .attention
                .iter()
                .any(|item| item.contains("last synced version 1"))
        );
    }

    #[test]
    fn sync_publishes_local_work_view_after_main_head_advances() {
        let (temp, db_path) = seeded_store("phase9-work-overlay-historical-base");
        let project_path = temp.root().join("Code").join("apps/web");
        fs::create_dir_all(project_path.join("src")).expect("project src");
        fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

        let output = create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "historical-base".to_string(),
            owner_device_id: Some(DeviceId::new("device-1")),
            generated_at: now(),
        })
        .expect("work view");
        let materialized = temp.root().join("Code/.work/apps/web/historical-base");
        fs::write(materialized.join("src/index.ts"), "console.log('overlay')")
            .expect("overlay edit");

        let control_plane = FakeControlPlaneClient::default();
        control_plane.create_workspace("ws_code");
        let base_ref = control_plane
            .compare_and_swap_workspace_ref("ws_code", 0, "snap_project_base", "device-1")
            .expect("base ref");
        commit_test_snapshot_manifest(&control_plane, "ws_code", "snap_project_base", "device-1");
        let advanced_ref = control_plane
            .compare_and_swap_workspace_ref(
                "ws_code",
                base_ref.version,
                "snap_project_next",
                "device-1",
            )
            .expect("advanced ref");
        let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 99)
            .expect("byte store");

        let report = sync_local_work_view_overlays(
            WorkViewOverlaySyncOptions {
                db_path: db_path.clone(),
                device_id: DeviceId::new("device-1"),
                storage_key: StorageKey::deterministic(99),
                key_epoch: 1,
                generated_at: now(),
            },
            &control_plane,
            &byte_store,
            &advanced_ref,
        )
        .expect("historical-base work view should still publish");

        assert_eq!(report.uploaded, 1);
        assert_eq!(report.attention, 0);
        let remote = control_plane
            .list_work_views("ws_code", true)
            .expect("remote work views")
            .into_iter()
            .find(|view| view.work_view_id == output.work_view.id.as_str())
            .expect("remote work view");
        assert_eq!(remote.base_snapshot_id, "snap_project_base");
        assert_eq!(remote.base_workspace_version, 0);
        assert!(remote.overlay_head.is_some());
    }

    #[test]
    fn sync_does_not_upload_main_head_changes_as_work_view_overlay() {
        let (temp, db_path) = seeded_store("phase9-work-overlay-main-head-noise");
        let project_path = temp.root().join("Code").join("apps/web");
        fs::create_dir_all(project_path.join("src")).expect("project src");
        fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

        let output = create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "untouched-work".to_string(),
            owner_device_id: Some(DeviceId::new("device-1")),
            generated_at: now(),
        })
        .expect("work view");
        fs::write(
            project_path.join("src/index.ts"),
            "console.log('main advanced')",
        )
        .expect("main project edit");

        let control_plane = FakeControlPlaneClient::default();
        control_plane.create_workspace("ws_code");
        let base_ref = control_plane
            .compare_and_swap_workspace_ref("ws_code", 0, "snap_project_base", "device-1")
            .expect("base ref");
        commit_test_snapshot_manifest(&control_plane, "ws_code", "snap_project_base", "device-1");
        let advanced_ref = control_plane
            .compare_and_swap_workspace_ref(
                "ws_code",
                base_ref.version,
                "snap_project_next",
                "device-1",
            )
            .expect("advanced ref");
        let byte_store =
            LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 100)
                .expect("byte store");

        let report = sync_local_work_view_overlays(
            WorkViewOverlaySyncOptions {
                db_path,
                device_id: DeviceId::new("device-1"),
                storage_key: StorageKey::deterministic(100),
                key_epoch: 1,
                generated_at: now(),
            },
            &control_plane,
            &byte_store,
            &advanced_ref,
        )
        .expect("untouched historical work view should publish metadata only");

        assert_eq!(report.uploaded, 0);
        assert_eq!(report.attention, 0);
        assert_eq!(
            control_plane
                .object_pointers("ws_code")
                .into_iter()
                .filter(|pointer| pointer.kind == ControlObjectKind::AgentOverlay)
                .count(),
            0,
            "main project changes must not become work-view overlay payloads"
        );
        let remote = control_plane
            .list_work_views("ws_code", true)
            .expect("remote work views")
            .into_iter()
            .find(|view| view.work_view_id == output.work_view.id.as_str())
            .expect("remote work view");
        assert_eq!(remote.base_snapshot_id, "snap_project_base");
        assert!(remote.overlay_head.is_none());
    }

    #[test]
    fn sync_records_delete_when_base_file_becomes_directory() {
        let (temp, db_path) = seeded_store("phase9-work-overlay-file-to-dir");
        let project_path = temp.root().join("Code").join("apps/web");
        fs::create_dir_all(project_path.join("src")).expect("project src");
        fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "file-to-dir".to_string(),
            owner_device_id: Some(DeviceId::new("device-1")),
            generated_at: now(),
        })
        .expect("work view");
        let replaced = temp
            .root()
            .join("Code/.work/apps/web/file-to-dir/src/index.ts");
        fs::remove_file(&replaced).expect("remove base file in work view");
        fs::create_dir_all(&replaced).expect("replacement dir");
        fs::write(replaced.join("nested.ts"), "console.log('nested')").expect("nested file");

        let store = MetadataStore::open(&db_path).expect("metadata");
        let work_view = store
            .work_views(&WorkspaceId::new("ws_code"), true, None)
            .expect("work views")
            .into_iter()
            .find(|view| view.name == "file-to-dir")
            .expect("work view");
        let deltas = overlay_deltas_for_upload(&store, &work_view).expect("overlay deltas");
        let paths = deltas
            .iter()
            .map(|delta| {
                (
                    delta.path.display().to_string(),
                    overlay_delta_kind_name(&delta.kind),
                )
            })
            .collect::<Vec<_>>();

        assert!(paths.contains(&("src/index.ts".to_string(), "delete")));
        assert!(paths.contains(&("src/index.ts/nested.ts".to_string(), "create")));
    }

    #[test]
    fn sync_treats_same_object_stale_overlay_as_converged() {
        let (temp, db_path) = seeded_store("phase9-work-overlay-same-object-stale");
        let project_path = temp.root().join("Code").join("apps/web");
        fs::create_dir_all(project_path.join("src")).expect("project src");
        fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

        let output = create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "same-object".to_string(),
            owner_device_id: Some(DeviceId::new("device-1")),
            generated_at: now(),
        })
        .expect("work view");
        let materialized = temp.root().join("Code/.work/apps/web/same-object");
        fs::write(materialized.join("src/index.ts"), "console.log('overlay')")
            .expect("overlay edit");

        let control_plane = FakeControlPlaneClient::default();
        control_plane.create_workspace("ws_code");
        let workspace_ref = control_plane
            .compare_and_swap_workspace_ref("ws_code", 0, "snap_project_base", "device-1")
            .expect("base ref");
        control_plane.make_next_overlay_commit_stale_with_same_object_for_harness(
            "ws_code",
            output.work_view.id.as_str(),
        );
        let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 98)
            .expect("byte store");
        let storage_key = StorageKey::deterministic(98);

        let report = sync_local_work_view_overlays(
            WorkViewOverlaySyncOptions {
                db_path: db_path.clone(),
                device_id: DeviceId::new("device-1"),
                storage_key,
                key_epoch: 1,
                generated_at: now(),
            },
            &control_plane,
            &byte_store,
            &workspace_ref,
        )
        .expect("same-object stale overlay converges");

        assert_eq!(report.uploaded, 1);
        assert_eq!(report.attention, 0);
        let remote = control_plane
            .list_work_views("ws_code", true)
            .expect("remote work views")
            .into_iter()
            .find(|view| view.work_view_id == output.work_view.id.as_str())
            .expect("remote work view");
        let overlay = remote.overlay_head.expect("overlay head");
        assert_eq!(
            control_plane
                .head_object_metadata("ws_code", &overlay.object_key)
                .expect("overlay metadata")
                .retention_state,
            StorageRetentionState::Current
        );

        let store = MetadataStore::open(&db_path).expect("metadata");
        let synced = store
            .work_view_by_id(&WorkspaceId::new("ws_code"), &output.work_view.id)
            .expect("work view lookup")
            .expect("local work view");
        assert_eq!(synced.sync_state, WorkViewSyncState::Synced);
        assert!(synced.overlay_head.starts_with("b3_"));
    }

    #[test]
    fn sync_blocks_secret_bearing_work_view_overlay_before_upload() {
        let (temp, db_path) = seeded_store("phase9-work-overlay-secret");
        let project_path = temp.root().join("Code").join("apps/web");
        fs::create_dir_all(project_path.join("src")).expect("project src");
        fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

        let output = create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "env-edit".to_string(),
            owner_device_id: Some(DeviceId::new("device-1")),
            generated_at: now(),
        })
        .expect("work view");
        let materialized = temp.root().join("Code/.work/apps/web/env-edit");
        fs::write(materialized.join(".env.local"), "TOKEN=secret").expect("work env");

        let control_plane = FakeControlPlaneClient::default();
        control_plane.create_workspace("ws_code");
        let workspace_ref = control_plane
            .compare_and_swap_workspace_ref("ws_code", 0, "snap_project_base", "device-1")
            .expect("base ref");
        let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 92)
            .expect("byte store");

        let report = sync_local_work_view_overlays(
            WorkViewOverlaySyncOptions {
                db_path: db_path.clone(),
                device_id: DeviceId::new("device-1"),
                storage_key: StorageKey::deterministic(92),
                key_epoch: 1,
                generated_at: now(),
            },
            &control_plane,
            &byte_store,
            &workspace_ref,
        )
        .expect("overlay sync");

        assert_eq!(report.uploaded, 0);
        assert_eq!(report.attention, 1);
        assert!(
            control_plane
                .list_work_views("ws_code", true)
                .expect("remote work views")
                .is_empty()
        );

        let store = MetadataStore::open(&db_path).expect("metadata");
        let blocked = store
            .work_view_by_id(&WorkspaceId::new("ws_code"), &output.work_view.id)
            .expect("work view lookup")
            .expect("local work view");
        assert_eq!(blocked.sync_state, WorkViewSyncState::Attention);
        assert!(
            blocked
                .attention
                .iter()
                .any(|item| item.contains("review before overlay sync"))
        );
    }

    #[test]
    fn sync_marks_symlink_work_view_overlay_attention_without_aborting() {
        let (temp, db_path) = seeded_store("phase9-work-overlay-symlink");
        let project_path = temp.root().join("Code").join("apps/web");
        fs::create_dir_all(project_path.join("src")).expect("project src");
        fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

        let output = create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "link-edit".to_string(),
            owner_device_id: Some(DeviceId::new("device-1")),
            generated_at: now(),
        })
        .expect("work view");
        let materialized = temp.root().join("Code/.work/apps/web/link-edit");
        symlink("src/index.ts", materialized.join("linked.ts")).expect("work symlink");

        let control_plane = FakeControlPlaneClient::default();
        control_plane.create_workspace("ws_code");
        let workspace_ref = control_plane
            .compare_and_swap_workspace_ref("ws_code", 0, "snap_project_base", "device-1")
            .expect("base ref");
        let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 93)
            .expect("byte store");

        let report = sync_local_work_view_overlays(
            WorkViewOverlaySyncOptions {
                db_path: db_path.clone(),
                device_id: DeviceId::new("device-1"),
                storage_key: StorageKey::deterministic(93),
                key_epoch: 1,
                generated_at: now(),
            },
            &control_plane,
            &byte_store,
            &workspace_ref,
        )
        .expect("symlink should not abort workspace sync");

        assert_eq!(report.uploaded, 0);
        assert_eq!(report.attention, 1);
        let store = MetadataStore::open(&db_path).expect("metadata");
        let blocked = store
            .work_view_by_id(&WorkspaceId::new("ws_code"), &output.work_view.id)
            .expect("work view lookup")
            .expect("local work view");
        assert_eq!(blocked.sync_state, WorkViewSyncState::Attention);
        assert!(
            blocked
                .attention
                .iter()
                .any(|item| item.contains("needs review before sync"))
        );
    }

    #[test]
    fn sync_ignores_stale_create_log_when_file_no_longer_exists() {
        let (temp, db_path) = seeded_store("phase9-work-overlay-stale-log");
        let project_path = temp.root().join("Code").join("apps/web");
        fs::create_dir_all(project_path.join("src")).expect("project src");
        fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "stale-log".to_string(),
            owner_device_id: Some(DeviceId::new("device-1")),
            generated_at: now(),
        })
        .expect("work view");
        let store = MetadataStore::open(&db_path).expect("metadata");
        store
            .append_local_write_log(&LocalWriteLogRecord {
                id: "write-stale-create".to_string(),
                workspace_id: WorkspaceId::new("ws_code"),
                device_id: DeviceId::new("device-1"),
                project_id: Some(ProjectId::new("proj_web")),
                path: display(&temp.root().join("Code/.work/apps/web/stale-log/ghost.ts")),
                source_path: None,
                operation: "create".to_string(),
                staged_content_id: None,
                policy_classification: PathClassification::WorkspaceSync,
                causation_id: "test".to_string(),
                settled_at: now(),
                created_at: now(),
            })
            .expect("write log");
        drop(store);

        let control_plane = FakeControlPlaneClient::default();
        control_plane.create_workspace("ws_code");
        let workspace_ref = control_plane
            .compare_and_swap_workspace_ref("ws_code", 0, "snap_project_base", "device-1")
            .expect("base ref");
        let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 94)
            .expect("byte store");

        let report = sync_local_work_view_overlays(
            WorkViewOverlaySyncOptions {
                db_path,
                device_id: DeviceId::new("device-1"),
                storage_key: StorageKey::deterministic(94),
                key_epoch: 1,
                generated_at: now(),
            },
            &control_plane,
            &byte_store,
            &workspace_ref,
        )
        .expect("stale log should not abort sync");

        assert_eq!(report.uploaded, 0);
        assert_eq!(report.attention, 0);
    }

    #[test]
    fn sync_skips_attention_work_view_overlay_until_user_review() {
        let (temp, db_path) = seeded_store("phase9-work-overlay-attention-skip");
        let project_path = temp.root().join("Code").join("apps/web");
        fs::create_dir_all(project_path.join("src")).expect("project src");
        fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

        let output = create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "stale-remote".to_string(),
            owner_device_id: Some(DeviceId::new("device-1")),
            generated_at: now(),
        })
        .expect("work view");
        fs::write(
            temp.root()
                .join("Code/.work/apps/web/stale-remote/src/index.ts"),
            "console.log('local')",
        )
        .expect("overlay edit");
        let store = MetadataStore::open(&db_path).expect("metadata");
        let mut blocked = store
            .work_view_by_id(&WorkspaceId::new("ws_code"), &output.work_view.id)
            .expect("work view lookup")
            .expect("work view");
        blocked.sync_state = WorkViewSyncState::Attention;
        blocked.attention = vec!["Remote overlay changed; review required.".to_string()];
        store.upsert_work_view(&blocked).expect("attention view");
        drop(store);

        let control_plane = FakeControlPlaneClient::default();
        control_plane.create_workspace("ws_code");
        let workspace_ref = control_plane
            .compare_and_swap_workspace_ref("ws_code", 0, "snap_project_base", "device-1")
            .expect("base ref");
        let byte_store = LocalByteStore::open_deterministic(temp.root().join(".state/objects"), 95)
            .expect("byte store");

        let report = sync_local_work_view_overlays(
            WorkViewOverlaySyncOptions {
                db_path,
                device_id: DeviceId::new("device-1"),
                storage_key: StorageKey::deterministic(95),
                key_epoch: 1,
                generated_at: now(),
            },
            &control_plane,
            &byte_store,
            &workspace_ref,
        )
        .expect("attention view should not retry");

        assert_eq!(report.uploaded, 0);
        assert_eq!(report.attention, 0);
        assert!(
            control_plane
                .list_work_views("ws_code", true)
                .expect("remote work views")
                .is_empty()
        );
    }

    #[test]
    fn workon_rejects_duplicate_name_without_rewriting_existing_view() {
        let (temp, db_path) = seeded_store("phase9-workon-duplicate");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(&project_path).expect("project");
        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "same-name".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("first work view");

        let error = create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "same-name".to_string(),
            owner_device_id: None,
            generated_at: "2026-06-25T13:00:00Z".to_string(),
        })
        .expect_err("duplicate should fail");

        assert!(error.to_string().contains("already exists"));

        let error = create_work_view(WorkonOptions {
            db_path: Some(db_path),
            project_path: project_path.display().to_string(),
            name: "Same-Name".to_string(),
            owner_device_id: None,
            generated_at: "2026-06-25T13:00:01Z".to_string(),
        })
        .expect_err("case-only duplicate should fail");

        assert!(error.to_string().contains("already exists"));
    }

    #[test]
    fn workon_rejects_preexisting_non_empty_materialization() {
        let (temp, db_path) = seeded_store("phase9-workon-stale-materialization");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(&project_path).expect("project");
        let stale = temp.root().join("Code/.work/apps/web/stale/src");
        fs::create_dir_all(&stale).expect("stale dir");
        fs::write(stale.join("old.ts"), "stale\n").expect("stale file");

        let error = create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "stale".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect_err("stale materialization should fail");

        assert!(
            error
                .to_string()
                .contains("materialization path is not empty")
        );
        let store = MetadataStore::open(&db_path).expect("metadata");
        let workspace = store
            .current_workspace()
            .expect("workspace query")
            .expect("workspace");
        assert!(
            store
                .work_views_by_name(&workspace.id, None, "stale")
                .expect("work views")
                .is_empty()
        );
    }

    #[test]
    fn workon_rejects_symlinked_work_namespace() {
        let (temp, db_path) = seeded_store("phase9-workon-symlink-namespace");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(&project_path).expect("project");
        let work_root = temp.root().join("Code/.work");
        let outside = temp.root().join("outside-work");
        fs::create_dir_all(&outside).expect("outside");
        symlink(&outside, &work_root).expect("work symlink");

        let error = create_work_view(WorkonOptions {
            db_path: Some(db_path),
            project_path: project_path.display().to_string(),
            name: "escape".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect_err("symlinked namespace should fail");

        assert!(
            error
                .to_string()
                .contains("materialization escapes workspace")
        );
        assert!(!outside.join("apps/web/escape").exists());
    }

    #[test]
    fn root_project_base_capture_skips_work_namespace() {
        let (temp, db_path) = seeded_store("phase9-root-project-work-skip");
        let code_root = temp.root().join("Code");
        let workspace_id = WorkspaceId::new("ws_code");
        let project_id = ProjectId::new("proj_root");
        let store = MetadataStore::open(&db_path).expect("metadata");
        store
            .insert_project(
                &project_id,
                &workspace_id,
                "root_code",
                "",
                "2026-06-25T00:01:00Z",
            )
            .expect("root project");
        store
            .set_project_latest_snapshot_id(
                &workspace_id,
                &project_id,
                &SnapshotId::new("snap_root_base"),
            )
            .expect("root snapshot");
        drop(store);

        fs::create_dir_all(code_root.join("src")).expect("src");
        fs::write(code_root.join("src/app.ts"), "console.log('root')").expect("source");
        fs::create_dir_all(code_root.join(".work/apps/web/other/src")).expect("work namespace");
        fs::write(
            code_root.join(".work/apps/web/other/src/generated.ts"),
            "console.log('work')",
        )
        .expect("work file");

        let output = create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: code_root.display().to_string(),
            name: "root-edit".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("root work view");

        let store = MetadataStore::open(&db_path).expect("metadata");
        assert!(
            store
                .work_view_base_hash(&workspace_id, &output.work_view.id, "src/app.ts")
                .expect("source hash")
                .is_some()
        );
        assert!(
            store
                .work_view_base_hash(
                    &workspace_id,
                    &output.work_view.id,
                    ".work/apps/web/other/src/generated.ts",
                )
                .expect("work hash")
                .is_none()
        );
    }

    #[test]
    fn workon_removes_materialization_after_post_create_metadata_failure() {
        let (temp, db_path) = seeded_store("phase9-workon-metadata-rollback");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(&project_path).expect("project");
        fs::write(project_path.join("index.ts"), "console.log('base')\n").expect("base file");
        let materialized = temp.root().join("Code/.work/apps/web/rollback");
        let store = MetadataStore::open(&db_path).expect("metadata");
        store
            .connection()
            .execute(
                "CREATE TRIGGER fail_work_view_base_file_insert
                 BEFORE INSERT ON work_view_base_files
                 BEGIN
                   SELECT RAISE(ABORT, 'forced base file insert failure');
                 END",
                [],
            )
            .expect("create failing trigger");
        drop(store);

        create_work_view(WorkonOptions {
            db_path: Some(db_path),
            project_path: project_path.display().to_string(),
            name: "rollback".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect_err("metadata failure should abort workon");

        assert!(!materialized.exists());
    }

    #[test]
    fn lifecycle_transitions_hide_then_restore_retained_work_view() {
        let (temp, db_path) = seeded_store("phase9-lifecycle");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(&project_path).expect("project");
        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "billing".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("work view");

        let discarded = discard_work_view(WorkSelectorOptions {
            db_path: Some(db_path.clone()),
            selector: "billing".to_string(),
            generated_at: now(),
        })
        .expect("discard");
        assert_eq!(
            serde_json::to_value(discarded.work_view.lifecycle).unwrap(),
            "discarded"
        );
        let visible = list_work_views(WorkListOptions {
            db_path: Some(db_path.clone()),
            include_hidden: false,
            current_device_id: None,
            generated_at: now(),
        })
        .expect("list");
        assert!(visible.work_views.is_empty());

        let restored = restore_work_view(WorkSelectorOptions {
            db_path: Some(db_path),
            selector: "billing".to_string(),
            generated_at: now(),
        })
        .expect("restore");
        assert_eq!(
            serde_json::to_value(restored.work_view.lifecycle).unwrap(),
            "active"
        );
    }

    #[test]
    fn discard_work_view_marks_matching_agent_lease_discarded() {
        let (temp, db_path) = seeded_store("phase9-discard-agent-lease");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(&project_path).expect("project");
        let lease = create_agent_lease(AgentLeaseCreateOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            task: "discard me".to_string(),
            base: AgentLeaseBase::LatestWorkspace,
            hydrate_budget_bytes: 1024 * 1024,
            work_view: true,
            device_id: DeviceId::new("device-test"),
            generated_at: now(),
        })
        .expect("lease")
        .lease;

        discard_work_view(WorkSelectorOptions {
            db_path: Some(db_path.clone()),
            selector: lease.work_view_id.as_str().to_string(),
            generated_at: now(),
        })
        .expect("discard");

        let stored = MetadataStore::open(&db_path)
            .expect("store")
            .agent_lease_by_id(&lease.id)
            .expect("lease query")
            .expect("lease stored");
        assert_eq!(stored.output_state, AgentLeaseOutputState::Discarded);
        assert_eq!(stored.status_summary, "discarded");
    }

    #[test]
    fn restore_recreates_missing_retained_materialization() {
        let (temp, db_path) = seeded_store("phase9-restore-after-cleanup");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(&project_path).expect("project");
        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "restore-me".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("work view");
        discard_work_view(WorkSelectorOptions {
            db_path: Some(db_path.clone()),
            selector: "restore-me".to_string(),
            generated_at: now(),
        })
        .expect("discard");
        let materialized = temp.root().join("Code/.work/apps/web/restore-me");
        fs::remove_dir_all(&materialized).expect("remove materialization");
        assert!(!materialized.exists());

        let restored = restore_work_view(WorkSelectorOptions {
            db_path: Some(db_path),
            selector: "restore-me".to_string(),
            generated_at: "2026-06-25T13:00:00Z".to_string(),
        })
        .expect("restore");

        assert_eq!(
            serde_json::to_value(restored.work_view.lifecycle).unwrap(),
            "active"
        );
        assert!(materialized.is_dir());
    }

    #[test]
    fn restore_rejects_cleaned_delete_eligible_work_view() {
        let (temp, db_path) = seeded_store("phase9-restore-after-cleanup");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(&project_path).expect("project");
        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "restore-me".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("work view");
        discard_work_view(WorkSelectorOptions {
            db_path: Some(db_path.clone()),
            selector: "restore-me".to_string(),
            generated_at: now(),
        })
        .expect("discard");
        cleanup_work_views(WorkCleanupOptions {
            db_path: Some(db_path.clone()),
            apply: true,
            generated_at: now(),
        })
        .expect("cleanup");
        let materialized = temp.root().join("Code/.work/apps/web/restore-me");
        assert!(!materialized.exists());

        let error = restore_work_view(WorkSelectorOptions {
            db_path: Some(db_path.clone()),
            selector: "restore-me".to_string(),
            generated_at: "2026-06-25T13:00:00Z".to_string(),
        })
        .expect_err("cleaned work view should not restore");
        assert!(error.to_string().contains("is not restorable"));
        assert!(!materialized.exists());

        let store = MetadataStore::open(&db_path).expect("metadata");
        let workspace = store
            .current_workspace()
            .expect("workspace query")
            .expect("workspace");
        let cleaned = store
            .work_views_by_name(&workspace.id, None, "restore-me")
            .expect("work views")
            .pop()
            .expect("cleaned view");
        assert_eq!(
            serde_json::to_value(cleaned.retention.state).unwrap(),
            "delete-eligible"
        );
        assert!(!cleaned.retention.restorable);
    }

    #[test]
    fn list_reports_review_ready_work_view_attention() {
        let (temp, db_path) = seeded_store("phase9-list-review-ready");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(&project_path).expect("project");
        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "needs-review".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("work view");
        let store = MetadataStore::open(&db_path).expect("metadata");
        let workspace = store
            .current_workspace()
            .expect("workspace query")
            .expect("workspace");
        let mut view = store
            .work_views_by_name(&workspace.id, None, "needs-review")
            .expect("work views")
            .pop()
            .expect("work view");
        view.lifecycle = WorkViewLifecycle::ReviewReady;
        view.sync_state = WorkViewSyncState::Attention;
        store.upsert_work_view(&view).expect("review-ready view");
        drop(store);

        let listed = list_work_views(WorkListOptions {
            db_path: Some(db_path),
            include_hidden: false,
            current_device_id: None,
            generated_at: now(),
        })
        .expect("list");

        assert_eq!(listed.status.level, StatusLevel::Attention);
        assert!(listed.status.attention_items[0].contains("needs-review"));
    }

    #[test]
    fn default_work_list_hides_unfollowed_remote_active_views() {
        let (temp, db_path) = seeded_store("phase9-list-visibility");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(&project_path).expect("project");
        for (name, owner) in [
            ("local-edit", "dev_mac"),
            ("remote-edit", "dev_linux"),
            ("remote-review", "dev_linux"),
        ] {
            create_work_view(WorkonOptions {
                db_path: Some(db_path.clone()),
                project_path: project_path.display().to_string(),
                name: name.to_string(),
                owner_device_id: Some(DeviceId::new(owner)),
                generated_at: now(),
            })
            .expect("work view");
        }
        let store = MetadataStore::open(&db_path).expect("metadata");
        let workspace = store
            .current_workspace()
            .expect("workspace query")
            .expect("workspace");
        let mut review = store
            .work_views_by_name(&workspace.id, None, "remote-review")
            .expect("review query")
            .pop()
            .expect("review view");
        review.lifecycle = WorkViewLifecycle::ReviewReady;
        review.sync_state = WorkViewSyncState::Attention;
        store.upsert_work_view(&review).expect("review update");
        drop(store);

        let listed = list_work_views(WorkListOptions {
            db_path: Some(db_path),
            include_hidden: false,
            current_device_id: Some(DeviceId::new("dev_mac")),
            generated_at: now(),
        })
        .expect("list");
        let names = listed
            .work_views
            .iter()
            .map(|view| view.name.as_str())
            .collect::<Vec<_>>();

        assert!(names.contains(&"local-edit"));
        assert!(names.contains(&"remote-review"));
        assert!(!names.contains(&"remote-edit"));
    }

    #[test]
    fn discarded_work_view_must_be_restored_before_accept() {
        let (temp, db_path) = seeded_store("phase9-discard-accept");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(&project_path).expect("project");
        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "discarded-edit".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("work view");
        let materialized = temp.root().join("Code/.work/apps/web/discarded-edit/src");
        fs::create_dir_all(&materialized).expect("work src");
        fs::write(materialized.join("leak.ts"), "stale\n").expect("stale overlay");
        discard_work_view(WorkSelectorOptions {
            db_path: Some(db_path.clone()),
            selector: "discarded-edit".to_string(),
            generated_at: now(),
        })
        .expect("discard");

        let error = accept_work_view(WorkSelectorOptions {
            db_path: Some(db_path),
            selector: "discarded-edit".to_string(),
            generated_at: now(),
        })
        .expect_err("discarded work should not accept");

        assert!(error.to_string().contains("must be restored"));
        assert!(!project_path.join("src/leak.ts").exists());
    }

    #[test]
    fn accept_secret_bearing_work_file_requires_review() {
        let (temp, db_path) = seeded_store("phase9-accept-secret-review");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(&project_path).expect("project");
        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "secret-edit".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("work view");
        let materialized = temp.root().join("Code/.work/apps/web/secret-edit");
        fs::write(materialized.join(".env.local"), "TOKEN=secret\n").expect("work env");

        let accepted = accept_work_view(WorkSelectorOptions {
            db_path: Some(db_path),
            selector: "secret-edit".to_string(),
            generated_at: now(),
        })
        .expect("accept");

        assert_eq!(
            serde_json::to_value(accepted.work_view.lifecycle).unwrap(),
            "review-ready"
        );
        assert!(!project_path.join(".env.local").exists());
    }

    #[test]
    fn cleanup_preview_is_non_destructive_and_apply_removes_archived_dirs() {
        let (temp, db_path) = seeded_store("phase9-cleanup");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(&project_path).expect("project");
        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "cleanup-me".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("work view");
        let materialized = temp.root().join("Code/.work/apps/web/cleanup-me");
        assert!(materialized.is_dir());
        discard_work_view(WorkSelectorOptions {
            db_path: Some(db_path.clone()),
            selector: "cleanup-me".to_string(),
            generated_at: now(),
        })
        .expect("discard");

        let preview = cleanup_work_views(WorkCleanupOptions {
            db_path: Some(db_path.clone()),
            apply: false,
            generated_at: now(),
        })
        .expect("preview");
        assert!(preview.deleted_paths.is_empty());
        assert!(materialized.is_dir());

        let applied = cleanup_work_views(WorkCleanupOptions {
            db_path: Some(db_path),
            apply: true,
            generated_at: now(),
        })
        .expect("apply");
        assert_eq!(applied.deleted_paths, vec![display(&materialized)]);
        assert!(!materialized.exists());
    }

    #[test]
    fn clean_accept_applies_new_work_view_files_to_main_view() {
        let (temp, db_path) = seeded_store("phase9-accept-clean");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(&project_path).expect("project");
        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "new-file".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("work view");
        let materialized = temp.root().join("Code/.work/apps/web/new-file");
        fs::create_dir_all(materialized.join("src")).expect("src");
        fs::write(materialized.join("src/new.ts"), "export const ok = true;\n").expect("work file");

        let accepted = accept_work_view(WorkSelectorOptions {
            db_path: Some(db_path),
            selector: "new-file".to_string(),
            generated_at: now(),
        })
        .expect("accept");

        assert_eq!(serde_json::to_value(accepted.action).unwrap(), "accepted");
        assert_eq!(
            fs::read_to_string(project_path.join("src/new.ts")).expect("main file"),
            "export const ok = true;\n"
        );
    }

    #[test]
    fn accept_dependency_file_ignores_local_regenerate_churn() {
        let (temp, db_path) = seeded_store("phase9-accept-policy");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(&project_path).expect("project");
        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "deps".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("work view");
        let work_file = temp
            .root()
            .join("Code/.work/apps/web/deps/node_modules/lodash/index.js");
        fs::create_dir_all(work_file.parent().expect("parent")).expect("dependency dir");
        fs::write(&work_file, "module.exports = {}\n").expect("dependency file");

        let accepted = accept_work_view(WorkSelectorOptions {
            db_path: Some(db_path),
            selector: "deps".to_string(),
            generated_at: now(),
        })
        .expect("accept");

        assert_eq!(serde_json::to_value(accepted.action).unwrap(), "accepted");
        assert!(accepted.work_view.attention.is_empty());
        assert!(
            !temp
                .root()
                .join("Code/apps/web/node_modules/lodash/index.js")
                .exists()
        );
    }

    #[test]
    fn clean_accept_applies_existing_file_when_main_has_not_changed() {
        let (temp, db_path) = seeded_store("phase9-accept-existing-clean");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(project_path.join("src")).expect("project");
        fs::write(project_path.join("src/index.ts"), "base\n").expect("main file");
        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "edit-existing".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("work view");
        let materialized = temp.root().join("Code/.work/apps/web/edit-existing/src");
        fs::create_dir_all(&materialized).expect("work src");
        fs::write(materialized.join("index.ts"), "work edit\n").expect("work file");

        let accepted = accept_work_view(WorkSelectorOptions {
            db_path: Some(db_path),
            selector: "edit-existing".to_string(),
            generated_at: "2026-06-25T12:05:00Z".to_string(),
        })
        .expect("accept");

        assert_eq!(serde_json::to_value(accepted.action).unwrap(), "accepted");
        assert_eq!(
            fs::read_to_string(project_path.join("src/index.ts")).expect("main file"),
            "work edit\n"
        );
    }

    #[test]
    fn accept_detects_unlogged_main_view_edits_from_base_hash() {
        let (temp, db_path) = seeded_store("phase9-accept-unlogged-main-edit");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(project_path.join("src")).expect("project");
        fs::write(project_path.join("src/index.ts"), "base\n").expect("main file");
        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "unlogged-main".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("work view");
        fs::write(project_path.join("src/index.ts"), "main changed\n").expect("missed main edit");
        let materialized = temp.root().join("Code/.work/apps/web/unlogged-main/src");
        fs::create_dir_all(&materialized).expect("work src");
        fs::write(materialized.join("index.ts"), "work edit\n").expect("work file");

        let accepted = accept_work_view(WorkSelectorOptions {
            db_path: Some(db_path),
            selector: "unlogged-main".to_string(),
            generated_at: now(),
        })
        .expect("accept");

        assert_eq!(
            serde_json::to_value(accepted.work_view.lifecycle).unwrap(),
            "review-ready"
        );
        assert_eq!(
            fs::read_to_string(project_path.join("src/index.ts")).expect("main file"),
            "main changed\n"
        );
    }

    #[test]
    fn accept_detects_main_view_deletion_from_base_hash() {
        let (temp, db_path) = seeded_store("phase9-accept-main-delete");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(project_path.join("src")).expect("project");
        fs::write(project_path.join("src/index.ts"), "base\n").expect("main file");
        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "main-delete".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("work view");
        fs::remove_file(project_path.join("src/index.ts")).expect("main delete");
        let materialized = temp.root().join("Code/.work/apps/web/main-delete/src");
        fs::create_dir_all(&materialized).expect("work src");
        fs::write(materialized.join("index.ts"), "work edit\n").expect("work file");

        let accepted = accept_work_view(WorkSelectorOptions {
            db_path: Some(db_path),
            selector: "main-delete".to_string(),
            generated_at: now(),
        })
        .expect("accept");

        assert_eq!(
            serde_json::to_value(accepted.work_view.lifecycle).unwrap(),
            "review-ready"
        );
        assert!(!project_path.join("src/index.ts").exists());
    }

    #[test]
    fn clean_accept_applies_work_view_deletions() {
        let (temp, db_path) = seeded_store("phase9-accept-delete");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(project_path.join("src")).expect("project");
        fs::write(project_path.join("src/old.ts"), "old\n").expect("main file");
        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "delete-old".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("work view");
        let store = MetadataStore::open(&db_path).expect("metadata");
        store
            .append_local_write_log(&LocalWriteLogRecord {
                id: "write-delete-old".to_string(),
                workspace_id: WorkspaceId::new("ws_code"),
                device_id: DeviceId::new("dev_mac"),
                project_id: Some(ProjectId::new("proj_web")),
                path: format!(
                    "{}/src/old.ts",
                    temp.root().join("Code/.work/apps/web/delete-old").display()
                ),
                source_path: None,
                operation: "delete".to_string(),
                staged_content_id: None,
                policy_classification: PathClassification::WorkspaceSync,
                causation_id: "human".to_string(),
                settled_at: "2026-06-25T12:01:00Z".to_string(),
                created_at: "2026-06-25T12:01:00Z".to_string(),
            })
            .expect("delete write");
        drop(store);

        let accepted = accept_work_view(WorkSelectorOptions {
            db_path: Some(db_path),
            selector: "delete-old".to_string(),
            generated_at: now(),
        })
        .expect("accept");

        assert_eq!(serde_json::to_value(accepted.action).unwrap(), "accepted");
        assert!(!project_path.join("src/old.ts").exists());
    }

    #[test]
    fn clean_accept_preserves_recreated_file_after_delete_log() {
        let (temp, db_path) = seeded_store("work-view-delete-recreate");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(project_path.join("src")).expect("project");
        fs::write(project_path.join("src/recreated.ts"), "old\n").expect("main file");
        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "delete-recreate".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("work view");
        let work_file = temp
            .root()
            .join("Code/.work/apps/web/delete-recreate/src/recreated.ts");
        fs::write(&work_file, "new\n").expect("recreated work file");
        let store = MetadataStore::open(&db_path).expect("metadata");
        for (id, operation, created_at) in [
            ("write-delete-recreated", "delete", "2026-06-25T12:01:00Z"),
            ("write-update-recreated", "update", "2026-06-25T12:02:00Z"),
        ] {
            store
                .append_local_write_log(&LocalWriteLogRecord {
                    id: id.to_string(),
                    workspace_id: WorkspaceId::new("ws_code"),
                    device_id: DeviceId::new("dev_mac"),
                    project_id: Some(ProjectId::new("proj_web")),
                    path: work_file.display().to_string(),
                    source_path: None,
                    operation: operation.to_string(),
                    staged_content_id: None,
                    policy_classification: PathClassification::WorkspaceSync,
                    causation_id: "human".to_string(),
                    settled_at: created_at.to_string(),
                    created_at: created_at.to_string(),
                })
                .expect("write log");
        }
        drop(store);

        let accepted = accept_work_view(WorkSelectorOptions {
            db_path: Some(db_path),
            selector: "delete-recreate".to_string(),
            generated_at: now(),
        })
        .expect("accept");

        assert_eq!(serde_json::to_value(accepted.action).unwrap(), "accepted");
        assert_eq!(
            fs::read_to_string(project_path.join("src/recreated.ts")).expect("accepted file"),
            "new\n"
        );
    }

    #[test]
    fn clean_accept_renames_by_deleting_source_path() {
        let (temp, db_path) = seeded_store("work-view-rename");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(project_path.join("src")).expect("project");
        fs::write(project_path.join("src/old.ts"), "old\n").expect("main file");
        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "rename-file".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("work view");
        let work_root = temp.root().join("Code/.work/apps/web/rename-file");
        let old_file = work_root.join("src/old.ts");
        let new_file = work_root.join("src/new.ts");
        fs::rename(&old_file, &new_file).expect("rename in work view");
        let store = MetadataStore::open(&db_path).expect("metadata");
        store
            .append_local_write_log(&LocalWriteLogRecord {
                id: "write-rename-file".to_string(),
                workspace_id: WorkspaceId::new("ws_code"),
                device_id: DeviceId::new("dev_mac"),
                project_id: Some(ProjectId::new("proj_web")),
                path: new_file.display().to_string(),
                source_path: Some(old_file.display().to_string()),
                operation: "rename".to_string(),
                staged_content_id: None,
                policy_classification: PathClassification::WorkspaceSync,
                causation_id: "human".to_string(),
                settled_at: "2026-06-25T12:02:00Z".to_string(),
                created_at: "2026-06-25T12:02:00Z".to_string(),
            })
            .expect("rename log");
        drop(store);

        let accepted = accept_work_view(WorkSelectorOptions {
            db_path: Some(db_path),
            selector: "rename-file".to_string(),
            generated_at: now(),
        })
        .expect("accept");

        assert_eq!(serde_json::to_value(accepted.action).unwrap(), "accepted");
        assert!(!project_path.join("src/old.ts").exists());
        assert_eq!(
            fs::read_to_string(project_path.join("src/new.ts")).expect("renamed file"),
            "old\n"
        );
    }

    #[test]
    fn accept_derives_deleted_touched_base_files_without_delete_log() {
        let (temp, db_path) = seeded_store("phase9-accept-derived-delete");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(project_path.join("src")).expect("project");
        fs::write(project_path.join("src/old.ts"), "old\n").expect("main file");
        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "delete-derived".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("work view");
        let materialized = temp
            .root()
            .join("Code/.work/apps/web/delete-derived/src/old.ts");
        fs::create_dir_all(materialized.parent().expect("materialized parent")).expect("work src");
        fs::write(&materialized, "old\n").expect("materialized file");
        fs::remove_file(&materialized).expect("delete materialized file without delete log");
        let store = MetadataStore::open(&db_path).expect("metadata");
        store
            .append_local_write_log(&LocalWriteLogRecord {
                id: "write-derived-delete-old".to_string(),
                workspace_id: WorkspaceId::new("ws_code"),
                device_id: DeviceId::new("dev_mac"),
                project_id: Some(ProjectId::new("proj_web")),
                path: format!(
                    "{}/src/old.ts",
                    temp.root()
                        .join("Code/.work/apps/web/delete-derived")
                        .display()
                ),
                source_path: None,
                operation: "modify".to_string(),
                staged_content_id: None,
                policy_classification: PathClassification::WorkspaceSync,
                causation_id: "human".to_string(),
                settled_at: "2026-06-25T12:01:00Z".to_string(),
                created_at: "2026-06-25T12:01:00Z".to_string(),
            })
            .expect("modify write");
        drop(store);

        let accepted = accept_work_view(WorkSelectorOptions {
            db_path: Some(db_path),
            selector: "delete-derived".to_string(),
            generated_at: now(),
        })
        .expect("accept");

        assert_eq!(serde_json::to_value(accepted.action).unwrap(), "accepted");
        assert!(!project_path.join("src/old.ts").exists());
    }

    #[test]
    fn accept_applies_unlogged_filesystem_deletions() {
        let (temp, db_path) = seeded_store("work-view-accept-unlogged-delete");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(project_path.join("src")).expect("project");
        fs::write(project_path.join("src/old.ts"), "old\n").expect("main file");
        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "delete-unlogged".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("work view");
        let materialized = temp
            .root()
            .join("Code/.work/apps/web/delete-unlogged/src/old.ts");
        fs::remove_file(&materialized).expect("delete materialized file without log");

        let diff = diff_work_view(WorkSelectorOptions {
            db_path: Some(db_path.clone()),
            selector: "delete-unlogged".to_string(),
            generated_at: now(),
        })
        .expect("diff");
        assert_eq!(diff.changes.len(), 1);
        assert_eq!(diff.changes[0].kind, WorkDiffChangeKind::Deleted);

        let accepted = accept_work_view(WorkSelectorOptions {
            db_path: Some(db_path),
            selector: "delete-unlogged".to_string(),
            generated_at: now(),
        })
        .expect("accept");

        assert_eq!(serde_json::to_value(accepted.action).unwrap(), "accepted");
        assert!(!project_path.join("src/old.ts").exists());
    }

    #[test]
    fn diff_includes_unlogged_deletions_alongside_logged_updates() {
        let (temp, db_path) = seeded_store("work-view-diff-mixed-unlogged-delete");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(project_path.join("src")).expect("project");
        fs::write(project_path.join("src/edit.ts"), "old\n").expect("edit");
        fs::write(project_path.join("src/delete.ts"), "delete\n").expect("delete");
        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "mixed".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("work view");
        let work_root = temp.root().join("Code/.work/apps/web/mixed");
        let edit_file = work_root.join("src/edit.ts");
        fs::write(&edit_file, "new\n").expect("edit work");
        fs::remove_file(work_root.join("src/delete.ts")).expect("delete work");
        let store = MetadataStore::open(&db_path).expect("metadata");
        store
            .append_local_write_log(&LocalWriteLogRecord {
                id: "write-mixed-edit".to_string(),
                workspace_id: WorkspaceId::new("ws_code"),
                device_id: DeviceId::new("dev_mac"),
                project_id: Some(ProjectId::new("proj_web")),
                path: edit_file.display().to_string(),
                source_path: None,
                operation: "update".to_string(),
                staged_content_id: None,
                policy_classification: PathClassification::WorkspaceSync,
                causation_id: "human".to_string(),
                settled_at: "2026-06-25T12:02:00Z".to_string(),
                created_at: "2026-06-25T12:02:00Z".to_string(),
            })
            .expect("write");
        drop(store);

        let diff = diff_work_view(WorkSelectorOptions {
            db_path: Some(db_path),
            selector: "mixed".to_string(),
            generated_at: now(),
        })
        .expect("diff");
        let changes = diff
            .changes
            .iter()
            .map(|entry| (entry.path.as_str(), entry.kind))
            .collect::<Vec<_>>();

        assert!(changes.contains(&("src/edit.ts", WorkDiffChangeKind::Modified)));
        assert!(changes.contains(&("src/delete.ts", WorkDiffChangeKind::Deleted)));
    }

    #[test]
    fn clean_accept_preserves_untouched_base_files() {
        let (temp, db_path) = seeded_store("work-view-accept-carry-forward");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(project_path.join("src")).expect("project");
        fs::write(project_path.join("src/keep.ts"), "keep\n").expect("base keep");
        fs::write(project_path.join("src/edit.ts"), "base\n").expect("base edit");
        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "carry-forward".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("work view");
        let materialized = temp
            .root()
            .join("Code/.work/apps/web/carry-forward/src/edit.ts");
        fs::write(&materialized, "work\n").expect("work edit");

        let accepted = accept_work_view(WorkSelectorOptions {
            db_path: Some(db_path),
            selector: "carry-forward".to_string(),
            generated_at: now(),
        })
        .expect("accept");

        assert_eq!(serde_json::to_value(accepted.action).unwrap(), "accepted");
        assert_eq!(
            fs::read_to_string(project_path.join("src/keep.ts")).expect("base-only file"),
            "keep\n"
        );
        assert_eq!(
            fs::read_to_string(project_path.join("src/edit.ts")).expect("accepted edit"),
            "work\n"
        );
    }

    #[test]
    fn unsupported_overlay_write_requires_review_without_mutating_main() {
        let (temp, db_path) = seeded_store("work-view-unsupported-overlay");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(project_path.join("src")).expect("project");
        fs::write(project_path.join("src/index.ts"), "main\n").expect("main");
        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "unsupported".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("work view");
        let work_file = temp
            .root()
            .join("Code/.work/apps/web/unsupported/src/index.ts");
        fs::write(&work_file, "work\n").expect("work");
        let store = MetadataStore::open(&db_path).expect("metadata");
        store
            .append_local_write_log(&LocalWriteLogRecord {
                id: "write-mmap-unsupported".to_string(),
                workspace_id: WorkspaceId::new("ws_code"),
                device_id: DeviceId::new("dev_mac"),
                project_id: Some(ProjectId::new("proj_web")),
                path: work_file.display().to_string(),
                source_path: None,
                operation: "mmap".to_string(),
                staged_content_id: None,
                policy_classification: PathClassification::WorkspaceSync,
                causation_id: "human".to_string(),
                settled_at: "2026-06-25T12:02:00Z".to_string(),
                created_at: "2026-06-25T12:02:00Z".to_string(),
            })
            .expect("unsupported write");
        drop(store);

        let diff = diff_work_view(WorkSelectorOptions {
            db_path: Some(db_path.clone()),
            selector: "unsupported".to_string(),
            generated_at: now(),
        })
        .expect("diff");
        assert_eq!(diff.status.level, StatusLevel::Attention);
        assert_eq!(diff.changes[0].kind, WorkDiffChangeKind::PolicyReview);

        let output = accept_work_view(WorkSelectorOptions {
            db_path: Some(db_path),
            selector: "unsupported".to_string(),
            generated_at: now(),
        })
        .expect("accept");

        assert_eq!(serde_json::to_value(output.action).unwrap(), "review-ready");
        assert_eq!(
            fs::read_to_string(project_path.join("src/index.ts")).expect("main"),
            "main\n"
        );
        assert!(
            output
                .work_view
                .attention
                .iter()
                .any(|item| item.contains("src/index.ts"))
        );
    }

    #[test]
    fn accept_ignores_source_control_metadata_scaffold() {
        let (temp, db_path) = seeded_store("phase9-accept-git-scaffold");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(project_path.join("src")).expect("project");
        fs::write(project_path.join("src/message.ts"), "old\n").expect("main file");
        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "git-edit".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("work view");
        let materialized = temp.root().join("Code/.work/apps/web/git-edit");
        fs::create_dir_all(materialized.join(".git")).expect("git dir");
        fs::write(materialized.join(".git/config"), "[core]\n").expect("git config");
        fs::write(materialized.join("src/message.ts"), "new\n").expect("work file");
        let store = MetadataStore::open(&db_path).expect("metadata");
        store
            .append_local_write_log(&LocalWriteLogRecord {
                id: "write-git-scaffold".to_string(),
                workspace_id: WorkspaceId::new("ws_code"),
                device_id: DeviceId::new("dev_mac"),
                project_id: Some(ProjectId::new("proj_web")),
                path: format!("{}/.git/config", materialized.display()),
                source_path: None,
                operation: "modify".to_string(),
                staged_content_id: None,
                policy_classification: PathClassification::WorkspaceSync,
                causation_id: "materialization".to_string(),
                settled_at: "2026-06-25T12:02:00Z".to_string(),
                created_at: "2026-06-25T12:02:00Z".to_string(),
            })
            .expect("git scaffold write");
        drop(store);

        let accepted = accept_work_view(WorkSelectorOptions {
            db_path: Some(db_path),
            selector: "git-edit".to_string(),
            generated_at: now(),
        })
        .expect("accept");

        assert_eq!(serde_json::to_value(accepted.action).unwrap(), "accepted");
        assert!(!project_path.join(".git/config").exists());
        assert_eq!(
            fs::read_to_string(project_path.join("src/message.ts")).expect("accepted file"),
            "new\n"
        );
    }

    #[test]
    fn diff_ignores_main_project_write_log_entries() {
        let (temp, db_path) = seeded_store("phase9-diff-main-write-log");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(&project_path).expect("project");
        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "scoped-diff".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("work view");
        let materialized = temp.root().join("Code/.work/apps/web/scoped-diff/src");
        fs::create_dir_all(&materialized).expect("work src");
        fs::write(materialized.join("work.ts"), "export const work = true;\n").expect("work file");

        let store = MetadataStore::open(&db_path).expect("metadata");
        store
            .append_local_write_log(&LocalWriteLogRecord {
                id: "write-main".to_string(),
                workspace_id: WorkspaceId::new("ws_code"),
                device_id: DeviceId::new("dev_mac"),
                project_id: Some(ProjectId::new("proj_web")),
                path: "apps/web/src/main.ts".to_string(),
                source_path: None,
                operation: "modify".to_string(),
                staged_content_id: None,
                policy_classification: PathClassification::WorkspaceSync,
                causation_id: "human".to_string(),
                settled_at: "2026-06-25T12:02:00Z".to_string(),
                created_at: "2026-06-25T12:02:00Z".to_string(),
            })
            .expect("main write");
        drop(store);

        let diff = diff_work_view(WorkSelectorOptions {
            db_path: Some(db_path),
            selector: "scoped-diff".to_string(),
            generated_at: now(),
        })
        .expect("diff");

        assert_eq!(diff.changes.len(), 1);
        assert_eq!(diff.changes[0].path, "src/work.ts");
    }

    #[test]
    fn diff_ignores_sibling_work_view_name_prefixes() {
        let (temp, db_path) = seeded_store("phase9-diff-prefix-sibling");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(&project_path).expect("project");
        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "auth".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("auth work view");
        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "auth-fix".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("auth-fix work view");
        let store = MetadataStore::open(&db_path).expect("metadata");
        store
            .append_local_write_log(&LocalWriteLogRecord {
                id: "write-auth-fix".to_string(),
                workspace_id: WorkspaceId::new("ws_code"),
                device_id: DeviceId::new("dev_mac"),
                project_id: Some(ProjectId::new("proj_web")),
                path: ".work/apps/web/auth-fix/src/leak.ts".to_string(),
                source_path: None,
                operation: "create".to_string(),
                staged_content_id: None,
                policy_classification: PathClassification::WorkspaceSync,
                causation_id: "human".to_string(),
                settled_at: "2026-06-25T12:01:00Z".to_string(),
                created_at: "2026-06-25T12:01:00Z".to_string(),
            })
            .expect("sibling write");
        drop(store);

        let diff = diff_work_view(WorkSelectorOptions {
            db_path: Some(db_path),
            selector: "auth".to_string(),
            generated_at: now(),
        })
        .expect("diff");

        assert!(diff.changes.is_empty());
    }

    #[test]
    fn conflicting_accept_becomes_review_ready_without_overwriting_main_view() {
        let (temp, db_path) = seeded_store("phase9-accept-conflict");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(project_path.join("src")).expect("project src");
        fs::write(project_path.join("src/index.ts"), "main\n").expect("main file");
        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "conflict-file".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("work view");
        fs::write(project_path.join("src/index.ts"), "main changed\n").expect("main update");
        let store = MetadataStore::open(&db_path).expect("metadata");
        store
            .append_local_write_log(&LocalWriteLogRecord {
                id: "write-conflict-main".to_string(),
                workspace_id: WorkspaceId::new("ws_code"),
                device_id: DeviceId::new("dev_mac"),
                project_id: Some(ProjectId::new("proj_web")),
                path: "apps/web/src/index.ts".to_string(),
                source_path: None,
                operation: "modify".to_string(),
                staged_content_id: None,
                policy_classification: PathClassification::WorkspaceSync,
                causation_id: "human".to_string(),
                settled_at: "2026-06-25T12:02:00Z".to_string(),
                created_at: "2026-06-25T12:02:00Z".to_string(),
            })
            .expect("main write");
        drop(store);
        let materialized = temp.root().join("Code/.work/apps/web/conflict-file/src");
        fs::create_dir_all(&materialized).expect("work src");
        fs::write(materialized.join("index.ts"), "work\n").expect("work file");

        let output = accept_work_view(WorkSelectorOptions {
            db_path: Some(db_path.clone()),
            selector: "conflict-file".to_string(),
            generated_at: now(),
        })
        .expect("accept");

        assert_eq!(serde_json::to_value(output.action).unwrap(), "review-ready");
        assert_eq!(
            serde_json::to_value(output.work_view.lifecycle).unwrap(),
            "review-ready"
        );
        assert_eq!(
            fs::read_to_string(project_path.join("src/index.ts")).expect("main file"),
            "main changed\n"
        );

        let status = compose_status(StatusOptions {
            db_path: Some(db_path.clone()),
            requested_path: Some(project_path.display().to_string()),
            workspace_scope: false,
            generated_at: now(),
        })
        .expect("status");
        assert_eq!(status.status.level, StatusLevel::Attention);

        discard_work_view(WorkSelectorOptions {
            db_path: Some(db_path.clone()),
            selector: "conflict-file".to_string(),
            generated_at: "2026-06-25T12:10:00Z".to_string(),
        })
        .expect("discard");
        let status = compose_status(StatusOptions {
            db_path: Some(db_path),
            requested_path: Some(project_path.display().to_string()),
            workspace_scope: false,
            generated_at: now(),
        })
        .expect("status");
        assert_eq!(status.status.level, StatusLevel::Healthy);
    }

    #[test]
    fn status_reports_durable_review_ready_work_view_without_event() {
        let (temp, db_path) = seeded_store("phase9-status-durable-work-view");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(&project_path).expect("project");
        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "durable-review".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("work view");

        let store = MetadataStore::open(&db_path).expect("metadata");
        let workspace = store
            .current_workspace()
            .expect("workspace query")
            .expect("workspace");
        let mut view = store
            .work_views_by_name(&workspace.id, None, "durable-review")
            .expect("work views")
            .pop()
            .expect("work view");
        let view_id = view.id.as_str().to_string();
        view.lifecycle = WorkViewLifecycle::ReviewReady;
        view.sync_state = WorkViewSyncState::Attention;
        store.upsert_work_view(&view).expect("review ready");
        drop(store);

        let status = compose_status(StatusOptions {
            db_path: Some(db_path),
            requested_path: Some(project_path.display().to_string()),
            workspace_scope: false,
            generated_at: now(),
        })
        .expect("status");

        assert_eq!(status.status.level, StatusLevel::Attention);
        assert!(status.items.iter().any(|item| {
            item.kind == bowline_core::status::StatusItemKind::WorkView
                && item
                    .subject
                    .as_ref()
                    .is_some_and(|subject| subject.id == view_id.as_str())
        }));
    }

    #[test]
    fn accept_rejects_symlinked_work_view_entries() {
        let (temp, db_path) = seeded_store("phase9-accept-symlink");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(&project_path).expect("project");
        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "symlink-file".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("work view");
        let outside = temp.root().join("outside-secret");
        fs::write(&outside, "do not copy").expect("outside");
        let work_root = temp.root().join("Code/.work/apps/web/symlink-file");
        symlink(&outside, work_root.join("linked")).expect("symlink");

        let error = accept_work_view(WorkSelectorOptions {
            db_path: Some(db_path),
            selector: "symlink-file".to_string(),
            generated_at: now(),
        })
        .expect_err("symlink should be rejected");

        assert!(error.to_string().contains("symlinks are not followed"));
        assert!(!project_path.join("linked").exists());
    }

    #[test]
    fn accept_rejects_symlinked_work_view_root() {
        let (temp, db_path) = seeded_store("phase9-accept-root-symlink");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(&project_path).expect("project");
        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "root-link".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("work view");
        let work_root = temp.root().join("Code/.work/apps/web/root-link");
        fs::remove_dir_all(&work_root).expect("remove work root");
        let outside = temp.root().join("outside-work-root");
        fs::create_dir_all(outside.join("src")).expect("outside src");
        fs::write(outside.join("src/leak.ts"), "leak\n").expect("outside file");
        symlink(&outside, &work_root).expect("root symlink");

        let error = accept_work_view(WorkSelectorOptions {
            db_path: Some(db_path),
            selector: "root-link".to_string(),
            generated_at: now(),
        })
        .expect_err("symlinked work root should fail");

        assert!(error.to_string().contains("work view root escapes .work"));
        assert!(!project_path.join("src/leak.ts").exists());
    }

    #[test]
    fn cleanup_rejects_tampered_materialization_outside_work_namespace() {
        let (temp, db_path) = seeded_store("phase9-cleanup-tamper");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(&project_path).expect("project");
        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "tampered".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("work view");
        let store = MetadataStore::open(&db_path).expect("metadata");
        let workspace = store
            .current_workspace()
            .expect("workspace query")
            .expect("workspace");
        let mut view = store
            .work_views_by_name(&workspace.id, None, "tampered")
            .expect("work views")
            .pop()
            .expect("work view");
        let outside = temp.root().join("outside-do-not-delete");
        fs::create_dir_all(&outside).expect("outside");
        view.host_materializations = vec![outside.display().to_string()];
        view.lifecycle = WorkViewLifecycle::Discarded;
        view.visibility = WorkViewVisibility::Hidden;
        store.upsert_work_view(&view).expect("tampered view");
        drop(store);

        let error = cleanup_work_views(WorkCleanupOptions {
            db_path: Some(db_path),
            apply: true,
            generated_at: now(),
        })
        .expect_err("outside cleanup should be rejected");

        assert!(error.to_string().contains("cleanup is limited to .work"));
        assert!(outside.is_dir());
    }

    #[test]
    fn cleanup_rejects_parent_component_materialization_escape() {
        let (temp, db_path) = seeded_store("phase9-cleanup-parent-traversal");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(project_path.join("keep")).expect("project keep dir");
        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "traversal".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("work view");
        let store = MetadataStore::open(&db_path).expect("metadata");
        let workspace = store
            .current_workspace()
            .expect("workspace query")
            .expect("workspace");
        let mut view = store
            .work_views_by_name(&workspace.id, None, "traversal")
            .expect("work views")
            .pop()
            .expect("work view");
        let traversal = temp
            .root()
            .join("Code/.work/apps/web/traversal/../../../../apps/web/keep");
        assert!(traversal.exists());
        view.host_materializations = vec![traversal.display().to_string()];
        view.lifecycle = WorkViewLifecycle::Discarded;
        view.visibility = WorkViewVisibility::Hidden;
        store.upsert_work_view(&view).expect("traversal view");
        drop(store);

        let error = cleanup_work_views(WorkCleanupOptions {
            db_path: Some(db_path),
            apply: true,
            generated_at: now(),
        })
        .expect_err("parent traversal cleanup should be rejected");

        assert!(error.to_string().contains("cleanup is limited to .work"));
        assert!(project_path.join("keep").is_dir());
    }

    #[test]
    fn cleanup_rejects_symlinked_work_namespace_root() {
        let (temp, db_path) = seeded_store("phase9-cleanup-namespace-symlink");
        let project_path = temp.root().join("Code/apps/web");
        fs::create_dir_all(&project_path).expect("project");
        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: "symlink-namespace".to_string(),
            owner_device_id: None,
            generated_at: now(),
        })
        .expect("work view");
        discard_work_view(WorkSelectorOptions {
            db_path: Some(db_path.clone()),
            selector: "symlink-namespace".to_string(),
            generated_at: now(),
        })
        .expect("discard");
        let namespace_root = temp.root().join("Code/.work/apps/web");
        fs::remove_dir_all(&namespace_root).expect("remove namespace");
        let outside = temp.root().join("outside-do-not-delete");
        fs::create_dir_all(outside.join("symlink-namespace")).expect("outside target");
        symlink(&outside, &namespace_root).expect("namespace symlink");

        let error = cleanup_work_views(WorkCleanupOptions {
            db_path: Some(db_path),
            apply: true,
            generated_at: now(),
        })
        .expect_err("symlinked namespace should be rejected");

        assert!(
            error
                .to_string()
                .contains("cleanup namespace escapes .work")
        );
        assert!(outside.join("symlink-namespace").is_dir());
    }

    fn seeded_store(name: &str) -> (TempWorkspace, std::path::PathBuf) {
        seeded_store_with_snapshot(name, true)
    }

    fn seeded_store_without_snapshot(name: &str) -> (TempWorkspace, std::path::PathBuf) {
        seeded_store_with_snapshot(name, false)
    }

    fn seeded_store_with_snapshot(
        name: &str,
        project_has_snapshot: bool,
    ) -> (TempWorkspace, std::path::PathBuf) {
        let temp = TempWorkspace::new(name).expect("temp workspace");
        let code_root = temp.root().join("Code");
        fs::create_dir_all(code_root.join("apps/web")).expect("project dir");
        let db_path = temp.root().join(".state/local.sqlite3");
        let store = MetadataStore::open(&db_path).expect("metadata");
        let workspace_id = WorkspaceId::new("ws_code");
        let project_id = ProjectId::new("proj_web");
        store
            .insert_workspace(&workspace_id, "User Code", "2026-06-25T00:00:00Z")
            .expect("workspace");
        store
            .insert_root(
                "root_code",
                &workspace_id,
                &code_root.display().to_string(),
                "2026-06-25T00:00:00Z",
            )
            .expect("root");
        store
            .insert_project(
                &project_id,
                &workspace_id,
                "root_code",
                "apps/web",
                "2026-06-25T00:00:00Z",
            )
            .expect("project");
        if project_has_snapshot {
            store
                .set_project_latest_snapshot_id(
                    &workspace_id,
                    &project_id,
                    &SnapshotId::new("snap_project_base"),
                )
                .expect("project latest snapshot");
        }
        drop(store);
        (temp, db_path)
    }

    fn commit_test_snapshot_manifest(
        control_plane: &FakeControlPlaneClient,
        workspace_id: &str,
        snapshot_id: &str,
        device_id: &str,
    ) {
        let manifest_content_id = format!("content_manifest_{snapshot_id}");
        let pack_content_id = format!("content_pack_{snapshot_id}");
        let manifest_upload = control_plane
            .create_upload_intent(
                UploadIntentRequest::new(workspace_id, ControlObjectKind::SnapshotManifest, 64)
                    .with_content_id(&manifest_content_id),
            )
            .expect("snapshot manifest upload intent");
        let pack_upload = control_plane
            .create_upload_intent(
                UploadIntentRequest::new(workspace_id, ControlObjectKind::SourcePack, 256)
                    .with_content_id(&pack_content_id),
            )
            .expect("source pack upload intent");

        control_plane
            .commit_object_manifest(ObjectManifestCommit {
                workspace_id: workspace_id.to_string(),
                snapshot_id: snapshot_id.to_string(),
                manifest_id: format!("manifest_{snapshot_id}"),
                manifest_object: ObjectPointer {
                    object_key: manifest_upload.object_key,
                    content_id: manifest_content_id,
                    byte_len: 64,
                    hash: format!("b3_manifest_{snapshot_id}"),
                    key_epoch: 1,
                    kind: ControlObjectKind::SnapshotManifest,
                    created_at: ControlPlaneTimestamp { tick: 90 },
                },
                pack_objects: vec![ObjectPointer {
                    object_key: pack_upload.object_key,
                    content_id: pack_content_id,
                    byte_len: 256,
                    hash: format!("b3_pack_{snapshot_id}"),
                    key_epoch: 1,
                    kind: ControlObjectKind::SourcePack,
                    created_at: ControlPlaneTimestamp { tick: 91 },
                }],
                committed_by_device_id: device_id.to_string(),
            })
            .expect("snapshot manifest commit");
    }

    fn reserve_test_overlay_object(
        control_plane: &FakeControlPlaneClient,
        workspace_id: &str,
        content_id: &str,
        created_at_tick: u64,
    ) -> ObjectPointer {
        let upload = control_plane
            .create_upload_intent(
                UploadIntentRequest::new(workspace_id, ControlObjectKind::AgentOverlay, 512)
                    .with_content_id(content_id),
            )
            .expect("overlay upload intent");
        ObjectPointer {
            object_key: upload.object_key,
            content_id: content_id.to_string(),
            byte_len: 512,
            hash: format!("b3_{content_id}"),
            key_epoch: 1,
            kind: ControlObjectKind::AgentOverlay,
            created_at: ControlPlaneTimestamp {
                tick: created_at_tick,
            },
        }
    }

    fn now() -> String {
        "2026-06-25T12:00:00Z".to_string()
    }

    fn display(path: &std::path::Path) -> String {
        path.display().to_string()
    }
}
