use std::{fs, path::Path};

use bowline_core::git_worktree_link::{is_out_of_root_admin_target, worktree_registration_prefix};

use super::SyncRunnerError;

pub(super) fn preserves_local_out_of_root_worktree_registration(
    root: &Path,
    path: &str,
) -> Result<bool, SyncRunnerError> {
    let Some(prefix) = worktree_registration_prefix(path) else {
        return Ok(false);
    };
    let gitdir = root.join(prefix).join("gitdir");
    let bytes = match fs::read(gitdir) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(SyncRunnerError::StateIo(error)),
    };
    Ok(is_out_of_root_admin_target(&bytes, root))
}
