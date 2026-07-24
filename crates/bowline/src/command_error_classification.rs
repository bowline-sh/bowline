use bowline_core::commands::{CommandExitCode, CommandName};
use bowline_local::work_views::WorkViewError;

use crate::errors::{print_runtime_error, print_user_action_error};
use crate::work::WorkCommandError;

pub(super) fn print_work_error(
    command: CommandName,
    generated_at: String,
    error: &WorkCommandError,
    json: bool,
) -> CommandExitCode {
    // A daemon RPC failure is a retryable runtime error (start the daemon and
    // retry); workspace/selector-state errors keep the frozen classification.
    let WorkCommandError::View(error) = error else {
        return print_runtime_error(command, generated_at, &error.to_string(), json);
    };
    if work_error_requires_user_action(error) {
        return print_user_action_error(
            command,
            generated_at,
            "work_requires_action",
            &error.to_string(),
            "Inspect `bowline work list --json`, correct the selector or workspace state, and retry.",
            json,
        );
    }
    print_runtime_error(command, generated_at, &error.to_string(), json)
}

fn work_error_requires_user_action(error: &WorkViewError) -> bool {
    use bowline_local::sync::manifest_engine::work_view_cli::WorkViewCliError;
    if let WorkViewError::Index(index_error) = error {
        return matches!(
            index_error,
            WorkViewCliError::InvalidPathSelector { .. }
                | WorkViewCliError::EmptyPathSelection { .. }
                | WorkViewCliError::UnknownView { .. }
                | WorkViewCliError::Unrestorable { .. }
        );
    }
    matches!(
        error,
        WorkViewError::MissingMetadataDb
            | WorkViewError::MissingWorkspace
            | WorkViewError::MissingWorkspaceRoot
            | WorkViewError::MissingProject { .. }
            | WorkViewError::MissingBaseSnapshot { .. }
            | WorkViewError::UnknownBaseSnapshot { .. }
            | WorkViewError::DirtyProject { .. }
            | WorkViewError::InvalidName { .. }
            | WorkViewError::NameCollision { .. }
            | WorkViewError::AmbiguousSelector { .. }
            | WorkViewError::MissingWorkView { .. }
            | WorkViewError::InactiveWorkView { .. }
            | WorkViewError::UnrestorableWorkView { .. }
            | WorkViewError::InvalidPathSelector { .. }
            | WorkViewError::EmptyPathSelection { .. }
    )
}
