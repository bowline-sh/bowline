use super::*;

#[test]
fn accept_rejects_symlinked_work_view_entries() {
    let (temp, db_path) = seeded_store("phase9-accept-symlink");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "symlink-file".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    let outside = temp.root().join("outside-secret");
    fs::write(&outside, "do not copy").expect("outside");
    let work_root = temp.root().join("Code/.work/apps/web/symlink-file");
    symlink(&outside, work_root.join("linked")).expect("symlink");

    let error = accept_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "symlink-file".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect_err("symlink should be rejected");

    assert!(error.to_string().contains("symlinks are not followed"));
    assert!(!project_path.join("linked").exists());
}

#[test]
fn accept_rejects_symlinked_work_view_root() {
    let (temp, db_path) = seeded_store("phase9-accept-root-symlink");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "root-link".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    let work_root = temp.root().join("Code/.work/apps/web/root-link");
    fs::remove_dir_all(&work_root).expect("remove work root");
    let outside = temp.root().join("outside-work-root");
    fs::create_dir_all(outside.join("src")).expect("outside src");
    fs::write(outside.join("src/leak.ts"), "leak\n").expect("outside file");
    symlink(&outside, &work_root).expect("root symlink");

    let error = accept_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "root-link".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect_err("symlinked work root should fail");

    assert!(error.to_string().contains("work view root escapes .work"));
    assert!(!project_path.join("src/leak.ts").exists());
}

#[test]
fn cleanup_rejects_tampered_materialization_outside_work_namespace() {
    let (temp, db_path) = seeded_store("phase9-cleanup-tamper");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "tampered".to_string(),
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
        .work_views_by_name(&workspace.id, None, "tampered")
        .expect("work views")
        .pop()
        .expect("work view");
    let outside = temp.root().join("outside-do-not-delete");
    fs::create_dir_all(&outside).expect("outside");
    view.host_materializations = vec![outside.display().to_string()];
    view.lifecycle = WorkViewLifecycle::Discarded;
    view.visibility = WorkViewVisibility::Hidden;
    store.upsert_work_view(&view).expect("tampered view");
    drop(store);

    let error = cleanup_work_views(WorkCleanupOptions {
        db_path: Some(db_path),
        apply: true,
        generated_at: now(),
    })
    .expect_err("outside cleanup should be rejected");

    assert!(error.to_string().contains("cleanup is limited to .work"));
    assert!(outside.is_dir());
}

#[test]
fn cleanup_rejects_parent_component_materialization_escape() {
    let (temp, db_path) = seeded_store("phase9-cleanup-parent-traversal");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(project_path.join("keep")).expect("project keep dir");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "traversal".to_string(),
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
        .work_views_by_name(&workspace.id, None, "traversal")
        .expect("work views")
        .pop()
        .expect("work view");
    let traversal = temp
        .root()
        .join("Code/.work/apps/web/traversal/../../../../apps/web/keep");
    assert!(traversal.exists());
    view.host_materializations = vec![traversal.display().to_string()];
    view.lifecycle = WorkViewLifecycle::Discarded;
    view.visibility = WorkViewVisibility::Hidden;
    store.upsert_work_view(&view).expect("traversal view");
    drop(store);

    let error = cleanup_work_views(WorkCleanupOptions {
        db_path: Some(db_path),
        apply: true,
        generated_at: now(),
    })
    .expect_err("parent traversal cleanup should be rejected");

    assert!(error.to_string().contains("cleanup is limited to .work"));
    assert!(project_path.join("keep").is_dir());
}

#[test]
fn cleanup_rejects_symlinked_work_namespace_root() {
    let (temp, db_path) = seeded_store("phase9-cleanup-namespace-symlink");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(&project_path).expect("project");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "symlink-namespace".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    discard_work_view(WorkSelectorOptions {
        db_path: Some(db_path.clone()),
        selector: "symlink-namespace".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("discard");
    let namespace_root = temp.root().join("Code/.work/apps/web");
    fs::remove_dir_all(&namespace_root).expect("remove namespace");
    let outside = temp.root().join("outside-do-not-delete");
    fs::create_dir_all(outside.join("symlink-namespace")).expect("outside target");
    symlink(&outside, &namespace_root).expect("namespace symlink");

    let error = cleanup_work_views(WorkCleanupOptions {
        db_path: Some(db_path),
        apply: true,
        generated_at: now(),
    })
    .expect_err("symlinked namespace should be rejected");

    assert!(
        error
            .to_string()
            .contains("cleanup namespace escapes .work")
    );
    assert!(outside.join("symlink-namespace").is_dir());
}
