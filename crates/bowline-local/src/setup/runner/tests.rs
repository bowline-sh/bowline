use super::write_setup_log;
use crate::workspace::TempWorkspace;
use std::fs;

#[cfg(unix)]
#[test]
fn setup_log_writer_replaces_stale_symlink_without_following_it() {
    use std::os::unix::{fs::PermissionsExt, fs::symlink};

    let state = TempWorkspace::new("setup-log-state").expect("state");
    let outside = TempWorkspace::new("setup-log-outside").expect("outside");
    let db_path = state.root().join("metadata.sqlite3");
    let log_dir = state.root().join("setup-logs");
    fs::create_dir_all(&log_dir).expect("log dir");
    let outside_target = outside.root().join("target");
    fs::write(&outside_target, b"outside").expect("outside");
    symlink(&outside_target, log_dir.join("setup_test.log")).expect("log symlink");

    let log_path = write_setup_log(&db_path, "setup_test", "SECRET=[redacted]").expect("log write");

    assert_eq!(
        fs::read(outside_target).expect("outside unchanged"),
        b"outside"
    );
    assert_eq!(
        fs::read_to_string(&log_path).expect("log text"),
        "SECRET=[redacted]"
    );
    assert_eq!(
        fs::metadata(log_path)
            .expect("log metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}
