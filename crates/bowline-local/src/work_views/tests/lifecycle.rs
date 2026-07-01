use super::*;

#[test]
fn lifecycle_transitions_hide_then_restore_retained_work_view() {
    let (temp, db_path) = seeded_store("phase9-lifecycle");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "billing".to_string(),
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");

    let discarded = discard_work_view(WorkSelectorOptions {
        db_path: Some(db_path.clone()),
        selector: "billing".to_string(),
        generated_at: now(),
    })
    .expect("discard");
    assert_eq!(
        serde_json::to_value(discarded.work_view.lifecycle).unwrap(),
        "discarded"
    );
    let visible = list_work_views(WorkListOptions {
        db_path: Some(db_path.clone()),
        include_hidden: false,
        current_device_id: None,
        generated_at: now(),
    })
    .expect("list");
    assert!(visible.work_views.is_empty());

    let restored = restore_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "billing".to_string(),
        generated_at: now(),
    })
    .expect("restore");
    assert_eq!(
        serde_json::to_value(restored.work_view.lifecycle).unwrap(),
        "active"
    );
}

#[test]
fn discard_work_view_marks_matching_agent_lease_discarded() {
    let (temp, db_path) = seeded_store("phase9-discard-agent-lease");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    let lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "discard me".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        hydrate_budget_bytes: 1024 * 1024,
        work_view: true,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease")
    .lease;

    discard_work_view(WorkSelectorOptions {
        db_path: Some(db_path.clone()),
        selector: lease.work_view_id.as_str().to_string(),
        generated_at: now(),
    })
    .expect("discard");

    let stored = MetadataStore::open(&db_path)
        .expect("store")
        .agent_lease_by_id(&lease.id)
        .expect("lease query")
        .expect("lease stored");
    assert_eq!(stored.output_state, AgentLeaseOutputState::Discarded);
    assert_eq!(stored.status_summary, "discarded");
}

#[test]
fn restore_recreates_missing_retained_materialization() {
    let (temp, db_path) = seeded_store("phase9-restore-after-cleanup");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "restore-me".to_string(),
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    discard_work_view(WorkSelectorOptions {
        db_path: Some(db_path.clone()),
        selector: "restore-me".to_string(),
        generated_at: now(),
    })
    .expect("discard");
    let materialized = temp.root().join("Code/.work/apps/web/restore-me");
    fs::remove_dir_all(&materialized).expect("remove materialization");
    assert!(!materialized.exists());

    let restored = restore_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "restore-me".to_string(),
        generated_at: "2026-06-25T13:00:00Z".to_string(),
    })
    .expect("restore");

    assert_eq!(
        serde_json::to_value(restored.work_view.lifecycle).unwrap(),
        "active"
    );
    assert!(materialized.is_dir());
}

#[test]
fn restore_rejects_cleaned_delete_eligible_work_view() {
    let (temp, db_path) = seeded_store("phase9-restore-after-cleanup");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "restore-me".to_string(),
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    discard_work_view(WorkSelectorOptions {
        db_path: Some(db_path.clone()),
        selector: "restore-me".to_string(),
        generated_at: now(),
    })
    .expect("discard");
    cleanup_work_views(WorkCleanupOptions {
        db_path: Some(db_path.clone()),
        apply: true,
        generated_at: now(),
    })
    .expect("cleanup");
    let materialized = temp.root().join("Code/.work/apps/web/restore-me");
    assert!(!materialized.exists());

    let error = restore_work_view(WorkSelectorOptions {
        db_path: Some(db_path.clone()),
        selector: "restore-me".to_string(),
        generated_at: "2026-06-25T13:00:00Z".to_string(),
    })
    .expect_err("cleaned work view should not restore");
    assert!(error.to_string().contains("is not restorable"));
    assert!(!materialized.exists());

    let store = MetadataStore::open(&db_path).expect("metadata");
    let workspace = store
        .current_workspace()
        .expect("workspace query")
        .expect("workspace");
    let cleaned = store
        .work_views_by_name(&workspace.id, None, "restore-me")
        .expect("work views")
        .pop()
        .expect("cleaned view");
    assert_eq!(
        serde_json::to_value(cleaned.retention.state).unwrap(),
        "delete-eligible"
    );
    assert!(!cleaned.retention.restorable);
}

#[test]
fn list_reports_review_ready_work_view_attention() {
    let (temp, db_path) = seeded_store("phase9-list-review-ready");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "needs-review".to_string(),
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    let store = MetadataStore::open(&db_path).expect("metadata");
    let workspace = store
        .current_workspace()
        .expect("workspace query")
        .expect("workspace");
    let mut view = store
        .work_views_by_name(&workspace.id, None, "needs-review")
        .expect("work views")
        .pop()
        .expect("work view");
    view.lifecycle = WorkViewLifecycle::ReviewReady;
    view.sync_state = WorkViewSyncState::Attention;
    store.upsert_work_view(&view).expect("review-ready view");
    drop(store);

    let listed = list_work_views(WorkListOptions {
        db_path: Some(db_path),
        include_hidden: false,
        current_device_id: None,
        generated_at: now(),
    })
    .expect("list");

    assert_eq!(listed.status.level, StatusLevel::Attention);
    assert!(listed.status.attention_items[0].contains("needs-review"));
}

#[test]
fn default_work_list_hides_unfollowed_remote_active_views() {
    let (temp, db_path) = seeded_store("phase9-list-visibility");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    for (name, owner) in [
        ("local-edit", "dev_mac"),
        ("remote-edit", "dev_linux"),
        ("remote-review", "dev_linux"),
    ] {
        create_work_view(WorkonOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: name.to_string(),
            owner_device_id: Some(DeviceId::new(owner)),
            generated_at: now(),
        })
        .expect("work view");
    }
    let store = MetadataStore::open(&db_path).expect("metadata");
    let workspace = store
        .current_workspace()
        .expect("workspace query")
        .expect("workspace");
    let mut review = store
        .work_views_by_name(&workspace.id, None, "remote-review")
        .expect("review query")
        .pop()
        .expect("review view");
    review.lifecycle = WorkViewLifecycle::ReviewReady;
    review.sync_state = WorkViewSyncState::Attention;
    store.upsert_work_view(&review).expect("review update");
    drop(store);

    let listed = list_work_views(WorkListOptions {
        db_path: Some(db_path),
        include_hidden: false,
        current_device_id: Some(DeviceId::new("dev_mac")),
        generated_at: now(),
    })
    .expect("list");
    let names = listed
        .work_views
        .iter()
        .map(|view| view.name.as_str())
        .collect::<Vec<_>>();

    assert!(names.contains(&"local-edit"));
    assert!(names.contains(&"remote-review"));
    assert!(!names.contains(&"remote-edit"));
}

#[test]
fn discarded_work_view_must_be_restored_before_accept() {
    let (temp, db_path) = seeded_store("phase9-discard-accept");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "discarded-edit".to_string(),
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    let materialized = temp.root().join("Code/.work/apps/web/discarded-edit/src");
    fs::create_dir_all(&materialized).expect("work src");
    fs::write(materialized.join("leak.ts"), "stale\n").expect("stale overlay");
    discard_work_view(WorkSelectorOptions {
        db_path: Some(db_path.clone()),
        selector: "discarded-edit".to_string(),
        generated_at: now(),
    })
    .expect("discard");

    let error = accept_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "discarded-edit".to_string(),
        generated_at: now(),
    })
    .expect_err("discarded work should not accept");

    assert!(error.to_string().contains("must be restored"));
    assert!(!project_path.join("src/leak.ts").exists());
}

#[test]
fn cleanup_preview_is_non_destructive_and_apply_removes_archived_dirs() {
    let (temp, db_path) = seeded_store("phase9-cleanup");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "cleanup-me".to_string(),
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    let materialized = temp.root().join("Code/.work/apps/web/cleanup-me");
    assert!(materialized.is_dir());
    discard_work_view(WorkSelectorOptions {
        db_path: Some(db_path.clone()),
        selector: "cleanup-me".to_string(),
        generated_at: now(),
    })
    .expect("discard");

    let preview = cleanup_work_views(WorkCleanupOptions {
        db_path: Some(db_path.clone()),
        apply: false,
        generated_at: now(),
    })
    .expect("preview");
    assert!(preview.deleted_paths.is_empty());
    assert!(materialized.is_dir());

    let applied = cleanup_work_views(WorkCleanupOptions {
        db_path: Some(db_path),
        apply: true,
        generated_at: now(),
    })
    .expect("apply");
    assert_eq!(applied.deleted_paths, vec![display(&materialized)]);
    assert!(!materialized.exists());
}
