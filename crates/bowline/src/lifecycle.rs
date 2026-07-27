use std::path::PathBuf;

use bowline_core::commands::{
    CommandName, NamespaceLifecycleCommandOutput, NamespaceLifecyclePreview, StatusCommandOutput,
};
use bowline_core::status::{ConvergenceReadinessState, SyncQueueStatus};
use bowline_local::lifecycle::{
    ConvergenceUnproven, NamespaceLifecycleError, NamespaceLifecycleOptions, SyncConvergence,
    archive, forget_local, purge,
};

use crate::surface::style::{self, Presentation, Role};
use crate::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgetLocalArgs {
    pub project_path: String,
    pub yes: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveArgs {
    pub project_path: String,
    pub restore: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgeArgs {
    pub project_path: String,
    pub cancel: bool,
    pub grace_days: Option<u32>,
}

pub fn run_forget_local(
    args: ForgetLocalArgs,
    db_path: Option<PathBuf>,
    generated_at: String,
) -> Result<NamespaceLifecycleCommandOutput, NamespaceLifecycleError> {
    let convergence = sync_convergence();
    forget_local(NamespaceLifecycleOptions {
        db_path,
        project_path: args.project_path,
        generated_at,
        yes: args.yes,
        dry_run: false,
        restore: false,
        cancel: false,
        grace_days: None,
        convergence,
    })
}

pub fn run_archive(
    args: ArchiveArgs,
    db_path: Option<PathBuf>,
    generated_at: String,
) -> Result<NamespaceLifecycleCommandOutput, NamespaceLifecycleError> {
    let convergence = sync_convergence();
    archive(NamespaceLifecycleOptions {
        db_path,
        project_path: args.project_path,
        generated_at,
        yes: true,
        dry_run: false,
        restore: args.restore,
        cancel: false,
        grace_days: None,
        convergence,
    })
}

pub fn run_purge(
    args: PurgeArgs,
    db_path: Option<PathBuf>,
    generated_at: String,
) -> Result<NamespaceLifecycleCommandOutput, NamespaceLifecycleError> {
    let convergence = sync_convergence();
    purge(NamespaceLifecycleOptions {
        db_path,
        project_path: args.project_path,
        generated_at,
        yes: true,
        dry_run: false,
        restore: false,
        cancel: args.cancel,
        grace_days: args.grace_days,
        convergence,
    })
}

pub fn render_lifecycle_human(output: &NamespaceLifecycleCommandOutput) -> String {
    let pres = Presentation::detect(false);
    let mut lines = vec![
        format!(
            "{}  {}",
            style::section("Project", &pres),
            style::paint(&output.project_path, Role::Strong, &pres)
        ),
        format!(
            "{}  {}",
            style::section("Action", &pres),
            style::paint(&style::kebab(&output.action), Role::Label, &pres)
        ),
        preview_line(&output.preview),
    ];
    if let Some(purge_after) = &output.preview.purge_after {
        lines.push(format!("Purge after  {purge_after}"));
    }
    lines.push(String::new());
    lines.join("\n")
}

fn preview_line(preview: &NamespaceLifecyclePreview) -> String {
    format!(
        "Preview  {} paths, {} bytes, {} packs",
        preview.paths.len(),
        preview.byte_total,
        preview.pack_count
    )
}

pub fn print_forget_local(args: ForgetLocalArgs, json: bool) -> ExitCode {
    print_result(
        CommandName::ForgetLocal,
        run_forget_local(args, None, generated_at()),
        json,
    )
}

pub fn print_archive(args: ArchiveArgs, json: bool) -> ExitCode {
    print_result(
        CommandName::Archive,
        run_archive(args, None, generated_at()),
        json,
    )
}

pub fn print_purge(args: PurgeArgs, json: bool) -> ExitCode {
    print_result(
        CommandName::Purge,
        run_purge(args, None, generated_at()),
        json,
    )
}

fn print_result(
    command: CommandName,
    result: Result<NamespaceLifecycleCommandOutput, NamespaceLifecycleError>,
    json: bool,
) -> ExitCode {
    match result {
        Ok(output) => {
            if json {
                print_json(&output);
            } else {
                print!("{}", render_lifecycle_human(&output));
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            let output = lifecycle_error_output(command, &error);
            print_command_error_output(&output, json).into()
        }
    }
}

/// Ask the daemon whether this workspace's local work is already pushed.
/// Lifecycle mutations delete real bytes, so the daemon is the only authority
/// here: an unreachable or unready daemon yields `Unproven`, never an
/// optimistic `Converged`.
fn sync_convergence() -> SyncConvergence {
    let Ok(socket) = crate::wire::control_socket_path() else {
        return SyncConvergence::Unproven(ConvergenceUnproven::ProbeFailed);
    };
    match crate::wire::daemon_status_snapshot_scoped(&socket, None) {
        Ok(status) => convergence_from_status(&status),
        Err(error) if error.daemon_is_absent() => {
            SyncConvergence::Unproven(ConvergenceUnproven::DaemonUnreachable)
        }
        Err(_) => SyncConvergence::Unproven(ConvergenceUnproven::ProbeFailed),
    }
}

fn convergence_from_status(status: &StatusCommandOutput) -> SyncConvergence {
    let Some(convergence) = status.convergence.as_ref() else {
        return SyncConvergence::Unproven(ConvergenceUnproven::DaemonNotReady);
    };
    match convergence.state {
        ConvergenceReadinessState::Ready => {}
        ConvergenceReadinessState::Converging
        | ConvergenceReadinessState::Recovering
        | ConvergenceReadinessState::Limited => {
            return SyncConvergence::Unproven(ConvergenceUnproven::DaemonNotReady);
        }
    }
    if status
        .sync_queue
        .as_ref()
        .is_some_and(SyncQueueStatus::has_pending_work)
    {
        return SyncConvergence::PendingWork {
            paths: outstanding_paths(status),
        };
    }
    SyncConvergence::Converged
}

/// The paths the daemon is still carrying, sorted and deduped so the refusal
/// message names the same work in the same order on every device.
fn outstanding_paths(status: &StatusCommandOutput) -> Vec<String> {
    let mut paths = status
        .items
        .iter()
        .filter_map(|item| item.path.clone())
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    paths
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LifecycleErrorKind {
    ConfirmationRequired,
    UnsyncedWork,
    ConvergenceUnproven,
    InvalidState,
    ProjectMissing,
    AuditAppendFailed,
    RuntimeFailure,
}

impl LifecycleErrorKind {
    fn from_error(error: &NamespaceLifecycleError) -> Self {
        match error {
            NamespaceLifecycleError::ConfirmationRequired => Self::ConfirmationRequired,
            NamespaceLifecycleError::UnsyncedWork { .. } => Self::UnsyncedWork,
            NamespaceLifecycleError::ConvergenceUnproven(_) => Self::ConvergenceUnproven,
            NamespaceLifecycleError::InvalidState(_) => Self::InvalidState,
            NamespaceLifecycleError::ProjectMissing(_) => Self::ProjectMissing,
            NamespaceLifecycleError::EventAppend(_) => Self::AuditAppendFailed,
            NamespaceLifecycleError::Metadata(_) | NamespaceLifecycleError::Io(_) => {
                Self::RuntimeFailure
            }
        }
    }

    fn code(self) -> &'static str {
        match self {
            Self::ConfirmationRequired => "confirmation_required",
            Self::UnsyncedWork => "unsynced_local_work",
            Self::ConvergenceUnproven => "sync_convergence_unproven",
            Self::InvalidState => "invalid_lifecycle_state",
            Self::ProjectMissing => "project_not_found",
            Self::AuditAppendFailed => "audit_append_failed",
            Self::RuntimeFailure => "lifecycle_failed",
        }
    }

    fn recoverability(self) -> CommandRecoverability {
        match self {
            Self::ConfirmationRequired
            | Self::UnsyncedWork
            | Self::InvalidState
            | Self::ProjectMissing => CommandRecoverability::UserAction,
            // The daemon may simply not be up yet; retrying is the fix.
            Self::ConvergenceUnproven | Self::AuditAppendFailed | Self::RuntimeFailure => {
                CommandRecoverability::Retry
            }
        }
    }

    fn remediation(self) -> &'static str {
        match self {
            Self::ConfirmationRequired => "Review the preview, then retry with --yes.",
            Self::UnsyncedWork => "Resolve or sync the listed local work, then retry.",
            Self::ConvergenceUnproven => {
                "Start the daemon with `bowline daemon start`, then retry once sync has converged."
            }
            Self::InvalidState => "Move the project into the required lifecycle state, then retry.",
            Self::ProjectMissing => "Inspect tracked projects and retry with a valid project path.",
            Self::AuditAppendFailed => "Retry after local metadata and event storage recover.",
            Self::RuntimeFailure => "Retry after local metadata and filesystem access recover.",
        }
    }
}

fn lifecycle_error_output(
    command: CommandName,
    error: &NamespaceLifecycleError,
) -> CommandErrorOutput {
    let kind = LifecycleErrorKind::from_error(error);
    let mut output = bowline_local::status::command_error_output(
        command,
        generated_at(),
        kind.code(),
        error.to_string(),
        kind.recoverability(),
    );
    output.error.remediation = Some(kind.remediation().to_string());
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use bowline_local::metadata::MetadataError;

    #[test]
    fn lifecycle_error_variants_map_to_contract_exit_codes() {
        let user_action_errors = [
            NamespaceLifecycleError::ConfirmationRequired,
            NamespaceLifecycleError::UnsyncedWork {
                paths: vec!["apps/web/src/auth.ts".to_string()],
            },
            NamespaceLifecycleError::InvalidState("project is active".to_string()),
            NamespaceLifecycleError::ProjectMissing("apps/missing".to_string()),
        ];
        for error in &user_action_errors {
            assert_lifecycle_error(error, CommandRecoverability::UserAction, 4);
        }

        let retryable_errors = [
            NamespaceLifecycleError::Metadata(MetadataError::InvalidStorageMetadata(
                "corrupt metadata".to_string(),
            )),
            NamespaceLifecycleError::Io(std::io::Error::other("disk unavailable")),
            NamespaceLifecycleError::EventAppend("event store unavailable".to_string()),
        ];
        for error in &retryable_errors {
            assert_lifecycle_error(error, CommandRecoverability::Retry, 3);
        }
    }

    fn assert_lifecycle_error(
        error: &NamespaceLifecycleError,
        recoverability: CommandRecoverability,
        expected_exit_code: u8,
    ) {
        let output = lifecycle_error_output(CommandName::Archive, error);
        assert_eq!(output.status, CommandErrorStatus::Failed);
        assert_eq!(output.error.recoverability, recoverability);
        assert_eq!(
            CommandExitCode::for_error(output.status, output.error.recoverability).code(),
            expected_exit_code
        );
    }
}
