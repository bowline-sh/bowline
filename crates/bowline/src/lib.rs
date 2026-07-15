#![deny(unsafe_code)]

use std::any::Any;
use std::ffi::OsString;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, ExitCode};
use std::time::{Duration, Instant};
use std::{env, panic, thread};

mod agent;
mod agent_adapters;
mod bootstrap;
mod cli;
mod command_error_classification;
mod daemon;
mod debug;
mod device_commands;
mod devices;
mod dispatch;
mod errors;
mod handoff_commands;
mod handoff_trust;
mod idempotency;
mod io_helpers;
mod lease;
mod lifecycle;
mod login;
mod login_init;
mod logout;
mod mcp;
mod recovery;
mod registry;
mod render;
mod resolve;
mod runtime;
mod service;
mod status_commands;
mod surface;
mod update;
mod wire;
mod work;
mod work_agent_commands;
mod workspace_root_selection;

#[cfg(test)]
mod handoff_commands_tests;
#[cfg(test)]
mod lib_daemon_tests;
#[cfg(test)]
mod lib_handoff_parse_tests;
#[cfg(test)]
mod lib_parse_tests;

use bowline_core::commands::{
    BoundedOutputControls, CONTRACT_VERSION, CliCommandDescriptor, CliCommandExample,
    CliCommandGroup, CliCommandOption, CliCommandPositional, CliCommandSummary, CommandError,
    CommandErrorOutput, CommandErrorStatus, CommandExitCode, CommandName, CommandRecoverability,
    ContractCommandOutput, ContractFixtureDescriptor, ContractSummaryCommandOutput,
    DaemonCommandOutput, DaemonProcessOutput, DaemonServiceOutput, DaemonServiceState,
    DaemonStatusOutput, DiagnosticsCollectCommandOutput, DryRunCommandOutput, DryRunStatus,
    EventsCommandOutput, HandoffAgent, HelpCommandOutput, ScopedContractCommandOutput,
    SetupCommandOutput, SetupProjectOutcome, SetupProjectOutput, SetupProjectState,
    StatusCommandOutput, UpdateCommandOutput, VersionCommandOutput, WatchFrame,
};
use bowline_core::devices::{AccountLoginState, AccountLoginStatus};
use bowline_core::events::EVENT_SCHEMA_VERSION;
use bowline_core::ids::{DeviceApprovalRequestId, DeviceId, WorkspaceId};
use bowline_core::status::{
    DeviceApprovalAffordance, RepairCommand, StatusFact, StatusFactScope, StatusItem,
    StatusItemKind, StatusSubject, StatusSubjectKind, reduce_status_facts, status_fact_policy,
};
use bowline_local::{
    bootstrap::process::{ProcessRunner, SystemProcessRunner},
    init::{InitOptions, LocalInitError},
    linux_service::{self, LinuxServiceConfig, LinuxServiceOptions},
    macos_service::{self, MacosServiceConfig, MacosServiceOptions},
    metadata::{MetadataStore, default_control_socket_path, default_database_path},
    setup::{ProjectSetupOptions, ProjectSetupState, redact::redact_setup_text, run_project_setup},
    status::{EventsOptions, StatusOptions},
};
use cli::*;
use dispatch::run;
use registry::{print_contract, print_help, print_version};
const PROTOCOL: &str = bowline_daemon_rpc::DAEMON_RPC_PROTOCOL;
const PROTOCOL_VERSION: u32 = bowline_daemon_rpc::DAEMON_RPC_PROTOCOL_VERSION as u32;
const DEFAULT_SOCKET_FALLBACK: &str = ".bowline/runtime/bowline-daemon.sock";
const CLI_VERSION: &str = env!("CARGO_PKG_VERSION");
const PACKAGE_CONTRACT_SOURCE: &str = "packages/contracts/src/index.ts";
const DAEMON_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const ENV_METADATA_DB: &str = "BOWLINE_METADATA_DB";
const ENV_GENERATED_AT: &str = "BOWLINE_GENERATED_AT";
const EXIT_USAGE: CommandExitCode = CommandExitCode::UsageError;
const EXIT_RUNTIME: CommandExitCode = CommandExitCode::RetryableRuntimeError;
pub fn main() -> ExitCode {
    install_panic_hook();
    match panic::catch_unwind(|| {
        let cli = parse_args(env::args().skip(1));
        run(cli)
    }) {
        Ok(code) => code,
        Err(payload) if is_broken_pipe_panic(payload.as_ref()) => ExitCode::SUCCESS,
        Err(payload) => panic::resume_unwind(payload),
    }
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        if is_broken_pipe_panic(info.payload()) {
            return;
        }
        eprintln!(
            "bowline hit an internal error. Run `bowline status --root <path>` and inspect daemon logs; environment values were not printed."
        );
    }));
}

fn is_broken_pipe_panic(payload: &(dyn Any + Send)) -> bool {
    payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .is_some_and(|message| message.contains("Broken pipe"))
}

fn usage_error(command: CommandName, message: impl Into<String>) -> Result<Command, ParseError> {
    Err(ParseError::Usage {
        command,
        message: message.into(),
    })
}

fn selected_workspace_path(selection: WorkspaceSelection) -> Option<String> {
    let root = resolve_explicit_path(selection.root);
    match selection.project {
        Some(project) if !project.is_empty() => {
            if project == "." || project.starts_with("./") {
                current_path_within_root(&root, Some(&project)).or_else(|| {
                    let resolved = resolve_explicit_path(project);
                    Some(
                        std::fs::canonicalize(&resolved)
                            .unwrap_or_else(|_| PathBuf::from(&resolved))
                            .display()
                            .to_string(),
                    )
                })
            } else if project == "~"
                || project.starts_with("~/")
                || Path::new(&project).is_absolute()
            {
                Some(resolve_explicit_path(project))
            } else {
                Some(format!(
                    "{}/{}",
                    root.trim_end_matches('/'),
                    project.trim_start_matches('/')
                ))
            }
        }
        _ => current_path_within_root(&root, None).or(Some(root)),
    }
}

fn current_path_within_root(root: &str, project: Option<&str>) -> Option<String> {
    let root_path = service::expand_home_path(root);
    let comparable_root = std::fs::canonicalize(&root_path).unwrap_or_else(|_| root_path.clone());
    let cwd = env::current_dir().ok()?;
    let candidate = project
        .map(|path| cwd.join(path))
        .map(|path| std::fs::canonicalize(&path).unwrap_or(path))
        .unwrap_or(cwd);
    let relative = candidate.strip_prefix(&comparable_root).ok()?;
    if relative.as_os_str().is_empty() {
        return None;
    }
    Some(root_path.join(relative).display().to_string())
}

use daemon::*;
use debug::*;
use device_commands::*;
use errors::*;
use handoff_commands::*;
use io_helpers::*;
use lifecycle::*;
use login_init::*;
use render::*;
use service::*;
use status_commands::*;
use update::*;
use wire::*;
use work_agent_commands::*;
