use super::*;

use bowline_core::wire::generated::DaemonStatusSnapshotResult;
use bowline_daemon_rpc::{ClientOptions, DaemonClient};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DaemonInfo {
    daemon_version: String,
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

pub(super) fn wake_durable_work_best_effort() {
    let Ok(socket) = default_control_socket_path() else {
        return;
    };
    let Ok(client) = DaemonClient::connect(&socket, ClientOptions::new("cli", CLI_VERSION)) else {
        return;
    };
    let _wake = client.call::<_, serde_json::Value>(
        "daemon.wakeDurableWork",
        &serde_json::json!({}),
        Some(Duration::from_secs(2)),
    );
}

fn rpc_io_error(error: bowline_daemon_rpc::ClientError) -> io::Error {
    io::Error::other(error)
}
