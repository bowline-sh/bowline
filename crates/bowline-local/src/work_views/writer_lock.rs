use std::{
    fs::{self, File, OpenOptions},
    io,
    path::Path,
    thread,
    time::{Duration, SystemTime},
};

use bowline_core::ids::{ProjectId, WorkspaceId};
use fs2::FileExt;

use super::WorkViewError;

const WRITER_LOCK_TIMEOUT: Duration = Duration::from_millis(2000);
const WRITER_LOCK_POLL: Duration = Duration::from_millis(25);

#[derive(Debug)]
pub(super) struct ProjectWriterLock {
    _file: File,
}

impl ProjectWriterLock {
    pub(super) fn acquire(
        namespace_root: &Path,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
        project_path: &str,
    ) -> Result<Self, WorkViewError> {
        Self::acquire_with_timeout(
            namespace_root,
            workspace_id,
            project_id,
            project_path,
            WRITER_LOCK_TIMEOUT,
        )
    }

    pub(super) fn acquire_with_timeout(
        namespace_root: &Path,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
        project_path: &str,
        timeout: Duration,
    ) -> Result<Self, WorkViewError> {
        let lock_dir = namespace_root.join(".writer-locks");
        fs::create_dir_all(&lock_dir)?;
        let lock_path = lock_dir.join(format!(
            "{}--{}.lock",
            lock_component(workspace_id.as_str()),
            lock_component(project_id.as_str())
        ));
        let started = SystemTime::now();
        loop {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&lock_path)?;
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    let elapsed = started.elapsed().map_err(io::Error::other)?;
                    if elapsed >= timeout {
                        return Err(WorkViewError::ProjectWriterBusy {
                            project_path: project_path.to_string(),
                            reason: format!(
                                "project writer lock is busy at {}",
                                lock_path.display()
                            ),
                        });
                    }
                    thread::sleep(WRITER_LOCK_POLL);
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}

fn lock_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}
