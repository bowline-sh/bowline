use super::{SetupRunError, preserve_setup_error, write_setup_log};
use crate::{metadata::MetadataError, workspace::TempWorkspace};
use std::{fs, io};

#[test]
fn terminal_state_write_failure_does_not_mask_setup_error() {
    let setup_error = SetupRunError::Io(io::Error::other("failed to spawn setup shell"));
    let state_error = MetadataError::Io(io::Error::other("metadata database is unavailable"));

    let preserved = preserve_setup_error(setup_error, Err(state_error));

    assert_eq!(
        preserved.to_string(),
        "setup run failed: failed to spawn setup shell"
    );
    match preserved {
        SetupRunError::Io(error) => {
            assert_eq!(error.to_string(), "failed to spawn setup shell");
        }
        other => panic!("expected original setup I/O error, got {other}"),
    }
}

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
