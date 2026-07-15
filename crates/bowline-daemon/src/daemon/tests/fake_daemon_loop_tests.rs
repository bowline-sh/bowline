use super::*;
use bowline_control_plane::{
    CompactEventKind, LeaseCreate, LeaseSessionState, LeaseUpdate, LeaseWriteTargetMode,
};
use bowline_core::ids::{LeaseId, ProjectId, SnapshotId, WorkViewId};

const HANDOFF_MATERIALIZED_STATUS: &str = "handoff-materialized";

#[test]
fn handoff_rendezvous_lets_origin_observe_materialized_on_target_host() {
    let control_plane = FakeControlPlaneClient::default();
    control_plane.create_workspace("workspace-dispatch");
    let origin_device = "device-a";
    let target_device = "device-b";

    let handoff = control_plane
        .create_lease(LeaseCreate {
            workspace_id: WorkspaceId::new("workspace-dispatch"),
            lease_id: LeaseId::new("lease-dispatch-1"),
            project_id: ProjectId::new("project-acme"),
            device_id: DeviceId::new(origin_device),
            target_device_ref: Some(target_device.to_string()),
            origin_device_ref: Some(origin_device.to_string()),
            write_target_mode: LeaseWriteTargetMode::WorkView,
            work_view_id: Some(WorkViewId::new("work-dispatch-1")),
            base_snapshot_id: SnapshotId::new("snapshot-base"),
            task_label: Some("fix dispatch rendezvous".to_string()),
            session_state: LeaseSessionState::Provisional,
            status_code: "pending".to_string(),
            expires_at: ControlPlaneTimestamp { tick: 3_600 },
        })
        .expect("origin writes handoff lease");
    assert_eq!(handoff.status_code, "pending");

    // The target host writes a read-only materialized acknowledgement — not a run
    // supervisor state — after making the workspace appear locally.
    let acknowledged = control_plane
        .update_lease(LeaseUpdate {
            workspace_id: WorkspaceId::new("workspace-dispatch"),
            lease_id: LeaseId::new("lease-dispatch-1"),
            expected_version: handoff.version,
            updated_by_device_id: DeviceId::new(target_device),
            session_state: Some(LeaseSessionState::Open),
            status_code: Some(HANDOFF_MATERIALIZED_STATUS.to_string()),
            event_kind: Some(CompactEventKind::LeaseUpdated),
        })
        .expect("target acknowledges materialization");

    let observed = control_plane
        .list_leases(&WorkspaceId::new("workspace-dispatch"))
        .expect("origin lists handoff leases")
        .into_iter()
        .find(|lease| lease.lease_id == "lease-dispatch-1")
        .expect("origin observes handoff lease");
    assert_eq!(observed.status_code, HANDOFF_MATERIALIZED_STATUS);
    assert_eq!(observed.session_state, LeaseSessionState::Open);
    assert_eq!(observed.version, acknowledged.version);
    let events = control_plane
        .list_events(&WorkspaceId::new("workspace-dispatch"))
        .expect("dispatch events");
    assert!(events.iter().any(|event| {
        event.kind == CompactEventKind::LeaseDispatched && event.subject == "lease-dispatch-1"
    }));
}

#[test]
fn daemon_poll_materializes_handoff_and_acknowledges_on_target_host() {
    let temp = unique_temp_dir("bowline-daemon-dispatch-claim-loop");
    let root = temp.join("device-b").join("Code");
    let state = temp.join("device-b").join("state");
    fs::create_dir_all(root.join("project/src")).expect("root dir");
    fs::create_dir_all(root.join("project/.git")).expect("git marker");
    fs::write(
        root.join("project/src/lib.rs"),
        "pub fn dispatch_target() {}\n",
    )
    .expect("project file");
    let control_plane = Arc::new(Mutex::new(FakeControlPlaneClient::default()));
    let byte_store = Arc::new(Mutex::new(
        LocalByteStore::open_deterministic(temp.join("objects"), 47).expect("byte store"),
    ));

    let mut daemon_b = fake_daemon_runtime(
        root.clone(),
        state.clone(),
        "workspace-dispatch-loop",
        "device-b",
        Arc::clone(&control_plane),
        byte_store,
        [47_u8; 32],
    );
    poll_until(
        &mut daemon_b,
        |runtime| sync_status_version(runtime) >= 1,
        "target initial project sync",
    );
    let store = open_store_for_test(state.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    let project = store
        .current_project_by_path(&root.join("project").display().to_string())
        .expect("project lookup")
        .expect("project synced locally");
    let base_snapshot_id = store
        .project_latest_snapshot_id(&WorkspaceId::new("workspace-dispatch-loop"), &project.id)
        .expect("project latest snapshot lookup")
        .expect("project latest snapshot");

    {
        let control_plane = control_plane.lock().expect("fake control plane lock");
        control_plane
            .create_lease(LeaseCreate {
                workspace_id: WorkspaceId::new("workspace-dispatch-loop"),
                lease_id: LeaseId::new("lease-dispatch-loop-unready"),
                project_id: ProjectId::new("project-missing"),
                device_id: DeviceId::new("device-a"),
                target_device_ref: Some("device-b".to_string()),
                origin_device_ref: Some("device-a".to_string()),
                write_target_mode: LeaseWriteTargetMode::WorkView,
                work_view_id: Some(WorkViewId::new("work-dispatch-loop-unready")),
                base_snapshot_id: SnapshotId::new("snapshot-missing"),
                task_label: Some("unready first".to_string()),
                session_state: LeaseSessionState::Provisional,
                status_code: "pending".to_string(),
                expires_at: ControlPlaneTimestamp { tick: 3_600 },
            })
            .expect("origin writes unready handoff lease");
        control_plane
            .create_lease(LeaseCreate {
                workspace_id: WorkspaceId::new("workspace-dispatch-loop"),
                lease_id: LeaseId::new("lease-dispatch-loop-1"),
                project_id: project.id.clone(),
                device_id: DeviceId::new("device-a"),
                target_device_ref: Some("device-b".to_string()),
                origin_device_ref: Some("device-a".to_string()),
                write_target_mode: LeaseWriteTargetMode::WorkView,
                work_view_id: Some(WorkViewId::new("work-dispatch-loop-1")),
                base_snapshot_id: base_snapshot_id.clone(),
                task_label: Some("fix target bug".to_string()),
                session_state: LeaseSessionState::Provisional,
                status_code: "pending".to_string(),
                expires_at: ControlPlaneTimestamp { tick: 3_600 },
            })
            .expect("origin writes ready handoff lease");
    }
    // The durable scheduler may finish a post-commit operation before it visits
    // the handoff lane, so drive ticks until the target acknowledgement lands.
    poll_until(
        &mut daemon_b,
        |_| {
            control_plane
                .lock()
                .expect("fake control plane lock")
                .list_leases(&WorkspaceId::new("workspace-dispatch-loop"))
                .expect("leases list")
                .into_iter()
                .any(|lease| {
                    lease.lease_id == "lease-dispatch-loop-1"
                        && lease.status_code == HANDOFF_MATERIALIZED_STATUS
                })
        },
        "target handoff materialization",
    );

    // The ready handoff materializes locally and is acknowledged remotely.
    let observed = control_plane
        .lock()
        .expect("fake control plane lock")
        .list_leases(&WorkspaceId::new("workspace-dispatch-loop"))
        .expect("leases list")
        .into_iter()
        .find(|lease| lease.lease_id == "lease-dispatch-loop-1")
        .expect("handoff lease exists");
    assert_eq!(observed.status_code, HANDOFF_MATERIALIZED_STATUS);
    assert_eq!(observed.session_state, LeaseSessionState::Open);
    // The unready handoff (base snapshot not synced) stays pending, unmaterialized.
    let unready = control_plane
        .lock()
        .expect("fake control plane lock")
        .list_leases(&WorkspaceId::new("workspace-dispatch-loop"))
        .expect("leases list")
        .into_iter()
        .find(|lease| lease.lease_id == "lease-dispatch-loop-unready")
        .expect("unready handoff lease exists");
    assert_eq!(unready.status_code, "pending");
    assert!(
        store
            .agent_lease_by_id(&bowline_core::ids::LeaseId::new(
                "lease-dispatch-loop-unready".to_string(),
            ))
            .expect("local unready lease reads")
            .is_none(),
        "unready handoff must not materialize locally"
    );

    let local_lease = store
        .agent_lease_by_id(&bowline_core::ids::LeaseId::new(
            "lease-dispatch-loop-1".to_string(),
        ))
        .expect("local lease reads")
        .expect("handoff materializes local lease");
    assert_eq!(
        local_lease.dispatch_state,
        bowline_core::commands::AgentLeaseDispatchState::Claimed
    );
    assert_eq!(local_lease.device_id.as_str(), "device-b");
    assert_eq!(local_lease.base_snapshot_id, base_snapshot_id);
    assert_eq!(local_lease.task, "fix target bug");
    assert_eq!(local_lease.work_view_id.as_str(), "work-dispatch-loop-1");
    time::OffsetDateTime::parse(
        &local_lease.expires_at,
        &time::format_description::well_known::Rfc3339,
    )
    .expect("dispatched local lease expiry is RFC3339");
    assert_eq!(
        local_lease
            .target_device_ref
            .as_ref()
            .map(|device| device.as_str()),
        Some("device-b")
    );
    assert_eq!(
        local_lease
            .origin_device_ref
            .as_ref()
            .map(|device| device.as_str()),
        Some("device-a")
    );

    // A second poll is idempotent: the already-acknowledged lease is not re-acked.
    let acknowledged_version = observed.version;
    daemon_b.poll();
    let stable = control_plane
        .lock()
        .expect("fake control plane lock")
        .list_leases(&WorkspaceId::new("workspace-dispatch-loop"))
        .expect("leases list")
        .into_iter()
        .find(|lease| lease.lease_id == "lease-dispatch-loop-1")
        .expect("handoff lease exists");
    assert_eq!(stable.status_code, HANDOFF_MATERIALIZED_STATUS);
    assert_eq!(stable.version, acknowledged_version);

    // Completion is a durable local transition. The next daemon handoff pass
    // forwards it to the shared lease so the origin device can observe it.
    let mut completed_local = local_lease;
    completed_local.session_state = bowline_core::commands::AgentSessionState::Completed;
    completed_local.status_summary = "completed".to_string();
    store
        .upsert_agent_lease(&completed_local)
        .expect("local completion persists");
    let completed_remote = {
        let control_plane = control_plane.lock().expect("fake control plane lock");
        claim_pending_dispatched_lease_with(
            &*control_plane,
            daemon_b.options.args.clone(),
            &WorkspaceId::new("workspace-dispatch-loop"),
            &DeviceId::new("device-b"),
            [48_u8; 32],
        )
        .expect("completion forwards")
        .expect("completed shared lease")
    };
    assert_eq!(completed_remote.session_state, LeaseSessionState::Completed);
    assert_eq!(completed_remote.status_code, "completed");
    let origin_observed = control_plane
        .lock()
        .expect("fake control plane lock")
        .list_leases(&WorkspaceId::new("workspace-dispatch-loop"))
        .expect("leases list")
        .into_iter()
        .find(|lease| lease.lease_id == "lease-dispatch-loop-1")
        .expect("handoff lease exists");
    assert_eq!(origin_observed.session_state, LeaseSessionState::Completed);

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn daemon_poll_acknowledges_already_materialized_handoff() {
    let temp = unique_temp_dir("bowline-daemon-dispatch-claimed-review-ready");
    let root = temp.join("device-b").join("Code");
    let state = temp.join("device-b").join("state");
    fs::create_dir_all(root.join("project/src")).expect("root dir");
    fs::create_dir_all(root.join("project/.git")).expect("git marker");
    fs::write(root.join("project/src/lib.rs"), "pub fn claimed() {}\n").expect("project file");
    let control_plane = Arc::new(Mutex::new(FakeControlPlaneClient::default()));
    let byte_store = Arc::new(Mutex::new(
        LocalByteStore::open_deterministic(temp.join("objects"), 48).expect("byte store"),
    ));

    let mut daemon_b = fake_daemon_runtime(
        root.clone(),
        state.clone(),
        "workspace-claimed-recovery",
        "device-b",
        Arc::clone(&control_plane),
        byte_store,
        [48_u8; 32],
    );
    poll_until(
        &mut daemon_b,
        |runtime| sync_status_version(runtime) >= 1,
        "target initial project sync",
    );
    let db_path = state.join(DEFAULT_DATABASE_FILE);
    let store = open_store_for_test(db_path.clone()).expect("metadata opens");
    let project = store
        .current_project_by_path(&root.join("project").display().to_string())
        .expect("project lookup")
        .expect("project synced locally");
    let base_snapshot_id = store
        .project_latest_snapshot_id(&WorkspaceId::new("workspace-claimed-recovery"), &project.id)
        .expect("project latest snapshot lookup")
        .expect("project latest snapshot");

    // The control-plane handoff lease exists but was never acknowledged (e.g. the
    // ack update was interrupted on a prior tick).
    let handoff = {
        let control_plane = control_plane.lock().expect("fake control plane lock");
        control_plane
            .create_lease(LeaseCreate {
                workspace_id: WorkspaceId::new("workspace-claimed-recovery"),
                lease_id: LeaseId::new("lease-claimed-recovery-1"),
                project_id: project.id.clone(),
                device_id: DeviceId::new("device-a"),
                target_device_ref: Some("device-b".to_string()),
                origin_device_ref: Some("device-a".to_string()),
                write_target_mode: LeaseWriteTargetMode::WorkView,
                work_view_id: Some(WorkViewId::new("work-claimed-recovery-1")),
                base_snapshot_id: base_snapshot_id.clone(),
                task_label: Some("finish claimed task".to_string()),
                session_state: LeaseSessionState::Provisional,
                status_code: "pending".to_string(),
                expires_at: ControlPlaneTimestamp { tick: 3_600 },
            })
            .expect("origin writes handoff lease")
    };
    // The workspace is already materialized locally on this host.
    bowline_local::agents::create_dispatched_agent_lease(
        bowline_local::agents::DispatchedAgentLeaseCreateOptions {
            lease: bowline_local::agents::AgentLeaseCreateOptions {
                db_path: Some(db_path),
                project_path: root.join("project").display().to_string(),
                task: "finish claimed task".to_string(),
                base: bowline_core::commands::AgentLeaseBase::LatestWorkspace,
                work_view: true,
                force_stale: false,
                device_id: DeviceId::new("device-b"),
                generated_at: "2026-06-25T12:00:00Z".to_string(),
            },
            identity: bowline_local::agents::DispatchedAgentLeaseIdentity {
                lease_id: bowline_core::ids::LeaseId::new(handoff.lease_id.clone()),
                base_snapshot_id: base_snapshot_id.clone(),
                work_view_id: Some(bowline_core::ids::WorkViewId::new(
                    "work-claimed-recovery-1".to_string(),
                )),
                target_device_ref: DeviceId::new("device-b"),
                origin_device_ref: DeviceId::new("device-a"),
                expires_at: "2026-06-25T13:00:00Z".to_string(),
            },
            workspace_content_key: [48_u8; 32],
        },
    )
    .expect("local dispatch lease materialized before ack");

    // The durable scheduler may finish a post-commit operation before it visits
    // the handoff lane, so drive ticks until the interrupted acknowledgement is
    // repaired.
    poll_until(
        &mut daemon_b,
        |_| {
            control_plane
                .lock()
                .expect("fake control plane lock")
                .list_leases(&WorkspaceId::new("workspace-claimed-recovery"))
                .expect("leases list")
                .into_iter()
                .any(|lease| {
                    lease.lease_id == "lease-claimed-recovery-1"
                        && lease.status_code == HANDOFF_MATERIALIZED_STATUS
                })
        },
        "interrupted handoff acknowledgement",
    );

    let acknowledged = control_plane
        .lock()
        .expect("fake control plane lock")
        .list_leases(&WorkspaceId::new("workspace-claimed-recovery"))
        .expect("leases list")
        .into_iter()
        .find(|lease| lease.lease_id == "lease-claimed-recovery-1")
        .expect("handoff lease exists");
    assert_eq!(acknowledged.status_code, HANDOFF_MATERIALIZED_STATUS);
    assert_eq!(acknowledged.session_state, LeaseSessionState::Open);

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn two_fake_daemon_loops_sync_edit_without_manual_sync_once() {
    let temp = unique_temp_dir("bowline-daemon-two-loop");
    let workspace_id = "ws_two_daemon_loop";
    let a_root = temp.join("device-a").join("Code");
    let b_root = temp.join("device-b").join("Code");
    let a_state = temp.join("device-a").join("state");
    let b_state = temp.join("device-b").join("state");
    let note_path = PathBuf::from("project/notes/loop.txt");
    fs::create_dir_all(a_root.join("project/notes")).expect("a project dirs");
    fs::create_dir_all(&b_root).expect("b root");
    fs::write(a_root.join(&note_path), "initial daemon loop\n").expect("initial file");

    let control_plane = Arc::new(Mutex::new(FakeControlPlaneClient::default()));
    let byte_store = Arc::new(Mutex::new(
        LocalByteStore::open_deterministic(temp.join("objects"), 41).expect("byte store"),
    ));
    let workspace_key = [41_u8; 32];
    let mut daemon_a = fake_daemon_runtime(
        a_root.clone(),
        a_state.clone(),
        workspace_id,
        "device-a",
        Arc::clone(&control_plane),
        Arc::clone(&byte_store),
        workspace_key,
    );
    let mut daemon_b = fake_daemon_runtime(
        b_root.clone(),
        b_state.clone(),
        workspace_id,
        "device-b",
        Arc::clone(&control_plane),
        Arc::clone(&byte_store),
        workspace_key,
    );

    poll_until(
        &mut daemon_a,
        |runtime| sync_status_version(runtime) >= 1,
        "device A initial upload",
    );
    poll_until(
        &mut daemon_b,
        |_| file_contains(&b_root.join(&note_path), "initial daemon loop"),
        "device B initial materialization",
    );

    fs::write(
        a_root.join(&note_path),
        "initial daemon loop\nlive edit from daemon A\n",
    )
    .expect("edit file");

    poll_until(
        &mut daemon_a,
        |runtime| sync_status_version(runtime) >= 2,
        "device A edit upload",
    );
    poll_until(
        &mut daemon_b,
        |_| file_contains(&b_root.join(&note_path), "live edit from daemon A"),
        "device B edit materialization",
    );

    assert!(
        daemon_a.status_json().contains("\"state\":\"idle\""),
        "{}",
        daemon_a.status_json()
    );
    assert!(
        daemon_b.status_json().contains("\"state\":\"idle\""),
        "{}",
        daemon_b.status_json()
    );
    let a_checkpoints = checkpoint_steps(&a_state);
    for expected in [
        "snapshot-candidate-built",
        "namespace-page-build",
        "source-pack-uploaded",
        "snapshot-root-committed",
        "workspace-ref-advanced",
    ] {
        assert!(
            a_checkpoints.iter().any(|step| step == expected),
            "missing device A checkpoint {expected}; got {a_checkpoints:?}"
        );
    }
    let b_checkpoints = checkpoint_steps(&b_state);
    for expected in ["remote-import-started", "remote-materialized"] {
        assert!(
            b_checkpoints.iter().any(|step| step == expected),
            "missing device B checkpoint {expected}; got {b_checkpoints:?}"
        );
    }

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn restarted_daemon_reconciles_real_directory_edit_without_data_loss() {
    let temp = unique_temp_dir("bowline-daemon-restart-real-root-edit");
    let workspace_id = "ws_two_daemon_loop_restart";
    let a_root = temp.join("device-a").join("Code");
    let b_root = temp.join("device-b").join("Code");
    let a_state = temp.join("device-a").join("state");
    let b_state = temp.join("device-b").join("state");
    let note_path = PathBuf::from("project/notes/restart.txt");
    fs::create_dir_all(a_root.join("project/notes")).expect("a project dirs");
    fs::create_dir_all(&b_root).expect("b root");
    fs::write(a_root.join(&note_path), "initial before restart\n").expect("initial file");

    let control_plane = Arc::new(Mutex::new(FakeControlPlaneClient::default()));
    let byte_store = Arc::new(Mutex::new(
        LocalByteStore::open_deterministic(temp.join("objects"), 43).expect("byte store"),
    ));
    let workspace_key = [43_u8; 32];
    let mut daemon_a = fake_daemon_runtime(
        a_root.clone(),
        a_state.clone(),
        workspace_id,
        "device-a",
        Arc::clone(&control_plane),
        Arc::clone(&byte_store),
        workspace_key,
    );
    let mut daemon_b = fake_daemon_runtime(
        b_root.clone(),
        b_state,
        workspace_id,
        "device-b",
        Arc::clone(&control_plane),
        Arc::clone(&byte_store),
        workspace_key,
    );

    poll_until(
        &mut daemon_a,
        |runtime| sync_status_version(runtime) >= 1,
        "device A initial upload",
    );
    poll_until(
        &mut daemon_b,
        |_| file_contains(&b_root.join(&note_path), "initial before restart"),
        "device B initial materialization",
    );

    drop(daemon_a);
    fs::write(
        a_root.join(&note_path),
        "initial before restart\nedit while daemon was down\n",
    )
    .expect("edit real file while daemon is down");
    let mut restarted_daemon_a = fake_daemon_runtime(
        a_root.clone(),
        a_state,
        workspace_id,
        "device-a",
        Arc::clone(&control_plane),
        Arc::clone(&byte_store),
        workspace_key,
    );

    poll_until(
        &mut restarted_daemon_a,
        |runtime| sync_status_version(runtime) >= 2,
        "restarted device A upload",
    );
    poll_until(
        &mut daemon_b,
        |_| file_contains(&b_root.join(&note_path), "edit while daemon was down"),
        "device B materializes edit from restarted daemon",
    );

    assert!(
        file_contains(&a_root.join(&note_path), "edit while daemon was down"),
        "restarted sync must never roll back the local real-directory edit"
    );
    assert!(
        restarted_daemon_a
            .status_json()
            .contains("\"state\":\"idle\""),
        "{}",
        restarted_daemon_a.status_json()
    );
    assert!(
        daemon_b.status_json().contains("\"state\":\"idle\""),
        "{}",
        daemon_b.status_json()
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn restarted_daemon_adopts_materialized_remote_head_without_reupload() {
    let temp = unique_temp_dir("bowline-daemon-restart-adopt-materialized");
    let workspace_id = "ws_two_daemon_loop_adopt";
    let a_root = temp.join("device-a").join("Code");
    let b_root = temp.join("device-b").join("Code");
    let a_state = temp.join("device-a").join("state");
    let b_state = temp.join("device-b").join("state");
    let note_path = PathBuf::from("project/notes/adopt.txt");
    fs::create_dir_all(a_root.join("project/notes")).expect("a project dirs");
    fs::create_dir_all(&b_root).expect("b root");
    fs::write(a_root.join(&note_path), "remote materialized bytes\n").expect("initial file");

    let control_plane = Arc::new(Mutex::new(FakeControlPlaneClient::default()));
    let byte_store = Arc::new(Mutex::new(
        LocalByteStore::open_deterministic(temp.join("objects"), 44).expect("byte store"),
    ));
    let workspace_key = [44_u8; 32];
    let mut daemon_a = fake_daemon_runtime(
        a_root.clone(),
        a_state,
        workspace_id,
        "device-a",
        Arc::clone(&control_plane),
        Arc::clone(&byte_store),
        workspace_key,
    );
    let mut daemon_b = fake_daemon_runtime(
        b_root.clone(),
        b_state.clone(),
        workspace_id,
        "device-b",
        Arc::clone(&control_plane),
        Arc::clone(&byte_store),
        workspace_key,
    );

    poll_until(
        &mut daemon_a,
        |runtime| sync_status_version(runtime) >= 1,
        "device A initial upload",
    );
    poll_until(
        &mut daemon_b,
        |_| file_contains(&b_root.join(&note_path), "remote materialized bytes"),
        "device B initial materialization",
    );
    let remote_before = control_plane
        .lock()
        .expect("fake control plane lock")
        .get_workspace_ref(&WorkspaceId::new(workspace_id))
        .expect("remote ref reads")
        .expect("remote ref exists");
    assert_eq!(remote_before.version, 1);

    drop(daemon_b);
    fs::remove_file(b_state.join(DEFAULT_DATABASE_FILE)).expect("remove local metadata db");
    let mut restarted_daemon_b = fake_daemon_runtime(
        b_root.clone(),
        b_state.clone(),
        workspace_id,
        "device-b",
        Arc::clone(&control_plane),
        Arc::clone(&byte_store),
        workspace_key,
    );

    poll_until(
        &mut restarted_daemon_b,
        |runtime| sync_status_version(runtime) >= 1,
        "restarted device B adopts materialized remote head",
    );

    let remote_after = control_plane
        .lock()
        .expect("fake control plane lock")
        .get_workspace_ref(&WorkspaceId::new(workspace_id))
        .expect("remote ref reads")
        .expect("remote ref exists");
    assert_eq!(
        remote_after.version, remote_before.version,
        "materialized remote bytes must not be uploaded as a new workspace version"
    );
    assert_eq!(remote_after.snapshot_id, remote_before.snapshot_id);
    let recovered_store =
        open_store_for_test(b_state.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
    let recovered_head = recovered_store
        .workspace_sync_head(&WorkspaceId::new(workspace_id))
        .expect("head reads")
        .expect("head restored");
    assert_eq!(
        recovered_head.workspace_ref.snapshot_id,
        remote_before.snapshot_id
    );
    assert_eq!(recovered_head.workspace_ref.version, remote_before.version);
    assert!(
        file_contains(&b_root.join(&note_path), "remote materialized bytes"),
        "restart must preserve the real-directory bytes"
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn two_fake_daemon_loops_sync_safe_save_without_temp_churn() {
    let temp = unique_temp_dir("bowline-daemon-two-loop-safe-save");
    let workspace_id = "ws_two_daemon_loop_safe_save";
    let a_root = temp.join("device-a").join("Code");
    let b_root = temp.join("device-b").join("Code");
    let a_state = temp.join("device-a").join("state");
    let b_state = temp.join("device-b").join("state");
    let note_path = PathBuf::from("project/notes/safe-save.txt");
    let temp_path = PathBuf::from("project/notes/.safe-save.txt.tmp");
    fs::create_dir_all(a_root.join("project/notes")).expect("a project dirs");
    fs::create_dir_all(&b_root).expect("b root");
    fs::write(a_root.join(&note_path), "initial safe save\n").expect("initial file");

    let control_plane = Arc::new(Mutex::new(FakeControlPlaneClient::default()));
    let byte_store = Arc::new(Mutex::new(
        LocalByteStore::open_deterministic(temp.join("objects"), 42).expect("byte store"),
    ));
    let workspace_key = [42_u8; 32];
    let mut daemon_a = fake_daemon_runtime(
        a_root.clone(),
        a_state,
        workspace_id,
        "device-a",
        Arc::clone(&control_plane),
        Arc::clone(&byte_store),
        workspace_key,
    );
    let mut daemon_b = fake_daemon_runtime(
        b_root.clone(),
        b_state,
        workspace_id,
        "device-b",
        Arc::clone(&control_plane),
        Arc::clone(&byte_store),
        workspace_key,
    );

    poll_until(
        &mut daemon_a,
        |runtime| sync_status_version(runtime) >= 1,
        "device A initial upload",
    );
    poll_until(
        &mut daemon_b,
        |_| file_contains(&b_root.join(&note_path), "initial safe save"),
        "device B initial materialization",
    );

    fs::write(a_root.join(&temp_path), "safe-save final bytes\n").expect("temp write");
    fs::rename(a_root.join(&temp_path), a_root.join(&note_path)).expect("safe-save rename");

    poll_until(
        &mut daemon_a,
        |runtime| sync_status_version(runtime) >= 2,
        "device A safe-save upload",
    );
    poll_until(
        &mut daemon_b,
        |_| file_contains(&b_root.join(&note_path), "safe-save final bytes"),
        "device B safe-save materialization",
    );

    assert!(
        !b_root.join(&temp_path).exists(),
        "safe-save temp path should not materialize remotely"
    );

    let _ = fs::remove_dir_all(temp);
}
