//! Filesystem materialization primitives for pull/apply (Plan 109 Step 5).
//!
//! Split from `apply.rs` at the domain seam between the apply *transaction* —
//! which decides which op to run, journals its intent, and drives the loop —
//! and the primitives here that *execute* one op against the workspace tree:
//! staging temps, installing entries, checked deletes, conflict asides, and
//! preimage preservation. Every primitive goes through the no-follow
//! [`super::super::fs_guard`] boundary and none of them read the intent journal
//! or the merge plan; the orchestrator in `apply.rs` composes them.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use bowline_core::ids::ContentId;

use super::naming::{free_aside_path, materialized_aside_path, quarantine_leaf, temp_name};
use super::{FsOp, FsOpKind, PullError, entry_mode, record_for_entry};
use crate::sync::manifest_engine::fs_guard::{
    ExpectedFile, FileRead, Observed, ParentChain, ParentChainMode, observe, prepare_parent_chain,
    read_file_bounded, write_private_file,
};
use crate::sync::manifest_engine::manifest::{
    BlobKey, EntryKind, FileMode, ManifestEntry, WorkspacePath, open_file, physical_blob_key,
};
use crate::sync::manifest_engine::push::{EngineContext, RemoteObjects};
use crate::sync::manifest_engine::store::FileRecord;

/// A prepared 0600 temp file holding the exact bytes to install.
pub struct TempFile {
    pub path: PathBuf,
    pub name: String,
}

/// The outcome of one filesystem materialization attempt. `ParentBlocked` is not
/// an error — it is the type-conflict divergence the caller maps to keep-local,
/// exactly as the merge matrix resolves a local symlink vs. a remote dir-tree.
pub(crate) enum Materialized<T> {
    Done(T),
    ParentBlocked,
}

/// Create the final directory component after its parent chain is verified.
/// Single-component `create_dir` (never `create_dir_all`, which would recreate
/// and follow parents); an existing real directory is accepted, and a symlink or
/// file already at the final name is left untouched so it is never written
/// through — the manifest's children of it are blocked on their own descent.
fn create_dir_no_follow(absolute: &Path) -> Result<(), PullError> {
    match fs::create_dir(absolute) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(PullError::Io(error)),
    }
}

pub(crate) fn stage_write_temp<O: RemoteObjects>(
    ctx: &EngineContext,
    objects: &O,
    op: &FsOp,
) -> Result<Option<TempFile>, PullError> {
    let entry = match &op.kind {
        FsOpKind::Install(entry) | FsOpKind::ConflictAside(entry) => entry,
        _ => return Ok(None),
    };
    let ManifestEntry::File {
        content_id,
        blob_key,
        ..
    } = entry
    else {
        return Ok(None); // directories and symlinks carry no temp content
    };
    let plaintext = download_file(ctx, objects, content_id, blob_key)?;
    let name = temp_name(&op.path, blob_key);
    let temp_dir = ctx.engine_dir().join("tmp");
    fs::create_dir_all(&temp_dir).map_err(PullError::Io)?;
    let path = temp_dir.join(&name);
    write_private_file(&path, &plaintext).map_err(PullError::Io)?;
    Ok(Some(TempFile { path, name }))
}

pub(crate) fn download_file<O: RemoteObjects>(
    ctx: &EngineContext,
    objects: &O,
    content_id: &ContentId,
    blob_key: &BlobKey,
) -> Result<Vec<u8>, PullError> {
    let sealed = objects.get_blob(blob_key).map_err(PullError::Transport)?;
    if &physical_blob_key(&sealed) != blob_key {
        return Err(PullError::BlobKeyMismatch);
    }
    open_file(&ctx.crypto, content_id, &sealed).map_err(PullError::Manifest)
}

/// Atomic no-replace install (create) or checked replace (preserve preimage to
/// quarantine, then atomic rename). Directories and symlinks are recreated;
/// symlinks are never followed.
pub(crate) fn install_entry<O: RemoteObjects>(
    ctx: &EngineContext,
    _objects: &O,
    path: &WorkspacePath,
    entry: &ManifestEntry,
    temp: Option<TempFile>,
    preimage: Option<&Observed>,
) -> Result<Materialized<FileRecord>, PullError> {
    // Verify the parent chain before consuming the temp or touching disk: a
    // symlinked intermediate must never be written through (workspace escape).
    if let ParentChain::Blocked =
        prepare_parent_chain(&ctx.workspace_root, path, ParentChainMode::CreateMissing)?
    {
        return Ok(Materialized::ParentBlocked);
    }
    let absolute = ctx.workspace_root.join(path.as_str());

    // Quarantine the bytes being displaced (kill-9 rollback asset) and, when the
    // remote entry replaces a DIFFERENT-kind local entry (file↔directory↔symlink),
    // clear that target first: a cross-kind install cannot overwrite in place — a
    // rename onto a directory, a create_dir over a file, or a symlink over a
    // directory all fail. A file/symlink preimage unlinks; a directory preimage is
    // removed only when empty (the shared "never destroy local-only content"
    // rule), so a populated one keeps local and the caller asides the remote.
    if let Some(observed) = preimage {
        preserve_preimage(ctx, path, &absolute)?;
        if observed.kind != entry.kind() {
            match observed.kind {
                EntryKind::Directory => {
                    if let DirRemoval::LocalContent = remove_empty_dir(&absolute)? {
                        return Ok(Materialized::ParentBlocked);
                    }
                }
                EntryKind::File | EntryKind::Symlink => {
                    fs::remove_file(&absolute).map_err(PullError::Io)?;
                }
            }
        }
    }

    match entry {
        ManifestEntry::File { .. } => {
            let temp = temp.ok_or(PullError::Internal {
                reason: "file install without temp",
            })?;
            // Stamp the manifest entry's mode onto the 0600 staging temp BEFORE
            // the atomic rename, so the installed file carries the intended mode
            // rather than 0600. Without this the next pull re-observes a spurious
            // mode change and conflict-asides a file it should have installed.
            chmod_temp(&temp.path, entry_mode(entry))?;
            fs::rename(&temp.path, &absolute).map_err(PullError::Io)?;
        }
        ManifestEntry::Directory { mode } => {
            create_dir_no_follow(&absolute)?;
            set_mode(&ctx.workspace_root, path, *mode)?;
        }
        ManifestEntry::Symlink { target, .. } => {
            // A same-kind symlink replace still needs the old link gone before
            // symlink() (EEXIST otherwise); a different-kind target was cleared
            // above, and a fresh install has nothing to remove.
            let _ = fs::remove_file(&absolute);
            symlink(target, &absolute).map_err(PullError::Io)?;
        }
    }
    fsync_parent(&absolute)?;
    let observed = observe(&ctx.workspace_root, path)
        .map_err(PullError::Io)?
        .ok_or(PullError::Internal {
            reason: "installed target vanished",
        })?;
    Ok(Materialized::Done(record_for_entry(
        entry,
        observed.fingerprint,
    )))
}

/// The outcome of a checked delete: the target was removed, or it was kept local
/// (a symlinked parent, or a directory still holding local-only content).
pub(crate) enum DeleteOutcome {
    Deleted,
    KeptLocal,
}

/// Whether a non-recursive directory remove succeeded. The single owner of the
/// "never destroy local-only content inside a directory" rule shared by delete
/// (recursive-delete data loss) and kind replacement (a directory replaced by a
/// file/symlink). `remove_dir` unlinks only an EMPTY directory; a populated one
/// holds descendants the manifest does not delete — untracked local work or a
/// racing edit to a tracked child — that must survive.
pub(crate) enum DirRemoval {
    Removed,
    LocalContent,
}

pub(crate) fn remove_empty_dir(absolute: &Path) -> Result<DirRemoval, PullError> {
    match fs::remove_dir(absolute) {
        Ok(()) => Ok(DirRemoval::Removed),
        Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => {
            Ok(DirRemoval::LocalContent)
        }
        Err(error) => Err(PullError::Io(error)),
    }
}

/// Delete only a fingerprint-clean target (verified by the caller's preimage
/// re-observation). The displaced bytes are preserved to quarantine first. A
/// directory is removed NON-recursively and only when empty: tracked children
/// were already unlinked (bottom-up apply order), so any surviving entry is
/// local-only work that must never be destroyed — keep the directory local (it
/// resurrects via push_again like any keep-local divergence).
pub(crate) fn checked_delete(
    ctx: &EngineContext,
    path: &WorkspacePath,
) -> Result<DeleteOutcome, PullError> {
    // Refuse to remove through a symlinked intermediate: a delete that descended
    // a symlink would unlink a file OUTSIDE the workspace root. Verify (never
    // create) the chain before reading or unlinking anything.
    if let ParentChain::Blocked =
        prepare_parent_chain(&ctx.workspace_root, path, ParentChainMode::RequireExisting)?
    {
        return Ok(DeleteOutcome::KeptLocal);
    }
    let absolute = ctx.workspace_root.join(path.as_str());
    preserve_preimage(ctx, path, &absolute)?;
    match fs::symlink_metadata(&absolute) {
        Ok(metadata) if metadata.is_dir() => {
            if let DirRemoval::LocalContent = remove_empty_dir(&absolute)? {
                return Ok(DeleteOutcome::KeptLocal);
            }
        }
        Ok(_) => fs::remove_file(&absolute).map_err(PullError::Io)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(PullError::Io(error)),
    }
    fsync_parent(&absolute)?;
    Ok(DeleteOutcome::Deleted)
}

/// No-replace create of the remote bytes under a deterministic aside name
/// (`<path> (conflict from <content-prefix>)`, see `naming::materialized_aside_path`),
/// appending a collision suffix if that name is taken. Never touches the
/// original path.
pub(crate) fn materialize_aside<O: RemoteObjects>(
    ctx: &EngineContext,
    objects: &O,
    path: &WorkspacePath,
    entry: &ManifestEntry,
    temp: Option<TempFile>,
) -> Result<Materialized<WorkspacePath>, PullError> {
    let aside = free_aside_path(ctx, path, entry);
    // The aside shares the incoming entry's parent; if that parent is a symlink
    // there is nowhere safe to place the aside without escaping the root, so keep
    // local and surface the path as a divergence (the caller maps ParentBlocked).
    if let ParentChain::Blocked =
        prepare_parent_chain(&ctx.workspace_root, &aside, ParentChainMode::CreateMissing)?
    {
        return Ok(Materialized::ParentBlocked);
    }
    let absolute = ctx.workspace_root.join(aside.as_str());
    match entry {
        ManifestEntry::File {
            content_id,
            blob_key,
            ..
        } => {
            let bytes = match temp {
                Some(temp) => fs::read(&temp.path).map_err(PullError::Io)?,
                None => download_file(ctx, objects, content_id, blob_key)?,
            };
            create_no_replace(&absolute, &bytes)?;
        }
        ManifestEntry::Directory { .. } => {
            create_dir_no_follow(&absolute)?;
        }
        ManifestEntry::Symlink { target, .. } => {
            symlink(target, &absolute).map_err(PullError::Io)?;
        }
    }
    fsync_parent(&absolute)?;
    Ok(Materialized::Done(aside))
}

/// Whether an aside carrying `entry`'s content already sits on disk for `path`.
/// Aside names are content-derived and `free_aside_path` fills the first free
/// slot, so existing asides occupy a gapless run from the base name; scanning it
/// for a content match makes conflict-aside recovery idempotent across crashes.
pub(crate) fn aside_already_materialized(
    ctx: &EngineContext,
    path: &WorkspacePath,
    entry: &ManifestEntry,
) -> Result<bool, PullError> {
    let base = materialized_aside_path(path, entry);
    if aside_content_matches(ctx, &base, entry)? {
        return Ok(true);
    }
    for suffix in 1..u32::MAX {
        let candidate = WorkspacePath::new(format!("{} ({suffix})", base.as_str()));
        if !ctx.workspace_root.join(candidate.as_str()).exists() {
            break; // first free slot: nothing materialized beyond here
        }
        if aside_content_matches(ctx, &candidate, entry)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn aside_content_matches(
    ctx: &EngineContext,
    candidate: &WorkspacePath,
    entry: &ManifestEntry,
) -> Result<bool, PullError> {
    let absolute = ctx.workspace_root.join(candidate.as_str());
    let Ok(metadata) = fs::symlink_metadata(&absolute) else {
        return Ok(false);
    };
    match entry {
        ManifestEntry::File { content_id, .. } => {
            if !metadata.is_file() {
                return Ok(false);
            }
            // Read no-follow, validated against the fingerprint just stat'd: an
            // aside slot raced into a symlink must never be read through.
            match read_file_bounded(
                &ctx.workspace_root,
                candidate,
                ctx.config.max_seal_bytes,
                &ExpectedFile::from_metadata(&metadata),
            )
            .map_err(PullError::Push)?
            {
                FileRead::Bytes(bytes) => Ok(ctx.crypto.content_id(&bytes) == *content_id),
                FileRead::Diverged => Ok(false),
            }
        }
        ManifestEntry::Directory { .. } => Ok(metadata.is_dir()),
        ManifestEntry::Symlink { target, .. } => Ok(metadata.file_type().is_symlink()
            && fs::read_link(&absolute)
                .ok()
                .and_then(|link| link.to_str().map(str::to_string))
                .as_deref()
                == Some(target.as_str())),
    }
}

pub(crate) fn create_no_replace(absolute: &Path, bytes: &[u8]) -> Result<(), PullError> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true).mode(0o644);
    let mut file = options.open(absolute).map_err(PullError::Io)?;
    file.write_all(bytes).map_err(PullError::Io)?;
    file.sync_all().map_err(PullError::Io)?;
    Ok(())
}

pub(crate) fn preserve_preimage(
    ctx: &EngineContext,
    path: &WorkspacePath,
    absolute: &Path,
) -> Result<(), PullError> {
    if !absolute.exists() {
        return Ok(());
    }
    let dir = ctx.engine_dir().join("quarantine");
    fs::create_dir_all(&dir).map_err(PullError::Io)?;
    let target = dir.join(quarantine_leaf(path));
    if let Ok(metadata) = fs::symlink_metadata(absolute)
        && metadata.is_file()
    {
        let bytes = fs::read(absolute).map_err(PullError::Io)?;
        write_private_file(&target, &bytes).map_err(PullError::Io)?;
    }
    Ok(())
}

pub(crate) fn reinstall_from_download<O: RemoteObjects>(
    ctx: &EngineContext,
    objects: &O,
    path: &WorkspacePath,
    entry: &ManifestEntry,
) -> Result<Materialized<FileRecord>, PullError> {
    if let ManifestEntry::File {
        content_id,
        blob_key,
        ..
    } = entry
    {
        if let ParentChain::Blocked =
            prepare_parent_chain(&ctx.workspace_root, path, ParentChainMode::CreateMissing)?
        {
            return Ok(Materialized::ParentBlocked);
        }
        let bytes = download_file(ctx, objects, content_id, blob_key)?;
        let absolute = ctx.workspace_root.join(path.as_str());
        // Preserve the on-disk preimage before overwriting, so a reinstall (no
        // temp survived) still leaves a rollback asset like the rename path does.
        preserve_preimage(ctx, path, &absolute)?;
        write_private_file(&absolute, &bytes).map_err(PullError::Io)?;
        set_mode(&ctx.workspace_root, path, entry_mode(entry))?;
        fsync_parent(&absolute)?;
    }
    let observed = observe(&ctx.workspace_root, path)
        .map_err(PullError::Io)?
        .ok_or(PullError::Internal {
            reason: "reinstall target vanished",
        })?;
    Ok(Materialized::Done(record_for_entry(
        entry,
        observed.fingerprint,
    )))
}

// ---- low-level filesystem helpers -------------------------------------------

pub(crate) fn fsync_parent(absolute: &Path) -> Result<(), PullError> {
    if let Some(parent) = absolute.parent()
        && let Ok(dir) = fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Fchmod a staging temp to `mode` before it is atomically renamed into place.
/// Operates on the file object (fchmod) rather than re-resolving the path, so a
/// racing swap of the temp cannot redirect the chmod.
pub(crate) fn chmod_temp(temp: &Path, mode: FileMode) -> Result<(), PullError> {
    let file = fs::File::open(temp).map_err(PullError::Io)?;
    file.set_permissions(fs::Permissions::from_mode(mode.get()))
        .map_err(PullError::Io)?;
    Ok(())
}

pub(crate) fn set_mode(root: &Path, path: &WorkspacePath, mode: FileMode) -> Result<(), PullError> {
    use rustix::fs::{Mode, OFlags};
    use rustix::io::Errno;

    let absolute = root.join(path.as_str());
    // Open the leaf itself and chmod the DESCRIPTOR, never re-resolving the path:
    // a `symlink_metadata` check followed by a path-based `set_permissions` leaves
    // a TOCTOU window in which the leaf can be swapped for a symlink and the chmod
    // then follows it onto a target OUTSIDE the workspace. `O_NOFOLLOW` refuses to
    // open a symlink leaf (ELOOP), and `fchmod` on the held fd cannot re-resolve.
    // `O_NONBLOCK` so a leaf raced into a FIFO opens immediately rather than
    // blocking on a writer.
    let fd = match rustix::fs::open(
        &absolute,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        // ELOOP: the leaf is (now) a symlink — skip; symlink modes are not portably
        // settable and are never followed. NOENT: it vanished. NOTDIR: an
        // intermediate raced into a non-directory. All are keep-local divergences
        // that the follow-on pull re-derives, never a fatal or a followed chmod.
        Err(Errno::LOOP | Errno::NOENT | Errno::NOTDIR) => return Ok(()),
        Err(errno) => return Err(PullError::Io(io::Error::from(errno))),
    };
    // fchmod the descriptor we hold — no path re-resolution — so the object chmod'd
    // is exactly the leaf opened no-follow.
    fs::File::from(fd)
        .set_permissions(fs::Permissions::from_mode(mode.get()))
        .map_err(PullError::Io)
}
