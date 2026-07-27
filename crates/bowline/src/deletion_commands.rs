//! `bowline deletions`: read the removal batch sync is refusing to publish, and
//! authorise exactly one push of it.
//!
//! The engine's deletion breaker refuses a removal batch no plausible edit
//! produces, then publishes nothing until a human agrees. Without this command
//! that state has no exit: status can say sync needs attention, but nothing can
//! say which files would go, and nothing can say yes.
//!
//! Shaped like `bowline resolve`: one command, a read-only default and an
//! explicit mutating flag, so the preview and the decision cannot drift apart
//! into two different views of the same batch.

use bowline_core::commands::{
    BlockedDeletionsReport, DeletionsCommandOutput, DeletionsConfirmation,
    DeletionsConfirmationReport, DeletionsState,
};

use super::*;

mod render_deletions;

pub(super) use render_deletions::render_deletions_human;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DeletionsArgs {
    /// Authorise the refused batch. Without it the command only reports.
    pub(super) confirm: bool,
}

/// How many refused paths the human rendering prints before summarising the
/// rest. The JSON contract carries the daemon's whole bounded sample; a terminal
/// gets a page.
const HUMAN_PATH_LIMIT: usize = 20;

pub(super) fn print_deletions(args: DeletionsArgs, json: bool, socket: &Path) -> ExitCode {
    let generated_at = generated_at();
    let outcome = if args.confirm {
        crate::wire::confirm_deletions(socket).map(confirmed_output)
    } else {
        crate::wire::blocked_deletions(socket).map(preview_output)
    };
    let output = match outcome {
        Ok(output) => output,
        Err(error) => return daemon_failure(generated_at, &error, json),
    };
    let output = DeletionsCommandOutput {
        generated_at,
        ..output
    };
    if json {
        print_json(&output);
    } else {
        print!("{}", render_deletions_human(&output, HUMAN_PATH_LIMIT));
    }
    ExitCode::SUCCESS
}

fn preview_output(report: BlockedDeletionsReport) -> DeletionsCommandOutput {
    let next_actions = match report.state {
        DeletionsState::Clear => Vec::new(),
        DeletionsState::Blocked => vec![RepairCommand::mutating(
            "Confirm the deletion and let sync continue".to_string(),
            Some("bowline deletions --confirm".to_string()),
        )],
    };
    DeletionsCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Deletions,
        generated_at: String::new(),
        state: report.state,
        changed: false,
        blocked: report.blocked,
        confirmation: None,
        next_actions,
    }
}

fn confirmed_output(report: DeletionsConfirmationReport) -> DeletionsCommandOutput {
    let (state, next_actions) = match report.state {
        // The authorised push runs on the engine's own schedule, so the honest
        // next step is to watch it land, not to claim it already has.
        DeletionsConfirmation::Authorized => (
            DeletionsState::Blocked,
            vec![RepairCommand::inspect(
                "Watch sync catch up".to_string(),
                Some("bowline status --watch".to_string()),
            )],
        ),
        DeletionsConfirmation::NotBlocked => (DeletionsState::Clear, Vec::new()),
    };
    DeletionsCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Deletions,
        generated_at: String::new(),
        state,
        changed: report.state == DeletionsConfirmation::Authorized,
        blocked: report.blocked,
        confirmation: Some(report.state),
        next_actions,
    }
}

/// A daemon that cannot be reached cannot be asked about deletions, and a
/// blocked workspace is exactly the state where a user must not be told
/// "nothing is wrong". The reachability classification owns the remedy.
fn daemon_failure(generated_at: String, error: &DaemonRpcError, json: bool) -> ExitCode {
    let output = CommandErrorOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Deletions,
        generated_at,
        status: CommandErrorStatus::Failed,
        error: CommandError {
            code: "daemon_unreachable".to_string(),
            message: error.to_string(),
            recoverability: CommandRecoverability::Retry,
            remediation: Some(format!("{}.", error.reachability().remediation())),
            details: None,
            retry_after_seconds: None,
            correlation_id: None,
        },
        next_actions: vec![RepairCommand::inspect(
            "Check the local daemon".to_string(),
            Some("bowline daemon status".to_string()),
        )],
    };
    print_command_error_output(&output, json).into()
}

#[cfg(test)]
mod tests;
