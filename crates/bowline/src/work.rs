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
use bowline_core::work_views::{WorkCommandAction, WorkDiffChangeKind, WorkView};
use bowline_local::metadata::MetadataStore;
use bowline_local::sync::manifest_engine::aux_index::{
    AuxIndex, WorkViewId as AuxWorkViewId, WorkViewLifecycle as AuxLifecycle, WorkViewRecord,
};
use bowline_local::sync::manifest_engine::manifest::ManifestKey;
use bowline_local::sync::manifest_engine::work_view_cli::{
    overlay_engine_truth, read_aux_index_file, wire_diff_entries, write_aux_index_file,
};
use bowline_local::work_views::{
    WorkAcceptTransition, WorkCleanupOptions, WorkListOptions, WorkViewError, apply_accept_success,
    cleanup_work_views, discard_work_view, expand_display_path, list_work_views, open_store,
    resolve_work_view, restore_work_view,
};
use serde::{Deserialize, Serialize};

use crate::surface::style::{self, Presentation, Role};

mod create;

pub use create::run_work_create;

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
    aside_refused_paths: Vec<String>,
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
) -> Result<WorkLifecycleRunOutput, WorkCommandError> {
    match lifecycle {
        WorkLifecycle::Accept => run_accept(args, db_path, generated_at),
        WorkLifecycle::Discard => {
            discard_work_view(bowline_local::work_views::WorkSelectorOptions {
                db_path,
                selector: args.selector,
                paths: args.paths,
                generated_at,
            })
            .map(WorkLifecycleRunOutput::without_conflicts)
            .map_err(Into::into)
        }
        WorkLifecycle::Restore => {
            restore_work_view(bowline_local::work_views::WorkSelectorOptions {
                db_path,
                selector: args.selector,
                paths: args.paths,
                generated_at,
            })
            .map(WorkLifecycleRunOutput::without_conflicts)
            .map_err(Into::into)
        }
    }
}

fn run_accept(
    args: WorkSelectorArgs,
    db_path: Option<PathBuf>,
    generated_at: String,
) -> Result<WorkLifecycleRunOutput, WorkCommandError> {
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
            captured_overlay: accepted.overlay_manifest_key,
            accepted_base: Some(accepted.base_manifest_key),
        },
    )?;
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

pub fn render_lifecycle_human(run: &WorkLifecycleRunOutput) -> String {
    let output = &run.output;
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
    // A refused aside leaves no file to discover either: the view's version was
    // dropped because nowhere beside these paths may hold a second copy.
    if !output.aside_refused_paths.is_empty() {
        text.push_str(&format!(
            "{}  no second copy can sit beside these, so the view's version was not kept\n",
            style::section("Kept (view version dropped)", &pres),
        ));
        for path in &output.aside_refused_paths {
            text.push_str(&format!("  {}\n", style::paint(path, Role::Label, &pres)));
        }
    }
    // Conflict asides are real files sitting in the project. Naming them here is
    // the only record of which accept wrote them, and the resolve line is what
    // turns that record into something the reader can act on without a lookup.
    if !run.conflict_asides.is_empty() {
        text.push_str(&format!(
            "{}  the merge could not reconcile these, so both versions were kept side by side\n",
            style::section("Conflicts", &pres),
        ));
        for path in &run.conflict_asides {
            text.push_str(&format!(
                "  {}\n",
                style::paint(path, Role::Attention, &pres)
            ));
            text.push_str(&format!(
                "{}\n",
                style::next_action(
                    &format!(
                        "bowline resolve {} --diff",
                        bowline_core::shell::quote_word(path)
                    ),
                    "compare both versions",
                    &pres,
                )
            ));
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

#[cfg(test)]
mod tests {
    use super::*;
    use bowline_core::ids::{ProjectId, SnapshotId, WorkViewId, WorkspaceId};
    use bowline_core::work_views::{
        WorkViewLifecycle as WireLifecycle, WorkViewRetention, WorkViewRetentionState,
        WorkViewSyncState, WorkViewVisibility,
    };

    fn accepted_run(conflict_asides: Vec<String>) -> WorkLifecycleRunOutput {
        WorkLifecycleRunOutput {
            output: WorkLifecycleCommandOutput {
                contract_version: CONTRACT_VERSION,
                command: CommandName::Accept,
                generated_at: "2026-07-25T12:00:00Z".to_string(),
                action: WorkCommandAction::Accepted,
                paths: vec!["src/main.rs".to_string()],
                discarded_deletions: Vec::new(),
                aside_refused_paths: Vec::new(),
                partial: false,
                work_view: WorkView {
                    id: WorkViewId::new("wv_conflicts"),
                    workspace_id: WorkspaceId::new("ws_conflicts"),
                    project_id: ProjectId::new("proj_conflicts"),
                    project_path: "~/Code/app".to_string(),
                    name: "fix-login".to_string(),
                    visible_path: "~/Code/app.work/fix-login".to_string(),
                    base_snapshot_id: SnapshotId::new("snap_base"),
                    overlay_head: "overlay_head".to_string(),
                    overlay_version: 1,
                    env_profile: "default".to_string(),
                    lifecycle: WireLifecycle::Accepted,
                    visibility: WorkViewVisibility::DefaultVisible,
                    sync_state: WorkViewSyncState::Synced,
                    retention: WorkViewRetention {
                        state: WorkViewRetentionState::Retained,
                        retain_until: None,
                        restorable: true,
                    },
                    owner_device_id: None,
                    followed_by: Vec::new(),
                    host_materializations: Vec::new(),
                    attention: Vec::new(),
                    created_at: "2026-07-25T11:00:00Z".to_string(),
                    updated_at: "2026-07-25T12:00:00Z".to_string(),
                },
                status: WorkspaceStatus::healthy(),
                next_actions: Vec::new(),
            },
            conflict_asides,
        }
    }

    /// A silent accept leaves unexplained files in the project; the paths the
    /// merge could not reconcile have to reach both output surfaces.
    #[test]
    fn accept_reports_conflict_asides_in_json() {
        let run = accepted_run(vec!["src/main.rs.bowline-conflict".to_string()]);

        let json = serde_json::to_value(run.json_payload()).expect("payload serializes");

        assert_eq!(
            json["conflictAsides"],
            serde_json::json!(["src/main.rs.bowline-conflict"])
        );
    }

    #[test]
    fn accept_without_conflicts_omits_the_field() {
        let run = accepted_run(Vec::new());

        let json = serde_json::to_value(run.json_payload()).expect("payload serializes");

        assert!(json.get("conflictAsides").is_none());
    }

    #[test]
    fn accept_human_output_names_the_conflict_files() {
        let run = accepted_run(vec!["src/main.rs.bowline-conflict".to_string()]);

        let human = render_lifecycle_human(&run);

        assert!(human.contains("src/main.rs.bowline-conflict"));
        assert!(human.contains("Conflicts"));
    }
}
