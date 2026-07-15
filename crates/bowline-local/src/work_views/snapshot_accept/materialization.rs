use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
};

use bowline_core::workspace_graph::{FileExecutability, NamespaceEntry, NamespaceEntryKind};

use crate::sync::{MergeContentReader, MergedNamespace};
use crate::work_views::content_identity::clone_file_at_start;
use crate::work_views::safe_materialization::SafeMaterializationRoot;

use super::{Reader, ReaderSource, is_owner_only_work_view_policy};

pub(super) fn apply_merged(
    staged: &Path,
    base: &Reader,
    work: &Reader,
    current: &Reader,
    merged: &MergedNamespace,
) -> io::Result<()> {
    let staged_root = SafeMaterializationRoot::new(staged)?;
    let by_path = merged
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect::<BTreeMap<_, _>>();
    let mut universe = base
        .entries
        .iter()
        .chain(&work.entries)
        .map(|entry| entry.path.as_str())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    universe.sort_by(|left, right| {
        right
            .split('/')
            .count()
            .cmp(&left.split('/').count())
            .then_with(|| left.cmp(right))
    });
    for path in universe {
        let relative = Path::new(path);
        let Some(entry) = by_path.get(path) else {
            let deleted_kind = [work, base, current]
                .into_iter()
                .flat_map(|reader| reader.entries.iter())
                .find(|entry| entry.path == path)
                .map(|entry| entry.kind);
            if deleted_kind == Some(NamespaceEntryKind::Directory) {
                staged_root.remove(relative, true)?;
            } else {
                staged_root.remove(relative, false)?;
            }
            continue;
        };
        match entry.kind {
            NamespaceEntryKind::Directory => {
                staged_root.create_dir(relative)?;
            }
            NamespaceEntryKind::File => {
                write_merged_file(MergedFileRequest {
                    staged: &staged_root,
                    relative,
                    path,
                    entry,
                    merged,
                    readers: [work, current, base],
                })?;
                apply_permissions(&staged_root, relative, entry)?;
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unsupported merged entry kind for `{path}`"),
                ));
            }
        }
    }
    Ok(())
}

struct MergedFileRequest<'a> {
    staged: &'a SafeMaterializationRoot<'a>,
    relative: &'a Path,
    path: &'a str,
    entry: &'a NamespaceEntry,
    merged: &'a MergedNamespace,
    readers: [&'a Reader; 3],
}

fn write_merged_file(request: MergedFileRequest<'_>) -> io::Result<()> {
    let mut target = request.staged.create_file(request.relative)?;
    if let Some(prepared) = request
        .entry
        .content_id
        .as_ref()
        .and_then(|id| request.merged.prepared_content.get(id))
    {
        let mut source = prepared.open()?;
        std::io::copy(&mut source, &mut target)?;
        target.sync_all()?;
        return Ok(());
    }
    for reader in request.readers {
        if reader
            .entries
            .iter()
            .find(|candidate| candidate.path == request.path)
            .and_then(|candidate| candidate.content_id.as_ref())
            == request.entry.content_id.as_ref()
        {
            return match reader.sources.get(request.path) {
                Some(ReaderSource::File(source)) => {
                    let mut source = clone_file_at_start(source)?;
                    std::io::copy(&mut source, &mut target)?;
                    target.sync_all()?;
                    Ok(())
                }
                None => {
                    let bytes = reader.read_file_for_path(request.path)?.ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::NotFound,
                            format!("missing merged source for `{}`", request.path),
                        )
                    })?;
                    use std::io::Write as _;
                    target.write_all(&bytes)?;
                    target.sync_all()
                }
            };
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("missing merged bytes for `{}`", request.path),
    ))
}

pub(super) fn copy_tree(source: &Path, destination: &Path) -> io::Result<()> {
    enum CopyStep {
        Visit {
            source: PathBuf,
            relative: PathBuf,
        },
        FinishDirectory {
            relative: PathBuf,
            permissions: fs::Permissions,
        },
    }
    let destination = SafeMaterializationRoot::new(destination)?;
    let mut children = fs::read_dir(source)?.collect::<Result<Vec<_>, _>>()?;
    children.sort_by_key(fs::DirEntry::file_name);
    let mut pending = children
        .into_iter()
        .rev()
        .map(|child| CopyStep::Visit {
            source: child.path(),
            relative: PathBuf::from(child.file_name()),
        })
        .collect::<Vec<_>>();
    let mut visited = 0_usize;
    while let Some(step) = pending.pop() {
        visited = visited.saturating_add(1);
        if visited > 1_000_000 {
            return Err(io::Error::other("accept staging exceeds the entry budget"));
        }
        match step {
            CopyStep::FinishDirectory {
                relative,
                permissions,
            } => {
                destination.set_permissions(&relative, permissions)?;
            }
            CopyStep::Visit { source, relative } => {
                if relative.components().count() > 4_096 {
                    return Err(io::Error::other("accept staging exceeds the depth budget"));
                }
                let metadata = fs::symlink_metadata(&source)?;
                if metadata.is_dir() {
                    destination.create_dir(&relative)?;
                    let mut children = fs::read_dir(&source)?.collect::<Result<Vec<_>, _>>()?;
                    children.sort_by_key(fs::DirEntry::file_name);
                    pending.push(CopyStep::FinishDirectory {
                        relative: relative.clone(),
                        permissions: metadata.permissions(),
                    });
                    pending.extend(children.into_iter().rev().map(|child| CopyStep::Visit {
                        source: child.path(),
                        relative: relative.join(child.file_name()),
                    }));
                } else if metadata.is_file() {
                    let mut source = fs::File::open(source)?;
                    let mut target = destination.create_file(&relative)?;
                    std::io::copy(&mut source, &mut target)?;
                    target.sync_all()?;
                    destination.set_permissions(&relative, metadata.permissions())?;
                } else if metadata.file_type().is_symlink() {
                    copy_symlink(&source, &destination, &relative)?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn copy_symlink(
    source: &Path,
    destination: &SafeMaterializationRoot<'_>,
    relative: &Path,
) -> io::Result<()> {
    destination.create_symlink(relative, &fs::read_link(source)?)
}

#[cfg(not(unix))]
fn copy_symlink(_: &Path, _: &SafeMaterializationRoot<'_>, _: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "symlink staging is unsupported",
    ))
}

pub(crate) fn tree_fence(root: &Path) -> io::Result<String> {
    let mut hasher = blake3::Hasher::new();
    hash_tree(root, &mut hasher)?;
    Ok(format!("b3_{}", hasher.finalize().to_hex()))
}

fn hash_tree(root: &Path, hasher: &mut blake3::Hasher) -> io::Result<()> {
    let mut children = fs::read_dir(root)?.collect::<Result<Vec<_>, _>>()?;
    children.sort_by_key(fs::DirEntry::file_name);
    let mut pending = children
        .into_iter()
        .rev()
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    let mut visited = 0_usize;
    while let Some(path) = pending.pop() {
        visited = visited.saturating_add(1);
        if visited > 1_000_000 {
            return Err(io::Error::other("main fence exceeds the entry budget"));
        }
        let relative = path.strip_prefix(root).map_err(io::Error::other)?;
        if relative.components().count() > 4_096 {
            return Err(io::Error::other("main fence exceeds the depth budget"));
        }
        hasher.update(relative.as_os_str().as_encoded_bytes());
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            hasher.update(b"d");
            hash_permissions(&metadata, hasher);
            let mut children = fs::read_dir(&path)?.collect::<Result<Vec<_>, _>>()?;
            children.sort_by_key(fs::DirEntry::file_name);
            pending.extend(children.into_iter().rev().map(|entry| entry.path()));
        } else if metadata.is_file() {
            hasher.update(b"f");
            hash_permissions(&metadata, hasher);
            let mut source = fs::File::open(path)?;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let read = std::io::Read::read(&mut source, &mut buffer)?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
        } else if metadata.file_type().is_symlink() {
            hasher.update(b"l");
            hasher.update(fs::read_link(path)?.as_os_str().as_encoded_bytes());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn hash_permissions(metadata: &fs::Metadata, hasher: &mut blake3::Hasher) {
    use std::os::unix::fs::PermissionsExt as _;

    hasher.update(&(metadata.permissions().mode() & 0o7777).to_le_bytes());
}

#[cfg(not(unix))]
fn hash_permissions(metadata: &fs::Metadata, hasher: &mut blake3::Hasher) {
    hasher.update(&[u8::from(metadata.permissions().readonly())]);
}

pub(super) fn project_relative<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    if prefix.is_empty() {
        Some(path.trim_matches('/'))
    } else {
        path.strip_prefix(prefix)?.strip_prefix('/')
    }
}

#[cfg(unix)]
pub(super) fn executability(metadata: &fs::Metadata) -> FileExecutability {
    use std::os::unix::fs::PermissionsExt as _;
    if metadata.permissions().mode() & 0o111 == 0 {
        FileExecutability::Regular
    } else {
        FileExecutability::Executable
    }
}

#[cfg(not(unix))]
pub(super) fn executability(_: &fs::Metadata) -> FileExecutability {
    FileExecutability::Regular
}

#[cfg(unix)]
fn apply_permissions(
    staged: &SafeMaterializationRoot<'_>,
    relative: &Path,
    entry: &NamespaceEntry,
) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = if is_owner_only_work_view_policy(entry.classification, entry.mode) {
        0o600
    } else if entry.executability == FileExecutability::Executable {
        0o755
    } else {
        0o644
    };
    staged.set_permissions(relative, fs::Permissions::from_mode(mode))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::workspace::TempWorkspace;
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    #[test]
    fn copy_tree_preserves_symlink_leaf_without_traversing_target() {
        let temp = TempWorkspace::new("snapshot-copy-symlink-leaf").expect("temp");
        let source = temp.root().join("main");
        let staged = temp.root().join("staged");
        let outside = temp.root().join("outside");
        fs::create_dir_all(&source).expect("source");
        fs::create_dir_all(&staged).expect("staged");
        fs::create_dir_all(&outside).expect("outside");
        fs::write(outside.join("sentinel"), "outside").expect("sentinel");
        symlink(&outside, source.join("legitimate-link")).expect("link");

        copy_tree(&source, &staged).expect("copy");

        assert_eq!(
            fs::read_link(staged.join("legitimate-link")).unwrap(),
            outside
        );
        assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"outside");
    }

    #[test]
    fn copy_tree_preserves_file_and_directory_modes() {
        let temp = TempWorkspace::new("snapshot-copy-tree-modes").expect("temp");
        let source = temp.root().join("main");
        let staged = temp.root().join("staged");
        let tools = source.join("tools");
        fs::create_dir_all(&tools).expect("source");
        fs::create_dir_all(&staged).expect("staged");
        fs::write(tools.join("run"), "#!/bin/sh\n").expect("executable");
        fs::write(source.join("secret"), "owner-only\n").expect("secret");
        fs::set_permissions(&tools, fs::Permissions::from_mode(0o750)).expect("directory mode");
        fs::set_permissions(tools.join("run"), fs::Permissions::from_mode(0o751))
            .expect("executable mode");
        fs::set_permissions(source.join("secret"), fs::Permissions::from_mode(0o600))
            .expect("secret mode");

        copy_tree(&source, &staged).expect("copy");

        assert_eq!(mode(&staged.join("tools")), 0o750);
        assert_eq!(mode(&staged.join("tools/run")), 0o751);
        assert_eq!(mode(&staged.join("secret")), 0o600);
    }

    #[test]
    fn tree_fence_includes_file_mode() {
        let temp = TempWorkspace::new("snapshot-tree-fence-mode").expect("temp");
        let root = temp.root().join("tree");
        fs::create_dir_all(&root).expect("tree");
        let file = root.join("run");
        fs::write(&file, "same bytes\n").expect("file");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).expect("regular mode");
        let regular = tree_fence(&root).expect("regular fence");

        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).expect("executable mode");

        assert_ne!(tree_fence(&root).expect("executable fence"), regular);
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).expect("metadata").permissions().mode() & 0o777
    }
}

#[cfg(not(unix))]
fn apply_permissions(
    _: &SafeMaterializationRoot<'_>,
    _: &Path,
    _: &NamespaceEntry,
) -> io::Result<()> {
    Ok(())
}
