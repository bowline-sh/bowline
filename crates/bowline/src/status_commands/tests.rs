use super::*;
use bowline_core::status::StatusLevel;

#[test]
fn persisted_remote_session_does_not_probe_desktop_secret_store() {
    let authenticated = account_context_available_from_sources(true, false, || {
        panic!("persisted remote authentication must not probe a desktop secret store")
    });

    assert!(authenticated);
}

#[test]
fn missing_session_fails_closed_when_passive_secret_probe_is_unavailable() {
    let authenticated = account_context_available_from_sources(false, false, || {
        panic!("unavailable secret stores must not be probed")
    });

    assert!(!authenticated);
}

#[test]
fn persisted_remote_session_allows_trust_fetch_without_interactive_tokens() {
    let available = account_context_available_from_sources(true, true, || {
        panic!("Mac-enrolled devices must not require interactive account tokens")
    });

    assert!(available);
}

fn revision_watch_fixture() -> (PathBuf, WorkspaceId) {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = env::temp_dir().join(format!("bowline-status-watch-{unique}"));
    let db_path = root.join("local.sqlite3");
    let workspace_id = WorkspaceId::new(format!("ws_watch_{unique}"));
    let store = bowline_local::metadata::MetadataStore::open(&db_path).expect("metadata opens");
    store
        .insert_workspace(&workspace_id, "Watch test", "2026-07-12T12:00:00Z")
        .expect("workspace insert");
    store
        .insert_root(
            "root_watch",
            &workspace_id,
            "~/Code",
            "2026-07-12T12:00:00Z",
        )
        .expect("root insert");
    drop(store);
    (db_path, workspace_id)
}

fn watch_test_socket() -> PathBuf {
    // A socket no daemon serves, so the sync fallback stays `None` and watch
    // frames remain deterministic for byte-equality assertions.
    PathBuf::from("/tmp/bowline-status-watch-test-unreachable.sock")
}

fn uncached_watch_frame(options: &StatusOptions, sequence: u64) -> WatchFrame {
    let mut output =
        bowline_local::status::compose_status(options.clone()).expect("uncached composition");
    attach_update_status_if_available(&mut output, false);
    abbreviate_status_requested_path(&mut output);
    status_watch_frame(output, sequence)
}

/// The machine-introspection block is derived from live service/trust/daemon
/// state that varies by environment and watch-loop cache, so this test compares
/// only the deterministic composed body. Introspection presence has its own test.
fn without_introspection(mut frame: WatchFrame) -> WatchFrame {
    if let WatchFrame::Status { status, .. } = &mut frame {
        status.service = None;
        status.authentication = None;
        status.sync = None;
    }
    frame
}

#[test]
fn status_watch_sixty_unchanged_ticks_retain_one_store_and_frame_contract() {
    let (db_path, _) = revision_watch_fixture();
    let options = StatusOptions {
        db_path: Some(db_path.clone()),
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-07-12T12:00:00Z".to_string(),
    };
    let expected_frame = uncached_watch_frame(&options, 1);
    let expected_bytes = serde_json::to_vec(&expected_frame).expect("expected frame json");
    let mut state = StatusWatchState::new(watch_test_socket());

    let WatchTick::Frame(first) = next_status_watch_tick(&mut state, &options) else {
        panic!("first tick must emit status");
    };
    assert_eq!(
        serde_json::to_vec(&without_introspection(first)).expect("actual frame json"),
        expected_bytes
    );
    for _ in 0..60 {
        assert!(matches!(
            next_status_watch_tick(&mut state, &options),
            WatchTick::Unchanged
        ));
    }
    let metrics = state.composer.as_ref().expect("composer").metrics();
    assert_eq!(metrics.full_compositions, 1);
    assert_eq!(metrics.store_opens, 1);

    let external =
        bowline_local::metadata::MetadataStore::open(&db_path).expect("external metadata opens");
    external
        .append_event(bowline_core::events::WorkspaceEvent::new(
            bowline_core::ids::EventId::new("evt_watch_changed"),
            bowline_core::events::EventName::HydrationCompleted,
            "2026-07-12T12:00:01Z",
            bowline_core::events::EventSeverity::Info,
            "Watch revision changed.",
            state
                .last_output
                .as_ref()
                .expect("last output")
                .workspace_id
                .clone(),
        ))
        .expect("external commit");
    drop(external);
    assert!(matches!(
        next_status_watch_tick(&mut state, &options),
        WatchTick::Frame(_)
    ));
    let metrics = state.composer.as_ref().expect("composer").metrics();
    assert_eq!(metrics.full_compositions, 2);
    assert_eq!(metrics.store_opens, 1);
    assert!(matches!(
        next_status_watch_tick(&mut state, &options),
        WatchTick::Unchanged
    ));

    std::fs::remove_file(&db_path).expect("database removal");
    let expected_removed = uncached_watch_frame(&options, 3);
    let WatchTick::Frame(removed) = next_status_watch_tick(&mut state, &options) else {
        panic!("database removal must emit a status frame");
    };
    assert_eq!(
        serde_json::to_vec(&without_introspection(removed)).expect("removed frame json"),
        serde_json::to_vec(&expected_removed).expect("expected removed frame json")
    );
    let metrics = state.composer.as_ref().expect("composer").metrics();
    assert_eq!(metrics.full_compositions, 3);
    assert_eq!(metrics.store_opens, 1);

    let replacement_workspace_id = WorkspaceId::new("ws_watch_replacement");
    let replacement =
        bowline_local::metadata::MetadataStore::open(&db_path).expect("replacement metadata opens");
    replacement
        .insert_workspace(
            &replacement_workspace_id,
            "Replacement watch test",
            "2026-07-12T12:00:02Z",
        )
        .expect("replacement workspace insert");
    replacement
        .insert_root(
            "root_watch_replacement",
            &replacement_workspace_id,
            "~/Code",
            "2026-07-12T12:00:02Z",
        )
        .expect("replacement root insert");
    drop(replacement);
    let expected_replacement = uncached_watch_frame(&options, 4);
    let WatchTick::Frame(replaced) = next_status_watch_tick(&mut state, &options) else {
        panic!("database replacement must emit a status frame");
    };
    assert_eq!(
        serde_json::to_vec(&without_introspection(replaced)).expect("replacement frame json"),
        serde_json::to_vec(&expected_replacement).expect("expected replacement frame json")
    );
    assert_eq!(
        state
            .last_output
            .as_ref()
            .expect("replacement output")
            .workspace_id,
        replacement_workspace_id
    );
    let metrics = state.composer.as_ref().expect("composer").metrics();
    assert_eq!(metrics.full_compositions, 4);
    assert_eq!(metrics.store_opens, 2);
    assert!(matches!(
        next_status_watch_tick(&mut state, &options),
        WatchTick::Unchanged
    ));
    let metrics = state.composer.as_ref().expect("composer").metrics();
    assert_eq!(metrics.full_compositions, 4);
    assert_eq!(metrics.store_opens, 2);
    std::fs::remove_dir_all(db_path.parent().expect("state root")).expect("fixture cleanup");
}

#[test]
fn status_watch_frame_carries_machine_introspection() {
    let (db_path, _) = revision_watch_fixture();
    let options = StatusOptions {
        db_path: Some(db_path.clone()),
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-07-12T12:00:00Z".to_string(),
    };
    let mut state = StatusWatchState::new(watch_test_socket());
    let WatchTick::Frame(WatchFrame::Status { status, .. }) =
        next_status_watch_tick(&mut state, &options)
    else {
        panic!("first watch tick must emit a status frame");
    };
    // Watch frames must carry the same machine-introspection block as one-shot
    // `status --json`; `sync` may be absent when no queue and no daemon exist, but
    // service and authentication are always derived.
    assert!(
        status.service.is_some(),
        "watch frame must carry service introspection"
    );
    assert!(
        status.authentication.is_some(),
        "watch frame must carry authentication introspection"
    );
    std::fs::remove_dir_all(db_path.parent().expect("state root")).expect("fixture cleanup");
}

#[test]
fn status_watch_update_cache_revision_recomposes_immediately() {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = env::temp_dir().join(format!("bowline-update-revision-{unique}"));
    let db_path = root.join("missing.sqlite3");
    let cache_path = root.join("release-manifest.json");
    let options = StatusOptions {
        db_path: Some(db_path),
        requested_path: None,
        workspace_scope: true,
        generated_at: "2026-07-12T12:00:00Z".to_string(),
    };
    let mut state = StatusWatchState::new(watch_test_socket());
    assert!(matches!(
        next_status_watch_tick(&mut state, &options),
        WatchTick::Frame(_)
    ));
    refresh_update_status_revision(&mut state, update::update_status_revision_at(&cache_path));
    std::fs::create_dir_all(&root).expect("cache root");
    std::fs::write(&cache_path, r#"{"version":"999.0.0","urgency":"normal"}"#)
        .expect("cache write");
    refresh_update_status_revision(&mut state, update::update_status_revision_at(&cache_path));
    assert!(matches!(
        next_status_watch_tick(&mut state, &options),
        WatchTick::Unchanged
    ));
    assert_eq!(
        state
            .composer
            .as_ref()
            .expect("composer")
            .metrics()
            .full_compositions,
        2
    );
    std::fs::remove_dir_all(root).expect("fixture cleanup");
}

#[test]
fn status_watch_recoverable_error_frame_then_status_frame() {
    let status = bowline_local::status::compose_status(StatusOptions {
        db_path: Some(PathBuf::from("/tmp/bowline-watch-test-missing.sqlite3")),
        requested_path: Some("~/Code".to_string()),
        workspace_scope: true,
        generated_at: "2026-07-02T12:00:00Z".to_string(),
    })
    .expect("missing metadata composes limited status");
    let mut state = StatusWatchState::new(watch_test_socket());
    let mut calls = 0;
    let mut compose = || {
        calls += 1;
        if calls == 1 {
            Err(bowline_local::status::LocalStatusError::MetadataState(
                bowline_local::metadata::DatabaseState::Locked,
            ))
        } else {
            Ok(status.clone())
        }
    };

    let first = next_status_watch_tick_with(&mut state, &mut compose);
    let WatchTick::RecoverableError { frame, backoff } = first else {
        panic!("first tick should be a recoverable error frame");
    };
    assert_eq!(backoff, Duration::from_secs(1));
    assert!(matches!(frame, WatchFrame::Error { sequence: 1, .. }));

    let second = next_status_watch_tick_with(&mut state, &mut compose);
    let WatchTick::Frame(frame) = second else {
        panic!("second tick should resume with a status frame");
    };
    assert!(matches!(frame, WatchFrame::Status { sequence: 2, .. }));
    assert_eq!(calls, 2);
}

fn status_output() -> StatusCommandOutput {
    StatusCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: CommandName::Status,
        generated_at: "2026-07-02T12:00:00Z".to_string(),
        workspace_id: WorkspaceId::new("workspace_1"),
        project_id: None,
        scope: None,
        requested_path: None,
        resolved_workspace_root: Some("/tmp/workspace".to_string()),
        resolved_project_root: None,
        workspace_summary: None,
        setup_readiness: None,
        sync_queue: None,
        convergence: None,
        freshness: bowline_core::status::FreshnessVerdict::Unknown,
        stale_bases: Vec::new(),
        status: bowline_core::status::WorkspaceStatus::healthy(),
        status_summary: bowline_core::status::reduce_status_facts(
            Vec::new(),
            1,
            "2026-07-02T12:00:00Z",
        ),
        items: Vec::new(),
        limits: Vec::new(),
        event_watermarks: bowline_core::status::EventWatermarks {
            last_scan_at: None,
            last_event_id: None,
            event_lag_ms: None,
        },
        next_actions: Vec::new(),
        device_approvals: Vec::new(),
        service: None,
        authentication: None,
        sync: None,
    }
}

#[test]
fn sync_introspection_prefers_local_queue_over_socket() {
    let mut output = status_output();
    output.sync_queue = Some(bowline_core::status::SyncQueueStatus {
        queued: 3,
        ..bowline_core::status::SyncQueueStatus::default()
    });
    // With a local queue present, the unreachable override socket must not be
    // consulted; the derived introspection comes straight from the queue.
    let sync = sync_introspection_for(&output, &PathBuf::from("/tmp/bowline-r3-unreachable.sock"))
        .expect("local queue yields introspection");
    assert_eq!(sync.pending_uploads, 3);
}

#[test]
fn sync_introspection_absent_without_queue_or_daemon() {
    // No local queue and an unreachable socket: the sync field stays absent
    // rather than fabricating a settled view.
    let output = status_output();
    assert!(
        sync_introspection_for(&output, &PathBuf::from("/tmp/bowline-r3-unreachable.sock"))
            .is_none()
    );
}

#[test]
fn daemon_sync_facts_replace_stale_local_projection_for_matching_workspace() {
    let mut output = status_output();
    output.sync_queue = Some(bowline_core::status::SyncQueueStatus {
        queued: 9,
        ..bowline_core::status::SyncQueueStatus::default()
    });
    let mut daemon = status_output();
    daemon.sync_queue = Some(bowline_core::status::SyncQueueStatus::default());
    daemon.convergence = Some(bowline_core::status::ConvergenceStatusSummary {
        revision: 42,
        state: bowline_core::status::ConvergenceReadinessState::Ready,
        reasons: Vec::new(),
    });

    overlay_daemon_sync_facts(&mut output, &daemon);

    assert_eq!(
        output.sync_queue,
        Some(bowline_core::status::SyncQueueStatus::default())
    );
    assert_eq!(output.convergence, daemon.convergence);
}

#[test]
fn daemon_sync_facts_do_not_cross_workspace_identity() {
    let mut output = status_output();
    let mut daemon = status_output();
    daemon.workspace_id = WorkspaceId::new("workspace_other");
    daemon.sync_queue = Some(bowline_core::status::SyncQueueStatus::default());
    daemon.convergence = Some(bowline_core::status::ConvergenceStatusSummary {
        revision: 42,
        state: bowline_core::status::ConvergenceReadinessState::Ready,
        reasons: Vec::new(),
    });

    overlay_daemon_sync_facts(&mut output, &daemon);

    assert!(output.sync_queue.is_none());
    assert!(output.convergence.is_none());
}

#[test]
fn git_cwd_requests_project_scope_even_when_metadata_composes_workspace_scope() {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = env::temp_dir().join(format!("bowline-status-cwd-project-{unique}"));
    let nested = root.join("src");
    std::fs::create_dir_all(root.join(".git")).expect("git marker");
    std::fs::create_dir_all(&nested).expect("nested project path");
    let mut output = status_output();
    output.scope = Some(bowline_core::status::StatusScope::Workspace);
    output.requested_path = Some(nested.display().to_string());

    assert_eq!(
        project_scope_for_output(false, None, &output),
        Some(std::fs::canonicalize(&root).expect("canonical project root"))
    );
    assert_eq!(project_scope_for_output(true, None, &output), None);

    std::fs::remove_dir_all(root).expect("fixture cleanup");
}

fn empty_trust() -> DeviceTrustSnapshot {
    DeviceTrustSnapshot {
        pending_requests: Vec::new(),
        authorized_devices: Vec::new(),
        revoked_devices: Vec::new(),
    }
}

#[test]
fn approval_fact_binds_its_local_action_to_the_request() {
    let mut output = status_output();
    append_status_fact(
        &mut output,
        "device.approval_requested",
        "approval-fact",
        "approval-request",
        StatusFactScope::Device,
        Some("device_pending"),
        Some("request_pending"),
    );

    let action = output
        .status_summary
        .facts
        .first()
        .and_then(|fact| fact.action.as_ref())
        .expect("approval fact action");
    assert_eq!(action.kind, "approve-device-local");
    assert_eq!(action.target_id.as_deref(), Some("request_pending"));
}

fn authorized_device(device_id: &str) -> bowline_control_plane::AuthorizedDeviceRecord {
    bowline_control_plane::AuthorizedDeviceRecord {
        workspace_id: bowline_core::ids::WorkspaceId::new("workspace_1"),
        device_id: bowline_core::ids::DeviceId::new(device_id),
        device_name: "dev laptop".to_string(),
        platform: "macos".to_string(),
        device_fingerprint: "fingerprint_1".to_string(),
        authorized_at: bowline_control_plane::ControlPlaneTimestamp { tick: 10 },
        authorized_by_device_id: Some(bowline_core::ids::DeviceId::new("device_admin")),
        device_authorization_proof_verifier: None,
        revoked_at: None,
    }
}

fn revoked_device(device_id: &str) -> bowline_control_plane::RevokedDeviceRecord {
    bowline_control_plane::RevokedDeviceRecord {
        workspace_id: bowline_core::ids::WorkspaceId::new("workspace_1"),
        device_id: bowline_core::ids::DeviceId::new(device_id),
        device_name: "dev laptop".to_string(),
        platform: "macos".to_string(),
        device_fingerprint: "fingerprint_1".to_string(),
        revoked_at: bowline_control_plane::ControlPlaneTimestamp { tick: 20 },
        revoked_by_device_id: bowline_core::ids::DeviceId::new("device_admin"),
        reason: "rotated".to_string(),
    }
}

#[test]
fn apply_device_status_marks_revoked_local_device_unavailable_and_required() {
    let mut output = status_output();
    let local_device_id = DeviceId::new("device_local");
    let mut trust = empty_trust();
    trust.revoked_devices.push(revoked_device("device_local"));

    apply_device_status_for_local_device(&mut output, &trust, &local_device_id);

    assert_eq!(output.status.level, StatusLevel::Attention);
    assert!(
        output
            .status
            .attention_items
            .iter()
            .any(|item| item.contains("revoked"))
    );
    assert_eq!(output.items.len(), 1);
    assert_eq!(
        output.items[0]
            .device_id
            .as_ref()
            .expect("device id")
            .as_str(),
        "device_local"
    );
    assert_eq!(output.next_actions.len(), 1);
}

#[test]
fn apply_device_status_renders_authorized_local_device() {
    let mut output = status_output();
    let local_device_id = DeviceId::new("device_local");
    let mut trust = empty_trust();
    trust
        .authorized_devices
        .push(authorized_device("device_local"));

    apply_device_status_for_local_device(&mut output, &trust, &local_device_id);

    assert_eq!(output.status.level, StatusLevel::Healthy);
    assert_eq!(output.items.len(), 1);
    assert!(output.items[0].summary.contains("trusted"));
    assert_eq!(
        output.items[0]
            .device_id
            .as_ref()
            .expect("device id")
            .as_str(),
        "device_local"
    );
}

#[test]
fn apply_device_status_leaves_output_unchanged_without_trust_records() {
    let mut output = status_output();
    let original = output.clone();
    let local_device_id = DeviceId::new("device_local");

    apply_device_status_for_local_device(&mut output, &empty_trust(), &local_device_id);

    assert_eq!(output, original);
}

#[test]
fn status_watch_trust_refresh_schedule_fetches_on_thirty_second_cadence() {
    let start = Instant::now();
    let mut schedule = RefreshCadence::new(start, TRUST_REFRESH_INTERVAL);

    assert!(schedule.due(start));
    schedule.record_attempt(start);
    assert!(!schedule.due(start + Duration::from_secs(1)));
    assert!(schedule.due(start + TRUST_REFRESH_INTERVAL));

    let delayed = start + Duration::from_secs(95);
    assert!(schedule.due(delayed));
    schedule.record_attempt(delayed);
    assert!(!schedule.due(delayed + Duration::from_secs(1)));
    assert!(schedule.due(delayed + TRUST_REFRESH_INTERVAL));
}

#[test]
fn status_watch_cached_device_trust_survives_failed_refresh() {
    let workspace_id = WorkspaceId::new("workspace_1");
    let other_workspace_id = WorkspaceId::new("workspace_2");
    let mut old = empty_trust();
    old.authorized_devices.push(authorized_device("device_old"));
    let mut cached = Some(CachedDeviceTrust {
        workspace_id: workspace_id.clone(),
        trust: old,
    });

    update_cached_device_trust(&mut cached, &workspace_id, DeviceTrustFetch::Unavailable);

    assert_eq!(
        cached_device_trust_for_workspace(&cached, &workspace_id)
            .expect("cached trust")
            .authorized_devices[0]
            .device_id,
        "device_old"
    );
    assert!(cached_device_trust_for_workspace(&cached, &other_workspace_id).is_none());

    let mut fresh = empty_trust();
    fresh
        .authorized_devices
        .push(authorized_device("device_new"));
    update_cached_device_trust(
        &mut cached,
        &other_workspace_id,
        DeviceTrustFetch::Fetched(fresh),
    );

    assert_eq!(
        cached_device_trust_for_workspace(&cached, &other_workspace_id)
            .expect("cached trust")
            .authorized_devices[0]
            .device_id,
        "device_new"
    );
    assert!(cached_device_trust_for_workspace(&cached, &workspace_id).is_none());
}

fn pending_request(device_id: &str) -> bowline_control_plane::DeviceRequest {
    bowline_control_plane::DeviceRequest {
        request_id: bowline_core::ids::DeviceApprovalRequestId::new("request_1"),
        workspace_id: bowline_core::ids::WorkspaceId::new("workspace_1"),
        device_id: bowline_core::ids::DeviceId::new(device_id),
        device_name: "dev laptop".to_string(),
        platform: "macos".to_string(),
        device_public_key: "public_key".to_string(),
        device_fingerprint: "fingerprint_1".to_string(),
        device_authorization_proof_verifier: "verifier".to_string(),
        matching_code: "123456".to_string(),
        account_id: None,
        host: None,
        root: None,
        runtime: None,
        setup_receipts_digest: None,
        requested_at: bowline_control_plane::ControlPlaneTimestamp { tick: 5 },
        expires_at: bowline_control_plane::ControlPlaneTimestamp { tick: 50 },
        state: bowline_control_plane::DeviceRequestState::Pending,
    }
}

#[test]
fn authentication_state_is_unauthenticated_without_account() {
    // The unauthenticated short-circuit must not resolve a device id (which would
    // reach the control plane and materialize local metadata), so drive the
    // public reducer with a workspace id it must never consult.
    let workspace_id = WorkspaceId::new("ws_unauthenticated");
    assert_eq!(
        authentication_state(
            &workspace_id,
            &DeviceTrustFetch::Fetched(empty_trust()),
            false
        ),
        bowline_core::introspection::AuthenticationState::Unauthenticated
    );
}

#[test]
fn authentication_state_failed_fetch_does_not_raise_the_rung() {
    // A logged-in device whose trust fetch failed must not be reported as
    // authenticated: a transient control-plane error during a device-approval
    // wait must never satisfy an `authenticated`/`ready` target.
    let workspace_id = WorkspaceId::new("ws_trust_unknown");
    let state = authentication_state(&workspace_id, &DeviceTrustFetch::Unavailable, true);
    assert_eq!(
        state,
        bowline_core::introspection::AuthenticationState::ApprovalPending
    );
    assert!(state < bowline_core::introspection::AuthenticationState::Authenticated);
}

#[test]
fn authentication_state_is_authenticated_for_authorized_local_device() {
    let local = DeviceId::new("device_local");
    let mut trust = empty_trust();
    trust
        .authorized_devices
        .push(authorized_device("device_local"));
    assert_eq!(
        reduce_device_trust(&trust, &local),
        bowline_core::introspection::AuthenticationState::Authenticated
    );
}

#[test]
fn authentication_state_is_approval_pending_with_open_request() {
    let local = DeviceId::new("device_local");
    let mut trust = empty_trust();
    trust.pending_requests.push(pending_request("device_local"));
    assert_eq!(
        reduce_device_trust(&trust, &local),
        bowline_core::introspection::AuthenticationState::ApprovalPending
    );
}

#[test]
fn authentication_state_is_approval_pending_when_other_devices_are_authorized() {
    let local = DeviceId::new("device_local");
    let mut trust = empty_trust();
    trust
        .authorized_devices
        .push(authorized_device("device_other"));
    assert_eq!(
        reduce_device_trust(&trust, &local),
        bowline_core::introspection::AuthenticationState::ApprovalPending
    );
}

#[test]
fn authentication_state_is_unauthenticated_for_revoked_local_device() {
    let local = DeviceId::new("device_local");
    let mut trust = empty_trust();
    trust.revoked_devices.push(revoked_device("device_local"));
    assert_eq!(
        reduce_device_trust(&trust, &local),
        bowline_core::introspection::AuthenticationState::Unauthenticated
    );
}

#[test]
fn missing_status_target_resolves_through_its_existing_git_ancestor() {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("bowline-missing-status-target-{unique}"));
    let project = root.join("project");
    std::fs::create_dir_all(project.join(".git")).expect("git metadata");
    let missing = project.join("src/deleted.rs");

    assert_eq!(
        find_git_project_root(missing.to_str().expect("test path is Unicode")),
        Some(std::fs::canonicalize(&project).expect("canonical project"))
    );

    std::fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn project_watch_preserves_the_original_requested_path() {
    let options = StatusOptions {
        db_path: None,
        requested_path: Some("/Code/project/src/deleted.rs".to_string()),
        workspace_scope: false,
        generated_at: "2026-07-24T00:00:00Z".to_string(),
    };

    let scope = daemon_status_scope_params(&options, Some(std::path::Path::new("/Code/project")));

    assert_eq!(
        scope.requested_path.as_deref(),
        Some("/Code/project/src/deleted.rs")
    );
    assert_eq!(scope.project_path.as_deref(), Some("/Code/project"));
    assert!(scope.workspace_root.is_none());
}
