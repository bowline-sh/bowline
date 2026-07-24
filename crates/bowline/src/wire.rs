use super::*;

use bowline_core::introspection::SyncIntrospection;
use bowline_core::wire::generated::DaemonStatusSnapshotResult;
use bowline_core::wire::status_command_from_wire;
use bowline_daemon_rpc::{ClientOptions, DaemonClient};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DaemonInfo {
    daemon_version: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SyncBarrierResult {
    convergence_revision: u64,
}

/// Establish a post-invocation convergence boundary. Unlike a status snapshot,
/// this call cannot acknowledge an idle state captured before the caller's
/// filesystem mutation reached the watcher or before a hosted ref update arrived.
pub(super) fn await_daemon_sync_barrier(
    socket: &Path,
    workspace_id: &WorkspaceId,
    timeout: Duration,
) -> io::Result<u64> {
    let options = ClientOptions::new("cli", CLI_VERSION);
    let client = DaemonClient::connect(socket, options).map_err(rpc_io_error)?;
    let timeout_ms = timeout.as_millis().min(u128::from(u64::MAX)) as u64;
    let result: SyncBarrierResult = client
        .call(
            "sync.barrier",
            &serde_json::json!({
                "workspaceId": workspace_id.as_str(),
                "timeoutMs": timeout_ms.max(1),
            }),
            Some(timeout),
        )
        .map_err(rpc_io_error)?;
    Ok(result.convergence_revision)
}

pub(super) fn handshake(socket: &Path) -> io::Result<Handshake> {
    let options = ClientOptions::new("cli", CLI_VERSION);
    let client = DaemonClient::connect(socket, options).map_err(rpc_io_error)?;
    let info: DaemonInfo = client
        .call(
            "daemon.info",
            &serde_json::json!({}),
            Some(DAEMON_HANDSHAKE_TIMEOUT),
        )
        .map_err(rpc_io_error)?;
    let status: DaemonStatusSnapshotResult = client
        .call(
            "status.getSnapshot",
            &serde_json::json!({}),
            Some(DAEMON_HANDSHAKE_TIMEOUT),
        )
        .map_err(rpc_io_error)?;
    Ok(Handshake {
        daemon_version: info.daemon_version,
        status_json: serde_json::to_string(&status.snapshot).map_err(io::Error::other)?,
    })
}

/// Best-effort daemon-version probe for `bowline version --json`. Returns `None`
/// when the daemon is unreachable; only `daemon.info` is called so a degraded
/// status snapshot never masks a running daemon.
pub(super) fn daemon_version(socket: &Path) -> Option<String> {
    let options = ClientOptions::new("cli", CLI_VERSION);
    let client = DaemonClient::connect(socket, options).ok()?;
    let info: DaemonInfo = client
        .call(
            "daemon.info",
            &serde_json::json!({}),
            Some(DAEMON_HANDSHAKE_TIMEOUT),
        )
        .ok()?;
    Some(info.daemon_version)
}

/// Fetch the daemon's live status snapshot for its active workspace. Returns
/// `None` when the daemon is unreachable or the snapshot cannot be decoded.
pub(super) fn daemon_status_snapshot(
    socket: &Path,
) -> Option<bowline_core::commands::StatusCommandOutput> {
    daemon_status_snapshot_for_scope(socket, None)
}

pub(super) fn daemon_status_snapshot_for_project(
    socket: &Path,
    project_path: &Path,
) -> Option<bowline_core::commands::StatusCommandOutput> {
    daemon_status_snapshot_for_scope(socket, Some(project_path))
}

fn daemon_status_snapshot_for_scope(
    socket: &Path,
    project_path: Option<&Path>,
) -> Option<bowline_core::commands::StatusCommandOutput> {
    let options = ClientOptions::new("cli", CLI_VERSION);
    let client = DaemonClient::connect(socket, options).ok()?;
    let snapshot: DaemonStatusSnapshotResult = client
        .call(
            "status.getSnapshot",
            &bowline_core::wire::generated::DaemonStatusScopeParams {
                workspace_root: None,
                project_path: project_path.map(|path| path.to_string_lossy().into_owned()),
                requested_path: None,
            },
            Some(DAEMON_HANDSHAKE_TIMEOUT),
        )
        .ok()?;
    status_command_from_wire(snapshot.snapshot).ok()
}

/// The daemon's live direction-split sync view, or `None` when the daemon is
/// unreachable or reports no sync queue.
pub(super) fn daemon_sync_introspection(socket: &Path) -> Option<SyncIntrospection> {
    daemon_status_snapshot(socket)?
        .sync_queue
        .as_ref()
        .map(SyncIntrospection::from_queue)
}

pub(super) fn request_shutdown(socket: &Path) -> io::Result<()> {
    let options = ClientOptions::new("cli", CLI_VERSION);
    let client = DaemonClient::connect(socket, options).map_err(rpc_io_error)?;
    client
        .call::<_, serde_json::Value>(
            "daemon.shutdown",
            &serde_json::json!({}),
            Some(Duration::from_secs(2)),
        )
        .map(|_| ())
        .map_err(rpc_io_error)
}

/// Work-view engine RPC (create/review/accept). Materialize and accept move
/// real workspace bytes through the hosted transport, so the timeout is
/// generous rather than interactive.
const WORK_RPC_TIMEOUT: Duration = Duration::from_secs(120);

pub(super) fn call_work_rpc(
    method: &str,
    params: &serde_json::Value,
) -> io::Result<serde_json::Value> {
    // Explicit socket override first (tests and non-default daemon layouts);
    // otherwise the state-root default the daemon binds.
    let socket = match std::env::var_os("BOWLINE_CONTROL_SOCKET") {
        Some(path) => PathBuf::from(path),
        None => default_control_socket_path()?,
    };
    let client = DaemonClient::connect(&socket, ClientOptions::new("cli", CLI_VERSION))
        .map_err(rpc_io_error)?;
    client
        .call(method, params, Some(WORK_RPC_TIMEOUT))
        .map_err(rpc_io_error)
}

fn rpc_io_error(error: bowline_daemon_rpc::ClientError) -> io::Error {
    match error {
        bowline_daemon_rpc::ClientError::Io { source, .. } => source,
        error => io::Error::other(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_transport_errors_preserve_their_io_kind() {
        let error = rpc_io_error(bowline_daemon_rpc::ClientError::Io {
            operation: "connect",
            source: io::Error::from(io::ErrorKind::NotFound),
        });
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}
