use super::*;

pub(super) mod acceptor;
mod connection_executor;
mod coordinator_runtime;
mod supervisor;
#[cfg(test)]
mod tests;

use acceptor::{AcceptorEvent, AcceptorWake, BlockingAcceptor};
use connection_executor::ConnectionExecutor;
use coordinator_runtime::run_scheduler;
#[cfg(all(test, target_os = "linux"))]
pub(in crate::daemon) use coordinator_runtime::watcher_bridge::WatcherBridge;
use supervisor::{DaemonThreads, ShutdownOutcome};

pub(super) fn handle_shutdown_grace_expiry(component: &'static str) {
    eprintln!(
        "bowline-daemon forced shutdown: {component} exceeded the grace deadline; the manifest engine re-syncs on next start"
    );
}

#[cfg(not(test))]
pub(super) fn force_process_shutdown(socket: &Path) -> ! {
    if let Err(error) = fs::remove_file(socket)
        && error.kind() != io::ErrorKind::NotFound
    {
        eprintln!("bowline-daemon forced shutdown could not remove the control socket: {error}");
    }
    eprintln!("bowline-daemon forced shutdown grace expired; the process will terminate");
    std::process::abort();
}

#[cfg(test)]
pub(super) fn force_process_shutdown(_socket: &Path) {
    eprintln!("bowline-daemon test watchdog observed forced process termination");
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct ThreadJoinReport {
    pub(super) expected: usize,
    pub(super) joined: usize,
    pub(super) forced_recovery: bool,
}

impl ThreadJoinReport {
    fn record_joined(&mut self, count: usize) {
        self.expected += count;
        self.joined += count;
    }

    fn merge(&mut self, other: Self) {
        self.expected += other.expected;
        self.joined += other.joined;
        self.forced_recovery |= other.forced_recovery;
    }
}

pub(super) struct StatusSnapshot {
    pub(super) daemon_version: String,
    pub(super) snapshot: serde_json::Value,
}

pub(super) struct SocketGuard {
    pub(super) path: Option<PathBuf>,
}

impl SocketGuard {
    fn cleanup(mut self) -> io::Result<()> {
        let path = self
            .path
            .take()
            .expect("socket path remains owned until cleanup");
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.take()
            && let Err(error) = fs::remove_file(path)
            && error.kind() != io::ErrorKind::NotFound
        {
            eprintln!("bowline-daemon socket cleanup failed: {error}");
        }
    }
}

const MAX_CONCURRENT_CONNECTIONS: usize = 32;
const CONNECTION_BUSY_RETRY_AFTER: Duration = Duration::from_millis(250);

struct ConnectionGuard {
    state: Arc<DaemonServerState>,
    acceptor_wake: AcceptorWake,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.state.active_connections.fetch_sub(1, Ordering::AcqRel);
        if self.state.shutting_down.load(Ordering::Acquire)
            && let Err(error) = self.acceptor_wake.stop()
        {
            eprintln!("bowline-daemon could not wake RPC acceptor after connection: {error}");
        }
    }
}

pub(super) fn serve(socket: &Path, once: bool, runtime: DaemonRuntime) -> io::Result<()> {
    prepare_socket(socket)?;
    let state = Arc::new(DaemonServerState::new(&runtime)?);
    let threads = DaemonThreads::start(socket, once, runtime, Arc::clone(&state))?;
    let guard = SocketGuard {
        path: Some(socket.to_path_buf()),
    };
    let accept_error = run_accept_loop(
        threads.acceptor(),
        threads.connections(),
        &state,
        threads.socket_owner_uid(),
        threads.rpc_executor(),
        once,
    );
    let reason = if accept_error.is_some() {
        ShutdownReason::AcceptorFailed
    } else if once {
        ShutdownReason::ServeOnceComplete
    } else {
        state
            .shutdown_reason()
            .unwrap_or(ShutdownReason::ClientRequest)
    };
    let report = threads.shutdown(reason)?;
    log_shutdown_report(report);
    state.advance_shutdown(ShutdownPhase::RemoveSocketState);
    guard.cleanup()?;
    if report.joined_threads != report.expected_threads {
        return Err(io::Error::other(
            "daemon shutdown did not join every owned thread",
        ));
    }
    if report.coordinator_metrics.configured_workers != report.coordinator_metrics.joined_workers {
        return Err(io::Error::other(
            "daemon coordinator metrics did not account for every lane worker",
        ));
    }
    if report.outcome == ShutdownOutcome::ForcedRecovery {
        return Err(io::Error::other(
            "daemon shutdown exceeded its grace deadline; forced recovery was recorded after every owned thread joined",
        ));
    }
    state.advance_shutdown(ShutdownPhase::Complete);
    accept_error.map_or(Ok(()), Err)
}

fn log_shutdown_report(report: supervisor::ShutdownReport) {
    eprintln!(
        "bowline-daemon shutdown outcome={} elapsed_ms={} grace_ms={} joined_threads={} expected_threads={} coordinator_workers={} coordinator_joined={} coordinator_worker_losses={} coordinator_shutdown_recoveries={} rpc_workers={} rpc_panics={} rpc_queue_delay_max_ns={} rpc_execution_max_ns={} cancellation_latency_max_ns={}",
        match report.outcome {
            ShutdownOutcome::Clean => "clean",
            ShutdownOutcome::ForcedRecovery => "forced-recovery",
        },
        report.elapsed.as_millis(),
        report.grace.as_millis(),
        report.joined_threads,
        report.expected_threads,
        report.coordinator_metrics.configured_workers,
        report.coordinator_metrics.joined_workers,
        report.coordinator_metrics.worker_losses,
        report.coordinator_metrics.shutdown_recoveries,
        report.rpc_metrics.configured_workers(),
        report.rpc_metrics.panicked(),
        report.rpc_metrics.queue_delay_max_nanos(),
        report.rpc_metrics.execution_max_nanos(),
        report.rpc_metrics.cancellation_latency_max_nanos(),
    );
}

fn run_accept_loop(
    acceptor: &BlockingAcceptor,
    connections: &ConnectionExecutor,
    state: &Arc<DaemonServerState>,
    socket_owner_uid: Option<u32>,
    rpc_executor: &Arc<super::protocol_v2::RpcExecutor>,
    once: bool,
) -> Option<io::Error> {
    let mut once_in_flight = false;
    let mut once_completed = false;
    let mut acceptor_stopped = false;
    loop {
        if acceptor_stopped {
            if !once_in_flight || once_completed {
                return None;
            }
            match connections.completions().recv() {
                Ok(()) => once_completed = true,
                Err(_) => {
                    return Some(io::Error::other(
                        "daemon connection completion channel closed",
                    ));
                }
            }
            continue;
        }
        crossbeam_channel::select_biased! {
            recv(acceptor.events()) -> event => match event {
                Ok(AcceptorEvent::Accepted(stream)) => {
                    if once_in_flight {
                        continue;
                    }
                    let admitted = admit_connection(
                        stream,
                        connections,
                        state,
                        socket_owner_uid,
                        rpc_executor,
                        acceptor.wake(),
                    );
                    if once {
                        once_in_flight = true;
                        once_completed = !admitted;
                        if let Err(error) = acceptor.stop() {
                            return Some(error);
                        }
                    }
                }
                Ok(AcceptorEvent::Failed(error)) => return Some(error),
                Ok(AcceptorEvent::Stopped) => acceptor_stopped = true,
                Err(_) => return Some(io::Error::other("daemon acceptor channel closed")),
            },
            recv(connections.completions()) -> completion => {
                if completion.is_err() {
                    return Some(io::Error::other("daemon connection completion channel closed"));
                }
                once_completed |= once_in_flight;
            },
        }
    }
}

fn admit_connection(
    stream: UnixStream,
    connections: &ConnectionExecutor,
    state: &Arc<DaemonServerState>,
    socket_owner_uid: Option<u32>,
    rpc_executor: &Arc<super::protocol_v2::RpcExecutor>,
    acceptor_wake: AcceptorWake,
) -> bool {
    if !reserve_connection(state) {
        reject_overloaded_connection(stream, state, socket_owner_uid);
        return false;
    }
    if let Err(stream) = connections.try_submit(
        stream,
        Arc::clone(state),
        socket_owner_uid,
        Arc::clone(rpc_executor),
        acceptor_wake,
    ) {
        state.active_connections.fetch_sub(1, Ordering::AcqRel);
        reject_overloaded_connection(stream, state, socket_owner_uid);
        return false;
    }
    true
}

fn reserve_connection(state: &DaemonServerState) -> bool {
    state
        .active_connections
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
            (active < MAX_CONCURRENT_CONNECTIONS).then_some(active + 1)
        })
        .is_ok()
}

fn reject_overloaded_connection(
    mut stream: UnixStream,
    state: &Arc<DaemonServerState>,
    socket_owner_uid: Option<u32>,
) {
    if let Err(error) = verify_connection_magic(&mut stream).and_then(|()| {
        super::protocol_v2::reject_overloaded_connection(
            stream,
            state,
            socket_owner_uid,
            CONNECTION_BUSY_RETRY_AFTER,
        )
    }) {
        eprintln!("bowline-daemon could not return connection busy response: {error}");
    }
}

fn handle_connection(
    mut stream: UnixStream,
    state: &Arc<DaemonServerState>,
    socket_owner_uid: Option<u32>,
    executor: Arc<super::protocol_v2::RpcExecutor>,
) -> io::Result<()> {
    verify_connection_magic(&mut stream)?;
    super::protocol_v2::handle_v2_connection(stream, state, socket_owner_uid, executor)
}

fn verify_connection_magic(stream: &mut UnixStream) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut received = [0_u8; bowline_daemon_rpc::CONNECTION_MAGIC.len()];
    stream.read_exact(&mut received)?;
    if received != bowline_daemon_rpc::CONNECTION_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "daemon RPC connection magic is invalid",
        ));
    }
    Ok(())
}

pub(super) fn prepare_socket(socket: &Path) -> io::Result<()> {
    if let Some(parent) = socket.parent() {
        fs::create_dir_all(parent)?;
        ensure_owner_only_socket_dir(parent)?;
    }

    if socket.exists() {
        if UnixStream::connect(socket).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "daemon socket is already in use",
            ));
        }
        ensure_socket_path_owned_by_current_user(socket)?;
        fs::remove_file(socket)?;
    }

    Ok(())
}

fn ensure_owner_only_socket_dir(parent: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(parent)?;
    if !metadata.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "daemon socket directory {} is not a directory",
                parent.display()
            ),
        ));
    }
    let uid = current_uid();
    if metadata.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "daemon socket directory {} is owned by uid {}, expected {uid}",
                parent.display(),
                metadata.uid()
            ),
        ));
    }
    let mode = metadata.permissions().mode() & 0o777;
    if mode != 0o700 {
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn ensure_socket_path_owned_by_current_user(socket: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(socket)?;
    let uid = current_uid();
    if metadata.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "daemon socket path {} is owned by uid {}, expected {uid}",
                socket.display(),
                metadata.uid()
            ),
        ));
    }
    Ok(())
}

fn current_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

pub(super) fn current_timestamp() -> String {
    format_timestamp(OffsetDateTime::now_utc())
}

pub(super) fn format_timestamp(timestamp: OffsetDateTime) -> String {
    timestamp
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

pub(super) fn local_peer_credential_checked(
    stream: &UnixStream,
    socket_owner_uid: Option<u32>,
) -> bool {
    let Some(socket_owner_uid) = socket_owner_uid else {
        return false;
    };
    stream
        .initial_peer_credentials()
        .is_ok_and(|credentials| credentials.euid() == socket_owner_uid)
}

pub(super) fn status_snapshot(socket: &Path) -> io::Result<StatusSnapshot> {
    let options = bowline_daemon_rpc::ClientOptions::new("daemon-cli", env!("CARGO_PKG_VERSION"));
    let client =
        bowline_daemon_rpc::DaemonClient::connect(socket, options).map_err(io::Error::other)?;
    let daemon_version = client.server_hello().daemon_version.clone();
    let status: bowline_core::wire::generated::DaemonStatusSnapshotResult = client
        .call(
            "status.getSnapshot",
            &bowline_core::wire::generated::DaemonStatusScopeParams {
                workspace_root: None,
                project_path: None,
                requested_path: None,
            },
            Some(Duration::from_secs(2)),
        )
        .map_err(io::Error::other)?;
    Ok(StatusSnapshot {
        daemon_version,
        snapshot: serde_json::to_value(status.snapshot).map_err(io::Error::other)?,
    })
}

/// Read the daemon's runtime metrics (Plan 111 Step 5), including the engine
/// cost meters under the `engine` key. Opaque JSON: the CLI prints it verbatim so
/// the release gate can classify the C1–C5 budgets from the recorded counters.
pub(super) fn metrics_snapshot(socket: &Path) -> io::Result<serde_json::Value> {
    let options = bowline_daemon_rpc::ClientOptions::new("daemon-cli", env!("CARGO_PKG_VERSION"));
    let client =
        bowline_daemon_rpc::DaemonClient::connect(socket, options).map_err(io::Error::other)?;
    let metrics: serde_json::Value = client
        .call(
            "daemon.metrics",
            &serde_json::json!({}),
            Some(Duration::from_secs(2)),
        )
        .map_err(io::Error::other)?;
    Ok(metrics)
}

pub(super) fn request_shutdown(socket: &Path) -> io::Result<()> {
    let options = bowline_daemon_rpc::ClientOptions::new("daemon-cli", env!("CARGO_PKG_VERSION"));
    let client =
        bowline_daemon_rpc::DaemonClient::connect(socket, options).map_err(io::Error::other)?;
    client
        .call::<_, serde_json::Value>(
            "daemon.shutdown",
            &serde_json::json!({}),
            Some(Duration::from_secs(2)),
        )
        .map(|_| ())
        .map_err(io::Error::other)
}

pub(super) fn json_string(input: &str) -> String {
    match serde_json::to_string(input) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("bowline-daemon JSON string serialization failed: {error}");
            serde_json::Value::String(String::new()).to_string()
        }
    }
}
