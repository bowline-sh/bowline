use super::*;

struct FailingNotificationSender;

impl NotificationSender for FailingNotificationSender {
    fn send(
        &self,
        _payload: &bowline_local::notifications::NotificationPayload,
    ) -> Result<(), bowline_local::notifications::NotificationSendError> {
        Err(bowline_local::notifications::NotificationSendError::Failed(
            "injected".to_string(),
        ))
    }
}

#[derive(Default)]
struct RecordingNotificationSender {
    sent: Mutex<Vec<bowline_local::notifications::NotificationPayload>>,
}

impl NotificationSender for RecordingNotificationSender {
    fn send(
        &self,
        payload: &bowline_local::notifications::NotificationPayload,
    ) -> Result<(), bowline_local::notifications::NotificationSendError> {
        self.sent
            .lock()
            .expect("recording sender")
            .push(payload.clone());
        Ok(())
    }
}

fn snapshot(sequence: u64) -> CachedDaemonStatus {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let fixture = std::fs::read_to_string(root.join("tests/contracts/status/healthy.json"))
        .expect("healthy status fixture");
    CachedDaemonStatus {
        instance_id: "daemon-test".to_string(),
        sequence,
        status: serde_json::from_str(&fixture).expect("typed healthy status fixture"),
    }
}

#[test]
fn slow_subscription_keeps_only_newest_snapshot_and_marks_gap() {
    let subscription = StatusSubscription::new("subscription-test".to_string(), None);
    subscription.publish(snapshot(2));
    subscription.publish(snapshot(3));

    let (pending, gap) = subscription.take_pending().expect("newest snapshot");
    assert_eq!(pending.sequence, 3);
    assert!(gap);
    assert!(subscription.take_pending().is_none());
}

#[test]
fn cancelled_subscription_drops_pending_and_future_snapshots() {
    let subscription = StatusSubscription::new("subscription-test".to_string(), None);
    subscription.publish(snapshot(2));
    subscription.cancel();
    subscription.publish(snapshot(3));

    assert!(subscription.is_cancelled());
    assert!(subscription.take_pending().is_none());
}

#[test]
fn subscription_projection_wakes_are_bounded_and_coalesced() {
    let (wake, woke) = crossbeam_channel::bounded(1);
    let subscription = StatusSubscription::new("subscription-test".to_string(), Some(wake));

    subscription.publish(snapshot(2));
    subscription.publish(snapshot(3));

    woke.recv_timeout(Duration::from_secs(1))
        .expect("projection update wakes connection pump");
    assert!(
        woke.try_recv().is_err(),
        "latest-only updates share one wake"
    );
    assert_eq!(
        subscription
            .take_pending()
            .expect("latest pending snapshot")
            .0
            .sequence,
        3
    );
}

#[test]
fn sixty_unchanged_daemon_ticks_keep_one_build_and_sequence() {
    let runtime = DaemonRuntime {
        sync: None,
        notify_approvals: false,
        notification_dedupe: Arc::new(Mutex::new(NotificationDedupe::default())),
        next_notification_poll: Instant::now(),
        pending_notification_status: None,
    };
    let state = DaemonServerState::new(&runtime).expect("daemon state");
    for expected in 1..=60 {
        state.send_projection_event(StatusInputEvent::SourceChanged(StatusSource::Metadata));
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let metrics = state.test_projection_metrics();
            if metrics.no_op_refreshes == expected {
                break;
            }
            assert!(Instant::now() < deadline, "unchanged tick deadline");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    let projection = state.current_projection();
    let metrics = state.test_projection_metrics();
    assert_eq!(projection.sequence.get(), 1);
    assert_eq!(metrics.semantic_changes, 0);
    assert_eq!(metrics.broadcasts, 0);
    assert_eq!(
        metrics.collector_calls.get(&StatusSource::Metadata),
        Some(&61)
    );
    assert_eq!(
        metrics.collector_skips.get(&StatusSource::Metadata),
        Some(&60)
    );
    assert_eq!(
        metrics
            .builds_by_reason
            .get(&ProjectionBuildReason::Initial),
        Some(&1)
    );
    assert_eq!(
        metrics
            .builds_by_reason
            .get(&ProjectionBuildReason::SourceChanged),
        Some(&60)
    );
}

#[test]
fn rpc_instance_identity_is_the_sync_claimant_identity() {
    let state_root = std::env::temp_dir().join(format!(
        "bowline-daemon-identity-{}-{}",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    let sync = crate::daemon::tests::watcher_test_runtime(
        state_root.join("Code"),
        state_root.clone(),
        "ws_identity",
    );
    let claimant_id = sync.claimant_id.clone();
    let runtime = DaemonRuntime {
        sync: Some(sync),
        notify_approvals: false,
        notification_dedupe: Arc::new(Mutex::new(NotificationDedupe::default())),
        next_notification_poll: Instant::now(),
        pending_notification_status: None,
    };

    let state = DaemonServerState::new(&runtime).expect("daemon state");

    assert_eq!(state.instance_id(), claimant_id);
    let _ = fs::remove_dir_all(state_root);
}

#[test]
fn pending_device_trust_adds_canonical_status_and_local_action_affordances() {
    let trust = DeviceApprovalRequestList {
        pending_requests: vec![bowline_control_plane::DeviceRequest {
            request_id: bowline_core::ids::DeviceApprovalRequestId::new("request-1"),
            workspace_id: WorkspaceId::new("workspace-1"),
            device_id: DeviceId::new("device-new"),
            device_name: "New Mac".to_string(),
            platform: "macos".to_string(),
            device_public_key: "public-key".to_string(),
            device_fingerprint: "fingerprint".to_string(),
            device_authorization_proof_verifier: "verifier".to_string(),
            matching_code: "bowline-0123456789abcdef".to_string(),
            account_id: None,
            host: None,
            lease_handoff_digest: None,
            lease_id: None,
            root: None,
            runtime: None,
            setup_receipts_digest: None,
            requested_at: ControlPlaneTimestamp { tick: 1 },
            expires_at: ControlPlaneTimestamp { tick: 2 },
            state: bowline_control_plane::DeviceRequestState::Pending,
        }],
        authorized_devices: Vec::new(),
        revoked_devices: Vec::new(),
    };

    let status = device_trust_status_facts(&trust, Some(Path::new("/tmp/Code")));

    assert_eq!(status.approvals[0].request_id, "request-1");
    assert_eq!(status.approvals[0].device_name, "New Mac");
    assert_eq!(status.approvals[0].code, "0123-4567");
    assert!(
        status.approvals[0]
            .approve_command
            .contains("bowline device approve")
    );
    let fact = status
        .facts
        .iter()
        .find(|fact| fact.kind.as_str() == "device.approval_requested")
        .expect("canonical approval fact");
    assert_eq!(fact.scope, StatusFactScope::Device);
    assert_eq!(fact.scope_id.as_deref(), Some("device-new"));
    assert_eq!(
        fact.action
            .as_ref()
            .and_then(|action| action.target_id.as_deref()),
        Some("request-1")
    );
    let item = status
        .items
        .iter()
        .find(|item| {
            item.subject.as_ref().is_some_and(|subject| {
                subject.kind == StatusSubjectKind::DeviceApprovalRequest
                    && subject.id == "request-1"
            })
        })
        .expect("canonical approval item");
    assert_eq!(item.kind, StatusItemKind::Device);
    assert_eq!(
        item.device_id.as_ref().map(DeviceId::as_str),
        Some("device-new")
    );
    assert!(item.summary.contains("waiting for local approval"));
    assert!(!item.summary.contains("0123-4567"));
}

#[test]
fn pending_device_projection_is_identical_across_rpc_hosted_and_notifications() {
    let state_root = std::env::temp_dir().join(format!(
        "bowline-daemon-device-projection-{}-{}",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    let sync = crate::daemon::tests::watcher_test_runtime(
        state_root.join("Code"),
        state_root.clone(),
        "ws_device_projection",
    );
    let mut runtime = DaemonRuntime {
        sync: Some(sync),
        notify_approvals: true,
        notification_dedupe: Arc::new(Mutex::new(NotificationDedupe::default())),
        next_notification_poll: Instant::now(),
        pending_notification_status: None,
    };
    let state = DaemonServerState::new(&runtime).expect("daemon state");
    let first = state
        .subscribe_with_snapshot(None)
        .expect("first subscription")
        .0;
    let second = state
        .subscribe_with_snapshot(None)
        .expect("second subscription")
        .0;
    let trust = DeviceApprovalRequestList {
        pending_requests: vec![bowline_control_plane::DeviceRequest {
            request_id: bowline_core::ids::DeviceApprovalRequestId::new("request-shared"),
            workspace_id: WorkspaceId::new("ws_device_projection"),
            device_id: DeviceId::new("device-shared"),
            device_name: "Shared Mac".to_string(),
            platform: "macos".to_string(),
            device_public_key: "public-key".to_string(),
            device_fingerprint: "fingerprint".to_string(),
            device_authorization_proof_verifier: "verifier".to_string(),
            matching_code: "bowline-89abcdef01234567".to_string(),
            account_id: None,
            host: None,
            lease_handoff_digest: None,
            lease_id: None,
            root: None,
            runtime: None,
            setup_receipts_digest: None,
            requested_at: ControlPlaneTimestamp { tick: 1 },
            expires_at: ControlPlaneTimestamp { tick: 2 },
            state: bowline_control_plane::DeviceRequestState::Pending,
        }],
        authorized_devices: Vec::new(),
        revoked_devices: Vec::new(),
    };
    state.update_projection_source(
        &state.projection_sources.device_trust,
        StatusSourceFacts::DeviceTrustDetails(device_trust_status_facts(
            &trust,
            Some(state_root.join("Code").as_path()),
        )),
    );
    assert!(
        !state
            .projection_sources
            .device_trust
            .update(StatusSourceFacts::DeviceTrustDetails(
                device_trust_status_facts(&trust, Some(state_root.join("Code").as_path()),)
            ),)
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    let projection = loop {
        let current = state.current_projection();
        if current.sequence.get() == 2 {
            break current;
        }
        assert!(Instant::now() < deadline, "projection update deadline");
        std::thread::sleep(Duration::from_millis(5));
    };
    state.publish_rpc_projection(&projection);

    let first = first.take_pending().expect("first projection").0;
    let second = second.take_pending().expect("second projection").0;
    assert_eq!(first.instance_id, second.instance_id);
    assert_eq!(first.sequence, 2);
    assert_eq!(first.status, second.status);
    assert_eq!(
        projection.status.device_approvals[0].request_id,
        "request-shared"
    );
    let hosted =
        bowline_local::status::redacted_status_snapshot(&projection.status, "publishing-device");
    assert!(hosted.facts.iter().any(|fact| {
        fact.kind.as_str() == "status.aggregate_input"
            && fact.attention_impact == bowline_core::status::StatusAttention::Required
    }));
    let payloads = pending_device_payloads(&projection.status);
    assert_eq!(payloads.len(), 1);
    assert!(
        payloads[0]
            .action
            .as_deref()
            .is_some_and(|action| action.contains("89ab-cdef"))
    );

    let projection_input = state.test_projection_input();
    let failed = runtime.dispatch_projection_notifications_with(
        &projection.status,
        &FailingNotificationSender,
        &projection_input,
    );
    assert_eq!(failed.failures.len(), 1);
    let recording = RecordingNotificationSender::default();
    let sent = runtime.dispatch_projection_notifications_with(
        &projection.status,
        &recording,
        &projection_input,
    );
    assert_eq!(sent.sent, 1);
    let deduped = runtime.dispatch_projection_notifications_with(
        &projection.status,
        &recording,
        &projection_input,
    );
    assert_eq!(deduped.skipped, 1);
    let metrics = state.test_projection_metrics();
    assert_eq!(metrics.semantic_changes, 1);
    assert_eq!(metrics.broadcasts, 1);
    assert_eq!(metrics.rpc_serializations, 2);
    assert_eq!(metrics.notification_candidates, 3);
    assert_eq!(metrics.notification_failures, 1);
    assert_eq!(metrics.notification_sent, 1);
    assert_eq!(metrics.notification_suppressed, 1);
    let _ = fs::remove_dir_all(state_root);
}

#[test]
fn requested_scope_accepts_absolute_and_home_relative_workspace_roots() {
    let home = env::var_os("HOME").map(PathBuf::from).expect("HOME is set");
    let configured = home.join("Code");

    assert!(requested_root_matches("~/Code", &configured));
    assert!(requested_root_matches(
        &configured.display().to_string(),
        &configured
    ));
    assert!(!requested_root_matches("~/Different", &configured));
}
