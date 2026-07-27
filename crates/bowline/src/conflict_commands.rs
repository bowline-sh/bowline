//! `bowline conflicts` and `bowline resolve`: the two verbs that close the
//! conflict loop status points at.

use bowline_core::commands::{
    ConflictAction, ConflictAsideSummary, ConflictsCommandOutput, ResolveCommandOutput,
};
use bowline_local::conflicts::{
    ConflictAside, ConflictError, ConflictResolution, ProjectScope, conflict_at, in_project_scope,
    list_conflicts, resolve_conflict, workspace_conflict_path,
};
use bowline_local::sync::manifest_engine::WorkspacePath;

use super::*;

mod diff;
mod render_conflicts;

use diff::{DiffOutcome, unified_diff};
use render_conflicts::{render_conflicts_human, render_conflicts_quiet, render_resolve_human};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ConflictsArgs {
    pub(super) selection: WorkspaceSelection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ResolveArgs {
    pub(super) root: String,
    pub(super) aside_path: String,
    pub(super) action: ConflictAction,
}

pub(super) fn print_conflicts(args: ConflictsArgs, json: bool, quiet: bool) -> ExitCode {
    let generated_at = generated_at();
    let root = service::expand_home_path(&args.selection.root);
    let scope = match project_scope(&root, &args.selection) {
        Ok(scope) => scope,
        Err(outside) => {
            return print_usage_error(
                CommandName::Conflicts,
                "project_outside_workspace",
                &format!(
                    "--project {} is not inside {}",
                    outside.as_str(),
                    abbreviate_requested_path(&args.selection.root)
                ),
                json,
            )
            .into();
        }
    };
    let asides = match list_conflicts(&root) {
        Ok(asides) => asides,
        Err(error) => return conflict_failure(CommandName::Conflicts, generated_at, &error, json),
    };
    let conflicts = asides
        .into_iter()
        .filter(|conflict| in_project_scope(conflict, scope.as_ref()))
        .map(|conflict| summarize(&conflict, &args.selection.root))
        .collect::<Vec<_>>();

    let output = ConflictsCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Conflicts,
        generated_at: generated_at.clone(),
        workspace_root: abbreviate_requested_path(&args.selection.root),
        next_actions: next_actions_for(&conflicts),
        conflicts,
    };

    if json {
        print_json(&output);
        return ExitCode::SUCCESS;
    }
    if quiet {
        return write_human_or_exit(
            CommandName::Conflicts,
            generated_at,
            &render_conflicts_quiet(&output),
        );
    }
    print!("{}", render_conflicts_human(&output));
    ExitCode::SUCCESS
}

pub(super) fn print_resolve(args: ResolveArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let root = service::expand_home_path(&args.root);
    // The path is a caller-supplied string, so it is validated before it reaches
    // any filesystem call — `--diff` included, which must not be a way to name a
    // path the mutating verbs would refuse.
    let aside = match workspace_conflict_path(&WorkspacePath::new(args.aside_path)) {
        Ok(aside) => aside,
        Err(error) => return conflict_failure(CommandName::Resolve, generated_at, &error, json),
    };

    let outcome = match args.action {
        ConflictAction::Diff => diff_only(&root, &aside),
        ConflictAction::KeepLocal => {
            apply(&root, &aside, ConflictResolution::KeepLocal).map(|conflict| (conflict, None))
        }
        ConflictAction::TakeRemote => {
            apply(&root, &aside, ConflictResolution::TakeRemote).map(|conflict| (conflict, None))
        }
    };
    let (conflict, outcome) = match outcome {
        Ok(outcome) => outcome,
        Err(error) => return conflict_failure(CommandName::Resolve, generated_at, &error, json),
    };
    if let Some(resolution) = resolution_of(args.action) {
        record_resolution(&conflict, resolution, &args.root, &generated_at);
    }
    let (diff, diff_unavailable) = match outcome {
        Some(DiffOutcome::Unified(diff)) => (Some(diff), None),
        Some(DiffOutcome::Unavailable(reason)) => (None, Some(reason)),
        None => (None, None),
    };

    let summary = summarize(&conflict, &args.root);
    let output = ResolveCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Resolve,
        generated_at,
        workspace_root: abbreviate_requested_path(&args.root),
        action: args.action,
        changed: args.action.changes_files(),
        diff,
        diff_unavailable,
        next_actions: resolve_next_actions(args.action, &summary),
        conflict: summary,
    };

    if json {
        print_json(&output);
    } else {
        print!("{}", render_resolve_human(&output));
    }
    ExitCode::SUCCESS
}

fn resolution_of(action: ConflictAction) -> Option<ConflictResolution> {
    match action {
        ConflictAction::KeepLocal => Some(ConflictResolution::KeepLocal),
        ConflictAction::TakeRemote => Some(ConflictResolution::TakeRemote),
        ConflictAction::Diff => None,
    }
}

/// Append the timeline entry for a resolution.
///
/// The workspace is already reconciled at this point and the aside's absence is
/// what every surface reads, so a metadata failure must not turn a successful
/// resolution into a failed command. It is still never silent: the reason is
/// reported on stderr rather than dropped.
fn record_resolution(
    conflict: &ConflictAside,
    resolution: ConflictResolution,
    root_label: &str,
    occurred_at: &str,
) {
    // Never create a metadata store as a side effect of reconciling a file: on a
    // device that has not been set up there is no timeline to append to, and
    // conjuring one would invent workspace state the user never accepted.
    let Some(db_path) = runtime::selected_metadata_database_path().filter(|path| path.exists())
    else {
        return;
    };
    let store = match MetadataStore::open(&db_path) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("bowline resolve: conflict timeline not updated: {error}");
            return;
        }
    };
    // The root the command already resolved is the workspace identity; a
    // resolution outside any accepted root has no timeline to append to.
    let Ok(Some(workspace)) = store.workspace_by_accepted_root(root_label) else {
        return;
    };
    let subject = bowline_local::events::ConflictEventSubject {
        workspace_id: &workspace.id,
        project_id: None,
        origin_path: conflict.origin.as_str(),
        aside_path: conflict.aside.as_str(),
        occurred_at,
    };
    if let Err(error) = store.append_conflict_resolved(&subject, resolution) {
        eprintln!("bowline resolve: conflict timeline not updated: {error}");
    }
}

fn apply(
    root: &Path,
    aside: &WorkspacePath,
    resolution: ConflictResolution,
) -> Result<ConflictAside, ConflictError> {
    resolve_conflict(root, aside, resolution).map(|resolved| resolved.conflict)
}

/// `--diff` reads both sides and changes nothing, so it does not go through
/// `resolve_conflict`. It reaches the conflict through the same locator the
/// mutating verbs use rather than through the workspace scan, so the preview
/// accepts and refuses exactly the paths they do: a scan prunes subtrees, stops
/// at a descriptor depth and at an entry budget, so a preview built on it
/// refused precisely where the irreversible action succeeded.
fn diff_only(
    root: &Path,
    aside: &WorkspacePath,
) -> Result<(ConflictAside, Option<DiffOutcome>), ConflictError> {
    let conflict = conflict_at(root, aside)?;
    let diff = unified_diff(root, &conflict)?;
    Ok((conflict, Some(diff)))
}

fn summarize(conflict: &ConflictAside, root: &str) -> ConflictAsideSummary {
    ConflictAsideSummary {
        origin_path: conflict.origin.as_str().to_string(),
        aside_path: conflict.aside.as_str().to_string(),
        origin_missing: conflict.origin_missing,
        resolve_command: resolve_command(conflict, root, "--take-remote"),
    }
}

/// One suggested `bowline resolve` line, assembled from its parts.
///
/// The verb is a parameter rather than something a caller substitutes into a
/// finished command: the rendered line embeds the shell-quoted aside path, and
/// `notes--take-remote.md.bowline-conflict.abcd1234` is a legal workspace path,
/// so rewriting the string would edit the path instead of the flag and suggest a
/// command naming an aside that does not exist.
fn resolve_command(conflict: &ConflictAside, root: &str, verb: &str) -> String {
    format!(
        "bowline resolve {} --root {} {verb}",
        bowline_core::shell::quote_word(conflict.aside.as_str()),
        bowline_core::shell::quote_word(&abbreviate_requested_path(root)),
    )
}

/// The same line for a summary that already rendered one, without re-deriving
/// the root. Splitting on the trailing verb is safe where the string-rewrite was
/// not: the verb is the final word, so only the suffix is replaced.
fn resolve_command_variant(rendered: &str, verb: &str) -> String {
    match rendered.rsplit_once(' ') {
        Some((prefix, _)) => format!("{prefix} {verb}"),
        None => rendered.to_string(),
    }
}

fn next_actions_for(conflicts: &[ConflictAsideSummary]) -> Vec<RepairCommand> {
    let Some(first) = conflicts.first() else {
        return Vec::new();
    };
    vec![
        RepairCommand::inspect(
            format!("Compare both versions of {}", first.origin_path),
            Some(resolve_command_variant(&first.resolve_command, "--diff")),
        ),
        RepairCommand::mutating(
            format!("Adopt the incoming version of {}", first.origin_path),
            Some(first.resolve_command.clone()),
        ),
    ]
}

fn resolve_next_actions(
    action: ConflictAction,
    conflict: &ConflictAsideSummary,
) -> Vec<RepairCommand> {
    match action {
        ConflictAction::Diff => vec![
            RepairCommand::mutating(
                "Keep the file as it is".to_string(),
                Some(resolve_command_variant(
                    &conflict.resolve_command,
                    "--keep-local",
                )),
            ),
            RepairCommand::mutating(
                "Adopt the incoming version".to_string(),
                Some(conflict.resolve_command.clone()),
            ),
        ],
        ConflictAction::KeepLocal | ConflictAction::TakeRemote => {
            vec![RepairCommand::inspect(
                "Check for remaining conflicts".to_string(),
                Some("bowline conflicts".to_string()),
            )]
        }
    }
}

/// A `--project` that names somewhere the workspace root does not contain, so
/// no conflict in this workspace could ever be under it.
#[derive(Debug, PartialEq, Eq)]
struct ProjectOutsideWorkspace(String);

impl ProjectOutsideWorkspace {
    fn as_str(&self) -> &str {
        &self.0
    }
}

/// The narrowing `--project` asks for, or `None` for the whole workspace — no
/// `--project`, or one naming the root itself.
///
/// `--project` is resolved by the convention every other command uses, so `.`
/// and `./x` mean the same directory here as they do in `bowline status`.
fn project_scope(
    root: &Path,
    selection: &WorkspaceSelection,
) -> Result<Option<ProjectScope>, ProjectOutsideWorkspace> {
    let Some(project) = selection
        .project
        .as_deref()
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    let outside = || ProjectOutsideWorkspace(project.to_string());
    let absolute = selected_project_path(&selection.root, project).ok_or_else(outside)?;
    let relative =
        workspace_relative(root, &service::expand_home_path(&absolute)).ok_or_else(outside)?;
    Ok(ProjectScope::new(&relative))
}

/// A path inside the workspace, as a workspace-relative path.
fn workspace_relative(root: &Path, absolute: &Path) -> Option<String> {
    if let Ok(relative) = absolute.strip_prefix(root) {
        return Some(relative.to_string_lossy().into_owned());
    }
    // The spellings differ only for paths that exist: `--project .` resolves
    // through the working directory's real path, and a root reached through a
    // symlink (`/tmp` on macOS) does not literally prefix its own children. A
    // path that cannot be canonicalized has nothing under it either way.
    let comparable_root = std::fs::canonicalize(root).ok()?;
    let comparable = std::fs::canonicalize(absolute).ok()?;
    Some(
        comparable
            .strip_prefix(&comparable_root)
            .ok()?
            .to_string_lossy()
            .into_owned(),
    )
}

/// A conflict failure is a user-actionable fact about one path, so it carries the
/// error's stable tag as the code rather than a formatted sentence.
fn conflict_failure(
    command: CommandName,
    generated_at: String,
    error: &ConflictError,
    json: bool,
) -> ExitCode {
    // Owned by the error itself: an agent loops on `retry`, so which failures
    // are permanent is a property of the failure, not of the surface reporting
    // it.
    let recoverability = error.recoverability();
    let remediation = match error {
        ConflictError::Root { .. } => {
            "Restore the workspace folder, then run `bowline conflicts` again."
        }
        ConflictError::DirectoryAside { .. } => {
            "Reconcile the files inside the folder one at a time; `bowline conflicts` lists them."
        }
        ConflictError::ParentNotADirectory { .. } => {
            "Replace the symlink or file on the way to it with a real folder, then run `bowline conflicts` again."
        }
        _ => "Run `bowline conflicts` for the exact aside paths still waiting.",
    };
    let output = CommandErrorOutput {
        contract_version: CONTRACT_VERSION,
        command,
        generated_at,
        status: CommandErrorStatus::Failed,
        error: CommandError {
            code: error.tag().to_string(),
            message: error.to_string(),
            recoverability,
            remediation: Some(remediation.to_string()),
            details: None,
            retry_after_seconds: None,
            correlation_id: None,
        },
        next_actions: vec![RepairCommand::inspect(
            "List unreconciled conflicts".to_string(),
            Some("bowline conflicts".to_string()),
        )],
    };
    print_command_error_output(&output, json).into()
}

#[cfg(test)]
mod tests;
