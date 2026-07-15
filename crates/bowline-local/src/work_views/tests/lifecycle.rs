use super::*;
use std::collections::{BTreeMap, BTreeSet};

#[test]
fn partial_completion_advances_only_selected_authoritative_entries_idempotently() {
    let (temp, db_path) = seeded_store("partial-authoritative-base");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project");
    fs::write(project_path.join("src/a.ts"), "base a\n").expect("a");
    fs::write(project_path.join("src/b.ts"), "base b\n").expect("b");
    let created = create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "partial-authority".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    let store = MetadataStore::open(&db_path).expect("store");
    let descriptor = store
        .work_view_exposed_base(&created.work_view.workspace_id, &created.work_view.id)
        .expect("base")
        .expect("authoritative");
    let mut entries = exposed_entries(&store, &descriptor);
    let before_b = entries
        .iter()
        .find(|entry| entry.path == "apps/web/src/b.ts")
        .expect("before b")
        .clone();
    let accepted_id = workspace_content_id([7_u8; 32], b"accepted a\n");
    let accepted_entry = entries
        .iter_mut()
        .find(|entry| entry.path == "apps/web/src/a.ts")
        .expect("a entry");
    accepted_entry.content_id = Some(accepted_id.clone());
    accepted_entry.content_layout = Some(
        ContentLayout::single_segment(ContentLocator {
            content_id: accepted_id.clone(),
            storage: ContentStorage::Packed,
            raw_size: 11,
            pack_id: Some(bowline_core::ids::PackId::new("pk_partial")),
            offset: Some(0),
            length: Some(11),
        })
        .expect("partial content layout"),
    );
    accepted_entry.byte_len = Some(11);
    let identity =
        crate::sync::rebuild_manifest_identity(&created.work_view.workspace_id, &entries, "test");
    let target = crate::sync::SnapshotContent::new(
        SnapshotDraft {
            schema_version: bowline_core::workspace_graph::SNAPSHOT_SCHEMA_VERSION,
            snapshot_id: identity.snapshot_id,
            workspace_id: created.work_view.workspace_id.clone(),
            project_id: Some(created.work_view.project_id.clone()),
            kind: SnapshotKind::WorkspaceHead,
            base_snapshot_id: Some(created.work_view.base_snapshot_id.clone()),
            entries,
            refs: Vec::new(),
        },
        BTreeMap::from([(accepted_id.clone(), b"accepted a\n".to_vec())]),
        [7; 32],
    )
    .expect("page-backed target");
    let work_root = temp
        .root()
        .join("Code/.work/apps/web/partial-authority/src");
    fs::write(work_root.join("a.ts"), b"accepted a\n").expect("accepted work a");
    fs::write(work_root.join("b.ts"), b"pending b\n").expect("pending work b");
    let selected = BTreeSet::from(["src/a.ts".to_string()]);

    let captured_at = now();
    let cache_root = store.content_cache_root().expect("cache root");
    let first = advance_partial_exposed_base(PartialExposedBaseAdvance {
        store: &store,
        work_view: &created.work_view,
        selected_paths: &selected,
        target_snapshot: &target,
        cache_root: &cache_root,
        workspace_content_key: [7_u8; 32],
        captured_at: &captured_at,
    })
    .expect("first completion");
    let after_first = store
        .work_view_exposed_base(&first.workspace_id, &first.id)
        .expect("base")
        .expect("authoritative");
    let second = advance_partial_exposed_base(PartialExposedBaseAdvance {
        store: &store,
        work_view: &first,
        selected_paths: &selected,
        target_snapshot: &target,
        cache_root: &cache_root,
        workspace_content_key: [7_u8; 32],
        captured_at: &captured_at,
    })
    .expect("idempotent retry");
    let after_second = store
        .work_view_exposed_base(&second.workspace_id, &second.id)
        .expect("base")
        .expect("authoritative");

    assert_eq!(after_first, after_second);
    assert_eq!(second.lifecycle, WorkViewLifecycle::Active);
    let exposed = super::super::namespace::load_exposed_snapshot(&store, &after_second)
        .expect("page-backed exposed root");
    let a = super::super::namespace::get_entry(&exposed, "apps/web/src/a.ts")
        .expect("lookup")
        .expect("advanced a");
    assert_eq!(a.content_id, Some(accepted_id.clone()));
    let cache = LocalContentCache::open(&cache_root).expect("partial cache");
    assert_eq!(
        cache
            .get_previously_verified_content(&accepted_id)
            .expect("retained accepted content"),
        b"accepted a\n"
    );
    let remaining = diff_work_view(WorkSelectorOptions {
        db_path: Some(db_path.clone()),
        selector: "partial-authority".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("remaining partial diff");
    assert_eq!(
        remaining
            .changes
            .iter()
            .map(|change| change.path.as_str())
            .collect::<Vec<_>>(),
        vec!["src/b.ts"]
    );
    let after_b = super::super::namespace::get_entry(&exposed, "apps/web/src/b.ts")
        .expect("lookup")
        .expect("after b");
    assert_eq!(before_b, after_b);
}

#[test]
fn review_finalizer_is_idempotent_and_does_not_touch_main() {
    let (temp, db_path) = seeded_store("accept-review-finalizer");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    fs::write(project_path.join("value.txt"), "main\n").expect("main");
    let created = create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "review-finalizer".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("view");
    let store = MetadataStore::open(&db_path).expect("store");
    let review = WorkViewAcceptReview::MergeConflict { path_count: 2 };

    let first =
        finalize_review_ready(&store, &created.work_view, &review, &now()).expect("first finalize");
    let second = finalize_review_ready(&store, &first, &review, &now()).expect("retry finalize");

    assert_eq!(first, second);
    assert_eq!(second.lifecycle, WorkViewLifecycle::ReviewReady);
    assert_eq!(fs::read(project_path.join("value.txt")).unwrap(), b"main\n");
}

#[test]
fn lifecycle_transitions_hide_then_restore_retained_work_view() {
    let (temp, db_path) = seeded_store("phase9-lifecycle");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    let created = create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "billing".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    let open_command = format!("cd {}", created.work_view.visible_path);
    assert_eq!(
        created.next_actions[0].command.as_deref(),
        Some(open_command.as_str())
    );

    let discarded = discard_work_view(WorkSelectorOptions {
        db_path: Some(db_path.clone()),
        selector: "billing".to_string(),
        paths: Vec::new(),
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
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("restore");
    assert_eq!(
        serde_json::to_value(restored.work_view.lifecycle).unwrap(),
        "active"
    );
}

#[test]
fn discard_rejects_an_active_durable_accept() {
    let (temp, db_path) = seeded_store("phase108-discard-active-accept");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "publishing".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    let operation = enqueue_work_view_accept(
        WorkSelectorOptions {
            db_path: Some(db_path.clone()),
            selector: "publishing".to_string(),
            paths: Vec::new(),
            generated_at: now(),
        },
        DeviceId::new("device-1"),
    )
    .expect("accept operation");

    let error = discard_work_view(WorkSelectorOptions {
        db_path: Some(db_path.clone()),
        selector: "publishing".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect_err("discard must not race publication");
    assert!(matches!(
        error,
        WorkViewError::AcceptOperationPending { operation_id, state }
            if operation_id == operation.id
                && state == crate::metadata::WorkViewAcceptOperationState::Queued
    ));
    let store = MetadataStore::open(&db_path).expect("metadata");
    assert_eq!(
        store
            .work_views_by_name(
                &WorkspaceId::new("ws_code"),
                Some(&ProjectId::new("proj_web")),
                "publishing",
            )
            .expect("work view")
            .pop()
            .expect("record")
            .lifecycle,
        WorkViewLifecycle::Active
    );
}

#[test]
fn lifecycle_and_cleanup_actions_use_selected_workspace_root() {
    let (temp, db_path) = seeded_store("phase9-lifecycle-custom-root");
    let workspace_id = WorkspaceId::new("ws_code");
    let spaced_root = temp.root().join("Code With Spaces");
    let project_path = spaced_root.join("apps/web");
    fs::create_dir_all(&project_path).expect("project");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &spaced_root.display().to_string(),
            "2026-06-25T00:00:01Z",
        )
        .expect("root");
    drop(store);

    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "custom-root".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    let expected_status = format!("bowline status --root '{}' --all", spaced_root.display());

    let discarded = discard_work_view(WorkSelectorOptions {
        db_path: Some(db_path.clone()),
        selector: "custom-root".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("discard");
    assert_eq!(
        discarded.next_actions[0].command.as_deref(),
        Some(expected_status.as_str())
    );

    let restored = restore_work_view(WorkSelectorOptions {
        db_path: Some(db_path.clone()),
        selector: "custom-root".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("restore");
    assert_eq!(
        restored.next_actions[0].command.as_deref(),
        Some(expected_status.as_str())
    );

    let accepted = accept_work_view(WorkSelectorOptions {
        db_path: Some(db_path.clone()),
        selector: "custom-root".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("accept");
    assert_eq!(
        accepted.next_actions[0].command.as_deref(),
        Some(expected_status.as_str())
    );

    let cleanup = cleanup_work_views(WorkCleanupOptions {
        db_path: Some(db_path),
        apply: false,
        generated_at: now(),
    })
    .expect("cleanup");
    assert_eq!(
        cleanup.next_actions[0].command.as_deref(),
        Some(expected_status.as_str())
    );
}

#[test]
fn discard_work_view_leaves_matching_agent_lease_untouched() {
    let (temp, db_path) = seeded_store("phase9-discard-agent-lease");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    let lease = create_agent_lease(AgentLeaseCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        task: "discard me".to_string(),
        base: AgentLeaseBase::LatestWorkspace,
        work_view: true,
        force_stale: false,
        device_id: DeviceId::new("device-test"),
        generated_at: now(),
    })
    .expect("lease")
    .lease;

    discard_work_view(WorkSelectorOptions {
        db_path: Some(db_path.clone()),
        selector: lease.work_view_id.as_str().to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("discard");

    // The human work-view lifecycle no longer mutates agent-lease supervisor
    // state (that gate was removed). The lease record survives the discard.
    let stored = MetadataStore::open(&db_path)
        .expect("store")
        .agent_lease_by_id(&lease.id)
        .expect("lease query")
        .expect("lease stored");
    assert_eq!(stored.id, lease.id);
    assert_eq!(stored.session_state, lease.session_state);
}

#[test]
fn restore_recreates_missing_retained_materialization() {
    let (temp, db_path) = seeded_store("phase9-restore-after-cleanup");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "restore-me".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    discard_work_view(WorkSelectorOptions {
        db_path: Some(db_path.clone()),
        selector: "restore-me".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("discard");
    let materialized = temp.root().join("Code/.work/apps/web/restore-me");
    fs::remove_dir_all(&materialized).expect("remove materialization");
    assert!(!materialized.exists());

    let restored = restore_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "restore-me".to_string(),
        paths: Vec::new(),
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
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "restore-me".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    discard_work_view(WorkSelectorOptions {
        db_path: Some(db_path.clone()),
        selector: "restore-me".to_string(),
        paths: Vec::new(),
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
        paths: Vec::new(),
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
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "needs-review".to_string(),
        base_snapshot_selector: None,
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
        create_work_view(WorkCreateOptions {
            db_path: Some(db_path.clone()),
            project_path: project_path.display().to_string(),
            name: name.to_string(),
            base_snapshot_selector: None,
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
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "discarded-edit".to_string(),
        base_snapshot_selector: None,
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
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("discard");

    let error = accept_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "discarded-edit".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect_err("discarded work should not accept");

    assert!(error.to_string().contains("must be restored"));
    assert!(!project_path.join("src/leak.ts").exists());
}

#[test]
fn cleanup_preview_is_non_destructive_and_apply_marks_retained_views_delete_eligible() {
    let (temp, db_path) = seeded_store("phase9-cleanup");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "cleanup-me".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    let materialized = temp.root().join("Code/.work/apps/web/cleanup-me");
    assert!(materialized.is_dir());
    discard_work_view(WorkSelectorOptions {
        db_path: Some(db_path.clone()),
        selector: "cleanup-me".to_string(),
        paths: Vec::new(),
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
        db_path: Some(db_path.clone()),
        apply: true,
        generated_at: now(),
    })
    .expect("apply");
    assert_eq!(applied.deleted_paths, vec![display(&materialized)]);
    assert!(!materialized.exists());

    let store = MetadataStore::open(&db_path).expect("metadata");
    let workspace = store
        .current_workspace()
        .expect("workspace query")
        .expect("workspace");
    let cleaned = store
        .work_views_by_name(&workspace.id, None, "cleanup-me")
        .expect("work views")
        .pop()
        .expect("cleaned view");
    assert_eq!(cleaned.lifecycle, WorkViewLifecycle::Discarded);
    assert_eq!(
        cleaned.retention.state,
        WorkViewRetentionState::DeleteEligible
    );
    assert!(!cleaned.retention.restorable);
    assert_eq!(cleaned.visibility, WorkViewVisibility::Hidden);
    drop(store);

    let repeated = cleanup_work_views(WorkCleanupOptions {
        db_path: Some(db_path),
        apply: true,
        generated_at: now(),
    })
    .expect("repeat cleanup");
    assert!(repeated.previewed_paths.is_empty());
    assert!(repeated.deleted_paths.is_empty());
}
