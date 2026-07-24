use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{
    Arc, Condvar, Mutex, Weak,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    mpsc::{self, Receiver},
};
use std::time::{Duration, Instant};

#[cfg(test)]
use bowline_control_plane::ControlPlaneTimestamp;
use bowline_control_plane::{
    AuthorizedDeviceRecord, ControlPlaneError, DeviceApprovalRequestList, DeviceControlPlaneClient,
    HostedControlPlaneClient, SignedUrlByteStore, SignedUrlHttpClient,
};
use bowline_core::{
    devices::display_matching_code,
    hosted::{DEFAULT_CONVEX_URL, DEFAULT_WORKOS_CLIENT_ID},
    ids::{DeviceId, WorkspaceId},
    policy::{MaterializationMode, PathClassification},
    workspace_graph::normalize_workspace_path,
};
use bowline_local::{
    account::workos,
    device_keys::{DeviceKeyError, DeviceKeyStore, DeviceProofVerifier, default_device_key_store},
    metadata::{DEFAULT_DATABASE_FILE, default_control_socket_path},
    notifications::{
        DesktopNotificationSender, NotificationDedupe, NotificationDispatchReport,
        NotificationSender, dispatch_new_notifications_with_checkpoint, pending_device_payloads,
    },
    policy::{PathFacts, UserPolicy, classify_path},
    trust::grants,
};
use notify::{
    Event, RecommendedWatcher, RecursiveMode, Watcher,
    event::{EventKind, ModifyKind, RemoveKind},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uds::UnixStreamExt;

const PHASE: &str = "0D";
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
const WATCHER_DRAIN_BUDGET: usize = 512;
static DAEMON_ENV: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

mod cli;
mod control_plane;
mod coordinator;
mod finder_status;
mod hosted_context;
mod protocol;
mod protocol_v2;
mod server_state;
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

pub(super) fn load_persisted_daemon_env(state_root: &Path) {
    let entries = fs::read_to_string(state_root.join("daemon.env"))
        .ok()
        .map(|contents| {
            contents
                .lines()
                .filter_map(|line| line.split_once('='))
                .filter(|(key, value)| valid_persisted_daemon_env_key(key) && !value.is_empty())
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if let Ok(mut daemon_env) = DAEMON_ENV.lock() {
        *daemon_env = entries;
    }
}

pub(super) fn daemon_env_var(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            DAEMON_ENV
                .lock()
                .ok()
                .and_then(|daemon_env| {
                    daemon_env
                        .iter()
                        .find_map(|(key, value)| (key == name).then(|| value.clone()))
                })
                .filter(|value| !value.is_empty())
        })
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
            | "BOWLINE_ACCOUNT_SESSION_REVOCATION_TOKEN"
            | "BOWLINE_CONTROL_PLANE_TOKEN"
            | "BOWLINE_WORKOS_ACCESS_TOKEN"
            | "BOWLINE_WORKOS_CLIENT_ID"
    )
}

#[cfg(test)]
use bowline_local::notifications::dispatch_new_notifications;
#[cfg(test)]
use cli::{Command, parse_args};
use control_plane::{hosted_control_plane, key_store, runtime_error, workspace_key_bytes};
use protocol::{
    current_timestamp, json_string, metrics_snapshot, request_shutdown, serve, status_snapshot,
};
use server_state::{DaemonServerState, ShutdownPhase, ShutdownReason, StatusSubscription};
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
use watcher::watcher_relative_path;
use watcher::{WatcherSignal, start_sync_watcher};
