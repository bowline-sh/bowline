use std::{
    fs, io,
    path::{Path, PathBuf},
};

use bowline_core::{
    fs_atomic::{AtomicWriteOptions, write_atomic},
    ids::WorkViewId,
};
use serde::{Deserialize, Serialize};

use super::snapshot_accept::tree_fence;

const CREATION_CHECKPOINT_VERSION: u32 = 1;

#[derive(Debug, Deserialize, Serialize)]
struct CreationPublishCheckpoint {
    format_version: u32,
    work_view_id: WorkViewId,
    tree_fence: String,
}

pub(super) fn checkpoint_path(staging_path: &Path) -> io::Result<PathBuf> {
    let name = staging_path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "work-view creation staging path has no file name",
        )
    })?;
    Ok(staging_path.with_file_name(format!("{}.checkpoint", name.to_string_lossy())))
}

pub(super) fn write(
    checkpoint_path: &Path,
    work_view_id: &WorkViewId,
    staged_tree: &Path,
) -> io::Result<()> {
    let checkpoint = CreationPublishCheckpoint {
        format_version: CREATION_CHECKPOINT_VERSION,
        work_view_id: work_view_id.clone(),
        tree_fence: tree_fence(staged_tree)?,
    };
    let bytes = serde_json::to_vec(&checkpoint).map_err(io::Error::other)?;
    write_atomic(
        checkpoint_path,
        &bytes,
        AtomicWriteOptions {
            unix_mode: Some(0o600),
            reject_symlink: true,
            replace_existing: true,
        },
    )
}

pub(super) fn verify(
    checkpoint_path: &Path,
    work_view_id: &WorkViewId,
    visible_tree: &Path,
) -> io::Result<bool> {
    let bytes = read_no_follow(checkpoint_path)?;
    let checkpoint: CreationPublishCheckpoint =
        serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    Ok(checkpoint.format_version == CREATION_CHECKPOINT_VERSION
        && checkpoint.work_view_id == *work_view_id
        && checkpoint.tree_fence == tree_fence(visible_tree)?)
}

pub(super) fn remove(checkpoint_path: &Path) -> io::Result<()> {
    match fs::remove_file(checkpoint_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn read_no_follow(path: &Path) -> io::Result<Vec<u8>> {
    use std::io::Read as _;

    let descriptor = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .map_err(io::Error::from)?;
    let mut file = fs::File::from(descriptor);
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(not(unix))]
fn read_no_follow(path: &Path) -> io::Result<Vec<u8>> {
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "work-view creation checkpoint is a symlink",
        ));
    }
    fs::read(path)
}
