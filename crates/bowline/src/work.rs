//! The `work` command family, rewired onto the manifest-sync engine
//! (Plan 112). The CLI owns work-view *state*: the metadata DB remains the
//! naming registry (project, paths, timestamps, visibility) and the synced aux
//! index (`.bowline-meta/aux-index`) is the engine truth (base/overlay manifest
//! keys + lifecycle). The daemon executes the engine operations that need live
//! transport — `work.create` (materialize the head into the view directory),
//! `work.review` (capture + manifest diff), `work.accept` (capture + three-way
//! merge + CAS publish). List, discard, restore, and cleanup are in-process
//! metadata + aux-file operations.

use std::collections::BTreeSet;
use std::path::PathBuf;

use bowline_core::commands::{
    CONTRACT_VERSION, CommandName, WorkCleanupCommandOutput, WorkCreateCommandOutput,
    WorkDiffCommandOutput, WorkLifecycleCommandOutput, WorkListCommandOutput,
};
use bowline_core::events::EventName;
use bowline_core::ids::{DeviceId, SnapshotId};
use bowline_core::status::{RepairCommand, WorkspaceStatus};
use bowline_core::work_views::{
    OVERLAY_HEAD_EMPTY, WorkCommandAction, WorkDiffChangeKind, WorkView,
    WorkViewLifecycle as WireLifecycle, WorkViewRetention, WorkViewRetentionState,
    WorkViewSyncState, WorkViewVisibility,
};
use bowline_local::metadata::{MetadataStore, ProjectRecord};
use bowline_local::scanner::scan_workspace_scoped;
use bowline_local::sync::manifest_engine::aux_index::{
    AuxIndex, WorkViewId as AuxWorkViewId, WorkViewLifecycle as AuxLifecycle, WorkViewRecord,
};
use bowline_local::sync::manifest_engine::manifest::ManifestKey;
use bowline_local::sync::manifest_engine::work_view_cli::{
    overlay_engine_truth, read_aux_index_file, wire_diff_entries, write_aux_index_file,
};
use bowline_local::work_views::{
    WorkAcceptTransition, WorkCleanupOptions, WorkListOptions, WorkViewError, append_work_event,
    apply_accept_success, cleanup_work_views, discard_work_view, display_path, expand_display_path,
    list_work_views, open_store, overlay_aux_engine_truth, reconcile_aux_work_views,
    resolve_work_view, restore_work_view, validate_work_view_name, visible_path, work_view_id,
};
use serde::Deserialize;

use crate::surface::style::{self, Presentation, Role};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkCreateArgs {
    pub project_path: String,
    pub name: String,
    pub from: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkListArgs {
    pub include_hidden: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkSelectorArgs {
    pub selector: String,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkCleanupArgs {
    pub apply: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkLifecycle {
    Accept,
    Discard,
    Restore,
}

impl WorkLifecycle {
    pub fn command_name(self) -> CommandName {
        match self {
            Self::Accept => CommandName::Accept,
            Self::Discard => CommandName::Discard,
            Self::Restore => CommandName::Restore,
        }
    }
}

// ---- errors -----------------------------------------------------------------

/// A work command failure: either a workspace/selector-state error (the frozen
/// `WorkViewError` surface) or a daemon RPC failure — the engine operations run
/// in the daemon, so an unreachable daemon is a retryable runtime error.
#[derive(Debug)]
pub enum WorkCommandError {
    View(WorkViewError),
    Daemon {
        operation: &'static str,
        detail: String,
    },
}

impl std::fmt::Display for WorkCommandError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::View(error) => error.fmt(formatter),
            Self::Daemon { operation, detail } => write!(
                formatter,
                "work-view {operation} needs the bowline daemon: {detail}. Start it with `bowline daemon start` and retry."
            ),
        }
    }
}

impl std::error::Error for WorkCommandError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::View(error) => Some(error),
            Self::Daemon { .. } => None,
        }
    }
}

impl From<WorkViewError> for WorkCommandError {
    fn from(error: WorkViewError) -> Self {
        Self::View(error)
    }
}

impl From<bowline_local::sync::manifest_engine::work_view_cli::WorkViewCliError>
    for WorkCommandError
{
    fn from(error: bowline_local::sync::manifest_engine::work_view_cli::WorkViewCliError) -> Self {
        Self::View(WorkViewError::Index(error))
    }
}

// ---- daemon RPC wire shapes -------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkCreateRpcResult {
    base_manifest_key: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkReviewRpcResult {
    overlay_manifest_key: String,
    changes: Vec<WorkChangeRpc>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkChangeRpc {
    path: String,
    kind: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkAcceptRpcResult {
    overlay_manifest_key: String,
    base_manifest_key: String,
    #[serde(default)]
    conflict_asides: Vec<String>,
    #[serde(default)]
    discarded_deletions: Vec<String>,
    #[serde(default)]
    accepted_paths: Vec<String>,
}

fn call_work_rpc<T: serde::de::DeserializeOwned>(
    operation: &'static str,
    method: &str,
    params: serde_json::Value,
) -> Result<T, WorkCommandError> {
    let value =
        crate::wire::call_work_rpc(method, &params).map_err(|error| WorkCommandError::Daemon {
            operation,
            detail: error.to_string(),
        })?;
    serde_json::from_value(value).map_err(|error| WorkCommandError::Daemon {
        operation,
        detail: format!("daemon returned an unexpected response: {error}"),
    })
}

// ---- shared context ---------------------------------------------------------

struct AuxState {
    root: PathBuf,
    aux: AuxIndex,
}

impl AuxState {
    fn load(store: &MetadataStore) -> Result<Self, WorkCommandError> {
        let root = store
            .current_workspace_root()
            .map_err(WorkViewError::from)?
            .ok_or(WorkViewError::MissingWorkspaceRoot)?;
        let root = expand_display_path(&root);
        let aux = read_aux_index_file(&root)?;
        Ok(Self { root, aux })
    }

    fn record(&self, view: &WorkView, selector: &str) -> Result<&WorkViewRecord, WorkCommandError> {
        self.aux
            .get(&AuxWorkViewId::new(view.id.as_str()))
            .ok_or_else(|| {
                WorkCommandError::View(WorkViewError::MissingWorkView {
                    selector: selector.to_string(),
                })
            })
    }

    fn write(&self) -> Result<(), WorkCommandError> {
        write_aux_index_file(&self.root, &self.aux)?;
        Ok(())
    }
}

// ---- create -----------------------------------------------------------------

pub fn run_work_create(
    args: WorkCreateArgs,
    db_path: Option<PathBuf>,
    owner_device_id: DeviceId,
    generated_at: String,
) -> Result<WorkCreateCommandOutput, WorkCommandError> {
    validate_work_view_name(&args.name)?;
    // Base selection died with the snapshot model: a view always forks from the
    // current synced head (the manifest CAS ref). An explicit `--from` selector
    // has nothing to resolve against.
    if let Some(selector) = args.from {
        return Err(WorkViewError::UnknownBaseSnapshot { selector }.into());
    }
    let store = open_store(db_path.as_deref())?;
    let workspace = store
        .current_workspace()
        .map_err(WorkViewError::from)?
        .ok_or(WorkViewError::MissingWorkspace)?;
    let root = store
        .current_workspace_root()
        .map_err(WorkViewError::from)?
        .ok_or(WorkViewError::MissingWorkspaceRoot)?;
    let project = resolve_or_register_git_project(
        &store,
        &workspace.id,
        &root,
        &args.project_path,
        &generated_at,
    )?;
    reconcile_aux_work_views(&store)?;

    let existing = store
        .work_views_by_name(&workspace.id, Some(&project.id), &args.name)
        .map_err(WorkViewError::from)?;
    if let [row] = existing.as_slice() {
        if matches!(
            row.lifecycle,
            WireLifecycle::Active | WireLifecycle::ReviewReady
        ) {
            let mut row = row.clone();
            let visible = expand_display_path(&row.visible_path);
            if !visible.is_dir() {
                let aux_state = AuxState::load(&store)?;
                let record = aux_state.record(&row, &args.name)?;
                let _: WorkCreateRpcResult = call_work_rpc(
                    "create",
                    "work.create",
                    serde_json::json!({
                        "viewDir": visible.display().to_string(),
                        "projectPath": &project.path,
                        "overlayManifestKey": record.overlay_manifest_key.as_str(),
                    }),
                )?;
                row.host_materializations = vec![display_path(&visible)];
                store.upsert_work_view(&row).map_err(WorkViewError::from)?;
            }
            overlay_aux_engine_truth(&store, std::slice::from_mut(&mut row))?;
            return Ok(work_create_output(
                WorkCommandAction::Reused,
                row,
                generated_at,
            ));
        }
        return Err(WorkViewError::NameCollision {
            name: args.name,
            project_path: project.path,
        }
        .into());
    }

    let visible = visible_path(&root, &project.path, &args.name);
    let created: WorkCreateRpcResult = call_work_rpc(
        "create",
        "work.create",
        serde_json::json!({
            "viewDir": visible.display().to_string(),
            "projectPath": &project.path,
        }),
    )?;
    let base = created.base_manifest_key;

    let mut aux_state = AuxState::load(&store)?;
    let id = work_view_id(workspace.id.as_str(), project.id.as_str(), &args.name);
    aux_state.aux.upsert(
        AuxWorkViewId::new(id.as_str()),
        WorkViewRecord {
            project_id: project.id.clone(),
            project_path: project.path.clone(),
            name: args.name.clone(),
            owner_device_id: owner_device_id.clone(),
            created_at: generated_at.clone(),
            updated_at: generated_at.clone(),
            base_manifest_key: ManifestKey::new(base.clone()),
            overlay_manifest_key: ManifestKey::new(base.clone()),
            lifecycle: AuxLifecycle::Active,
        },
    );
    aux_state.write()?;

    let work_view = WorkView {
        id,
        workspace_id: workspace.id,
        project_id: project.id,
        project_path: project.path,
        name: args.name,
        visible_path: display_path(&visible),
        base_snapshot_id: SnapshotId::new(base),
        overlay_head: OVERLAY_HEAD_EMPTY.to_string(),
        overlay_version: 0,
        env_profile: "default".to_string(),
        lifecycle: WireLifecycle::Active,
        visibility: WorkViewVisibility::DefaultVisible,
        sync_state: WorkViewSyncState::LocalOnly,
        retention: WorkViewRetention {
            state: WorkViewRetentionState::Current,
            retain_until: None,
            restorable: false,
        },
        owner_device_id: Some(owner_device_id),
        followed_by: Vec::new(),
        host_materializations: vec![display_path(&visible)],
        attention: Vec::new(),
        created_at: generated_at.clone(),
        updated_at: generated_at.clone(),
    };
    store
        .upsert_work_view(&work_view)
        .map_err(WorkViewError::from)?;
    append_work_event(&store, EventName::WorkCreated, &work_view, &generated_at);
    Ok(work_create_output(
        WorkCommandAction::Created,
        work_view,
        generated_at,
    ))
}

fn resolve_or_register_git_project(
    store: &MetadataStore,
    workspace_id: &bowline_core::ids::WorkspaceId,
    workspace_root: &str,
    requested_path: &str,
    generated_at: &str,
) -> Result<ProjectRecord, WorkCommandError> {
    if let Some(project) = store
        .current_project_by_path(requested_path)
        .map_err(WorkViewError::from)?
    {
        return Ok(project);
    }

    let root =
        std::fs::canonicalize(expand_display_path(workspace_root)).map_err(WorkViewError::from)?;
    let requested = std::fs::canonicalize(requested_path).map_err(WorkViewError::from)?;
    if !requested.starts_with(&root) {
        return Err(WorkViewError::MissingProject {
            path: requested_path.to_string(),
        }
        .into());
    }
    let mut candidate = if requested.is_dir() {
        requested.as_path()
    } else {
        requested.parent().unwrap_or(requested.as_path())
    };
    let git_root = loop {
        if candidate.join(".git").exists() {
            break candidate;
        }
        if candidate == root {
            return Err(WorkViewError::MissingProject {
                path: requested_path.to_string(),
            }
            .into());
        }
        candidate = candidate
            .parent()
            .filter(|parent| parent.starts_with(&root))
            .ok_or_else(|| WorkViewError::MissingProject {
                path: requested_path.to_string(),
            })?;
    };
    let relative = git_root
        .strip_prefix(&root)
        .map_err(|_| WorkViewError::MissingProject {
            path: requested_path.to_string(),
        })?;
    let relative = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    let report =
        scan_workspace_scoped(&root, &BTreeSet::from([relative.clone()])).map_err(|_| {
            WorkViewError::MissingProject {
                path: requested_path.to_string(),
            }
        })?;
    let observed = report
        .projects
        .into_iter()
        .find(|project| project.path == relative && project.has_git_repo)
        .ok_or_else(|| WorkViewError::MissingProject {
            path: requested_path.to_string(),
        })?;
    let root_id = store
        .accepted_root_id_for_path(workspace_id, workspace_root)
        .map_err(WorkViewError::from)?
        .ok_or(WorkViewError::MissingWorkspaceRoot)?;
    store
        .insert_project(
            &observed.id,
            workspace_id,
            &root_id,
            &observed.path,
            generated_at,
        )
        .map_err(WorkViewError::from)?;
    store
        .current_project_by_path(requested_path)
        .map_err(WorkViewError::from)?
        .ok_or_else(|| {
            WorkViewError::MissingProject {
                path: requested_path.to_string(),
            }
            .into()
        })
}

fn work_create_output(
    action: WorkCommandAction,
    work_view: WorkView,
    generated_at: String,
) -> WorkCreateCommandOutput {
    let next_actions = vec![RepairCommand::inspect(
        "Open the work view".to_string(),
        Some(format!(
            "cd {}",
            bowline_core::shell::quote_word(&work_view.visible_path)
        )),
    )];
    WorkCreateCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::WorkCreate,
        generated_at,
        action,
        work_view,
        status: WorkspaceStatus::healthy(),
        next_actions,
    }
}

// ---- list -------------------------------------------------------------------

pub fn run_list(
    args: WorkListArgs,
    db_path: Option<PathBuf>,
    current_device_id: DeviceId,
    generated_at: String,
) -> Result<WorkListCommandOutput, WorkCommandError> {
    list_work_views(WorkListOptions {
        db_path,
        include_hidden: args.include_hidden,
        current_device_id: Some(current_device_id),
        generated_at,
    })
    .map_err(Into::into)
}

// ---- diff / review ----------------------------------------------------------

pub fn run_diff(
    args: WorkSelectorArgs,
    db_path: Option<PathBuf>,
    generated_at: String,
) -> Result<WorkDiffCommandOutput, WorkCommandError> {
    let store = open_store(db_path.as_deref())?;
    let mut work_view = resolve_work_view(&store, &args.selector)?;
    let mut aux_state = AuxState::load(&store)?;
    let record = aux_state.record(&work_view, &args.selector)?.clone();

    let reviewed: WorkReviewRpcResult = call_work_rpc(
        "review",
        "work.review",
        serde_json::json!({
            "viewDir": expand_display_path(&work_view.visible_path).display().to_string(),
            "projectPath": &work_view.project_path,
            "baseManifestKey": record.base_manifest_key.as_str(),
            "overlayManifestKey": record.overlay_manifest_key.as_str(),
        }),
    )?;

    // Persist a capture-advanced overlay so accept and a later review agree.
    let mut record = record;
    if reviewed.overlay_manifest_key != record.overlay_manifest_key.as_str() {
        record.overlay_manifest_key = ManifestKey::new(reviewed.overlay_manifest_key.clone());
        aux_state
            .aux
            .upsert(AuxWorkViewId::new(work_view.id.as_str()), record.clone());
        aux_state.write()?;
    }
    overlay_engine_truth(&mut work_view, &record);

    let raw_changes = reviewed
        .changes
        .iter()
        .map(|change| (change.path.clone(), parse_change_kind(&change.kind)))
        .collect::<Vec<_>>();
    let changes = wire_diff_entries(&work_view.name, &raw_changes, &args.paths)?;
    let next_actions = vec![RepairCommand::mutating(
        "Accept work view".to_string(),
        Some(accept_command(&args.selector, &args.paths)),
    )];
    Ok(WorkDiffCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Diff,
        generated_at,
        action: WorkCommandAction::Diffed,
        work_view,
        changes,
        status: WorkspaceStatus::healthy(),
        next_actions,
    })
}

fn parse_change_kind(kind: &str) -> WorkDiffChangeKind {
    match kind {
        "added" => WorkDiffChangeKind::Added,
        "deleted" => WorkDiffChangeKind::Deleted,
        _ => WorkDiffChangeKind::Modified,
    }
}

fn accept_command(selector: &str, paths: &[String]) -> String {
    let mut command = format!(
        "bowline work accept {}",
        bowline_core::shell::quote_word(selector)
    );
    for path in paths {
        command.push_str(" --path ");
        command.push_str(&bowline_core::shell::quote_word(path));
    }
    command
}

// ---- lifecycle --------------------------------------------------------------

pub fn run_lifecycle(
    lifecycle: WorkLifecycle,
    args: WorkSelectorArgs,
    db_path: Option<PathBuf>,
    _current_device_id: DeviceId,
    generated_at: String,
) -> Result<WorkLifecycleCommandOutput, WorkCommandError> {
    match lifecycle {
        WorkLifecycle::Accept => run_accept(args, db_path, generated_at),
        WorkLifecycle::Discard => {
            discard_work_view(bowline_local::work_views::WorkSelectorOptions {
                db_path,
                selector: args.selector,
                paths: args.paths,
                generated_at,
            })
            .map_err(Into::into)
        }
        WorkLifecycle::Restore => {
            restore_work_view(bowline_local::work_views::WorkSelectorOptions {
                db_path,
                selector: args.selector,
                paths: args.paths,
                generated_at,
            })
            .map_err(Into::into)
        }
    }
}

fn run_accept(
    args: WorkSelectorArgs,
    db_path: Option<PathBuf>,
    generated_at: String,
) -> Result<WorkLifecycleCommandOutput, WorkCommandError> {
    let store = open_store(db_path.as_deref())?;
    let mut work_view = resolve_work_view(&store, &args.selector)?;
    let aux_state = AuxState::load(&store)?;
    let record = aux_state.record(&work_view, &args.selector)?.clone();
    if record.lifecycle != AuxLifecycle::Active {
        return Err(WorkViewError::InactiveWorkView {
            name: work_view.name,
        }
        .into());
    }
    let partial = !args.paths.is_empty();

    let accepted: WorkAcceptRpcResult = call_work_rpc(
        "accept",
        "work.accept",
        serde_json::json!({
            "viewDir": expand_display_path(&work_view.visible_path).display().to_string(),
            "projectPath": &work_view.project_path,
            "baseManifestKey": record.base_manifest_key.as_str(),
            "overlayManifestKey": record.overlay_manifest_key.as_str(),
            "paths": args.paths,
        }),
    )?;
    if partial && accepted.accepted_paths.is_empty() && accepted.discarded_deletions.is_empty() {
        // Nothing the selector matched changed anything — not even a discarded
        // deletion, which still counts as a matched (if overridden) change.
        return Err(WorkViewError::EmptyPathSelection {
            patterns: args.paths,
        }
        .into());
    }
    // The merged head carries any conflict-asides as ordinary files; they sync
    // to every device, so accept itself never blocks on them.
    let _conflict_asides = accepted.conflict_asides;

    // Project the accepted engine truth onto the wire row before the metadata
    // transition composes the output.
    let mut captured_record = record.clone();
    captured_record.overlay_manifest_key = ManifestKey::new(accepted.overlay_manifest_key.clone());
    overlay_engine_truth(&mut work_view, &captured_record);
    apply_accept_success(
        store,
        work_view,
        generated_at,
        WorkAcceptTransition {
            paths: accepted.accepted_paths,
            discarded_deletions: accepted.discarded_deletions,
            partial,
            captured_overlay: accepted.overlay_manifest_key,
            accepted_base: Some(accepted.base_manifest_key),
        },
    )
    .map_err(Into::into)
}

// ---- cleanup ----------------------------------------------------------------

pub fn run_cleanup(
    args: WorkCleanupArgs,
    db_path: Option<PathBuf>,
    generated_at: String,
) -> Result<WorkCleanupCommandOutput, WorkCommandError> {
    cleanup_work_views(WorkCleanupOptions {
        db_path,
        apply: args.apply,
        generated_at,
    })
    .map_err(Into::into)
}

// ---- human rendering --------------------------------------------------------

pub fn render_work_create_human(output: &WorkCreateCommandOutput) -> String {
    let pres = Presentation::detect(false);
    format!(
        "{}  {}\n{}  {}\n{}  {}\n\n",
        style::section("Work view", &pres),
        style::paint(&output.work_view.name, Role::Strong, &pres),
        style::section("Path", &pres),
        output.work_view.visible_path,
        style::section("State", &pres),
        style::paint("active", Role::Ready, &pres),
    )
}

pub fn render_list_human(output: &WorkListCommandOutput) -> String {
    let pres = Presentation::detect(false);
    let mut lines = vec![format!(
        "{}  {}",
        style::section("Work views", &pres),
        output.work_views.len()
    )];
    lines.extend(output.work_views.iter().map(|view| {
        format!(
            "  {}  {}  {}",
            style::paint(&view.name, Role::Strong, &pres),
            style::paint(&view.visible_path, Role::Label, &pres),
            style::paint(&style::kebab(&view.lifecycle), Role::Label, &pres),
        )
    }));
    lines.push(String::new());
    lines.join("\n")
}

pub fn render_diff_human(output: &WorkDiffCommandOutput) -> String {
    let pres = Presentation::detect(false);
    let mut lines = vec![format!(
        "{}  {}",
        style::section("Work view", &pres),
        style::paint(&output.work_view.name, Role::Strong, &pres)
    )];
    if output.changes.is_empty() {
        lines.push(format!(
            "  {}",
            style::paint("No local changes recorded.", Role::Label, &pres)
        ));
    } else {
        lines.extend(output.changes.iter().map(|change| {
            let redacted = if change.contains_secrets {
                style::paint("  (redacted)", Role::Label, &pres)
            } else {
                String::new()
            };
            format!(
                "  {} {}{redacted}",
                style::paint(&style::kebab(&change.kind), Role::Label, &pres),
                change.path,
            )
        }));
    }
    lines.push(String::new());
    lines.join("\n")
}

pub fn render_lifecycle_human(output: &WorkLifecycleCommandOutput) -> String {
    let pres = Presentation::detect(false);
    let mut text = format!(
        "{}  {}\n{}  {}\n",
        style::section("Work view", &pres),
        style::paint(&output.work_view.name, Role::Strong, &pres),
        style::section("State", &pres),
        style::kebab(&output.work_view.lifecycle),
    );
    // A discarded deletion is the one accept outcome with no file to discover, so
    // it must be spelled out: the view's deletion did not land because the live
    // workspace edit is newer and stays canonical.
    if !output.discarded_deletions.is_empty() {
        text.push_str(&format!(
            "{}  the workspace edited these since the fork, so the deletion did not land\n",
            style::section("Kept (deletion skipped)", &pres),
        ));
        for path in &output.discarded_deletions {
            text.push_str(&format!("  {}\n", style::paint(path, Role::Label, &pres)));
        }
    }
    text.push('\n');
    text
}

pub fn render_cleanup_human(output: &WorkCleanupCommandOutput) -> String {
    let pres = Presentation::detect(false);
    let mut lines = vec![format!(
        "{}  {}",
        style::section("Cleanup candidates", &pres),
        output.previewed_paths.len()
    )];
    if output.deleted_paths.is_empty() {
        lines.extend(
            output
                .previewed_paths
                .iter()
                .map(|path| format!("  {}", style::paint(path, Role::Label, &pres))),
        );
    } else {
        lines.extend(
            output
                .deleted_paths
                .iter()
                .map(|path| format!("  {} {path}", style::paint("deleted", Role::Limited, &pres))),
        );
    }
    lines.push(String::new());
    lines.join("\n")
}
