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
mod cli;
mod daemon;
mod dev_spike;
mod device_commands;
mod devices;
mod dispatch;
mod errors;
mod idempotency;
mod io_helpers;
mod login;
mod login_init;
mod logout;
mod recovery;
mod registry;
mod render;
mod resolve;
mod runtime;
mod service;
mod status_commands;
mod surface;
mod wire;
mod work;
mod work_agent_commands;

#[cfg(test)]
mod lib_daemon_tests;
#[cfg(test)]
mod lib_parse_tests;

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
use cli::*;
use dev_spike::{
    run_fake_cloud_spike, run_hosted_cloud_spike_from_env, skip_hosted_cloud_spike_from_env,
};
use dispatch::run;
use registry::{print_contract, print_help, print_version};
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

fn usage_error(command: CommandName, message: impl Into<String>) -> Command {
    Command::UsageError {
        command,
        message: message.into(),
    }
}

use daemon::*;
use device_commands::*;
use errors::*;
use io_helpers::*;
use login_init::*;
use render::*;
use service::*;
use status_commands::*;
use wire::*;
use work_agent_commands::*;
