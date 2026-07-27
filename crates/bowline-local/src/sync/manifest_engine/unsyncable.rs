//! Paths the engine cannot sync, and why.
//!
//! Before this existed, every per-path filesystem failure that was not one of a
//! handful of hand-listed errnos became `CycleError::Fatal` and killed the engine
//! thread: one root-owned file, one 0700 directory, one transient `EMFILE`, one
//! non-UTF-8 filename, or one file over the seal ceiling stopped ALL sync for the
//! whole workspace. A path the engine cannot read is a fact about that path, not
//! a fault of the engine — it belongs in a durable, user-visible set with a
//! remedy, and the cycle continues.
//!
//! `Fatal` is now reserved for genuine invariant violations.

use std::fmt;
use std::io;

use super::manifest::{PathRejection, WorkspacePath};

/// Why one path cannot participate in sync. Each variant names a condition with
/// a distinct user remedy, so status can print an action rather than an errno.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UnsyncableReason {
    /// The engine may not read or traverse it (EACCES/EPERM). Remedy: ownership
    /// or mode change.
    PermissionDenied,
    /// A transient resource limit (EMFILE/ENFILE/ENOMEM). Retried on the next
    /// cycle; recorded so a persistent one is visible.
    ResourceExhausted,
    /// The filename is not valid UTF-8, so it has no representable workspace
    /// path. Remedy: rename the file.
    NonUtf8Name,
    /// A symlink whose target is not valid UTF-8.
    NonUtf8SymlinkTarget,
    /// A socket, FIFO, or device node — not a file, directory, or symlink.
    UnsupportedKind,
    /// The name cannot be a manifest path at all (absolute, `..`, private engine
    /// state, or a form the workspace normalizer rewrites, such as a POSIX
    /// filename containing a backslash). Publishing it would make every peer's
    /// decode fail.
    UnrepresentablePath,
    /// Larger than the engine's whole-buffer seal ceiling.
    AboveSealCeiling,
    /// The engine reached it but the read failed for another reason (EIO and
    /// friends).
    ReadFailed,
    /// A published symlink whose target escapes the workspace (absolute, or
    /// climbing through `..`). Refused at materialization rather than at decode:
    /// one hostile or malformed entry must not fail a peer's whole manifest.
    /// Bowline never follows symlinks itself, but the user's own tools do.
    EscapingSymlinkTarget,
    /// The filesystem refused an operation on this path for a condition the
    /// engine does not model by name (ENOSPC, EROFS, ELOOP, EIO, ...). This is
    /// the DEFAULT answer for a failed single-path operation, and that default is
    /// the point: every unmodelled errno used to propagate, reach
    /// `CycleError::Fatal`, and kill sync for the whole workspace.
    FilesystemRefused,
    /// An interrupted apply could not be finished from its journalled intent, so
    /// the intent was retired rather than replayed. Not data loss: the ancestor
    /// is untouched and the follow-on pull re-derives this path from the ref.
    RecoveryAbandoned,
}

impl UnsyncableReason {
    /// Every reason, in declaration order. The one list: `from_tag` searches it
    /// and the round-trip test iterates it, so a new variant cannot be added to
    /// the enum and forgotten by either. (It was: `EscapingSymlinkTarget` reached
    /// `from_tag` while the test kept its own copy of the list and missed it.)
    pub const ALL: &'static [Self] = &[
        Self::PermissionDenied,
        Self::ResourceExhausted,
        Self::NonUtf8Name,
        Self::NonUtf8SymlinkTarget,
        Self::UnsupportedKind,
        Self::UnrepresentablePath,
        Self::AboveSealCeiling,
        Self::ReadFailed,
        Self::EscapingSymlinkTarget,
        Self::FilesystemRefused,
        Self::RecoveryAbandoned,
    ];

    /// Wire/storage tag. Paired with [`Self::from_tag`]; both sides of the pair
    /// live here so a new variant cannot be added to only one of them.
    pub fn tag(self) -> &'static str {
        match self {
            Self::PermissionDenied => "permission-denied",
            Self::ResourceExhausted => "resource-exhausted",
            Self::NonUtf8Name => "non-utf8-name",
            Self::NonUtf8SymlinkTarget => "non-utf8-symlink-target",
            Self::UnsupportedKind => "unsupported-kind",
            Self::UnrepresentablePath => "unrepresentable-path",
            Self::AboveSealCeiling => "above-seal-ceiling",
            Self::ReadFailed => "read-failed",
            Self::EscapingSymlinkTarget => "escaping-symlink-target",
            Self::FilesystemRefused => "filesystem-refused",
            Self::RecoveryAbandoned => "recovery-abandoned",
        }
    }

    pub fn from_tag(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|reason| reason.tag() == value)
    }

    /// The user-facing remedy. Status surfaces this verbatim; it must always name
    /// something the user can actually do.
    pub fn remedy(self) -> &'static str {
        match self {
            Self::PermissionDenied => {
                "Bowline cannot read this path. Fix its ownership or permissions \
                 (chmod/chown) and it syncs on the next scan."
            }
            Self::ResourceExhausted => {
                "The system ran out of file descriptors or memory while reading this \
                 path. It retries automatically."
            }
            Self::NonUtf8Name => {
                "This filename is not valid UTF-8 and cannot be represented. Rename it."
            }
            Self::NonUtf8SymlinkTarget => {
                "This symlink points at a target that is not valid UTF-8. Recreate the link."
            }
            Self::UnsupportedKind => {
                "This is a socket, pipe, or device node. Bowline syncs files, \
                 directories, and symlinks only."
            }
            Self::UnrepresentablePath => {
                "This name cannot be a workspace path (it is absolute, traverses \
                 upward, is reserved engine state, or contains a backslash). Rename it."
            }
            Self::AboveSealCeiling => {
                "This file is larger than the maximum Bowline can encrypt in one \
                 piece. Move it out of the workspace or split it."
            }
            Self::ReadFailed => {
                "Reading this path failed. Check the disk and the file, then rescan."
            }
            Self::EscapingSymlinkTarget => {
                "This symlink points outside the workspace, so Bowline did not \
                 create it on this device. Repoint it inside the workspace to sync it."
            }
            Self::FilesystemRefused => {
                "The filesystem refused an operation on this path. Check that the \
                 volume has free space, is writable, and is healthy; Bowline retries \
                 on the next scan."
            }
            Self::RecoveryAbandoned => {
                "Bowline could not finish an interrupted change to this path and \
                 stopped retrying it. The next sync re-derives the path from the \
                 workspace; no local content was changed."
            }
        }
    }

    /// Whether the condition is expected to clear without user action, so the
    /// engine keeps retrying it rather than parking it as an attention item.
    pub fn is_transient(self) -> bool {
        matches!(
            self,
            Self::ResourceExhausted
                | Self::ReadFailed
                | Self::FilesystemRefused
                | Self::RecoveryAbandoned
        )
    }
}

impl fmt::Display for UnsyncableReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.tag())
    }
}

impl From<PathRejection> for UnsyncableReason {
    fn from(_: PathRejection) -> Self {
        Self::UnrepresentablePath
    }
}

/// One durable unsyncable entry: the reason, the raw errno when the kernel gave
/// one (so a support transcript carries the real cause), and when it was last
/// observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsyncableRecord {
    pub reason: UnsyncableReason,
    pub errno: Option<i32>,
    pub observed_at: i64,
}

impl UnsyncableRecord {
    pub fn new(reason: UnsyncableReason, errno: Option<i32>, observed_at: i64) -> Self {
        Self {
            reason,
            errno,
            observed_at,
        }
    }
}

/// Classify a per-path `io::Error` as an unsyncable condition, or `None` when it
/// is not path-scoped and the caller must propagate it.
///
/// `NotFound`/`NotADirectory` deliberately return `None`: they mean "absent",
/// which every caller already models as a removal, not a divergence.
pub fn classify_io_error(error: &io::Error) -> Option<UnsyncableReason> {
    match error.kind() {
        io::ErrorKind::PermissionDenied => Some(UnsyncableReason::PermissionDenied),
        io::ErrorKind::OutOfMemory => Some(UnsyncableReason::ResourceExhausted),
        io::ErrorKind::InvalidData => Some(UnsyncableReason::ReadFailed),
        _ => match error.raw_os_error() {
            // EMFILE / ENFILE: the process or the system is out of descriptors.
            // Both are load conditions that clear on their own, never engine bugs.
            Some(code) if code == libc_emfile() || code == libc_enfile() => {
                Some(UnsyncableReason::ResourceExhausted)
            }
            _ => None,
        },
    }
}

/// The path-scoped reason for an `io::Error` raised by an operation that names
/// exactly ONE workspace path.
///
/// Unlike [`classify_io_error`] this never answers "not path-scoped": an
/// operation on a single path that failed is, by construction, a fact about that
/// path. The default is the whole point. Every call site that instead propagated
/// an unmodelled errno reached `CycleError::Fatal` and killed sync for the entire
/// workspace — and when the failing operation sat behind a durable intent, crash
/// recovery replayed it into the same error on every restart, so the device never
/// started again. `NotFound`/`NotADirectory` are the caller's business: they mean
/// "absent", which every caller models before it ever gets here.
pub fn path_scoped_reason(error: &io::Error) -> UnsyncableReason {
    classify_io_error(error).unwrap_or(UnsyncableReason::FilesystemRefused)
}

// rustix is already a dependency for the no-follow open path; reuse its errno
// constants rather than taking a second libc dependency for two numbers.
fn libc_emfile() -> i32 {
    rustix::io::Errno::MFILE.raw_os_error()
}

fn libc_enfile() -> i32 {
    rustix::io::Errno::NFILE.raw_os_error()
}

/// A path plus the reason it could not sync: what a walk, a scan, or a failed
/// single-path filesystem operation names when it refuses that one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsyncablePath {
    pub path: WorkspacePath,
    pub record: UnsyncableRecord,
}

impl UnsyncablePath {
    /// The refusal for a single-path operation that failed with `error`. The
    /// reason is [`path_scoped_reason`], so no errno can silently become an
    /// engine fault by omission.
    pub fn from_io(path: &WorkspacePath, error: &io::Error, observed_at: i64) -> Self {
        Self {
            path: path.clone(),
            record: UnsyncableRecord::new(
                path_scoped_reason(error),
                error.raw_os_error(),
                observed_at,
            ),
        }
    }

    pub fn new(path: &WorkspacePath, reason: UnsyncableReason, observed_at: i64) -> Self {
        Self {
            path: path.clone(),
            record: UnsyncableRecord::new(reason, None, observed_at),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reason_round_trips_through_its_tag() {
        for reason in UnsyncableReason::ALL.iter().copied() {
            assert_eq!(UnsyncableReason::from_tag(reason.tag()), Some(reason));
            assert!(!reason.remedy().is_empty());
        }
        // Tags are a storage format: two reasons sharing one would silently
        // rewrite a stored row's meaning on the next read.
        let tags: std::collections::BTreeSet<&str> =
            UnsyncableReason::ALL.iter().map(|r| r.tag()).collect();
        assert_eq!(
            tags.len(),
            UnsyncableReason::ALL.len(),
            "tags must be unique"
        );
    }

    #[test]
    fn a_permission_error_is_path_scoped_and_a_missing_path_is_not() {
        let denied = io::Error::from(rustix::io::Errno::ACCESS);
        assert_eq!(
            classify_io_error(&denied),
            Some(UnsyncableReason::PermissionDenied)
        );
        let absent = io::Error::from(rustix::io::Errno::NOENT);
        assert_eq!(classify_io_error(&absent), None);
    }

    #[test]
    fn descriptor_exhaustion_is_a_transient_unsyncable_condition() {
        let exhausted = io::Error::from(rustix::io::Errno::MFILE);
        let reason = classify_io_error(&exhausted).expect("EMFILE is path-scoped");
        assert_eq!(reason, UnsyncableReason::ResourceExhausted);
        assert!(reason.is_transient());
    }
}
