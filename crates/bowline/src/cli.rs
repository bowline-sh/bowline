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
    ApproveMergePlugin(MergePluginApproveArgs),
    Deny(ApproveArgs),
    Revoke(RevokeArgs),
    Setup(SetupArgs),
    Status(StatusArgs),
    Tui(TuiArgs),
    DebugClassify(DebugClassifyArgs),
    Devices(devices::DevicesArgs),
    Recovery(recovery::RecoveryArgs),
    Resolve(resolve::ResolveArgs),
    Events(EventsArgs),
    History(HistoryArgs),
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
    AgentLeaseCreate(agent::AgentLeaseCreateArgs),
    AgentContext(agent::AgentLeaseSelectorArgs),
    AgentPrompt(agent::AgentLeaseSelectorArgs),
    AgentComplete(agent::AgentLeaseSelectorArgs),
    AgentCancel(agent::AgentLeaseSelectorArgs),
    AgentExtend(agent::AgentLeaseExtendArgs),
    AgentMcpToken(agent::AgentMcpTokenArgs),
    Mcp(McpArgs),
    LeaseJoin(lease::LeaseJoinArgs),
    BootstrapSsh(bootstrap::BootstrapSshArgs),
    Handoff(HandoffArgs),
    HandoffInstallBundle,
    Daemon(DaemonCommand),
    DiagnosticsCollect(WorkspaceSelection),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ContractMode {
    Full,
    Summary,
    Topic(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct McpArgs {
    pub(super) lease_id: Option<String>,
    pub(super) token_file: Option<String>,
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
pub(super) struct MergePluginApproveArgs {
    pub(super) selection: WorkspaceSelection,
    pub(super) id: String,
    pub(super) version: String,
    pub(super) digest: String,
    pub(super) matcher_version: Option<String>,
    pub(super) validator_version: Option<String>,
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
pub(super) struct HistoryArgs {
    pub(super) target_path: String,
    pub(super) mode: HistoryArgMode,
    pub(super) limit: u32,
    pub(super) cursor: Option<usize>,
    pub(super) since: Option<String>,
    pub(super) until: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum HistoryArgMode {
    Timeline,
    Path,
    Diff { from: String, to: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HandoffArgs {
    pub(super) target: String,
    pub(super) agent: Option<HandoffAgent>,
    pub(super) session: Option<String>,
    pub(super) prompt: Option<String>,
    pub(super) prompt_file: Option<String>,
    pub(super) project: Option<String>,
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
                crate::registry::DefinitionTarget::HandoffInstallBundle => {
                    if definition.values.positionals().is_empty() {
                        Ok(Command::HandoffInstallBundle)
                    } else {
                        usage_error(
                            CommandName::Handoff,
                            "internal handoff install-bundle takes no positional arguments",
                        )
                    }
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
        "--quiet is only available for work, events, history list/path, and devices list",
    )
}

fn command_supports_quiet(command: &Command) -> bool {
    matches!(
        command,
        Command::History(HistoryArgs {
            mode: HistoryArgMode::Timeline | HistoryArgMode::Path,
            ..
        }) | Command::Work(_)
            | Command::Events(_)
            | Command::Devices(devices::DevicesArgs::List { .. })
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
            Command::Approve(_) | Command::ApproveMergePlugin(_) => CommandName::Approve,
            Command::Deny(_) => CommandName::Deny,
            Command::Revoke(_) => CommandName::Revoke,
            Command::Setup(_) => CommandName::Setup,
            Command::Status(_) => CommandName::Status,
            Command::Tui(_) => CommandName::Tui,
            Command::DebugClassify(_) => CommandName::Unknown,
            Command::Mcp(_) => CommandName::Mcp,
            Command::Recovery(_) => CommandName::Recover,
            Command::Resolve(_) => CommandName::Resolve,
            Command::Work(_) => CommandName::Work,
            Command::Events(_) => CommandName::Events,
            Command::History(_) => CommandName::History,
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
            Command::AgentLeaseCreate(_) => CommandName::AgentStart,
            Command::AgentContext(_) => CommandName::AgentContext,
            Command::AgentPrompt(_) => CommandName::AgentPrompt,
            Command::AgentComplete(_) => CommandName::AgentComplete,
            Command::AgentCancel(_) => CommandName::AgentCancel,
            Command::AgentExtend(_) => CommandName::AgentExtend,
            Command::AgentMcpToken(_) => CommandName::AgentMcpToken,
            Command::LeaseJoin(_) => CommandName::LeaseJoin,
            Command::BootstrapSsh(_) => CommandName::Connect,
            Command::Handoff(_) | Command::HandoffInstallBundle => CommandName::Handoff,
            Command::Daemon(DaemonCommand::Start) => CommandName::DaemonStart,
            Command::Daemon(DaemonCommand::Stop) => CommandName::DaemonStop,
            Command::Daemon(DaemonCommand::Status) => CommandName::DaemonStatus,
            Command::Daemon(DaemonCommand::Install) => CommandName::DaemonInstall,
            Command::Daemon(DaemonCommand::Restart) => CommandName::DaemonRestart,
            Command::Daemon(DaemonCommand::Uninstall) => CommandName::DaemonUninstall,
            Command::DiagnosticsCollect(_) => CommandName::DiagnosticsCollect,
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
mod lease_parse;
mod parser;
mod prompt;
mod recovery_parse;
mod resolve_parse;
mod work_agent;
mod workspace;

use args::*;
use connect::*;
pub(crate) use context::current_dir_string;
use device_parse::*;
use lease_parse::*;
use parser::*;
pub(crate) use prompt::confirm_return;
use recovery_parse::*;
use resolve_parse::*;
use work_agent::*;
use workspace::*;

pub(crate) fn command_name_token(command: CommandName) -> &'static str {
    command.token()
}
