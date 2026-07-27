use super::*;
use crate::workspace_root_selection::{WorkspaceRootSelection, WorkspaceRootSelectionError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Cli {
    pub(super) json: bool,
    pub(super) quiet: bool,
    pub(super) socket: PathBuf,
    pub(super) dry_run: bool,
    /// Declared by the registry spec this invocation matched; `None` only when
    /// the argv never resolved to a spec.
    pub(super) side_effect_level: Option<crate::registry::SideEffectLevel>,
    /// The argv exactly as typed. The dry-run preview echoes it back with
    /// `--dry-run` removed, so the apply line can never drift from the command.
    pub(super) argv: Vec<String>,
    pub(super) command: Command,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ParsedInvocation {
    pub(super) json: bool,
    pub(super) human: bool,
    pub(super) quiet: bool,
    pub(super) socket: PathBuf,
    pub(super) dry_run: bool,
    pub(super) side_effect_level: Option<crate::registry::SideEffectLevel>,
    pub(super) argv: Vec<String>,
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
    DebugClassify(DebugClassifyArgs),
    Devices(devices::DevicesArgs),
    DeviceKeyStatus(devices::DeviceKeyStatusArgs),
    Recovery(recovery::RecoveryArgs),
    Events(EventsArgs),
    Conflicts(conflict_commands::ConflictsArgs),
    Resolve(conflict_commands::ResolveArgs),
    Deletions(deletion_commands::DeletionsArgs),
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
    Request(DeviceApprovalRequestId),
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
    pub(super) device_id: DeviceId,
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
                crate::registry::DefinitionTarget::Recovery(action) => {
                    parse_recovery_command(action, &definition.values)
                }
                crate::registry::DefinitionTarget::DebugClassify => {
                    parse_debug_classify_command(&definition.values)
                }
                crate::registry::DefinitionTarget::SyncWait => {
                    parse_sync_wait_command(&definition.values)
                }
            };
            ParsedInvocation {
                json: definition.json,
                human: definition.human,
                quiet: definition.quiet,
                socket: definition.socket,
                dry_run: definition.dry_run,
                side_effect_level: Some(definition.side_effect_level),
                argv: args,
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
            side_effect_level: None,
            argv: args,
            command: Err(error),
        },
    }
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
            Command::DebugClassify(_) => CommandName::Unknown,
            Command::Recovery(_) => CommandName::Recover,
            Command::Work(_) => CommandName::Work,
            Command::Events(_) => CommandName::Events,
            Command::Conflicts(_) => CommandName::Conflicts,
            Command::Resolve(_) => CommandName::Resolve,
            Command::Deletions(_) => CommandName::Deletions,
            Command::Devices(args) => args.command_name(),
            Command::DeviceKeyStatus(_) => CommandName::DeviceKeyStatus,
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
mod conflicts;
mod connect;
mod context;
mod deletions;
mod device_parse;
mod parser;
mod prompt;
mod recovery_parse;
mod work_agent;
mod workspace;

use args::*;
use conflicts::*;
use connect::*;
pub(crate) use context::current_dir_string;
use deletions::*;
use device_parse::*;
use parser::*;
pub(crate) use prompt::confirm_return;
use recovery_parse::*;
use work_agent::*;
use workspace::*;

pub(crate) fn command_name_token(command: CommandName) -> &'static str {
    command.token()
}
