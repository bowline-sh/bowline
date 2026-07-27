//! Serde shapes for the daemon CLI's machine output.
//!
//! Every payload is a struct, never a hand-assembled format string, so the
//! shapes cannot drift between the success and failure branches of the same
//! command and cannot emit unescaped values.

use std::path::Path;
use std::process::ExitCode;

use bowline_daemon_rpc::DaemonReachability;

use crate::daemon::EXIT_FAILURE;
use crate::daemon::PROTOCOL;
use crate::daemon::PROTOCOL_VERSION;
use crate::daemon::cli::CONTRACT_VERSION;
use serde::Serialize;

/// What the CLI observed about the daemon behind the socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum DaemonProcessState {
    Running,
    Stopping,
    /// Nothing is listening. Starting a daemon is the correct next step.
    Stopped,
    /// A daemon is running and speaks an incompatible wire generation.
    VersionSkew,
    /// A daemon may be running; this process could not talk to it. Never report
    /// this as `stopped` — it is precisely the case a human must look at.
    Unreachable,
}

impl DaemonProcessState {
    /// Whether this outcome is a normal, actionable-by-nobody state. Anything
    /// else must leave the process with a non-zero exit so scripts and agents
    /// notice.
    pub(super) const fn is_expected(self) -> bool {
        matches!(self, Self::Running | Self::Stopping | Self::Stopped)
    }

    pub(super) fn exit_code(self) -> ExitCode {
        if self.is_expected() {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(EXIT_FAILURE)
        }
    }

    pub(super) fn from_reachability(reachability: &DaemonReachability) -> Self {
        match reachability {
            DaemonReachability::NotRunning => Self::Stopped,
            DaemonReachability::VersionSkew(_) => Self::VersionSkew,
            DaemonReachability::Unreachable => Self::Unreachable,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SocketProtocol {
    pub(super) protocol: &'static str,
    pub(super) version: u32,
}

impl SocketProtocol {
    pub(super) const fn current() -> Self {
        Self {
            protocol: PROTOCOL,
            version: PROTOCOL_VERSION,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DaemonProcess {
    pub(super) state: DaemonProcessState,
    pub(super) socket: String,
    pub(super) protocol: SocketProtocol,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) daemon_version: Option<String>,
    /// The concrete reason the daemon could not be reached, present whenever the
    /// state is not `running`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) unavailable_because: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) remediation: Option<&'static str>,
}

impl DaemonProcess {
    pub(super) fn running(socket: &Path, daemon_version: String) -> Self {
        Self {
            state: DaemonProcessState::Running,
            socket: socket.display().to_string(),
            protocol: SocketProtocol::current(),
            daemon_version: Some(daemon_version),
            unavailable_because: None,
            remediation: None,
        }
    }

    pub(super) fn stopping(socket: &Path) -> Self {
        Self {
            state: DaemonProcessState::Stopping,
            socket: socket.display().to_string(),
            protocol: SocketProtocol::current(),
            daemon_version: None,
            unavailable_because: None,
            remediation: None,
        }
    }

    pub(super) fn unavailable(socket: &Path, reachability: &DaemonReachability) -> Self {
        Self {
            state: DaemonProcessState::from_reachability(reachability),
            socket: socket.display().to_string(),
            protocol: SocketProtocol::current(),
            daemon_version: None,
            unavailable_because: Some(reachability.to_string()),
            remediation: Some(reachability.remediation()),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct HelpOutput {
    pub(super) ok: bool,
    pub(super) command: &'static str,
    pub(super) contract_version: u16,
    pub(super) commands: &'static [&'static str],
    pub(super) socket: SocketProtocol,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct StatusOutput {
    pub(super) ok: bool,
    pub(super) command: &'static str,
    pub(super) contract_version: u16,
    pub(super) daemon: DaemonProcess,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) snapshot: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MetricsOutput {
    pub(super) ok: bool,
    pub(super) command: &'static str,
    pub(super) contract_version: u16,
    pub(super) daemon: DaemonProcess,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) metrics: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct StopOutput {
    pub(super) ok: bool,
    pub(super) command: &'static str,
    pub(super) contract_version: u16,
    pub(super) daemon: DaemonProcess,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct VersionOutput {
    pub(super) ok: bool,
    pub(super) command: &'static str,
    pub(super) contract_version: u16,
    pub(super) daemon_version: &'static str,
    pub(super) socket: SocketProtocol,
}

/// The failure code a machine consumer branches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ErrorCode {
    UsageError,
    UnknownCommand,
    DaemonError,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CommandError {
    pub(super) code: ErrorCode,
    pub(super) message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ErrorOutput {
    pub(super) ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) command: Option<String>,
    pub(super) contract_version: u16,
    pub(super) error: CommandError,
}

impl ErrorOutput {
    pub(super) fn new(command: Option<String>, code: ErrorCode, message: String) -> Self {
        Self {
            ok: false,
            command,
            contract_version: CONTRACT_VERSION,
            error: CommandError { code, message },
        }
    }
}

/// Print a machine payload. Serialization of these owned structs cannot fail in
/// practice; if it somehow does, say so on stderr rather than emitting a
/// half-written line on stdout.
pub(super) fn print_json<T: Serialize>(payload: &T) {
    match serde_json::to_string(payload) {
        Ok(line) => println!("{line}"),
        Err(error) => eprintln!("bowline-daemon output serialization failed: {error}"),
    }
}
