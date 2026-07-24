use super::*;
use crate::workspace_root_selection::{WorkspaceRootSelection, WorkspaceRootSelectionError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Cli {
    pub(super) json: bool,
    pub(super) quiet: bool,
    pub(super) socket: PathBuf,
    pub(super) dry_run: bool,
    pub(super) command: Command,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ParsedInvocation {
    pub(super) json: bool,
    pub(super) human: bool,
    pub(super) quiet: bool,
    pub(super) socket: PathBuf,
    pub(super) dry_run: bool,
    pub(super) command: Result<Command, ParseError>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Command {
    Help(Option<Vec<String>>),
    Version,
    Contract(ContractMode),
    Update(UpdateArgs),
    Login(login::LoginArgs),
    Logout,
    Approve(ApproveArgs),
    Deny(ApproveArgs),
    Revoke(RevokeArgs),
    Setup(SetupArgs),
    Status(StatusArgs),
    Tui(TuiArgs),
    SyncWait(SyncWaitArgs),
    SyncAttention,
    SyncRetry(crate::sync_attention::RetrySelector),
    SyncDismiss(String),
    DebugClassify(DebugClassifyArgs),
    Devices(devices::DevicesArgs),
    Recovery(recovery::RecoveryArgs),
    Events(EventsArgs),
    WorkCreate(work::WorkCreateArgs),
    Work(work::WorkListArgs),
    WorkDiff(work::WorkSelectorArgs),
    Review(work::WorkSelectorArgs),
    WorkAccept(work::WorkSelectorArgs),
    WorkDiscard(work::WorkSelectorArgs),
    WorkRestore(work::WorkSelectorArgs),
    WorkCleanup(work::WorkCleanupArgs),
    ForgetLocal(ForgetLocalArgs),
    Archive(ArchiveArgs),
    Purge(PurgeArgs),
    BootstrapSsh(bootstrap::BootstrapSshArgs),
    Daemon(DaemonCommand),
    DiagnosticsCollect(WorkspaceSelection),
    Doctor(DoctorArgs),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DoctorArgs {
    pub(super) engine: bowline_core::commands::DoctorEngine,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ContractMode {
    Full,
    Summary,
    Topic(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CommandUsageError {
    pub(super) command: CommandName,
    pub(super) code: &'static str,
    pub(super) message: String,
    pub(super) next_actions: Vec<RepairCommand>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ParseError {
    Command(CommandUsageError),
    Usage {
        command: CommandName,
        message: String,
    },
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WorkspaceSelection {
    pub(super) root: String,
    pub(super) project: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TrustRequestSelector {
    Request(String),
    Code(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ApproveArgs {
    pub(super) selection: WorkspaceSelection,
    pub(super) selector: TrustRequestSelector,
    pub(super) yes: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RevokeArgs {
    pub(super) selection: WorkspaceSelection,
    pub(super) device_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SetupArgs {
    pub(super) mode: SetupMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SetupMode {
    Machine { root: Option<String> },
    Project { project_path: String, yes: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StatusArgs {
    pub(super) selection: WorkspaceSelection,
    pub(super) watch: bool,
    pub(super) include_all: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TuiArgs {
    pub(super) selection: WorkspaceSelection,
}

/// Machine-facing `bowline sync wait`: block until the daemon reports the
/// workspace at or past `target_state`, or `timeout` elapses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SyncWaitArgs {
    pub(super) workspace_id: String,
    pub(super) target_state: bowline_core::introspection::WorkspaceReadiness,
    pub(super) timeout: std::time::Duration,
}

/// Hidden `bowline debug classify <path>` affordance. Not in public help or the
/// command registry; prints only classification / mode / access.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DebugClassifyArgs {
    pub(super) path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct EventsArgs {
    pub(super) selection: WorkspaceSelection,
    pub(super) limit: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OutputMode {
    Human,
    Json,
    Quiet,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct UpdateArgs {
    pub(super) check: bool,
    pub(super) version: Option<String>,
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
    pub(super) status_json: String,
}

pub(super) fn parse_args<I, S>(args: I) -> ParsedInvocation
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let args = args.into_iter().map(Into::into).collect::<Vec<String>>();
    match crate::registry::resolve_definition(&args) {
        Ok(resolved) => {
            let definition = resolved.invocation;
            let command = match resolved.target {
                crate::registry::DefinitionTarget::Public(command) => {
                    construct_command(command, &definition.values)
                }
                crate::registry::DefinitionTarget::DebugClassify => {
                    parse_debug_classify_command(&definition.values)
                }
                crate::registry::DefinitionTarget::SyncWait => {
                    parse_sync_wait_command(&definition.values)
                }
                crate::registry::DefinitionTarget::SyncAttention => Ok(Command::SyncAttention),
                crate::registry::DefinitionTarget::SyncRetry => {
                    parse_sync_retry_command(&definition.values)
                }
                crate::registry::DefinitionTarget::SyncDismiss => {
                    parse_sync_dismiss_command(&definition.values)
                }
            }
            .and_then(|command| validate_quiet(command, definition.quiet));
            ParsedInvocation {
                json: definition.json,
                human: definition.human,
                quiet: definition.quiet,
                socket: definition.socket,
                dry_run: definition.dry_run,
                command,
            }
        }
        Err(crate::registry::DefinitionFailure {
            json,
            human,
            quiet,
            error,
        }) => ParsedInvocation {
            json,
            human,
            quiet,
            socket: default_socket_path(),
            dry_run: false,
            command: Err(error),
        },
    }
}

fn validate_quiet(command: Command, quiet: bool) -> Result<Command, ParseError> {
    if !quiet || command_supports_quiet(&command) {
        return Ok(command);
    }
    usage_error(
        command.name(),
        "--quiet is only available for work, events, and devices list",
    )
}

fn command_supports_quiet(command: &Command) -> bool {
    matches!(
        command,
        Command::Work(_) | Command::Events(_) | Command::Devices(devices::DevicesArgs::List { .. })
    )
}

impl Command {
    pub(super) fn name(&self) -> CommandName {
        match self {
            Command::Help(_) => CommandName::Help,
            Command::Version => CommandName::Version,
            Command::Contract(_) => CommandName::Contract,
            Command::Update(_) => CommandName::Update,
            Command::Login(_) => CommandName::Login,
            Command::Logout => CommandName::Logout,
            Command::Approve(_) => CommandName::Approve,
            Command::Deny(_) => CommandName::Deny,
            Command::Revoke(_) => CommandName::Revoke,
            Command::Setup(_) => CommandName::Setup,
            Command::Status(_) => CommandName::Status,
            Command::Tui(_) => CommandName::Tui,
            Command::SyncWait(_) => CommandName::Unknown,
            Command::SyncAttention | Command::SyncRetry(_) | Command::SyncDismiss(_) => {
                CommandName::Unknown
            }
            Command::DebugClassify(_) => CommandName::Unknown,
            Command::Recovery(_) => CommandName::Recover,
            Command::Work(_) => CommandName::Work,
            Command::Events(_) => CommandName::Events,
            Command::Devices(args) => args.command_name(),
            Command::WorkCreate(_) => CommandName::WorkCreate,
            Command::WorkDiff(_) => CommandName::Diff,
            Command::Review(_) => CommandName::Review,
            Command::WorkAccept(_) => CommandName::Accept,
            Command::WorkDiscard(_) => CommandName::Discard,
            Command::WorkRestore(_) => CommandName::Restore,
            Command::WorkCleanup(_) => CommandName::Cleanup,
            Command::ForgetLocal(_) => CommandName::ForgetLocal,
            Command::Archive(_) => CommandName::Archive,
            Command::Purge(_) => CommandName::Purge,
            Command::BootstrapSsh(_) => CommandName::Connect,
            Command::Daemon(DaemonCommand::Start) => CommandName::DaemonStart,
            Command::Daemon(DaemonCommand::Stop) => CommandName::DaemonStop,
            Command::Daemon(DaemonCommand::Status) => CommandName::DaemonStatus,
            Command::Daemon(DaemonCommand::Install) => CommandName::DaemonInstall,
            Command::Daemon(DaemonCommand::Restart) => CommandName::DaemonRestart,
            Command::Daemon(DaemonCommand::Uninstall) => CommandName::DaemonUninstall,
            Command::DiagnosticsCollect(_) => CommandName::DiagnosticsCollect,
            Command::Doctor(_) => CommandName::Doctor,
        }
    }
}

pub(super) fn default_socket_path() -> PathBuf {
    default_control_socket_path().unwrap_or_else(|_| PathBuf::from(DEFAULT_SOCKET_FALLBACK))
}

mod args;
mod connect;
mod context;
mod device_parse;
mod parser;
mod prompt;
mod recovery_parse;
mod work_agent;
mod workspace;

use args::*;
use connect::*;
pub(crate) use context::current_dir_string;
use device_parse::*;
use parser::*;
pub(crate) use prompt::confirm_return;
use recovery_parse::*;
use work_agent::*;
use workspace::*;

pub(crate) fn command_name_token(command: CommandName) -> &'static str {
    command.token()
}
