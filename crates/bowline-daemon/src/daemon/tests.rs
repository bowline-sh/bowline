use super::protocol::{SocketGuard, prepare_socket};
use super::{
    Command, ContinuousSyncRuntime, DEFAULT_DATABASE_FILE, DaemonRuntime, DaemonServerState,
    NotificationDedupe, STATUS_PUBLISH_INTERVAL, StatusPublishOutcome, StatusPublishPayload,
    StatusPublishRequest, StatusPublisher, SyncArgs, WorkspaceId, daemon_env_var, drain_policy,
    invalidate_policy_cache_for_path, load_persisted_daemon_env, parse_args, runtime_error,
    test_hosted_context_resolver, watcher_relative_path,
};
use bowline_core::policy::PathClassification;
use bowline_local::metadata::MetadataStore;
use bowline_local::policy::{PathFacts, UserPolicy as DirectPolicy, classify_path};
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, atomic::AtomicUsize, atomic::Ordering};
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};
use time::OffsetDateTime;

fn noop_status_publisher() -> StatusPublisher {
    StatusPublisher::new(|payload| {
        Ok(StatusPublishOutcome {
            fingerprint: payload.fingerprint.unwrap_or_else(|| "noop".to_string()),
        })
    })
}

#[test]
fn drain_policy_cache_matches_direct_load_and_refreshes_after_ignore_edit() {
    let root = unique_temp_dir("bowline-drain-policy-refresh");
    fs::create_dir_all(root.join("ignored")).expect("dir");
    fs::write(root.join(".bowlineignore"), "").expect("ignore");
    let path = "ignored/note.txt";
    let mut cache = HashMap::new();
    let facts = PathFacts {
        relative_path: path.into(),
        is_dir: false,
        byte_len: Some(4),
    };
    let cached = classify_path(&facts, drain_policy(&root, path, &mut cache));
    let direct = classify_path(&facts, &DirectPolicy::load_for_path(&root, path).unwrap());
    assert_eq!(cached, direct);
    assert_eq!(cached.classification, PathClassification::WorkspaceSync);
    fs::write(root.join(".bowlineignore"), "ignored/**\n").expect("ignore update");
    invalidate_policy_cache_for_path(".bowlineignore", &mut cache);
    let cached = classify_path(&facts, drain_policy(&root, path, &mut cache));
    let direct = classify_path(&facts, &DirectPolicy::load_for_path(&root, path).unwrap());
    assert_eq!(cached, direct);
    assert_eq!(cached.classification, PathClassification::LocalOnly);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn parses_serve_once_socket() {
    let cli = parse_args([
        "serve",
        "--once",
        "--socket",
        "/tmp/bowline-daemon-test.sock",
    ]);

    assert_eq!(cli.socket, PathBuf::from("/tmp/bowline-daemon-test.sock"));
    assert_eq!(cli.command, Command::Serve { once: true });
}

#[test]
fn default_socket_path_uses_per_user_runtime_dir() {
    let cli = parse_args(["status"]);
    let socket = cli.socket.display().to_string();

    assert!(socket.ends_with("/bowline/runtime/bowline-daemon.sock"));
    assert_ne!(socket, "/tmp/bowline-daemon.sock");
}

#[test]
fn prepare_socket_creates_owner_only_runtime_dir() {
    let temp = unique_temp_dir("bowline-daemon-socket-perms");
    let socket = temp.join("runtime").join("bowline-daemon.sock");

    prepare_socket(&socket).expect("prepare socket");

    let mode = fs::metadata(socket.parent().expect("socket parent"))
        .expect("runtime dir metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700);
    assert!(!socket.exists());
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn socket_owner_removes_control_socket_when_shutdown_scope_exits() {
    let temp = PathBuf::from("/tmp").join(format!(
        "blds-{}-{}",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    let socket = temp.join("d.sock");
    prepare_socket(&socket).expect("prepare socket");
    let listener = super::UnixListener::bind(&socket).expect("bind socket");
    assert!(socket.exists());

    let owner = SocketGuard {
        path: Some(socket.clone()),
    };
    drop(listener);
    drop(owner);

    assert!(!socket.exists(), "shutdown scope removes socket state");
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn persisted_daemon_env_loads_allowlisted_values_without_shell() {
    let temp = unique_temp_dir("bowline-daemon-env-overlay");
    fs::write(
        temp.join("daemon.env"),
        "BOWLINE_DEVICE_NAME=mac mini\nBOWLINE_WORKOS_REFRESH_TOKEN=secret-refresh\n",
    )
    .expect("daemon env");

    load_persisted_daemon_env(&temp);

    assert_eq!(
        daemon_env_var("BOWLINE_DEVICE_NAME").as_deref(),
        Some("mac mini")
    );
    assert_eq!(daemon_env_var("BOWLINE_WORKOS_REFRESH_TOKEN"), None);
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn parses_version_flags() {
    let cli = parse_args(["--version"]);
    assert_eq!(cli.command, Command::Version);

    let cli = parse_args(["-V", "--json"]);
    assert!(cli.json);
    assert_eq!(cli.command, Command::Version);
}

#[test]
fn parses_continuous_sync_for_serve() {
    let cli = parse_args([
        "serve",
        "--sync-root",
        "/tmp/code",
        "--sync-state-root",
        "/tmp/state",
        "--sync-workspace",
        "ws_custom",
        "--sync-device",
        "device_custom",
    ]);
    let sync = cli.continuous_sync.expect("sync args should be configured");

    assert_eq!(sync.root, PathBuf::from("/tmp/code"));
    assert_eq!(sync.state_root, PathBuf::from("/tmp/state"));
    assert_eq!(sync.workspace_id, "ws_custom");
    assert_eq!(sync.device_id, "device_custom");
}

#[test]
fn parses_notify_approvals_for_continuous_serve() {
    let cli = parse_args([
        "serve",
        "--sync-root",
        "/tmp/code",
        "--sync-state-root",
        "/tmp/state",
        "--notify-approvals",
    ]);

    assert!(cli.notify_approvals);
    assert_eq!(cli.command, Command::Serve { once: false });
    assert!(cli.continuous_sync.is_some());
}

#[test]
fn manifest_engine_pending_publishes_limited_status_and_retries_on_backoff() {
    use bowline_local::sync::manifest_engine::EnginePhase;

    let temp = unique_temp_dir("bowline-daemon-manifest-pending");
    let mut runtime =
        watcher_test_runtime(temp.join("Code"), temp.join(".state"), "ws_engine_pending");

    // A workspace whose driver could not be built presents `limited` status.
    runtime.simulate_manifest_engine_unavailable();
    let handle = runtime.manifest_snapshot_handle();
    assert_eq!(
        handle.current().phase,
        EnginePhase::Stopped,
        "pending rebuild publishes the limited host-status snapshot"
    );

    // The retry is gated by the backoff deadline and wakes the scheduler loop.
    let retry_at = runtime
        .next_manifest_retry()
        .expect("a pending rebuild schedules a retry");
    assert!(!runtime.retry_manifest_engine(retry_at - Duration::from_millis(1)));
    assert!(
        runtime.manifest_event_sender().is_none(),
        "no engine events flow while the driver is pending"
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn initial_background_build_failure_uses_the_initial_retry_delay() {
    let temp = unique_temp_dir("bowline-daemon-manifest-initial-retry");
    let mut runtime = ContinuousSyncRuntime::new(SyncArgs {
        root: temp.join("Code"),
        state_root: temp.join(".state"),
        workspace_id: "ws_initial_retry".to_string(),
        device_id: "device-test".to_string(),
    });
    let first_attempt = runtime
        .next_manifest_retry()
        .expect("an unattempted background build is immediately due");

    assert!(!runtime.retry_manifest_engine(first_attempt));
    let retry_at = runtime
        .next_manifest_retry()
        .expect("a failed initial build schedules a retry");
    assert_eq!(
        retry_at.duration_since(first_attempt),
        Duration::from_secs(1)
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn dead_engine_thread_demotes_active_host_to_pending_rebuild() {
    let temp = unique_temp_dir("bowline-daemon-dead-engine-thread");
    let mut runtime =
        watcher_test_runtime(temp.join("Code"), temp.join(".state"), "ws_dead_thread");

    // An `Active` host whose engine thread has already exited (panic / unexpected
    // return) is a dead driver: it still hands out an event sender and a stale
    // snapshot, so nothing else signals the failure.
    runtime.simulate_active_manifest_engine_with_exited_thread();
    assert!(
        runtime.manifest_event_sender().is_some(),
        "the dead-but-Active driver still exposes a sender before demotion"
    );
    // The dead thread schedules an immediate rebuild rather than looking idle.
    let retry_at = runtime
        .next_manifest_retry()
        .expect("a dead engine thread is due for an immediate rebuild");

    // The retry demotes the dead host; the rebuild then fails (no workspace key in
    // this harness), leaving the host `PendingRebuild` with no live event flow.
    assert!(!runtime.retry_manifest_engine(retry_at));
    assert!(
        runtime.manifest_event_sender().is_none(),
        "a demoted host stops feeding events to the dead driver"
    );
    assert!(
        runtime.next_manifest_retry().is_some(),
        "the demoted host keeps a pending-rebuild retry scheduled"
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn watcher_relative_path_rejects_absolute_paths_outside_root() {
    assert_eq!(
        watcher_relative_path(
            PathBuf::from("/tmp/Code").as_path(),
            PathBuf::from("/etc/passwd").as_path()
        ),
        None
    );
}

mod watcher_root_routing_tests;

#[test]
fn parses_status_json() {
    let cli = parse_args(["status", "--json"]);

    assert!(cli.json);
    assert_eq!(cli.command, Command::Status);
}

#[test]
fn running_manifest_thread_without_initial_ref_value_is_not_observer_ready() {
    let fixture = watcher_fixture(
        "bowline-observer-initial-value-readiness",
        "ws_observer_initial_value",
    );
    let mut sync = watcher_test_runtime(
        fixture.root,
        fixture.state_root,
        fixture.workspace_id.as_str(),
    );
    let driver = bowline_daemon::manifest_driver::ManifestDriver::spawn(|inbox, _sink| {
        while let Ok(event) = inbox.recv() {
            if matches!(
                event,
                bowline_local::sync::manifest_engine::EngineEvent::Shutdown
            ) {
                break;
            }
        }
    })
    .expect("stub manifest driver starts");
    sync.manifest_engine = super::sync::ManifestEngineHost::Active(driver);
    let runtime = DaemonRuntime {
        sync: Some(sync),
        notify_approvals: false,
        notification_dedupe: Arc::new(Mutex::new(NotificationDedupe::default())),
        next_notification_poll: Instant::now(),
        pending_notification_status: None,
    };

    assert_eq!(
        super::server_state::runtime_adapter_observer_state(&runtime),
        bowline_daemon::status_projection::StatusSourceState::Degraded
    );
}

#[test]
fn remote_observer_reconnects_promptly_with_bounded_backoff() {
    let delays = (1..=8)
        .map(super::sync::remote_observer_reconnect_delay)
        .collect::<Vec<_>>();

    assert_eq!(
        delays,
        vec![
            Duration::from_millis(250),
            Duration::from_millis(500),
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(4),
            Duration::from_secs(5),
            Duration::from_secs(5),
            Duration::from_secs(5),
        ]
    );
}

pub(super) fn watcher_test_runtime(
    root: PathBuf,
    state_root: PathBuf,
    workspace_id: &str,
) -> ContinuousSyncRuntime {
    fs::create_dir_all(&root).expect("watcher test root exists");
    let store = MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE))
        .expect("watcher test metadata opens");
    store
        .insert_workspace(
            &WorkspaceId::new(workspace_id),
            "Code",
            "2026-07-18T00:00:00Z",
        )
        .expect("watcher test workspace registers");
    drop(store);
    let manifest_snapshot = bowline_daemon::manifest_driver::shared_engine_snapshot();
    manifest_snapshot
        .0
        .publish(bowline_daemon::manifest_driver::host_status_snapshot());
    ContinuousSyncRuntime {
        args: SyncArgs {
            root,
            state_root,
            workspace_id: workspace_id.to_string(),
            device_id: "device-test".to_string(),
        },
        watcher: None,
        change_rx: None,
        status_publisher: noop_status_publisher(),
        next_status_publish: Instant::now() + STATUS_PUBLISH_INTERVAL,
        last_status_publish_fingerprint: None,
        last_status_publish_at: None,
        last_status_publish_failed_at: None,
        hosted_resolver: test_hosted_context_resolver(),
        claimant_id: "daemon-test".to_string(),
        manifest_engine: super::sync::ManifestEngineHost::PendingRebuild {
            next_attempt: Instant::now(),
            backoff: Some(Duration::from_secs(1)),
        },
        manifest_snapshot,
        manifest_counters: bowline_local::sync::manifest_engine::EngineCounters::shared(),
    }
}

#[test]
fn unchanged_status_heartbeats_reuse_composition_until_independent_safety_deadline() {
    let fixture = watcher_fixture(
        "bowline-status-heartbeat-suppression",
        "ws_status_heartbeat",
    );
    let mut runtime = watcher_test_runtime(
        fixture.root.clone(),
        fixture.state_root.clone(),
        fixture.workspace_id.as_str(),
    );
    let projection_runtime = DaemonRuntime {
        sync: Some(watcher_test_runtime(
            fixture.root.clone(),
            fixture.state_root.clone(),
            fixture.workspace_id.as_str(),
        )),
        notify_approvals: false,
        notification_dedupe: Arc::new(Mutex::new(NotificationDedupe::default())),
        next_notification_poll: Instant::now(),
        pending_notification_status: None,
    };
    let projection_state =
        DaemonServerState::new(&projection_runtime).expect("projection daemon state");
    let projection = projection_state.current_projection();
    let projection_input = projection_state.test_projection_input();
    let publishes = Arc::new(Mutex::new(Vec::<StatusPublishPayload>::new()));
    let captured = Arc::clone(&publishes);
    runtime.status_publisher = StatusPublisher::new(move |payload| {
        let fingerprint = payload
            .fingerprint
            .clone()
            .unwrap_or_else(|| "direct-status-publish".to_string());
        captured.lock().expect("publish capture lock").push(payload);
        Ok(StatusPublishOutcome { fingerprint })
    });

    let start = Instant::now();
    runtime.next_status_publish = start;
    runtime.publish_projection_status(&projection, false, start, &projection_input);
    assert_eq!(publishes.lock().expect("published").len(), 1);
    assert!(
        publishes.lock().expect("published")[0].snapshot.is_some(),
        "heartbeat publish should reuse the precomposed status snapshot"
    );

    for tick in 1..60 {
        runtime.publish_projection_status(
            &projection,
            true,
            start + STATUS_PUBLISH_INTERVAL * tick,
            &projection_input,
        );
        assert_eq!(
            runtime.next_status_publish,
            start + STATUS_PUBLISH_INTERVAL * (tick + 1),
            "a deduplicated publish must advance the coordinator deadline"
        );
    }
    assert_eq!(publishes.lock().expect("published").len(), 12);

    runtime.publish_projection_status(
        &projection,
        true,
        start + STATUS_PUBLISH_INTERVAL * 60,
        &projection_input,
    );
    assert_eq!(publishes.lock().expect("published").len(), 13);
    assert_eq!(projection.sequence.get(), 1);
}

#[test]
fn failed_hosted_projection_publish_retries_without_changing_local_sequence() {
    let fixture = watcher_fixture("bowline-status-publish-retry", "ws_status_publish_retry");
    let projection_runtime = DaemonRuntime {
        sync: Some(watcher_test_runtime(
            fixture.root.clone(),
            fixture.state_root.clone(),
            fixture.workspace_id.as_str(),
        )),
        notify_approvals: false,
        notification_dedupe: Arc::new(Mutex::new(NotificationDedupe::default())),
        next_notification_poll: Instant::now(),
        pending_notification_status: None,
    };
    let projection_state =
        DaemonServerState::new(&projection_runtime).expect("projection daemon state");
    let projection = projection_state.current_projection();
    let projection_input = projection_state.test_projection_input();
    let initial_sequence = projection.sequence;
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&attempts);
    let mut runtime = watcher_test_runtime(
        fixture.root.clone(),
        fixture.state_root.clone(),
        fixture.workspace_id.as_str(),
    );
    runtime.status_publisher = StatusPublisher::new(move |payload| {
        if observed.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(runtime_error("injected hosted publish failure"));
        }
        Ok(StatusPublishOutcome {
            fingerprint: payload.fingerprint.expect("projection fingerprint"),
        })
    });

    let start = Instant::now();
    runtime.publish_projection_status(&projection, false, start, &projection_input);
    assert_eq!(runtime.last_status_publish_fingerprint, None);
    runtime.publish_projection_status(
        &projection,
        false,
        start + Duration::from_secs(1),
        &projection_input,
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "projection churn must not bypass failed-publish backoff"
    );
    runtime.retry_projection_status_if_due(
        &projection,
        start + STATUS_PUBLISH_INTERVAL,
        &projection_input,
    );

    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert!(runtime.last_status_publish_fingerprint.is_some());
    assert_eq!(projection.sequence, initial_sequence);
    let metrics = projection_state.test_projection_metrics();
    assert_eq!(metrics.hosted_serializations, 3);
    assert_eq!(metrics.hosted_publish_attempts, 2);
    assert_eq!(metrics.hosted_publish_failures, 1);
    assert_eq!(metrics.hosted_publish_successes, 1);
}

#[test]
fn status_publish_rejects_projection_for_a_different_workspace() {
    let fixture = watcher_fixture(
        "bowline-status-workspace-mismatch",
        "ws_status_projection_source",
    );
    let projection_runtime = DaemonRuntime {
        sync: Some(watcher_test_runtime(
            fixture.root.clone(),
            fixture.state_root.clone(),
            fixture.workspace_id.as_str(),
        )),
        notify_approvals: false,
        notification_dedupe: Arc::new(Mutex::new(NotificationDedupe::default())),
        next_notification_poll: Instant::now(),
        pending_notification_status: None,
    };
    let projection_state =
        DaemonServerState::new(&projection_runtime).expect("projection daemon state");
    let projection = projection_state.current_projection();
    let mut args = watcher_test_runtime(
        fixture.root.clone(),
        fixture.state_root.clone(),
        fixture.workspace_id.as_str(),
    )
    .args;
    args.workspace_id = "ws_different_configured".to_string();

    let error = StatusPublishPayload::from_projection(StatusPublishRequest { args }, &projection)
        .expect_err("workspace mismatch must be rejected");

    assert!(
        error
            .to_string()
            .contains("status projection workspace does not match configured daemon workspace")
    );
}

struct WatcherFixture {
    temp: PathBuf,
    root: PathBuf,
    state_root: PathBuf,
    workspace_id: WorkspaceId,
}

fn watcher_fixture(label: &str, workspace_id: &str) -> WatcherFixture {
    let temp = unique_temp_dir(label);
    let root = temp.join("Code");
    let state_root = temp.join(".state");
    fs::create_dir_all(&root).expect("root dir");
    let workspace_id = WorkspaceId::new(workspace_id);
    let store =
        MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    store
        .insert_workspace(&workspace_id, "Code", "2026-06-26T12:00:00Z")
        .expect("workspace");
    store
        .insert_root(
            "root-code",
            &workspace_id,
            &root.display().to_string(),
            "2026-06-26T12:00:00Z",
        )
        .expect("root");
    WatcherFixture {
        temp,
        root,
        state_root,
        workspace_id,
    }
}

pub(super) fn unique_temp_dir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("{label}-{}", unique_test_suffix()));
    fs::create_dir_all(&path).expect("temp dir");
    path
}

fn unique_test_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos()
}
