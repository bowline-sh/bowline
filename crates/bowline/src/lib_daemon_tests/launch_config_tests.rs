use std::path::Path;

use super::tempfile_dir;

#[test]
fn daemon_service_launch_config_refuses_before_setup_without_mutating_metadata() {
    let temp = tempfile_dir("bowline-daemon-service-default");
    let db_path = temp.join("state").join("local.sqlite3");
    let store = bowline_local::metadata::MetadataStore::open(&db_path).expect("metadata store");
    let daemon = temp.join("bowline-daemon");

    let error = match crate::daemon_service_launch_config_for_store(
        Path::new("/tmp/bowline.sock"),
        &db_path,
        &store,
        daemon.clone(),
    ) {
        Ok(_) => panic!("service launch config should require authenticated setup"),
        Err(error) => error,
    };

    assert!(error.contains("run `bowline setup --root <path>` first"));
    assert!(
        store
            .current_workspace()
            .expect("current workspace")
            .is_none()
    );
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn daemon_service_launch_config_uses_authenticated_accepted_root() {
    let temp = tempfile_dir("bowline-daemon-service-authenticated");
    let state = temp.join("state");
    let db_path = state.join("local.sqlite3");
    let store = bowline_local::metadata::MetadataStore::open(&db_path).expect("metadata store");
    let workspace_id = bowline_core::ids::WorkspaceId::new("ws_code_account");
    store
        .insert_workspace(&workspace_id, "Code", "2026-07-15T12:00:00Z")
        .expect("workspace");
    store
        .insert_root(
            "root_account",
            &workspace_id,
            "~/Projects/Bowline",
            "2026-07-15T12:00:00Z",
        )
        .expect("root");
    std::fs::write(
        state.join("daemon.env"),
        "BOWLINE_WORKSPACE_ID=ws_code_account\nBOWLINE_DEVICE_ID=device_fixture\n",
    )
    .expect("persisted daemon identity");
    let daemon = temp.join("bowline-daemon");

    let launch = crate::daemon_service_launch_config_for_store(
        Path::new("/tmp/bowline.sock"),
        &db_path,
        &store,
        daemon.clone(),
    )
    .expect("authenticated service launch config");

    assert_eq!(launch.workspace_id, workspace_id);
    assert_eq!(launch.root, crate::expand_home_path("~/Projects/Bowline"));
    assert_eq!(launch.daemon, daemon);
    assert_eq!(
        launch.state_root,
        std::fs::canonicalize(state).expect("canonical state root")
    );
    assert_eq!(launch.device_id.as_str(), "device_fixture");
    let _ = std::fs::remove_dir_all(temp);
}

#[cfg(unix)]
#[test]
fn daemon_service_launch_config_follows_the_workspace_database_symlink() {
    use std::os::unix::fs::symlink;

    let temp = tempfile_dir("bowline-daemon-service-symlink");
    let default_state = temp.join("default");
    let workspace_state = temp.join("workspace");
    std::fs::create_dir_all(&default_state).expect("default state");
    std::fs::create_dir_all(&workspace_state).expect("workspace state");
    let workspace_db = workspace_state.join("local.sqlite3");
    let store =
        bowline_local::metadata::MetadataStore::open(&workspace_db).expect("metadata store");
    let workspace_id = bowline_core::ids::WorkspaceId::new("ws_code_account");
    store
        .insert_workspace(&workspace_id, "Code", "2026-07-15T12:00:00Z")
        .expect("workspace");
    store
        .insert_root(
            "root_account",
            &workspace_id,
            "~/Code",
            "2026-07-15T12:00:00Z",
        )
        .expect("root");
    std::fs::write(
        workspace_state.join("daemon.env"),
        "BOWLINE_WORKSPACE_ID=ws_code_account\nBOWLINE_DEVICE_ID=device_remote\nBOWLINE_ACCOUNT_SESSION_ID=session_remote\n",
    )
    .expect("daemon env");
    let default_db = default_state.join("local.sqlite3");
    symlink(&workspace_db, &default_db).expect("default database symlink");

    let launch = crate::daemon_service_launch_config_for_store(
        Path::new("/tmp/bowline.sock"),
        &default_db,
        &store,
        temp.join("bowline-daemon"),
    )
    .expect("service launch config");

    assert_eq!(
        launch.state_root,
        std::fs::canonicalize(&workspace_state).expect("canonical workspace state")
    );
    assert_eq!(launch.device_id.as_str(), "device_remote");
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn daemon_launch_uses_persisted_device_id() {
    let temp = tempfile_dir("bowline-daemon-persisted-device");
    let state = temp.join("state");
    let db_path = state.join("local.sqlite3");
    std::fs::create_dir_all(&state).expect("state dir");
    let workspace_id = bowline_core::ids::WorkspaceId::new("ws_code_account");
    std::fs::write(
            state.join("daemon.env"),
            format!(
                "BOWLINE_WORKSPACE_ID={}\nBOWLINE_DEVICE_ID=device_remote_box\nBOWLINE_WORKOS_REFRESH_TOKEN=stale-refresh\n",
                workspace_id.as_str()
            ),
        )
        .expect("daemon env");
    let store = bowline_local::metadata::MetadataStore::open(&db_path).expect("metadata store");
    store
        .insert_workspace(&workspace_id, "Code", "2026-07-15T12:00:00Z")
        .expect("workspace");
    store
        .insert_root(
            "root_account",
            &workspace_id,
            "~/Code",
            "2026-07-15T12:00:00Z",
        )
        .expect("root");
    let daemon = temp.join("bowline-daemon");

    let launch = crate::daemon_service_launch_config_for_store(
        Path::new("/tmp/bowline.sock"),
        &db_path,
        &store,
        daemon,
    )
    .expect("service launch config");

    assert_eq!(launch.device_id.as_str(), "device_remote_box");
    assert_eq!(
        bowline_local::daemon_env::value(&state, "BOWLINE_WORKOS_REFRESH_TOKEN"),
        None
    );
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn persisted_daemon_device_id_is_workspace_bound() {
    let temp = tempfile_dir("bowline-daemon-persisted-device-workspace");
    let state = temp.join("state");
    std::fs::create_dir_all(&state).expect("state dir");
    std::fs::write(
        state.join("daemon.env"),
        "BOWLINE_WORKSPACE_ID=ws_a\nBOWLINE_DEVICE_ID=device_a\n",
    )
    .expect("daemon env");

    assert_eq!(
        crate::persisted_daemon_device_id_for_workspace(
            &state,
            &bowline_core::ids::WorkspaceId::new("ws_a")
        )
        .as_deref(),
        Some("device_a")
    );
    assert_eq!(
        crate::persisted_daemon_device_id_for_workspace(
            &state,
            &bowline_core::ids::WorkspaceId::new("ws_b")
        ),
        None
    );
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn persisted_daemon_env_excludes_refresh_tokens() {
    let temp = tempfile_dir("bowline-daemon-env-sanitized");
    std::fs::write(
            temp.join("daemon.env"),
            "BOWLINE_ACCOUNT_SESSION_ID=session\nBOWLINE_WORKOS_ACCESS_TOKEN=access\nBOWLINE_WORKOS_REFRESH_TOKEN=refresh\nBOWLINE_DEVICE_ID=device_remote\n",
        )
        .expect("daemon env");

    let env = crate::persisted_daemon_env(&temp);

    assert_eq!(
        env.get("BOWLINE_ACCOUNT_SESSION_ID").map(String::as_str),
        Some("session")
    );
    assert_eq!(
        env.get("BOWLINE_WORKOS_ACCESS_TOKEN").map(String::as_str),
        Some("access")
    );
    assert_eq!(
        env.get("BOWLINE_DEVICE_ID").map(String::as_str),
        Some("device_remote")
    );
    assert!(!env.contains_key("BOWLINE_WORKOS_REFRESH_TOKEN"));
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn daemon_binary_path_requires_sibling_daemon() {
    let temp = tempfile_dir("bowline-daemon-missing");
    let error = crate::daemon_binary_path_next_to(&temp.join("bowline"))
        .expect_err("missing daemon binary");

    assert!(error.contains("bowline-daemon binary is unavailable"));
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn daemon_binary_path_accepts_executable_sibling() {
    let temp = tempfile_dir("bowline-daemon-present");
    let daemon = temp.join(if cfg!(windows) {
        "bowline-daemon.exe"
    } else {
        "bowline-daemon"
    });
    std::fs::write(&daemon, b"daemon").expect("daemon file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&daemon)
            .expect("daemon metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&daemon, permissions).expect("daemon permissions");
    }

    assert_eq!(
        crate::daemon_binary_path_next_to(&temp.join("bowline")).expect("daemon path"),
        daemon
    );
    let _ = std::fs::remove_dir_all(temp);
}

#[test]
fn daemon_binary_path_accepts_target_debug_fallback() {
    let temp = tempfile_dir("bowline-daemon-target-debug");
    let deps = temp.join("target").join("debug").join("deps");
    std::fs::create_dir_all(&deps).expect("debug deps dir");
    let daemon = temp.join("target").join("debug").join(if cfg!(windows) {
        "bowline-daemon.exe"
    } else {
        "bowline-daemon"
    });
    std::fs::write(&daemon, b"daemon").expect("daemon file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&daemon)
            .expect("daemon metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&daemon, permissions).expect("daemon permissions");
    }

    assert_eq!(
        crate::daemon_binary_path_next_to(&deps.join("bowline")).expect("daemon path"),
        daemon
    );
    let _ = std::fs::remove_dir_all(temp);
}
