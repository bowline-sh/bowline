use super::*;

use std::fmt;

use bowline_core::wire::generated::{
    DaemonStatusScopeParams, DaemonStatusSnapshotResult, DaemonSyncBarrierResult,
};
use bowline_core::wire::{StatusTransportError, status_command_from_wire, status_command_to_wire};
use bowline_daemon_rpc::{
    ClientError, ClientOptions, DaemonClient, DaemonReachability, RetryDisposition, VersionSkew,
};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DaemonInfo {
    daemon_version: String,
}

#[derive(Debug)]
pub(super) enum DaemonRpcError {
    /// The transport or handshake failed. The `ClientError` is kept whole so the
    /// shared `DaemonReachability` classification decides what it means; nothing
    /// on this path is allowed to flatten a running-but-skewed daemon into
    /// "stopped".
    Client(Box<ClientError>),
    DecodeStatus(StatusTransportError),
}

impl DaemonRpcError {
    pub(super) fn reachability(&self) -> DaemonReachability {
        match self {
            Self::Client(error) => error.reachability(),
            // The daemon answered; this build could not read what it said.
            Self::DecodeStatus(_) => DaemonReachability::Unreachable,
        }
    }

    pub(super) fn version_skew(&self) -> Option<VersionSkew> {
        self.reachability().version_skew().cloned()
    }

    /// The transport-level `io::ErrorKind`, when this failure came from the
    /// socket itself. `None` means the daemon answered.
    pub(super) fn io_kind(&self) -> Option<io::ErrorKind> {
        match self {
            Self::Client(error) => match error.as_ref() {
                ClientError::Io { source, .. } => Some(source.kind()),
                _ => None,
            },
            Self::DecodeStatus(_) => None,
        }
    }

    /// Whether no daemon is running, which is the only state where starting one
    /// is the right move.
    pub(super) fn daemon_is_absent(&self) -> bool {
        self.reachability().is_absent()
    }

    /// Whether a caller holding a deadline should try the same call again.
    pub(super) fn retry_disposition(&self) -> RetryDisposition {
        match self {
            Self::Client(error) => error.retry_disposition(),
            // The daemon answered; this build cannot read the answer, and a
            // later identical answer would be just as unreadable.
            Self::DecodeStatus(_) => RetryDisposition::Terminal,
        }
    }

    /// Whether the daemon itself answered with a structured error, as opposed to
    /// the transport failing before any answer arrived. The distinction decides
    /// whether a caller is waiting on the daemon or on one of its subsystems.
    pub(super) fn daemon_answered(&self) -> bool {
        match self {
            Self::Client(error) => matches!(error.as_ref(), ClientError::Remote(_)),
            Self::DecodeStatus(_) => true,
        }
    }
}

impl fmt::Display for DaemonRpcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Client(error) => write!(formatter, "{error}"),
            Self::DecodeStatus(source) => {
                write!(formatter, "daemon status snapshot is undecodable: {source}")
            }
        }
    }
}

impl std::error::Error for DaemonRpcError {}

impl From<ClientError> for DaemonRpcError {
    fn from(error: ClientError) -> Self {
        Self::Client(Box::new(error))
    }
}

impl From<DaemonRpcError> for io::Error {
    fn from(error: DaemonRpcError) -> Self {
        match error {
            DaemonRpcError::Client(client) => match *client {
                ClientError::Io { source, .. } => source,
                other => io::Error::other(other.to_string()),
            },
            other => io::Error::other(other.to_string()),
        }
    }
}

/// A live daemon's identity plus its decoded status snapshot. The snapshot stays
/// typed all the way to the lifecycle decisions that read it; nothing on this
/// path re-serializes it to a string and re-parses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Handshake {
    pub(super) daemon_version: String,
    pub(super) status: StatusCommandOutput,
}

fn connect(socket: &Path) -> Result<DaemonClient, DaemonRpcError> {
    DaemonClient::connect(socket, ClientOptions::new("cli", CLI_VERSION))
        .map_err(DaemonRpcError::from)
}

fn call<P, R>(
    client: &DaemonClient,
    method: &str,
    params: &P,
    timeout: Duration,
) -> Result<R, DaemonRpcError>
where
    P: serde::Serialize,
    R: serde::de::DeserializeOwned,
{
    client
        .call(method, params, Some(timeout))
        .map_err(DaemonRpcError::from)
}

/// Establish a post-invocation convergence boundary. Unlike a status snapshot,
/// this call cannot acknowledge an idle state captured before the caller's
/// filesystem mutation reached the watcher or before a hosted ref update arrived.
///
/// The typed `DaemonRpcError` reaches the caller whole: a wait decides between
/// sitting through a starting daemon and reporting a refusal from the structured
/// error code, which flattening to `io::Error` would destroy.
pub(super) fn await_daemon_sync_barrier(
    socket: &Path,
    workspace_id: &WorkspaceId,
    timeout: Duration,
) -> Result<DaemonSyncBarrierResult, DaemonRpcError> {
    let client = connect(socket)?;
    let timeout_ms = timeout.as_millis().min(u128::from(u64::MAX)) as u64;
    call(
        &client,
        "sync.barrier",
        &serde_json::json!({
            "workspaceId": workspace_id.as_str(),
            "timeoutMs": timeout_ms.max(1),
        }),
        timeout,
    )
}

/// Read the removal batch the engine is currently refusing. Read-only: the
/// daemon answers from the snapshot it already publishes.
pub(super) fn blocked_deletions(
    socket: &Path,
) -> Result<bowline_core::commands::BlockedDeletionsReport, DaemonRpcError> {
    let client = connect(socket)?;
    call(
        &client,
        "sync.getBlockedDeletions",
        &serde_json::json!({}),
        DAEMON_HANDSHAKE_TIMEOUT,
    )
}

/// Authorise one push of whatever the engine is refusing. Carries no batch: the
/// engine authorises its own current refusal, never one the caller describes.
pub(super) fn confirm_deletions(
    socket: &Path,
) -> Result<bowline_core::commands::DeletionsConfirmationReport, DaemonRpcError> {
    let client = connect(socket)?;
    call(
        &client,
        "sync.confirmDeletions",
        &serde_json::json!({}),
        DAEMON_HANDSHAKE_TIMEOUT,
    )
}

pub(super) fn handshake(socket: &Path) -> Result<Handshake, DaemonRpcError> {
    let client = connect(socket)?;
    let info: DaemonInfo = call(
        &client,
        "daemon.info",
        &serde_json::json!({}),
        DAEMON_HANDSHAKE_TIMEOUT,
    )?;
    let status = snapshot_over(&client, None)?;
    Ok(Handshake {
        daemon_version: info.daemon_version,
        status,
    })
}

/// Re-encode a decoded snapshot into the wire shape `daemon status --json`
/// publishes, so the machine surface stays exactly what the daemon sent while
/// every CLI decision runs on the typed value.
pub(super) fn status_snapshot_wire_value(
    status: &StatusCommandOutput,
) -> Result<serde_json::Value, StatusTransportError> {
    let wire = status_command_to_wire(status)?;
    serde_json::to_value(wire).map_err(|_| StatusTransportError::SerializeWire)
}

/// Best-effort daemon-version probe for `bowline version --json`. Returns `None`
/// when the daemon is unreachable; only `daemon.info` is called so a degraded
/// status snapshot never masks a running daemon.
pub(super) fn daemon_version(socket: &Path) -> Option<String> {
    let client = connect(socket).ok()?;
    let info: DaemonInfo = call(
        &client,
        "daemon.info",
        &serde_json::json!({}),
        DAEMON_HANDSHAKE_TIMEOUT,
    )
    .ok()?;
    Some(info.daemon_version)
}

/// Fetch the daemon's live status snapshot, optionally scoped to one project.
pub(super) fn daemon_status_snapshot_scoped(
    socket: &Path,
    project_path: Option<&Path>,
) -> Result<StatusCommandOutput, DaemonRpcError> {
    let client = connect(socket)?;
    snapshot_over(&client, project_path)
}

/// Workspace-wide snapshot for callers that need "the daemon's view or nothing".
pub(super) fn daemon_status_snapshot(socket: &Path) -> Option<StatusCommandOutput> {
    daemon_status_snapshot_scoped(socket, None).ok()
}

fn snapshot_over(
    client: &DaemonClient,
    project_path: Option<&Path>,
) -> Result<StatusCommandOutput, DaemonRpcError> {
    let snapshot: DaemonStatusSnapshotResult = call(
        client,
        "status.getSnapshot",
        &DaemonStatusScopeParams {
            workspace_root: None,
            project_path: project_path.map(|path| path.to_string_lossy().into_owned()),
            requested_path: None,
        },
        DAEMON_HANDSHAKE_TIMEOUT,
    )?;
    status_command_from_wire(snapshot.snapshot).map_err(DaemonRpcError::DecodeStatus)
}

pub(super) fn request_shutdown(socket: &Path) -> Result<(), DaemonRpcError> {
    let client = connect(socket)?;
    call::<_, serde_json::Value>(
        &client,
        "daemon.shutdown",
        &serde_json::json!({}),
        Duration::from_secs(2),
    )
    .map(|_| ())
}

/// Work-view engine RPC (create/review/accept). Materialize and accept move
/// real workspace bytes through the hosted transport, so the timeout is
/// generous rather than interactive.
const WORK_RPC_TIMEOUT: Duration = Duration::from_secs(120);

/// The control socket this CLI should talk to when no `--socket` override
/// reached the caller: an explicit environment override first (tests and
/// non-default daemon layouts), otherwise the state-root default the daemon
/// binds.
pub(super) fn control_socket_path() -> io::Result<PathBuf> {
    match std::env::var_os("BOWLINE_CONTROL_SOCKET") {
        Some(path) => Ok(PathBuf::from(path)),
        None => default_control_socket_path(),
    }
}

pub(super) fn call_work_rpc(
    method: &str,
    params: &serde_json::Value,
) -> io::Result<serde_json::Value> {
    let socket = control_socket_path()?;
    let client = connect(&socket)?;
    Ok(call(&client, method, params, WORK_RPC_TIMEOUT)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bowline_daemon_rpc::{VersionDimension, VersionWindow};

    #[test]
    fn a_missing_socket_reads_as_an_absent_daemon() {
        let error = DaemonRpcError::from(ClientError::Io {
            operation: "connect",
            source: io::Error::from(io::ErrorKind::NotFound),
        });

        assert_eq!(error.io_kind(), Some(io::ErrorKind::NotFound));
        assert!(error.daemon_is_absent());
        assert!(error.version_skew().is_none());
        assert_eq!(io::Error::from(error).kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn version_skew_survives_as_a_typed_error_with_remediation() {
        let error = DaemonRpcError::from(ClientError::IncompatibleVersion {
            dimension: VersionDimension::MachineContract,
            peer_version: 6,
            window: VersionWindow {
                minimum: 8,
                supported: 8,
            },
        });

        let skew = error.version_skew().expect("skew is preserved");
        assert_eq!(skew.peer_version, Some(6));
        assert_eq!(skew.dimension, Some(VersionDimension::MachineContract));
        // A skewed daemon is running: it must never look like an absent daemon.
        assert!(!error.daemon_is_absent());
        assert_eq!(
            error.reachability().remediation(),
            "run `bowline daemon restart`"
        );
    }

    #[test]
    fn an_undecodable_snapshot_is_not_an_absent_daemon() {
        let error = DaemonRpcError::DecodeStatus(StatusTransportError::DeserializeDomain);

        assert!(!error.daemon_is_absent());
        assert!(error.io_kind().is_none());
    }

    #[test]
    fn skew_reaches_io_callers_as_a_readable_message() {
        let error = io::Error::from(DaemonRpcError::from(ClientError::IncompatibleVersion {
            dimension: VersionDimension::MachineContract,
            peer_version: 6,
            window: VersionWindow {
                minimum: 8,
                supported: 8,
            },
        }));

        assert!(error.to_string().contains('6'));
    }
}
