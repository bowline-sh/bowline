use super::*;

#[test]
fn accept_secret_bearing_work_file_requires_review() {
    let (temp, db_path) = seeded_store("phase9-accept-secret-review");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "secret-edit".to_string(),
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    let materialized = temp.root().join("Code/.work/apps/web/secret-edit");
    fs::write(materialized.join(".env.local"), "TOKEN=secret\n").expect("work env");

    let accepted = accept_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "secret-edit".to_string(),
        generated_at: now(),
    })
    .expect("accept");

    assert_eq!(
        serde_json::to_value(accepted.work_view.lifecycle).unwrap(),
        "review-ready"
    );
    assert!(!project_path.join(".env.local").exists());
}

#[test]
fn clean_accept_applies_new_work_view_files_to_main_view() {
    let (temp, db_path) = seeded_store("phase9-accept-clean");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "new-file".to_string(),
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
        generated_at: now(),
    })
    .expect("accept");

    assert_eq!(serde_json::to_value(accepted.action).unwrap(), "accepted");
    assert_eq!(
        fs::read_to_string(project_path.join("src/new.ts")).expect("main file"),
        "export const ok = true;\n"
    );
}

#[test]
fn accept_dependency_file_ignores_local_regenerate_churn() {
    let (temp, db_path) = seeded_store("phase9-accept-policy");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "deps".to_string(),
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
    create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "edit-existing".to_string(),
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
    create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "unlogged-main".to_string(),
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
fn accept_detects_main_view_deletion_from_base_hash() {
    let (temp, db_path) = seeded_store("phase9-accept-main-delete");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project");
    fs::write(project_path.join("src/index.ts"), "base\n").expect("main file");
    create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "main-delete".to_string(),
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
    create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "delete-old".to_string(),
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

    let accepted = accept_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "delete-old".to_string(),
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
    create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "delete-recreate".to_string(),
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
    create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "rename-file".to_string(),
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
fn accept_derives_deleted_touched_base_files_without_delete_log() {
    let (temp, db_path) = seeded_store("phase9-accept-derived-delete");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project");
    fs::write(project_path.join("src/old.ts"), "old\n").expect("main file");
    create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "delete-derived".to_string(),
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
    create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "delete-unlogged".to_string(),
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
        generated_at: now(),
    })
    .expect("diff");
    assert_eq!(diff.changes.len(), 1);
    assert_eq!(diff.changes[0].kind, WorkDiffChangeKind::Deleted);

    let accepted = accept_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "delete-unlogged".to_string(),
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
    create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "mixed".to_string(),
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
fn clean_accept_preserves_untouched_base_files() {
    let (temp, db_path) = seeded_store("work-view-accept-carry-forward");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project");
    fs::write(project_path.join("src/keep.ts"), "keep\n").expect("base keep");
    fs::write(project_path.join("src/edit.ts"), "base\n").expect("base edit");
    create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "carry-forward".to_string(),
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
fn unsupported_overlay_write_requires_review_without_mutating_main() {
    let (temp, db_path) = seeded_store("work-view-unsupported-overlay");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project");
    fs::write(project_path.join("src/index.ts"), "main\n").expect("main");
    create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "unsupported".to_string(),
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
        generated_at: now(),
    })
    .expect("diff");
    assert_eq!(diff.status.level, StatusLevel::Attention);
    assert_eq!(diff.changes[0].kind, WorkDiffChangeKind::PolicyReview);

    let output = accept_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "unsupported".to_string(),
        generated_at: now(),
    })
    .expect("accept");

    assert_eq!(serde_json::to_value(output.action).unwrap(), "review-ready");
    assert_eq!(
        fs::read_to_string(project_path.join("src/index.ts")).expect("main"),
        "main\n"
    );
    assert!(
        output
            .work_view
            .attention
            .iter()
            .any(|item| item.contains("src/index.ts"))
    );
}

#[test]
fn accept_ignores_source_control_metadata_scaffold() {
    let (temp, db_path) = seeded_store("phase9-accept-git-scaffold");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project");
    fs::write(project_path.join("src/message.ts"), "old\n").expect("main file");
    create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "git-edit".to_string(),
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
    create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "scoped-diff".to_string(),
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
    create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "auth".to_string(),
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("auth work view");
    create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "auth-fix".to_string(),
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
    create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "conflict-file".to_string(),
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

    let status = compose_status(StatusOptions {
        db_path: Some(db_path.clone()),
        requested_path: Some(project_path.display().to_string()),
        workspace_scope: false,
        generated_at: now(),
    })
    .expect("status");
    assert_eq!(status.status.level, StatusLevel::Attention);

    discard_work_view(WorkSelectorOptions {
        db_path: Some(db_path.clone()),
        selector: "conflict-file".to_string(),
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
}

#[test]
fn status_reports_durable_review_ready_work_view_without_event() {
    let (temp, db_path) = seeded_store("phase9-status-durable-work-view");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    create_work_view(WorkonOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "durable-review".to_string(),
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
