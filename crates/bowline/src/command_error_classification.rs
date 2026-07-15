use bowline_core::commands::{CommandExitCode, CommandName};
use bowline_local::{agents::AgentError, work_views::WorkViewError};

use crate::errors::{print_runtime_error, print_user_action_error};

pub(super) fn print_work_error(
    command: CommandName,
    generated_at: String,
    error: &WorkViewError,
    json: bool,
) -> CommandExitCode {
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

pub(super) fn print_agent_error(
    command: CommandName,
    generated_at: String,
    error: &AgentError,
    json: bool,
) -> CommandExitCode {
    if agent_error_requires_user_action(error) {
        return print_user_action_error(
            command,
            generated_at,
            "agent_requires_action",
            &error.to_string(),
            "Inspect the lease and work-view selectors, correct the local state, and retry.",
            json,
        );
    }
    print_runtime_error(command, generated_at, &error.to_string(), json)
}

fn agent_error_requires_user_action(error: &AgentError) -> bool {
    match error {
        AgentError::MissingWorkspace
        | AgentError::MissingProject { .. }
        | AgentError::MissingLease { .. }
        | AgentError::MissingWorkView { .. }
        | AgentError::StaleBaseHeld { .. }
        | AgentError::InvalidLease { .. } => true,
        AgentError::WorkView(error) => work_error_requires_user_action(error),
        _ => false,
    }
}
