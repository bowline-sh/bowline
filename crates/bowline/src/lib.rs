#![deny(unsafe_code)]

use std::ffi::OsString;
use std::io::{self, IsTerminal, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, ExitCode, Stdio};
use std::time::{Duration, Instant};
use std::{env, thread};

mod agent;
mod agent_adapters;
mod bootstrap;
mod dev_spike;
mod devices;
mod login;
mod recovery;
mod resolve;
mod runtime;
mod surface;
mod work;

use bowline_core::commands::{
    BoundedOutputControls, CONTRACT_VERSION, CliCommandDescriptor, CliCommandExample,
    CliCommandGroup, CliCommandOption, CommandError, CommandErrorOutput, CommandErrorStatus,
    CommandName, CommandRecoverability, ContractCommandOutput, ContractFixtureDescriptor,
    DaemonCommandOutput, DaemonProcessOutput, DaemonServiceOutput, DaemonServiceState,
    DaemonStatusOutput, DiagnosticsCollectCommandOutput, DryRunCommandOutput, DryRunStatus,
    EventsCommandOutput, HelpCommandOutput, PrewarmCommandOutcome, PrewarmCommandOutput,
    PrewarmCommandState, StatusCommandOutput, VersionCommandOutput, WatchFrame,
};
use bowline_core::ids::{DeviceApprovalRequestId, DeviceId, WorkspaceId};
use bowline_core::status::{
    SafeAction, StatusItem, StatusItemKind, StatusLevel, StatusSubject, StatusSubjectKind,
};
use bowline_local::{
    bootstrap::process::{ProcessRunner, SystemProcessRunner},
    explain::ExplainOptions,
    init::{InitOptions, LocalInitError},
    linux_service::{self, LinuxServiceConfig, LinuxServiceOptions},
    macos_service::{self, MacosServiceConfig, MacosServiceOptions},
    metadata::{CommandIdempotencyRecord, MetadataStore, default_database_path},
    setup::{PrewarmOptions, SetupRunError, prewarm_project, redact::redact_setup_text},
    status::{EventsOptions, StatusOptions},
};
use dev_spike::{
    run_fake_cloud_spike, run_hosted_cloud_spike_from_env, skip_hosted_cloud_spike_from_env,
};
const PROTOCOL: &str = "bowline.local";
const PROTOCOL_VERSION: u32 = 1;
const DEFAULT_SOCKET: &str = "/tmp/bowline-daemon.sock";
const CLI_VERSION: &str = env!("CARGO_PKG_VERSION");
const EVENT_SCHEMA_VERSION: u16 = 1;
const PACKAGE_CONTRACT_SOURCE: &str = "packages/contracts/src/index.ts";
const DAEMON_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const ENV_METADATA_DB: &str = "BOWLINE_METADATA_DB";
const ENV_GENERATED_AT: &str = "BOWLINE_GENERATED_AT";
const EXIT_USAGE: u8 = 2;
const EXIT_RUNTIME: u8 = 1;
const DEFAULT_EXPLORATION_LIMIT: usize = 20;
const MAX_EXPLORATION_LIMIT: usize = 100;
const MAX_EXPLORATION_CURSOR_OFFSET: usize = 10_000;
const DEFAULT_AGENT_HYDRATE_BUDGET_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Cli {
    json: bool,
    socket: PathBuf,
    dry_run: bool,
    idempotency_key: Option<String>,
    command: Command,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
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
struct CommandUsageError {
    command: CommandName,
    code: &'static str,
    message: String,
    next_actions: Vec<SafeAction>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InitArgs {
    root: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ApproveArgs {
    request_id: Option<String>,
    yes: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RevokeArgs {
    device_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PrewarmArgs {
    project_path: String,
    approve_setup: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SetupArgs {
    project_path: Option<String>,
    yes: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StatusArgs {
    path: Option<String>,
    watch: bool,
    workspace: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActionsArgs {
    path: Option<String>,
    workspace: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TuiArgs {
    path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchArgs {
    query: String,
    path: Option<String>,
    limit: usize,
    cursor: Option<usize>,
    path_prefix: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SymbolsArgs {
    query: String,
    path: Option<String>,
    limit: usize,
    cursor: Option<usize>,
    path_prefix: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExplainArgs {
    path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EventsArgs {
    path: Option<String>,
    workspace: bool,
    limit: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CloudSpikeArgs {
    provider: CloudSpikeProvider,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloudSpikeProvider {
    Fake,
    Hosted,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct CloudSpikeFakeOutput<'a> {
    ok: bool,
    command: &'static str,
    provider: &'static str,
    workspace_id: &'a str,
    starting_version: u64,
    advanced_version: u64,
    pack_object_count: usize,
    source_file_count: usize,
    hydrated_cold_file_byte_len: usize,
    stale_ref_detected: bool,
    device_approval_harness_only: bool,
    event_count: usize,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct CloudSpikeSkipOutput {
    ok: bool,
    command: &'static str,
    provider: &'static str,
    skipped: bool,
    missing_env: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DaemonCommand {
    Start,
    Stop,
    Status,
    Install,
    Restart,
    Uninstall,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Handshake {
    daemon_version: String,
    sync_json: Option<String>,
}

pub fn main() -> ExitCode {
    install_panic_hook();
    let cli = parse_args(env::args().skip(1));
    run(cli)
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|_| {
        eprintln!(
            "bowline hit an internal error. Run `bowline status` and inspect daemon logs; environment values were not printed."
        );
    }));
}

fn parse_args<I, S>(args: I) -> Cli
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

fn parse_positionals(args: &[String]) -> Command {
    match args {
        [] => Command::Help(None),
        [command] if command == "help" => Command::Help(None),
        [command, rest @ ..] if command == "help" => Command::Help(Some(rest.to_vec())),
        [command] if command == "version" => Command::Version,
        [command] if command == "contract" || command == "schema" => Command::Contract,
        [command, rest @ ..] if command == "login" => parse_login_command(rest),
        [command, rest @ ..] if command == "approve" => parse_approve_command(rest),
        [command, rest @ ..] if command == "revoke" => parse_revoke_command(rest),
        [command, rest @ ..] if command == "recover" => parse_recover_command(rest),
        [command, rest @ ..] if command == "init" => parse_init_command(rest),
        [command, rest @ ..] if command == "setup" => parse_setup_command(rest),
        [command, rest @ ..] if command == "prewarm" => parse_prewarm_command(rest),
        [command, rest @ ..] if command == "status" => parse_status_command(rest),
        [command, rest @ ..] if command == "actions" => parse_actions_command(rest),
        [command, rest @ ..] if command == "tui" => parse_tui_command(rest),
        [command, rest @ ..] if command == "search" => parse_search_command(rest),
        [command, rest @ ..] if command == "symbols" => parse_symbols_command(rest),
        [command, rest @ ..] if command == "explain" => parse_explain_command(rest),
        [command, rest @ ..] if command == "devices" => parse_devices_command(rest),
        [command, rest @ ..] if command == "recovery" => parse_recovery_command(rest),
        [command, rest @ ..] if command == "events" => parse_events_command(rest),
        [command, rest @ ..] if command == "workon" => parse_workon_command(rest),
        [command, rest @ ..] if command == "work" => parse_work_command(rest),
        [command, rest @ ..] if command == "review" => parse_review_command(rest),
        [command, rest @ ..] if command == "diff" => parse_work_selector_command(
            CommandName::Diff,
            rest,
            "bowline diff requires a work-view id or name",
        ),
        [command, rest @ ..] if command == "accept" => parse_work_selector_command(
            CommandName::Accept,
            rest,
            "bowline accept requires a work-view id or name",
        ),
        [command, rest @ ..] if command == "discard" => parse_work_selector_command(
            CommandName::Discard,
            rest,
            "bowline discard requires a work-view id or name",
        ),
        [command, rest @ ..] if command == "restore" => parse_work_selector_command(
            CommandName::Restore,
            rest,
            "bowline restore requires a work-view id or name",
        ),
        [command, rest @ ..] if command == "cleanup" => parse_cleanup_command(rest),
        [command, subcommand, rest @ ..] if command == "dev" && subcommand == "cloud-spike" => {
            parse_dev_cloud_spike_command(rest)
        }
        [command, ..] if command == "dev" => {
            usage_error(CommandName::Unknown, "expected `bowline dev cloud-spike`")
        }
        [command, subcommand, rest @ ..] if command == "bootstrap" && subcommand == "ssh" => {
            parse_bootstrap_ssh_command(rest)
        }
        [command, rest @ ..] if command == "connect" => parse_connect_command(rest),
        [command, ..] if command == "bootstrap" => {
            usage_error(CommandName::Connect, "expected `bowline connect <host>`")
        }
        [command, group, action, ..]
            if command == "agent" && group == "lease" && action == "create" =>
        {
            parse_agent_lease_create_command(&args[3..])
        }
        [command, subcommand, rest @ ..] if command == "agent" && subcommand == "start" => {
            parse_agent_start_command(rest)
        }
        [command, subcommand, rest @ ..] if command == "agent" && subcommand == "context" => {
            parse_agent_selector_command(CommandName::AgentContext, rest)
        }
        [command, subcommand, rest @ ..] if command == "agent" && subcommand == "prompt" => {
            parse_agent_selector_command(CommandName::AgentPrompt, rest)
        }
        [command, subcommand, rest @ ..] if command == "agent" && subcommand == "publish" => {
            parse_agent_selector_command(CommandName::AgentPublish, rest)
        }
        [command, subcommand, rest @ ..] if command == "agent" && subcommand == "complete" => {
            parse_agent_selector_command(CommandName::AgentComplete, rest)
        }
        [command, subcommand, rest @ ..] if command == "agent" && subcommand == "budget" => {
            parse_agent_budget_command(rest)
        }
        [command, ..] if command == "agent" => usage_error(
            CommandName::AgentStart,
            "expected `bowline agent start ...`, `bowline agent context ...`, `bowline agent prompt ...`, `bowline agent publish ...`, `bowline agent complete ...`, or `bowline agent budget ...`",
        ),
        [command, rest @ ..] if command == "resolve" => parse_resolve_command(rest),
        [command, subcommand] if command == "daemon" && subcommand == "start" => {
            Command::Daemon(DaemonCommand::Start)
        }
        [command, subcommand] if command == "daemon" && subcommand == "stop" => {
            Command::Daemon(DaemonCommand::Stop)
        }
        [command, subcommand] if command == "daemon" && subcommand == "status" => {
            Command::Daemon(DaemonCommand::Status)
        }
        [command, subcommand] if command == "daemon" && subcommand == "install" => {
            Command::Daemon(DaemonCommand::Install)
        }
        [command, subcommand] if command == "daemon" && subcommand == "restart" => {
            Command::Daemon(DaemonCommand::Restart)
        }
        [command, subcommand] if command == "daemon" && subcommand == "uninstall" => {
            Command::Daemon(DaemonCommand::Uninstall)
        }
        [command, ..] if command == "daemon" => usage_error(
            CommandName::DaemonStatus,
            "expected `bowline daemon start`, `bowline daemon stop`, `bowline daemon status`, `bowline daemon install`, `bowline daemon restart`, or `bowline daemon uninstall`",
        ),
        [command, subcommand] if command == "diagnostics" && subcommand == "collect" => {
            Command::DiagnosticsCollect
        }
        [command, ..] if command == "diagnostics" => usage_error(
            CommandName::DiagnosticsCollect,
            "expected `bowline diagnostics collect`",
        ),
        [command, ..] => Command::Unknown(command.clone()),
    }
}

fn run(cli: Cli) -> ExitCode {
    let parsed_error = matches!(
        cli.command,
        Command::CommandUsageError(_) | Command::UsageError { .. } | Command::Unknown(_)
    );
    if !parsed_error {
        if cli.dry_run {
            return print_dry_run(cli);
        }
        if cli.idempotency_key.is_some() {
            return run_with_idempotency(cli);
        }
    }
    match cli.command {
        Command::Help(topic) => {
            print_help(topic.as_deref(), cli.json);
            ExitCode::SUCCESS
        }
        Command::Version => {
            print_version(cli.json);
            ExitCode::SUCCESS
        }
        Command::Contract => {
            print_contract(cli.json);
            ExitCode::SUCCESS
        }
        Command::Login(args) => print_login(args, cli.json),
        Command::Approve(args) => print_approve(args, cli.json),
        Command::Revoke(args) => print_revoke(args, cli.json),
        Command::Init(args) => print_init(args, cli.json),
        Command::Prewarm(args) => print_prewarm(args, cli.json),
        Command::Setup(args) => print_setup(args, cli.json),
        Command::Status(args) => print_status(args, cli.json),
        Command::Actions(args) => print_actions(args, cli.json),
        Command::Tui(args) => print_tui(args, cli.json, &cli.socket),
        Command::Search(args) => print_search(args, cli.json),
        Command::Symbols(args) => print_symbols(args, cli.json),
        Command::Explain(args) => print_explain(args, cli.json),
        Command::Devices(args) => print_devices(args, cli.json),
        Command::Recovery(args) => print_recovery(args, cli.json),
        Command::Resolve(args) => print_resolve(args, cli.json, &cli.socket),
        Command::Events(args) => print_events(args, cli.json),
        Command::Workon(args) => print_workon(args, cli.json),
        Command::Work(args) => print_work(args, cli.json),
        Command::WorkDiff(args) => print_work_diff(args, cli.json),
        Command::Review(args) => print_work_review(args, cli.json),
        Command::WorkAccept(args) => print_work_lifecycle(CommandName::Accept, args, cli.json),
        Command::WorkDiscard(args) => print_work_lifecycle(CommandName::Discard, args, cli.json),
        Command::WorkRestore(args) => print_work_lifecycle(CommandName::Restore, args, cli.json),
        Command::WorkCleanup(args) => print_work_cleanup(args, cli.json),
        Command::AgentLeaseCreate(args) => print_agent_lease_create(args, cli.json),
        Command::AgentContext(args) => print_agent_context(args, cli.json),
        Command::AgentPrompt(args) => print_agent_prompt(args, cli.json),
        Command::AgentPublish(args) => {
            print_agent_tool_action(CommandName::AgentPublish, args, cli.json)
        }
        Command::AgentComplete(args) => {
            print_agent_tool_action(CommandName::AgentComplete, args, cli.json)
        }
        Command::AgentBudget(args) => print_agent_budget(args, cli.json),
        Command::BootstrapSsh(args) => print_bootstrap_ssh(args, cli.json),
        Command::DevCloudSpike(args) => print_dev_cloud_spike(args, cli.json),
        Command::Daemon(DaemonCommand::Start) => print_daemon_start(&cli.socket, cli.json),
        Command::Daemon(DaemonCommand::Stop) => print_daemon_stop(&cli.socket, cli.json),
        Command::Daemon(DaemonCommand::Status) => {
            print_daemon_status(&cli.socket, cli.json);
            ExitCode::SUCCESS
        }
        Command::Daemon(DaemonCommand::Install) => {
            print_daemon_service_install(&cli.socket, cli.json)
        }
        Command::Daemon(DaemonCommand::Restart) => print_daemon_service_restart(cli.json),
        Command::Daemon(DaemonCommand::Uninstall) => print_daemon_service_uninstall(cli.json),
        Command::DiagnosticsCollect => print_diagnostics_collect(&cli.socket, cli.json),
        Command::CommandUsageError(error) => {
            print_command_usage_error(error, generated_at(), cli.json);
            ExitCode::from(EXIT_USAGE)
        }
        Command::UsageError { command, message } => {
            print_usage_error(command, "usage_error", &message, cli.json);
            ExitCode::from(EXIT_USAGE)
        }
        Command::Unknown(command) => {
            print_unknown_command(&command, cli.json);
            ExitCode::from(EXIT_USAGE)
        }
    }
}

fn usage_error(command: CommandName, message: impl Into<String>) -> Command {
    Command::UsageError {
        command,
        message: message.into(),
    }
}

fn print_dry_run(cli: Cli) -> ExitCode {
    let Some((command_name, target, would_change, risk)) = dry_run_plan(&cli.command) else {
        print_usage_error(
            command_name_for_command(&cli.command),
            "dry_run_unsupported",
            "--dry-run is not supported for this command",
            cli.json,
        );
        return ExitCode::from(EXIT_USAGE);
    };
    let (apply_command, warnings) = dry_run_apply_command(&cli);
    let output = DryRunCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: command_name,
        generated_at: generated_at(),
        status: DryRunStatus::DryRun,
        allowed: true,
        risk,
        target,
        would_change,
        warnings,
        apply_command,
        next_actions: vec![SafeAction {
            label: "Run the command without --dry-run".to_string(),
            command: None,
        }],
    };
    if cli.json {
        print_json(&output);
    } else {
        println!(
            "bowline dry-run: {}\nTarget: {}\nRisk: {}\nWould change:\n  {}",
            command_name_token(command_name),
            output.target,
            output.risk,
            output.would_change.join("\n  ")
        );
        println!("\nApply:\n  {}", output.apply_command);
    }
    ExitCode::SUCCESS
}

fn dry_run_apply_command(cli: &Cli) -> (String, Vec<String>) {
    let Some(command_args) = command_args_for_apply(&cli.command) else {
        return ("bowline".to_string(), Vec::new());
    };
    let mut args = vec!["bowline".to_string()];
    if cli.socket != Path::new(DEFAULT_SOCKET) {
        args.push("--socket".to_string());
        args.push(cli.socket.display().to_string());
    }
    let mut warnings = Vec::new();
    if let Some(key) = &cli.idempotency_key {
        if is_idempotent_mutation(&cli.command) {
            args.push("--json".to_string());
            args.push("--idempotency-key".to_string());
            args.push(key.clone());
        } else {
            warnings.push(
                "Omitted --idempotency-key from applyCommand because this command cannot be replayed safely."
                    .to_string(),
            );
        }
    }
    args.extend(command_args);
    (shell_join(args), warnings)
}

fn command_args_for_apply(command: &Command) -> Option<Vec<String>> {
    match command {
        Command::Recovery(recovery::RecoveryArgs::Verify { envelope_id }) => Some(vec![
            "recover".to_string(),
            "verify".to_string(),
            envelope_id.clone(),
        ]),
        Command::Recovery(recovery::RecoveryArgs::Use { envelope_id }) => Some(vec![
            "recover".to_string(),
            "use".to_string(),
            envelope_id.clone(),
        ]),
        _ => command_args_for_replay(command),
    }
}

fn run_with_idempotency(mut cli: Cli) -> ExitCode {
    let key = cli
        .idempotency_key
        .take()
        .expect("idempotency key checked by caller");
    let command_name = command_name_for_command(&cli.command);
    if !cli.json {
        print_usage_error(
            command_name,
            "idempotency_requires_json",
            "--idempotency-key requires --json so the replayed result has a stable shape",
            false,
        );
        return ExitCode::from(EXIT_USAGE);
    }
    if !is_idempotent_mutation(&cli.command) {
        print_usage_error(
            command_name,
            "idempotency_unsupported",
            "--idempotency-key is only supported for non-dry-run mutations",
            true,
        );
        return ExitCode::from(EXIT_USAGE);
    }
    let Some(command_args) = command_args_for_replay(&cli.command) else {
        print_usage_error(
            command_name,
            "idempotency_unsupported",
            "--idempotency-key is not available for this command",
            true,
        );
        return ExitCode::from(EXIT_USAGE);
    };
    let request_cwd = idempotency_cwd_for_request(&cli.command, &cli.socket);
    let request_hash = idempotency_request_hash(
        command_name,
        &command_args,
        &cli.socket,
        request_cwd.as_deref(),
    );
    let (store, workspace_id) = match open_idempotency_store() {
        Ok(opened) => opened,
        Err(message) => {
            print_runtime_error(command_name, generated_at(), &message, true);
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    let now = generated_at();
    let expires_at = idempotency_expires_at_from(&now);
    let pending_record = CommandIdempotencyRecord {
        workspace_id: workspace_id.clone(),
        idempotency_key: key.clone(),
        command: command_name_token(command_name).to_string(),
        request_hash: request_hash.clone(),
        result_json: "{}".to_string(),
        status: "pending".to_string(),
        created_at: now.clone(),
        updated_at: now,
        expires_at,
    };
    loop {
        match store.try_insert_command_idempotency_record(&pending_record) {
            Ok(true) => break,
            Ok(false) => match store.command_idempotency_record(&workspace_id, &key) {
                Ok(Some(record))
                    if idempotency_record_expired(&record, &pending_record.created_at) =>
                {
                    if let Err(error) = store.delete_command_idempotency_record(
                        &workspace_id,
                        &key,
                        &record.request_hash,
                    ) {
                        print_runtime_error(command_name, generated_at(), &error.to_string(), true);
                        return ExitCode::from(EXIT_RUNTIME);
                    }
                }
                Ok(Some(record)) if record.request_hash != request_hash => {
                    print_idempotency_conflict(command_name, &key);
                    return ExitCode::from(EXIT_USAGE);
                }
                Ok(Some(record)) if record.status == "success" => {
                    print_replayed_result(&record.result_json);
                    return ExitCode::SUCCESS;
                }
                Ok(Some(_record)) => {
                    print_idempotency_in_progress(command_name, &key);
                    return ExitCode::from(EXIT_RUNTIME);
                }
                Ok(None) => {
                    print_runtime_error(
                        command_name,
                        generated_at(),
                        "idempotency reservation disappeared before execution",
                        true,
                    );
                    return ExitCode::from(EXIT_RUNTIME);
                }
                Err(error) => {
                    print_runtime_error(command_name, generated_at(), &error.to_string(), true);
                    return ExitCode::from(EXIT_RUNTIME);
                }
            },
            Err(error) => {
                print_runtime_error(command_name, generated_at(), &error.to_string(), true);
                return ExitCode::from(EXIT_RUNTIME);
            }
        }
    }

    let mut child_args = Vec::new();
    child_args.push("--json".to_string());
    if cli.socket != Path::new(DEFAULT_SOCKET) {
        child_args.push("--socket".to_string());
        child_args.push(cli.socket.display().to_string());
    }
    child_args.extend(command_args.iter().cloned());

    let output = match env::current_exe().and_then(|exe| {
        ProcessCommand::new(exe)
            .args(&child_args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
    }) {
        Ok(output) => output,
        Err(error) => {
            let _ = store.delete_command_idempotency_record(&workspace_id, &key, &request_hash);
            print_runtime_error(command_name, generated_at(), &error.to_string(), true);
            return ExitCode::from(EXIT_RUNTIME);
        }
    };

    if output.status.success() {
        let result_json = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if serde_json::from_str::<serde_json::Value>(&result_json).is_ok() {
            let now = generated_at();
            let expires_at = idempotency_expires_at_from(&now);
            let record = CommandIdempotencyRecord {
                workspace_id: workspace_id.clone(),
                idempotency_key: key.clone(),
                command: command_name_token(command_name).to_string(),
                request_hash: request_hash.clone(),
                result_json,
                status: "success".to_string(),
                created_at: now.clone(),
                updated_at: now,
                expires_at,
            };
            if let Err(error) = store.finish_command_idempotency_record(&record) {
                print_runtime_error(command_name, generated_at(), &error.to_string(), true);
                return ExitCode::from(EXIT_RUNTIME);
            }
        }
    } else {
        let _ = store.delete_command_idempotency_record(&workspace_id, &key, &request_hash);
    }

    let _ = io::stdout().write_all(&output.stdout);
    let _ = io::stderr().write_all(&output.stderr);
    ExitCode::from(output.status.code().unwrap_or(i32::from(EXIT_RUNTIME)) as u8)
}

fn is_idempotent_mutation(command: &Command) -> bool {
    matches!(
        command,
        Command::Approve(_)
            | Command::Revoke(_)
            | Command::Recovery(recovery::RecoveryArgs::Create)
            | Command::Recovery(recovery::RecoveryArgs::Rotate)
            | Command::Recovery(recovery::RecoveryArgs::Revoke { .. })
            | Command::BootstrapSsh(_)
            | Command::Workon(_)
            | Command::WorkAccept(_)
            | Command::WorkDiscard(_)
            | Command::WorkRestore(_)
            | Command::WorkCleanup(_)
            | Command::AgentLeaseCreate(_)
            | Command::AgentPublish(_)
            | Command::AgentComplete(_)
            | Command::AgentBudget(_)
            | Command::Daemon(DaemonCommand::Install)
            | Command::Daemon(DaemonCommand::Restart)
            | Command::Daemon(DaemonCommand::Uninstall)
    )
}

fn open_idempotency_store() -> Result<(MetadataStore, WorkspaceId), String> {
    let store =
        MetadataStore::open(metadata_db_path_or_default()?).map_err(|error| error.to_string())?;
    let workspace_id = store
        .current_workspace()
        .map_err(|error| error.to_string())?
        .map(|workspace| workspace.id)
        .unwrap_or_else(|| WorkspaceId::new("ws_local_uninitialized"));
    Ok((store, workspace_id))
}

fn idempotency_request_hash(
    command: CommandName,
    args: &[String],
    socket: &Path,
    cwd: Option<&Path>,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(command_name_token(command).as_bytes());
    hasher.update(&[0]);
    hasher.update(b"socket");
    hasher.update(&[0]);
    hasher.update(socket.display().to_string().as_bytes());
    hasher.update(&[0]);
    if let Some(cwd) = cwd {
        hasher.update(b"cwd");
        hasher.update(&[0]);
        hasher.update(cwd.display().to_string().as_bytes());
        hasher.update(&[0]);
    }
    for arg in args {
        hasher.update(arg.as_bytes());
        hasher.update(&[0]);
    }
    hasher.finalize().to_hex().to_string()
}

fn idempotency_cwd_for_request(command: &Command, socket: &Path) -> Option<PathBuf> {
    (command_has_cwd_relative_target(command) || socket_depends_on_cwd(socket))
        .then(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn command_has_cwd_relative_target(command: &Command) -> bool {
    match command {
        Command::Workon(args) => path_depends_on_cwd(&args.project_path),
        Command::AgentLeaseCreate(args) => path_depends_on_cwd(&args.project_path),
        Command::BootstrapSsh(args) => {
            path_depends_on_cwd(&args.root)
                || args.artifact.as_deref().is_some_and(path_depends_on_cwd)
                || args.project.as_deref().is_some_and(path_depends_on_cwd)
        }
        _ => false,
    }
}

fn path_depends_on_cwd(path: &str) -> bool {
    if path == "~" || path.starts_with("~/") {
        return false;
    }
    !PathBuf::from(path).is_absolute()
}

fn socket_depends_on_cwd(path: &Path) -> bool {
    if path.is_absolute() {
        return false;
    }
    path.to_str().is_none_or(path_depends_on_cwd)
}

fn idempotency_expires_at_from(generated_at: &str) -> String {
    let base =
        time::OffsetDateTime::parse(generated_at, &time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    (base + time::Duration::days(7))
        .format(&time::format_description::well_known::Rfc3339)
        .expect("UTC timestamp should format")
}

fn idempotency_record_expired(record: &CommandIdempotencyRecord, generated_at: &str) -> bool {
    let Ok(expires_at) = time::OffsetDateTime::parse(
        &record.expires_at,
        &time::format_description::well_known::Rfc3339,
    ) else {
        return true;
    };
    let now =
        time::OffsetDateTime::parse(generated_at, &time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    expires_at <= now
}

fn print_replayed_result(result_json: &str) {
    match serde_json::from_str::<serde_json::Value>(result_json) {
        Ok(mut value) => {
            if let Some(object) = value.as_object_mut() {
                object.insert("replayed".to_string(), serde_json::Value::Bool(true));
            }
            print_json(&value);
        }
        Err(_) => println!("{result_json}"),
    }
}

fn print_idempotency_conflict(command: CommandName, key: &str) {
    print_json(&CommandErrorOutput {
        contract_version: CONTRACT_VERSION,
        command,
        generated_at: generated_at(),
        status: CommandErrorStatus::UsageError,
        error: CommandError {
            code: "idempotency_conflict".to_string(),
            message: "idempotency key was already used for a different request".to_string(),
            recoverability: CommandRecoverability::UserAction,
            remediation: Some(
                "Use the same request with this key, or choose a new key.".to_string(),
            ),
            details: Some(serde_json::json!({ "idempotencyKey": key })),
            retry_after_seconds: None,
            correlation_id: None,
        },
        next_actions: Vec::new(),
    });
}

fn print_idempotency_in_progress(command: CommandName, key: &str) {
    print_json(&CommandErrorOutput {
        contract_version: CONTRACT_VERSION,
        command,
        generated_at: generated_at(),
        status: CommandErrorStatus::Failed,
        error: CommandError {
            code: "idempotency_in_progress".to_string(),
            message: "idempotency key is already executing".to_string(),
            recoverability: CommandRecoverability::Retry,
            remediation: Some(
                "Retry the same request after the in-flight command finishes.".to_string(),
            ),
            details: Some(serde_json::json!({ "idempotencyKey": key })),
            retry_after_seconds: Some(1),
            correlation_id: None,
        },
        next_actions: Vec::new(),
    });
}

fn dry_run_plan(command: &Command) -> Option<(CommandName, String, Vec<String>, String)> {
    match command {
        Command::Approve(args) => Some((
            CommandName::Approve,
            args.request_id
                .clone()
                .unwrap_or_else(|| "first pending approval request".to_string()),
            vec!["approve a pending device trust request".to_string()],
            "trust-change".to_string(),
        )),
        Command::Revoke(args) => Some((
            CommandName::Revoke,
            args.device_id.clone(),
            vec!["revoke device trust".to_string()],
            "trust-change".to_string(),
        )),
        Command::Recovery(recovery::RecoveryArgs::Create) => Some((
            CommandName::Recover,
            "current workspace recovery key".to_string(),
            vec!["create a new recovery key envelope".to_string()],
            "secret-material".to_string(),
        )),
        Command::Recovery(recovery::RecoveryArgs::Rotate) => Some((
            CommandName::Recover,
            "current workspace recovery key".to_string(),
            vec!["rotate recovery key material".to_string()],
            "secret-material".to_string(),
        )),
        Command::Recovery(recovery::RecoveryArgs::Verify { envelope_id }) => Some((
            CommandName::Recover,
            envelope_id.clone(),
            vec!["verify recovery words from stdin".to_string()],
            "secret-material".to_string(),
        )),
        Command::Recovery(recovery::RecoveryArgs::Revoke { envelope_id }) => Some((
            CommandName::Recover,
            envelope_id.clone(),
            vec!["revoke recovery key envelope".to_string()],
            "trust-change".to_string(),
        )),
        Command::Recovery(recovery::RecoveryArgs::Use { envelope_id }) => Some((
            CommandName::Recover,
            envelope_id.clone(),
            vec!["submit recovery words from stdin and create a device grant".to_string()],
            "secret-material".to_string(),
        )),
        Command::BootstrapSsh(args) => Some((
            CommandName::Connect,
            args.host.clone(),
            vec![
                "install or update remote bowline binaries".to_string(),
                "optionally create a remote agent handoff".to_string(),
            ],
            "remote-mutation".to_string(),
        )),
        Command::Workon(args) => Some((
            CommandName::Workon,
            format!("{}:{}", args.project_path, args.name),
            vec!["create or reuse a work view".to_string()],
            "workspace-metadata".to_string(),
        )),
        Command::WorkAccept(args) => Some((
            CommandName::Accept,
            args.selector.clone(),
            vec!["apply work-view changes to the target project".to_string()],
            "filesystem-write".to_string(),
        )),
        Command::WorkDiscard(args) => Some((
            CommandName::Discard,
            args.selector.clone(),
            vec!["mark work view as discarded".to_string()],
            "workspace-metadata".to_string(),
        )),
        Command::WorkRestore(args) => Some((
            CommandName::Restore,
            args.selector.clone(),
            vec!["restore a discarded work view".to_string()],
            "workspace-metadata".to_string(),
        )),
        Command::WorkCleanup(args) => Some((
            CommandName::Cleanup,
            "retained work views".to_string(),
            if args.apply {
                vec!["remove cleanup-eligible work-view metadata and overlays".to_string()]
            } else {
                vec!["no changes; cleanup preview remains read-only".to_string()]
            },
            if args.apply {
                "workspace-metadata".to_string()
            } else {
                "none".to_string()
            },
        )),
        Command::AgentLeaseCreate(args) => Some((
            CommandName::AgentStart,
            args.project_path.clone(),
            vec![
                "create an agent lease".to_string(),
                "reserve hydration budget".to_string(),
                "optionally create a work view".to_string(),
            ],
            "workspace-metadata".to_string(),
        )),
        Command::AgentPublish(args) => Some((
            CommandName::AgentPublish,
            args.lease_id.clone(),
            vec!["publish agent output for review".to_string()],
            "workspace-metadata".to_string(),
        )),
        Command::AgentComplete(args) => Some((
            CommandName::AgentComplete,
            args.lease_id.clone(),
            vec!["mark agent lease complete".to_string()],
            "workspace-metadata".to_string(),
        )),
        Command::AgentBudget(args) => Some((
            CommandName::AgentBudget,
            args.lease_id.clone(),
            vec!["grant additional hydration budget".to_string()],
            "workspace-metadata".to_string(),
        )),
        Command::Daemon(DaemonCommand::Install) => Some((
            CommandName::DaemonInstall,
            "local OS service".to_string(),
            vec!["install or update daemon service files".to_string()],
            "service-mutation".to_string(),
        )),
        Command::Daemon(DaemonCommand::Restart) => Some((
            CommandName::DaemonRestart,
            "local OS service".to_string(),
            vec!["restart daemon service".to_string()],
            "service-mutation".to_string(),
        )),
        Command::Daemon(DaemonCommand::Uninstall) => Some((
            CommandName::DaemonUninstall,
            "local OS service".to_string(),
            vec!["uninstall daemon service files".to_string()],
            "service-mutation".to_string(),
        )),
        _ => None,
    }
}

fn command_name_for_command(command: &Command) -> CommandName {
    match command {
        Command::Help(_) => CommandName::Help,
        Command::Version => CommandName::Version,
        Command::Contract => CommandName::Contract,
        Command::Login(_) => CommandName::Login,
        Command::Approve(_) => CommandName::Approve,
        Command::Revoke(_) => CommandName::Revoke,
        Command::Init(_) => CommandName::Init,
        Command::Prewarm(_) => CommandName::Prewarm,
        Command::Setup(_) => CommandName::Setup,
        Command::Status(_) => CommandName::Status,
        Command::Actions(_) => CommandName::Actions,
        Command::Tui(_) => CommandName::Tui,
        Command::Search(_) => CommandName::Search,
        Command::Symbols(_) => CommandName::Symbols,
        Command::Explain(_) => CommandName::Explain,
        Command::Devices(_) => CommandName::Devices,
        Command::Recovery(_) => CommandName::Recover,
        Command::Resolve(_) => CommandName::Resolve,
        Command::Events(_) => CommandName::Events,
        Command::Workon(_) => CommandName::Workon,
        Command::Work(_) => CommandName::Work,
        Command::WorkDiff(_) => CommandName::Diff,
        Command::Review(_) => CommandName::Review,
        Command::WorkAccept(_) => CommandName::Accept,
        Command::WorkDiscard(_) => CommandName::Discard,
        Command::WorkRestore(_) => CommandName::Restore,
        Command::WorkCleanup(_) => CommandName::Cleanup,
        Command::AgentLeaseCreate(_) => CommandName::AgentStart,
        Command::AgentContext(_) => CommandName::AgentContext,
        Command::AgentPrompt(_) => CommandName::AgentPrompt,
        Command::AgentPublish(_) => CommandName::AgentPublish,
        Command::AgentComplete(_) => CommandName::AgentComplete,
        Command::AgentBudget(_) => CommandName::AgentBudget,
        Command::BootstrapSsh(_) => CommandName::Connect,
        Command::Daemon(DaemonCommand::Start) => CommandName::DaemonStart,
        Command::Daemon(DaemonCommand::Stop) => CommandName::DaemonStop,
        Command::Daemon(DaemonCommand::Status) => CommandName::DaemonStatus,
        Command::Daemon(DaemonCommand::Install) => CommandName::DaemonInstall,
        Command::Daemon(DaemonCommand::Restart) => CommandName::DaemonRestart,
        Command::Daemon(DaemonCommand::Uninstall) => CommandName::DaemonUninstall,
        Command::DiagnosticsCollect => CommandName::DiagnosticsCollect,
        Command::UsageError { command, .. } => *command,
        Command::DevCloudSpike(_) | Command::CommandUsageError(_) | Command::Unknown(_) => {
            CommandName::Unknown
        }
    }
}

fn command_args_for_replay(command: &Command) -> Option<Vec<String>> {
    match command {
        Command::Approve(args) => {
            let mut argv = vec!["approve".to_string()];
            if let Some(request_id) = &args.request_id {
                argv.push(request_id.clone());
            }
            if args.yes {
                argv.push("--yes".to_string());
            }
            Some(argv)
        }
        Command::Revoke(args) => Some(vec!["revoke".to_string(), args.device_id.clone()]),
        Command::Recovery(recovery::RecoveryArgs::Create) => {
            Some(vec!["recover".to_string(), "create".to_string()])
        }
        Command::Recovery(recovery::RecoveryArgs::Rotate) => {
            Some(vec!["recover".to_string(), "rotate".to_string()])
        }
        Command::Recovery(recovery::RecoveryArgs::Revoke { envelope_id }) => Some(vec![
            "recover".to_string(),
            "revoke".to_string(),
            envelope_id.clone(),
        ]),
        Command::BootstrapSsh(args) => {
            let mut argv = vec![
                "connect".to_string(),
                args.host.clone(),
                "--root".to_string(),
                args.root.clone(),
            ];
            if let Some(artifact) = &args.artifact {
                argv.extend(["--binary".to_string(), artifact.clone()]);
            }
            if let Some(project) = &args.project {
                argv.extend(["--project".to_string(), project.clone()]);
            }
            if let Some(task) = &args.task {
                argv.extend(["--task".to_string(), task.clone()]);
            }
            if let Some(agent) = &args.agent {
                argv.extend(["--agent".to_string(), agent.clone()]);
            }
            Some(argv)
        }
        Command::Workon(args) => Some(vec![
            "workon".to_string(),
            args.project_path.clone(),
            args.name.clone(),
        ]),
        Command::WorkAccept(args) => Some(vec!["accept".to_string(), args.selector.clone()]),
        Command::WorkDiscard(args) => Some(vec!["discard".to_string(), args.selector.clone()]),
        Command::WorkRestore(args) => Some(vec!["restore".to_string(), args.selector.clone()]),
        Command::WorkCleanup(args) => {
            let mut argv = vec!["cleanup".to_string()];
            if args.apply {
                argv.push("--apply".to_string());
            }
            Some(argv)
        }
        Command::AgentLeaseCreate(args) => {
            let mut argv = vec![
                "agent".to_string(),
                "start".to_string(),
                args.project_path.clone(),
                "--task".to_string(),
                args.task.clone(),
                "--base".to_string(),
                agent_base_token(args.base).to_string(),
                "--hydrate-budget".to_string(),
                args.hydrate_budget_bytes.to_string(),
            ];
            if args.work_view {
                argv.push("--work-view".to_string());
            }
            Some(argv)
        }
        Command::AgentPublish(args) => Some(vec![
            "agent".to_string(),
            "publish".to_string(),
            "--lease".to_string(),
            args.lease_id.clone(),
        ]),
        Command::AgentComplete(args) => Some(vec![
            "agent".to_string(),
            "complete".to_string(),
            "--lease".to_string(),
            args.lease_id.clone(),
        ]),
        Command::AgentBudget(args) => Some(vec![
            "agent".to_string(),
            "budget".to_string(),
            "--lease".to_string(),
            args.lease_id.clone(),
            "--add".to_string(),
            args.add_bytes.to_string(),
        ]),
        Command::Daemon(DaemonCommand::Install) => {
            Some(vec!["daemon".to_string(), "install".to_string()])
        }
        Command::Daemon(DaemonCommand::Restart) => {
            Some(vec!["daemon".to_string(), "restart".to_string()])
        }
        Command::Daemon(DaemonCommand::Uninstall) => {
            Some(vec!["daemon".to_string(), "uninstall".to_string()])
        }
        _ => None,
    }
}

fn agent_base_token(base: bowline_core::commands::AgentLeaseBase) -> &'static str {
    match base {
        bowline_core::commands::AgentLeaseBase::LatestWorkspace => "latest-workspace",
        bowline_core::commands::AgentLeaseBase::LatestMain => "latest:main",
    }
}

fn shell_join(args: impl IntoIterator<Item = String>) -> String {
    args.into_iter()
        .map(|arg| shell_escape(&arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_escape(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_./:=@".contains(character))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn parse_dev_cloud_spike_command(args: &[String]) -> Command {
    let mut provider = CloudSpikeProvider::Fake;
    let mut index = 0_usize;

    while index < args.len() {
        match args[index].as_str() {
            "--provider" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(CommandName::Unknown, "missing value for --provider");
                };
                provider = match value.as_str() {
                    "fake" => CloudSpikeProvider::Fake,
                    "hosted" => CloudSpikeProvider::Hosted,
                    _ => {
                        return usage_error(
                            CommandName::Unknown,
                            "expected --provider fake or --provider hosted",
                        );
                    }
                };
                index += 2;
            }
            flag if flag.starts_with("--") => {
                return usage_error(
                    CommandName::Unknown,
                    format!("unknown bowline dev cloud-spike option `{flag}`"),
                );
            }
            value => {
                return usage_error(
                    CommandName::Unknown,
                    format!("unexpected bowline dev cloud-spike argument `{value}`"),
                );
            }
        }
    }

    Command::DevCloudSpike(CloudSpikeArgs { provider })
}

#[derive(Clone, Copy)]
struct CommandSpec {
    group: &'static str,
    name: &'static str,
    aliases: &'static [&'static str],
    summary: &'static str,
    usage: &'static str,
    options: &'static [OptionSpec],
    examples: &'static [ExampleSpec],
    json_output_type: &'static str,
    side_effect_level: &'static str,
    supports_json: bool,
    supports_dry_run: bool,
    supports_idempotency_key: bool,
    bounded_output: Option<BoundedSpec>,
    related_commands: &'static [&'static str],
}

#[derive(Clone, Copy)]
struct OptionSpec {
    name: &'static str,
    value_name: Option<&'static str>,
    summary: &'static str,
    required: bool,
    repeatable: bool,
}

#[derive(Clone, Copy)]
struct ExampleSpec {
    command: &'static str,
    summary: &'static str,
}

#[derive(Clone, Copy)]
struct BoundedSpec {
    default_limit: u16,
    max_limit: u16,
    cursor_format: &'static str,
    path_prefix: bool,
}

const GLOBAL_JSON_OPTION: OptionSpec = OptionSpec {
    name: "--json",
    value_name: None,
    summary: "Return the command contract JSON on stdout.",
    required: false,
    repeatable: false,
};
const DRY_RUN_OPTION: OptionSpec = OptionSpec {
    name: "--dry-run",
    value_name: None,
    summary: "Preview the mutation without changing local or daemon state.",
    required: false,
    repeatable: false,
};
const IDEMPOTENCY_OPTION: OptionSpec = OptionSpec {
    name: "--idempotency-key",
    value_name: Some("key"),
    summary: "Replay-safe key for a non-dry-run mutation.",
    required: false,
    repeatable: false,
};
const RECOVERY_IDEMPOTENCY_OPTION: OptionSpec = OptionSpec {
    name: "--idempotency-key",
    value_name: Some("key"),
    summary: "Replay-safe key for recover create, rotate, and revoke; recover verify and use read stdin and reject it.",
    required: false,
    repeatable: false,
};
const SEARCH_BOUND: BoundedSpec = BoundedSpec {
    default_limit: 20,
    max_limit: 100,
    cursor_format: "v1:<offset>",
    path_prefix: true,
};

const EMPTY_OPTIONS: &[OptionSpec] = &[];
const EMPTY_EXAMPLES: &[ExampleSpec] = &[];

const COMMAND_REGISTRY: &[CommandSpec] = &[
    CommandSpec {
        group: "Discovery",
        name: "help",
        aliases: &[],
        summary: "Show human or JSON help for every CLI command.",
        usage: "bowline help [topic] [--json]",
        options: &[GLOBAL_JSON_OPTION],
        examples: &[ExampleSpec {
            command: "bowline help agent start --json",
            summary: "Inspect nested help for agent lease creation.",
        }],
        json_output_type: "HelpCommandOutput",
        side_effect_level: "none",
        supports_json: true,
        supports_dry_run: false,
        supports_idempotency_key: false,
        bounded_output: None,
        related_commands: &["contract", "version"],
    },
    CommandSpec {
        group: "Discovery",
        name: "version",
        aliases: &["--version"],
        summary: "Print CLI version and protocol compatibility.",
        usage: "bowline version [--json]",
        options: &[GLOBAL_JSON_OPTION],
        examples: EMPTY_EXAMPLES,
        json_output_type: "VersionCommandOutput",
        side_effect_level: "none",
        supports_json: true,
        supports_dry_run: false,
        supports_idempotency_key: false,
        bounded_output: None,
        related_commands: &["contract"],
    },
    CommandSpec {
        group: "Discovery",
        name: "contract",
        aliases: &["schema"],
        summary: "Print the machine-readable CLI command and output contract.",
        usage: "bowline contract --json",
        options: &[GLOBAL_JSON_OPTION],
        examples: &[ExampleSpec {
            command: "bowline schema --json",
            summary: "Alias used by agents that search for schema discovery.",
        }],
        json_output_type: "ContractCommandOutput",
        side_effect_level: "none",
        supports_json: true,
        supports_dry_run: false,
        supports_idempotency_key: false,
        bounded_output: None,
        related_commands: &["help", "version"],
    },
    CommandSpec {
        group: "Workspace",
        name: "login",
        aliases: &[],
        summary: "Log in and prepare the local workspace root.",
        usage: "bowline login [--root <path>] [--headless] [--no-poll]",
        options: &[
            OptionSpec {
                name: "--root",
                value_name: Some("path"),
                summary: "Choose the workspace root.",
                required: false,
                repeatable: false,
            },
            GLOBAL_JSON_OPTION,
        ],
        examples: EMPTY_EXAMPLES,
        json_output_type: "LoginCommandOutput",
        side_effect_level: "mutation",
        supports_json: true,
        supports_dry_run: false,
        supports_idempotency_key: false,
        bounded_output: None,
        related_commands: &["status"],
    },
    CommandSpec {
        group: "Workspace",
        name: "status",
        aliases: &[],
        summary: "Inspect workspace, index, sync, and safe next actions.",
        usage: "bowline status [path] [--watch] [--workspace|--all] [--json]",
        options: &[
            OptionSpec {
                name: "--watch",
                value_name: None,
                summary: "Stream status frames.",
                required: false,
                repeatable: false,
            },
            OptionSpec {
                name: "--workspace",
                value_name: None,
                summary: "Inspect the whole workspace.",
                required: false,
                repeatable: false,
            },
            GLOBAL_JSON_OPTION,
        ],
        examples: &[ExampleSpec {
            command: "bowline status --json",
            summary: "Parse the current workspace status.",
        }],
        json_output_type: "StatusCommandOutput",
        side_effect_level: "read",
        supports_json: true,
        supports_dry_run: false,
        supports_idempotency_key: false,
        bounded_output: None,
        related_commands: &["actions", "events", "diagnostics collect"],
    },
    CommandSpec {
        group: "Workspace",
        name: "init",
        aliases: &[],
        summary: "Initialize or observe a workspace root.",
        usage: "bowline init [path] [--json]",
        options: &[GLOBAL_JSON_OPTION],
        examples: &[ExampleSpec {
            command: "bowline init --json",
            summary: "Create or inspect the default workspace root.",
        }],
        json_output_type: "InitCommandOutput",
        side_effect_level: "mutation",
        supports_json: true,
        supports_dry_run: false,
        supports_idempotency_key: false,
        bounded_output: None,
        related_commands: &["login", "status", "setup"],
    },
    CommandSpec {
        group: "Workspace",
        name: "setup",
        aliases: &[],
        summary: "Prepare a project and report setup receipts.",
        usage: "bowline setup [path] [--yes] [--json]",
        options: &[
            OptionSpec {
                name: "--yes",
                value_name: None,
                summary: "Approve required setup changes.",
                required: false,
                repeatable: false,
            },
            GLOBAL_JSON_OPTION,
        ],
        examples: EMPTY_EXAMPLES,
        json_output_type: "PrewarmCommandOutput",
        side_effect_level: "mutation",
        supports_json: true,
        supports_dry_run: false,
        supports_idempotency_key: false,
        bounded_output: None,
        related_commands: &["status"],
    },
    CommandSpec {
        group: "Workspace",
        name: "prewarm",
        aliases: &[],
        summary: "Hydrate and prepare a project path.",
        usage: "bowline prewarm <project> [--approve-setup] [--json]",
        options: &[
            OptionSpec {
                name: "--approve-setup",
                value_name: None,
                summary: "Allow setup receipts when needed.",
                required: false,
                repeatable: false,
            },
            GLOBAL_JSON_OPTION,
        ],
        examples: EMPTY_EXAMPLES,
        json_output_type: "PrewarmCommandOutput",
        side_effect_level: "mutation",
        supports_json: true,
        supports_dry_run: false,
        supports_idempotency_key: false,
        bounded_output: None,
        related_commands: &["setup"],
    },
    CommandSpec {
        group: "Workspace",
        name: "actions",
        aliases: &[],
        summary: "Return safe next actions for the current status.",
        usage: "bowline actions [path] [--workspace] [--json]",
        options: &[GLOBAL_JSON_OPTION],
        examples: EMPTY_EXAMPLES,
        json_output_type: "ActionsCommandOutput",
        side_effect_level: "read",
        supports_json: true,
        supports_dry_run: false,
        supports_idempotency_key: false,
        bounded_output: None,
        related_commands: &["status"],
    },
    CommandSpec {
        group: "Workspace",
        name: "search",
        aliases: &[],
        summary: "Search indexed project text with bounded, cursored results.",
        usage: "bowline search <query> [path] [--limit <n>] [--cursor <cursor>] [--path-prefix <prefix>] [--json]",
        options: &[
            OptionSpec {
                name: "--limit",
                value_name: Some("n"),
                summary: "Maximum results, default 20, max 100.",
                required: false,
                repeatable: false,
            },
            OptionSpec {
                name: "--cursor",
                value_name: Some("cursor"),
                summary: "Opaque cursor from nextCursor.",
                required: false,
                repeatable: false,
            },
            OptionSpec {
                name: "--path-prefix",
                value_name: Some("prefix"),
                summary: "Restrict matches to a path prefix.",
                required: false,
                repeatable: false,
            },
            GLOBAL_JSON_OPTION,
        ],
        examples: EMPTY_EXAMPLES,
        json_output_type: "SearchCommandOutput",
        side_effect_level: "read",
        supports_json: true,
        supports_dry_run: false,
        supports_idempotency_key: false,
        bounded_output: Some(SEARCH_BOUND),
        related_commands: &["symbols"],
    },
    CommandSpec {
        group: "Workspace",
        name: "symbols",
        aliases: &[],
        summary: "Look up indexed code symbols with bounded, cursored results.",
        usage: "bowline symbols <name> [path] [--limit <n>] [--cursor <cursor>] [--path-prefix <prefix>] [--json]",
        options: &[
            OptionSpec {
                name: "--limit",
                value_name: Some("n"),
                summary: "Maximum symbols, default 20, max 100.",
                required: false,
                repeatable: false,
            },
            OptionSpec {
                name: "--cursor",
                value_name: Some("cursor"),
                summary: "Opaque cursor from nextCursor.",
                required: false,
                repeatable: false,
            },
            OptionSpec {
                name: "--path-prefix",
                value_name: Some("prefix"),
                summary: "Restrict matches to a path prefix.",
                required: false,
                repeatable: false,
            },
            GLOBAL_JSON_OPTION,
        ],
        examples: EMPTY_EXAMPLES,
        json_output_type: "SymbolCommandOutput",
        side_effect_level: "read",
        supports_json: true,
        supports_dry_run: false,
        supports_idempotency_key: false,
        bounded_output: Some(SEARCH_BOUND),
        related_commands: &["search"],
    },
    CommandSpec {
        group: "Workspace",
        name: "explain",
        aliases: &[],
        summary: "Explain path classification, access, and hydration behavior.",
        usage: "bowline explain <path> [--json]",
        options: &[GLOBAL_JSON_OPTION],
        examples: EMPTY_EXAMPLES,
        json_output_type: "ExplainCommandOutput",
        side_effect_level: "read",
        supports_json: true,
        supports_dry_run: false,
        supports_idempotency_key: false,
        bounded_output: None,
        related_commands: &["status"],
    },
    CommandSpec {
        group: "Workspace",
        name: "events",
        aliases: &[],
        summary: "List recent workspace events.",
        usage: "bowline events [path] [--workspace] [--limit <n>] [--json]",
        options: &[GLOBAL_JSON_OPTION],
        examples: EMPTY_EXAMPLES,
        json_output_type: "EventsCommandOutput",
        side_effect_level: "read",
        supports_json: true,
        supports_dry_run: false,
        supports_idempotency_key: false,
        bounded_output: None,
        related_commands: &["status"],
    },
    CommandSpec {
        group: "Workspace",
        name: "resolve",
        aliases: &[],
        summary: "Resolve conflicts or produce agent-ready conflict context.",
        usage: "bowline resolve [path] [--tui|--copy-prompt|--diff <conflict>] [--json]",
        options: &[GLOBAL_JSON_OPTION],
        examples: EMPTY_EXAMPLES,
        json_output_type: "ResolveCommandOutput",
        side_effect_level: "read",
        supports_json: true,
        supports_dry_run: false,
        supports_idempotency_key: false,
        bounded_output: None,
        related_commands: &["status"],
    },
    CommandSpec {
        group: "Workspace",
        name: "tui",
        aliases: &[],
        summary: "Open the terminal workspace UI.",
        usage: "bowline tui [path]",
        options: EMPTY_OPTIONS,
        examples: EMPTY_EXAMPLES,
        json_output_type: "none",
        side_effect_level: "interactive",
        supports_json: false,
        supports_dry_run: false,
        supports_idempotency_key: false,
        bounded_output: None,
        related_commands: &["status"],
    },
    CommandSpec {
        group: "Trust",
        name: "approve",
        aliases: &[],
        summary: "Approve a pending device trust request.",
        usage: "bowline approve [request] [--yes] [--json] [--dry-run] [--idempotency-key <key>]",
        options: &[
            OptionSpec {
                name: "--yes",
                value_name: None,
                summary: "Approve without an interactive prompt.",
                required: false,
                repeatable: false,
            },
            GLOBAL_JSON_OPTION,
            DRY_RUN_OPTION,
            IDEMPOTENCY_OPTION,
        ],
        examples: EMPTY_EXAMPLES,
        json_output_type: "DevicesCommandOutput",
        side_effect_level: "mutation",
        supports_json: true,
        supports_dry_run: true,
        supports_idempotency_key: true,
        bounded_output: None,
        related_commands: &["revoke", "devices"],
    },
    CommandSpec {
        group: "Trust",
        name: "revoke",
        aliases: &[],
        summary: "Revoke a trusted device.",
        usage: "bowline revoke <device> [--json] [--dry-run] [--idempotency-key <key>]",
        options: &[GLOBAL_JSON_OPTION, DRY_RUN_OPTION, IDEMPOTENCY_OPTION],
        examples: EMPTY_EXAMPLES,
        json_output_type: "DevicesCommandOutput",
        side_effect_level: "mutation",
        supports_json: true,
        supports_dry_run: true,
        supports_idempotency_key: true,
        bounded_output: None,
        related_commands: &["approve", "devices"],
    },
    CommandSpec {
        group: "Trust",
        name: "recover",
        aliases: &["recovery"],
        summary: "Manage recovery keys and recovery-based device grants.",
        usage: "bowline recover [status|create|verify <id>|rotate|revoke <id>|use <id>] [--json] [--dry-run] [--idempotency-key <key>]",
        options: &[
            GLOBAL_JSON_OPTION,
            DRY_RUN_OPTION,
            RECOVERY_IDEMPOTENCY_OPTION,
        ],
        examples: EMPTY_EXAMPLES,
        json_output_type: "RecoveryCommandOutput",
        side_effect_level: "mixed",
        supports_json: true,
        supports_dry_run: true,
        supports_idempotency_key: true,
        bounded_output: None,
        related_commands: &["approve"],
    },
    CommandSpec {
        group: "Trust",
        name: "devices",
        aliases: &[],
        summary: "List or manage local device records.",
        usage: "bowline devices [list|request|approve|accept|deny|revoke] [--json]",
        options: &[GLOBAL_JSON_OPTION],
        examples: EMPTY_EXAMPLES,
        json_output_type: "DevicesCommandOutput",
        side_effect_level: "mixed",
        supports_json: true,
        supports_dry_run: false,
        supports_idempotency_key: false,
        bounded_output: None,
        related_commands: &["approve", "revoke"],
    },
    CommandSpec {
        group: "Trust",
        name: "connect",
        aliases: &["bootstrap ssh"],
        summary: "Install bowline on a remote host and optionally hand off to an agent.",
        usage: "bowline connect <host> [--root <path>] [--project <project> --task <task> --agent <agent>] [--json] [--dry-run] [--idempotency-key <key>]",
        options: &[GLOBAL_JSON_OPTION, DRY_RUN_OPTION, IDEMPOTENCY_OPTION],
        examples: EMPTY_EXAMPLES,
        json_output_type: "BootstrapSshCommandOutput",
        side_effect_level: "remote-mutation",
        supports_json: true,
        supports_dry_run: true,
        supports_idempotency_key: true,
        bounded_output: None,
        related_commands: &["agent start"],
    },
    CommandSpec {
        group: "Work",
        name: "workon",
        aliases: &[],
        summary: "Create or reuse a named work view for a project.",
        usage: "bowline workon [project] <name> [--json] [--dry-run] [--idempotency-key <key>]",
        options: &[GLOBAL_JSON_OPTION, DRY_RUN_OPTION, IDEMPOTENCY_OPTION],
        examples: EMPTY_EXAMPLES,
        json_output_type: "WorkonCommandOutput",
        side_effect_level: "mutation",
        supports_json: true,
        supports_dry_run: true,
        supports_idempotency_key: true,
        bounded_output: None,
        related_commands: &["review", "accept", "discard"],
    },
    CommandSpec {
        group: "Work",
        name: "work",
        aliases: &[],
        summary: "List work views.",
        usage: "bowline work [--include-hidden] [--json]",
        options: &[GLOBAL_JSON_OPTION],
        examples: EMPTY_EXAMPLES,
        json_output_type: "WorkListCommandOutput",
        side_effect_level: "read",
        supports_json: true,
        supports_dry_run: false,
        supports_idempotency_key: false,
        bounded_output: None,
        related_commands: &["workon"],
    },
    CommandSpec {
        group: "Work",
        name: "diff",
        aliases: &[],
        summary: "Show the diff for a work view.",
        usage: "bowline diff <target> [--json]",
        options: &[GLOBAL_JSON_OPTION],
        examples: EMPTY_EXAMPLES,
        json_output_type: "WorkDiffCommandOutput",
        side_effect_level: "read",
        supports_json: true,
        supports_dry_run: false,
        supports_idempotency_key: false,
        bounded_output: None,
        related_commands: &["review"],
    },
    CommandSpec {
        group: "Work",
        name: "review",
        aliases: &[],
        summary: "Preview whether a work view is ready to accept.",
        usage: "bowline review <target> [--json]",
        options: &[GLOBAL_JSON_OPTION],
        examples: EMPTY_EXAMPLES,
        json_output_type: "WorkDiffCommandOutput",
        side_effect_level: "read",
        supports_json: true,
        supports_dry_run: false,
        supports_idempotency_key: false,
        bounded_output: None,
        related_commands: &["accept"],
    },
    CommandSpec {
        group: "Work",
        name: "accept",
        aliases: &[],
        summary: "Apply a review-ready work view.",
        usage: "bowline accept <target> [--json] [--dry-run] [--idempotency-key <key>]",
        options: &[GLOBAL_JSON_OPTION, DRY_RUN_OPTION, IDEMPOTENCY_OPTION],
        examples: EMPTY_EXAMPLES,
        json_output_type: "WorkLifecycleCommandOutput",
        side_effect_level: "mutation",
        supports_json: true,
        supports_dry_run: true,
        supports_idempotency_key: true,
        bounded_output: None,
        related_commands: &["review"],
    },
    CommandSpec {
        group: "Work",
        name: "discard",
        aliases: &[],
        summary: "Discard a work view.",
        usage: "bowline discard <target> [--json] [--dry-run] [--idempotency-key <key>]",
        options: &[GLOBAL_JSON_OPTION, DRY_RUN_OPTION, IDEMPOTENCY_OPTION],
        examples: EMPTY_EXAMPLES,
        json_output_type: "WorkLifecycleCommandOutput",
        side_effect_level: "mutation",
        supports_json: true,
        supports_dry_run: true,
        supports_idempotency_key: true,
        bounded_output: None,
        related_commands: &["work"],
    },
    CommandSpec {
        group: "Work",
        name: "restore",
        aliases: &[],
        summary: "Restore a discarded work view.",
        usage: "bowline restore <target> [--json] [--dry-run] [--idempotency-key <key>]",
        options: &[GLOBAL_JSON_OPTION, DRY_RUN_OPTION, IDEMPOTENCY_OPTION],
        examples: EMPTY_EXAMPLES,
        json_output_type: "WorkLifecycleCommandOutput",
        side_effect_level: "mutation",
        supports_json: true,
        supports_dry_run: true,
        supports_idempotency_key: true,
        bounded_output: None,
        related_commands: &["work"],
    },
    CommandSpec {
        group: "Work",
        name: "cleanup",
        aliases: &[],
        summary: "Preview or apply cleanup for retained work views.",
        usage: "bowline cleanup [--apply] [--json] [--dry-run] [--idempotency-key <key>]",
        options: &[
            OptionSpec {
                name: "--apply",
                value_name: None,
                summary: "Apply cleanup instead of previewing it.",
                required: false,
                repeatable: false,
            },
            GLOBAL_JSON_OPTION,
            DRY_RUN_OPTION,
            IDEMPOTENCY_OPTION,
        ],
        examples: EMPTY_EXAMPLES,
        json_output_type: "WorkCleanupCommandOutput",
        side_effect_level: "conditional-mutation",
        supports_json: true,
        supports_dry_run: true,
        supports_idempotency_key: true,
        bounded_output: None,
        related_commands: &["work"],
    },
    CommandSpec {
        group: "Agent",
        name: "agent start",
        aliases: &["agent lease create"],
        summary: "Create an agent lease and return context/prompt commands.",
        usage: "bowline agent start [project] --task <task> [--base <base>] [--hydrate-budget <bytes>] [--work-view] [--json] [--dry-run] [--idempotency-key <key>]",
        options: &[
            OptionSpec {
                name: "--task",
                value_name: Some("task"),
                summary: "Task description for the lease.",
                required: true,
                repeatable: false,
            },
            OptionSpec {
                name: "--base",
                value_name: Some("base"),
                summary: "Lease base, such as latest-workspace or latest:main.",
                required: false,
                repeatable: false,
            },
            OptionSpec {
                name: "--hydrate-budget",
                value_name: Some("bytes"),
                summary: "Hydration budget reserved for the lease.",
                required: false,
                repeatable: false,
            },
            OptionSpec {
                name: "--work-view",
                value_name: None,
                summary: "Write through an isolated work view.",
                required: false,
                repeatable: false,
            },
            GLOBAL_JSON_OPTION,
            DRY_RUN_OPTION,
            IDEMPOTENCY_OPTION,
        ],
        examples: EMPTY_EXAMPLES,
        json_output_type: "AgentLeaseCreateCommandOutput",
        side_effect_level: "mutation",
        supports_json: true,
        supports_dry_run: true,
        supports_idempotency_key: true,
        bounded_output: None,
        related_commands: &["agent context", "agent prompt"],
    },
    CommandSpec {
        group: "Agent",
        name: "agent context",
        aliases: &[],
        summary: "Return full context for an agent lease.",
        usage: "bowline agent context --lease <id> [--json]",
        options: &[GLOBAL_JSON_OPTION],
        examples: EMPTY_EXAMPLES,
        json_output_type: "AgentContextCommandOutput",
        side_effect_level: "read",
        supports_json: true,
        supports_dry_run: false,
        supports_idempotency_key: false,
        bounded_output: None,
        related_commands: &["agent prompt"],
    },
    CommandSpec {
        group: "Agent",
        name: "agent prompt",
        aliases: &[],
        summary: "Return launch prompt text for an agent lease.",
        usage: "bowline agent prompt --lease <id> [--json]",
        options: &[GLOBAL_JSON_OPTION],
        examples: EMPTY_EXAMPLES,
        json_output_type: "AgentPromptCommandOutput",
        side_effect_level: "read",
        supports_json: true,
        supports_dry_run: false,
        supports_idempotency_key: false,
        bounded_output: None,
        related_commands: &["agent context"],
    },
    CommandSpec {
        group: "Agent",
        name: "agent publish",
        aliases: &[],
        summary: "Publish agent output for review.",
        usage: "bowline agent publish --lease <id> [--json] [--dry-run] [--idempotency-key <key>]",
        options: &[GLOBAL_JSON_OPTION, DRY_RUN_OPTION, IDEMPOTENCY_OPTION],
        examples: EMPTY_EXAMPLES,
        json_output_type: "AgentToolResult",
        side_effect_level: "mutation",
        supports_json: true,
        supports_dry_run: true,
        supports_idempotency_key: true,
        bounded_output: None,
        related_commands: &["agent complete"],
    },
    CommandSpec {
        group: "Agent",
        name: "agent complete",
        aliases: &[],
        summary: "Mark an agent lease complete.",
        usage: "bowline agent complete --lease <id> [--json] [--dry-run] [--idempotency-key <key>]",
        options: &[GLOBAL_JSON_OPTION, DRY_RUN_OPTION, IDEMPOTENCY_OPTION],
        examples: EMPTY_EXAMPLES,
        json_output_type: "AgentToolResult",
        side_effect_level: "mutation",
        supports_json: true,
        supports_dry_run: true,
        supports_idempotency_key: true,
        bounded_output: None,
        related_commands: &["agent publish"],
    },
    CommandSpec {
        group: "Agent",
        name: "agent budget",
        aliases: &[],
        summary: "Grant additional hydration budget to an agent lease.",
        usage: "bowline agent budget --lease <id> --add <bytes> [--json] [--dry-run] [--idempotency-key <key>]",
        options: &[GLOBAL_JSON_OPTION, DRY_RUN_OPTION, IDEMPOTENCY_OPTION],
        examples: EMPTY_EXAMPLES,
        json_output_type: "AgentBudgetCommandOutput",
        side_effect_level: "mutation",
        supports_json: true,
        supports_dry_run: true,
        supports_idempotency_key: true,
        bounded_output: None,
        related_commands: &["agent context"],
    },
    CommandSpec {
        group: "Daemon",
        name: "daemon start",
        aliases: &[],
        summary: "Start the local daemon process.",
        usage: "bowline daemon start [--json]",
        options: &[GLOBAL_JSON_OPTION],
        examples: EMPTY_EXAMPLES,
        json_output_type: "DaemonCommandOutput",
        side_effect_level: "daemon-mutation",
        supports_json: true,
        supports_dry_run: false,
        supports_idempotency_key: false,
        bounded_output: None,
        related_commands: &["daemon status"],
    },
    CommandSpec {
        group: "Daemon",
        name: "daemon stop",
        aliases: &[],
        summary: "Stop the local daemon process.",
        usage: "bowline daemon stop [--json]",
        options: &[GLOBAL_JSON_OPTION],
        examples: EMPTY_EXAMPLES,
        json_output_type: "DaemonCommandOutput",
        side_effect_level: "daemon-mutation",
        supports_json: true,
        supports_dry_run: false,
        supports_idempotency_key: false,
        bounded_output: None,
        related_commands: &["daemon status"],
    },
    CommandSpec {
        group: "Daemon",
        name: "daemon status",
        aliases: &[],
        summary: "Inspect daemon process and service state.",
        usage: "bowline daemon status [--json]",
        options: &[GLOBAL_JSON_OPTION],
        examples: EMPTY_EXAMPLES,
        json_output_type: "DaemonStatusOutput",
        side_effect_level: "read",
        supports_json: true,
        supports_dry_run: false,
        supports_idempotency_key: false,
        bounded_output: None,
        related_commands: &["daemon start", "daemon install"],
    },
    CommandSpec {
        group: "Daemon",
        name: "daemon install",
        aliases: &[],
        summary: "Install or update the OS service for the daemon.",
        usage: "bowline daemon install [--json] [--dry-run] [--idempotency-key <key>]",
        options: &[GLOBAL_JSON_OPTION, DRY_RUN_OPTION, IDEMPOTENCY_OPTION],
        examples: EMPTY_EXAMPLES,
        json_output_type: "DaemonServiceOutput",
        side_effect_level: "service-mutation",
        supports_json: true,
        supports_dry_run: true,
        supports_idempotency_key: true,
        bounded_output: None,
        related_commands: &["daemon restart", "daemon uninstall"],
    },
    CommandSpec {
        group: "Daemon",
        name: "daemon restart",
        aliases: &[],
        summary: "Restart the installed daemon service.",
        usage: "bowline daemon restart [--json] [--dry-run] [--idempotency-key <key>]",
        options: &[GLOBAL_JSON_OPTION, DRY_RUN_OPTION, IDEMPOTENCY_OPTION],
        examples: EMPTY_EXAMPLES,
        json_output_type: "DaemonServiceOutput",
        side_effect_level: "service-mutation",
        supports_json: true,
        supports_dry_run: true,
        supports_idempotency_key: true,
        bounded_output: None,
        related_commands: &["daemon install"],
    },
    CommandSpec {
        group: "Daemon",
        name: "daemon uninstall",
        aliases: &[],
        summary: "Uninstall the daemon OS service.",
        usage: "bowline daemon uninstall [--json] [--dry-run] [--idempotency-key <key>]",
        options: &[GLOBAL_JSON_OPTION, DRY_RUN_OPTION, IDEMPOTENCY_OPTION],
        examples: EMPTY_EXAMPLES,
        json_output_type: "DaemonServiceOutput",
        side_effect_level: "service-mutation",
        supports_json: true,
        supports_dry_run: true,
        supports_idempotency_key: true,
        bounded_output: None,
        related_commands: &["daemon install"],
    },
    CommandSpec {
        group: "Support",
        name: "diagnostics collect",
        aliases: &[],
        summary: "Print a redacted diagnostics bundle.",
        usage: "bowline diagnostics collect [--json]",
        options: &[GLOBAL_JSON_OPTION],
        examples: EMPTY_EXAMPLES,
        json_output_type: "DiagnosticsCollectCommandOutput",
        side_effect_level: "read",
        supports_json: true,
        supports_dry_run: false,
        supports_idempotency_key: false,
        bounded_output: None,
        related_commands: &["status"],
    },
];

fn print_help(topic: Option<&[String]>, json: bool) {
    let topic_name = topic.map(|parts| parts.join(" "));
    let commands = command_descriptors_for_topic(topic_name.as_deref());
    if json {
        print_json(&HelpCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::Help,
            generated_at: generated_at(),
            topic: topic_name,
            groups: command_groups_for_descriptors(&commands),
            commands,
        });
        return;
    }

    if let Some(topic_name) = topic_name.as_deref() {
        if commands.is_empty() {
            eprintln!("bowline help: no topic named `{topic_name}`");
            return;
        }
        for descriptor in commands {
            println!("{}", render_command_help(&descriptor));
        }
        return;
    }

    println!("bowline command shell\n");
    for group in command_groups() {
        println!("{}:", group.name);
        for command in group.commands {
            if let Some(spec) = COMMAND_REGISTRY.iter().find(|spec| spec.name == command) {
                println!("  {}", spec.usage);
            }
        }
        println!();
    }
    println!(
        "Global options:\n  --json\n  --socket <path>\n  --dry-run\n  --idempotency-key <key>"
    );
}

fn print_version(json: bool) {
    if json {
        print_json(&VersionCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::Version,
            generated_at: generated_at(),
            cli_version: CLI_VERSION.to_string(),
            protocol: PROTOCOL.to_string(),
            protocol_version: PROTOCOL_VERSION,
            default_socket: DEFAULT_SOCKET.to_string(),
            package: "bowline".to_string(),
        });
        return;
    }
    println!("bowline {CLI_VERSION}");
}

fn print_contract(json: bool) {
    let output = ContractCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Contract,
        generated_at: generated_at(),
        cli_version: CLI_VERSION.to_string(),
        protocol: PROTOCOL.to_string(),
        protocol_version: PROTOCOL_VERSION,
        event_schema_version: EVENT_SCHEMA_VERSION,
        package: "bowline".to_string(),
        package_contract_source: PACKAGE_CONTRACT_SOURCE.to_string(),
        command_output_types: command_output_types(),
        commands: command_descriptors(),
        fixtures: contract_fixtures(),
    };
    if json {
        print_json(&output);
        return;
    }
    println!(
        "bowline contract v{}: {} commands, {} fixtures. Use `bowline contract --json` for the machine contract.",
        output.contract_version,
        output.commands.len(),
        output.fixtures.len()
    );
}

fn command_descriptors_for_topic(topic: Option<&str>) -> Vec<CliCommandDescriptor> {
    let Some(topic) = topic else {
        return command_descriptors();
    };
    let topic = topic.trim();
    if topic.is_empty() {
        return command_descriptors();
    }
    COMMAND_REGISTRY
        .iter()
        .filter(|spec| {
            spec.name == topic
                || spec.aliases.contains(&topic)
                || spec.group.eq_ignore_ascii_case(topic)
        })
        .map(command_descriptor)
        .collect()
}

fn command_descriptors() -> Vec<CliCommandDescriptor> {
    COMMAND_REGISTRY.iter().map(command_descriptor).collect()
}

fn command_descriptor(spec: &CommandSpec) -> CliCommandDescriptor {
    CliCommandDescriptor {
        group: spec.group.to_string(),
        name: spec.name.to_string(),
        aliases: spec
            .aliases
            .iter()
            .map(|alias| (*alias).to_string())
            .collect(),
        summary: spec.summary.to_string(),
        usage: spec.usage.to_string(),
        options: spec.options.iter().map(command_option).collect(),
        examples: spec.examples.iter().map(command_example).collect(),
        json_output_type: spec.json_output_type.to_string(),
        side_effect_level: spec.side_effect_level.to_string(),
        supports_json: spec.supports_json,
        supports_dry_run: spec.supports_dry_run,
        supports_idempotency_key: spec.supports_idempotency_key,
        bounded_output: spec.bounded_output.map(|bounded| BoundedOutputControls {
            default_limit: bounded.default_limit,
            max_limit: bounded.max_limit,
            cursor_format: bounded.cursor_format.to_string(),
            path_prefix: bounded.path_prefix,
        }),
        related_commands: spec
            .related_commands
            .iter()
            .map(|command| (*command).to_string())
            .collect(),
    }
}

fn command_option(option: &OptionSpec) -> CliCommandOption {
    CliCommandOption {
        name: option.name.to_string(),
        value_name: option.value_name.map(str::to_string),
        summary: option.summary.to_string(),
        required: option.required,
        repeatable: option.repeatable,
    }
}

fn command_example(example: &ExampleSpec) -> CliCommandExample {
    CliCommandExample {
        command: example.command.to_string(),
        summary: example.summary.to_string(),
    }
}

fn command_groups() -> Vec<CliCommandGroup> {
    command_groups_for_descriptors(&command_descriptors())
}

fn command_groups_for_descriptors(descriptors: &[CliCommandDescriptor]) -> Vec<CliCommandGroup> {
    let mut groups = Vec::<CliCommandGroup>::new();
    for descriptor in descriptors {
        if let Some(group) = groups
            .iter_mut()
            .find(|group| group.name == descriptor.group)
        {
            group.commands.push(descriptor.name.clone());
        } else {
            groups.push(CliCommandGroup {
                name: descriptor.group.clone(),
                commands: vec![descriptor.name.clone()],
            });
        }
    }
    groups
}

fn render_command_help(descriptor: &CliCommandDescriptor) -> String {
    let mut output = format!(
        "{}\n  {}\n\nUsage:\n  {}\n\nJSON output:\n  {}\n\nSide effects:\n  {}",
        descriptor.name,
        descriptor.summary,
        descriptor.usage,
        descriptor.json_output_type,
        descriptor.side_effect_level
    );
    if !descriptor.aliases.is_empty() {
        output.push_str(&format!(
            "\n\nAliases:\n  {}",
            descriptor.aliases.join(", ")
        ));
    }
    if !descriptor.options.is_empty() {
        output.push_str("\n\nOptions:");
        for option in &descriptor.options {
            let value = option
                .value_name
                .as_ref()
                .map(|value| format!(" <{value}>"))
                .unwrap_or_default();
            output.push_str(&format!("\n  {}{}  {}", option.name, value, option.summary));
        }
    }
    if let Some(bounded) = &descriptor.bounded_output {
        output.push_str(&format!(
            "\n\nBounds:\n  default limit {}, max {}, cursor {}",
            bounded.default_limit, bounded.max_limit, bounded.cursor_format
        ));
    }
    if !descriptor.related_commands.is_empty() {
        output.push_str(&format!(
            "\n\nRelated:\n  {}",
            descriptor.related_commands.join(", ")
        ));
    }
    output
}

fn command_output_types() -> Vec<String> {
    COMMAND_REGISTRY
        .iter()
        .map(|spec| spec.json_output_type)
        .filter(|output_type| *output_type != "none")
        .fold(Vec::<String>::new(), |mut output_types, output_type| {
            if !output_types.iter().any(|existing| existing == output_type) {
                output_types.push(output_type.to_string());
            }
            output_types
        })
}

fn contract_fixtures() -> Vec<ContractFixtureDescriptor> {
    [
        (
            "agent-context",
            "tests/contracts/commands/agent-context.json",
            "AgentContextCommandOutput",
        ),
        (
            "agent-lease-create",
            "tests/contracts/commands/agent-lease-create.json",
            "AgentLeaseCreateCommandOutput",
        ),
        (
            "agent-prompt",
            "tests/contracts/commands/agent-prompt.json",
            "AgentPromptCommandOutput",
        ),
        (
            "contract",
            "tests/contracts/commands/contract.json",
            "ContractCommandOutput",
        ),
        (
            "dry-run",
            "tests/contracts/commands/dry-run.json",
            "DryRunCommandOutput",
        ),
        (
            "explain-env",
            "tests/contracts/commands/explain-env.json",
            "ExplainCommandOutput",
        ),
        (
            "help",
            "tests/contracts/commands/help.json",
            "HelpCommandOutput",
        ),
        (
            "setup-blocked",
            "tests/contracts/commands/setup-blocked.json",
            "PrewarmCommandOutput",
        ),
        (
            "version",
            "tests/contracts/commands/version.json",
            "VersionCommandOutput",
        ),
        (
            "work-accept-review-ready",
            "tests/contracts/commands/work-accept-review-ready.json",
            "WorkDiffCommandOutput",
        ),
        (
            "work-accept",
            "tests/contracts/commands/work-accept.json",
            "WorkLifecycleCommandOutput",
        ),
        (
            "work-discard",
            "tests/contracts/commands/work-discard.json",
            "WorkLifecycleCommandOutput",
        ),
        (
            "work-review",
            "tests/contracts/commands/work-review.json",
            "WorkDiffCommandOutput",
        ),
        (
            "workon-created",
            "tests/contracts/commands/workon-created.json",
            "WorkonCommandOutput",
        ),
    ]
    .into_iter()
    .map(|(name, path, output_type)| ContractFixtureDescriptor {
        name: name.to_string(),
        path: path.to_string(),
        output_type: output_type.to_string(),
    })
    .collect()
}

fn parse_login_command(args: &[String]) -> Command {
    let mut root = None;
    let mut headless = false;
    let mut no_poll = false;
    let mut index = 0_usize;

    while index < args.len() {
        match args[index].as_str() {
            "--root" => {
                let Some(value) = args.get(index + 1) else {
                    return command_usage_error(
                        CommandName::Login,
                        "usage_error",
                        "bowline login --root requires a path".to_string(),
                        vec![SafeAction {
                            label: "Log in and prepare ~/Code".to_string(),
                            command: Some("bowline login".to_string()),
                        }],
                    );
                };
                root = Some(value.to_string());
                index += 2;
            }
            "--headless" => {
                headless = true;
                index += 1;
            }
            "--no-poll" => {
                no_poll = true;
                index += 1;
            }
            flag if flag.starts_with("--") => {
                return command_usage_error(
                    CommandName::Login,
                    "usage_error",
                    format!("unknown bowline login option `{flag}`"),
                    vec![SafeAction {
                        label: "Start login".to_string(),
                        command: Some("bowline login".to_string()),
                    }],
                );
            }
            value => {
                return command_usage_error(
                    CommandName::Login,
                    "usage_error",
                    format!("unexpected bowline login argument `{value}`"),
                    vec![SafeAction {
                        label: "Start login".to_string(),
                        command: Some("bowline login".to_string()),
                    }],
                );
            }
        }
    }

    Command::Login(login::LoginArgs {
        root,
        no_poll,
        headless,
    })
}

fn parse_approve_command(args: &[String]) -> Command {
    let mut request_id = None;
    let mut yes = false;
    for arg in args {
        match arg.as_str() {
            "--yes" => yes = true,
            flag if flag.starts_with("--") => {
                return command_usage_error(
                    CommandName::Approve,
                    "usage_error",
                    format!("unknown bowline approve option `{flag}`"),
                    vec![SafeAction {
                        label: "Approve pending device".to_string(),
                        command: Some("bowline approve".to_string()),
                    }],
                );
            }
            value if request_id.is_none() => request_id = Some(value.to_string()),
            value => {
                return command_usage_error(
                    CommandName::Approve,
                    "usage_error",
                    format!("unexpected bowline approve argument `{value}`"),
                    vec![SafeAction {
                        label: "Approve pending device".to_string(),
                        command: Some("bowline approve".to_string()),
                    }],
                );
            }
        }
    }
    Command::Approve(ApproveArgs { request_id, yes })
}

fn parse_revoke_command(args: &[String]) -> Command {
    match args {
        [device_id] => Command::Revoke(RevokeArgs {
            device_id: device_id.to_string(),
        }),
        [] => command_usage_error(
            CommandName::Revoke,
            "usage_error",
            "bowline revoke requires a device id".to_string(),
            vec![SafeAction {
                label: "Inspect workspace status".to_string(),
                command: Some("bowline status".to_string()),
            }],
        ),
        [flag, ..] if flag.starts_with("--") => command_usage_error(
            CommandName::Revoke,
            "usage_error",
            format!("unknown bowline revoke option `{flag}`"),
            vec![SafeAction {
                label: "Inspect workspace status".to_string(),
                command: Some("bowline status".to_string()),
            }],
        ),
        _ => command_usage_error(
            CommandName::Revoke,
            "usage_error",
            "bowline revoke accepts exactly one device id".to_string(),
            vec![SafeAction {
                label: "Inspect workspace status".to_string(),
                command: Some("bowline status".to_string()),
            }],
        ),
    }
}

fn parse_recover_command(args: &[String]) -> Command {
    parse_recovery_command(args)
}

fn parse_init_command(args: &[String]) -> Command {
    match args {
        [] => Command::Init(InitArgs { root: None }),
        [flag, ..] if flag.starts_with("--") => command_usage_error(
            CommandName::Init,
            "usage_error",
            format!("unknown bowline login option `{flag}`"),
            vec![SafeAction {
                label: "Log in and choose a root".to_string(),
                command: Some("bowline login --root <path>".to_string()),
            }],
        ),
        [root] => Command::Init(InitArgs {
            root: Some(root.to_string()),
        }),
        _ => command_usage_error(
            CommandName::Init,
            "usage_error",
            "bowline login accepts at most one root path".to_string(),
            vec![SafeAction {
                label: "Log in and choose a root".to_string(),
                command: Some("bowline login --root <path>".to_string()),
            }],
        ),
    }
}

fn parse_prewarm_command(args: &[String]) -> Command {
    let mut approve_setup = false;
    let mut project_path = None;

    for arg in args {
        match arg.as_str() {
            "--approve-setup" => approve_setup = true,
            flag if flag.starts_with("--") => {
                return usage_error(
                    CommandName::Prewarm,
                    format!("unknown bowline setup option `{flag}`"),
                );
            }
            value if project_path.is_none() => project_path = Some(value.to_string()),
            _ => {
                return usage_error(
                    CommandName::Prewarm,
                    "bowline setup accepts exactly one path",
                );
            }
        }
    }

    match project_path {
        Some(project_path) => Command::Prewarm(PrewarmArgs {
            project_path,
            approve_setup,
        }),
        None => usage_error(
            CommandName::Prewarm,
            "bowline setup requires a project path",
        ),
    }
}

fn parse_setup_command(args: &[String]) -> Command {
    let mut yes = false;
    let mut project_path = None;

    for arg in args {
        match arg.as_str() {
            "--yes" => yes = true,
            flag if flag.starts_with("--") => {
                return command_usage_error(
                    CommandName::Setup,
                    "usage_error",
                    format!("unknown bowline setup option `{flag}`"),
                    vec![SafeAction {
                        label: "Prepare the current project".to_string(),
                        command: Some("bowline setup".to_string()),
                    }],
                );
            }
            value if project_path.is_none() => project_path = Some(value.to_string()),
            value => {
                return command_usage_error(
                    CommandName::Setup,
                    "usage_error",
                    format!("unexpected bowline setup argument `{value}`"),
                    vec![SafeAction {
                        label: "Prepare the current project".to_string(),
                        command: Some("bowline setup".to_string()),
                    }],
                );
            }
        }
    }

    Command::Setup(SetupArgs { project_path, yes })
}

fn parse_status_command(args: &[String]) -> Command {
    let mut watch = false;
    let mut workspace = false;
    let mut path = None;

    for arg in args {
        match arg.as_str() {
            "--watch" => watch = true,
            "--workspace" | "--all" => workspace = true,
            flag if flag.starts_with("--") => {
                return usage_error(
                    CommandName::Status,
                    format!("unknown bowline status option `{flag}`"),
                );
            }
            value if path.is_none() => path = Some(value.to_string()),
            _ => {
                return usage_error(
                    CommandName::Status,
                    "bowline status accepts at most one path",
                );
            }
        }
    }

    Command::Status(StatusArgs {
        path,
        watch,
        workspace,
    })
}

fn parse_actions_command(args: &[String]) -> Command {
    let mut workspace = false;
    let mut path = None;

    for arg in args {
        match arg.as_str() {
            "--workspace" => workspace = true,
            flag if flag.starts_with("--") => {
                return command_usage_error(
                    CommandName::Actions,
                    "usage_error",
                    format!("unknown bowline status option `{flag}`"),
                    vec![SafeAction {
                        label: "Inspect workspace status".to_string(),
                        command: Some("bowline status [path] --json".to_string()),
                    }],
                );
            }
            value if path.is_none() => path = Some(value.to_string()),
            _ => {
                return command_usage_error(
                    CommandName::Actions,
                    "usage_error",
                    "bowline status accepts at most one path".to_string(),
                    vec![SafeAction {
                        label: "Inspect workspace status".to_string(),
                        command: Some("bowline status [path] --json".to_string()),
                    }],
                );
            }
        }
    }

    Command::Actions(ActionsArgs { path, workspace })
}

fn parse_tui_command(args: &[String]) -> Command {
    match args {
        [] => Command::Tui(TuiArgs { path: None }),
        [flag, ..] if flag.starts_with("--") => command_usage_error(
            CommandName::Tui,
            "usage_error",
            format!("unknown bowline tui option `{flag}`"),
            vec![SafeAction {
                label: "Open the terminal UI".to_string(),
                command: Some("bowline tui [path]".to_string()),
            }],
        ),
        [path] => Command::Tui(TuiArgs {
            path: Some(path.to_string()),
        }),
        _ => command_usage_error(
            CommandName::Tui,
            "usage_error",
            "bowline tui accepts at most one path".to_string(),
            vec![SafeAction {
                label: "Open the terminal UI".to_string(),
                command: Some("bowline tui [path]".to_string()),
            }],
        ),
    }
}

fn parse_search_command(args: &[String]) -> Command {
    let mut values = Vec::new();
    let mut limit = DEFAULT_EXPLORATION_LIMIT;
    let mut cursor = None;
    let mut path_prefix = None;
    let mut index = 0_usize;
    while index < args.len() {
        match args[index].as_str() {
            "--limit" => {
                let Some(value) = args.get(index + 1) else {
                    return exploration_usage_error(
                        CommandName::Search,
                        "bowline search --limit requires a number",
                    );
                };
                let Some(parsed) = parse_exploration_limit(value) else {
                    return exploration_usage_error(
                        CommandName::Search,
                        "bowline search --limit must be between 1 and 100",
                    );
                };
                limit = parsed;
                index += 2;
            }
            "--cursor" => {
                let Some(value) = args.get(index + 1) else {
                    return exploration_usage_error(
                        CommandName::Search,
                        "bowline search --cursor requires a cursor",
                    );
                };
                let Some(parsed) = parse_exploration_cursor(value) else {
                    return exploration_usage_error(
                        CommandName::Search,
                        "bowline search --cursor must be opaque cursor format v1:<offset> with offset at most 10000",
                    );
                };
                cursor = Some(parsed);
                index += 2;
            }
            "--path-prefix" => {
                let Some(value) = args.get(index + 1) else {
                    return exploration_usage_error(
                        CommandName::Search,
                        "bowline search --path-prefix requires a prefix",
                    );
                };
                path_prefix = Some(value.to_string());
                index += 2;
            }
            flag if flag.starts_with("--") => {
                return command_usage_error(
                    CommandName::Search,
                    "usage_error",
                    format!("unknown bowline search option `{flag}`"),
                    vec![SafeAction {
                        label: "Search a project".to_string(),
                        command: Some("bowline search <query> [path]".to_string()),
                    }],
                );
            }
            value => {
                values.push(value.to_string());
                index += 1;
            }
        }
    }
    match values.as_slice() {
        [query] => Command::Search(SearchArgs {
            query: query.to_string(),
            path: None,
            limit,
            cursor,
            path_prefix,
        }),
        [query, path] => Command::Search(SearchArgs {
            query: query.to_string(),
            path: Some(path.to_string()),
            limit,
            cursor,
            path_prefix,
        }),
        [] => command_usage_error(
            CommandName::Search,
            "usage_error",
            "bowline search requires a query".to_string(),
            vec![SafeAction {
                label: "Search a project".to_string(),
                command: Some("bowline search <query> [path]".to_string()),
            }],
        ),
        _ => command_usage_error(
            CommandName::Search,
            "usage_error",
            "bowline search accepts <query> and an optional path".to_string(),
            vec![SafeAction {
                label: "Search a project".to_string(),
                command: Some("bowline search <query> [path]".to_string()),
            }],
        ),
    }
}

fn parse_symbols_command(args: &[String]) -> Command {
    let mut values = Vec::new();
    let mut limit = DEFAULT_EXPLORATION_LIMIT;
    let mut cursor = None;
    let mut path_prefix = None;
    let mut index = 0_usize;
    while index < args.len() {
        match args[index].as_str() {
            "--limit" => {
                let Some(value) = args.get(index + 1) else {
                    return exploration_usage_error(
                        CommandName::Symbols,
                        "bowline symbols --limit requires a number",
                    );
                };
                let Some(parsed) = parse_exploration_limit(value) else {
                    return exploration_usage_error(
                        CommandName::Symbols,
                        "bowline symbols --limit must be between 1 and 100",
                    );
                };
                limit = parsed;
                index += 2;
            }
            "--cursor" => {
                let Some(value) = args.get(index + 1) else {
                    return exploration_usage_error(
                        CommandName::Symbols,
                        "bowline symbols --cursor requires a cursor",
                    );
                };
                let Some(parsed) = parse_exploration_cursor(value) else {
                    return exploration_usage_error(
                        CommandName::Symbols,
                        "bowline symbols --cursor must be opaque cursor format v1:<offset> with offset at most 10000",
                    );
                };
                cursor = Some(parsed);
                index += 2;
            }
            "--path-prefix" => {
                let Some(value) = args.get(index + 1) else {
                    return exploration_usage_error(
                        CommandName::Symbols,
                        "bowline symbols --path-prefix requires a prefix",
                    );
                };
                path_prefix = Some(value.to_string());
                index += 2;
            }
            flag if flag.starts_with("--") => {
                return command_usage_error(
                    CommandName::Symbols,
                    "usage_error",
                    format!("unknown bowline symbols option `{flag}`"),
                    vec![SafeAction {
                        label: "Look up symbols".to_string(),
                        command: Some("bowline symbols <name> [path]".to_string()),
                    }],
                );
            }
            value => {
                values.push(value.to_string());
                index += 1;
            }
        }
    }
    match values.as_slice() {
        [query] => Command::Symbols(SymbolsArgs {
            query: query.to_string(),
            path: None,
            limit,
            cursor,
            path_prefix,
        }),
        [query, path] => Command::Symbols(SymbolsArgs {
            query: query.to_string(),
            path: Some(path.to_string()),
            limit,
            cursor,
            path_prefix,
        }),
        [] => command_usage_error(
            CommandName::Symbols,
            "usage_error",
            "bowline symbols requires a name".to_string(),
            vec![SafeAction {
                label: "Look up symbols".to_string(),
                command: Some("bowline symbols <name> [path]".to_string()),
            }],
        ),
        _ => command_usage_error(
            CommandName::Symbols,
            "usage_error",
            "bowline symbols accepts <name> and an optional path".to_string(),
            vec![SafeAction {
                label: "Look up symbols".to_string(),
                command: Some("bowline symbols <name> [path]".to_string()),
            }],
        ),
    }
}

fn parse_exploration_limit(value: &str) -> Option<usize> {
    let limit = value.parse::<usize>().ok()?;
    (1..=MAX_EXPLORATION_LIMIT)
        .contains(&limit)
        .then_some(limit)
}

fn parse_exploration_cursor(value: &str) -> Option<usize> {
    let offset = value.strip_prefix("v1:")?.parse::<usize>().ok()?;
    (offset <= MAX_EXPLORATION_CURSOR_OFFSET).then_some(offset)
}

fn exploration_usage_error(command: CommandName, message: &str) -> Command {
    command_usage_error(
        command,
        "usage_error",
        message.to_string(),
        vec![SafeAction {
            label: "Inspect command help".to_string(),
            command: Some(format!(
                "bowline help {} --json",
                match command {
                    CommandName::Search => "search",
                    CommandName::Symbols => "symbols",
                    _ => "help",
                }
            )),
        }],
    )
}

fn parse_explain_command(args: &[String]) -> Command {
    match args {
        [] => command_usage_error(
            CommandName::Explain,
            "usage_error",
            "bowline explain requires a path".to_string(),
            vec![SafeAction {
                label: "Explain a path".to_string(),
                command: Some("bowline explain <path>".to_string()),
            }],
        ),
        [flag, ..] if flag.starts_with("--") => command_usage_error(
            CommandName::Explain,
            "usage_error",
            format!("unknown bowline explain option `{flag}`"),
            vec![SafeAction {
                label: "Explain a path".to_string(),
                command: Some("bowline explain <path>".to_string()),
            }],
        ),
        [path] => Command::Explain(ExplainArgs {
            path: path.to_string(),
        }),
        _ => command_usage_error(
            CommandName::Explain,
            "usage_error",
            "bowline explain accepts exactly one path".to_string(),
            vec![SafeAction {
                label: "Explain a path".to_string(),
                command: Some("bowline explain <path>".to_string()),
            }],
        ),
    }
}

fn parse_devices_command(args: &[String]) -> Command {
    match args {
        [] => Command::Devices(devices::DevicesArgs::List),
        [subcommand] if subcommand == "list" => Command::Devices(devices::DevicesArgs::List),
        [subcommand] if subcommand == "request" => {
            Command::Devices(devices::DevicesArgs::Request { root: None })
        }
        [subcommand, flag, root] if subcommand == "request" && flag == "--root" => {
            Command::Devices(devices::DevicesArgs::Request {
                root: Some(root.to_string()),
            })
        }
        [subcommand, flag] if subcommand == "request" && flag == "--root" => command_usage_error(
            CommandName::Devices,
            "usage_error",
            "bowline login --root requires a path".to_string(),
            devices_usage_actions(),
        ),
        [subcommand, flag, ..] if subcommand == "request" && flag.starts_with("--") => {
            command_usage_error(
                CommandName::Devices,
                "usage_error",
                format!("unknown bowline login option `{flag}`"),
                devices_usage_actions(),
            )
        }
        [subcommand, request_id] if subcommand == "approve" => {
            Command::Devices(devices::DevicesArgs::Approve {
                request_id: request_id.to_string(),
            })
        }
        [subcommand, request_id] if subcommand == "accept" => {
            Command::Devices(devices::DevicesArgs::Accept {
                request_id: request_id.to_string(),
            })
        }
        [subcommand, request_id] if subcommand == "deny" => {
            Command::Devices(devices::DevicesArgs::Deny {
                request_id: request_id.to_string(),
            })
        }
        [subcommand, device_id] if subcommand == "revoke" => {
            Command::Devices(devices::DevicesArgs::Revoke {
                device_id: device_id.to_string(),
            })
        }
        [flag, ..] if flag.starts_with("--") => command_usage_error(
            CommandName::Devices,
            "usage_error",
            format!("unknown bowline trust option `{flag}`"),
            devices_usage_actions(),
        ),
        _ => command_usage_error(
            CommandName::Devices,
            "usage_error",
            "expected `bowline approve [request]`, `bowline revoke <device>`, or `bowline login --root <path>`"
                .to_string(),
            devices_usage_actions(),
        ),
    }
}

fn parse_recovery_command(args: &[String]) -> Command {
    match args {
        [] => Command::Recovery(recovery::RecoveryArgs::Status),
        [subcommand] if subcommand == "status" => Command::Recovery(recovery::RecoveryArgs::Status),
        [subcommand] if subcommand == "create" => Command::Recovery(recovery::RecoveryArgs::Create),
        [subcommand, envelope_id] if subcommand == "verify" => {
            Command::Recovery(recovery::RecoveryArgs::Verify {
                envelope_id: envelope_id.to_string(),
            })
        }
        [subcommand, _, words @ ..] if subcommand == "verify" && !words.is_empty() => {
            command_usage_error(
                CommandName::Recover,
                "usage_error",
                "Recovery Key words must be provided on stdin, not argv".to_string(),
                recovery_usage_actions(),
            )
        }
        [subcommand] if subcommand == "rotate" => Command::Recovery(recovery::RecoveryArgs::Rotate),
        [subcommand, envelope_id] if subcommand == "revoke" => {
            Command::Recovery(recovery::RecoveryArgs::Revoke {
                envelope_id: envelope_id.to_string(),
            })
        }
        [subcommand, envelope_id] if subcommand == "use" => {
            Command::Recovery(recovery::RecoveryArgs::Use {
                envelope_id: envelope_id.to_string(),
            })
        }
        [subcommand, _, words @ ..] if subcommand == "use" && !words.is_empty() => {
            command_usage_error(
                CommandName::Recover,
                "usage_error",
                "Recovery Key words must be provided on stdin, not argv".to_string(),
                recovery_usage_actions(),
            )
        }
        [flag, ..] if flag.starts_with("--") => command_usage_error(
            CommandName::Recover,
            "usage_error",
            format!("unknown bowline recover option `{flag}`"),
            recovery_usage_actions(),
        ),
        _ => command_usage_error(
            CommandName::Recover,
            "usage_error",
            "expected `bowline recover [status|create|verify <envelope-id>|rotate|revoke <envelope-id>|use <envelope-id>]`; Recovery Key words are read from stdin".to_string(),
            recovery_usage_actions(),
        ),
    }
}

fn parse_events_command(args: &[String]) -> Command {
    let mut workspace = false;
    let mut limit = 50;
    let mut path = None;
    let mut iter = args.iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--workspace" => workspace = true,
            "--limit" => {
                let Some(raw_limit) = iter.next() else {
                    return usage_error(CommandName::Events, "missing value for --limit");
                };
                match raw_limit.parse::<u32>() {
                    Ok(parsed)
                        if (1..=bowline_local::status::MAX_EVENTS_LIMIT).contains(&parsed) =>
                    {
                        limit = parsed;
                    }
                    _ => {
                        return usage_error(
                            CommandName::Events,
                            format!(
                                "expected --limit between 1 and {}",
                                bowline_local::status::MAX_EVENTS_LIMIT
                            ),
                        );
                    }
                }
            }
            flag if flag.starts_with("--") => {
                return usage_error(
                    CommandName::Events,
                    format!("unknown bowline status --watch option `{flag}`"),
                );
            }
            value if path.is_none() => path = Some(value.to_string()),
            _ => {
                return usage_error(
                    CommandName::Events,
                    "bowline status --watch accepts at most one path",
                );
            }
        }
    }

    Command::Events(EventsArgs {
        path,
        workspace,
        limit,
    })
}

fn parse_workon_command(args: &[String]) -> Command {
    match args {
        [project_path, name] => Command::Workon(work::WorkonArgs {
            project_path: project_path.to_string(),
            name: name.to_string(),
        }),
        [name] => Command::Workon(work::WorkonArgs {
            project_path: current_dir_string(),
            name: name.to_string(),
        }),
        [flag, ..] if flag.starts_with("--") => command_usage_error(
            CommandName::Workon,
            "usage_error",
            format!("unknown bowline workon option `{flag}`"),
            work_usage_actions(),
        ),
        [] => command_usage_error(
            CommandName::Workon,
            "usage_error",
            "bowline workon requires a name".to_string(),
            work_usage_actions(),
        ),
        _ => command_usage_error(
            CommandName::Workon,
            "usage_error",
            "bowline workon accepts [project-path] <name>".to_string(),
            work_usage_actions(),
        ),
    }
}

fn parse_review_command(args: &[String]) -> Command {
    match args {
        [] => Command::Review(work::WorkSelectorArgs {
            selector: current_dir_string(),
        }),
        [target] => Command::Review(work::WorkSelectorArgs {
            selector: target.to_string(),
        }),
        [flag, ..] if flag.starts_with("--") => command_usage_error(
            CommandName::Review,
            "usage_error",
            format!("unknown bowline review option `{flag}`"),
            work_usage_actions(),
        ),
        _ => command_usage_error(
            CommandName::Review,
            "usage_error",
            "bowline review accepts at most one target".to_string(),
            work_usage_actions(),
        ),
    }
}

fn parse_work_command(args: &[String]) -> Command {
    let mut include_hidden = false;
    for arg in args {
        match arg.as_str() {
            "--all" => include_hidden = true,
            flag if flag.starts_with("--") => {
                return command_usage_error(
                    CommandName::Work,
                    "usage_error",
                    format!("unknown bowline work option `{flag}`"),
                    work_usage_actions(),
                );
            }
            value => {
                return command_usage_error(
                    CommandName::Work,
                    "usage_error",
                    format!("unexpected bowline work argument `{value}`"),
                    work_usage_actions(),
                );
            }
        }
    }

    Command::Work(work::WorkListArgs { include_hidden })
}

fn parse_work_selector_command(
    command: CommandName,
    args: &[String],
    missing_message: &'static str,
) -> Command {
    match args {
        [selector] => {
            let args = work::WorkSelectorArgs {
                selector: selector.to_string(),
            };
            match command {
                CommandName::Diff => Command::WorkDiff(args),
                CommandName::Accept => Command::WorkAccept(args),
                CommandName::Discard => Command::WorkDiscard(args),
                CommandName::Restore => Command::WorkRestore(args),
                _ => unreachable!("unsupported work selector command"),
            }
        }
        [flag, ..] if flag.starts_with("--") => command_usage_error(
            command,
            "usage_error",
            format!(
                "unknown bowline {} option `{flag}`",
                command_name_token(command)
            ),
            work_usage_actions(),
        ),
        [] if matches!(
            command,
            CommandName::Accept | CommandName::Discard | CommandName::Restore
        ) =>
        {
            let args = work::WorkSelectorArgs {
                selector: current_dir_string(),
            };
            match command {
                CommandName::Accept => Command::WorkAccept(args),
                CommandName::Discard => Command::WorkDiscard(args),
                CommandName::Restore => Command::WorkRestore(args),
                _ => unreachable!("unsupported work selector command"),
            }
        }
        [] => command_usage_error(
            command,
            "usage_error",
            missing_message.to_string(),
            work_usage_actions(),
        ),
        _ => command_usage_error(
            command,
            "usage_error",
            "work-view selector commands accept exactly one id or name".to_string(),
            work_usage_actions(),
        ),
    }
}

fn parse_agent_lease_create_command(args: &[String]) -> Command {
    let mut project_path = None;
    let mut task = None;
    let mut base = agent::parse_base("latest-workspace").expect("default base is valid");
    let mut hydrate_budget_bytes = DEFAULT_AGENT_HYDRATE_BUDGET_BYTES;
    let mut work_view = false;
    let mut index = 0_usize;

    while index < args.len() {
        match args[index].as_str() {
            "--work-view" => {
                work_view = true;
                index += 1;
            }
            "--task" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(CommandName::AgentStart, "missing value for --task");
                };
                task = Some(value.to_string());
                index += 2;
            }
            "--base" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(CommandName::AgentStart, "missing value for --base");
                };
                let Some(parsed) = agent::parse_base(value) else {
                    return command_usage_error(
                        CommandName::AgentStart,
                        "usage_error",
                        "expected --base latest-workspace or --base latest:main".to_string(),
                        agent_usage_actions(),
                    );
                };
                base = parsed;
                index += 2;
            }
            "--hydrate-budget" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(
                        CommandName::AgentStart,
                        "missing value for --hydrate-budget",
                    );
                };
                let Some(parsed) = parse_byte_budget(value) else {
                    return command_usage_error(
                        CommandName::AgentStart,
                        "usage_error",
                        "expected --hydrate-budget as bytes, KiB, MiB, or GiB".to_string(),
                        agent_usage_actions(),
                    );
                };
                hydrate_budget_bytes = parsed;
                index += 2;
            }
            flag if flag.starts_with("--") => {
                return command_usage_error(
                    CommandName::AgentStart,
                    "usage_error",
                    format!("unknown bowline agent start option `{flag}`"),
                    agent_usage_actions(),
                );
            }
            value if project_path.is_none() => {
                project_path = Some(value.to_string());
                index += 1;
            }
            value => {
                return command_usage_error(
                    CommandName::AgentStart,
                    "usage_error",
                    format!("unexpected bowline agent start argument `{value}`"),
                    agent_usage_actions(),
                );
            }
        }
    }

    let project_path = project_path.unwrap_or_else(current_dir_string);
    let Some(task) = task else {
        return command_usage_error(
            CommandName::AgentStart,
            "usage_error",
            "bowline agent start requires --task <task>".to_string(),
            agent_usage_actions(),
        );
    };

    Command::AgentLeaseCreate(agent::AgentLeaseCreateArgs {
        project_path,
        task,
        base,
        hydrate_budget_bytes,
        work_view,
    })
}

fn parse_agent_start_command(args: &[String]) -> Command {
    parse_agent_lease_create_command(args)
}

fn parse_agent_selector_command(command: CommandName, args: &[String]) -> Command {
    let mut lease_id = None;
    let mut index = 0_usize;
    while index < args.len() {
        match args[index].as_str() {
            "--lease" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(command, "missing value for --lease");
                };
                lease_id = Some(value.to_string());
                index += 2;
            }
            flag if flag.starts_with("--") => {
                return command_usage_error(
                    command,
                    "usage_error",
                    format!(
                        "unknown bowline {} option `{flag}`",
                        command_name_token(command)
                    ),
                    agent_usage_actions(),
                );
            }
            value => {
                return command_usage_error(
                    command,
                    "usage_error",
                    format!(
                        "unexpected bowline {} argument `{value}`",
                        command_name_token(command)
                    ),
                    agent_usage_actions(),
                );
            }
        }
    }
    let Some(lease_id) = lease_id else {
        return command_usage_error(
            command,
            "usage_error",
            format!(
                "bowline {} requires --lease <id>",
                command_name_token(command)
            ),
            agent_usage_actions(),
        );
    };
    let args = agent::AgentLeaseSelectorArgs { lease_id };
    match command {
        CommandName::AgentContext => Command::AgentContext(args),
        CommandName::AgentPrompt => Command::AgentPrompt(args),
        CommandName::AgentPublish => Command::AgentPublish(args),
        CommandName::AgentComplete => Command::AgentComplete(args),
        _ => unreachable!("unsupported agent selector command"),
    }
}

fn parse_agent_budget_command(args: &[String]) -> Command {
    let mut lease_id = None;
    let mut add_bytes = None;
    let mut index = 0_usize;
    while index < args.len() {
        match args[index].as_str() {
            "--lease" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(CommandName::AgentBudget, "missing value for --lease");
                };
                lease_id = Some(value.to_string());
                index += 2;
            }
            "--add" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(CommandName::AgentBudget, "missing value for --add");
                };
                let Some(parsed) = parse_byte_budget(value) else {
                    return command_usage_error(
                        CommandName::AgentBudget,
                        "usage_error",
                        "expected --add as bytes, KiB, MiB, or GiB".to_string(),
                        agent_usage_actions(),
                    );
                };
                add_bytes = Some(parsed);
                index += 2;
            }
            flag if flag.starts_with("--") => {
                return command_usage_error(
                    CommandName::AgentBudget,
                    "usage_error",
                    format!("unknown bowline agent budget option `{flag}`"),
                    agent_usage_actions(),
                );
            }
            value => {
                return command_usage_error(
                    CommandName::AgentBudget,
                    "usage_error",
                    format!("unexpected bowline agent budget argument `{value}`"),
                    agent_usage_actions(),
                );
            }
        }
    }
    let Some(lease_id) = lease_id else {
        return command_usage_error(
            CommandName::AgentBudget,
            "usage_error",
            "bowline agent budget requires --lease <id>".to_string(),
            agent_usage_actions(),
        );
    };
    let Some(add_bytes) = add_bytes else {
        return command_usage_error(
            CommandName::AgentBudget,
            "usage_error",
            "bowline agent budget requires --add <bytes>".to_string(),
            agent_usage_actions(),
        );
    };
    Command::AgentBudget(agent::AgentBudgetArgs {
        lease_id,
        add_bytes,
    })
}

fn current_dir_string() -> String {
    env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .to_string_lossy()
        .to_string()
}

fn confirm_return(prompt: &str) -> bool {
    if !io::stdin().is_terminal() {
        return false;
    }
    print!("{prompt} Press Return to approve, or type no to cancel: ");
    let _ = io::stdout().flush();
    let mut answer = String::new();
    io::stdin().read_line(&mut answer).is_ok()
        && !matches!(answer.trim().to_ascii_lowercase().as_str(), "n" | "no")
}

fn parse_cleanup_command(args: &[String]) -> Command {
    let mut apply = false;
    for arg in args {
        match arg.as_str() {
            "--apply" => apply = true,
            flag if flag.starts_with("--") => {
                return command_usage_error(
                    CommandName::Cleanup,
                    "usage_error",
                    format!("unknown bowline cleanup option `{flag}`"),
                    work_usage_actions(),
                );
            }
            value => {
                return command_usage_error(
                    CommandName::Cleanup,
                    "usage_error",
                    format!("unexpected bowline cleanup argument `{value}`"),
                    work_usage_actions(),
                );
            }
        }
    }

    Command::WorkCleanup(work::WorkCleanupArgs { apply })
}

fn parse_resolve_command(args: &[String]) -> Command {
    let mut project_or_path = None;
    let mut copy_prompt = false;
    let mut tui = false;
    let mut diff = None;
    let mut agent = None;
    let mut decision = None;
    let mut index = 0_usize;

    while index < args.len() {
        match args[index].as_str() {
            "--copy-prompt" => {
                copy_prompt = true;
                index += 1;
            }
            "--tui" => {
                tui = true;
                index += 1;
            }
            "--diff" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(CommandName::Resolve, "missing value for --diff");
                };
                diff = Some(value.to_string());
                index += 2;
            }
            "--agent" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(CommandName::Resolve, "missing value for --agent");
                };
                let Some(parsed) = resolve::parse_agent(value) else {
                    return usage_error(
                        CommandName::Resolve,
                        "expected --agent codex, --agent claude, or --agent cursor",
                    );
                };
                agent = Some(parsed);
                index += 2;
            }
            "--accept" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(CommandName::Resolve, "missing value for --accept");
                };
                if decision.is_some() {
                    return usage_error(
                        CommandName::Resolve,
                        "bowline resolve accepts only one --accept or --reject action",
                    );
                }
                decision = Some(resolve::ResolveDecision::Accept(value.to_string()));
                index += 2;
            }
            "--reject" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(CommandName::Resolve, "missing value for --reject");
                };
                if decision.is_some() {
                    return usage_error(
                        CommandName::Resolve,
                        "bowline resolve accepts only one --accept or --reject action",
                    );
                }
                decision = Some(resolve::ResolveDecision::Reject(value.to_string()));
                index += 2;
            }
            flag if flag.starts_with("--") => {
                return usage_error(
                    CommandName::Resolve,
                    format!("unknown bowline resolve option `{flag}`"),
                );
            }
            value if project_or_path.is_none() => {
                project_or_path = Some(value.to_string());
                index += 1;
            }
            value => {
                return usage_error(
                    CommandName::Resolve,
                    format!("unexpected bowline resolve argument `{value}`"),
                );
            }
        }
    }

    let project_or_path = project_or_path.unwrap_or_else(current_dir_string);
    if diff.is_some() && decision.is_some() {
        return usage_error(
            CommandName::Resolve,
            "bowline resolve --diff cannot be combined with --accept or --reject",
        );
    }

    Command::Resolve(resolve::ResolveArgs {
        project_or_path,
        copy_prompt,
        tui,
        diff,
        agent,
        decision,
    })
}

fn parse_bootstrap_ssh_command(args: &[String]) -> Command {
    let Some(host) = args.first() else {
        return command_usage_error(
            CommandName::BootstrapSsh,
            "usage_error",
            "bowline connect requires a host".to_string(),
            vec![SafeAction {
                label: "Connect a remote host".to_string(),
                command: Some("bowline connect <host>".to_string()),
            }],
        );
    };
    let mut root = None;
    let mut artifact = None;
    let mut project = None;
    let mut task = None;
    let mut agent = None;
    let mut index = 1_usize;

    while index < args.len() {
        match args[index].as_str() {
            "--root" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(CommandName::BootstrapSsh, "missing value for --root");
                };
                root = Some(value.to_string());
                index += 2;
            }
            "--binary" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(CommandName::BootstrapSsh, "missing value for --binary");
                };
                artifact = Some(value.to_string());
                index += 2;
            }
            "--project" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(CommandName::BootstrapSsh, "missing value for --project");
                };
                project = Some(value.to_string());
                index += 2;
            }
            "--task" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(CommandName::BootstrapSsh, "missing value for --task");
                };
                task = Some(value.to_string());
                index += 2;
            }
            "--agent" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(CommandName::BootstrapSsh, "missing value for --agent");
                };
                agent = Some(value.to_string());
                index += 2;
            }
            flag if flag.starts_with("--") => {
                return command_usage_error(
                    CommandName::BootstrapSsh,
                    "usage_error",
                    format!("unknown bowline connect option `{flag}`"),
                    vec![SafeAction {
                        label: "Connect a remote host".to_string(),
                        command: Some("bowline connect <host>".to_string()),
                    }],
                );
            }
            value => {
                return command_usage_error(
                    CommandName::BootstrapSsh,
                    "usage_error",
                    format!("unexpected bowline connect argument `{value}`"),
                    vec![SafeAction {
                        label: "Connect a remote host".to_string(),
                        command: Some("bowline connect <host>".to_string()),
                    }],
                );
            }
        }
    }

    let Some(root) = root else {
        return command_usage_error(
            CommandName::BootstrapSsh,
            "usage_error",
            "bowline connect uses the active workspace root; run bowline login first or pass --root"
                .to_string(),
            vec![SafeAction {
                label: "Connect a remote host".to_string(),
                command: Some(format!("bowline connect {host}")),
            }],
        );
    };

    if project.is_some() != task.is_some() {
        return command_usage_error(
            CommandName::BootstrapSsh,
            "usage_error",
            "bowline connect agent handoff requires both --project <project> and --task <task>"
                .to_string(),
            vec![SafeAction {
                label: "Connect and start remote agent work".to_string(),
                command: Some(format!(
                    "bowline connect {host} --project <project> --task '<task>'"
                )),
            }],
        );
    }
    if agent.is_some() && project.is_none() {
        return command_usage_error(
            CommandName::BootstrapSsh,
            "usage_error",
            "bowline connect --agent requires --project <project> and --task <task>".to_string(),
            vec![SafeAction {
                label: "Connect and start remote agent work".to_string(),
                command: Some(format!(
                    "bowline connect {host} --project <project> --task '<task>' --agent codex"
                )),
            }],
        );
    }

    Command::BootstrapSsh(bootstrap::BootstrapSshArgs {
        host: host.to_string(),
        root,
        artifact,
        project,
        task,
        agent,
    })
}

fn parse_connect_command(args: &[String]) -> Command {
    let Some(host) = args.first() else {
        return command_usage_error(
            CommandName::Connect,
            "usage_error",
            "bowline connect requires a host".to_string(),
            vec![SafeAction {
                label: "Connect a host".to_string(),
                command: Some("bowline connect <host>".to_string()),
            }],
        );
    };
    let mut root = None;
    let mut artifact = None;
    let mut project = None;
    let mut task = None;
    let mut agent = None;
    let mut index = 1_usize;

    while index < args.len() {
        match args[index].as_str() {
            "--root" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(CommandName::Connect, "missing value for --root");
                };
                root = Some(value.to_string());
                index += 2;
            }
            "--binary" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(CommandName::Connect, "missing value for --binary");
                };
                artifact = Some(value.to_string());
                index += 2;
            }
            "--project" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(CommandName::Connect, "missing value for --project");
                };
                project = Some(value.to_string());
                index += 2;
            }
            "--task" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(CommandName::Connect, "missing value for --task");
                };
                task = Some(value.to_string());
                index += 2;
            }
            "--agent" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(CommandName::Connect, "missing value for --agent");
                };
                agent = Some(value.to_string());
                index += 2;
            }
            flag if flag.starts_with("--") => {
                return command_usage_error(
                    CommandName::Connect,
                    "usage_error",
                    format!("unknown bowline connect option `{flag}`"),
                    vec![SafeAction {
                        label: "Connect a host".to_string(),
                        command: Some(format!("bowline connect {host}")),
                    }],
                );
            }
            value => {
                return command_usage_error(
                    CommandName::Connect,
                    "usage_error",
                    format!("unexpected bowline connect argument `{value}`"),
                    vec![SafeAction {
                        label: "Connect a host".to_string(),
                        command: Some(format!("bowline connect {host}")),
                    }],
                );
            }
        }
    }

    if project.is_some() != task.is_some() {
        return command_usage_error(
            CommandName::Connect,
            "usage_error",
            "bowline connect agent handoff requires both --project <project> and --task <task>"
                .to_string(),
            vec![SafeAction {
                label: "Connect and start remote agent work".to_string(),
                command: Some(format!(
                    "bowline connect {host} --project <project> --task '<task>'"
                )),
            }],
        );
    }
    if agent.is_some() && project.is_none() {
        return command_usage_error(
            CommandName::Connect,
            "usage_error",
            "bowline connect --agent requires --project <project> and --task <task>".to_string(),
            vec![SafeAction {
                label: "Connect and start remote agent work".to_string(),
                command: Some(format!(
                    "bowline connect {host} --project <project> --task '<task>' --agent codex"
                )),
            }],
        );
    }

    Command::BootstrapSsh(bootstrap::BootstrapSshArgs {
        host: host.to_string(),
        root: root
            .or_else(runtime::active_workspace_root)
            .unwrap_or_else(|| "~/Code".to_string()),
        artifact,
        project,
        task,
        agent,
    })
}

fn devices_usage_actions() -> Vec<SafeAction> {
    vec![
        SafeAction {
            label: "Inspect workspace status".to_string(),
            command: Some("bowline status".to_string()),
        },
        SafeAction {
            label: "Approve a pending device".to_string(),
            command: Some("bowline approve".to_string()),
        },
    ]
}

fn recovery_usage_actions() -> Vec<SafeAction> {
    vec![
        SafeAction {
            label: "Show Recovery Key status".to_string(),
            command: Some("bowline recover status".to_string()),
        },
        SafeAction {
            label: "Create a Recovery Key".to_string(),
            command: Some("bowline recover create".to_string()),
        },
    ]
}

fn work_usage_actions() -> Vec<SafeAction> {
    vec![
        SafeAction {
            label: "Start a work view".to_string(),
            command: Some("bowline workon <name>".to_string()),
        },
        SafeAction {
            label: "Review work".to_string(),
            command: Some("bowline review".to_string()),
        },
    ]
}

fn agent_usage_actions() -> Vec<SafeAction> {
    vec![
        SafeAction {
            label: "Start agent work".to_string(),
            command: Some(
                "bowline agent start <project> --task <task> --base latest-workspace".to_string(),
            ),
        },
        SafeAction {
            label: "Inspect an agent work".to_string(),
            command: Some("bowline agent context --lease <id>".to_string()),
        },
        SafeAction {
            label: "Publish an agent work for review".to_string(),
            command: Some("bowline agent publish --lease <id>".to_string()),
        },
        SafeAction {
            label: "Increase agent hydration budget".to_string(),
            command: Some("bowline agent budget --lease <id> --add 64MiB".to_string()),
        },
    ]
}

fn parse_byte_budget(value: &str) -> Option<u64> {
    let trimmed = value.trim();
    let (number, multiplier) = if let Some(number) = trimmed.strip_suffix("GiB") {
        (number, 1024_u64 * 1024 * 1024)
    } else if let Some(number) = trimmed.strip_suffix("MiB") {
        (number, 1024_u64 * 1024)
    } else if let Some(number) = trimmed.strip_suffix("KiB") {
        (number, 1024_u64)
    } else if let Some(number) = trimmed.strip_suffix("GB") {
        (number, 1_000_000_000_u64)
    } else if let Some(number) = trimmed.strip_suffix("MB") {
        (number, 1_000_000_u64)
    } else if let Some(number) = trimmed.strip_suffix("KB") {
        (number, 1_000_u64)
    } else {
        (trimmed, 1_u64)
    };
    number.trim().parse::<u64>().ok()?.checked_mul(multiplier)
}

fn command_name_token(command: CommandName) -> &'static str {
    match command {
        CommandName::Help => "help",
        CommandName::Version => "version",
        CommandName::Contract => "contract",
        CommandName::Unknown => "unknown",
        CommandName::Login => "login",
        CommandName::Approve => "approve",
        CommandName::Revoke => "revoke",
        CommandName::Recover => "recover",
        CommandName::Init => "init",
        CommandName::Setup => "setup",
        CommandName::Prewarm => "prewarm",
        CommandName::Status => "status",
        CommandName::Search => "search",
        CommandName::Symbols => "symbols",
        CommandName::Explain => "explain",
        CommandName::Devices => "devices",
        CommandName::Recovery => "recovery",
        CommandName::Events => "events",
        CommandName::Actions => "actions",
        CommandName::Tui => "tui",
        CommandName::Resolve => "resolve",
        CommandName::Workon => "workon",
        CommandName::Review => "review",
        CommandName::Work => "work",
        CommandName::Diff => "diff",
        CommandName::Accept => "accept",
        CommandName::Discard => "discard",
        CommandName::Restore => "restore",
        CommandName::Cleanup => "cleanup",
        CommandName::AgentContext => "agent context",
        CommandName::AgentLeaseCreate => "agent lease create",
        CommandName::AgentStart => "agent start",
        CommandName::AgentPrompt => "agent prompt",
        CommandName::AgentPublish => "agent publish",
        CommandName::AgentComplete => "agent complete",
        CommandName::AgentBudget => "agent budget",
        CommandName::DaemonStart => "daemon start",
        CommandName::DaemonStop => "daemon stop",
        CommandName::DaemonStatus => "daemon status",
        CommandName::DaemonInstall => "daemon install",
        CommandName::DaemonRestart => "daemon restart",
        CommandName::DaemonUninstall => "daemon uninstall",
        CommandName::DiagnosticsCollect => "diagnostics collect",
        CommandName::BootstrapSsh => "bootstrap ssh",
        CommandName::Connect => "connect",
    }
}

fn command_usage_error(
    command: CommandName,
    code: &'static str,
    message: String,
    next_actions: Vec<SafeAction>,
) -> Command {
    Command::CommandUsageError(CommandUsageError {
        command,
        code,
        message,
        next_actions,
    })
}

fn print_login(mut args: login::LoginArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let root = args.root.clone();
    let wait_for_trust = !json && !args.no_poll && !args.headless;
    if json && args.no_poll && root.is_some() && can_finish_login_workspace_without_auth() {
        return print_login_workspace(root, generated_at, false, true);
    }
    args = login_args_for_output(args, json);
    if !json && !args.no_poll && !args.headless {
        return print_polling_login(root, generated_at, wait_for_trust);
    }
    match login::run(args, generated_at.clone()) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", render_login_human(&output));
            print_login_workspace(root, generated_at, wait_for_trust, false)
        }
        Err(error) => {
            print_runtime_error(CommandName::Login, generated_at, &error, json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn can_finish_login_workspace_without_auth() -> bool {
    runtime::control_plane().is_ok()
}

fn print_polling_login(
    root: Option<String>,
    generated_at: String,
    wait_for_trust: bool,
) -> ExitCode {
    let (authorization, pending_output) = match login::start(generated_at.clone()) {
        Ok(started) => started,
        Err(error) => {
            print_runtime_error(CommandName::Login, generated_at, &error, false);
            return ExitCode::from(EXIT_RUNTIME);
        }
    };

    print!("{}", render_login_human(&pending_output));
    let _ = io::stdout().flush();

    match login::finish(authorization, generated_at.clone()) {
        Ok(output) => {
            print!("{}", render_login_human(&output));
            print_login_workspace(root, generated_at, wait_for_trust, false)
        }
        Err(error) => {
            print_runtime_error(CommandName::Login, generated_at, &error, false);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn print_login_workspace(
    root: Option<String>,
    generated_at: String,
    wait_for_trust: bool,
    json: bool,
) -> ExitCode {
    let options = InitOptions {
        db_path: metadata_db_path(),
        requested_root: root
            .or_else(runtime::active_workspace_root)
            .map(resolve_explicit_path),
        generated_at: generated_at.clone(),
    };
    match bowline_local::init::initialize_root_with_workspace(
        options,
        runtime::active_workspace_id(),
    ) {
        Ok(mut output) => {
            output.command = CommandName::Login;
            let pending_request =
                attach_first_device_trust_if_available(&mut output, &generated_at);
            let workspace_id = output.workspace_id.clone();
            if json {
                print_json(&output);
            } else {
                print!("{}", render_init_human(&output));
            }
            if wait_for_trust && let Some(request_id) = pending_request {
                return wait_for_device_grant(workspace_id, request_id, generated_at);
            }
            ExitCode::SUCCESS
        }
        Err(LocalInitError::AmbiguousDefaultRoot(candidates)) => {
            print_ambiguous_init_root(candidates, generated_at, json);
            ExitCode::from(EXIT_USAGE)
        }
        Err(error) => {
            print_runtime_error(CommandName::Login, generated_at, &error.to_string(), json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn login_args_for_output(mut args: login::LoginArgs, json: bool) -> login::LoginArgs {
    if json {
        args.no_poll = true;
    }
    args
}

fn print_init(args: InitArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let options = InitOptions {
        db_path: metadata_db_path(),
        requested_root: args.root.map(resolve_explicit_path),
        generated_at: generated_at.clone(),
    };

    match bowline_local::init::initialize_root_with_workspace(
        options,
        runtime::active_workspace_id_without_local_metadata_probe(),
    ) {
        Ok(mut output) if json => {
            attach_first_device_trust_if_available(&mut output, &generated_at);
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(mut output) => {
            attach_first_device_trust_if_available(&mut output, &generated_at);
            print!("{}", render_init_human(&output));
            ExitCode::SUCCESS
        }
        Err(LocalInitError::AmbiguousDefaultRoot(candidates)) => {
            print_ambiguous_init_root(candidates, generated_at, json);
            ExitCode::from(EXIT_USAGE)
        }
        Err(error) => {
            print_runtime_error(CommandName::Init, generated_at, &error.to_string(), json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn print_prewarm(args: PrewarmArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let outcome = prewarm_project(PrewarmOptions {
        db_path: metadata_db_path(),
        project_path: resolve_explicit_path(args.project_path),
        approve_setup: args.approve_setup,
        trigger: if args.approve_setup {
            "cli-approved-setup".to_string()
        } else {
            "cli-setup".to_string()
        },
        generated_at: generated_at.clone(),
    });

    match outcome {
        Ok(outcome) if json => {
            print_json(&PrewarmCommandOutput {
                contract_version: CONTRACT_VERSION,
                command: CommandName::Prewarm,
                generated_at,
                outcome: PrewarmCommandOutcome {
                    workspace_id: outcome.workspace_id,
                    project_id: outcome.project_id,
                    project_path: outcome.project_path,
                    state: match outcome.state {
                        bowline_local::setup::PrewarmState::Hot => PrewarmCommandState::Hot,
                        bowline_local::setup::PrewarmState::SetupBlocked => {
                            PrewarmCommandState::SetupBlocked
                        }
                        bowline_local::setup::PrewarmState::NoSetupNeeded => {
                            PrewarmCommandState::NoSetupNeeded
                        }
                    },
                    receipt_ids: outcome.receipt_ids,
                    redacted_summary: outcome.redacted_summary,
                },
            });
            ExitCode::SUCCESS
        }
        Ok(outcome) => {
            println!("Prewarm {:?}: {}", outcome.state, outcome.redacted_summary);
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_prewarm_error(error, generated_at, json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn print_setup(args: SetupArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let project_path = args.project_path.unwrap_or_else(current_dir_string);
    let mut approve_setup = args.yes;

    loop {
        let outcome = prewarm_project(PrewarmOptions {
            db_path: metadata_db_path(),
            project_path: resolve_explicit_path(project_path.clone()),
            approve_setup,
            trigger: if approve_setup {
                "cli-approved-setup".to_string()
            } else {
                "cli-setup".to_string()
            },
            generated_at: generated_at.clone(),
        });

        match outcome {
            Ok(outcome)
                if !json
                    && !approve_setup
                    && outcome.state == bowline_local::setup::PrewarmState::SetupBlocked =>
            {
                println!("Setup needs approval: {}", outcome.redacted_summary);
                if !confirm_return("Approve setup?") {
                    return ExitCode::SUCCESS;
                }
                approve_setup = true;
            }
            Ok(outcome) if json => {
                print_json(&PrewarmCommandOutput {
                    contract_version: CONTRACT_VERSION,
                    command: CommandName::Setup,
                    generated_at,
                    outcome: PrewarmCommandOutcome {
                        workspace_id: outcome.workspace_id,
                        project_id: outcome.project_id,
                        project_path: outcome.project_path,
                        state: match outcome.state {
                            bowline_local::setup::PrewarmState::Hot => PrewarmCommandState::Hot,
                            bowline_local::setup::PrewarmState::SetupBlocked => {
                                PrewarmCommandState::SetupBlocked
                            }
                            bowline_local::setup::PrewarmState::NoSetupNeeded => {
                                PrewarmCommandState::NoSetupNeeded
                            }
                        },
                        receipt_ids: outcome.receipt_ids,
                        redacted_summary: outcome.redacted_summary,
                    },
                });
                return ExitCode::SUCCESS;
            }
            Ok(outcome) => {
                println!("Setup {:?}: {}", outcome.state, outcome.redacted_summary);
                return ExitCode::SUCCESS;
            }
            Err(error) => {
                print_runtime_error(CommandName::Setup, generated_at, &error.to_string(), json);
                return ExitCode::from(EXIT_RUNTIME);
            }
        }
    }
}

fn print_prewarm_error(error: SetupRunError, generated_at: String, json: bool) {
    print_runtime_error(CommandName::Prewarm, generated_at, &error.to_string(), json);
}

fn attach_first_device_trust_if_available(
    output: &mut bowline_core::commands::InitCommandOutput,
    generated_at: &str,
) -> Option<String> {
    if !runtime::passive_secret_store_probe_allowed() {
        output.next_actions.push(SafeAction {
            label: "Log in before enabling workspace sync".to_string(),
            command: Some("bowline login".to_string()),
        });
        return None;
    }

    let Ok(key_store) = runtime::key_store() else {
        output.next_actions.push(SafeAction {
            label: "Check local secret store before enabling sync".to_string(),
            command: Some("bowline status".to_string()),
        });
        return None;
    };

    match key_store.load_account_tokens() {
        Ok(Some(_tokens)) => {}
        Ok(None) | Err(_)
            if env_account_session_id_present()
                || env_workos_access_token_present()
                || env_control_plane_token_present() => {}
        Ok(None) | Err(_) => {
            output.next_actions.push(SafeAction {
                label: "Log in before enabling workspace sync".to_string(),
                command: Some("bowline login".to_string()),
            });
            return None;
        }
    }

    let Ok(control_plane) = runtime::control_plane() else {
        output.next_actions.push(SafeAction {
            label: "Check control-plane connectivity before enabling sync".to_string(),
            command: Some("bowline status".to_string()),
        });
        return None;
    };

    let _ = control_plane.create_workspace_ref(output.workspace_id.as_str());
    let trust = match control_plane.list_device_trust(output.workspace_id.as_str()) {
        Ok(trust) => trust,
        Err(error) => {
            output.next_actions.push(SafeAction {
                label: format!("Trust setup unavailable: {error}"),
                command: Some("bowline status".to_string()),
            });
            return None;
        }
    };

    let current_device_id = runtime::daemon_device_id(&output.workspace_id);
    if !trust.authorized_devices.is_empty() {
        if trust
            .authorized_devices
            .iter()
            .any(|device| device.device_id == current_device_id.as_str())
        {
            output.next_actions.push(SafeAction {
                label: "Inspect workspace status".to_string(),
                command: Some("bowline status".to_string()),
            });
            return None;
        }
        if let Some(request) = trust
            .pending_requests
            .iter()
            .find(|request| request.device_id == current_device_id.as_str())
        {
            match request.state {
                bowline_control_plane::DeviceRequestState::Approved => {
                    let request_id = DeviceApprovalRequestId::new(request.request_id.clone());
                    match bowline_local::trust::accept_device_grant(
                        &*control_plane,
                        &*key_store,
                        &output.workspace_id,
                        &request_id,
                        &current_device_id,
                    ) {
                        Ok(_) => {
                            output.next_actions.push(SafeAction {
                                label: "Inspect workspace status".to_string(),
                                command: Some("bowline status".to_string()),
                            });
                        }
                        Err(error) => {
                            output.next_actions.push(SafeAction {
                                label: format!("Device grant not accepted: {error}"),
                                command: Some("bowline status".to_string()),
                            });
                        }
                    }
                    return None;
                }
                bowline_control_plane::DeviceRequestState::Pending => {
                    output.next_actions.push(SafeAction {
                        label: format!(
                            "Approve {} with code {} on a trusted device",
                            request.device_name, request.matching_code
                        ),
                        command: Some(format!("bowline approve {}", request.request_id)),
                    });
                    return Some(request.request_id.clone());
                }
                bowline_control_plane::DeviceRequestState::Denied
                | bowline_control_plane::DeviceRequestState::Expired => {}
            }
        }
        match bowline_local::trust::create_device_request(
            &*control_plane,
            &*key_store,
            bowline_local::trust::DeviceRequestOptions {
                workspace_id: output.workspace_id.clone(),
                device_id: runtime::device_id(),
                device_name: runtime::device_name(),
                platform: runtime::platform(),
                host: None,
                root: Some(output.root.clone()),
                generated_at: generated_at.to_string(),
            },
        ) {
            Ok(request) => {
                let request_id = request.request_id.as_str().to_string();
                output.next_actions.push(SafeAction {
                    label: format!(
                        "Approve {} with code {} on a trusted device",
                        request.device_name, request.matching_code
                    ),
                    command: Some(format!("bowline approve {}", request.request_id.as_str())),
                });
                return Some(request_id);
            }
            Err(error) => {
                output.next_actions.push(SafeAction {
                    label: format!("Device approval request not created: {error}"),
                    command: Some("bowline status".to_string()),
                });
            }
        }
        return None;
    }

    match bowline_local::trust::ensure_first_device_trust_root(
        &*control_plane,
        &*key_store,
        output.workspace_id.clone(),
        runtime::device_id(),
        runtime::device_name(),
        runtime::platform(),
        generated_at.to_string(),
    ) {
        Ok(_) => {
            output.next_actions.push(SafeAction {
                label: "Create a Recovery Key".to_string(),
                command: Some("bowline recover create".to_string()),
            });
        }
        Err(error) => {
            output.next_actions.push(SafeAction {
                label: format!("Trust root not created: {error}"),
                command: Some("bowline status".to_string()),
            });
        }
    }
    None
}

fn wait_for_device_grant(
    workspace_id: WorkspaceId,
    request_id: String,
    generated_at: String,
) -> ExitCode {
    println!("Waiting for approval. On a trusted device, run `bowline approve`.");
    let control_plane = match runtime::control_plane() {
        Ok(control_plane) => control_plane,
        Err(error) => {
            print_runtime_error(CommandName::Login, generated_at, &error, false);
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    let key_store = match runtime::key_store() {
        Ok(key_store) => key_store,
        Err(error) => {
            print_runtime_error(CommandName::Login, generated_at, &error, false);
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    let request_id = DeviceApprovalRequestId::new(request_id);
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        match bowline_local::trust::accept_device_grant(
            &*control_plane,
            &*key_store,
            &workspace_id,
            &request_id,
            &runtime::device_id(),
        ) {
            Ok(_) => {
                println!("Device approved. Workspace is ready.");
                return ExitCode::SUCCESS;
            }
            Err(bowline_local::trust::TrustError::MissingPendingRequest(_)) => {
                if Instant::now() >= deadline {
                    print_runtime_error(
                        CommandName::Login,
                        generated_at,
                        "timed out waiting for device approval; run `bowline login --no-poll` to leave the request pending",
                        false,
                    );
                    return ExitCode::from(EXIT_RUNTIME);
                }
                thread::sleep(Duration::from_secs(2));
            }
            Err(error) => {
                print_runtime_error(CommandName::Login, generated_at, &error.to_string(), false);
                return ExitCode::from(EXIT_RUNTIME);
            }
        }
    }
}

fn env_workos_access_token_present() -> bool {
    env::var("BOWLINE_WORKOS_ACCESS_TOKEN")
        .ok()
        .is_some_and(|value| !value.is_empty())
}

fn env_account_session_id_present() -> bool {
    env::var("BOWLINE_ACCOUNT_SESSION_ID")
        .ok()
        .is_some_and(|value| !value.is_empty())
}

fn env_control_plane_token_present() -> bool {
    env::var("BOWLINE_CONTROL_PLANE_TOKEN")
        .ok()
        .is_some_and(|value| !value.is_empty())
}

fn print_devices(args: devices::DevicesArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    match devices::run(args, generated_at.clone()) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", render_devices_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(CommandName::Devices, generated_at, &error, json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn print_approve(args: ApproveArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let request_id = match args.request_id {
        Some(request_id) => request_id,
        None => match devices::pending_requests() {
            Ok(requests) if requests.is_empty() => {
                return print_approve_no_pending(json, generated_at);
            }
            Ok(requests) if requests.len() == 1 => {
                let request = &requests[0];
                if json && !args.yes {
                    print_command_usage_error(
                        CommandUsageError {
                            command: CommandName::Approve,
                            code: "request_required",
                            message: "JSON approval requires an explicit request id or --yes."
                                .to_string(),
                            next_actions: vec![SafeAction {
                                label: format!("Approve {}", request.device_name),
                                command: Some(format!(
                                    "bowline approve {} --yes",
                                    request.request_id.as_str()
                                )),
                            }],
                        },
                        generated_at,
                        true,
                    );
                    return ExitCode::from(EXIT_USAGE);
                }
                if !json && !args.yes {
                    println!(
                        "Approve {}? Matching code: {}",
                        request.device_name, request.matching_code
                    );
                    if !confirm_return("Approve?") {
                        return ExitCode::SUCCESS;
                    }
                }
                request.request_id.as_str().to_string()
            }
            Ok(requests) => {
                if json {
                    print_command_usage_error(
                        CommandUsageError {
                            command: CommandName::Approve,
                            code: "multiple_pending_devices",
                            message: "Multiple devices are waiting for approval.".to_string(),
                            next_actions: requests
                                .iter()
                                .map(|request| SafeAction {
                                    label: format!("Approve {}", request.device_name),
                                    command: Some(format!(
                                        "bowline approve {}",
                                        request.request_id.as_str()
                                    )),
                                })
                                .collect(),
                        },
                        generated_at,
                        json,
                    );
                } else {
                    println!("Multiple devices are waiting for approval:");
                    for request in requests {
                        println!(
                            "  {}  {}  {}",
                            request.request_id.as_str(),
                            request.device_name,
                            request.matching_code
                        );
                    }
                    println!("Run `bowline approve <request>`.");
                }
                return ExitCode::from(EXIT_USAGE);
            }
            Err(error) => {
                print_runtime_error(CommandName::Approve, generated_at, &error, json);
                return ExitCode::from(EXIT_RUNTIME);
            }
        },
    };

    match devices::approve(request_id, generated_at.clone()) {
        Ok(mut output) if json => {
            output.command = CommandName::Approve;
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(mut output) => {
            output.command = CommandName::Approve;
            print!("{}", render_devices_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(CommandName::Approve, generated_at, &error, json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn print_approve_no_pending(json: bool, generated_at: String) -> ExitCode {
    if json {
        print_command_usage_error(
            CommandUsageError {
                command: CommandName::Approve,
                code: "no_pending_device",
                message: "No device is waiting for approval.".to_string(),
                next_actions: vec![SafeAction {
                    label: "Inspect workspace status".to_string(),
                    command: Some("bowline status".to_string()),
                }],
            },
            generated_at,
            true,
        );
    } else {
        println!("No device is waiting for approval.\nNext: bowline status");
    }
    ExitCode::from(approve_no_pending_exit_code(json))
}

fn approve_no_pending_exit_code(json: bool) -> u8 {
    if json { EXIT_USAGE } else { 0 }
}

fn print_revoke(args: RevokeArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    match devices::run(
        devices::DevicesArgs::Revoke {
            device_id: args.device_id,
        },
        generated_at.clone(),
    ) {
        Ok(mut output) if json => {
            output.command = CommandName::Revoke;
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(mut output) => {
            output.command = CommandName::Revoke;
            print!("{}", render_devices_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(CommandName::Revoke, generated_at, &error, json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn print_recovery(args: recovery::RecoveryArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    match recovery::run(args, generated_at.clone()) {
        Ok(output) if json => {
            print_json(&output.output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", render_recovery_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(CommandName::Recover, generated_at, &error, json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn print_resolve(args: resolve::ResolveArgs, json: bool, socket: &Path) -> ExitCode {
    let generated_at = generated_at();
    let use_tui = args.tui;
    let args = resolve::ResolveArgs {
        project_or_path: resolve_explicit_path(args.project_or_path),
        ..args
    };
    let output = resolve::run(args, generated_at);

    let command_failed = output.command_failed;
    if json {
        print_json(&output);
    } else if use_tui && io::stdin().is_terminal() && io::stdout().is_terminal() {
        let model = surface::tui::TuiModel::from_resolve(
            output.status.summary.clone(),
            surface::tui::TuiTone::from_status_label(output.status.level),
            output
                .available_actions
                .iter()
                .map(|action| surface::tui::TuiAction {
                    label: action.label.clone(),
                    command: action.command.clone(),
                    mutates: action
                        .command
                        .as_deref()
                        .map(|command| {
                            command.contains(" --accept ") || command.contains(" --reject ")
                        })
                        .unwrap_or(false),
                })
                .collect(),
            output
                .conflicts
                .iter()
                .map(|conflict| {
                    if conflict.contains_secrets {
                        format!(
                            "{}: secret-bearing conflict at {}",
                            conflict.id, conflict.bundle_path
                        )
                    } else {
                        format!("{}: {}", conflict.id, conflict.affected_files.join(", "))
                    }
                })
                .collect(),
        );
        match surface::tui::run_app(model) {
            Ok(Some(command)) => return run_confirmed_tui_command(&command, socket),
            Ok(None) => {}
            Err(error) => {
                print_runtime_error(
                    CommandName::Resolve,
                    output.generated_at.clone(),
                    &error.to_string(),
                    false,
                );
                return ExitCode::from(EXIT_RUNTIME);
            }
        }
    } else {
        let human = resolve::render_human(&output);
        print!("{human}");
    }

    if command_failed {
        return ExitCode::from(EXIT_RUNTIME);
    }

    ExitCode::SUCCESS
}

fn print_status(args: StatusArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let options = StatusOptions {
        db_path: metadata_db_path(),
        requested_path: requested_path(args.path),
        workspace_scope: args.workspace,
        generated_at: generated_at.clone(),
    };

    if args.watch {
        return print_status_watch(options, generated_at, json);
    }

    match compose_status_for_cli(options) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            let human = bowline_local::status::render_status_human(&output);
            print!("{human}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(CommandName::Status, generated_at, &error.to_string(), json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn print_actions(args: ActionsArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let options = StatusOptions {
        db_path: metadata_db_path(),
        requested_path: requested_path(args.path),
        workspace_scope: args.workspace,
        generated_at: generated_at.clone(),
    };
    match compose_status_for_cli(options) {
        Ok(output) if json => {
            let output = surface::actions::from_status(&output);
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            let output = surface::actions::from_status(&output);
            let human = surface::human::render_actions(&output);
            write_human_or_exit(CommandName::Actions, generated_at, &human)
        }
        Err(error) => {
            print_runtime_error(CommandName::Actions, generated_at, &error.to_string(), json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn print_tui(args: TuiArgs, json: bool, socket: &Path) -> ExitCode {
    let generated_at = generated_at();
    if json {
        print_command_usage_error(
            CommandUsageError {
                command: CommandName::Tui,
                code: "usage_error",
                message: "bowline tui is an interactive command; use `bowline status --json`"
                    .to_string(),
                next_actions: vec![SafeAction {
                    label: "Inspect status as JSON".to_string(),
                    command: Some("bowline status --json".to_string()),
                }],
            },
            generated_at,
            true,
        );
        return ExitCode::from(EXIT_USAGE);
    }
    let options = StatusOptions {
        db_path: metadata_db_path(),
        requested_path: requested_path(args.path),
        workspace_scope: false,
        generated_at: generated_at.clone(),
    };
    match compose_status_for_cli(options) {
        Ok(output) if !io::stdin().is_terminal() || !io::stdout().is_terminal() => {
            let output = surface::actions::from_status(&output);
            let human = surface::human::render_actions(&output);
            write_human_or_exit(CommandName::Tui, generated_at, &human)
        }
        Ok(output) => {
            let model =
                surface::tui::TuiModel::from_actions(&surface::actions::from_status(&output));
            match surface::tui::run_app(model) {
                Ok(Some(command)) => run_confirmed_tui_command(&command, socket),
                Ok(None) => ExitCode::SUCCESS,
                Err(error) => {
                    print_runtime_error(CommandName::Tui, generated_at, &error.to_string(), false);
                    ExitCode::from(EXIT_RUNTIME)
                }
            }
        }
        Err(error) => {
            print_runtime_error(CommandName::Tui, generated_at, &error.to_string(), json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn compose_status_for_cli(
    options: StatusOptions,
) -> Result<StatusCommandOutput, bowline_local::status::LocalStatusError> {
    let mut output = bowline_local::status::compose_status(options)?;
    attach_device_status_if_available(&mut output);
    abbreviate_status_requested_path(&mut output);
    Ok(output)
}

fn print_search(args: SearchArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let offset = args.cursor.unwrap_or(0);
    let page_limit = args.limit;
    let options = bowline_local::search::SearchCommandOptions {
        db_path: metadata_db_path(),
        query: args.query,
        requested_path: requested_path(args.path),
        path_prefix: args.path_prefix,
        generated_at: generated_at.clone(),
        limit: page_limit,
        project_identity: None,
    };
    match bowline_local::search::search_workspace_page(options, offset) {
        Ok(mut output) if json => {
            page_search_output(&mut output, offset, page_limit);
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(mut output) => {
            page_search_output(&mut output, offset, page_limit);
            print!("{}", render_search_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(CommandName::Search, generated_at, &error.to_string(), json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn print_symbols(args: SymbolsArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let offset = args.cursor.unwrap_or(0);
    let page_limit = args.limit;
    let options = bowline_local::symbols::SymbolCommandOptions {
        db_path: metadata_db_path(),
        query: args.query,
        requested_path: requested_path(args.path),
        path_prefix: args.path_prefix,
        generated_at: generated_at.clone(),
        limit: page_limit,
        project_identity: None,
    };
    match bowline_local::symbols::lookup_symbols_page(options, offset) {
        Ok(mut output) if json => {
            page_symbol_output(&mut output, offset, page_limit);
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(mut output) => {
            page_symbol_output(&mut output, offset, page_limit);
            print!("{}", render_symbols_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(CommandName::Symbols, generated_at, &error.to_string(), json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn page_search_output(
    output: &mut bowline_core::commands::SearchCommandOutput,
    offset: usize,
    limit: usize,
) {
    let previous_truncated = output.truncated;
    let mut results = std::mem::take(&mut output.results);
    let has_more = previous_truncated || results.len() > limit;
    results.truncate(limit);
    output.results = results;
    output.truncated = has_more;
    output.next_cursor = next_exploration_cursor(offset, limit, has_more);
}

fn page_symbol_output(
    output: &mut bowline_core::commands::SymbolCommandOutput,
    offset: usize,
    limit: usize,
) {
    let previous_truncated = output.truncated;
    let mut symbols = std::mem::take(&mut output.symbols);
    let has_more = previous_truncated || symbols.len() > limit;
    symbols.truncate(limit);
    output.symbols = symbols;
    output.truncated = has_more;
    output.next_cursor = next_exploration_cursor(offset, limit, has_more);
}

fn next_exploration_cursor(offset: usize, limit: usize, has_more: bool) -> Option<String> {
    if !has_more {
        return None;
    }
    let next_offset = offset.saturating_add(limit);
    (next_offset <= MAX_EXPLORATION_CURSOR_OFFSET).then(|| format!("v1:{next_offset}"))
}

fn attach_device_status_if_available(output: &mut StatusCommandOutput) {
    if !runtime::passive_secret_store_probe_allowed() {
        return;
    }

    let Ok(key_store) = runtime::key_store() else {
        return;
    };
    if !matches!(key_store.load_account_tokens(), Ok(Some(_))) {
        return;
    }
    let Ok(control_plane) = runtime::control_plane() else {
        return;
    };
    let Ok(trust) = control_plane.list_device_trust(output.workspace_id.as_str()) else {
        return;
    };

    let local_device_id = runtime::daemon_device_id(&output.workspace_id);
    let local_id = local_device_id.as_str();
    if let Some(revoked) = trust
        .revoked_devices
        .iter()
        .find(|device| device.device_id == local_id)
    {
        output.status.level = StatusLevel::Limited;
        output.status.attention_items.push(format!(
            "This device was revoked from workspace {}.",
            output.workspace_id.as_str()
        ));
        let item = device_status_item(
            output,
            StatusSubjectKind::Device,
            revoked.device_id.as_str(),
            Some(DeviceId::new(revoked.device_id.clone())),
            format!(
                "This device is revoked; future sync and trust operations are blocked. Reason: {}",
                revoked.reason
            ),
        );
        output.items.push(item);
        output.next_actions.push(SafeAction {
            label: "Inspect workspace status".to_string(),
            command: Some("bowline status".to_string()),
        });
        return;
    }

    if let Some(device) = trust
        .authorized_devices
        .iter()
        .find(|device| device.device_id == local_id)
    {
        let item = device_status_item(
            output,
            StatusSubjectKind::Device,
            device.device_id.as_str(),
            Some(DeviceId::new(device.device_id.clone())),
            trusted_device_summary(device.device_id.as_str(), device.device_name.as_str()),
        );
        output.items.push(item);
    } else if let Some(request) = trust
        .pending_requests
        .iter()
        .find(|request| request.device_id == local_id)
    {
        if output.status.level == StatusLevel::Healthy {
            output.status.level = StatusLevel::Limited;
        }
        output
            .status
            .attention_items
            .push("This device is waiting for approval before it can sync.".to_string());
        let item = device_status_item(
            output,
            StatusSubjectKind::DeviceApprovalRequest,
            request.request_id.as_str(),
            Some(DeviceId::new(request.device_id.clone())),
            "This device has a pending approval request.".to_string(),
        );
        output.items.push(item);
    } else if !trust.authorized_devices.is_empty() {
        if output.status.level == StatusLevel::Healthy {
            output.status.level = StatusLevel::Limited;
        }
        output
            .status
            .attention_items
            .push("This device is not trusted for the workspace yet.".to_string());
        let item = device_status_item(
            output,
            StatusSubjectKind::Device,
            local_device_id.as_str(),
            Some(local_device_id.clone()),
            "Run `bowline login` to request workspace trust.".to_string(),
        );
        output.items.push(item);
    }

    if !trust.pending_requests.is_empty() {
        if output.status.level == StatusLevel::Healthy {
            output.status.level = StatusLevel::Attention;
        }
        output.status.attention_items.push(format!(
            "{} device approval request(s) are waiting.",
            trust.pending_requests.len()
        ));
        let pending_items = trust
            .pending_requests
            .into_iter()
            .map(|request| {
                output.next_actions.push(SafeAction {
                    label: format!("Approve {}", request.device_name),
                    command: Some(format!("bowline approve {}", request.request_id)),
                });
                device_status_item(
                    output,
                    StatusSubjectKind::DeviceApprovalRequest,
                    request.request_id.as_str(),
                    Some(DeviceId::new(request.device_id.clone())),
                    format!(
                        "{} is waiting for approval with matching code {}.",
                        request.device_name, request.matching_code
                    ),
                )
            })
            .collect::<Vec<_>>();
        output.items.extend(pending_items);
        output.next_actions.push(SafeAction {
            label: "Review workspace status".to_string(),
            command: Some("bowline status".to_string()),
        });
    }
}

fn trusted_device_summary(device_id: &str, device_name: &str) -> String {
    if device_name == device_id {
        return format!("This device is trusted as {device_id}.");
    }
    format!("This device is trusted as {device_id} ({device_name}).")
}

fn device_status_item(
    output: &StatusCommandOutput,
    subject_kind: StatusSubjectKind,
    subject_id: impl Into<String>,
    device_id: Option<DeviceId>,
    summary: String,
) -> StatusItem {
    StatusItem {
        kind: StatusItemKind::Device,
        summary,
        subject: Some(StatusSubject {
            kind: subject_kind,
            id: subject_id.into(),
            path: None,
        }),
        path: None,
        classification: None,
        mode: None,
        access: Vec::new(),
        event_id: None,
        event_name: None,
        device_id,
        lease_id: None,
        project_id: output.project_id.clone(),
        snapshot_id: None,
        policy_version: None,
        env_record_id: None,
    }
}

fn print_status_watch(options: StatusOptions, generated_at: String, json: bool) -> ExitCode {
    let mut sequence = 1;
    let mut last_output = None;

    loop {
        let output = match bowline_local::status::compose_status(options.clone()) {
            Ok(mut output) => {
                attach_device_status_if_available(&mut output);
                abbreviate_status_requested_path(&mut output);
                output
            }
            Err(error) => {
                print_runtime_error(CommandName::Status, generated_at, &error.to_string(), json);
                return ExitCode::from(EXIT_RUNTIME);
            }
        };

        if last_output.as_ref() != Some(&output) {
            let frame = status_watch_frame(output.clone(), sequence);
            let write_result = if json {
                write_json_line(&frame)
            } else {
                write_text(&surface::human::render_watch_frame(&frame))
            };
            if let Err(error) = write_result {
                return if error.kind() == io::ErrorKind::BrokenPipe {
                    ExitCode::SUCCESS
                } else {
                    print_runtime_error(
                        CommandName::Status,
                        generated_at,
                        &error.to_string(),
                        json,
                    );
                    ExitCode::from(EXIT_RUNTIME)
                };
            }
            last_output = Some(output);
            sequence += 1;
        }

        thread::sleep(Duration::from_secs(1));
    }
}

fn status_watch_frame(status: StatusCommandOutput, sequence: u64) -> WatchFrame {
    WatchFrame::Status {
        contract_version: CONTRACT_VERSION,
        sequence,
        generated_at: status.generated_at.clone(),
        workspace_id: status.workspace_id.clone(),
        project_id: status.project_id.clone(),
        last_event_id: status.event_watermarks.last_event_id.clone(),
        watermark: status.event_watermarks.clone(),
        status: Box::new(status),
    }
}

fn print_explain(args: ExplainArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let options = ExplainOptions {
        db_path: metadata_db_path(),
        requested_path: resolve_explicit_path(args.path),
        generated_at: generated_at.clone(),
    };

    match bowline_local::explain::compose_explain(options) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", bowline_local::explain::render_explain_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(CommandName::Explain, generated_at, &error.to_string(), json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn print_events(args: EventsArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let options = EventsOptions {
        db_path: metadata_db_path(),
        requested_path: requested_path(args.path),
        workspace_scope: args.workspace,
        generated_at: generated_at.clone(),
        limit: args.limit,
    };

    match bowline_local::status::compose_events(options) {
        Ok(mut output) if json => {
            abbreviate_events_requested_path(&mut output);
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(mut output) => {
            abbreviate_events_requested_path(&mut output);
            print!("{}", bowline_local::status::render_events_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(CommandName::Events, generated_at, &error.to_string(), json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn print_workon(args: work::WorkonArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let project_path = resolve_explicit_path(args.project_path);
    let args = work::WorkonArgs {
        project_path,
        name: args.name,
    };
    match work::run_workon(
        args,
        metadata_db_path(),
        runtime::device_id(),
        generated_at.clone(),
    ) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", work::render_workon_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(CommandName::Workon, generated_at, &error.to_string(), json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn print_work(args: work::WorkListArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    match work::run_list(
        args,
        metadata_db_path(),
        runtime::device_id(),
        generated_at.clone(),
    ) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", work::render_list_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(CommandName::Work, generated_at, &error.to_string(), json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn print_work_diff(args: work::WorkSelectorArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    match work::run_diff(args, metadata_db_path(), generated_at.clone()) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", work::render_diff_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(CommandName::Diff, generated_at, &error.to_string(), json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn print_work_review(args: work::WorkSelectorArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    match work::run_diff(args, metadata_db_path(), generated_at.clone()) {
        Ok(mut output) if json => {
            output.command = CommandName::Review;
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(mut output) => {
            output.command = CommandName::Review;
            print!("{}", work::render_diff_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(CommandName::Review, generated_at, &error.to_string(), json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn print_work_lifecycle(
    command: CommandName,
    args: work::WorkSelectorArgs,
    json: bool,
) -> ExitCode {
    let generated_at = generated_at();
    match work::run_lifecycle(command, args, metadata_db_path(), generated_at.clone()) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", work::render_lifecycle_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(command, generated_at, &error.to_string(), json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn print_work_cleanup(args: work::WorkCleanupArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    match work::run_cleanup(args, metadata_db_path(), generated_at.clone()) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", work::render_cleanup_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(CommandName::Cleanup, generated_at, &error.to_string(), json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn print_agent_lease_create(args: agent::AgentLeaseCreateArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let args = agent::AgentLeaseCreateArgs {
        project_path: resolve_explicit_path(args.project_path),
        task: args.task,
        base: args.base,
        hydrate_budget_bytes: args.hydrate_budget_bytes,
        work_view: args.work_view,
    };
    match agent::run_lease_create(
        args,
        metadata_db_path(),
        runtime::device_id(),
        generated_at.clone(),
    ) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", agent::render_lease_create_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(
                CommandName::AgentStart,
                generated_at,
                &error.to_string(),
                json,
            );
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn print_agent_context(args: agent::AgentLeaseSelectorArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    match agent::run_context(args, metadata_db_path(), generated_at.clone()) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", agent::render_context_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(
                CommandName::AgentContext,
                generated_at,
                &error.to_string(),
                json,
            );
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn print_agent_prompt(args: agent::AgentLeaseSelectorArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    match agent::run_prompt(args, metadata_db_path(), generated_at.clone()) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", agent::render_prompt_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(
                CommandName::AgentPrompt,
                generated_at,
                &error.to_string(),
                json,
            );
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn print_agent_tool_action(
    command: CommandName,
    args: agent::AgentLeaseSelectorArgs,
    json: bool,
) -> ExitCode {
    let generated_at = generated_at();
    let result = match command {
        CommandName::AgentPublish => {
            agent::run_publish(args, metadata_db_path(), generated_at.clone())
        }
        CommandName::AgentComplete => {
            agent::run_complete(args, metadata_db_path(), generated_at.clone())
        }
        _ => unreachable!("unsupported agent tool command"),
    };
    match result {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", agent::render_tool_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(command, generated_at, &error.to_string(), json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn print_agent_budget(args: agent::AgentBudgetArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    match agent::run_budget(args, metadata_db_path(), generated_at.clone()) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", agent::render_budget_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(
                CommandName::AgentBudget,
                generated_at,
                &error.to_string(),
                json,
            );
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn print_bootstrap_ssh(args: bootstrap::BootstrapSshArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let output = bootstrap::run(args, generated_at);
    let success = bootstrap_ssh_succeeded(&output);
    if json {
        print_json(&output);
    } else {
        print!("{}", render_bootstrap_ssh_human(&output));
    }
    if success {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(EXIT_RUNTIME)
    }
}

fn bootstrap_ssh_succeeded(output: &bowline_core::commands::BootstrapSshCommandOutput) -> bool {
    output.trusted
        && output
            .steps
            .iter()
            .all(|step| step.state != bowline_core::commands::BootstrapStepState::Blocked)
}

fn metadata_db_path() -> Option<PathBuf> {
    env::var_os(ENV_METADATA_DB).map(PathBuf::from)
}

fn generated_at() -> String {
    env::var(ENV_GENERATED_AT).unwrap_or_else(|_| {
        time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .expect("UTC timestamp should format")
    })
}

fn requested_path(explicit: Option<String>) -> Option<String> {
    explicit.map(resolve_explicit_path).or_else(|| {
        env::current_dir()
            .ok()
            .map(|path| path.display().to_string())
    })
}

fn resolve_explicit_path(path: String) -> String {
    if path == "~" || path.starts_with("~/") {
        return path;
    }

    let path_buf = PathBuf::from(&path);
    if path_buf.is_absolute() {
        return path;
    }

    env::current_dir()
        .map(|cwd| cwd.join(path_buf).display().to_string())
        .unwrap_or(path)
}

fn abbreviate_status_requested_path(output: &mut StatusCommandOutput) {
    output.requested_path = output
        .requested_path
        .as_deref()
        .map(abbreviate_requested_path);
}

fn abbreviate_events_requested_path(output: &mut EventsCommandOutput) {
    output.requested_path = output
        .requested_path
        .as_deref()
        .map(abbreviate_requested_path);
}

fn abbreviate_requested_path(path: &str) -> String {
    let path_buf = PathBuf::from(path);
    let Some(home) = env::var_os("HOME").map(PathBuf::from) else {
        return path.to_string();
    };
    let Ok(relative) = path_buf.strip_prefix(&home) else {
        return path.to_string();
    };

    if relative.as_os_str().is_empty() {
        return "~".to_string();
    }
    format!("~/{}", relative.display())
}

fn print_json(value: &impl serde::Serialize) {
    println!(
        "{}",
        serde_json::to_string(value).expect("command output should serialize")
    );
}

fn write_json_line(value: &impl serde::Serialize) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, value)?;
    writeln!(stdout)?;
    stdout.flush()
}

fn write_text(text: &str) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(text.as_bytes())?;
    stdout.flush()
}

fn write_human_or_exit(command: CommandName, generated_at: String, text: &str) -> ExitCode {
    match write_text(text) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(error) => {
            print_runtime_error(command, generated_at, &error.to_string(), false);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn run_confirmed_tui_command(command_line: &str, socket: &Path) -> ExitCode {
    let child_args = match confirmed_tui_child_args(command_line, socket) {
        Ok(args) => args,
        Err(error) => {
            print_runtime_error(CommandName::Tui, generated_at(), error, false);
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    let current_exe = match env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            print_runtime_error(CommandName::Tui, generated_at(), &error.to_string(), false);
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    let status = match ProcessCommand::new(current_exe).args(child_args).status() {
        Ok(status) => status,
        Err(error) => {
            print_runtime_error(CommandName::Tui, generated_at(), &error.to_string(), false);
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    match status.code() {
        Some(0) => ExitCode::SUCCESS,
        Some(code) => ExitCode::from(code.try_into().unwrap_or(EXIT_RUNTIME)),
        None => ExitCode::from(EXIT_RUNTIME),
    }
}

fn confirmed_tui_child_args(
    command_line: &str,
    socket: &Path,
) -> Result<Vec<OsString>, &'static str> {
    let words = split_tui_command_line(command_line)?;
    let Some((program, args)) = words.split_first() else {
        return Err("empty TUI action command");
    };
    if program != "bowline" {
        return Err("TUI action commands must start with `bowline`");
    }

    let mut child_args = vec![
        OsString::from("--socket"),
        socket.as_os_str().to_os_string(),
    ];
    child_args.extend(args.iter().map(OsString::from));
    Ok(child_args)
}

fn split_tui_command_line(input: &str) -> Result<Vec<String>, &'static str> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars().peekable();
    let mut in_single_quote = false;

    while let Some(ch) = chars.next() {
        if in_single_quote {
            if ch == '\'' {
                in_single_quote = false;
            } else {
                current.push(ch);
            }
            continue;
        }

        match ch {
            '\'' => in_single_quote = true,
            '\\' => {
                if let Some(next) = chars.next() {
                    current.push(next);
                } else {
                    current.push(ch);
                }
            }
            ch if ch.is_whitespace() => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            ch => current.push(ch),
        }
    }

    if in_single_quote {
        return Err("unterminated quote in TUI action command");
    }
    if !current.is_empty() {
        words.push(current);
    }
    Ok(words)
}

fn render_search_human(output: &bowline_core::commands::SearchCommandOutput) -> String {
    let mut lines = Vec::new();
    lines.push(format!(
        "Search: {} results for `{}` ({})",
        output.results.len(),
        output.query,
        output.index.summary
    ));
    for result in &output.results {
        let line = result
            .line_start
            .map(|line| format!(":{line}"))
            .unwrap_or_default();
        lines.push(format!(
            "  {}{}  score {:.1}",
            result.path, line, result.score
        ));
        if let Some(snippet) = &result.snippet {
            lines.push(format!("    {snippet}"));
        }
    }
    if output.results.is_empty() {
        lines.push("  No indexed matches.".to_string());
    }
    lines.push(String::new());
    lines.join("\n")
}

fn render_symbols_human(output: &bowline_core::commands::SymbolCommandOutput) -> String {
    let mut lines = Vec::new();
    lines.push(format!(
        "Symbols: {} results for `{}` ({})",
        output.symbols.len(),
        output.query,
        output.index.summary
    ));
    for symbol in &output.symbols {
        lines.push(format!(
            "  {}  {:?} {:?}  {}:{}",
            symbol.name, symbol.kind, symbol.language, symbol.path, symbol.line_start
        ));
    }
    if output.symbols.is_empty() {
        lines.push("  No indexed symbols.".to_string());
    }
    lines.push(String::new());
    lines.join("\n")
}

fn render_login_human(output: &bowline_core::commands::LoginCommandOutput) -> String {
    let mut lines = Vec::new();
    match output.account.status {
        bowline_core::devices::AccountLoginStatus::LoginPending => {
            lines.push("Login: waiting for browser approval".to_string());
            if let Some(uri) = &output.account.verification_uri_complete {
                lines.push(format!("Open: {uri}"));
            } else if let Some(uri) = &output.account.verification_uri {
                lines.push(format!("Open: {uri}"));
            }
            if let Some(code) = &output.account.user_code {
                lines.push(format!("Code: {code}"));
            }
        }
        bowline_core::devices::AccountLoginStatus::AccountAuthenticated => {
            lines.push("Login: authenticated".to_string());
            if let Some(account_id) = &output.account.account_id {
                lines.push(format!("Account: {}", account_id.as_str()));
            }
        }
        bowline_core::devices::AccountLoginStatus::Expired => {
            lines.push("Login: expired".to_string());
        }
        bowline_core::devices::AccountLoginStatus::NotLoggedIn => {
            lines.push("Login: not logged in".to_string());
        }
    }
    append_next_actions(&mut lines, &output.next_actions);
    lines.push(String::new());
    lines.join("\n")
}

fn render_init_human(output: &bowline_core::commands::InitCommandOutput) -> String {
    let mut lines = vec![
        format!("Root: {}", output.root),
        "State: observed locally; sync has not started".to_string(),
        format!(
            "Observed: {} repos, {} workspace-sync paths, {} env files, {} generated/dependency paths",
            output.scan_summary.repo_count,
            output.scan_summary.workspace_sync_path_count,
            output.scan_summary.env_file_count,
            output.scan_summary.generated_path_count + output.scan_summary.dependency_path_count,
        ),
    ];
    if output.created_root {
        lines.push("Created root directory.".to_string());
    }
    if !output.non_actions.is_empty() {
        lines.push("Did not:".to_string());
        lines.extend(output.non_actions.iter().map(|item| format!("  {item}")));
    }
    if !output.next_actions.is_empty() {
        lines.push("Suggested actions:".to_string());
        lines.extend(
            output
                .next_actions
                .iter()
                .map(|action| match &action.command {
                    Some(command) => format!("  {}: {command}", action.label),
                    None => format!("  {}", action.label),
                }),
        );
    }
    lines.push(String::new());
    lines.join("\n")
}

fn render_devices_human(output: &bowline_core::commands::DevicesCommandOutput) -> String {
    let mut lines = Vec::new();
    match output.action {
        bowline_core::commands::DeviceCommandAction::List => {
            lines.push(format!(
                "Devices: {} trusted, {} pending, {} revoked",
                output.devices.len(),
                output.pending_requests.len(),
                output.revoked_devices.len()
            ));
            lines.extend(output.devices.iter().map(|device| {
                let marker = if device.is_current_device {
                    " (this device)"
                } else {
                    ""
                };
                format!("  {}: trusted{marker}", device.name)
            }));
            lines.extend(output.pending_requests.iter().map(|request| {
                let state = match request.state {
                    bowline_core::devices::DeviceApprovalRequestState::Pending => {
                        "waiting for approval"
                    }
                    bowline_core::devices::DeviceApprovalRequestState::Approved => {
                        "approved, waiting for acceptance"
                    }
                    bowline_core::devices::DeviceApprovalRequestState::Denied => "denied",
                    bowline_core::devices::DeviceApprovalRequestState::Expired => "expired",
                };
                format!(
                    "  {}: {state}, code {} ({})",
                    request.device_name,
                    request.matching_code,
                    request.request_id.as_str()
                )
            }));
        }
        bowline_core::commands::DeviceCommandAction::Request => {
            if let Some(request) = &output.created_request {
                lines.push(format!("Device request: {}", request.request_id.as_str()));
                lines.push(format!("Code: {}", request.matching_code));
                lines.push("State: waiting for approval on an existing trusted device".to_string());
            } else {
                lines.push("Device request created.".to_string());
            }
        }
        bowline_core::commands::DeviceCommandAction::Approve => {
            if let Some(device) = &output.approved_device {
                lines.push(format!("Approved: {}", device.name));
            } else {
                lines.push("Device approved.".to_string());
            }
        }
        bowline_core::commands::DeviceCommandAction::Accept => {
            if let Some(device) = &output.local_device {
                lines.push(format!("Trusted: {}", device.name));
            } else {
                lines.push("Device grant accepted.".to_string());
            }
        }
        bowline_core::commands::DeviceCommandAction::Deny => {
            lines.push("Device request denied.".to_string());
        }
        bowline_core::commands::DeviceCommandAction::Revoke => {
            if let Some(device) = &output.revoked_device {
                lines.push(format!("Revoked: {}", device.name));
            } else {
                lines.push("Device revoked.".to_string());
            }
        }
    }
    append_next_actions(&mut lines, &output.next_actions);
    lines.push(String::new());
    lines.join("\n")
}

fn render_recovery_human(output: &recovery::RecoveryRunOutput) -> String {
    let mut lines = Vec::new();
    match output.output.action {
        bowline_core::commands::RecoveryCommandAction::Status => {
            lines.push(format!(
                "Recovery Key: {}",
                recovery_lifecycle_label(output.output.recovery_key.lifecycle)
            ));
        }
        bowline_core::commands::RecoveryCommandAction::Create => {
            lines.push("Recovery Key created.".to_string());
            if let Some(words) = &output.generated_words {
                lines.push("Words:".to_string());
                lines.push(words.to_string());
            }
            lines.push("This is the only time bowline prints these words.".to_string());
        }
        bowline_core::commands::RecoveryCommandAction::Verify => {
            lines.push("Recovery Key verified.".to_string());
        }
        bowline_core::commands::RecoveryCommandAction::Rotate => {
            lines.push("Recovery Key rotated.".to_string());
            if let Some(words) = &output.generated_words {
                lines.push("Words:".to_string());
                lines.push(words.to_string());
            }
            lines.push("This is the only time bowline prints these words.".to_string());
        }
        bowline_core::commands::RecoveryCommandAction::Revoke => {
            lines.push("Recovery Key revoked.".to_string());
        }
        bowline_core::commands::RecoveryCommandAction::Use => {
            lines.push("Recovery Key used.".to_string());
        }
    }
    append_next_actions(&mut lines, &output.output.next_actions);
    lines.push(String::new());
    lines.join("\n")
}

fn render_bootstrap_ssh_human(
    output: &bowline_core::commands::BootstrapSshCommandOutput,
) -> String {
    let mut lines = vec![
        format!("Bootstrap SSH: {}:{}", output.host, output.root),
        format!("Trusted: {}", if output.trusted { "yes" } else { "no" }),
    ];
    lines.extend(
        output
            .steps
            .iter()
            .map(|step| format!("  {}: {}", step.name, step.summary)),
    );
    append_next_actions(&mut lines, &output.next_actions);
    lines.push(String::new());
    lines.join("\n")
}

fn recovery_lifecycle_label(
    lifecycle: bowline_core::devices::RecoveryKeyLifecycle,
) -> &'static str {
    match lifecycle {
        bowline_core::devices::RecoveryKeyLifecycle::Missing => "missing",
        bowline_core::devices::RecoveryKeyLifecycle::GeneratedUnverified => "generated, unverified",
        bowline_core::devices::RecoveryKeyLifecycle::Active => "active",
        bowline_core::devices::RecoveryKeyLifecycle::Rotated => "rotated",
        bowline_core::devices::RecoveryKeyLifecycle::Revoked => "revoked",
    }
}

fn append_next_actions(lines: &mut Vec<String>, next_actions: &[SafeAction]) {
    if next_actions.is_empty() {
        return;
    }
    lines.push("Suggested actions:".to_string());
    lines.extend(next_actions.iter().map(|action| match &action.command {
        Some(command) => format!("  {}: {command}", action.label),
        None => format!("  {}", action.label),
    }));
}

fn print_ambiguous_init_root(candidates: Vec<PathBuf>, generated_at: String, json: bool) {
    let roots = candidates
        .iter()
        .map(|path| abbreviate_requested_path(&path.display().to_string()))
        .collect::<Vec<_>>();
    let message = format!(
        "bare bowline login found existing non-~/Code roots; pass an explicit root: {}",
        roots.join(", ")
    );
    let next_actions = roots
        .iter()
        .map(|root| SafeAction {
            label: format!("Log in with {root}"),
            command: Some(format!("bowline login --root {root}")),
        })
        .collect::<Vec<_>>();

    print_command_usage_error(
        CommandUsageError {
            command: CommandName::Init,
            code: "ambiguous_root",
            message,
            next_actions,
        },
        generated_at,
        json,
    );
}

fn print_dev_cloud_spike(args: CloudSpikeArgs, json: bool) -> ExitCode {
    match args.provider {
        CloudSpikeProvider::Fake => match run_fake_cloud_spike() {
            Ok(report) => {
                if json {
                    print_json(&CloudSpikeFakeOutput {
                        ok: true,
                        command: "dev cloud-spike",
                        provider: "fake",
                        workspace_id: &report.workspace_id,
                        starting_version: report.starting_version,
                        advanced_version: report.advanced_version,
                        pack_object_count: report.pack_object_count,
                        source_file_count: report.source_file_count,
                        hydrated_cold_file_byte_len: report.hydrated_cold_file_bytes.len(),
                        stale_ref_detected: report.stale_ref_detected,
                        device_approval_harness_only: report.device_approval_harness_only,
                        event_count: report.event_count,
                    });
                } else {
                    println!(
                        "bowline cloud spike fake: ok ({} pack objects, stale-ref proven)",
                        report.pack_object_count
                    );
                }
                ExitCode::SUCCESS
            }
            Err(error) => {
                print_runtime_error(
                    CommandName::Unknown,
                    generated_at(),
                    &error.to_string(),
                    json,
                );
                ExitCode::from(EXIT_RUNTIME)
            }
        },
        CloudSpikeProvider::Hosted => match skip_hosted_cloud_spike_from_env() {
            Some(skip) => {
                if json {
                    print_json(&CloudSpikeSkipOutput {
                        ok: true,
                        command: "dev cloud-spike",
                        provider: "hosted",
                        skipped: true,
                        missing_env: skip.missing_env,
                    });
                } else {
                    println!(
                        "bowline cloud spike hosted: skipped (missing {})",
                        skip.missing_env.join(", ")
                    );
                }
                ExitCode::SUCCESS
            }
            None => match run_hosted_cloud_spike_from_env() {
                Ok(report) => {
                    if json {
                        print_json(&CloudSpikeFakeOutput {
                            ok: true,
                            command: "dev cloud-spike",
                            provider: "hosted",
                            workspace_id: &report.workspace_id,
                            starting_version: report.starting_version,
                            advanced_version: report.advanced_version,
                            pack_object_count: report.pack_object_count,
                            source_file_count: report.source_file_count,
                            hydrated_cold_file_byte_len: report.hydrated_cold_file_bytes.len(),
                            stale_ref_detected: report.stale_ref_detected,
                            device_approval_harness_only: report.device_approval_harness_only,
                            event_count: report.event_count,
                        });
                    } else {
                        println!(
                            "bowline cloud spike hosted: ok ({} pack objects, stale-ref proven)",
                            report.pack_object_count
                        );
                    }
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    print_runtime_error(
                        CommandName::Unknown,
                        generated_at(),
                        &error.to_string(),
                        json,
                    );
                    ExitCode::from(EXIT_RUNTIME)
                }
            },
        },
    }
}

fn print_usage_error(command: CommandName, code: &str, message: &str, json: bool) {
    if json {
        print_json(&CommandErrorOutput {
            contract_version: CONTRACT_VERSION,
            command,
            generated_at: generated_at(),
            status: CommandErrorStatus::UsageError,
            error: CommandError {
                code: code.to_string(),
                message: message.to_string(),
                recoverability: CommandRecoverability::UserAction,
                remediation: Some(
                    "Run `bowline help --json` or `bowline help <topic> --json`.".to_string(),
                ),
                details: None,
                retry_after_seconds: None,
                correlation_id: None,
            },
            next_actions: vec![SafeAction {
                label: "Inspect CLI help".to_string(),
                command: Some("bowline help --json".to_string()),
            }],
        });
    } else {
        eprintln!("bowline usage error: {message}");
    }
}

fn print_command_usage_error(error: CommandUsageError, generated_at: String, json: bool) {
    if json {
        print_json(&CommandErrorOutput {
            contract_version: CONTRACT_VERSION,
            command: error.command,
            generated_at,
            status: CommandErrorStatus::UsageError,
            error: CommandError {
                code: error.code.to_string(),
                message: error.message,
                recoverability: CommandRecoverability::UserAction,
                remediation: Some(
                    "Inspect command help and retry with valid arguments.".to_string(),
                ),
                details: None,
                retry_after_seconds: None,
                correlation_id: None,
            },
            next_actions: error.next_actions,
        });
    } else {
        eprintln!("bowline usage error: {}", error.message);
    }
}

fn print_runtime_error(command: CommandName, generated_at: String, message: &str, json: bool) {
    if json {
        let output = bowline_local::status::command_error_output(
            command,
            generated_at,
            "runtime_error",
            message,
            CommandRecoverability::Retry,
        );
        print_json(&output);
    } else {
        let command = match command {
            CommandName::Init => "init",
            CommandName::Status => "status",
            CommandName::Explain => "explain",
            CommandName::Events => "events",
            CommandName::DaemonStart => "daemon start",
            CommandName::DaemonStop => "daemon stop",
            CommandName::DaemonInstall => "daemon install",
            CommandName::DaemonRestart => "daemon restart",
            CommandName::DaemonUninstall => "daemon uninstall",
            _ => "command",
        };
        eprintln!("bowline {command} failed: {message}");
    }
}

fn print_unknown_command(command: &str, json: bool) {
    if json {
        print_json(&CommandErrorOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::Unknown,
            generated_at: generated_at(),
            status: CommandErrorStatus::UsageError,
            error: CommandError {
                code: "unknown_command".to_string(),
                message: format!("unknown command `{command}`"),
                recoverability: CommandRecoverability::UserAction,
                remediation: Some(
                    "Run `bowline help --json` to discover supported commands.".to_string(),
                ),
                details: Some(serde_json::json!({ "command": command })),
                retry_after_seconds: None,
                correlation_id: None,
            },
            next_actions: vec![SafeAction {
                label: "List bowline commands".to_string(),
                command: Some("bowline help --json".to_string()),
            }],
        });
    } else {
        eprintln!("bowline unknown command: {command}");
    }
}

fn daemon_command_output(
    command: CommandName,
    generated_at: String,
    socket: &Path,
    state: &str,
    daemon_version: Option<&str>,
    pid: Option<u32>,
    include_protocol: bool,
) -> DaemonCommandOutput {
    DaemonCommandOutput {
        contract_version: CONTRACT_VERSION,
        command,
        generated_at,
        daemon: daemon_process_output(socket, state, daemon_version, pid, include_protocol),
    }
}

fn daemon_process_output(
    socket: &Path,
    state: &str,
    daemon_version: Option<&str>,
    pid: Option<u32>,
    include_protocol: bool,
) -> DaemonProcessOutput {
    DaemonProcessOutput {
        state: state.to_string(),
        socket: socket.display().to_string(),
        protocol: include_protocol.then(|| PROTOCOL.to_string()),
        version: include_protocol.then_some(PROTOCOL_VERSION),
        daemon_version: daemon_version.map(str::to_string),
        pid,
    }
}

fn daemon_service_state_from_status(status: &DaemonServiceStatus) -> DaemonServiceState {
    DaemonServiceState {
        state: status.state.clone(),
        name: None,
        unit_path: status.unit_path.display().to_string(),
        unavailable_because: status.unavailable_because.clone(),
    }
}

fn daemon_service_state_from_outcome(outcome: &DaemonServiceOutcome) -> DaemonServiceState {
    DaemonServiceState {
        state: outcome.state.clone(),
        name: Some(outcome.service_name.clone()),
        unit_path: outcome.unit_path.display().to_string(),
        unavailable_because: None,
    }
}

fn print_daemon_start(socket: &Path, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let workspace_id =
        daemon_workspace_id_for_start().unwrap_or_else(|_| runtime::active_workspace_id());
    match handshake(socket) {
        Ok(handshake) => {
            if handshake_sync_workspace_ready_for_start(&handshake, workspace_id.as_str()) {
                if json {
                    print_json(&daemon_command_output(
                        CommandName::DaemonStart,
                        generated_at.clone(),
                        socket,
                        "running",
                        Some(&handshake.daemon_version),
                        None,
                        true,
                    ));
                } else {
                    println!("bowline daemon: already running");
                }
                return ExitCode::SUCCESS;
            }
            let _ = request_shutdown(socket);
            wait_for_daemon_socket_to_stop(socket, Duration::from_secs(3));
        }
        Err(error) => {
            remove_stale_daemon_socket_after_connect_error(socket, &error);
        }
    }

    match start_daemon_process(socket) {
        Ok(child_id) => {
            if json {
                print_json(&daemon_command_output(
                    CommandName::DaemonStart,
                    generated_at,
                    socket,
                    "starting",
                    None,
                    Some(child_id),
                    false,
                ));
            } else {
                println!("bowline daemon: starting (pid {child_id})");
            }
            ExitCode::SUCCESS
        }
        Err(message) => {
            print_runtime_error(CommandName::DaemonStart, generated_at, &message, json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn remove_stale_daemon_socket_after_connect_error(socket: &Path, error: &io::Error) {
    if error.kind() != io::ErrorKind::ConnectionRefused {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;

        if std::fs::symlink_metadata(socket)
            .map(|metadata| metadata.file_type().is_socket())
            .unwrap_or(false)
        {
            let _ = std::fs::remove_file(socket);
        }
    }
}

fn handshake_sync_workspace_ready_for_start(handshake: &Handshake, workspace_id: &str) -> bool {
    handshake.sync_json.as_deref().is_some_and(|sync| {
        extract_json_string(sync, "workspaceId").as_deref() == Some(workspace_id)
            && !matches!(
                extract_json_string(sync, "state").as_deref(),
                Some("limited" | "degraded")
            )
    })
}

fn wait_for_daemon_socket_to_stop(socket: &Path, timeout: Duration) {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if handshake(socket).is_err() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn print_daemon_stop(socket: &Path, json: bool) -> ExitCode {
    let generated_at = generated_at();
    match request_shutdown(socket) {
        Ok(()) => {
            if json {
                print_json(&daemon_command_output(
                    CommandName::DaemonStop,
                    generated_at,
                    socket,
                    "stopping",
                    None,
                    None,
                    false,
                ));
            } else {
                println!("bowline daemon: stopping");
            }
            ExitCode::SUCCESS
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if json {
                print_json(&daemon_command_output(
                    CommandName::DaemonStop,
                    generated_at,
                    socket,
                    "stopped",
                    None,
                    None,
                    false,
                ));
            } else {
                println!("bowline daemon: stopped");
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(
                CommandName::DaemonStop,
                generated_at,
                &error.to_string(),
                json,
            );
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn print_diagnostics_collect(socket: &Path, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let bundle = diagnostics_bundle_text(socket, &generated_at);
    let redacted = redact_setup_text(&bundle);
    if json {
        let output = DiagnosticsCollectCommandOutput {
            contract_version: CONTRACT_VERSION,
            command: CommandName::DiagnosticsCollect,
            generated_at,
            redaction_rules: redacted.rules,
            bundle: redacted.text,
        };
        print_json(&output);
        return ExitCode::SUCCESS;
    }
    println!("{}", redacted.text);
    if !redacted.rules.is_empty() {
        println!("redaction_rules={}", redacted.rules.join(","));
    }
    ExitCode::SUCCESS
}

fn diagnostics_bundle_text(socket: &Path, generated_at: &str) -> String {
    let db_path = metadata_db_path_or_default();
    let state_root = db_path
        .as_ref()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("unavailable"));
    let db_path = db_path
        .map(|path| path.display().to_string())
        .unwrap_or_else(|error| format!("unavailable:{error}"));
    let service = daemon_service_status(&SystemProcessRunner)
        .map(|status| {
            let unavailable = status
                .unavailable_because
                .map(|message| format!(" unavailable={message}"))
                .unwrap_or_default();
            format!(
                "{} path={}{}",
                status.state,
                status.unit_path.display(),
                unavailable
            )
        })
        .unwrap_or_else(|| "unsupported".to_string());
    [
        "bowline diagnostics".to_string(),
        format!("generated_at={generated_at}"),
        format!("socket={}", socket.display()),
        format!("metadata_db={db_path}"),
        format!(
            "daemon_log={}",
            state_root.join("bowline-daemon.log").display()
        ),
        format!(
            "daemon_stdout={}",
            state_root.join("bowline-daemon.out.log").display()
        ),
        format!(
            "daemon_stderr={}",
            state_root.join("bowline-daemon.err.log").display()
        ),
        format!("service={service}"),
        "project_file_contents=excluded".to_string(),
    ]
    .join("\n")
}

fn print_daemon_service_install(socket: &Path, json: bool) -> ExitCode {
    let generated_at = generated_at();
    match daemon_service_install(socket) {
        Ok(outcome) => {
            print_service_outcome(
                CommandName::DaemonInstall,
                "daemon install",
                &outcome,
                generated_at,
                json,
            );
            ExitCode::SUCCESS
        }
        Err(message) => {
            print_service_error(CommandName::DaemonInstall, "daemon install", &message, json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn print_daemon_service_restart(json: bool) -> ExitCode {
    let generated_at = generated_at();
    match daemon_service_restart() {
        Ok(outcome) => {
            print_service_outcome(
                CommandName::DaemonRestart,
                "daemon restart",
                &outcome,
                generated_at,
                json,
            );
            ExitCode::SUCCESS
        }
        Err(message) => {
            print_service_error(CommandName::DaemonRestart, "daemon restart", &message, json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn print_daemon_service_uninstall(json: bool) -> ExitCode {
    let generated_at = generated_at();
    match daemon_service_uninstall() {
        Ok(outcome) => {
            print_service_outcome(
                CommandName::DaemonUninstall,
                "daemon uninstall",
                &outcome,
                generated_at,
                json,
            );
            ExitCode::SUCCESS
        }
        Err(message) => {
            print_service_error(
                CommandName::DaemonUninstall,
                "daemon uninstall",
                &message,
                json,
            );
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn daemon_service_install(socket: &Path) -> Result<DaemonServiceOutcome, String> {
    if linux_service::current_platform_supported() {
        return daemon_linux_service_options(socket).and_then(|options| {
            linux_service::install_or_update_service(&SystemProcessRunner, &options)
                .map(DaemonServiceOutcome::from)
                .map_err(|error| error.to_string())
        });
    }
    if macos_service::current_platform_supported() {
        return daemon_macos_service_options(socket).and_then(|options| {
            macos_service::install_or_update_service(&SystemProcessRunner, &options)
                .map(DaemonServiceOutcome::from)
                .map_err(|error| error.to_string())
        });
    }
    Err("daemon service commands are available only on Linux and macOS".to_string())
}

fn daemon_service_restart() -> Result<DaemonServiceOutcome, String> {
    if linux_service::current_platform_supported() {
        return daemon_linux_unit_dir().and_then(|unit_dir| {
            linux_service::restart_service(&SystemProcessRunner, &unit_dir)
                .map(DaemonServiceOutcome::from)
                .map_err(|error| error.to_string())
        });
    }
    if macos_service::current_platform_supported() {
        return daemon_macos_service_location().and_then(|(launch_agents_dir, launch_domain)| {
            macos_service::restart_service(&SystemProcessRunner, &launch_agents_dir, &launch_domain)
                .map(DaemonServiceOutcome::from)
                .map_err(|error| error.to_string())
        });
    }
    Err("daemon service commands are available only on Linux and macOS".to_string())
}

fn daemon_service_uninstall() -> Result<DaemonServiceOutcome, String> {
    if linux_service::current_platform_supported() {
        return daemon_linux_unit_dir().and_then(|unit_dir| {
            linux_service::uninstall_service(&SystemProcessRunner, &unit_dir)
                .map(DaemonServiceOutcome::from)
                .map_err(|error| error.to_string())
        });
    }
    if macos_service::current_platform_supported() {
        return daemon_macos_service_location().and_then(|(launch_agents_dir, launch_domain)| {
            macos_service::uninstall_service(
                &SystemProcessRunner,
                &launch_agents_dir,
                &launch_domain,
            )
            .map(DaemonServiceOutcome::from)
            .map_err(|error| error.to_string())
        });
    }
    Err("daemon service commands are available only on Linux and macOS".to_string())
}

fn print_service_outcome(
    command: CommandName,
    command_label: &str,
    outcome: &DaemonServiceOutcome,
    generated_at: String,
    json: bool,
) {
    if json {
        print_json(&DaemonServiceOutput {
            contract_version: CONTRACT_VERSION,
            command,
            generated_at,
            service: daemon_service_state_from_outcome(outcome),
        });
        return;
    }
    println!(
        "bowline {command_label}: {} ({})",
        outcome.state,
        outcome.unit_path.display()
    );
}

fn print_service_error(command: CommandName, command_label: &str, message: &str, json: bool) {
    if json {
        print_json(&CommandErrorOutput {
            contract_version: CONTRACT_VERSION,
            command,
            generated_at: generated_at(),
            status: CommandErrorStatus::Unsupported,
            error: CommandError {
                code: "service_unavailable".to_string(),
                message: message.to_string(),
                recoverability: CommandRecoverability::Unsupported,
                remediation: Some(
                    "Run `bowline daemon status --json` or retry on a supported OS.".to_string(),
                ),
                details: None,
                retry_after_seconds: None,
                correlation_id: None,
            },
            next_actions: vec![SafeAction {
                label: "Inspect daemon status".to_string(),
                command: Some("bowline daemon status --json".to_string()),
            }],
        });
        return;
    }
    eprintln!("bowline {command_label} unavailable: {message}");
}

impl From<linux_service::LinuxServiceOutcome> for DaemonServiceOutcome {
    fn from(outcome: linux_service::LinuxServiceOutcome) -> Self {
        Self {
            service_name: outcome.service_name,
            unit_path: outcome.unit_path,
            state: outcome.state.to_string(),
        }
    }
}

impl From<macos_service::MacosServiceOutcome> for DaemonServiceOutcome {
    fn from(outcome: macos_service::MacosServiceOutcome) -> Self {
        Self {
            service_name: outcome.service_name,
            unit_path: outcome.unit_path,
            state: outcome.state.to_string(),
        }
    }
}

fn start_daemon_process(socket: &Path) -> Result<u32, String> {
    let launch = daemon_launch_config(socket)?;
    let log_path = launch.state_root.join("bowline-daemon.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|error| format!("failed to open daemon log {}: {error}", log_path.display()))?;
    let err = log
        .try_clone()
        .map_err(|error| format!("failed to clone daemon log handle: {error}"))?;
    let mut command = ProcessCommand::new(launch.daemon);
    command
        .envs(persisted_daemon_env(&launch.state_root))
        .arg("serve")
        .arg("--socket")
        .arg(&launch.socket)
        .arg("--sync-root")
        .arg(&launch.root)
        .arg("--sync-state-root")
        .arg(&launch.state_root)
        .arg("--sync-workspace")
        .arg(launch.workspace_id.as_str())
        .arg("--sync-device")
        .arg(launch.device_id.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = command
        .spawn()
        .map_err(|error| format!("failed to start bowline-daemon: {error}"))?;
    Ok(child.id())
}

struct DaemonLaunchConfig {
    state_root: PathBuf,
    workspace_id: bowline_core::ids::WorkspaceId,
    root: PathBuf,
    daemon: PathBuf,
    socket: PathBuf,
    device_id: bowline_core::ids::DeviceId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DaemonServiceStatus {
    state: String,
    unit_path: PathBuf,
    unavailable_because: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DaemonServiceOutcome {
    service_name: String,
    unit_path: PathBuf,
    state: String,
}

fn daemon_launch_config(socket: &Path) -> Result<DaemonLaunchConfig, String> {
    let db_path = metadata_db_path_or_default()?;
    let state_root = db_path
        .parent()
        .ok_or_else(|| "metadata database path has no parent directory".to_string())?
        .to_path_buf();
    let store = MetadataStore::open(&db_path).map_err(|error| error.to_string())?;
    let workspace_id = daemon_workspace_id_for_store(&store)?;
    let root = store
        .accepted_roots(&workspace_id)
        .map_err(|error| error.to_string())?
        .into_iter()
        .next()
        .ok_or_else(|| "no accepted workspace root; run `bowline login` first".to_string())?;
    let root = expand_home_path(&root);
    let daemon = daemon_binary_path()?;
    let device_id = daemon_device_id_for_launch(&state_root, &workspace_id);
    Ok(DaemonLaunchConfig {
        state_root,
        workspace_id,
        root,
        daemon,
        socket: socket.to_path_buf(),
        device_id,
    })
}

fn daemon_linux_service_options(socket: &Path) -> Result<LinuxServiceOptions, String> {
    ensure_linux_service_supported()?;
    let launch = daemon_service_launch_config(socket)?;
    std::fs::create_dir_all(&launch.root).map_err(|error| {
        format!(
            "failed to prepare daemon root {}: {error}",
            launch.root.display()
        )
    })?;
    let unit_dir = daemon_linux_unit_dir()?;
    Ok(LinuxServiceOptions {
        unit_dir,
        config: LinuxServiceConfig {
            daemon: launch.daemon,
            root: launch.root,
            state_root: launch.state_root,
            socket: launch.socket,
            workspace_id: launch.workspace_id.as_str().to_string(),
            device_id: launch.device_id.as_str().to_string(),
        },
    })
}

fn daemon_service_launch_config(socket: &Path) -> Result<DaemonLaunchConfig, String> {
    let db_path = metadata_db_path_or_default()?;
    let store = MetadataStore::open(&db_path).map_err(|error| error.to_string())?;
    daemon_service_launch_config_for_store(socket, &db_path, &store, daemon_binary_path()?)
}

fn daemon_service_launch_config_for_store(
    socket: &Path,
    db_path: &Path,
    store: &MetadataStore,
    daemon: PathBuf,
) -> Result<DaemonLaunchConfig, String> {
    let state_root = db_path
        .parent()
        .ok_or_else(|| "metadata database path has no parent directory".to_string())?
        .to_path_buf();
    let workspace_id = daemon_workspace_id_for_store(store)?;
    let root = store
        .accepted_roots(&workspace_id)
        .map_err(|error| error.to_string())?
        .into_iter()
        .next()
        .map(|root| Ok(expand_home_path(&root)))
        .unwrap_or_else(|| seed_default_daemon_root(store, &workspace_id))?;
    let device_id = daemon_device_id_for_launch(&state_root, &workspace_id);
    Ok(DaemonLaunchConfig {
        state_root,
        workspace_id,
        root,
        daemon,
        socket: socket.to_path_buf(),
        device_id,
    })
}

fn seed_default_daemon_root(
    store: &MetadataStore,
    workspace_id: &bowline_core::ids::WorkspaceId,
) -> Result<PathBuf, String> {
    let now = generated_at();
    store
        .insert_workspace(workspace_id, "Code", &now)
        .map_err(|error| error.to_string())?;
    store
        .insert_root(
            &format!("root_{}", workspace_id.as_str()),
            workspace_id,
            "~/Code",
            &now,
        )
        .map_err(|error| error.to_string())?;
    Ok(default_workspace_root())
}

fn daemon_linux_unit_dir() -> Result<PathBuf, String> {
    ensure_linux_service_supported()?;
    linux_service::default_user_unit_dir().map_err(|error| error.to_string())
}

fn daemon_macos_service_options(socket: &Path) -> Result<MacosServiceOptions, String> {
    ensure_macos_service_supported()?;
    let launch = daemon_service_launch_config(socket)?;
    std::fs::create_dir_all(&launch.root).map_err(|error| {
        format!(
            "failed to prepare daemon root {}: {error}",
            launch.root.display()
        )
    })?;
    let (launch_agents_dir, launch_domain) = daemon_macos_service_location()?;
    Ok(MacosServiceOptions {
        launch_agents_dir,
        launch_domain,
        config: MacosServiceConfig {
            daemon: launch.daemon,
            root: launch.root,
            state_root: launch.state_root,
            socket: launch.socket,
            workspace_id: launch.workspace_id.as_str().to_string(),
            device_id: launch.device_id.as_str().to_string(),
        },
    })
}

fn daemon_macos_service_location() -> Result<(PathBuf, String), String> {
    ensure_macos_service_supported()?;
    let launch_agents_dir =
        macos_service::default_launch_agents_dir().map_err(|error| error.to_string())?;
    let launch_domain =
        macos_service::default_launch_domain().map_err(|error| error.to_string())?;
    Ok((launch_agents_dir, launch_domain))
}

fn ensure_linux_service_supported() -> Result<(), String> {
    if linux_service::current_platform_supported() {
        Ok(())
    } else {
        Err("Linux user service commands are available only on Linux".to_string())
    }
}

fn ensure_macos_service_supported() -> Result<(), String> {
    if macos_service::current_platform_supported() {
        Ok(())
    } else {
        Err("macOS daemon service commands are available only on macOS".to_string())
    }
}

fn persisted_daemon_env(state_root: &Path) -> Vec<(String, String)> {
    let Ok(contents) = std::fs::read_to_string(state_root.join("daemon.env")) else {
        return Vec::new();
    };
    contents
        .lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(key, value)| valid_persisted_daemon_env_key(key) && !value.is_empty())
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

fn daemon_device_id_for_launch(
    state_root: &Path,
    workspace_id: &bowline_core::ids::WorkspaceId,
) -> bowline_core::ids::DeviceId {
    persisted_daemon_device_id_for_workspace(state_root, workspace_id)
        .map(bowline_core::ids::DeviceId::new)
        .unwrap_or_else(|| runtime::daemon_device_id(workspace_id))
}

fn persisted_daemon_device_id_for_workspace(
    state_root: &Path,
    workspace_id: &bowline_core::ids::WorkspaceId,
) -> Option<String> {
    let persisted_workspace_id = persisted_daemon_env_value(state_root, "BOWLINE_WORKSPACE_ID")?;
    if persisted_workspace_id != workspace_id.as_str() {
        return None;
    }
    persisted_daemon_env_value(state_root, "BOWLINE_DEVICE_ID")
}

fn persisted_daemon_env_value(state_root: &Path, name: &str) -> Option<String> {
    persisted_daemon_env(state_root)
        .into_iter()
        .find_map(|(key, value)| (key == name).then_some(value))
}

fn valid_persisted_daemon_env_key(key: &str) -> bool {
    matches!(
        key,
        "CONVEX_URL"
            | "BOWLINE_WORKSPACE_ID"
            | "BOWLINE_DEVICE_ID"
            | "BOWLINE_DEVICE_NAME"
            | "BOWLINE_SECRET_STORE"
            | "BOWLINE_ACCOUNT_SESSION_ID"
            | "BOWLINE_CONTROL_PLANE_TOKEN"
            | "BOWLINE_WORKOS_ACCESS_TOKEN"
            | "BOWLINE_WORKOS_CLIENT_ID"
    )
}

fn daemon_workspace_id_for_start() -> Result<bowline_core::ids::WorkspaceId, String> {
    let db_path = metadata_db_path_or_default()?;
    let store = MetadataStore::open(&db_path).map_err(|error| error.to_string())?;
    daemon_workspace_id_for_store(&store)
}

fn daemon_workspace_id_for_store(
    store: &MetadataStore,
) -> Result<bowline_core::ids::WorkspaceId, String> {
    let active = runtime::active_workspace_id();
    if !store
        .accepted_roots(&active)
        .map_err(|error| error.to_string())?
        .is_empty()
    {
        return Ok(active);
    }
    if std::env::var("BOWLINE_WORKSPACE_ID")
        .ok()
        .is_some_and(|value| !value.is_empty())
    {
        return Ok(active);
    }
    if let Some(workspace) = store
        .current_workspace()
        .map_err(|error| error.to_string())?
        && !store
            .accepted_roots(&workspace.id)
            .map_err(|error| error.to_string())?
            .is_empty()
    {
        return Ok(workspace.id);
    }
    Ok(active)
}

fn metadata_db_path_or_default() -> Result<PathBuf, String> {
    metadata_db_path()
        .or_else(|| default_database_path().ok())
        .ok_or_else(|| "metadata database path is unavailable".to_string())
}

fn daemon_binary_path() -> Result<PathBuf, String> {
    if let Some(path) = env::var_os("BOWLINE_DAEMON_BIN") {
        let path = PathBuf::from(path);
        if !path.as_os_str().is_empty() {
            return Ok(path);
        }
    }
    let current = env::current_exe().map_err(|error| error.to_string())?;
    daemon_binary_path_next_to(&current)
}

fn daemon_binary_path_next_to(current: &Path) -> Result<PathBuf, String> {
    let daemon_name = if cfg!(windows) {
        "bowline-daemon.exe"
    } else {
        "bowline-daemon"
    };
    let sibling = current.with_file_name(daemon_name);
    if sibling.exists() {
        return validate_daemon_binary_path(sibling);
    }
    if let Some(debug_dir) = current.parent().and_then(Path::parent) {
        let target_debug = debug_dir.join(daemon_name);
        if target_debug.exists() {
            return validate_daemon_binary_path(target_debug);
        }
    }
    validate_daemon_binary_path(sibling)
}

fn validate_daemon_binary_path(daemon: PathBuf) -> Result<PathBuf, String> {
    let metadata = std::fs::metadata(&daemon).map_err(|error| {
        format!(
            "bowline-daemon binary is unavailable at {}: {error}",
            daemon.display()
        )
    })?;
    if !metadata.is_file() {
        return Err(format!(
            "bowline-daemon binary is unavailable at {}: not a file",
            daemon.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(format!(
                "bowline-daemon binary is unavailable at {}: not executable",
                daemon.display()
            ));
        }
    }
    Ok(daemon)
}

fn expand_home_path(path: &str) -> PathBuf {
    if path == "~" {
        return env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}

fn default_workspace_root() -> PathBuf {
    expand_home_path("~/Code")
}

fn print_daemon_status(socket: &Path, json: bool) {
    let service = daemon_service_status(&SystemProcessRunner);
    match handshake(socket) {
        Ok(handshake) => {
            if json {
                println!(
                    "{}",
                    daemon_status_json(
                        socket,
                        "running",
                        Some(&handshake.daemon_version),
                        handshake.sync_json.as_deref(),
                        service.as_ref()
                    )
                );
            } else {
                println!(
                    "bowline daemon: running ({PROTOCOL} v{PROTOCOL_VERSION}, daemon {})",
                    handshake.daemon_version
                );
                print_daemon_service_status_human(service.as_ref());
            }
        }
        Err(_) => {
            if json {
                println!(
                    "{}",
                    daemon_status_json(socket, "stopped", None, None, service.as_ref())
                );
            } else {
                println!("bowline daemon: stopped");
                print_daemon_service_status_human(service.as_ref());
            }
        }
    }
}

fn daemon_status_json(
    socket: &Path,
    state: &str,
    daemon_version: Option<&str>,
    sync_json: Option<&str>,
    service: Option<&DaemonServiceStatus>,
) -> String {
    serde_json::to_string(&DaemonStatusOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::DaemonStatus,
        generated_at: generated_at(),
        daemon: daemon_process_output(socket, state, daemon_version, None, true),
        sync: sync_json.and_then(|sync| serde_json::from_str(sync).ok()),
        service: service.map(daemon_service_state_from_status),
    })
    .expect("daemon status output should serialize")
}

fn daemon_service_status<R>(runner: &R) -> Option<DaemonServiceStatus>
where
    R: ProcessRunner,
{
    if linux_service::current_platform_supported() {
        return daemon_linux_service_status(runner);
    }
    if macos_service::current_platform_supported() {
        return daemon_macos_service_status(runner);
    }
    None
}

fn daemon_linux_service_status<R>(runner: &R) -> Option<DaemonServiceStatus>
where
    R: ProcessRunner,
{
    let unit_dir = match linux_service::default_user_unit_dir() {
        Ok(unit_dir) => unit_dir,
        Err(error) => {
            return Some(DaemonServiceStatus {
                state: "unavailable".to_string(),
                unit_path: PathBuf::from(linux_service::SERVICE_NAME),
                unavailable_because: Some(error.to_string()),
            });
        }
    };
    match linux_service::service_status(runner, &unit_dir) {
        Ok(outcome) => Some(DaemonServiceStatus {
            state: outcome.state.to_string(),
            unit_path: outcome.unit_path,
            unavailable_because: None,
        }),
        Err(error) => Some(DaemonServiceStatus {
            state: "unavailable".to_string(),
            unit_path: linux_service::unit_path(&unit_dir),
            unavailable_because: Some(error.to_string()),
        }),
    }
}

fn daemon_macos_service_status<R>(runner: &R) -> Option<DaemonServiceStatus>
where
    R: ProcessRunner,
{
    let (launch_agents_dir, launch_domain) = match daemon_macos_service_location() {
        Ok(location) => location,
        Err(error) => {
            return Some(DaemonServiceStatus {
                state: "unavailable".to_string(),
                unit_path: PathBuf::from(macos_service::PLIST_NAME),
                unavailable_because: Some(error),
            });
        }
    };
    match macos_service::service_status(runner, &launch_agents_dir, &launch_domain) {
        Ok(outcome) => Some(DaemonServiceStatus {
            state: outcome.state.to_string(),
            unit_path: outcome.unit_path,
            unavailable_because: None,
        }),
        Err(error) => Some(DaemonServiceStatus {
            state: "unavailable".to_string(),
            unit_path: macos_service::plist_path(&launch_agents_dir),
            unavailable_because: Some(error.to_string()),
        }),
    }
}

#[cfg(test)]
fn daemon_service_status_json(status: &DaemonServiceStatus) -> String {
    serde_json::to_string(&daemon_service_state_from_status(status))
        .expect("daemon service status should serialize")
}

fn print_daemon_service_status_human(status: Option<&DaemonServiceStatus>) {
    let Some(status) = status else {
        return;
    };
    match &status.unavailable_because {
        Some(message) => println!(
            "bowline service: unavailable ({}, {})",
            status.unit_path.display(),
            message
        ),
        None => println!(
            "bowline service: {} ({})",
            status.state,
            status.unit_path.display()
        ),
    }
}

fn handshake(socket: &Path) -> io::Result<Handshake> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(DAEMON_HANDSHAKE_TIMEOUT))?;
    stream.set_write_timeout(Some(DAEMON_HANDSHAKE_TIMEOUT))?;
    stream.write_all(
        format!(
            "{{\"type\":\"hello\",\"protocol\":\"{PROTOCOL}\",\"version\":{PROTOCOL_VERSION}}}\n"
        )
        .as_bytes(),
    )?;
    stream.flush()?;

    let response = read_line(&mut stream)?;
    if !response.contains("\"type\":\"hello_ack\"")
        || !response.contains(&format!("\"protocol\":\"{PROTOCOL}\""))
        || !response.contains(&format!("\"version\":{PROTOCOL_VERSION}"))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "daemon handshake response did not match the expected protocol",
        ));
    }

    Ok(Handshake {
        daemon_version: extract_json_string(&response, "daemonVersion")
            .unwrap_or_else(|| "unknown".to_string()),
        sync_json: extract_json_object(&response, "sync"),
    })
}

fn request_shutdown(socket: &Path) -> io::Result<()> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(
        format!(
            "{{\"type\":\"shutdown\",\"protocol\":\"{PROTOCOL}\",\"version\":{PROTOCOL_VERSION}}}\n"
        )
        .as_bytes(),
    )?;
    stream.flush()?;

    let response = read_line(&mut stream)?;
    if !response.contains("\"type\":\"shutdown_ack\"")
        || !response.contains(&format!("\"protocol\":\"{PROTOCOL}\""))
        || !response.contains(&format!("\"version\":{PROTOCOL_VERSION}"))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "daemon shutdown response did not match the expected protocol",
        ));
    }
    Ok(())
}

fn read_line(stream: &mut UnixStream) -> io::Result<String> {
    let mut bytes = Vec::new();
    let mut one = [0_u8; 1];
    loop {
        match stream.read(&mut one) {
            Ok(0) => break,
            Ok(_) if one[0] == b'\n' => break,
            Ok(_) => bytes.push(one[0]),
            Err(error) => return Err(error),
        }
    }
    String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn extract_json_string(input: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let start = input.find(&needle)? + needle.len();
    let mut value = String::new();
    let mut escaped = false;

    for character in input[start..].chars() {
        if escaped {
            value.push(character);
            escaped = false;
            continue;
        }
        match character {
            '\\' => escaped = true,
            '"' => return Some(value),
            _ => value.push(character),
        }
    }

    None
}

fn extract_json_object(input: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":");
    let marker_start = input.find(&needle)?;
    let object_start =
        marker_start + needle.len() + input[marker_start + needle.len()..].find('{')?;
    let mut depth = 0_usize;
    let mut in_string = false;
    let mut escaped = false;

    for (offset, character) in input[object_start..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let end = object_start + offset + character.len_utf8();
                    return Some(input[object_start..end].to_string());
                }
            }
            _ => {}
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use crate::runtime;

    use super::{
        ActionsArgs, Command, DEFAULT_AGENT_HYDRATE_BUDGET_BYTES, DaemonCommand, StatusArgs,
        TuiArgs, agent, approve_no_pending_exit_code, bootstrap::BootstrapSshArgs,
        devices::DevicesArgs, login, parse_args, recovery::RecoveryArgs, redact_setup_text,
        resolve,
    };
    use bowline_core::commands::{AgentLeaseBase, CommandName};
    use std::path::{Path, PathBuf};

    #[test]
    fn parses_global_json_anywhere() {
        let cli = parse_args(["status", "--json"]);

        assert!(cli.json);
        assert_eq!(
            cli.command,
            Command::Status(StatusArgs {
                path: None,
                watch: false,
                workspace: false,
            })
        );
    }

    #[test]
    fn json_login_does_not_poll_before_printing_verification_url() {
        let args = super::login_args_for_output(
            login::LoginArgs {
                root: None,
                no_poll: false,
                headless: false,
            },
            true,
        );

        assert!(args.no_poll);
        assert!(!args.headless);
    }

    #[test]
    fn approve_no_pending_json_uses_usage_exit_code() {
        assert_eq!(approve_no_pending_exit_code(true), super::EXIT_USAGE);
        assert_eq!(approve_no_pending_exit_code(false), 0);
    }

    #[test]
    fn parses_status_watch_workspace() {
        let cli = parse_args(["status", "--watch", "--workspace", "~/Code"]);

        assert_eq!(
            cli.command,
            Command::Status(StatusArgs {
                path: Some("~/Code".to_string()),
                watch: true,
                workspace: true,
            })
        );
    }

    #[test]
    fn parses_agent_lease_create() {
        let cli = parse_args([
            "agent",
            "lease",
            "create",
            "/tmp/project",
            "--task",
            "fix race",
            "--json",
        ]);

        assert!(cli.json);
        assert_eq!(
            cli.command,
            Command::AgentLeaseCreate(agent::AgentLeaseCreateArgs {
                project_path: "/tmp/project".to_string(),
                task: "fix race".to_string(),
                base: AgentLeaseBase::LatestWorkspace,
                hydrate_budget_bytes: DEFAULT_AGENT_HYDRATE_BUDGET_BYTES,
                work_view: false,
            })
        );
    }

    #[test]
    fn parses_agent_lease_create_work_view_opt_in() {
        let cli = parse_args([
            "agent",
            "start",
            "/tmp/project",
            "--task",
            "try router rewrite",
            "--work-view",
        ]);

        assert_eq!(
            cli.command,
            Command::AgentLeaseCreate(agent::AgentLeaseCreateArgs {
                project_path: "/tmp/project".to_string(),
                task: "try router rewrite".to_string(),
                base: AgentLeaseBase::LatestWorkspace,
                hydrate_budget_bytes: DEFAULT_AGENT_HYDRATE_BUDGET_BYTES,
                work_view: true,
            })
        );
    }

    #[test]
    fn parses_bootstrap_ssh() {
        let cli = parse_args(["bootstrap", "ssh", "linux-server-1", "--root", "/tmp/code"]);

        assert_eq!(
            cli.command,
            Command::BootstrapSsh(BootstrapSshArgs {
                host: "linux-server-1".to_string(),
                root: "/tmp/code".to_string(),
                artifact: None,
                project: None,
                task: None,
                agent: None,
            })
        );
    }

    #[test]
    fn parses_bootstrap_ssh_agent_handoff() {
        let cli = parse_args([
            "bootstrap",
            "ssh",
            "linux-server-1",
            "--root",
            "~/Code",
            "--project",
            "foo",
            "--task",
            "implement sync",
            "--agent",
            "codex",
        ]);

        assert_eq!(
            cli.command,
            Command::BootstrapSsh(BootstrapSshArgs {
                host: "linux-server-1".to_string(),
                root: "~/Code".to_string(),
                artifact: None,
                project: Some("foo".to_string()),
                task: Some("implement sync".to_string()),
                agent: Some("codex".to_string()),
            })
        );
    }

    #[test]
    fn parses_connect_explicit_root() {
        let cli = parse_args(["connect", "linux-server-1", "--root", "/tmp/code"]);

        assert_eq!(
            cli.command,
            Command::BootstrapSsh(BootstrapSshArgs {
                host: "linux-server-1".to_string(),
                root: "/tmp/code".to_string(),
                artifact: None,
                project: None,
                task: None,
                agent: None,
            })
        );
    }

    #[test]
    fn legacy_diff_usage_message_matches_invoked_command() {
        let cli = parse_args(["diff"]);
        let Command::CommandUsageError(error) = cli.command else {
            panic!("diff without a selector should return a usage error");
        };

        assert_eq!(error.command, CommandName::Diff);
        assert_eq!(
            error.message,
            "bowline diff requires a work-view id or name"
        );
    }

    #[test]
    fn parses_devices_request_default_and_explicit_root() {
        let default_cli = parse_args(["devices", "request"]);
        assert_eq!(
            default_cli.command,
            Command::Devices(DevicesArgs::Request { root: None })
        );

        let explicit_cli = parse_args(["devices", "request", "--root", "/tmp/code"]);
        assert_eq!(
            explicit_cli.command,
            Command::Devices(DevicesArgs::Request {
                root: Some("/tmp/code".to_string()),
            })
        );
    }

    #[test]
    fn parses_resolve_phase_7_shape() {
        let cli = parse_args([
            "resolve",
            "~/Code/app",
            "--copy-prompt",
            "--agent",
            "codex",
            "--json",
        ]);

        assert!(cli.json);
        assert_eq!(
            cli.command,
            Command::Resolve(resolve::ResolveArgs {
                project_or_path: "~/Code/app".to_string(),
                copy_prompt: true,
                tui: false,
                diff: None,
                agent: Some(resolve::ResolveAgent::Codex),
                decision: None,
            })
        );
    }

    #[test]
    fn parses_actions_and_tui_entrypoints() {
        let actions = parse_args(["actions", "~/Code/app", "--workspace"]);
        assert_eq!(
            actions.command,
            Command::Actions(ActionsArgs {
                path: Some("~/Code/app".to_string()),
                workspace: true,
            })
        );

        let tui = parse_args(["tui", "~/Code/app"]);
        assert_eq!(
            tui.command,
            Command::Tui(TuiArgs {
                path: Some("~/Code/app".to_string()),
            })
        );
    }

    #[test]
    fn parses_resolve_tui_flag() {
        let cli = parse_args(["resolve", "~/Code/app", "--tui"]);
        assert_eq!(
            cli.command,
            Command::Resolve(resolve::ResolveArgs {
                project_or_path: "~/Code/app".to_string(),
                copy_prompt: false,
                tui: true,
                diff: None,
                agent: None,
                decision: None,
            })
        );
    }

    #[test]
    fn splits_tui_action_commands_with_shell_quoted_paths() {
        assert_eq!(
            super::split_tui_command_line("bowline resolve '~/Code/my app' --accept conflict-1"),
            Ok(vec![
                "bowline".to_string(),
                "resolve".to_string(),
                "~/Code/my app".to_string(),
                "--accept".to_string(),
                "conflict-1".to_string(),
            ])
        );
        assert_eq!(
            super::split_tui_command_line("bowline status 'repo'\\''s path'"),
            Ok(vec![
                "bowline".to_string(),
                "status".to_string(),
                "repo's path".to_string(),
            ])
        );
        assert_eq!(
            super::split_tui_command_line("bowline status 'unterminated"),
            Err("unterminated quote in TUI action command")
        );
    }

    #[test]
    fn confirmed_tui_child_args_preserve_socket_override() {
        let args = super::confirmed_tui_child_args(
            "bowline resolve '~/Code/my app' --accept conflict-1",
            std::path::Path::new("/tmp/bowline-review.sock"),
        )
        .expect("command should parse");

        assert_eq!(
            args,
            vec![
                std::ffi::OsString::from("--socket"),
                std::ffi::OsString::from("/tmp/bowline-review.sock"),
                std::ffi::OsString::from("resolve"),
                std::ffi::OsString::from("~/Code/my app"),
                std::ffi::OsString::from("--accept"),
                std::ffi::OsString::from("conflict-1"),
            ]
        );
    }

    #[test]
    fn parses_resolve_accept_reject_as_single_action() {
        let accept = parse_args(["resolve", "~/Code/app", "--accept", "conflict-1"]);

        assert_eq!(
            accept.command,
            Command::Resolve(resolve::ResolveArgs {
                project_or_path: "~/Code/app".to_string(),
                copy_prompt: false,
                tui: false,
                diff: None,
                agent: None,
                decision: Some(resolve::ResolveDecision::Accept("conflict-1".to_string())),
            })
        );

        let diff = parse_args(["resolve", "~/Code/app", "--diff", "conflict-1"]);
        assert_eq!(
            diff.command,
            Command::Resolve(resolve::ResolveArgs {
                project_or_path: "~/Code/app".to_string(),
                copy_prompt: false,
                tui: false,
                diff: Some("conflict-1".to_string()),
                agent: None,
                decision: None,
            })
        );

        let reject = parse_args([
            "resolve",
            "~/Code/app",
            "--accept",
            "conflict-1",
            "--reject",
            "conflict-2",
        ]);

        assert!(matches!(reject.command, Command::UsageError { .. }));
    }

    #[test]
    fn parses_recovery_words_from_stdin_shape_only() {
        let cli = parse_args(["recovery", "verify", "rk_123"]);

        assert_eq!(
            cli.command,
            Command::Recovery(RecoveryArgs::Verify {
                envelope_id: "rk_123".to_string(),
            })
        );
    }

    #[test]
    fn rejects_recovery_words_in_argv() {
        let cli = parse_args(["recovery", "verify", "rk_123", "secret", "words"]);

        assert!(matches!(cli.command, Command::CommandUsageError(_)));
    }

    #[test]
    fn next_exploration_cursor_stops_at_accepted_cap() {
        assert_eq!(
            super::next_exploration_cursor(9_900, 100, true),
            Some("v1:10000".to_string())
        );
        assert_eq!(super::next_exploration_cursor(10_000, 100, true), None);
        assert_eq!(super::next_exploration_cursor(0, 100, false), None);
    }

    #[test]
    fn idempotency_cwd_identity_only_for_relative_paths() {
        assert!(super::path_depends_on_cwd("apps/web"));
        assert!(super::path_depends_on_cwd("."));
        assert!(!super::path_depends_on_cwd("/tmp/project"));
        assert!(!super::path_depends_on_cwd("~/Code/project"));
    }

    #[test]
    fn connect_relative_targets_require_cwd_identity() {
        let base = BootstrapSshArgs {
            host: "linux-server-1".to_string(),
            root: "~/Code".to_string(),
            artifact: None,
            project: None,
            task: None,
            agent: None,
        };
        assert!(!super::command_has_cwd_relative_target(
            &Command::BootstrapSsh(base.clone())
        ));

        let mut relative_root = base.clone();
        relative_root.root = "Code".to_string();
        assert!(super::command_has_cwd_relative_target(
            &Command::BootstrapSsh(relative_root)
        ));

        let mut relative_binary = base.clone();
        relative_binary.artifact = Some("target/release/bowline".to_string());
        assert!(super::command_has_cwd_relative_target(
            &Command::BootstrapSsh(relative_binary)
        ));

        let mut relative_project = base;
        relative_project.project = Some("apps/web".to_string());
        assert!(super::command_has_cwd_relative_target(
            &Command::BootstrapSsh(relative_project)
        ));
    }

    #[test]
    fn recovery_json_omits_one_time_generated_words() {
        let output = super::recovery::RecoveryRunOutput {
            output: bowline_core::commands::RecoveryCommandOutput {
                contract_version: bowline_core::commands::CONTRACT_VERSION,
                command: bowline_core::commands::CommandName::Recovery,
                generated_at: "2026-06-24T12:00:00Z".to_string(),
                action: bowline_core::commands::RecoveryCommandAction::Create,
                workspace_id: Some(bowline_core::ids::WorkspaceId::new("ws_recovery_json")),
                recovery_key: bowline_core::devices::RecoveryKeyState {
                    lifecycle: bowline_core::devices::RecoveryKeyLifecycle::GeneratedUnverified,
                    envelope_id: Some(bowline_core::ids::RecoveryEnvelopeId::new("rk_json")),
                    fingerprint: Some("rkp_json".to_string()),
                    created_at: Some("2026-06-24T12:00:00Z".to_string()),
                    verified_at: None,
                    rotated_at: None,
                    revoked_at: None,
                },
                device_request: None,
                encrypted_grant: None,
                next_actions: Vec::new(),
            },
            generated_words: Some("alpha beta gamma".to_string()),
        };

        let json = serde_json::to_value(&output.output).expect("recovery json output serializes");

        assert!(json.get("generatedWords").is_none());
        assert_eq!(json["action"], "create");
        assert_eq!(json["recoveryKey"]["lifecycle"], "generated-unverified");
    }

    #[test]
    fn devices_list_human_output_includes_pending_matching_code() {
        let workspace_id = bowline_core::ids::WorkspaceId::new("ws_devices");
        let output = bowline_core::commands::DevicesCommandOutput {
            contract_version: bowline_core::commands::CONTRACT_VERSION,
            command: bowline_core::commands::CommandName::Devices,
            generated_at: "2026-06-24T12:00:00Z".to_string(),
            action: bowline_core::commands::DeviceCommandAction::List,
            workspace_id: Some(workspace_id.clone()),
            local_device: None,
            devices: Vec::new(),
            revoked_devices: Vec::new(),
            pending_requests: vec![bowline_core::devices::DeviceApprovalRequest {
                request_id: bowline_core::ids::DeviceApprovalRequestId::new(
                    "device-request:ws_devices:linux",
                ),
                workspace_id: workspace_id.clone(),
                requester_device_id: bowline_core::ids::DeviceId::new("device_linux"),
                device_name: "linux-server-1".to_string(),
                platform: bowline_core::devices::DevicePlatform::Linux,
                device_public_key: bowline_core::devices::PublicDeviceKey::new("age1linux"),
                device_fingerprint: bowline_core::devices::DeviceFingerprint::new("fp_linux"),
                matching_code: "842113".to_string(),
                requested_at: "2026-06-24T12:00:00Z".to_string(),
                expires_at: "2026-06-24T12:10:00Z".to_string(),
                state: bowline_core::devices::DeviceApprovalRequestState::Pending,
                host: Some("linux-server-1".to_string()),
                root: Some("~/Code".to_string()),
            }],
            created_request: None,
            approved_device: None,
            denied_request: None,
            revoked_device: None,
            recovery_key: Some(bowline_core::devices::RecoveryKeyState::missing()),
            next_actions: Vec::new(),
        };

        let human = super::render_devices_human(&output);

        assert!(human.contains("code 842113"));
        assert!(human.contains("device-request:ws_devices:linux"));
    }

    #[test]
    fn device_status_item_uses_explicit_subject_and_device_identity() {
        let output = bowline_core::commands::StatusCommandOutput {
            contract_version: bowline_core::commands::CONTRACT_VERSION,
            command: bowline_core::commands::CommandName::Status,
            generated_at: "2026-06-24T12:00:00Z".to_string(),
            workspace_id: bowline_core::ids::WorkspaceId::new("ws_devices"),
            project_id: Some(bowline_core::ids::ProjectId::new("proj_devices")),
            scope: None,
            requested_path: None,
            resolved_workspace_root: None,
            workspace_summary: None,
            index: None,
            hydration_budget: None,
            hydration_progress: Vec::new(),
            sync_queue: None,
            status: bowline_core::status::WorkspaceStatus::healthy(),
            items: Vec::new(),
            limits: Vec::new(),
            event_watermarks: bowline_core::status::EventWatermarks {
                last_scan_at: None,
                last_event_id: None,
                event_lag_ms: Some(0),
                sync_state: None,
                watcher_state: None,
                network_state: None,
            },
            next_actions: Vec::new(),
        };

        let item = super::device_status_item(
            &output,
            bowline_core::status::StatusSubjectKind::DeviceApprovalRequest,
            "device-request:ws_devices:linux",
            Some(bowline_core::ids::DeviceId::new("device_linux")),
            "linux-server-1 is waiting for approval.".to_string(),
        );

        let subject = item.subject.expect("device status has a subject");
        assert_eq!(
            subject.kind,
            bowline_core::status::StatusSubjectKind::DeviceApprovalRequest
        );
        assert_eq!(subject.id, "device-request:ws_devices:linux");
        assert_eq!(item.device_id.expect("device id").as_str(), "device_linux");
        assert_eq!(
            item.project_id.expect("project id").as_str(),
            "proj_devices"
        );
    }

    #[test]
    fn bootstrap_ssh_success_requires_trusted_remote_and_unblocked_steps() {
        let mut output = bowline_core::commands::BootstrapSshCommandOutput {
            contract_version: bowline_core::commands::CONTRACT_VERSION,
            command: bowline_core::commands::CommandName::BootstrapSsh,
            generated_at: "2026-06-24T12:00:00Z".to_string(),
            workspace_id: Some(bowline_core::ids::WorkspaceId::new("ws_bootstrap")),
            project_id: None,
            host: "linux-server-1".to_string(),
            root: "~/Code".to_string(),
            steps: vec![bowline_core::commands::BootstrapStep {
                name: "trust".to_string(),
                state: bowline_core::commands::BootstrapStepState::Completed,
                summary: "Remote device is trusted.".to_string(),
            }],
            device_request: None,
            authorized_device: None,
            remote_device_fingerprint: None,
            trusted: true,
            secret_store: bowline_core::commands::BootstrapSecretStore::ServerLocal,
            sync: bowline_core::commands::BootstrapSyncState::Ready,
            next_required_phase: None,
            remote_status: bowline_core::status::WorkspaceStatus::healthy(),
            next_actions: Vec::new(),
        };

        assert!(super::bootstrap_ssh_succeeded(&output));

        output.trusted = false;
        assert!(!super::bootstrap_ssh_succeeded(&output));

        output.trusted = true;
        output.steps[0].state = bowline_core::commands::BootstrapStepState::Blocked;
        assert!(!super::bootstrap_ssh_succeeded(&output));

        output.steps[0].name = "sync".to_string();
        assert!(!super::bootstrap_ssh_succeeded(&output));

        output.steps[0].state = bowline_core::commands::BootstrapStepState::Completed;
        assert!(super::bootstrap_ssh_succeeded(&output));
    }

    #[test]
    fn daemon_start_reuses_only_usable_workspace_daemon() {
        let idle = super::Handshake {
            daemon_version: "test".to_string(),
            sync_json: Some(
                r#"{"state":"idle","workspaceId":"ws_code","snapshotId":"snap_1","version":1}"#
                    .to_string(),
            ),
        };
        let limited = super::Handshake {
            daemon_version: "test".to_string(),
            sync_json: Some(
                r#"{"state":"limited","workspaceId":"ws_code","unavailableBecause":"missing token"}"#
                    .to_string(),
            ),
        };
        let degraded = super::Handshake {
            daemon_version: "test".to_string(),
            sync_json: Some(r#"{"state":"degraded","workspaceId":"ws_code"}"#.to_string()),
        };

        assert!(super::handshake_sync_workspace_ready_for_start(
            &idle, "ws_code"
        ));
        assert!(!super::handshake_sync_workspace_ready_for_start(
            &idle, "ws_other"
        ));
        assert!(!super::handshake_sync_workspace_ready_for_start(
            &limited, "ws_code"
        ));
        assert!(!super::handshake_sync_workspace_ready_for_start(
            &degraded, "ws_code"
        ));
    }

    #[test]
    fn daemon_start_removes_socket_only_after_connection_refused() {
        let temp = tempfile_dir("bowline-stale-daemon-socket");
        let socket = temp.join("daemon.sock");
        {
            let _listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind socket");
        }

        assert!(socket.exists());
        super::remove_stale_daemon_socket_after_connect_error(
            &socket,
            &std::io::Error::from(std::io::ErrorKind::TimedOut),
        );
        assert!(socket.exists());
        super::remove_stale_daemon_socket_after_connect_error(
            &socket,
            &std::io::Error::from(std::io::ErrorKind::ConnectionRefused),
        );
        assert!(!socket.exists());

        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn parses_daemon_status_socket() {
        let cli = parse_args([
            "daemon",
            "status",
            "--socket",
            "/tmp/bowline-test.sock",
            "--json",
        ]);

        assert!(cli.json);
        assert_eq!(cli.socket, PathBuf::from("/tmp/bowline-test.sock"));
        assert_eq!(cli.command, Command::Daemon(DaemonCommand::Status));
    }

    #[test]
    fn parses_daemon_service_lifecycle_commands() {
        assert_eq!(
            parse_args(["daemon", "install"]).command,
            Command::Daemon(DaemonCommand::Install)
        );
        assert_eq!(
            parse_args(["daemon", "restart"]).command,
            Command::Daemon(DaemonCommand::Restart)
        );
        assert_eq!(
            parse_args(["daemon", "uninstall"]).command,
            Command::Daemon(DaemonCommand::Uninstall)
        );
    }

    #[test]
    fn parses_diagnostics_collect() {
        assert_eq!(
            parse_args(["diagnostics", "collect"]).command,
            Command::DiagnosticsCollect
        );
        assert!(matches!(
            parse_args(["diagnostics"]).command,
            Command::UsageError { .. }
        ));
    }

    #[test]
    fn diagnostics_redaction_removes_home_paths_and_tokens() {
        let home_db = ["", "home", "theo", ".bowline", "local.sqlite3"].join("/");
        let token = ["SECRET", "1234567890abcdef"].join("_");
        let redacted = redact_setup_text(&format!(
            "metadata_db={home_db} TOKEN_VALUE={token} project_file_contents=excluded"
        ));

        assert!(
            redacted
                .text
                .contains("metadata_db=~/.bowline/local.sqlite3")
        );
        assert!(redacted.text.contains("[redacted]"));
        assert!(!redacted.text.contains(&home_db));
        assert!(!redacted.text.contains(&token));
    }

    #[test]
    fn daemon_service_launch_config_defaults_before_login() {
        let temp = tempfile_dir("bowline-daemon-service-default");
        let db_path = temp.join("state").join("local.sqlite3");
        let store = bowline_local::metadata::MetadataStore::open(&db_path).expect("metadata store");
        let daemon = temp.join("bowline-daemon");

        let launch = super::daemon_service_launch_config_for_store(
            Path::new("/tmp/bowline.sock"),
            &db_path,
            &store,
            daemon.clone(),
        )
        .expect("service launch config");

        assert!(!launch.workspace_id.as_str().is_empty());
        assert_eq!(launch.daemon, daemon);
        assert_eq!(launch.root, super::default_workspace_root());
        assert_eq!(launch.state_root, temp.join("state"));
        assert_eq!(
            store
                .accepted_roots(&launch.workspace_id)
                .expect("accepted roots"),
            vec!["~/Code".to_string()]
        );
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn daemon_launch_uses_persisted_device_id() {
        let temp = tempfile_dir("bowline-daemon-persisted-device");
        let state = temp.join("state");
        let db_path = state.join("local.sqlite3");
        std::fs::create_dir_all(&state).expect("state dir");
        let workspace_id = runtime::active_workspace_id();
        std::fs::write(
            state.join("daemon.env"),
            format!(
                "BOWLINE_WORKSPACE_ID={}\nBOWLINE_DEVICE_ID=device_remote_box\nBOWLINE_WORKOS_REFRESH_TOKEN=stale-refresh\n",
                workspace_id.as_str()
            ),
        )
        .expect("daemon env");
        let store = bowline_local::metadata::MetadataStore::open(&db_path).expect("metadata store");
        let daemon = temp.join("bowline-daemon");

        let launch = super::daemon_service_launch_config_for_store(
            Path::new("/tmp/bowline.sock"),
            &db_path,
            &store,
            daemon,
        )
        .expect("service launch config");

        assert_eq!(launch.device_id.as_str(), "device_remote_box");
        assert_eq!(
            super::persisted_daemon_env_value(&state, "BOWLINE_WORKOS_REFRESH_TOKEN"),
            None
        );
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn persisted_daemon_device_id_is_workspace_bound() {
        let temp = tempfile_dir("bowline-daemon-persisted-device-workspace");
        let state = temp.join("state");
        std::fs::create_dir_all(&state).expect("state dir");
        std::fs::write(
            state.join("daemon.env"),
            "BOWLINE_WORKSPACE_ID=ws_a\nBOWLINE_DEVICE_ID=device_a\n",
        )
        .expect("daemon env");

        assert_eq!(
            super::persisted_daemon_device_id_for_workspace(
                &state,
                &bowline_core::ids::WorkspaceId::new("ws_a")
            )
            .as_deref(),
            Some("device_a")
        );
        assert_eq!(
            super::persisted_daemon_device_id_for_workspace(
                &state,
                &bowline_core::ids::WorkspaceId::new("ws_b")
            ),
            None
        );
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn persisted_daemon_env_excludes_refresh_tokens() {
        let temp = tempfile_dir("bowline-daemon-env-sanitized");
        std::fs::write(
            temp.join("daemon.env"),
            "BOWLINE_ACCOUNT_SESSION_ID=session\nBOWLINE_WORKOS_ACCESS_TOKEN=access\nBOWLINE_WORKOS_REFRESH_TOKEN=refresh\nBOWLINE_DEVICE_ID=device_remote\n",
        )
        .expect("daemon env");

        let env = super::persisted_daemon_env(&temp);

        assert!(env.contains(&(
            "BOWLINE_ACCOUNT_SESSION_ID".to_string(),
            "session".to_string()
        )));
        assert!(env.contains(&(
            "BOWLINE_WORKOS_ACCESS_TOKEN".to_string(),
            "access".to_string()
        )));
        assert!(env.contains(&("BOWLINE_DEVICE_ID".to_string(), "device_remote".to_string())));
        assert!(
            !env.iter()
                .any(|(key, _)| key == "BOWLINE_WORKOS_REFRESH_TOKEN")
        );
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn daemon_binary_path_requires_sibling_daemon() {
        let temp = tempfile_dir("bowline-daemon-missing");
        let error = super::daemon_binary_path_next_to(&temp.join("bowline"))
            .expect_err("missing daemon binary");

        assert!(error.contains("bowline-daemon binary is unavailable"));
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn daemon_binary_path_accepts_executable_sibling() {
        let temp = tempfile_dir("bowline-daemon-present");
        let daemon = temp.join(if cfg!(windows) {
            "bowline-daemon.exe"
        } else {
            "bowline-daemon"
        });
        std::fs::write(&daemon, b"daemon").expect("daemon file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&daemon)
                .expect("daemon metadata")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&daemon, permissions).expect("daemon permissions");
        }

        assert_eq!(
            super::daemon_binary_path_next_to(&temp.join("bowline")).expect("daemon path"),
            daemon
        );
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn daemon_binary_path_accepts_target_debug_fallback() {
        let temp = tempfile_dir("bowline-daemon-target-debug");
        let deps = temp.join("target").join("debug").join("deps");
        std::fs::create_dir_all(&deps).expect("debug deps dir");
        let daemon = temp.join("target").join("debug").join(if cfg!(windows) {
            "bowline-daemon.exe"
        } else {
            "bowline-daemon"
        });
        std::fs::write(&daemon, b"daemon").expect("daemon file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&daemon)
                .expect("daemon metadata")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&daemon, permissions).expect("daemon permissions");
        }

        assert_eq!(
            super::daemon_binary_path_next_to(&deps.join("bowline")).expect("daemon path"),
            daemon
        );
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn daemon_service_status_json_includes_unavailable_reason() {
        let status = super::DaemonServiceStatus {
            state: "unavailable".to_string(),
            unit_path: PathBuf::from("/tmp/bowline.service"),
            unavailable_because: Some("systemd user manager is unavailable".to_string()),
        };

        assert_eq!(
            super::daemon_service_status_json(&status),
            "{\"state\":\"unavailable\",\"unitPath\":\"/tmp/bowline.service\",\"unavailableBecause\":\"systemd user manager is unavailable\"}"
        );
    }

    #[test]
    fn daemon_status_json_keeps_service_top_level() {
        let service = super::DaemonServiceStatus {
            state: "failed".to_string(),
            unit_path: PathBuf::from("/tmp/bowline.service"),
            unavailable_because: None,
        };

        let running: serde_json::Value = serde_json::from_str(&super::daemon_status_json(
            Path::new("/tmp/bowline.sock"),
            "running",
            Some("daemon-test"),
            Some("{\"state\":\"ready\"}"),
            Some(&service),
        ))
        .expect("running status json");
        let stopped: serde_json::Value = serde_json::from_str(&super::daemon_status_json(
            Path::new("/tmp/bowline.sock"),
            "stopped",
            None,
            None,
            Some(&service),
        ))
        .expect("stopped status json");

        assert_eq!(running["daemon"]["state"], "running");
        assert_eq!(running["service"]["state"], "failed");
        assert!(running["daemon"]["service"].is_null());
        assert_eq!(stopped["daemon"]["state"], "stopped");
        assert_eq!(stopped["service"]["state"], "failed");
        assert!(stopped["daemon"]["service"].is_null());
    }

    fn tempfile_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("temp dir");
        path
    }
}
