use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use bowline_control_plane::{
    ControlPlaneClient, ControlPlaneError, HostedControlPlaneClient, SignedUrlByteStore,
    WorkspaceRef, hosted_function_call_counts,
};
use bowline_core::{
    commands::{AgentToolInvokeRequest, AgentToolTransport, CONTRACT_VERSION},
    events::{
        EventName, EventRedaction, EventSeverity, EventSubject, EventSubjectKind, WorkspaceEvent,
    },
    hosted::{DEFAULT_CONVEX_URL, DEFAULT_WORKOS_CLIENT_ID},
    ids::{DeviceId, EventId, WorkspaceId},
    policy::{MaterializationMode, PathClassification},
    workspace_graph::normalize_workspace_path,
};
use bowline_local::{
    account::workos,
    agents::invoke_agent_tool_from_local_daemon,
    device_keys::{DeviceKeyStore, KeyringDeviceKeyStore, ServerLocalSecretStore},
    metadata::{
        DEFAULT_DATABASE_FILE, LocalWriteLogRecord, MetadataStore, RemoteRefCursorRecord,
        SyncOperationCounts, SyncOperationRecord,
    },
    notifications::{
        DesktopNotificationSender, NotificationDedupe, NotificationDispatchReport,
        NotificationSender, dispatch_new_notifications, pending_device_payloads,
    },
    policy::{PathFacts, UserPolicy, classify_path},
    status::StatusOptions,
    sync::{SyncRunner, SyncRunnerOptions, SyncTickOutcome, UploadOutcome},
    trust::grants,
};
use bowline_storage::{ByteStore, StorageKey};
use notify::{
    Event, RecommendedWatcher, RecursiveMode, Watcher,
    event::{EventKind, ModifyKind, RemoveKind},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uds::UnixStreamExt;

const PHASE: &str = "0D";
const PROTOCOL: &str = "bowline.local";
const PROTOCOL_VERSION: u32 = 1;
const DEFAULT_SOCKET: &str = "/tmp/bowline-daemon.sock";
const ENV_METADATA_DB: &str = "BOWLINE_METADATA_DB";
const EXIT_USAGE: u8 = 2;
const EXIT_FAILURE: u8 = 1;
const WATCHER_SETTLE_WINDOW: Duration = Duration::from_millis(250);
const DEFAULT_SYNC_INTERVAL: Duration = Duration::from_secs(600);
const NOTIFICATION_POLL_INTERVAL: Duration = Duration::from_secs(30);
const STATUS_PUBLISH_INTERVAL: Duration = Duration::from_secs(60);
const REMOTE_OBSERVER_DRAIN_INTERVAL: Duration = Duration::from_secs(1);
const REMOTE_OBSERVER_RECONNECT_INITIAL: Duration = Duration::from_secs(30);
const REMOTE_OBSERVER_RECONNECT_MAX: Duration = Duration::from_secs(900);
const SYNC_CLAIM_TIMEOUT_SECONDS: i64 = 60;
const SYNC_RETRY_INITIAL_SECONDS: i64 = 2;
const SYNC_RETRY_MAX_SECONDS: i64 = 60;
const SYNC_RETRY_JITTER_SECONDS: i64 = 3;
const WATCHER_DRAIN_BUDGET: usize = 512;

mod cli;
mod control_plane;
mod protocol;
mod status;
mod sync;
mod watcher;

#[cfg(test)]
#[allow(clippy::arc_with_non_send_sync)]
mod tests;

pub(crate) fn entrypoint() -> ExitCode {
    cli::entrypoint()
}

#[cfg(test)]
use cli::{Command, parse_args};
use control_plane::{
    hosted_control_plane, key_store, require_convex_url, runtime_error, workspace_key_bytes,
};
use protocol::{
    current_timestamp, format_timestamp, handshake, json_string, json_string_array,
    request_shutdown, serve,
};
use status::{
    StatusPublishRequest, StatusPublisher, hosted_status_publisher, initial_sync_status_json,
    sync_operation_counts_json, sync_status_with_hosted_calls, waiting_queue_status_parts,
    watcher_runtime_state_json,
};
#[cfg(test)]
use sync::{
    ConflictSummary, RemoteRefObserver, SyncExecutor, SyncFailureAction, SyncOnceSummary,
    hosted_sync_executor, remote_observer_reconnect_delay,
    requeue_startup_sync_claims_with_resolved_attention, retry_delay_seconds, run_sync_once_with,
    sync_failure_action,
};
use sync::{
    ContinuousSyncOptions, ContinuousSyncRuntime, DaemonRuntime, SyncOnceArgs, run_sync_once,
};
use watcher::{
    WatcherDrain, WatcherRuntimeState, WatcherSignal, is_private_state_path, stable_token,
    start_sync_watcher, watcher_event_paths, watcher_operation, watcher_relative_path,
    watcher_should_record,
};
