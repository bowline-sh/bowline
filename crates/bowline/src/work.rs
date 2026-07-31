//! The `work` command family, rewired onto the manifest-sync engine
//! (Plan 112). The CLI owns work-view *state*: the metadata DB remains the
//! naming registry (project, paths, timestamps, visibility) and the synced aux
//! index (`.bowline-meta/aux-index`) is the engine truth (base/overlay manifest
//! keys + lifecycle). The daemon executes the engine operations that need live
//! transport — `work.create` (materialize the head into the view directory),
//! `work.review` (capture + manifest diff), `work.accept` (capture + three-way
//! merge + CAS publish). List, discard, restore, and cleanup are in-process
//! metadata + aux-file operations.

use std::path::PathBuf;

use bowline_core::commands::{
    CONTRACT_VERSION, CommandName, WorkCleanupCommandOutput, WorkCreateCommandOutput,
    WorkDiffCommandOutput, WorkLifecycleCommandOutput, WorkListCommandOutput,
};
use bowline_core::ids::DeviceId;
use bowline_core::status::{RepairCommand, WorkspaceStatus};
use bowline_core::work_views::{
    WorkCommandAction, WorkDiffChangeKind, WorkUnresolvedPath, WorkView,
};
use bowline_local::metadata::MetadataStore;
use bowline_local::sync::manifest_engine::aux_index::{
    AuxIndex, WorkViewId as AuxWorkViewId, WorkViewLifecycle as AuxLifecycle, WorkViewRecord,
};
use bowline_local::sync::manifest_engine::manifest::ManifestKey;
use bowline_local::sync::manifest_engine::work_view_cli::{
    overlay_engine_truth, read_aux_index_file, wire_diff_entries, write_aux_index_file,
};
use bowline_local::work_views::{
    WorkAcceptTransition, WorkCleanupOptions, WorkListOptions, WorkViewError,
    acquire_work_view_transition_lock, apply_accept_success, cleanup_work_views, discard_work_view,
    expand_display_path, list_work_views, materialization_snapshot, open_store, resolve_work_view,
    restore_work_view,
};
use serde::{Deserialize, Serialize};

mod create;
mod render;

pub use create::run_work_create;
pub use render::{
    render_cleanup_human, render_diff_human, render_lifecycle_human, render_list_human,
    render_work_create_human,
};

/// Accept can write conflict-aside files into the user's project. They are
/// ordinary synced files, so accept never blocks on them — but the user has to
/// be told they exist, or they find unexplained files with no record of which
/// accept produced them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkLifecycleRunOutput {
    pub output: WorkLifecycleCommandOutput,
    pub conflict_asides: Vec<String>,
}

impl WorkLifecycleRunOutput {
    fn without_conflicts(output: WorkLifecycleCommandOutput) -> Self {
        Self {
            output,
            conflict_asides: Vec::new(),
        }
    }

    pub fn json_payload(&self) -> WorkLifecycleJsonPayload<'_> {
        WorkLifecycleJsonPayload {
            output: &self.output,
            conflict_asides: &self.conflict_asides,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkLifecycleJsonPayload<'a> {
    #[serde(flatten)]
    output: &'a WorkLifecycleCommandOutput,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    conflict_asides: &'a [String],
}

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
    pub acknowledged_unresolved: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkCleanupArgs {
    pub apply: bool,
    pub acknowledged_unresolved: Vec<String>,
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
    #[serde(default)]
    unresolved_paths: Vec<WorkUnresolvedPath>,
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
    published_manifest_key: String,
    #[serde(default)]
    conflict_asides: Vec<String>,
    #[serde(default)]
    discarded_deletions: Vec<String>,
    #[serde(default)]
    aside_refused_paths: Vec<String>,
    #[serde(default)]
    accepted_paths: Vec<String>,
    #[serde(default)]
    unresolved_paths: Vec<WorkUnresolvedPath>,
    #[serde(default)]
    local_rebase_pending: bool,
    #[serde(default)]
    local_rebase_error: Option<String>,
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
    let _transition_lock = acquire_work_view_transition_lock(&store)?;
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
        record.generation = record.generation.checked_next().ok_or_else(|| {
            WorkViewError::WorkViewGenerationExhausted {
                name: work_view.name.clone(),
            }
        })?;
        aux_state
            .aux
            .upsert(AuxWorkViewId::new(work_view.id.as_str()), record.clone());
        aux_state.write()?;
    }
    overlay_engine_truth(&mut work_view, &record);
    store
        .upsert_work_view(&work_view)
        .map_err(WorkViewError::from)?;

    let raw_changes = reviewed
        .changes
        .iter()
        .map(|change| (change.path.clone(), parse_change_kind(&change.kind)))
        .collect::<Vec<_>>();
    let changes = wire_diff_entries(&work_view.name, &raw_changes, &args.paths)?;
    let mut next_actions = vec![RepairCommand::mutating(
        "Accept work view".to_string(),
        Some(accept_command(
            &args.selector,
            &args.paths,
            &reviewed.unresolved_paths,
        )),
    )];
    let status = unresolved_status(&reviewed.unresolved_paths);
    if !reviewed.unresolved_paths.is_empty() {
        next_actions.push(RepairCommand::inspect(
            "Resolve unreadable or changing work-view paths, then review again".to_string(),
            Some(format!(
                "bowline work review {}",
                bowline_core::shell::quote_word(&args.selector)
            )),
        ));
    }
    Ok(WorkDiffCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Diff,
        generated_at,
        action: WorkCommandAction::Diffed,
        work_view,
        changes,
        unresolved_paths: reviewed.unresolved_paths,
        status,
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

fn accept_command(
    selector: &str,
    paths: &[String],
    unresolved_paths: &[WorkUnresolvedPath],
) -> String {
    let mut command = format!(
        "bowline work accept {}",
        bowline_core::shell::quote_word(selector)
    );
    for path in paths {
        command.push_str(" --path ");
        command.push_str(&bowline_core::shell::quote_word(path));
    }
    for issue in unresolved_paths {
        command.push_str(" --acknowledge-unresolved ");
        command.push_str(&bowline_core::shell::quote_word(&format!(
            "{}={}",
            issue.path, issue.reason
        )));
    }
    command
}

fn unresolved_status(unresolved_paths: &[WorkUnresolvedPath]) -> WorkspaceStatus {
    if unresolved_paths.is_empty() {
        return WorkspaceStatus::healthy();
    }
    WorkspaceStatus {
        level: bowline_core::status::StatusLevel::Attention,
        attention_items: unresolved_paths
            .iter()
            .map(|issue| format!("{} could not be captured: {}", issue.path, issue.reason))
            .collect(),
    }
}

// ---- lifecycle --------------------------------------------------------------

pub fn run_lifecycle(
    lifecycle: WorkLifecycle,
    args: WorkSelectorArgs,
    db_path: Option<PathBuf>,
    _current_device_id: DeviceId,
    generated_at: String,
) -> Result<WorkLifecycleRunOutput, WorkCommandError> {
    match lifecycle {
        WorkLifecycle::Accept => run_accept(args, db_path, generated_at),
        WorkLifecycle::Discard => {
            let reviewed = run_diff(args.clone(), db_path.clone(), generated_at.clone())?;
            require_unresolved_acknowledgements(
                &reviewed.unresolved_paths,
                &args.acknowledged_unresolved,
            )?;
            let expected_materializations = snapshot_materializations(&reviewed.work_view)?;
            let mut output = discard_work_view(bowline_local::work_views::WorkSelectorOptions {
                db_path,
                selector: args.selector,
                paths: args.paths,
                generated_at,
                expected_materializations,
            })?;
            output.unresolved_paths = reviewed.unresolved_paths;
            output.status = unresolved_status(&output.unresolved_paths);
            Ok(WorkLifecycleRunOutput::without_conflicts(output))
        }
        WorkLifecycle::Restore => {
            restore_work_view(bowline_local::work_views::WorkSelectorOptions {
                db_path,
                selector: args.selector,
                paths: args.paths,
                generated_at,
                expected_materializations: Default::default(),
            })
            .map(WorkLifecycleRunOutput::without_conflicts)
            .map_err(Into::into)
        }
    }
}

fn require_unresolved_acknowledgements(
    unresolved_paths: &[WorkUnresolvedPath],
    acknowledged: &[String],
) -> Result<(), WorkCommandError> {
    let required = unresolved_paths
        .iter()
        .map(|issue| format!("{}={}", issue.path, issue.reason))
        .collect::<std::collections::BTreeSet<_>>();
    if required == acknowledged.iter().cloned().collect() {
        return Ok(());
    }
    Err(WorkViewError::UnresolvedPaths {
        paths: unresolved_paths.to_vec(),
    }
    .into())
}

fn run_accept(
    args: WorkSelectorArgs,
    db_path: Option<PathBuf>,
    generated_at: String,
) -> Result<WorkLifecycleRunOutput, WorkCommandError> {
    let store = open_store(db_path.as_deref())?;
    let _transition_lock = acquire_work_view_transition_lock(&store)?;
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
            "paths": &args.paths,
            "acknowledgedUnresolved": &args.acknowledged_unresolved,
        }),
    )?;
    if partial
        && accepted.accepted_paths.is_empty()
        && accepted.discarded_deletions.is_empty()
        && accepted.aside_refused_paths.is_empty()
    {
        // Nothing the selector matched changed anything — not even a discarded
        // deletion or a refused aside, each of which still counts as a matched
        // (if overridden) change.
        return Err(WorkViewError::EmptyPathSelection {
            patterns: args.paths,
        }
        .into());
    }
    // The merged head carries any conflict-asides as ordinary files; they sync
    // to every device, so accept itself never blocks on them — it reports them.
    let conflict_asides = accepted.conflict_asides;
    record_accept_conflicts(&store, &work_view, &conflict_asides, &generated_at);

    // Project the accepted engine truth onto the wire row before the metadata
    // transition composes the output.
    let mut captured_record = record.clone();
    captured_record.overlay_manifest_key = ManifestKey::new(accepted.overlay_manifest_key.clone());
    overlay_engine_truth(&mut work_view, &captured_record);
    let mut output = apply_accept_success(
        store,
        work_view,
        generated_at,
        WorkAcceptTransition {
            paths: accepted.accepted_paths,
            discarded_deletions: accepted.discarded_deletions,
            aside_refused_paths: accepted.aside_refused_paths,
            partial,
            local_completion_pending: accepted.local_rebase_pending,
            expected_generation: record.generation,
            captured_overlay: accepted.overlay_manifest_key,
            accepted_base: Some(accepted.base_manifest_key),
        },
    )?;
    output.unresolved_paths = accepted.unresolved_paths;
    output.status = unresolved_status(&output.unresolved_paths);
    if accepted.local_rebase_pending {
        let detail = accepted
            .local_rebase_error
            .as_deref()
            .unwrap_or("local work-view rebase did not complete");
        eprintln!(
            "bowline work accept: accepted remotely at {}; local rebase pending: {detail}",
            accepted.published_manifest_key
        );
        output.next_actions.push(RepairCommand::mutating(
            "Finish the accepted work view's local rebase".to_string(),
            Some(accept_command(
                &args.selector,
                &args.paths,
                &output.unresolved_paths,
            )),
        ));
    }
    // A next action with no command is a dead end; every aside accept wrote is
    // reconcilable by the same two verbs status points at, so name them.
    if let Some(first) = conflict_asides.first() {
        output.next_actions.push(RepairCommand::inspect(
            format!("List the {} accept wrote", conflict_noun(&conflict_asides)),
            Some("bowline conflicts".to_string()),
        ));
        output.next_actions.push(RepairCommand::inspect(
            "Compare both versions of the first one".to_string(),
            Some(format!(
                "bowline resolve {} --diff",
                bowline_core::shell::quote_word(first)
            )),
        ));
    }
    Ok(WorkLifecycleRunOutput {
        output,
        conflict_asides,
    })
}

/// Put every aside accept just wrote on the workspace timeline.
///
/// The files themselves are what status and `bowline conflicts` read, so the
/// accept has already succeeded by the time this runs and a metadata failure
/// must not undo it. It is never silent either: the reason goes to stderr.
/// Aside paths are project-relative, and the timeline is workspace-scoped.
fn record_accept_conflicts(
    store: &MetadataStore,
    work_view: &WorkView,
    asides: &[String],
    generated_at: &str,
) {
    for aside in asides {
        let aside_path = format!("{}/{aside}", work_view.project_path);
        let Some(origin_path) =
            bowline_local::sync::manifest_engine::conflict_aside_origin(&aside_path)
        else {
            continue;
        };
        let subject = bowline_local::events::ConflictEventSubject {
            workspace_id: &work_view.workspace_id,
            project_id: Some(&work_view.project_id),
            origin_path,
            aside_path: &aside_path,
            occurred_at: generated_at,
        };
        if let Err(error) = store.append_conflict_created(&subject) {
            eprintln!("bowline work accept: conflict timeline not updated: {error}");
        }
    }
}

fn conflict_noun(asides: &[String]) -> String {
    match asides.len() {
        1 => "conflict".to_string(),
        count => format!("{count} conflicts"),
    }
}

// ---- cleanup ----------------------------------------------------------------

pub fn run_cleanup(
    args: WorkCleanupArgs,
    db_path: Option<PathBuf>,
    generated_at: String,
) -> Result<WorkCleanupCommandOutput, WorkCommandError> {
    let (unresolved_paths, expected_materializations) = if args.apply {
        cleanup_unresolved_paths(db_path.clone(), &generated_at)?
    } else {
        (Vec::new(), Default::default())
    };
    require_unresolved_acknowledgements(&unresolved_paths, &args.acknowledged_unresolved)?;
    let mut output = cleanup_work_views(WorkCleanupOptions {
        db_path,
        apply: args.apply,
        generated_at,
        expected_materializations,
    })
    .map_err(WorkCommandError::from)?;
    output.unresolved_paths = unresolved_paths;
    output.status = unresolved_status(&output.unresolved_paths);
    Ok(output)
}

fn cleanup_unresolved_paths(
    db_path: Option<PathBuf>,
    generated_at: &str,
) -> Result<
    (
        Vec<WorkUnresolvedPath>,
        std::collections::BTreeMap<String, String>,
    ),
    WorkCommandError,
> {
    let store = open_store(db_path.as_deref())?;
    let workspace = store
        .current_workspace()
        .map_err(WorkViewError::from)?
        .ok_or(WorkViewError::MissingWorkspace)?;
    let candidates = store
        .work_views(&workspace.id, true, None)
        .map_err(WorkViewError::from)?
        .into_iter()
        .filter(|view| {
            matches!(
                view.lifecycle,
                bowline_core::work_views::WorkViewLifecycle::Accepted
                    | bowline_core::work_views::WorkViewLifecycle::Discarded
            ) && !matches!(
                view.retention.state,
                bowline_core::work_views::WorkViewRetentionState::DeleteEligible
            )
        })
        .collect::<Vec<_>>();
    drop(store);

    let mut unresolved = Vec::new();
    let mut expected_materializations = std::collections::BTreeMap::new();
    for view in candidates {
        if view.host_materializations.iter().all(|display| {
            expand_display_path(display)
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .is_some_and(|name| name.starts_with(".bowline-cleanup-"))
        }) {
            expected_materializations.extend(snapshot_materializations(&view)?);
            continue;
        }
        let reviewed = run_diff(
            WorkSelectorArgs {
                selector: view.id.as_str().to_string(),
                paths: Vec::new(),
                acknowledged_unresolved: Vec::new(),
            },
            db_path.clone(),
            generated_at.to_string(),
        )?;
        let root = expand_display_path(&view.visible_path);
        expected_materializations.extend(snapshot_materializations(&reviewed.work_view)?);
        unresolved.extend(
            reviewed
                .unresolved_paths
                .into_iter()
                .map(|issue| WorkUnresolvedPath {
                    path: root.join(issue.path).display().to_string(),
                    reason: issue.reason,
                }),
        );
    }
    unresolved.sort();
    unresolved.dedup();
    Ok((unresolved, expected_materializations))
}

fn snapshot_materializations(
    work_view: &WorkView,
) -> Result<std::collections::BTreeMap<String, String>, WorkCommandError> {
    work_view
        .host_materializations
        .iter()
        .map(|display| {
            materialization_snapshot(&expand_display_path(display))
                .map(|snapshot| (display.clone(), snapshot))
                .map_err(WorkCommandError::from)
        })
        .collect()
}
