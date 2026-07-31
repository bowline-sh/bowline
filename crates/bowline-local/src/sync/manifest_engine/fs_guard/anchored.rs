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
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, RenameFlags};
use rustix::io::Errno;

use crate::policy::{is_recovery_quarantine_component, is_recovery_temp_component};

use super::super::manifest::{FileMode, WorkspacePath};
use super::{AtomicWrite, GuardedWrite, PRIVATE_FILE_MODE, fingerprint_of};

mod atomic_write;
mod recovery_ops;

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

const RECOVERY_OWNER_MAGIC: &[u8; 8] = b"BWLREC01";
const RECOVERY_OWNER_HEADER_BYTES: usize = 50;
const RECOVERY_OWNER_BYTES: usize = RECOVERY_OWNER_HEADER_BYTES + 255;
const RECOVERY_CLEANUP_GRACE: Duration = Duration::from_secs(24 * 60 * 60);

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

    /// The final component of an absolute or relative filesystem path.
    pub fn from_path(path: &Path) -> Option<Self> {
        let leaf = path.file_name()?.as_bytes();
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

    fn recovery_sibling() -> io::Result<Self> {
        Self::random_temp_name(".bowline-recovery-")
    }

    fn atomic_write_sibling() -> io::Result<Self> {
        Self::random_temp_name(".bowline-materialize-atomic-")
    }

    fn random_temp_name(prefix: &str) -> io::Result<Self> {
        let mut nonce = [0_u8; 16];
        getrandom::fill(&mut nonce)
            .map_err(|_| io::Error::other("recovery temp randomness unavailable"))?;
        CString::new(format!("{prefix}{:032x}.tmp", u128::from_le_bytes(nonce)))
            .map(Self)
            .map_err(|_| io::Error::other("generated recovery temp name contained NUL"))
    }

    fn recovery_owner_sibling(&self) -> Option<Self> {
        let name = self.as_str()?;
        let nonce = name
            .strip_prefix(".bowline-recovery-")?
            .strip_suffix(".tmp")?;
        CString::new(format!(".bowline-recovery-owner-{nonce}.record"))
            .ok()
            .map(Self)
    }

    fn recovery_owner_completion_sibling(&self) -> Option<Self> {
        let name = self.as_str()?;
        CString::new(format!("{name}.complete")).ok().map(Self)
    }

    fn recovery_pending_owner_sibling(&self) -> Option<Self> {
        let name = self.as_str()?;
        let pending = name.strip_suffix(".complete").unwrap_or(name);
        CString::new(pending).ok().map(Self)
    }

    fn is_recovery_owner_completion(&self) -> bool {
        self.as_str()
            .is_some_and(|name| name.ends_with(".complete"))
    }

    fn recovery_quarantine_sibling(&self) -> Option<Self> {
        let name = self.as_str()?;
        let nonce = name
            .strip_prefix(".bowline-recovery-")?
            .strip_suffix(".tmp")?;
        let quarantine = format!(".bowline-recovery-quarantine-{nonce}.tmp");
        is_recovery_quarantine_component(&quarantine).then(|| {
            Self(CString::new(quarantine).expect("generated recovery quarantine is NUL-free"))
        })
    }

    fn recovery_temp_for_owner(&self) -> Option<Self> {
        let name = self.as_str()?;
        let base = name.strip_suffix(".complete").unwrap_or(name);
        let nonce = base
            .strip_prefix(".bowline-recovery-owner-")?
            .strip_suffix(".record")?;
        let temp = format!(".bowline-recovery-{nonce}.tmp");
        is_recovery_temp_component(&temp)
            .then(|| Self(CString::new(temp).expect("generated recovery leaf is NUL-free")))
    }
}

pub(crate) fn is_recovery_owner_record_name(name: &str) -> bool {
    let base = name.strip_suffix(".complete").unwrap_or(name);
    let Some(nonce) = base
        .strip_prefix(".bowline-recovery-owner-")
        .and_then(|name| name.strip_suffix(".record"))
    else {
        return false;
    };
    is_recovery_temp_component(&format!(".bowline-recovery-{nonce}.tmp"))
}

fn is_lower_hex_nonce(nonce: &str) -> bool {
    nonce.len() == 32
        && nonce
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
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

/// Open an engine-owned directory without following a replacement symlink.
pub fn open_private_root(root: &Path) -> io::Result<AnchoredDirectory> {
    rustix::fs::open(root, DIRECTORY_FLAGS, Mode::empty())
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

    /// Create one child directory without following its name.
    pub fn create_directory(&self, leaf: &LeafName, mode: u32) -> io::Result<AnchoredOpen> {
        match rustix::fs::mkdirat(
            &self.directory,
            leaf.as_c_str(),
            Mode::from_bits_truncate(mode as rustix::fs::RawMode),
        ) {
            Ok(()) | Err(Errno::EXIST) => Ok(self.open_directory(leaf)),
            Err(errno) => Err(io::Error::from(errno)),
        }
    }

    /// Exclusively create a private regular file in this held directory.
    pub fn create_private_file(&self, leaf: &LeafName) -> io::Result<fs::File> {
        rustix::fs::openat(
            &self.directory,
            leaf.as_c_str(),
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_bits_truncate(PRIVATE_FILE_MODE as rustix::fs::RawMode),
        )
        .map(fs::File::from)
        .map_err(io::Error::from)
    }

    /// Unlink `leaf` by name. A symlink loses its own name; whatever it pointed
    /// at is not this workspace's to delete.
    pub fn unlink(&self, leaf: &LeafName) -> io::Result<()> {
        rustix::fs::unlinkat(&self.directory, leaf.as_c_str(), AtFlags::empty())
            .map_err(io::Error::from)
    }

    fn sync(&self) -> io::Result<()> {
        rustix::fs::fsync(&self.directory).map_err(io::Error::from)
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

    /// Atomically install a staged regular file from another held directory.
    ///
    /// The staged inode is opened no-follow, made durable, and assigned its
    /// final mode before one descriptor-relative rename installs it. A symlink
    /// already at the destination is refused.
    pub fn install_staged_file(
        &self,
        source_directory: &Self,
        source_leaf: &LeafName,
        destination_leaf: &LeafName,
        final_mode: FileMode,
    ) -> io::Result<GuardedWrite> {
        let fd = match rustix::fs::openat(
            &source_directory.directory,
            source_leaf.as_c_str(),
            OFlags::RDWR | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::LOOP | Errno::NOENT | Errno::NOTDIR | Errno::ISDIR | Errno::NXIO) => {
                return Ok(GuardedWrite::Blocked);
            }
            Err(errno) => return Err(io::Error::from(errno)),
        };
        let mut file = fs::File::from(fd);
        if !file.metadata()?.file_type().is_file() {
            return Ok(GuardedWrite::Blocked);
        }
        file.sync_all()?;
        file.set_permissions(fs::Permissions::from_mode(final_mode.get()))?;
        file.sync_all()?;
        let exchanged =
            match self.rename_refusing_symlink(source_directory, source_leaf, destination_leaf) {
                Ok(Some(exchanged)) => exchanged,
                Ok(None) => return Ok(GuardedWrite::Blocked),
                Err(error) if error.raw_os_error() == Some(Errno::XDEV.raw_os_error()) => {
                    return self.copy_staged_file_atomic(
                        source_directory,
                        &mut file,
                        destination_leaf,
                        final_mode,
                    );
                }
                Err(error) => return Err(error),
            };
        let opened = file.metadata()?;
        let Some(named) = stat_at(self.directory.as_fd(), destination_leaf)? else {
            return Ok(GuardedWrite::Blocked);
        };
        if stat_identity(&named) != Some(file_identity(&opened)) {
            self.rollback_staged_install(
                source_directory,
                source_leaf,
                destination_leaf,
                exchanged,
            )?;
            return Ok(GuardedWrite::Blocked);
        }
        if exchanged {
            match source_directory.unlink(source_leaf) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(GuardedWrite::Written(fingerprint_of(&opened)))
    }

    fn rename_refusing_symlink(
        &self,
        source_directory: &Self,
        source_leaf: &LeafName,
        destination_leaf: &LeafName,
    ) -> io::Result<Option<bool>> {
        let destination = stat_at(self.directory.as_fd(), destination_leaf)?;
        if destination
            .as_ref()
            .is_some_and(|stat| FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile)
        {
            return Ok(None);
        }
        if destination.is_none() {
            return match rustix::fs::renameat_with(
                &source_directory.directory,
                source_leaf.as_c_str(),
                &self.directory,
                destination_leaf.as_c_str(),
                RenameFlags::NOREPLACE,
            ) {
                Ok(()) => Ok(Some(false)),
                Err(Errno::EXIST) => Ok(None),
                Err(Errno::XDEV) => Err(io::Error::from(Errno::XDEV)),
                Err(errno) => Err(io::Error::from(errno)),
            };
        }
        rustix::fs::renameat_with(
            &source_directory.directory,
            source_leaf.as_c_str(),
            &self.directory,
            destination_leaf.as_c_str(),
            RenameFlags::EXCHANGE,
        )
        .map_err(io::Error::from)?;
        if stat_at(source_directory.directory.as_fd(), source_leaf)?
            .is_some_and(|stat| FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile)
        {
            rustix::fs::renameat_with(
                &source_directory.directory,
                source_leaf.as_c_str(),
                &self.directory,
                destination_leaf.as_c_str(),
                RenameFlags::EXCHANGE,
            )
            .map_err(io::Error::from)?;
            return Ok(None);
        }
        Ok(Some(true))
    }

    /// Restore the namespace after the staged name was swapped between its
    /// validated open and the rename. An exchange puts the original destination
    /// back atomically. A no-replace create moves the substituted inode back out
    /// of the workspace, restoring the original absence.
    fn rollback_staged_install(
        &self,
        source_directory: &Self,
        source_leaf: &LeafName,
        destination_leaf: &LeafName,
        exchanged: bool,
    ) -> io::Result<()> {
        if exchanged {
            return rustix::fs::renameat_with(
                &source_directory.directory,
                source_leaf.as_c_str(),
                &self.directory,
                destination_leaf.as_c_str(),
                RenameFlags::EXCHANGE,
            )
            .map_err(io::Error::from);
        }
        match source_directory.unlink(source_leaf) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        rustix::fs::renameat_with(
            &self.directory,
            destination_leaf.as_c_str(),
            &source_directory.directory,
            source_leaf.as_c_str(),
            RenameFlags::NOREPLACE,
        )
        .map_err(io::Error::from)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

struct RecoveryOwnerRecord {
    created_at: u64,
    directory: FileIdentity,
    temp: Option<FileIdentity>,
    destination_leaf: LeafName,
}

fn file_identity(metadata: &fs::Metadata) -> FileIdentity {
    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

// `Stat` mirrors the platform's `struct stat`, and its field widths differ:
// `st_dev` is i32 on macOS but u64 on Linux, `st_mode` u16 but u32. A widening
// conversion is therefore mandatory on one target and a `useless_conversion`
// lint on the other, which no single expression satisfies. Both conversions
// live here once, per platform, so call sites read the same everywhere and a
// macOS-only check can never miss what Linux rejects.
#[cfg(target_os = "macos")]
fn device_number(stat: &rustix::fs::Stat) -> Option<u64> {
    u64::try_from(stat.st_dev).ok()
}

#[cfg(not(target_os = "macos"))]
fn device_number(stat: &rustix::fs::Stat) -> Option<u64> {
    Some(stat.st_dev)
}

#[cfg(target_os = "macos")]
pub(super) fn permission_bits(stat: &rustix::fs::Stat) -> u32 {
    u32::from(stat.st_mode & 0o777)
}

#[cfg(not(target_os = "macos"))]
pub(super) fn permission_bits(stat: &rustix::fs::Stat) -> u32 {
    stat.st_mode & 0o777
}

fn stat_identity(stat: &rustix::fs::Stat) -> Option<FileIdentity> {
    Some(FileIdentity {
        device: device_number(stat)?,
        inode: stat.st_ino,
    })
}

fn directory_identity(directory: &OwnedFd) -> io::Result<FileIdentity> {
    rustix::fs::fstat(directory)
        .map_err(io::Error::from)
        .and_then(|stat| {
            stat_identity(&stat).ok_or_else(|| io::Error::other("directory device overflow"))
        })
}

fn write_recovery_owner(
    file: &mut fs::File,
    created_at: u64,
    directory: FileIdentity,
    temp: Option<FileIdentity>,
    destination_leaf: &LeafName,
) -> io::Result<()> {
    let temp = temp.unwrap_or(FileIdentity {
        device: 0,
        inode: 0,
    });
    let destination_bytes = destination_leaf.as_c_str().to_bytes();
    let destination_length = u16::try_from(destination_bytes.len())
        .map_err(|_| io::Error::other("recovery destination leaf too long"))?;
    let mut record = [0_u8; RECOVERY_OWNER_BYTES];
    record[..8].copy_from_slice(RECOVERY_OWNER_MAGIC);
    for (index, value) in [
        created_at,
        directory.device,
        directory.inode,
        temp.device,
        temp.inode,
    ]
    .into_iter()
    .enumerate()
    {
        let start = 8 + index * 8;
        record[start..start + 8].copy_from_slice(&value.to_le_bytes());
    }
    record[48..50].copy_from_slice(&destination_length.to_le_bytes());
    record[RECOVERY_OWNER_HEADER_BYTES..RECOVERY_OWNER_HEADER_BYTES + destination_bytes.len()]
        .copy_from_slice(destination_bytes);
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&record)?;
    file.set_len(RECOVERY_OWNER_BYTES as u64)?;
    file.sync_all()
}

fn cleanup_recovery_setup(
    owner_directory: &AnchoredDirectory,
    owner: &LeafName,
    destination_directory: &AnchoredDirectory,
    temp: Option<&LeafName>,
) -> io::Result<()> {
    if let Some(temp) = temp {
        unlink_if_present(destination_directory, temp)?;
        destination_directory.sync()?;
    }
    if let Some(completion) = owner.recovery_owner_completion_sibling() {
        unlink_if_present(owner_directory, &completion)?;
    }
    unlink_if_present(owner_directory, owner)?;
    owner_directory.sync()
}

fn publish_completed_recovery_owner(
    owner_directory: &AnchoredDirectory,
    owner: &LeafName,
    created_at: u64,
    directory: FileIdentity,
    temp: FileIdentity,
    destination_leaf: &LeafName,
) -> io::Result<()> {
    let completion = owner
        .recovery_owner_completion_sibling()
        .ok_or_else(|| io::Error::other("recovery completion name invalid"))?;
    unlink_if_present(owner_directory, &completion)?;
    let mut completion_file = owner_directory.create_private_file(&completion)?;
    write_recovery_owner(
        &mut completion_file,
        created_at,
        directory,
        Some(temp),
        destination_leaf,
    )?;
    rustix::fs::renameat_with(
        &owner_directory.directory,
        completion.as_c_str(),
        &owner_directory.directory,
        owner.as_c_str(),
        RenameFlags::empty(),
    )
    .map_err(io::Error::from)?;
    owner_directory.sync()
}

fn remove_recovery_owner_family(
    owner_directory: &AnchoredDirectory,
    owner: &LeafName,
) -> io::Result<()> {
    let pending = owner
        .recovery_pending_owner_sibling()
        .ok_or_else(|| io::Error::other("recovery pending owner name invalid"))?;
    if let Some(completion) = pending.recovery_owner_completion_sibling() {
        unlink_if_present(owner_directory, &completion)?;
    }
    unlink_if_present(owner_directory, &pending)?;
    owner_directory.sync()
}

fn unlink_if_present(directory: &AnchoredDirectory, leaf: &LeafName) -> io::Result<()> {
    match directory.unlink(leaf) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn read_recovery_owner(
    directory: &AnchoredDirectory,
    owner: &LeafName,
) -> io::Result<Option<RecoveryOwnerRecord>> {
    let fd = match rustix::fs::openat(
        &directory.directory,
        owner.as_c_str(),
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(Errno::NOENT | Errno::LOOP | Errno::ISDIR | Errno::NOTDIR) => return Ok(None),
        Err(error) => return Err(io::Error::from(error)),
    };
    let mut file = fs::File::from(fd);
    let mut bytes = [0_u8; RECOVERY_OWNER_BYTES];
    match file.read_exact(&mut bytes) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let mut extra = [0_u8; 1];
    if file.read(&mut extra)? != 0 || &bytes[..8] != RECOVERY_OWNER_MAGIC {
        return Ok(None);
    }
    let mut values = [0_u64; 5];
    for (index, value) in values.iter_mut().enumerate() {
        let start = 8 + index * 8;
        *value = u64::from_le_bytes(
            bytes[start..start + 8]
                .try_into()
                .expect("fixed recovery owner field"),
        );
    }
    let temp = FileIdentity {
        device: values[3],
        inode: values[4],
    };
    let destination_length = usize::from(u16::from_le_bytes(
        bytes[48..50]
            .try_into()
            .expect("fixed recovery destination length"),
    ));
    let Some(destination_bytes) =
        bytes.get(RECOVERY_OWNER_HEADER_BYTES..RECOVERY_OWNER_HEADER_BYTES + destination_length)
    else {
        return Ok(None);
    };
    let Some(destination_leaf) = CString::new(destination_bytes).ok().map(LeafName) else {
        return Ok(None);
    };
    Ok(Some(RecoveryOwnerRecord {
        created_at: values[0],
        directory: FileIdentity {
            device: values[1],
            inode: values[2],
        },
        temp: (temp.device != 0 || temp.inode != 0).then_some(temp),
        destination_leaf,
    }))
}

fn unix_seconds() -> io::Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| io::Error::other("system clock predates Unix epoch"))
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
