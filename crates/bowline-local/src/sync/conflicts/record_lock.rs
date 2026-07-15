#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::{
    fs::{self, File, OpenOptions},
    io,
    path::Path,
    thread,
    time::{Duration, Instant},
};

use fs2::FileExt;

use super::{ConflictBundleError, STATUS_REVISION_FILE, set_owner_only};

const CONFLICT_RECORD_LOCK_TIMEOUT: Duration = Duration::from_secs(2);
const CONFLICT_RECORD_LOCK_POLL: Duration = Duration::from_millis(25);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConflictStatusRevision {
    exists: bool,
    len: u64,
    modified: Option<std::time::SystemTime>,
    identity: Option<(u64, u64)>,
}

pub(crate) fn conflict_status_revision(state_root: &Path) -> ConflictStatusRevision {
    let metadata = fs::metadata(state_root.join("conflicts").join(STATUS_REVISION_FILE)).ok();
    ConflictStatusRevision {
        exists: metadata.is_some(),
        len: metadata.as_ref().map_or(0, fs::Metadata::len),
        modified: metadata.as_ref().and_then(|value| value.modified().ok()),
        identity: conflict_revision_file_identity(metadata.as_ref()),
    }
}

#[cfg(unix)]
fn conflict_revision_file_identity(metadata: Option<&fs::Metadata>) -> Option<(u64, u64)> {
    metadata.map(|value| (value.dev(), value.ino()))
}

#[cfg(not(unix))]
fn conflict_revision_file_identity(metadata: Option<&fs::Metadata>) -> Option<(u64, u64)> {
    metadata.map(|value| (value.len(), 0))
}

pub(super) struct ConflictRecordLock {
    _file: File,
}

impl ConflictRecordLock {
    pub(super) fn acquire(
        conflicts_root: &Path,
        conflict_id: &str,
    ) -> Result<Self, ConflictBundleError> {
        let lock_root = conflicts_root.join(".locks");
        fs::create_dir_all(&lock_root)?;
        set_owner_only(&lock_root)?;
        let lock_path = lock_root.join(format!("{}.lock", lock_component(conflict_id)));
        let started = Instant::now();
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
                    if started.elapsed() >= CONFLICT_RECORD_LOCK_TIMEOUT {
                        return Err(ConflictBundleError::RecordLockTimeout {
                            conflict_id: conflict_id.to_string(),
                        });
                    }
                    thread::sleep(CONFLICT_RECORD_LOCK_POLL);
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
