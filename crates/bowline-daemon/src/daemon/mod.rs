use std::collections::{BTreeSet, HashMap};
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
use bowline_control_plane::hosted_function_call_counts;
use bowline_control_plane::{
    AuthorizedDeviceRecord, CompactEventKind, ConflictOccurrenceReconcile, ConflictOccurrenceState,
    ConflictReconcileOutcome, ConflictReconcileResult, ControlPlaneClient, ControlPlaneError,
    ControlPlaneTimestamp, DeviceApprovalRequestList, DeviceControlPlaneClient,
    HostedControlPlaneClient, Lease, LeaseControlPlaneClient, LeaseSessionState, LeaseUpdate,
    LeaseWriteTargetMode, RejectionCode, SignedUrlByteStore, SignedUrlHttpClient, WorkspaceRef,
};
use bowline_core::{
    commands::{AgentLeaseBase, AgentSessionState, AgentToolInvokeRequest, CONTRACT_VERSION},
    devices::display_matching_code,
    events::{
        EventName, EventRedaction, EventSeverity, EventSubject, EventSubjectKind, WorkspaceEvent,
    },
    hosted::{DEFAULT_CONVEX_URL, DEFAULT_WORKOS_CLIENT_ID},
    ids::{DeviceId, EventId, LeaseId, ProjectId, SnapshotId, WorkViewId, WorkspaceId},
    policy::{MaterializationMode, PathClassification},
    workspace_graph::normalize_workspace_path,
};
use bowline_local::{
    account::workos,
    agents::{
        AgentLeaseCreateOptions, DispatchedAgentLeaseCreateOptions, DispatchedAgentLeaseIdentity,
        create_dispatched_agent_lease, invoke_agent_tool_from_daemon_with_checkpoint,
    },
    device_keys::{DeviceKeyError, DeviceKeyStore, DeviceProofVerifier, default_device_key_store},
    metadata::{
        ClaimedSyncOperation, ConflictSnapshotRetention, DEFAULT_DATABASE_FILE,
        LocalMetadataRetentionPolicy, LocalWriteLogRecord, MetadataError, MetadataStore,
        PostCommitSyncComponent, RemoteRefCursorRecord, SyncCancellationOutcome, SyncClaimCheck,
        SyncClaimHandle, SyncClaimTransition, SyncCommittedCancelledLateResult,
        SyncOperationCounts, SyncOperationEnqueueOutcome, SyncOperationKind, SyncOperationRecord,
        SyncOperationState, SyncResourceKey, default_control_socket_path,
    },
    notifications::{
        DesktopNotificationSender, NotificationDedupe, NotificationDispatchReport,
        NotificationSender, dispatch_new_notifications_with_checkpoint, pending_device_payloads,
    },
    policy::{PathFacts, UserPolicy, classify_path, is_root_policy_affecting_path},
    sync::{
        ConflictState, DownloadError, FullScanReason, ScanScope, ScanStats,
        SyncExternalFailureCode, SyncRunner, SyncRunnerError, SyncRunnerFailureSource,
        SyncRunnerOptions, SyncTickOutcome, UploadError, UploadFailureSource, UploadOutcome,
        WorkViewOverlaySyncInput, WorkViewOverlaySyncResult, conflict_occurrence_is_current,
        conflict_occurrence_preparation_required, conflict_occurrence_queue_result,
        decode_conflict_occurrence_operation, decode_work_view_overlay_sync_operation,
        load_conflict_records, mark_conflict_occurrence_reconciled,
        pending_conflict_occurrence_operations, pending_work_view_overlay_sync_operation,
        work_view_overlay_sync_result,
    },
    trust::grants,
    work_views::WorkViewOverlaySyncError,
};
use bowline_storage::{
    ByteStore, ByteStoreError, CacheError, IntentFailureKind, StorageKey, TransferOperation,
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
const ENV_METADATA_DB: &str = "BOWLINE_METADATA_DB";
const EXIT_USAGE: u8 = 2;
const EXIT_FAILURE: u8 = 1;
const WATCHER_SETTLE_WINDOW: Duration = Duration::from_millis(250);
const DEFAULT_SYNC_INTERVAL: Duration = Duration::from_secs(600);
const NOTIFICATION_POLL_INTERVAL: Duration = Duration::from_secs(30);
const STATUS_PUBLISH_INTERVAL: Duration = Duration::from_secs(60);
// Idle daemons still publish periodically so dashboards can distinguish "quiet"
// from "dead" without paying for a full unchanged heartbeat every minute.
const STATUS_PUBLISH_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(300);
const REMOTE_OBSERVER_DRAIN_INTERVAL: Duration = Duration::from_secs(1);
const REMOTE_OBSERVER_RECONNECT_INITIAL: Duration = Duration::from_secs(30);
const REMOTE_OBSERVER_RECONNECT_MAX: Duration = Duration::from_secs(900);
const SYNC_CLAIM_TIMEOUT_SECONDS: i64 = 60;
const WATCHER_DRAIN_BUDGET: usize = 512;
const MAX_DIRTY_SUBTREES: usize = 64;
// An isolated overflow (small git checkout, editor save-all) is over in a
// second or two; re-arm fast so watch fidelity returns almost immediately
// while guaranteeing the drop/recreate cycle can never spin hot.
const WATCHER_REARM_INITIAL: Duration = Duration::from_secs(2);
// Doubling 2->4->8->16->32->60 covers a typical npm install (tens of seconds)
// by the third consecutive overflow. Capped at 60s: beyond that, interval
// sync latency dominates user pain, and even a pathological storm (endless
// build churn) costs at most one minute of watch blindness per cycle while
// the forced reconcile keeps correctness.
const WATCHER_REARM_MAX: Duration = Duration::from_secs(60);
// An overflow this long after the previous one is a fresh event, not a
// continuing storm -- restart the backoff ladder at WATCHER_REARM_INITIAL.
const WATCHER_OVERFLOW_RESET_WINDOW: Duration = Duration::from_secs(300);
// This many consecutive start_sync_watcher errors means the watch backend
// is genuinely broken (fd exhaustion, root gone) -- enter Limited, the same
// terminal state a construction failure produces today.
const WATCHER_REARM_FAILURE_LIMIT: u32 = 5;
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
mod store_access;
mod store_health;
mod sync;
mod watcher;

#[cfg(test)]
use hosted_context::CountingHostedContextFactory;
use hosted_context::{
    HOSTED_CONTEXT_TRUST_REFRESH_INTERVAL, HostedContext, HostedContextCache,
    HostedContextResolver, hosted_context_resolver,
};

#[cfg(test)]
fn test_hosted_context_resolver() -> HostedContextResolver {
    hosted_context_resolver(Arc::new(HostedContextCache::new()))
}

#[cfg(test)]
mod store_health_recovery_tests;
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
use control_plane::{
    HostedSetupError, hosted_control_plane, key_store, require_convex_url, runtime_error,
    workspace_key_bytes,
};
#[cfg(test)]
use protocol::handshake;
use protocol::{
    current_timestamp, format_timestamp, json_string, request_shutdown, serve, status_snapshot,
    validate_agent_tool_contract,
};
use server_state::{DaemonServerState, ShutdownPhase, ShutdownReason, StatusSubscription};
#[cfg(test)]
use status::sync_operation_counts_json;
#[cfg(test)]
use status::sync_status_with_hosted_calls;
use status::{
    StatusPublishOutcome, StatusPublishPayload, StatusPublishRequest, StatusPublisher,
    SyncOperationCountsJson, WatcherRuntimeStateJson, daemon_json,
    hosted_status_publisher_with_context, initial_sync_status_json, waiting_queue_status_parts,
};
use store_access::CachedStore;
#[cfg(test)]
use store_access::open_store_for_test;
#[cfg(test)]
use sync::{
    ConflictSummary, DispatchClaimer, RemoteObserverState, RemoteRefObserver, SyncExecutor,
    SyncFailureAction, SyncOnceError, SyncOnceSummary, claim_pending_dispatched_lease_with,
    drain_policy, hosted_sync_executor, invalidate_policy_cache_for_path, local_metadata_sweep_due,
    noop_dispatch_claimer, remote_observer_reconnect_delay,
    remote_ref_observer_with_stream_starter, requeue_startup_sync_claims_with_resolved_attention,
    retry_delay_seconds, run_sync_once_with,
};
use sync::{
    ContinuousSyncOptions, ContinuousSyncRuntime, DaemonRuntime, SyncOnceArgs, run_sync_once,
};
use watcher::{
    WatcherRecovery, WatcherRuntimeState, WatcherSignal, stable_token, start_sync_watcher,
};
#[cfg(test)]
use watcher::{send_watcher_signal, watcher_rearm_delay, watcher_relative_path};
