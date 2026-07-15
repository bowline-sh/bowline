use super::*;

#[test]
fn stale_base_guard_blocks_fresh_latest_base_until_forced() {
    let (temp, db_path) = seeded_store("agent-lease-stale-base-guard");
    let project_path = temp.root().join("Code/apps/web");
    let first = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "first".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        work_view: false,
        force_stale: false,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("first lease created");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .set_project_latest_snapshot_id(
            &WorkspaceId::new("ws_code"),
            &ProjectId::new("proj_web"),
            &SnapshotId::new("snap_project_new"),
        )
        .expect("new project snapshot");

    let error = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "second".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        work_view: false,
        force_stale: false,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect_err("fresh latest-base lease still requires explicit stale override");
    assert!(matches!(
        error,
        AgentError::StaleBaseHeld {
            summary,
            remedy_command
        } if summary.contains("older snapshot") && remedy_command == "bowline status --watch"
    ));

    let forced = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "forced".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        work_view: false,
        force_stale: true,
        device_id: DeviceId::new("device-test"),
        generated_at: "2026-06-25T12:00:01Z".to_string(),
    })
    .expect("forced lease proceeds");

    assert_eq!(first.lease.base_snapshot_id.as_str(), "snap_project_base");
    assert_eq!(forced.lease.base_snapshot_id.as_str(), "snap_project_new");
    assert!(forced.lease.status_summary.contains("stale-base override"));
    let status = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: Some(project_path.display().to_string()),
        workspace_scope: false,
        generated_at: now(),
    })
    .expect("status composes");
    assert_eq!(status.freshness, FreshnessVerdict::Behind);
}

#[test]
fn stale_base_override_audit_failure_rolls_back_forced_lease() {
    let (temp, db_path) = seeded_store("agent-lease-stale-base-override-rollback");
    let project_path = temp.root().join("Code/apps/web");
    let first = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "first".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        work_view: false,
        force_stale: false,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("first lease created");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .set_project_latest_snapshot_id(
            &WorkspaceId::new("ws_code"),
            &ProjectId::new("proj_web"),
            &SnapshotId::new("snap_project_new"),
        )
        .expect("new project snapshot");
    store
        .connection()
        .execute(
            "CREATE TRIGGER fail_stale_override_event
                 BEFORE INSERT ON events
                 WHEN NEW.name = 'lease.updated'
                 BEGIN
                   SELECT RAISE(FAIL, 'forced stale override audit failure');
                 END",
            [],
        )
        .expect("override event failure trigger");
    drop(store);

    let error = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "forced".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        work_view: false,
        force_stale: true,
        device_id: DeviceId::new("device-test"),
        generated_at: "2026-06-25T12:00:01Z".to_string(),
    })
    .expect_err("forced lease should fail when override audit event fails");
    assert!(matches!(error, AgentError::Event(_)));

    let store = MetadataStore::open(&db_path).expect("metadata");
    let leases = store
        .agent_leases(&WorkspaceId::new("ws_code"))
        .expect("leases");
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].id, first.lease.id);
    assert!(
        leases
            .iter()
            .all(|lease| !lease.status_summary.contains("stale-base override"))
    );
    let events = store.list_events(20).expect("events");
    assert!(
        events
            .iter()
            .all(|event| event.lease_id.as_ref() != Some(&LeaseId::new("lease_agent_forced")))
    );
}

#[test]
fn delete_agent_lease_preserves_audit_when_lease_delete_fails() {
    let (temp, db_path) = seeded_store("agent-lease-stale-base-direct-rollback-atomic");
    let project_path = temp.root().join("Code/apps/web");
    let created = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "first".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        work_view: false,
        force_stale: false,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("first lease created");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .connection()
        .execute(
            &format!(
                "CREATE TRIGGER fail_forced_lease_delete
                 BEFORE DELETE ON leases
                 WHEN OLD.id = '{}'
                 BEGIN
                   SELECT RAISE(FAIL, 'forced lease delete failure');
                 END",
                created.lease.id.as_str()
            ),
            [],
        )
        .expect("lease delete failure trigger");
    store
        .delete_agent_lease(&created.lease.id)
        .expect_err("lease delete should fail");

    assert!(
        store
            .agent_lease_by_id(&created.lease.id)
            .expect("lease lookup")
            .is_some()
    );
    let events = store.list_events(20).expect("events");
    assert!(events.iter().any(|event| {
        event.lease_id.as_ref() == Some(&created.lease.id) && event.name == EventName::LeaseCreated
    }));
}

#[test]
fn stale_base_guard_recovers_provisional_leases_before_checking_freshness() {
    let (temp, db_path) = seeded_store("agent-lease-stale-base-recovers-provisional");
    let project_path = temp.root().join("Code/apps/web");
    let mut provisional = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "interrupted".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        work_view: false,
        force_stale: false,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("provisional seed lease")
    .lease;
    let stale_lease_id = provisional.id.clone();
    provisional.session_state = AgentSessionState::Provisional;
    provisional.status_summary = AGENT_LEASE_STATUS_CREATING.to_string();

    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .upsert_agent_lease(&provisional)
        .expect("provisional lease update");
    store
        .set_project_latest_snapshot_id(
            &WorkspaceId::new("ws_code"),
            &ProjectId::new("proj_web"),
            &SnapshotId::new("snap_project_new"),
        )
        .expect("new project snapshot");
    drop(store);

    let created = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "retry".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        work_view: false,
        force_stale: false,
        device_id: DeviceId::new("device-test"),
        generated_at: "2026-06-25T12:00:01Z".to_string(),
    })
    .expect("retry should recover stale provisional before freshness check");

    assert_eq!(created.lease.base_snapshot_id.as_str(), "snap_project_new");
    let store = MetadataStore::open(&db_path).expect("metadata");
    assert!(
        store
            .agent_lease_by_id(&stale_lease_id)
            .expect("stale lease lookup")
            .is_none()
    );
}

#[test]
fn stale_base_reaches_agent_context_readiness_and_prompt() {
    let (temp, db_path) = seeded_store("agent-lease-stale-base-context");
    let project_path = temp.root().join("Code/apps/web");
    let created = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "inspect stale context".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        work_view: false,
        force_stale: false,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease created");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .set_project_latest_snapshot_id(
            &WorkspaceId::new("ws_code"),
            &ProjectId::new("proj_web"),
            &SnapshotId::new("snap_project_new"),
        )
        .expect("new project snapshot");
    drop(store);

    let context = agent_context(AgentLeaseSelectorOptions {
        db_path: Some(db_path.clone()),
        lease_id: created.lease.id.clone(),
        generated_at: now(),
    })
    .expect("context");
    assert_eq!(context.context.freshness, FreshnessVerdict::Behind);
    assert_eq!(context.context.status.level, StatusLevel::Attention);
    assert!(
        context
            .context
            .readiness
            .signals
            .iter()
            .any(|signal| signal.name == "freshness"
                && signal.state == AgentReadinessState::Attention)
    );
    assert!(
        context
            .context
            .stale_bases
            .iter()
            .any(
                |status| status.base_snapshot_id == Some(SnapshotId::new("snap_project_base"))
                    && status.latest_snapshot_id == Some(SnapshotId::new("snap_project_new"))
            )
    );

    let prompt = agent_prompt(AgentLeaseSelectorOptions {
        db_path: Some(db_path),
        lease_id: created.lease.id,
        generated_at: now(),
    })
    .expect("prompt");
    assert!(prompt.prompt.text.contains("Freshness: behind."));
    assert!(prompt.prompt.text.contains("bowline status --watch"));
}

#[test]
fn stale_base_override_audit_failure_rolls_back_forced_work_view() {
    let (temp, db_path) = seeded_store("agent-lease-stale-base-work-view-rollback");
    let project_path = temp.root().join("Code/apps/web");
    create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "first".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        work_view: false,
        force_stale: false,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("first lease created");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .set_project_latest_snapshot_id(
            &WorkspaceId::new("ws_code"),
            &ProjectId::new("proj_web"),
            &SnapshotId::new("snap_project_new"),
        )
        .expect("new project snapshot");
    store
        .connection()
        .execute(
            "CREATE TRIGGER fail_stale_work_view_override_event
                 BEFORE INSERT ON events
                 WHEN NEW.name = 'lease.updated'
                 BEGIN
                   SELECT RAISE(FAIL, 'forced stale work-view override audit failure');
                 END",
            [],
        )
        .expect("override event failure trigger");
    drop(store);

    let generated_at = "2026-06-25T12:00:01Z";
    let lease_name = lease_work_view_name("forced work view", generated_at);
    let work_view_path = temp
        .root()
        .join("Code/.work/apps/web")
        .join(lease_name.as_str());
    let error = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "forced work view".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        work_view: true,
        force_stale: true,
        device_id: DeviceId::new("device-test"),
        generated_at: generated_at.to_string(),
    })
    .expect_err("forced work-view lease should fail when override audit event fails");
    assert!(matches!(error, AgentError::Event(_)));

    assert!(
        !work_view_path.exists(),
        "failed forced work-view start must remove materialized work view"
    );
    let store = MetadataStore::open(&db_path).expect("metadata");
    let leases = store
        .agent_leases(&WorkspaceId::new("ws_code"))
        .expect("leases");
    assert_eq!(leases.len(), 1);
    assert!(
        store
            .work_views_by_name(
                &WorkspaceId::new("ws_code"),
                Some(&ProjectId::new("proj_web")),
                &lease_name
            )
            .expect("work views")
            .is_empty()
    );
    let events = store.list_events(20).expect("events");
    assert!(
        events
            .iter()
            .all(|event| event.subject.as_ref().is_none_or(|subject| {
                subject.id != agent_work_view_id("ws_code", "proj_web", &lease_name).as_str()
            }))
    );
}
