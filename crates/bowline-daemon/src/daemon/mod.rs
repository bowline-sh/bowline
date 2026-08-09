use std::collections::BTreeMap;
use std::env;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PROTOCOL: &str = bowline_daemon_rpc::DAEMON_RPC_PROTOCOL;
const PROTOCOL_VERSION: u32 = bowline_daemon_rpc::DAEMON_RPC_PROTOCOL_VERSION as u32;
const DEFAULT_SOCKET_FALLBACK: &str = ".bowline/runtime/bowline-daemon.sock";
const EXIT_USAGE: u8 = 2;
const EXIT_FAILURE: u8 = 1;
const NOTIFICATION_POLL_INTERVAL: Duration = Duration::from_secs(30);
const STATUS_PUBLISH_INTERVAL: Duration = Duration::from_secs(60);
// Idle daemons still publish periodically so dashboards can distinguish "quiet"
// from "dead" without paying for a full unchanged heartbeat every minute.
const STATUS_PUBLISH_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(300);
const REMOTE_OBSERVER_RECONNECT_INITIAL: Duration = Duration::from_millis(250);
const REMOTE_OBSERVER_RECONNECT_MAX: Duration = Duration::from_secs(5);
// A refused credential does not heal on the transport schedule: every reopen
// registers another account session against a control plane that has already
// refused this identity twice. The status projection carries the condition while
// this waits, so slowing down costs visibility nothing.
const REMOTE_OBSERVER_REAUTH_RECONNECT_MAX: Duration = Duration::from_secs(60);
// A remote head signed by a device this workspace does not authorize is not a
// transport fault: reopening the subscription re-reads the same unverifiable
// head. The observer keeps watching (the head can move, or that device can be
// approved) but on the slow schedule, and its bounded trust re-reads — not this
// interval — are what can actually clear the condition.
const REMOTE_OBSERVER_UNTRUSTED_SIGNER_RECONNECT_MAX: Duration = Duration::from_secs(60);
const WATCHER_DRAIN_BUDGET: usize = 512;
static DAEMON_ENV: Mutex<BTreeMap<String, String>> = Mutex::new(BTreeMap::new());
/// The state root this daemon was started against. Remembered because a session
/// the control plane refuses has to be replaced *and written back* to the
/// `daemon.env` the daemon was provisioned from, or the next start begins with
/// the same refused credential.
static DAEMON_STATE_ROOT: Mutex<Option<PathBuf>> = Mutex::new(None);

mod account_session;
mod cli;
mod control_plane;
mod coordinator;
mod finder_status;
mod hosted_context;
mod log_supervisor;
mod rpc_service;
mod server_state;
mod socket_server;
mod status;
mod sync;
mod watcher;

use hosted_context::{
    HostedContext, HostedContextCache, HostedContextResolver, hosted_context_resolver,
};

#[cfg(test)]
fn test_hosted_context_resolver() -> HostedContextResolver {
    hosted_context_resolver(Arc::new(HostedContextCache::new()))
}

#[cfg(test)]
#[allow(clippy::arc_with_non_send_sync)]
mod tests;

pub(crate) fn entrypoint() -> ExitCode {
    cli::entrypoint()
}

/// Binds this process to its daemon state root: loads the persisted `daemon.env`
/// overlay and remembers where it came from.
///
/// The allow-list and the parser are `bowline_local::daemon_env`'s, never a
/// second copy here — a daemon that read the file differently from the CLI that
/// writes it would silently lose its account session.
pub(super) fn bind_daemon_state_root(state_root: &Path) {
    if let Ok(mut daemon_env) = DAEMON_ENV.lock() {
        *daemon_env = bowline_local::daemon_env::read(state_root);
    }
    if let Ok(mut root) = DAEMON_STATE_ROOT.lock() {
        *root = Some(state_root.to_path_buf());
    }
}

pub(super) fn daemon_state_root() -> Option<PathBuf> {
    DAEMON_STATE_ROOT.lock().ok()?.clone()
}

pub(super) fn daemon_env_var(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| DAEMON_ENV.lock().ok()?.get(name).cloned())
}

#[cfg(test)]
use cli::{Command, parse_args};
use control_plane::{hosted_control_plane, key_store, runtime_error, workspace_key_bytes};
use server_state::{DaemonServerState, ShutdownPhase, ShutdownReason, StatusSubscription};
use socket_server::{
    current_timestamp, metrics_snapshot, request_shutdown, serve, status_snapshot,
};
use status::{
    StatusPublishOutcome, StatusPublishPayload, StatusPublishRequest, StatusPublisher,
    hosted_status_publisher_with_context,
};
use sync::{ContinuousSyncRuntime, DaemonRuntime, SyncArgs};
#[cfg(test)]
use sync::{drain_policy, invalidate_policy_cache_for_path};
#[cfg(all(test, target_os = "linux"))]
use watcher::send_watcher_signal;
#[cfg(test)]
use watcher::start_sync_watcher;
#[cfg(test)]
use watcher::watcher_relative_path;
use watcher::{WatcherSignal, start_sync_watcher_with_recovery};
