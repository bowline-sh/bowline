use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Cli {
    pub(super) json: bool,
    pub(super) socket: PathBuf,
    pub(super) dry_run: bool,
    pub(super) idempotency_key: Option<String>,
    pub(super) command: Command,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Command {
    Help(Option<Vec<String>>),
    Version,
    Contract,
    Login(login::LoginArgs),
    Approve(ApproveArgs),
    Revoke(RevokeArgs),
    Init(InitArgs),
    Prewarm(PrewarmArgs),
    Setup(SetupArgs),
    Status(StatusArgs),
    Actions(ActionsArgs),
    Tui(TuiArgs),
    Search(SearchArgs),
    Symbols(SymbolsArgs),
    Explain(ExplainArgs),
    Devices(devices::DevicesArgs),
    Recovery(recovery::RecoveryArgs),
    Resolve(resolve::ResolveArgs),
    Events(EventsArgs),
    Workon(work::WorkonArgs),
    Work(work::WorkListArgs),
    WorkDiff(work::WorkSelectorArgs),
    Review(work::WorkSelectorArgs),
    WorkAccept(work::WorkSelectorArgs),
    WorkDiscard(work::WorkSelectorArgs),
    WorkRestore(work::WorkSelectorArgs),
    WorkCleanup(work::WorkCleanupArgs),
    AgentLeaseCreate(agent::AgentLeaseCreateArgs),
    AgentContext(agent::AgentLeaseSelectorArgs),
    AgentPrompt(agent::AgentLeaseSelectorArgs),
    AgentPublish(agent::AgentLeaseSelectorArgs),
    AgentComplete(agent::AgentLeaseSelectorArgs),
    AgentBudget(agent::AgentBudgetArgs),
    BootstrapSsh(bootstrap::BootstrapSshArgs),
    DevCloudSpike(CloudSpikeArgs),
    Daemon(DaemonCommand),
    DiagnosticsCollect,
    CommandUsageError(CommandUsageError),
    UsageError {
        command: CommandName,
        message: String,
    },
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CommandUsageError {
    pub(super) command: CommandName,
    pub(super) code: &'static str,
    pub(super) message: String,
    pub(super) next_actions: Vec<SafeAction>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InitArgs {
    pub(super) root: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ApproveArgs {
    pub(super) request_id: Option<String>,
    pub(super) yes: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RevokeArgs {
    pub(super) device_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PrewarmArgs {
    pub(super) project_path: String,
    pub(super) approve_setup: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SetupArgs {
    pub(super) project_path: Option<String>,
    pub(super) yes: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StatusArgs {
    pub(super) path: Option<String>,
    pub(super) watch: bool,
    pub(super) workspace: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ActionsArgs {
    pub(super) path: Option<String>,
    pub(super) workspace: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TuiArgs {
    pub(super) path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SearchArgs {
    pub(super) query: String,
    pub(super) path: Option<String>,
    pub(super) limit: usize,
    pub(super) cursor: Option<usize>,
    pub(super) path_prefix: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SymbolsArgs {
    pub(super) query: String,
    pub(super) path: Option<String>,
    pub(super) limit: usize,
    pub(super) cursor: Option<usize>,
    pub(super) path_prefix: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ExplainArgs {
    pub(super) path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct EventsArgs {
    pub(super) path: Option<String>,
    pub(super) workspace: bool,
    pub(super) limit: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CloudSpikeArgs {
    pub(super) provider: CloudSpikeProvider,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CloudSpikeProvider {
    Fake,
    Hosted,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CloudSpikeFakeOutput<'a> {
    pub(super) ok: bool,
    pub(super) command: &'static str,
    pub(super) provider: &'static str,
    pub(super) workspace_id: &'a str,
    pub(super) starting_version: u64,
    pub(super) advanced_version: u64,
    pub(super) pack_object_count: usize,
    pub(super) source_file_count: usize,
    pub(super) hydrated_cold_file_byte_len: usize,
    pub(super) stale_ref_detected: bool,
    pub(super) device_approval_harness_only: bool,
    pub(super) event_count: usize,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CloudSpikeSkipOutput {
    pub(super) ok: bool,
    pub(super) command: &'static str,
    pub(super) provider: &'static str,
    pub(super) skipped: bool,
    pub(super) missing_env: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DaemonCommand {
    Start,
    Stop,
    Status,
    Install,
    Restart,
    Uninstall,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Handshake {
    pub(super) daemon_version: String,
    pub(super) sync_json: Option<String>,
}

pub(super) fn parse_args<I, S>(args: I) -> Cli
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut json = false;
    let mut socket = PathBuf::from(DEFAULT_SOCKET);
    let mut dry_run = false;
    let mut idempotency_key = None;
    let mut help_requested = false;
    let mut version_requested = false;
    let mut positionals = Vec::new();
    let mut iter = args.into_iter().map(Into::into);

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--dry-run" => dry_run = true,
            "--version" => version_requested = true,
            "--idempotency-key" => match iter.next() {
                Some(key) => idempotency_key = Some(key),
                None => {
                    return Cli {
                        json,
                        socket,
                        dry_run,
                        idempotency_key,
                        command: usage_error(
                            CommandName::Unknown,
                            "missing value for --idempotency-key",
                        ),
                    };
                }
            },
            "--socket" => match iter.next() {
                Some(path) => socket = PathBuf::from(path),
                None => {
                    return Cli {
                        json,
                        socket,
                        dry_run,
                        idempotency_key,
                        command: usage_error(CommandName::Unknown, "missing value for --socket"),
                    };
                }
            },
            "-h" | "--help" => help_requested = true,
            _ => positionals.push(arg),
        }
    }

    let command = if version_requested && positionals.is_empty() {
        Command::Version
    } else if help_requested {
        let topic = (!positionals.is_empty()).then_some(positionals);
        Command::Help(topic)
    } else {
        parse_positionals(&positionals)
    };
    Cli {
        json,
        socket,
        dry_run,
        idempotency_key,
        command,
    }
}

mod parser;
mod tail;
mod work_agent;
mod workspace;

use parser::*;
use tail::*;
pub(crate) use tail::{command_name_token, confirm_return, current_dir_string};
use work_agent::*;
use workspace::*;
