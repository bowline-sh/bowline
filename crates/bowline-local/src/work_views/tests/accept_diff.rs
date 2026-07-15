use super::*;
use crate::work_views::diff_work_view_with_checkpoint;

#[test]
fn untouched_content_addressed_large_file_has_no_diff_or_overlay_upload() {
    let (temp, db_path) = seeded_store("phase108-content-addressed-untouched");
    let byte_len = super::super::create_list::MAX_INLINE_EXPOSED_BASE_BYTES as usize + 23;
    let bytes = vec![b'a'; byte_len];
    seed_large_canonical_file(&temp, &db_path, &bytes, [51_u8; 32]);
    let project_path = temp.root().join("Code/apps/web");
    let created = create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "large-untouched".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("large work view");

    let diff = diff_work_view(WorkSelectorOptions {
        db_path: Some(db_path.clone()),
        selector: "large-untouched".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("untouched diff");
    assert!(diff.changes.is_empty());
    let store = MetadataStore::open(&db_path).expect("metadata");
    let upload = overlay_deltas_for_upload(&store, &created.work_view).expect("overlay plan");
    assert!(upload.deltas.is_empty());
}

#[test]
fn same_length_content_addressed_edit_is_a_real_modification() {
    let (temp, db_path) = seeded_store("phase108-content-addressed-modified");
    let byte_len = super::super::create_list::MAX_INLINE_EXPOSED_BASE_BYTES as usize + 23;
    let bytes = vec![b'a'; byte_len];
    seed_large_canonical_file(&temp, &db_path, &bytes, [52_u8; 32]);
    let project_path = temp.root().join("Code/apps/web");
    let created = create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "large-modified".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("large work view");
    fs::write(
        temp.root()
            .join("Code/.work/apps/web/large-modified/large.bin"),
        vec![b'b'; byte_len],
    )
    .expect("same-length work edit");

    let diff = diff_work_view(WorkSelectorOptions {
        db_path: Some(db_path.clone()),
        selector: "large-modified".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("modified diff");
    assert_eq!(diff.changes.len(), 1);
    assert_eq!(diff.changes[0].path, "large.bin");
    assert_eq!(diff.changes[0].kind, WorkDiffChangeKind::Modified);
    let store = MetadataStore::open(&db_path).expect("metadata");
    let upload = overlay_deltas_for_upload(&store, &created.work_view).expect("overlay plan");
    assert_eq!(upload.deltas.len(), 1);
}

#[test]
fn large_agent_diff_stops_at_the_next_chunk_checkpoint() {
    let (temp, db_path) = seeded_store("phase110-cancelled-agent-diff");
    let byte_len = super::super::create_list::MAX_INLINE_EXPOSED_BASE_BYTES as usize + 256 * 1024;
    let bytes = vec![b'a'; byte_len];
    seed_large_canonical_file(&temp, &db_path, &bytes, [54_u8; 32]);
    let project_path = temp.root().join("Code/apps/web");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "large-cancelled-diff".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("large work view");
    let mut checkpoints = 0_usize;

    let error = diff_work_view_with_checkpoint(
        WorkSelectorOptions {
            db_path: Some(db_path),
            selector: "large-cancelled-diff".to_string(),
            paths: Vec::new(),
            generated_at: now(),
        },
        || {
            checkpoints += 1;
            checkpoints < 4
        },
    )
    .expect_err("diff cancellation interrupts the next bounded read");

    assert!(matches!(
        error,
        WorkViewError::Io(error) if error.kind() == std::io::ErrorKind::Interrupted
    ));
    assert_eq!(checkpoints, 4);
}

#[test]
fn corrupt_content_addressed_base_blocks_partial_accept_enqueue() {
    let (temp, db_path) = seeded_store("phase108-content-addressed-corrupt");
    let byte_len = super::super::create_list::MAX_INLINE_EXPOSED_BASE_BYTES as usize + 23;
    let bytes = vec![b'a'; byte_len];
    let content_id = seed_large_canonical_file(&temp, &db_path, &bytes, [53_u8; 32]);
    let project_path = temp.root().join("Code/apps/web");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "large-corrupt".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("large work view");
    fs::write(
        temp.root()
            .join(".state/cache/content")
            .join(content_id.as_str()),
        vec![b'x'; byte_len],
    )
    .expect("corrupt cache");

    let error = enqueue_work_view_accept(
        WorkSelectorOptions {
            db_path: Some(db_path.clone()),
            selector: "large-corrupt".to_string(),
            paths: vec!["large.bin".to_string()],
            generated_at: now(),
        },
        DeviceId::new("dev_mac"),
    )
    .expect_err("corrupt base must block selector resolution");
    assert!(matches!(
        error,
        WorkViewError::ExposedBaseContentUnavailable { .. }
    ));
    let store = MetadataStore::open(&db_path).expect("metadata");
    let count: u64 = store
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM work_view_accept_operations",
            [],
            |row| row.get(0),
        )
        .expect("operation count");
    assert_eq!(count, 0);
    assert_eq!(
        fs::read(project_path.join("large.bin")).expect("main"),
        bytes
    );
}

#[test]
fn accept_case_variant_env_edit_round_trips_owner_only_materialization() {
    let (temp, db_path) = seeded_store("phase9-accept-secret-review");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    fs::write(project_path.join(".ENV.Local"), "TOKEN=base\n").expect("base env");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "secret-edit".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    let materialized = temp.root().join("Code/.work/apps/web/secret-edit");
    assert_eq!(
        fs::read_to_string(materialized.join(".ENV.Local")).expect("materialized base env"),
        "TOKEN=base\n"
    );
    fs::write(materialized.join(".ENV.Local"), "TOKEN=secret\n").expect("work env");

    let accepted = accept_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "secret-edit".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("accept");

    assert_eq!(serde_json::to_value(accepted.action).unwrap(), "accepted");
    assert_eq!(
        fs::read_to_string(project_path.join(".ENV.Local")).expect("accepted env"),
        "TOKEN=secret\n"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        assert_eq!(
            fs::metadata(project_path.join(".ENV.Local"))
                .expect("env metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn accept_never_applies_env_files_from_local_regenerate_namespaces() {
    let (temp, db_path) = seeded_store("phase9-accept-generated-env");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "generated-env".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    let materialized = temp.root().join("Code/.work/apps/web/generated-env");
    for path in [
        "node_modules/pkg/.env",
        "target/debug/.ENV.Local",
        "dist/service.env",
        ".cache/tool/.Env",
    ] {
        let path = materialized.join(path);
        fs::create_dir_all(path.parent().expect("nested parent")).expect("nested directory");
        fs::write(path, "TOKEN=must-not-escape\n").expect("nested env");
    }

    accept_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "generated-env".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("accept");

    for path in ["node_modules", "target", "dist", ".cache"] {
        assert!(!project_path.join(path).exists(), "{path} must stay local");
    }
}

#[test]
fn clean_accept_applies_new_work_view_files_to_main_view() {
    let (temp, db_path) = seeded_store("phase9-accept-clean");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "new-file".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    let materialized = temp.root().join("Code/.work/apps/web/new-file");
    fs::create_dir_all(materialized.join("src")).expect("src");
    fs::write(materialized.join("src/new.ts"), "export const ok = true;\n").expect("work file");

    let accepted = accept_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "new-file".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("accept");

    assert_eq!(serde_json::to_value(accepted.action).unwrap(), "accepted");
    assert_eq!(
        fs::read_to_string(project_path.join("src/new.ts")).expect("main file"),
        "export const ok = true;\n"
    );
    assert!(accept_journal_dirs(&temp.root().join("Code/.work")).is_empty());
}

#[test]
fn accept_dependency_file_ignores_local_regenerate_churn() {
    let (temp, db_path) = seeded_store("phase9-accept-policy");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "deps".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    let work_file = temp
        .root()
        .join("Code/.work/apps/web/deps/node_modules/lodash/index.js");
    fs::create_dir_all(work_file.parent().expect("parent")).expect("dependency dir");
    fs::write(&work_file, "module.exports = {}\n").expect("dependency file");

    let accepted = accept_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "deps".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("accept");

    assert_eq!(serde_json::to_value(accepted.action).unwrap(), "accepted");
    assert!(accepted.work_view.attention.is_empty());
    assert!(
        !temp
            .root()
            .join("Code/apps/web/node_modules/lodash/index.js")
            .exists()
    );
}

#[test]
fn clean_accept_applies_existing_file_when_main_has_not_changed() {
    let (temp, db_path) = seeded_store("phase9-accept-existing-clean");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project");
    fs::write(project_path.join("src/index.ts"), "base\n").expect("main file");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "edit-existing".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    let materialized = temp.root().join("Code/.work/apps/web/edit-existing/src");
    fs::create_dir_all(&materialized).expect("work src");
    fs::write(materialized.join("index.ts"), "work edit\n").expect("work file");

    let accepted = accept_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "edit-existing".to_string(),
        paths: Vec::new(),
        generated_at: "2026-06-25T12:05:00Z".to_string(),
    })
    .expect("accept");

    assert_eq!(serde_json::to_value(accepted.action).unwrap(), "accepted");
    assert_eq!(
        fs::read_to_string(project_path.join("src/index.ts")).expect("main file"),
        "work edit\n"
    );
}

#[test]
fn accept_detects_unlogged_main_view_edits_from_base_hash() {
    let (temp, db_path) = seeded_store("phase9-accept-unlogged-main-edit");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project");
    fs::write(project_path.join("src/index.ts"), "base\n").expect("main file");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "unlogged-main".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    fs::write(project_path.join("src/index.ts"), "main changed\n").expect("missed main edit");
    let materialized = temp.root().join("Code/.work/apps/web/unlogged-main/src");
    fs::create_dir_all(&materialized).expect("work src");
    fs::write(materialized.join("index.ts"), "work edit\n").expect("work file");

    let accepted = accept_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "unlogged-main".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("accept");

    assert_eq!(
        serde_json::to_value(accepted.work_view.lifecycle).unwrap(),
        "review-ready"
    );
    assert_eq!(
        fs::read_to_string(project_path.join("src/index.ts")).expect("main file"),
        "main changed\n"
    );
}

#[test]
fn snapshot_accept_merges_disjoint_hunks_in_one_utf8_file() {
    let (temp, db_path) = seeded_store("snapshot-accept-disjoint-hunks");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project");
    fs::write(
        project_path.join("src/index.ts"),
        "export const first = 1;\nconst middle = true;\nexport const last = 1;\n",
    )
    .expect("base");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "disjoint".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    fs::write(
        project_path.join("src/index.ts"),
        "export const first = 2;\nconst middle = true;\nexport const last = 1;\n",
    )
    .expect("main edit");
    fs::write(
        temp.root()
            .join("Code/.work/apps/web/disjoint/src/index.ts"),
        "export const first = 1;\nconst middle = true;\nexport const last = 2;\n",
    )
    .expect("work edit");

    let output = accept_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "disjoint".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("accept");

    assert_eq!(serde_json::to_value(output.action).unwrap(), "accepted");
    assert_eq!(
        fs::read_to_string(project_path.join("src/index.ts")).unwrap(),
        "export const first = 2;\nconst middle = true;\nexport const last = 2;\n"
    );
}

#[test]
fn snapshot_accept_both_delete_exposed_path_cleanly() {
    let (temp, db_path) = seeded_store("snapshot-accept-both-delete");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project");
    fs::write(project_path.join("src/old.ts"), "old\n").expect("base");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "both-delete".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    fs::remove_file(project_path.join("src/old.ts")).expect("main delete");
    fs::remove_file(
        temp.root()
            .join("Code/.work/apps/web/both-delete/src/old.ts"),
    )
    .expect("work delete");

    let output = accept_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "both-delete".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("accept");

    assert_eq!(serde_json::to_value(output.action).unwrap(), "accepted");
    assert!(!project_path.join("src/old.ts").exists());
}

#[test]
fn accept_detects_main_view_deletion_from_base_hash() {
    let (temp, db_path) = seeded_store("phase9-accept-main-delete");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project");
    fs::write(project_path.join("src/index.ts"), "base\n").expect("main file");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "main-delete".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    fs::remove_file(project_path.join("src/index.ts")).expect("main delete");
    let materialized = temp.root().join("Code/.work/apps/web/main-delete/src");
    fs::create_dir_all(&materialized).expect("work src");
    fs::write(materialized.join("index.ts"), "work edit\n").expect("work file");

    let accepted = accept_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "main-delete".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("accept");

    assert_eq!(
        serde_json::to_value(accepted.work_view.lifecycle).unwrap(),
        "review-ready"
    );
    assert!(!project_path.join("src/index.ts").exists());
}

#[test]
fn clean_accept_applies_work_view_deletions() {
    let (temp, db_path) = seeded_store("phase9-accept-delete");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project");
    fs::write(project_path.join("src/old.ts"), "old\n").expect("main file");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "delete-old".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .append_local_write_log(&LocalWriteLogRecord {
            id: "write-delete-old".to_string(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("dev_mac"),
            project_id: Some(ProjectId::new("proj_web")),
            path: format!(
                "{}/src/old.ts",
                temp.root().join("Code/.work/apps/web/delete-old").display()
            ),
            source_path: None,
            operation: "delete".to_string(),
            staged_content_id: None,
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: "human".to_string(),
            settled_at: "2026-06-25T12:01:00Z".to_string(),
            created_at: "2026-06-25T12:01:00Z".to_string(),
        })
        .expect("delete write");
    drop(store);
    fs::remove_file(
        temp.root()
            .join("Code/.work/apps/web/delete-old/src/old.ts"),
    )
    .expect("delete work-view file");

    let accepted = accept_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "delete-old".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("accept");

    assert_eq!(serde_json::to_value(accepted.action).unwrap(), "accepted");
    assert!(!project_path.join("src/old.ts").exists());
}

#[test]
fn clean_accept_preserves_recreated_file_after_delete_log() {
    let (temp, db_path) = seeded_store("work-view-delete-recreate");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project");
    fs::write(project_path.join("src/recreated.ts"), "old\n").expect("main file");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "delete-recreate".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    let work_file = temp
        .root()
        .join("Code/.work/apps/web/delete-recreate/src/recreated.ts");
    fs::write(&work_file, "new\n").expect("recreated work file");
    let store = MetadataStore::open(&db_path).expect("metadata");
    for (id, operation, created_at) in [
        ("write-delete-recreated", "delete", "2026-06-25T12:01:00Z"),
        ("write-update-recreated", "update", "2026-06-25T12:02:00Z"),
    ] {
        store
            .append_local_write_log(&LocalWriteLogRecord {
                id: id.to_string(),
                workspace_id: WorkspaceId::new("ws_code"),
                device_id: DeviceId::new("dev_mac"),
                project_id: Some(ProjectId::new("proj_web")),
                path: work_file.display().to_string(),
                source_path: None,
                operation: operation.to_string(),
                staged_content_id: None,
                policy_classification: PathClassification::WorkspaceSync,
                causation_id: "human".to_string(),
                settled_at: created_at.to_string(),
                created_at: created_at.to_string(),
            })
            .expect("write log");
    }
    drop(store);

    let accepted = accept_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "delete-recreate".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("accept");

    assert_eq!(serde_json::to_value(accepted.action).unwrap(), "accepted");
    assert_eq!(
        fs::read_to_string(project_path.join("src/recreated.ts")).expect("accepted file"),
        "new\n"
    );
}

#[test]
fn clean_accept_renames_by_deleting_source_path() {
    let (temp, db_path) = seeded_store("work-view-rename");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project");
    fs::write(project_path.join("src/old.ts"), "old\n").expect("main file");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "rename-file".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    let work_root = temp.root().join("Code/.work/apps/web/rename-file");
    let old_file = work_root.join("src/old.ts");
    let new_file = work_root.join("src/new.ts");
    fs::rename(&old_file, &new_file).expect("rename in work view");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .append_local_write_log(&LocalWriteLogRecord {
            id: "write-rename-file".to_string(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("dev_mac"),
            project_id: Some(ProjectId::new("proj_web")),
            path: new_file.display().to_string(),
            source_path: Some(old_file.display().to_string()),
            operation: "rename".to_string(),
            staged_content_id: None,
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: "human".to_string(),
            settled_at: "2026-06-25T12:02:00Z".to_string(),
            created_at: "2026-06-25T12:02:00Z".to_string(),
        })
        .expect("rename log");
    drop(store);

    let accepted = accept_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "rename-file".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("accept");

    assert_eq!(serde_json::to_value(accepted.action).unwrap(), "accepted");
    assert!(!project_path.join("src/old.ts").exists());
    assert_eq!(
        fs::read_to_string(project_path.join("src/new.ts")).expect("renamed file"),
        "old\n"
    );
}

#[test]
fn partial_accept_applies_only_selected_paths_and_leaves_view_active() {
    let (temp, db_path) = seeded_store("work-view-partial-accept");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project");
    fs::write(project_path.join("src/a.ts"), "base a\n").expect("main a");
    fs::write(project_path.join("src/b.ts"), "base b\n").expect("main b");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "partial".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    let work_root = temp.root().join("Code/.work/apps/web/partial");
    fs::write(work_root.join("src/a.ts"), "accepted a\n").expect("work a");
    fs::write(work_root.join("src/b.ts"), "pending b\n").expect("work b");

    let preview = diff_work_view(WorkSelectorOptions {
        db_path: Some(db_path.clone()),
        selector: "partial".to_string(),
        paths: vec!["src/a.ts".to_string()],
        generated_at: now(),
    })
    .expect("partial diff");
    assert_eq!(
        preview
            .changes
            .iter()
            .map(|change| change.path.as_str())
            .collect::<Vec<_>>(),
        vec!["src/a.ts"]
    );
    assert_eq!(
        preview
            .next_actions
            .first()
            .and_then(|action| action.command.as_deref()),
        Some("bowline work accept partial --path src/a.ts")
    );

    let accepted = accept_work_view(WorkSelectorOptions {
        db_path: Some(db_path.clone()),
        selector: "partial".to_string(),
        paths: vec!["src/a.ts".to_string()],
        generated_at: now(),
    })
    .expect("partial accept");

    assert!(accepted.partial);
    assert_eq!(accepted.paths, vec!["src/a.ts"]);
    assert_eq!(accepted.work_view.lifecycle, WorkViewLifecycle::Active);
    assert_eq!(
        fs::read_to_string(project_path.join("src/a.ts")).expect("main a"),
        "accepted a\n"
    );
    assert_eq!(
        fs::read_to_string(project_path.join("src/b.ts")).expect("main b"),
        "base b\n"
    );
    let remaining = diff_work_view(WorkSelectorOptions {
        db_path: Some(db_path.clone()),
        selector: "partial".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("remaining diff");
    assert_eq!(
        remaining
            .changes
            .iter()
            .map(|change| change.path.as_str())
            .collect::<Vec<_>>(),
        vec!["src/b.ts"]
    );

    let full = accept_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "partial".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("full accept after partial");
    assert!(!full.partial);
    assert_eq!(full.work_view.lifecycle, WorkViewLifecycle::Accepted);
    assert_eq!(
        fs::read_to_string(project_path.join("src/a.ts")).expect("main a"),
        "accepted a\n"
    );
    assert_eq!(
        fs::read_to_string(project_path.join("src/b.ts")).expect("main b"),
        "pending b\n"
    );
}

#[test]
fn partial_accept_path_glob_selects_rename_pair_atomically() {
    let (temp, db_path) = seeded_store("work-view-partial-rename");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project");
    fs::write(project_path.join("src/old.ts"), "old\n").expect("main old");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "rename-partial".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    let work_root = temp.root().join("Code/.work/apps/web/rename-partial");
    let old_file = work_root.join("src/old.ts");
    let new_file = work_root.join("src/new.ts");
    fs::rename(&old_file, &new_file).expect("rename in work view");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .append_local_write_log(&LocalWriteLogRecord {
            id: "write-rename-partial".to_string(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("dev_mac"),
            project_id: Some(ProjectId::new("proj_web")),
            path: new_file.display().to_string(),
            source_path: Some(old_file.display().to_string()),
            operation: "rename".to_string(),
            staged_content_id: None,
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: "human".to_string(),
            settled_at: "2026-06-25T12:02:00Z".to_string(),
            created_at: "2026-06-25T12:02:00Z".to_string(),
        })
        .expect("rename log");
    drop(store);

    let operation = enqueue_work_view_accept(
        WorkSelectorOptions {
            db_path: Some(db_path.clone()),
            selector: "rename-partial".to_string(),
            paths: vec!["src/new.ts".to_string()],
            generated_at: now(),
        },
        DeviceId::new("dev_mac"),
    )
    .expect("enqueue partial rename");
    assert_eq!(
        operation.selected_paths,
        Some(vec!["src/new.ts".to_string(), "src/old.ts".to_string()])
    );

    let accepted = accept_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "rename-partial".to_string(),
        paths: vec!["src/new.ts".to_string()],
        generated_at: now(),
    })
    .expect("partial rename accept");

    assert_eq!(accepted.paths, vec!["src/new.ts", "src/old.ts"]);
    assert!(!project_path.join("src/old.ts").exists());
    assert_eq!(
        fs::read_to_string(project_path.join("src/new.ts")).expect("new main file"),
        "old\n"
    );
}

#[test]
fn partial_accept_reports_empty_path_selection() {
    let (temp, db_path) = seeded_store("work-view-partial-empty");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "empty-select".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    fs::write(
        temp.root()
            .join("Code/.work/apps/web/empty-select/src/changed.ts"),
        "changed\n",
    )
    .expect("work change");

    let enqueue_error = enqueue_work_view_accept(
        WorkSelectorOptions {
            db_path: Some(db_path.clone()),
            selector: "empty-select".to_string(),
            paths: vec!["docs/**".to_string()],
            generated_at: now(),
        },
        DeviceId::new("dev_mac"),
    )
    .expect_err("durable selection should be empty");
    assert!(matches!(
        enqueue_error,
        WorkViewError::EmptyPathSelection { .. }
    ));

    let error = accept_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "empty-select".to_string(),
        paths: vec!["docs/**".to_string()],
        generated_at: now(),
    })
    .expect_err("selection should be empty");

    assert!(matches!(error, WorkViewError::EmptyPathSelection { .. }));
}

#[test]
fn accept_derives_deleted_touched_base_files_without_delete_log() {
    let (temp, db_path) = seeded_store("phase9-accept-derived-delete");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project");
    fs::write(project_path.join("src/old.ts"), "old\n").expect("main file");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "delete-derived".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    let materialized = temp
        .root()
        .join("Code/.work/apps/web/delete-derived/src/old.ts");
    fs::create_dir_all(materialized.parent().expect("materialized parent")).expect("work src");
    fs::write(&materialized, "old\n").expect("materialized file");
    fs::remove_file(&materialized).expect("delete materialized file without delete log");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .append_local_write_log(&LocalWriteLogRecord {
            id: "write-derived-delete-old".to_string(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("dev_mac"),
            project_id: Some(ProjectId::new("proj_web")),
            path: format!(
                "{}/src/old.ts",
                temp.root()
                    .join("Code/.work/apps/web/delete-derived")
                    .display()
            ),
            source_path: None,
            operation: "modify".to_string(),
            staged_content_id: None,
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: "human".to_string(),
            settled_at: "2026-06-25T12:01:00Z".to_string(),
            created_at: "2026-06-25T12:01:00Z".to_string(),
        })
        .expect("modify write");
    drop(store);

    let accepted = accept_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "delete-derived".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("accept");

    assert_eq!(serde_json::to_value(accepted.action).unwrap(), "accepted");
    assert!(!project_path.join("src/old.ts").exists());
}

#[test]
fn accept_applies_unlogged_filesystem_deletions() {
    let (temp, db_path) = seeded_store("work-view-accept-unlogged-delete");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project");
    fs::write(project_path.join("src/old.ts"), "old\n").expect("main file");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "delete-unlogged".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    let materialized = temp
        .root()
        .join("Code/.work/apps/web/delete-unlogged/src/old.ts");
    fs::remove_file(&materialized).expect("delete materialized file without log");

    let diff = diff_work_view(WorkSelectorOptions {
        db_path: Some(db_path.clone()),
        selector: "delete-unlogged".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("diff");
    assert_eq!(diff.changes.len(), 1);
    assert_eq!(diff.changes[0].kind, WorkDiffChangeKind::Deleted);

    let accepted = accept_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "delete-unlogged".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("accept");

    assert_eq!(serde_json::to_value(accepted.action).unwrap(), "accepted");
    assert!(!project_path.join("src/old.ts").exists());
}

#[test]
fn diff_includes_unlogged_deletions_alongside_logged_updates() {
    let (temp, db_path) = seeded_store("work-view-diff-mixed-unlogged-delete");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project");
    fs::write(project_path.join("src/edit.ts"), "old\n").expect("edit");
    fs::write(project_path.join("src/delete.ts"), "delete\n").expect("delete");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "mixed".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    let work_root = temp.root().join("Code/.work/apps/web/mixed");
    let edit_file = work_root.join("src/edit.ts");
    fs::write(&edit_file, "new\n").expect("edit work");
    fs::remove_file(work_root.join("src/delete.ts")).expect("delete work");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .append_local_write_log(&LocalWriteLogRecord {
            id: "write-mixed-edit".to_string(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("dev_mac"),
            project_id: Some(ProjectId::new("proj_web")),
            path: edit_file.display().to_string(),
            source_path: None,
            operation: "update".to_string(),
            staged_content_id: None,
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: "human".to_string(),
            settled_at: "2026-06-25T12:02:00Z".to_string(),
            created_at: "2026-06-25T12:02:00Z".to_string(),
        })
        .expect("write");
    drop(store);

    let diff = diff_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "mixed".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("diff");
    let changes = diff
        .changes
        .iter()
        .map(|entry| (entry.path.as_str(), entry.kind))
        .collect::<Vec<_>>();

    assert!(changes.contains(&("src/edit.ts", WorkDiffChangeKind::Modified)));
    assert!(changes.contains(&("src/delete.ts", WorkDiffChangeKind::Deleted)));
}

#[test]
fn filesystem_deletion_overrides_logged_update_in_diff() {
    let (temp, db_path) = seeded_store("work-view-diff-delete-after-update");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project");
    fs::write(project_path.join("src/removed.ts"), "old\n").expect("base file");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "delete-after-update".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    let work_file = temp
        .root()
        .join("Code/.work/apps/web/delete-after-update/src/removed.ts");
    fs::write(&work_file, "updated\n").expect("update work file");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .append_local_write_log(&LocalWriteLogRecord {
            id: "write-before-delete".to_string(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("dev_mac"),
            project_id: Some(ProjectId::new("proj_web")),
            path: work_file.display().to_string(),
            source_path: None,
            operation: "update".to_string(),
            staged_content_id: None,
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: "human".to_string(),
            settled_at: "2026-06-25T12:01:00Z".to_string(),
            created_at: "2026-06-25T12:01:00Z".to_string(),
        })
        .expect("write log");
    drop(store);
    fs::remove_file(work_file).expect("delete after update log");

    let diff = diff_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "delete-after-update".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("diff");

    assert_eq!(diff.changes.len(), 1);
    assert_eq!(diff.changes[0].path, "src/removed.ts");
    assert_eq!(diff.changes[0].kind, WorkDiffChangeKind::Deleted);
}

#[test]
fn clean_accept_preserves_untouched_base_files() {
    let (temp, db_path) = seeded_store("work-view-accept-carry-forward");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project");
    fs::write(project_path.join("src/keep.ts"), "keep\n").expect("base keep");
    fs::write(project_path.join("src/edit.ts"), "base\n").expect("base edit");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "carry-forward".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    let materialized = temp
        .root()
        .join("Code/.work/apps/web/carry-forward/src/edit.ts");
    fs::write(&materialized, "work\n").expect("work edit");

    let accepted = accept_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "carry-forward".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("accept");

    assert_eq!(serde_json::to_value(accepted.action).unwrap(), "accepted");
    assert_eq!(
        fs::read_to_string(project_path.join("src/keep.ts")).expect("base-only file"),
        "keep\n"
    );
    assert_eq!(
        fs::read_to_string(project_path.join("src/edit.ts")).expect("accepted edit"),
        "work\n"
    );
}

#[test]
fn unsupported_overlay_log_does_not_override_filesystem_candidate() {
    let (temp, db_path) = seeded_store("work-view-unsupported-overlay");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project");
    fs::write(project_path.join("src/index.ts"), "main\n").expect("main");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "unsupported".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    let work_file = temp
        .root()
        .join("Code/.work/apps/web/unsupported/src/index.ts");
    fs::write(&work_file, "work\n").expect("work");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .append_local_write_log(&LocalWriteLogRecord {
            id: "write-mmap-unsupported".to_string(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("dev_mac"),
            project_id: Some(ProjectId::new("proj_web")),
            path: work_file.display().to_string(),
            source_path: None,
            operation: "mmap".to_string(),
            staged_content_id: None,
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: "human".to_string(),
            settled_at: "2026-06-25T12:02:00Z".to_string(),
            created_at: "2026-06-25T12:02:00Z".to_string(),
        })
        .expect("unsupported write");
    drop(store);

    let diff = diff_work_view(WorkSelectorOptions {
        db_path: Some(db_path.clone()),
        selector: "unsupported".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("diff");
    assert_eq!(diff.status.level, StatusLevel::Attention);
    assert_eq!(diff.changes[0].kind, WorkDiffChangeKind::PolicyReview);

    let output = accept_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "unsupported".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("accept");

    assert_eq!(serde_json::to_value(output.action).unwrap(), "accepted");
    assert_eq!(
        fs::read_to_string(project_path.join("src/index.ts")).expect("main"),
        "work\n"
    );
    assert!(output.work_view.attention.is_empty());
}

#[test]
fn accept_ignores_source_control_metadata_scaffold() {
    let (temp, db_path) = seeded_store("phase9-accept-git-scaffold");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project");
    fs::write(project_path.join("src/message.ts"), "old\n").expect("main file");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "git-edit".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    let materialized = temp.root().join("Code/.work/apps/web/git-edit");
    fs::create_dir_all(materialized.join(".git")).expect("git dir");
    fs::write(materialized.join(".git/config"), "[core]\n").expect("git config");
    fs::write(materialized.join("src/message.ts"), "new\n").expect("work file");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .append_local_write_log(&LocalWriteLogRecord {
            id: "write-git-scaffold".to_string(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("dev_mac"),
            project_id: Some(ProjectId::new("proj_web")),
            path: format!("{}/.git/config", materialized.display()),
            source_path: None,
            operation: "modify".to_string(),
            staged_content_id: None,
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: "materialization".to_string(),
            settled_at: "2026-06-25T12:02:00Z".to_string(),
            created_at: "2026-06-25T12:02:00Z".to_string(),
        })
        .expect("git scaffold write");
    drop(store);

    let accepted = accept_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "git-edit".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("accept");

    assert_eq!(serde_json::to_value(accepted.action).unwrap(), "accepted");
    assert!(!project_path.join(".git/config").exists());
    assert_eq!(
        fs::read_to_string(project_path.join("src/message.ts")).expect("accepted file"),
        "new\n"
    );
}

#[test]
fn diff_ignores_main_project_write_log_entries() {
    let (temp, db_path) = seeded_store("phase9-diff-main-write-log");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "scoped-diff".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    let materialized = temp.root().join("Code/.work/apps/web/scoped-diff/src");
    fs::create_dir_all(&materialized).expect("work src");
    fs::write(materialized.join("work.ts"), "export const work = true;\n").expect("work file");

    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .append_local_write_log(&LocalWriteLogRecord {
            id: "write-main".to_string(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("dev_mac"),
            project_id: Some(ProjectId::new("proj_web")),
            path: "apps/web/src/main.ts".to_string(),
            source_path: None,
            operation: "modify".to_string(),
            staged_content_id: None,
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: "human".to_string(),
            settled_at: "2026-06-25T12:02:00Z".to_string(),
            created_at: "2026-06-25T12:02:00Z".to_string(),
        })
        .expect("main write");
    drop(store);

    let diff = diff_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "scoped-diff".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("diff");

    assert_eq!(diff.changes.len(), 1);
    assert_eq!(diff.changes[0].path, "src/work.ts");
}

#[test]
fn diff_ignores_sibling_work_view_name_prefixes() {
    let (temp, db_path) = seeded_store("phase9-diff-prefix-sibling");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "auth".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("auth work view");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "auth-fix".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("auth-fix work view");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .append_local_write_log(&LocalWriteLogRecord {
            id: "write-auth-fix".to_string(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("dev_mac"),
            project_id: Some(ProjectId::new("proj_web")),
            path: ".work/apps/web/auth-fix/src/leak.ts".to_string(),
            source_path: None,
            operation: "create".to_string(),
            staged_content_id: None,
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: "human".to_string(),
            settled_at: "2026-06-25T12:01:00Z".to_string(),
            created_at: "2026-06-25T12:01:00Z".to_string(),
        })
        .expect("sibling write");
    drop(store);

    let diff = diff_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "auth".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("diff");

    assert!(diff.changes.is_empty());
}

#[test]
fn conflicting_accept_becomes_review_ready_without_overwriting_main_view() {
    let (temp, db_path) = seeded_store("phase9-accept-conflict");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project src");
    fs::write(project_path.join("src/index.ts"), "main\n").expect("main file");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "conflict-file".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    fs::write(project_path.join("src/index.ts"), "main changed\n").expect("main update");
    let store = MetadataStore::open(&db_path).expect("metadata");
    store
        .append_local_write_log(&LocalWriteLogRecord {
            id: "write-conflict-main".to_string(),
            workspace_id: WorkspaceId::new("ws_code"),
            device_id: DeviceId::new("dev_mac"),
            project_id: Some(ProjectId::new("proj_web")),
            path: "apps/web/src/index.ts".to_string(),
            source_path: None,
            operation: "modify".to_string(),
            staged_content_id: None,
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: "human".to_string(),
            settled_at: "2026-06-25T12:02:00Z".to_string(),
            created_at: "2026-06-25T12:02:00Z".to_string(),
        })
        .expect("main write");
    drop(store);
    let materialized = temp.root().join("Code/.work/apps/web/conflict-file/src");
    fs::create_dir_all(&materialized).expect("work src");
    fs::write(materialized.join("index.ts"), "work\n").expect("work file");

    let output = accept_work_view(WorkSelectorOptions {
        db_path: Some(db_path.clone()),
        selector: "conflict-file".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("accept");

    assert_eq!(serde_json::to_value(output.action).unwrap(), "review-ready");
    assert_eq!(
        serde_json::to_value(output.work_view.lifecycle).unwrap(),
        "review-ready"
    );
    assert_eq!(
        fs::read_to_string(project_path.join("src/index.ts")).expect("main file"),
        "main changed\n"
    );
    let persisted = MetadataStore::open(&db_path)
        .expect("metadata")
        .work_views(&WorkspaceId::new("ws_code"), true, None)
        .expect("work views");
    assert!(persisted.iter().any(|view| {
        view.id == output.work_view.id && view.lifecycle == WorkViewLifecycle::ReviewReady
    }));

    let status = compose_status(StatusOptions {
        db_path: Some(db_path.clone()),
        requested_path: Some(project_path.display().to_string()),
        workspace_scope: false,
        generated_at: now(),
    })
    .expect("status");
    assert_eq!(status.status.level, StatusLevel::Attention, "{status:#?}");

    discard_work_view(WorkSelectorOptions {
        db_path: Some(db_path.clone()),
        selector: "conflict-file".to_string(),
        paths: Vec::new(),
        generated_at: "2026-06-25T12:10:00Z".to_string(),
    })
    .expect("discard");
    let status = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: Some(project_path.display().to_string()),
        workspace_scope: false,
        generated_at: now(),
    })
    .expect("status");
    assert_eq!(status.status.level, StatusLevel::Healthy);
    assert!(
        status
            .status
            .attention_items
            .iter()
            .all(|item| !item.contains("review ready"))
    );
}

#[test]
fn status_reports_durable_review_ready_work_view_without_event() {
    let (temp, db_path) = seeded_store("phase9-status-durable-work-view");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "durable-review".to_string(),
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
        .work_views_by_name(&workspace.id, None, "durable-review")
        .expect("work views")
        .pop()
        .expect("work view");
    let view_id = view.id.as_str().to_string();
    view.lifecycle = WorkViewLifecycle::ReviewReady;
    view.sync_state = WorkViewSyncState::Attention;
    store.upsert_work_view(&view).expect("review ready");
    drop(store);

    let status = compose_status(StatusOptions {
        db_path: Some(db_path),
        requested_path: Some(project_path.display().to_string()),
        workspace_scope: false,
        generated_at: now(),
    })
    .expect("status");

    assert_eq!(status.status.level, StatusLevel::Attention);
    assert!(status.items.iter().any(|item| {
        item.kind == bowline_core::status::StatusItemKind::WorkView
            && item
                .subject
                .as_ref()
                .is_some_and(|subject| subject.id == view_id.as_str())
    }));
}
