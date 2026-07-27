//! Mutations anchored to a held directory descriptor.
//!
//! [`super::prepare_parent_chain`] answers a question about a *path*: are these
//! components real directories right now? That answer expires the moment it is
//! returned. A caller that then renames or unlinks BY PATH makes the kernel
//! re-resolve every one of those components, so a parent swapped for a symlink
//! in between moves the mutation outside the workspace root — and Bowline's own
//! daemon is materializing into that very tree while `bowline resolve` runs, so
//! the window is expected traffic, not a hypothetical attacker.
//!
//! This module closes it the same way [`super::read_file_bounded`] closes it on
//! the read side: open the containing directory component by component with
//! `O_NOFOLLOW | O_DIRECTORY`, keep the descriptor, and perform the operation
//! with `renameat` / `unlinkat` against that descriptor. The check and the
//! operation then reference the same directory handle rather than the same
//! string, and nothing swapped in afterwards can redirect them.
//!
//! Traversal belongs here for the same reason mutation does. A walk that
//! re-derives `root.join(relative)` for each recursive `read_dir` re-resolves
//! every component of that path, so a directory it already checked and then
//! replaced with a symlink leads the walk out of the workspace — and a read-side
//! escape leaks the names it finds out there to whoever asked. Descending
//! through [`AnchoredDirectory::open_directory`] keeps the whole walk inside the
//! subtree the root descriptor refers to.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags};
use rustix::io::Errno;

use super::super::manifest::WorkspacePath;

/// How deep an anchored descent goes before refusing — [`AnchoredDirectory::remove_tree`]
/// and any caller walking with [`AnchoredDirectory::open_directory`].
///
/// Each level holds one open descriptor for the whole of its own subtree, so the
/// bound is what stops a pathological (or hostile) nesting from exhausting the
/// process's descriptors half way through. Real trees, `node_modules` included,
/// sit far below it.
pub const MAX_ANCHORED_DEPTH: u32 = 64;

/// Flags for every directory BELOW the root this module opens: no-follow so a
/// symlinked component fails `ELOOP` instead of being traversed, and
/// directory-only so a component raced into a file fails `ENOTDIR` instead of
/// opening.
const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// Flags for the workspace root itself. Identical but for `O_NOFOLLOW`: the root
/// is the trust anchor the caller named, and a `~/Code` that is a symlink to an
/// external volume is an ordinary setup that the sentinel
/// ([`super::super::workspace_root::classify_root_directory`]) already resolves
/// by following. The no-follow rule governs traversal INSIDE the root.
const ROOT_DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::CLOEXEC);

/// One path component: a name *inside* a directory, never a path.
///
/// The newtype is the guard. An operation anchored to a held descriptor that
/// accepted `a/b` would make the kernel resolve `a` again, which is exactly the
/// re-resolution the anchoring exists to prevent. It holds a [`CString`] because
/// the names it removes come off the filesystem, where they need not be UTF-8 —
/// a name this type could not represent would be a name a tree removal silently
/// skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeafName(CString);

impl LeafName {
    /// The final component of a workspace-relative path, or `None` when it has
    /// none (empty, or ending in `/`) or contains an interior NUL.
    pub fn of(path: &WorkspacePath) -> Option<Self> {
        let leaf = path.as_str().rsplit('/').next()?;
        if leaf.is_empty() {
            return None;
        }
        CString::new(leaf).ok().map(Self)
    }

    fn from_directory_entry(name: &std::ffi::CStr) -> Option<Self> {
        // `.` and `..` are the directory itself and its parent; acting on either
        // would leave the held descriptor's subtree.
        if matches!(name.to_bytes(), b"." | b"..") {
            return None;
        }
        Some(Self(name.to_owned()))
    }

    /// The name as text, or `None` when the filesystem holds bytes that are not
    /// UTF-8. A name Bowline cannot spell is a name it can never sync, so a
    /// caller that needs the text treats `None` as "skip this entry".
    pub fn as_str(&self) -> Option<&str> {
        self.0.to_str().ok()
    }

    fn as_c_str(&self) -> &std::ffi::CStr {
        &self.0
    }
}

/// One entry of an anchored listing, classified and sized through the very
/// descriptor it was listed from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchoredEntry {
    pub name: LeafName,
    pub kind: AnchoredLeafKind,
    /// The entry's own byte length. `None` for a directory, whose length is a
    /// filesystem bookkeeping artefact rather than content.
    pub byte_len: Option<u64>,
}

/// What sits at an anchored leaf, classified through the held descriptor
/// (`fstatat` with `AT_SYMLINK_NOFOLLOW`) rather than by re-resolving the path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchoredLeafKind {
    /// Nothing is there.
    Absent,
    /// A real directory — removal must empty it and use `AT_REMOVEDIR`.
    Directory,
    /// Anything else: a regular file, a symlink, a fifo, a device. All are
    /// removed by unlinking the name; a symlink's target is never touched.
    NonDirectory,
}

/// The outcome of opening a workspace path's containing directory.
pub enum AnchoredOpen {
    /// The directory is held open; every component from the root was a real
    /// directory at the moment it was opened, and remains the object the
    /// descriptor refers to no matter what is swapped in behind it.
    Ready(AnchoredDirectory),
    /// A component of the chain does not exist, so neither can the leaf inside
    /// it. Distinct from [`AnchoredOpen::Blocked`] because "nothing to act on"
    /// is an ordinary answer — an aside already reconciled, a path mistyped —
    /// while a blocked chain is a refusal the caller must report as one.
    Absent,
    /// The chain could not be opened: an intermediate component is a symlink or
    /// a file, or the filesystem refused it. Mirrors
    /// [`super::ParentChain::Blocked`] — the caller must refuse rather than
    /// mutate through it.
    Blocked,
}

/// A workspace directory held open by descriptor. Every mutation below names a
/// leaf inside it and runs as an `*at` syscall against the descriptor.
pub struct AnchoredDirectory {
    directory: OwnedFd,
}

/// Open the directory that CONTAINS `path`, walking every component from `root`
/// no-follow and keeping the descriptor.
///
/// `root` itself is resolved by path: it is the workspace the caller named, and
/// a user whose `~/Code` is a symlink still has one workspace. Everything below
/// it is opened relative to the descriptor above it, so no component is ever
/// resolved twice.
///
/// Missing components are never created. An anchored mutation acts on something
/// that is already there; a caller that needs a chain built uses
/// [`super::prepare_parent_chain`] with `CreateMissing` first.
pub fn open_containing_directory(root: &Path, path: &WorkspacePath) -> AnchoredOpen {
    let mut directory = match open_workspace_root(root) {
        Ok(directory) => directory.directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return AnchoredOpen::Absent,
        Err(_) => return AnchoredOpen::Blocked,
    };
    let components: Vec<&str> = path.as_str().split('/').collect();
    let parent_count = components.len().saturating_sub(1);
    for component in components.iter().take(parent_count) {
        if component.is_empty() {
            continue; // defensive: tolerate an accidental double slash
        }
        directory = match rustix::fs::openat(&directory, *component, DIRECTORY_FLAGS, Mode::empty())
        {
            Ok(child) => child,
            Err(Errno::NOENT) => return AnchoredOpen::Absent,
            // ELOOP (a symlink), ENOTDIR (a file), EACCES: the chain exists but
            // cannot be descended safely.
            Err(_) => return AnchoredOpen::Blocked,
        };
    }
    AnchoredOpen::Ready(AnchoredDirectory { directory })
}

/// Open the workspace root itself and keep the descriptor.
///
/// The io error is surfaced rather than folded into [`AnchoredOpen`] because a
/// caller anchoring a whole walk on the root must tell a missing drive from a
/// permission problem and report the named fault
/// ([`super::super::workspace_root::root_fault_from_io`]); below the root the
/// two answers are the same "cannot descend safely".
pub fn open_workspace_root(root: &Path) -> io::Result<AnchoredDirectory> {
    rustix::fs::open(root, ROOT_DIRECTORY_FLAGS, Mode::empty())
        .map(|directory| AnchoredDirectory { directory })
        .map_err(io::Error::from)
}

impl AnchoredDirectory {
    /// What `leaf` is right now, seen through this descriptor.
    pub fn classify(&self, leaf: &LeafName) -> io::Result<AnchoredLeafKind> {
        classify_at(self.directory.as_fd(), leaf)
    }

    /// Every entry in this directory, each `fstatat`-ed through this same
    /// descriptor. Entries that vanish between the listing and their stat are
    /// dropped: a walk reports what is there, and a name whose object is already
    /// gone is not.
    pub fn entries(&self) -> io::Result<Vec<AnchoredEntry>> {
        let mut entries = Vec::new();
        for name in leaf_names(self.directory.as_fd())? {
            let Some(stat) = stat_at(self.directory.as_fd(), &name)? else {
                continue;
            };
            let kind = kind_of(&stat);
            entries.push(AnchoredEntry {
                name,
                kind,
                byte_len: match kind {
                    AnchoredLeafKind::Directory | AnchoredLeafKind::Absent => None,
                    // `st_size` is a signed `off_t`; no filesystem reports a
                    // negative length, so a value that will not convert is
                    // reported as unknown rather than guessed at.
                    AnchoredLeafKind::NonDirectory => u64::try_from(stat.st_size).ok(),
                },
            });
        }
        Ok(entries)
    }

    /// Open a subdirectory of this one, no-follow, and keep the new descriptor.
    ///
    /// The child is NAMED, never pathed, so nothing above it is resolved a
    /// second time: this is what lets a walk descend without the path it was
    /// listed under being re-resolved underneath it. A child swapped for a
    /// symlink after the listing is [`AnchoredOpen::Blocked`] (`ELOOP`), never
    /// traversed.
    pub fn open_directory(&self, leaf: &LeafName) -> AnchoredOpen {
        match rustix::fs::openat(
            &self.directory,
            leaf.as_c_str(),
            DIRECTORY_FLAGS,
            Mode::empty(),
        ) {
            Ok(directory) => AnchoredOpen::Ready(AnchoredDirectory { directory }),
            Err(Errno::NOENT) => AnchoredOpen::Absent,
            Err(_) => AnchoredOpen::Blocked,
        }
    }

    /// Unlink `leaf` by name. A symlink loses its own name; whatever it pointed
    /// at is not this workspace's to delete.
    pub fn unlink(&self, leaf: &LeafName) -> io::Result<()> {
        rustix::fs::unlinkat(&self.directory, leaf.as_c_str(), AtFlags::empty())
            .map_err(io::Error::from)
    }

    /// Remove `leaf` and everything beneath it, descending only through
    /// descriptors opened from this one. No path is ever re-resolved, so a
    /// directory swapped for a symlink mid-removal cannot redirect the delete
    /// outside the subtree — the descriptor still refers to the original inode.
    pub fn remove_tree(&self, leaf: &LeafName) -> io::Result<()> {
        remove_tree_at(self.directory.as_fd(), leaf, MAX_ANCHORED_DEPTH)
    }

    /// Rename `from` onto `onto`, both names in THIS directory. Same-directory
    /// by construction, so the replacement is atomic and no second chain can be
    /// resolved (or raced) on the destination side.
    pub fn rename(&self, from: &LeafName, onto: &LeafName) -> io::Result<()> {
        rustix::fs::renameat(
            &self.directory,
            from.as_c_str(),
            &self.directory,
            onto.as_c_str(),
        )
        .map_err(io::Error::from)
    }
}

fn classify_at(directory: BorrowedFd<'_>, leaf: &LeafName) -> io::Result<AnchoredLeafKind> {
    Ok(stat_at(directory, leaf)?
        .as_ref()
        .map_or(AnchoredLeafKind::Absent, kind_of))
}

/// `fstatat` a name in a held directory without following it. `None` is the
/// absent answer both errnos mean: nothing at that name, or a component of it
/// that is not a directory.
fn stat_at(directory: BorrowedFd<'_>, leaf: &LeafName) -> io::Result<Option<rustix::fs::Stat>> {
    match rustix::fs::statat(directory, leaf.as_c_str(), AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(Some(stat)),
        Err(Errno::NOENT | Errno::NOTDIR) => Ok(None),
        Err(errno) => Err(io::Error::from(errno)),
    }
}

fn kind_of(stat: &rustix::fs::Stat) -> AnchoredLeafKind {
    if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
        AnchoredLeafKind::Directory
    } else {
        AnchoredLeafKind::NonDirectory
    }
}

fn remove_tree_at(parent: BorrowedFd<'_>, leaf: &LeafName, remaining_depth: u32) -> io::Result<()> {
    let Some(remaining_depth) = remaining_depth.checked_sub(1) else {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "directory nesting exceeds the anchored removal depth",
        ));
    };
    let directory = rustix::fs::openat(parent, leaf.as_c_str(), DIRECTORY_FLAGS, Mode::empty())
        .map_err(io::Error::from)?;
    // Names are collected before anything is unlinked: readdir's behaviour while
    // its own directory is being modified is unspecified, and a skipped entry
    // would leave the final rmdir failing with ENOTEMPTY.
    for child in leaf_names(directory.as_fd())? {
        match classify_at(directory.as_fd(), &child)? {
            AnchoredLeafKind::Directory => {
                remove_tree_at(directory.as_fd(), &child, remaining_depth)?;
            }
            AnchoredLeafKind::NonDirectory => {
                rustix::fs::unlinkat(&directory, child.as_c_str(), AtFlags::empty())
                    .map_err(io::Error::from)?;
            }
            // Removed by someone else between the listing and now: the goal of
            // this step is already met.
            AnchoredLeafKind::Absent => {}
        }
    }
    drop(directory);
    rustix::fs::unlinkat(parent, leaf.as_c_str(), AtFlags::REMOVEDIR).map_err(io::Error::from)
}

fn leaf_names(directory: BorrowedFd<'_>) -> io::Result<Vec<LeafName>> {
    let entries = Dir::read_from(directory).map_err(io::Error::from)?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io::Error::from)?;
        if let Some(leaf) = LeafName::from_directory_entry(entry.file_name()) {
            names.push(leaf);
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests;
