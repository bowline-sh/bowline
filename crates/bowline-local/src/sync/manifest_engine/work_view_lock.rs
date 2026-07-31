use std::fs::{self, File};
use std::io;
use std::path::Path;

use fs2::FileExt;
use rustix::fs::{Mode, OFlags};

use super::fs_guard::{ParentChain, ParentChainMode, prepare_parent_chain};
use super::manifest::WorkspacePath;

const WORK_VIEW_TRANSITION_LOCK_PATH: &str = ".bowline/work-view-transitions.lock";

/// Cross-process authority for the materialized aux index and its metadata
/// projection. Both CLI transitions and manifest apply hold this same inode.
pub(crate) fn acquire_work_view_transition_lock(root: &Path) -> io::Result<File> {
    let path = WorkspacePath::new(WORK_VIEW_TRANSITION_LOCK_PATH);
    if let ParentChain::Blocked = prepare_parent_chain(root, &path, ParentChainMode::CreateMissing)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "work-view transition lock path is blocked",
        ));
    }
    let fd = rustix::fs::open(
        root.join(path.as_str()),
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_bits_truncate(0o600),
    )
    .map_err(io::Error::from)?;
    let file = fs::File::from(fd);
    file.lock_exclusive()?;
    Ok(file)
}
