use super::*;

#[test]
fn untouched_work_view_does_not_delete_hidden_secret() {
    let (temp, db_path) = seeded_store("work-view-hidden-secret-accept");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(project_path.join("src")).expect("project source");
    fs::write(
        project_path.join("src/index.ts"),
        "export const ok = true;\n",
    )
    .expect("ordinary source");
    let secret_path = project_path.join("id_rsa");
    let secret_bytes = b"private-key-sentinel\n";
    fs::write(&secret_path, secret_bytes).expect("hidden secret");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(&secret_path, fs::Permissions::from_mode(0o600))
            .expect("secret permissions");
    }
    let secret_metadata = fs::metadata(&secret_path).expect("secret metadata");

    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "hidden-secret".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");

    let work_root = temp.root().join("Code/.work/apps/web/hidden-secret");
    assert!(work_root.join("src/index.ts").exists());
    assert!(!work_root.join("id_rsa").exists());

    let accepted = accept_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "hidden-secret".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("accept untouched work view");

    assert_eq!(serde_json::to_value(accepted.action).unwrap(), "accepted");
    assert_eq!(
        fs::read(&secret_path).expect("main secret remains present"),
        secret_bytes
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        assert_eq!(
            fs::metadata(&secret_path)
                .expect("preserved secret metadata")
                .permissions()
                .mode()
                & 0o777,
            secret_metadata.permissions().mode() & 0o777
        );
    }
}

#[test]
fn deleting_exposed_directory_preserves_never_exposed_hidden_child() {
    let (temp, db_path) = seeded_store("work-view-hidden-child-directory");
    let project_path = temp.root().join("Code/apps/web");
    fs::create_dir_all(project_path.join("keys")).expect("keys");
    fs::write(project_path.join("keys/visible.txt"), "visible\n").expect("visible");
    fs::write(project_path.join("keys/id_rsa"), "private\n").expect("hidden");
    create_work_view(WorkCreateOptions {
        db_path: Some(db_path.clone()),
        project_path: project_path.display().to_string(),
        name: "hidden-child".to_string(),
        base_snapshot_selector: None,
        owner_device_id: None,
        generated_at: now(),
    })
    .expect("work view");
    let work_keys = temp.root().join("Code/.work/apps/web/hidden-child/keys");
    assert!(work_keys.join("visible.txt").exists());
    assert!(!work_keys.join("id_rsa").exists());
    fs::remove_dir_all(&work_keys).expect("delete exposed directory");

    accept_work_view(WorkSelectorOptions {
        db_path: Some(db_path),
        selector: "hidden-child".to_string(),
        paths: Vec::new(),
        generated_at: now(),
    })
    .expect("accept");

    assert!(!project_path.join("keys/visible.txt").exists());
    assert_eq!(
        fs::read(project_path.join("keys/id_rsa")).expect("hidden child survives"),
        b"private\n"
    );
}
