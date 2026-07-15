use std::{
    fs,
    path::{Path, PathBuf},
};

use super::{WorkViewError, WorkViewOverlaySyncError};

pub(super) fn overlay_staging_root(
    work_root: &Path,
    overlay_id: &str,
) -> Result<PathBuf, WorkViewOverlaySyncError> {
    let parent = work_root
        .parent()
        .ok_or(WorkViewOverlaySyncError::MissingStateRoot)?;
    let name = work_root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| WorkViewError::UnsafeWorkViewPath {
            path: work_root.display().to_string(),
            reason: "work-view root has no valid final component",
        })?;
    let suffix = overlay_id.chars().take(20).collect::<String>();
    Ok(parent.join(format!(".{name}.overlay-{suffix}.staging")))
}

pub(super) fn publish_overlay_tree(
    work_root: &Path,
    staging_root: &Path,
    overlay_id: &str,
) -> Result<PathBuf, WorkViewOverlaySyncError> {
    let backup = overlay_backup_root(work_root, overlay_id)?;
    if backup.exists() {
        fs::remove_dir_all(&backup).map_err(WorkViewError::from)?;
    }
    fs::rename(work_root, &backup).map_err(WorkViewError::from)?;
    if let Err(error) = fs::rename(staging_root, work_root) {
        let rollback = fs::rename(&backup, work_root);
        return match rollback {
            Ok(()) => Err(WorkViewError::from(error).into()),
            Err(rollback_error) => Err(WorkViewError::AcceptRollbackFailed {
                path: work_root.display().to_string(),
                reason: rollback_error.to_string(),
            }
            .into()),
        };
    }
    Ok(backup)
}

pub(super) fn rollback_overlay_publish(
    work_root: &Path,
    backup: &Path,
) -> Result<(), WorkViewOverlaySyncError> {
    fs::remove_dir_all(work_root).map_err(WorkViewError::from)?;
    fs::rename(backup, work_root).map_err(WorkViewError::from)?;
    Ok(())
}

pub(super) fn recover_overlay_publish(
    work_root: &Path,
    overlay_id: &str,
    receipt_is_committed: bool,
) -> Result<(), WorkViewOverlaySyncError> {
    let backup = overlay_backup_root(work_root, overlay_id)?;
    match (work_root.exists(), backup.exists()) {
        (false, true) => fs::rename(backup, work_root).map_err(WorkViewError::from)?,
        (true, true) if receipt_is_committed => {
            fs::remove_dir_all(backup).map_err(WorkViewError::from)?;
        }
        (true, true) => rollback_overlay_publish(work_root, &backup)?,
        (true, false) => {}
        (false, false) => {
            return Err(WorkViewError::SnapshotMaterialization {
                snapshot_id: overlay_id.to_string(),
                reason: "work-view materialization and recovery backup are both missing"
                    .to_string(),
            }
            .into());
        }
    }
    Ok(())
}

fn overlay_backup_root(
    work_root: &Path,
    overlay_id: &str,
) -> Result<PathBuf, WorkViewOverlaySyncError> {
    let parent = work_root
        .parent()
        .ok_or(WorkViewOverlaySyncError::MissingStateRoot)?;
    let suffix = overlay_id.chars().take(20).collect::<String>();
    Ok(parent.join(format!(".bowline-overlay-{suffix}.backup")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::TempWorkspace;

    #[test]
    fn interrupted_publish_restores_backup_until_receipt_commits() {
        let temp = TempWorkspace::new("overlay-publish-recovery").expect("temp workspace");
        let work_root = temp.root().join("view");
        fs::create_dir(&work_root).expect("work root");
        fs::write(work_root.join("state"), b"new").expect("new tree");
        let backup = overlay_backup_root(&work_root, "cid_recovery").expect("backup path");
        fs::create_dir(&backup).expect("backup root");
        fs::write(backup.join("state"), b"old").expect("old tree");

        recover_overlay_publish(&work_root, "cid_recovery", false).expect("pre-commit recovery");
        assert_eq!(
            fs::read(work_root.join("state")).expect("restored tree"),
            b"old"
        );
        assert!(!backup.exists());
    }

    #[test]
    fn committed_publish_discards_recovery_backup() {
        let temp = TempWorkspace::new("overlay-publish-committed").expect("temp workspace");
        let work_root = temp.root().join("view");
        fs::create_dir(&work_root).expect("work root");
        fs::write(work_root.join("state"), b"new").expect("new tree");
        let backup = overlay_backup_root(&work_root, "cid_committed").expect("backup path");
        fs::create_dir(&backup).expect("backup root");
        fs::write(backup.join("state"), b"old").expect("old tree");

        recover_overlay_publish(&work_root, "cid_committed", true).expect("post-commit recovery");
        assert_eq!(
            fs::read(work_root.join("state")).expect("committed tree"),
            b"new"
        );
        assert!(!backup.exists());
    }
}
