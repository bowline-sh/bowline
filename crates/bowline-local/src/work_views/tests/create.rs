use super::*;

#[test]
fn workon_materializes_project_files_without_secrets_or_source_control_metadata() {
    let (temp, db_path) = seeded_store("phase9-workon");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("source file");
    fs::write(project_path.join(".env.local"), "TOKEN=secret").expect("env file");
    fs::create_dir_all(project_path.join(".git")).expect("git dir");
    fs::write(project_path.join(".git/config"), "[core]\n").expect("git config");

    let output = create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "auth-fix".to_string(),
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");

    assert_eq!(output.work_view.name, "auth-fix");
    let materialized = temp.root().join("Code/.work/apps/web/auth-fix");
    assert!(materialized.is_dir());
    assert_eq!(
        fs::read_to_string(materialized.join("src/index.ts")).expect("copied source"),
        "console.log('base')"
    );
    assert!(!materialized.join(".env.local").exists());
    assert!(!materialized.join(".git/config").exists());
    assert_eq!(
        output.work_view.host_materializations,
        vec![display(&materialized)]
    );

    let diff = diff_work_view(WorkSelectorOptions {
        db_path: Some(db_path.clone()),
        selector: materialized.join("src").display().to_string(),
        generated_at: now(),
    })
    .expect("diff from inside work view");
    assert_eq!(diff.work_view.id, output.work_view.id);

    let sibling = temp.root().join("Code/.work/apps/web/auth-fix-old");
    fs::create_dir_all(&sibling).expect("sibling prefix path");
    let error = diff_work_view(WorkSelectorOptions {
        db_path: Some(db_path.clone()),
        selector: sibling.display().to_string(),
        generated_at: now(),
    })
    .expect_err("sibling prefix is not inside work view");
    assert!(matches!(error, WorkViewError::MissingWorkView { .. }));

    let escaped_sibling = materialized.join("../auth-fix-old");
    let error = diff_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: escaped_sibling.display().to_string(),
        generated_at: now(),
    })
    .expect_err("parent traversal selector is not inside work view");
    assert!(matches!(error, WorkViewError::MissingWorkView { .. }));
}

#[test]
fn workon_requires_latest_project_snapshot_before_materializing() {
    let (temp, db_path) = seeded_store_without_snapshot("phase9-workon-empty-base");
    let project_path = temp.root().join("Code").join("apps/web");

    let error = create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "first-work".to_string(),
        owner_device_id: None,
        generated_at: now(),
    })
    .expect_err("missing base should block work view");

    assert!(matches!(error, WorkViewError::MissingBaseSnapshot { .. }));
    assert!(!temp.root().join("Code/.work/apps/web/first-work").exists());

    let store = MetadataStore::open(&db_path).expect("metadata");
    assert!(
        store
            .work_views(&WorkspaceId::new("ws_code"), true, None)
            .expect("work views")
            .is_empty()
    );
}

#[test]
fn workon_refuses_project_with_pending_local_writes() {
    let (temp, db_path) = seeded_store("phase9-workon-dirty-project");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('dirty')").expect("dirty file");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .append_local_write_log(&LocalWriteLogRecord {
            id: "write-dirty-project".to_string(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-1"),
            project_id: Some(ProjectId::new("proj_web")),
            path: display(&project_path.join("src/index.ts")),
            source_path: None,
            operation: "modify".to_string(),
            staged_content_id: None,
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: "test".to_string(),
            settled_at: now(),
            created_at: now(),
        })
        .expect("write log");
    drop(store);

    let error = create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "dirty-base".to_string(),
        owner_device_id: None,
        generated_at: now(),
    })
    .expect_err("dirty project should not become work-view base");

    assert!(matches!(error, WorkViewError::DirtyProject { .. }));
    assert!(!temp.root().join("Code/.work/apps/web/dirty-base").exists());
    let store = MetadataStore::open(&db_path).expect("metadata");
    assert!(
        store
            .work_views(&WorkspaceId::new("ws_code"), true, None)
            .expect("work views")
            .is_empty()
    );
}

#[test]
fn workon_allows_historical_writes_before_synced_head() {
    let (temp, db_path) = seeded_store("phase9-workon-historical-write");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('synced')").expect("synced file");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .append_local_write_log(&LocalWriteLogRecord {
            id: "write-synced-project".to_string(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-1"),
            project_id: Some(ProjectId::new("proj_web")),
            path: display(&project_path.join("src/index.ts")),
            source_path: None,
            operation: "modify".to_string(),
            staged_content_id: None,
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: "test".to_string(),
            settled_at: "2026-06-25T01:00:00Z".to_string(),
            created_at: "2026-06-25T01:00:00Z".to_string(),
        })
        .expect("write log");
    store
        .upsert_workspace_sync_head(&WorkspaceSyncHeadRecord {
            workspace_ref: WorkspaceRef {
                workspace_id: "ws_code".to_string(),
                version: 1,
                snapshot_id: "snap_project_base".to_string(),
                updated_at: ControlPlaneTimestamp { tick: 1 },
                updated_by_device_id: Some("device-1".to_string()),
            },
            observed_at: "2026-06-25T01:01:00Z".to_string(),
        })
        .expect("synced head");
    drop(store);

    let output = create_work_view(WorkonOptions {
        db_path: Some(db_path),
        project_path: project_path.display().to_string(),
        name: "after-sync".to_string(),
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("historical write should not block work view");

    assert_eq!(output.work_view.name, "after-sync");
    assert!(temp.root().join("Code/.work/apps/web/after-sync").exists());
}

#[test]
fn workon_ignores_project_root_modify_noise_after_synced_head() {
    let (temp, db_path) = seeded_store("phase9-workon-project-root-noise");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .append_local_write_log(&LocalWriteLogRecord {
            id: "write-project-root-noise".to_string(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-1"),
            project_id: None,
            path: "apps/web".to_string(),
            source_path: None,
            operation: "modify".to_string(),
            staged_content_id: None,
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: "test".to_string(),
            settled_at: "2026-06-25T01:02:00Z".to_string(),
            created_at: "2026-06-25T01:02:00Z".to_string(),
        })
        .expect("write log");
    store
        .upsert_workspace_sync_head(&WorkspaceSyncHeadRecord {
            workspace_ref: WorkspaceRef {
                workspace_id: "ws_code".to_string(),
                version: 1,
                snapshot_id: "snap_project_base".to_string(),
                updated_at: ControlPlaneTimestamp { tick: 1 },
                updated_by_device_id: Some("device-1".to_string()),
            },
            observed_at: "2026-06-25T01:01:00Z".to_string(),
        })
        .expect("synced head");
    drop(store);

    let output = create_work_view(WorkonOptions {
        db_path: Some(db_path),
        project_path: project_path.display().to_string(),
        name: "directory-noise".to_string(),
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("project root directory noise should not block work view");

    assert_eq!(output.work_view.name, "directory-noise");
    assert!(
        temp.root()
            .join("Code/.work/apps/web/directory-noise")
            .exists()
    );
}

#[test]
fn workon_ignores_pending_writes_inside_other_work_views() {
    let (temp, db_path) = seeded_store("phase9-workon-work-namespace-write");
    let project_path = temp.root().join("Code").join("apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "console.log('base')").expect("base file");

    create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "first".to_string(),
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("first work view");
    let first_work_file = temp.root().join("Code/.work/apps/web/first/src/index.ts");
    fs::write(&first_work_file, "console.log('overlay')").expect("overlay edit");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .append_local_write_log(&LocalWriteLogRecord {
            id: "write-first-work-view".to_string(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("device-1"),
            project_id: Some(ProjectId::new("proj_web")),
            path: display(&first_work_file),
            source_path: None,
            operation: "modify".to_string(),
            staged_content_id: None,
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: "test".to_string(),
            settled_at: now(),
            created_at: now(),
        })
        .expect("work-view write log");
    drop(store);

    let second = create_work_view(WorkonOptions {
        db_path: Some(db_path),
        project_path: project_path.display().to_string(),
        name: "second".to_string(),
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work-view overlay writes should not dirty main project");

    assert_eq!(second.work_view.name, "second");
    assert!(temp.root().join("Code/.work/apps/web/second").exists());
}

#[test]
fn workon_rejects_duplicate_name_without_rewriting_existing_view() {
    let (temp, db_path) = seeded_store("phase9-workon-duplicate");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "same-name".to_string(),
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("first work view");

    let error = create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "same-name".to_string(),
        owner_device_id: None,
        generated_at: "2026-06-25T13:00:00Z".to_string(),
    })
    .expect_err("duplicate should fail");

    assert!(error.to_string().contains("already exists"));

    let error = create_work_view(WorkonOptions {
        db_path: Some(db_path),
        project_path: project_path.display().to_string(),
        name: "Same-Name".to_string(),
        owner_device_id: None,
        generated_at: "2026-06-25T13:00:01Z".to_string(),
    })
    .expect_err("case-only duplicate should fail");

    assert!(error.to_string().contains("already exists"));
}

#[test]
fn workon_rejects_preexisting_non_empty_materialization() {
    let (temp, db_path) = seeded_store("phase9-workon-stale-materialization");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    let stale = temp.root().join("Code/.work/apps/web/stale/src");
    fs::create_dir_all(&stale).expect("stale dir");
    fs::write(stale.join("old.ts"), "stale\n").expect("stale file");

    let error = create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "stale".to_string(),
        owner_device_id: None,
        generated_at: now(),
    })
    .expect_err("stale materialization should fail");

    assert!(
        error
            .to_string()
            .contains("materialization path is not empty")
    );
    let store = MetadataStore::open(&db_path).expect("metadata");
    let workspace = store
        .current_workspace()
        .expect("workspace query")
        .expect("workspace");
    assert!(
        store
            .work_views_by_name(&workspace.id, None, "stale")
            .expect("work views")
            .is_empty()
    );
}

#[test]
fn workon_rejects_symlinked_work_namespace() {
    let (temp, db_path) = seeded_store("phase9-workon-symlink-namespace");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    let work_root = temp.root().join("Code/.work");
    let outside = temp.root().join("outside-work");
    fs::create_dir_all(&outside).expect("outside");
    symlink(&outside, &work_root).expect("work symlink");

    let error = create_work_view(WorkonOptions {
        db_path: Some(db_path),
        project_path: project_path.display().to_string(),
        name: "escape".to_string(),
        owner_device_id: None,
        generated_at: now(),
    })
    .expect_err("symlinked namespace should fail");

    assert!(
        error
            .to_string()
            .contains("materialization escapes workspace")
    );
    assert!(!outside.join("apps/web/escape").exists());
}

#[test]
fn root_project_base_capture_skips_work_namespace() {
    let (temp, db_path) = seeded_store("phase9-root-project-work-skip");
    let code_root = temp.root().join("Code");
    let workspace_id = WorkspaceId::new("ws_code");
    let project_id = ProjectId::new("proj_root");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .insert_project(
            &project_id,
            &workspace_id,
            "root_code",
            "",
            "2026-06-25T00:01:00Z",
        )
        .expect("root project");
    store
        .set_project_latest_snapshot_id(
            &workspace_id,
            &project_id,
            &SnapshotId::new("snap_root_base"),
        )
        .expect("root snapshot");
    drop(store);

    fs::create_dir_all(code_root.join("src")).expect("src");
    fs::write(code_root.join("src/app.ts"), "console.log('root')").expect("source");
    fs::create_dir_all(code_root.join(".work/apps/web/other/src")).expect("work namespace");
    fs::write(
        code_root.join(".work/apps/web/other/src/generated.ts"),
        "console.log('work')",
    )
    .expect("work file");

    let output = create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: code_root.display().to_string(),
        name: "root-edit".to_string(),
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("root work view");

    let store = MetadataStore::open(&db_path).expect("metadata");
    assert!(
        store
            .work_view_base_hash(&workspace_id, &output.work_view.id, "src/app.ts")
            .expect("source hash")
            .is_some()
    );
    assert!(
        store
            .work_view_base_hash(
                &workspace_id,
                &output.work_view.id,
                ".work/apps/web/other/src/generated.ts",
            )
            .expect("work hash")
            .is_none()
    );
}

#[test]
fn workon_removes_materialization_after_post_create_metadata_failure() {
    let (temp, db_path) = seeded_store("phase9-workon-metadata-rollback");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    fs::write(project_path.join("index.ts"), "console.log('base')\n").expect("base file");
    let materialized = temp.root().join("Code/.work/apps/web/rollback");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .connection()
        .execute(
            "CREATE TRIGGER fail_work_view_base_file_insert
                 BEFORE INSERT ON work_view_base_files
                 BEGIN
                   SELECT RAISE(ABORT, 'forced base file insert failure');
                 END",
            [],
        )
        .expect("create failing trigger");
    drop(store);

    create_work_view(WorkonOptions {
        db_path: Some(db_path),
        project_path: project_path.display().to_string(),
        name: "rollback".to_string(),
        owner_device_id: None,
        generated_at: now(),
    })
    .expect_err("metadata failure should abort workon");

    assert!(!materialized.exists());
}
