use std::fs;

use super::{
    account_session::{
        clear_persisted_account_session_from, persisted_account_session_revocation_from,
        persisted_account_session_state_root,
    },
    tempfile_dir,
};

#[test]
fn persisted_daemon_env_supplies_a_complete_remote_account_session() {
    let temp = tempfile_dir("bowline-runtime-account-session");
    fs::write(
        temp.join("daemon.env"),
        "BOWLINE_ACCOUNT_SESSION_ID=bowline_session_remote\nBOWLINE_ACCOUNT_SESSION_REVOCATION_TOKEN=bowline_revoke_remote\n",
    )
    .expect("daemon env");

    let session = persisted_account_session_revocation_from(&temp)
        .expect("persisted session")
        .expect("complete session");

    assert_eq!(session.session_id, "bowline_session_remote");
    assert_eq!(session.revocation_token, "bowline_revoke_remote");
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn persisted_daemon_env_rejects_partial_remote_account_sessions() {
    let temp = tempfile_dir("bowline-runtime-partial-account-session");
    fs::write(
        temp.join("daemon.env"),
        "BOWLINE_ACCOUNT_SESSION_ID=bowline_session_remote\n",
    )
    .expect("daemon env");

    let error = persisted_account_session_revocation_from(&temp)
        .expect_err("partial persisted session is rejected");

    assert!(error.contains("must be configured together"));
    let _ = fs::remove_dir_all(temp);
}

#[cfg(unix)]
#[test]
fn persisted_account_session_follows_the_default_database_symlink() {
    use std::os::unix::fs::symlink;

    let temp = tempfile_dir("bowline-runtime-account-session-symlink");
    let workspace = temp.join("workspace");
    let default = temp.join("default");
    fs::create_dir_all(&workspace).expect("workspace state");
    fs::create_dir_all(&default).expect("default state");
    let workspace_db = workspace.join("local.sqlite3");
    fs::write(&workspace_db, b"sqlite").expect("workspace database");
    let default_db = default.join("local.sqlite3");
    symlink(&workspace_db, &default_db).expect("default database symlink");
    let canonical_workspace = fs::canonicalize(&workspace).expect("canonical workspace state");

    assert_eq!(
        persisted_account_session_state_root(&default_db).as_deref(),
        Some(canonical_workspace.as_path())
    );
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn persisted_account_session_uses_adjacent_state_when_database_is_missing() {
    let temp = tempfile_dir("bowline-runtime-account-session-missing-db");
    let missing_db = temp.join("local.sqlite3");

    assert_eq!(
        persisted_account_session_state_root(&missing_db).as_deref(),
        Some(temp.as_path())
    );
    let _ = fs::remove_dir_all(temp);
}

#[test]
fn clearing_a_persisted_session_preserves_other_daemon_configuration() {
    let temp = tempfile_dir("bowline-runtime-clear-account-session");
    let env_path = temp.join("daemon.env");
    fs::write(
        &env_path,
        "CONVEX_URL=https://example.convex.cloud\nBOWLINE_ACCOUNT_SESSION_ID=bowline_session_remote\nBOWLINE_ACCOUNT_SESSION_REVOCATION_TOKEN=bowline_revoke_remote\nBOWLINE_DEVICE_ID=device_remote\n",
    )
    .expect("daemon env");

    assert!(clear_persisted_account_session_from(&temp).expect("clear persisted account session"));
    assert_eq!(
        fs::read_to_string(&env_path).expect("remaining daemon env"),
        "CONVEX_URL=https://example.convex.cloud\nBOWLINE_DEVICE_ID=device_remote\n"
    );
    let _ = fs::remove_dir_all(temp);
}
