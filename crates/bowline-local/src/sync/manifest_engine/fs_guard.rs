//! The no-follow filesystem trust boundary shared by push and pull/apply.
//!
//! Every read, write, and traversal the engine performs against the workspace
//! tree goes through here. The single contract: **never follow a symlink**, so a
//! leaf or an intermediate component swapped for a symlink (editor save,
//! concurrent tool, or a hostile local process) can never make the engine read
//! bytes from — or write bytes to — outside the workspace root. Observation is
//! `symlink_metadata`; the parent chain is walked component-by-component
//! no-follow ([`prepare_parent_chain`]); leaf reads open `O_NOFOLLOW` and fstat
//! the descriptor they hold ([`read_file_bounded`]); renames, deletes, and
//! recursive walks hold the containing directory open and act on that descriptor
//! ([`anchored`]). Extracted from `push.rs`
//! (which landed it first as Step 4) once the shared boundary earned its own
//! seam; it reuses [`PushError`] as the engine's read/traversal error taxonomy.

use std::collections::VecDeque;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use bowline_core::workspace_graph::resolve_symlink_target;

use super::manifest::{EntryKind, FileMode, WorkspacePath};
use super::push::PushError;
use super::store::StatFingerprint;
use super::unsyncable::{UnsyncableReason, path_scoped_reason};

mod anchored;

pub use anchored::{
    AnchoredDirectory, AnchoredEntry, AnchoredLeafKind, AnchoredOpen, LeafName, MAX_ANCHORED_DEPTH,
    open_containing_directory, open_workspace_root,
};

/// 0600 — owner read/write only. Every engine-authored file (temp, spool,
/// quarantine) is created private so a crash cannot leak plaintext to other
/// users on a shared host.
pub const PRIVATE_FILE_MODE: u32 = 0o600;

/// A single filesystem observation: typed kind plus the stat fingerprint. Never
/// follows symlinks (`symlink_metadata`); content hashing is a separate step so
/// stat-clean paths are never opened (invariant C1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    pub kind: EntryKind,
    pub size: u64,
    pub mode: FileMode,
    pub symlink_target: Option<String>,
    pub fingerprint: StatFingerprint,
}

/// What one observation found. `Unsyncable` is the third answer the engine used
/// to lack: a path that exists but can never be represented or read (a device
/// node, a symlink whose target is not UTF-8, an unreadable leaf). Conflating it
/// with an error is what turned one bad file into a dead engine.
#[derive(Debug)]
pub enum ObserveOutcome {
    Absent,
    Present(Observed),
    Unsyncable(UnsyncableReason),
}

/// Observe a workspace-relative path. **Total by construction**: every outcome
/// is one of the three answers, so observing a path can never fail a cycle.
///
/// `Absent` covers both "nothing there" and "an intermediate component is not a
/// directory" (e.g. a manifest names `f/child` while local `f` is still a file
/// mid kind-swap) — both mean absent locally. Directories, regular files, and
/// symlinks are typed; anything else (socket, fifo, device) and every remaining
/// stat failure is [`ObserveOutcome::Unsyncable`], carrying the reason.
///
/// The absence of a fourth "and sometimes it errors" answer is the design. This
/// function used to return `io::Result`, and an unmodelled errno (ELOOP on an
/// intermediate component, EIO, ENAMETOOLONG) propagated into
/// `CycleError::Fatal` — one path taking the whole workspace's sync down, and,
/// behind a durable intent, taking every subsequent startup down with it.
pub fn observe_classified(root: &Path, path: &WorkspacePath) -> ObserveOutcome {
    let absolute = root.join(path.as_str());
    let metadata = match fs::symlink_metadata(&absolute) {
        Ok(metadata) => metadata,
        Err(error) if is_absent(&error) => return ObserveOutcome::Absent,
        Err(error) => return ObserveOutcome::Unsyncable(path_scoped_reason(&error)),
    };
    let file_type = metadata.file_type();
    let fingerprint = fingerprint_of(&metadata);
    let mode = FileMode::new(metadata.permissions().mode());

    if file_type.is_symlink() {
        let link = match fs::read_link(&absolute) {
            Ok(link) => link,
            Err(error) if is_absent(&error) => return ObserveOutcome::Absent,
            Err(error) => return ObserveOutcome::Unsyncable(path_scoped_reason(&error)),
        };
        let Some(target) = link.to_str().map(str::to_string) else {
            return ObserveOutcome::Unsyncable(UnsyncableReason::NonUtf8SymlinkTarget);
        };
        return ObserveOutcome::Present(Observed {
            kind: EntryKind::Symlink,
            size: 0,
            // Never the mode `lstat` reported: it is the kernel's own constant and
            // differs per platform, so carrying it would make the same link
            // compare unequal across devices (see [`FileMode::symlink`]).
            mode: FileMode::symlink(),
            symlink_target: Some(target),
            fingerprint,
        });
    }
    if file_type.is_dir() {
        return ObserveOutcome::Present(Observed {
            kind: EntryKind::Directory,
            size: 0,
            mode,
            symlink_target: None,
            fingerprint,
        });
    }
    if file_type.is_file() {
        return ObserveOutcome::Present(Observed {
            kind: EntryKind::File,
            size: metadata.len(),
            mode,
            symlink_target: None,
            fingerprint,
        });
    }
    ObserveOutcome::Unsyncable(UnsyncableReason::UnsupportedKind)
}

fn is_absent(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

fn fingerprint_of(metadata: &fs::Metadata) -> StatFingerprint {
    use std::os::unix::fs::MetadataExt;
    StatFingerprint {
        mtime_ns: metadata.mtime_nsec_pair(),
        ctime_ns: metadata.ctime_nsec_pair(),
        inode: metadata.ino(),
        dev: metadata.dev(),
    }
}

/// The pre-open observation a content read is validated against: the exact stat
/// fingerprint and size the caller observed for a regular file. A content read
/// fstats the descriptor it opened and refuses to return bytes unless BOTH still
/// match — so a leaf swapped for a different inode (a hardlink to a secret
/// elsewhere on the device, a rename-in) between observe and open is caught and
/// never sealed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpectedFile {
    pub fingerprint: StatFingerprint,
    pub size: u64,
}

impl ExpectedFile {
    /// The fingerprint/size of a regular file already stat'd no-follow.
    pub fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            fingerprint: fingerprint_of(metadata),
            size: metadata.len(),
        }
    }
}

impl Observed {
    /// The read-validation fingerprint of this observation.
    pub fn expected_file(&self) -> ExpectedFile {
        ExpectedFile {
            fingerprint: self.fingerprint,
            size: self.size,
        }
    }
}

/// The result of a bounded content read validated against the observed file.
#[derive(Debug)]
pub enum FileRead {
    /// The regular file's bytes; its fstat identity and size still match the
    /// caller's pre-open observation.
    Bytes(Vec<u8>),
    /// The path is no longer the regular file that was observed — it became a
    /// symlink (`O_NOFOLLOW` refused the open), sits under a symlinked or missing
    /// parent, vanished, or its fstat identity/size diverged from the expectation.
    /// NEVER carries bytes read through a symlink or from outside the workspace;
    /// the caller must re-observe and re-derive rather than trust these bytes.
    Diverged,
}

/// Every non-final component of a workspace path, classified by a no-follow walk.
pub enum ParentChain {
    /// Every intermediate component that exists is a real directory (missing ones
    /// were created per-component under [`ParentChainMode::CreateMissing`]); the
    /// final-component operation may proceed.
    Ready,
    /// The chain could not be made ready: an intermediate component exists but is
    /// NOT a real directory (a symlink or a file), or the filesystem refused to
    /// stat or create one. Reading, writing, or deleting through it would escape
    /// the workspace root or cannot proceed at all, so the caller must refuse and
    /// treat it as a divergence.
    Blocked,
}

/// Whether [`prepare_parent_chain`] may create missing intermediate directories.
pub enum ParentChainMode {
    /// Create each missing intermediate component (single-component `create_dir`,
    /// never a `create_dir_all` that would recreate — and follow — parents). For
    /// writes that must land.
    CreateMissing,
    /// Never create; a missing component means the target already cannot exist,
    /// so there is nothing to descend into. For reads, deletes, and in-place mode
    /// changes.
    RequireExisting,
}

/// Validate (and, for writes, create) the parent directory chain of a
/// workspace-relative `path` WITHOUT ever following a symlink. Each intermediate
/// component is walked from the root with `symlink_metadata`: a real directory is
/// traversed, a missing one is created (`CreateMissing`) or stops the walk
/// (`RequireExisting`), and anything else — a symlink or a file — returns
/// [`ParentChain::Blocked`].
///
/// Why this is the single owner: it is reused by both the apply side (a sealed
/// manifest from an authorized peer can name `dir/file` while local `dir` is a
/// symlink pointing OUTSIDE the workspace — a naive `create_dir_all(parent)` +
/// rename/remove would materialize or delete through it) and the push read side
/// (the same symlinked `dir` would let a content read escape the root and seal
/// secrets from elsewhere on the device into workspace state). Refusing to
/// descend through a non-directory keeps every mutation and every read inside the
/// root; callers map `Blocked` to a keep-local / skip divergence.
///
/// Scope: this answers about the on-disk shape of a PATH, and that answer
/// expires when it returns. It is sound for a caller whose next step re-verifies
/// what it actually touched — the read side's post-open fstat identity check —
/// but not on its own for a caller that then mutates by name, because the kernel
/// re-resolves every component it just walked. Mutations use
/// [`anchored::open_containing_directory`] and act on the held descriptor
/// instead, so the check and the operation reference one directory handle.
///
/// Total by construction, like [`observe_classified`]: a stat or `create_dir`
/// the filesystem refuses (EACCES on a parent, ENOSPC, EROFS, ELOOP) answers
/// `Blocked` — the honest reading of "this chain is not usable" — rather than
/// raising an error that would classify as a fatal and stop the whole workspace.
pub fn prepare_parent_chain(
    root: &Path,
    path: &WorkspacePath,
    mode: ParentChainMode,
) -> ParentChain {
    let components: Vec<&str> = path.as_str().split('/').collect();
    // The final component is the target itself; the operation (open / rename /
    // create / remove / symlink) acts on it by name and never follows it. Only the
    // intermediate components form the parent chain we must verify.
    let parent_count = components.len().saturating_sub(1);
    let mut current = root.to_path_buf();
    for component in components.iter().take(parent_count) {
        if component.is_empty() {
            continue; // defensive: tolerate an accidental double slash
        }
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return ParentChain::Blocked,
            Err(error) if error.kind() == io::ErrorKind::NotFound => match mode {
                ParentChainMode::CreateMissing => {
                    if !ensure_dir(&current) {
                        return ParentChain::Blocked;
                    }
                }
                ParentChainMode::RequireExisting => return ParentChain::Ready,
            },
            Err(_) => return ParentChain::Blocked,
        }
    }
    ParentChain::Ready
}

/// Create one missing intermediate component, tolerating a racing writer that
/// created it first — but only when what now sits there is a real directory. An
/// `AlreadyExists` that turns out to be a file or a symlink is exactly the escape
/// this walk exists to refuse, so it answers `false` (blocked).
fn ensure_dir(current: &Path) -> bool {
    match fs::create_dir(current) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            fs::symlink_metadata(current).is_ok_and(|metadata| metadata.is_dir())
        }
        Err(_) => false,
    }
}

/// How many symlink hops one containment resolution follows before refusing.
/// Matches the kernel's own `ELOOP` ceiling, so a chain this walk refuses is one
/// the kernel would refuse to resolve anyway — and the bound is what makes a
/// cycle (`a -> b`, `b -> a`) terminate instead of spinning.
const MAX_SYMLINK_HOPS: u32 = 40;

/// Whether a symlink materialized at `link` with `target` still LANDS inside the
/// workspace once the on-disk shape of the path it names is taken into account.
///
/// The lexical gate
/// ([`bowline_core::workspace_graph::symlink_target_stays_in_workspace`]) decides
/// only where the target points on its face. It cannot see that
/// `~/Code/escape -> /etc` already exists — Bowline refuses to publish that link,
/// but refusing to sync it does not delete it — so a peer entry
/// `read-passwd -> escape/passwd` passes the lexical gate and resolves to
/// `/etc/passwd`. Bowline never follows a symlink, but the user's editor, build
/// tooling, and agents do. This is the gate that catches it, and it is why the
/// two gates are not redundant.
///
/// It reuses [`prepare_parent_chain`]'s discipline — component by component from
/// the root, `symlink_metadata` only, never an `open` or a `canonicalize` that
/// would let the kernel traverse on our behalf — with one deliberate difference:
/// a symlinked component is RESOLVED rather than refused outright, provided its
/// own target is lexically contained. Refusing every symlinked component would be
/// safe but wrong in practice: a pnpm workspace links `node_modules/.bin/x` to
/// `../pkg/bin.js` through a symlinked `node_modules/pkg`, and Bowline would stop
/// syncing ordinary repositories. Following only contained hops keeps every step
/// of the resolution inside the root, which is the property that actually matters.
///
/// Any doubt refuses: an unreadable component, a non-UTF-8 hop target, or a chain
/// past [`MAX_SYMLINK_HOPS`] all return `false`. Refusal is path-scoped by the
/// caller (recorded unsyncable, the path frozen), never a failed cycle, so a
/// hostile entry cannot take a peer's whole manifest down with it.
pub fn symlink_target_lands_in_workspace(root: &Path, link: &WorkspacePath, target: &str) -> bool {
    let Some(resolved) = resolve_symlink_target(link.as_str(), target) else {
        return false; // escapes on its face; the on-disk shape cannot redeem it
    };
    let mut pending: VecDeque<String> = path_components(resolved.as_str()).collect();
    let mut walked: Vec<String> = Vec::new();
    let mut hops = 0_u32;

    while let Some(component) = pending.pop_front() {
        walked.push(component);
        let walked_path = walked.join("/");
        let metadata = match fs::symlink_metadata(root.join(&walked_path)) {
            Ok(metadata) => metadata,
            // Nothing exists here, so nothing below it exists either: the rest of
            // the walk can meet no symlink, and the lexical resolution stands.
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                return true;
            }
            Err(_) => return false,
        };
        if !metadata.file_type().is_symlink() {
            continue;
        }
        hops = hops.saturating_add(1);
        if hops > MAX_SYMLINK_HOPS {
            return false;
        }
        let Ok(hop) = fs::read_link(root.join(&walked_path)) else {
            return false;
        };
        let Some(hop_target) = hop.to_str() else {
            return false;
        };
        let Some(hop_resolved) = resolve_symlink_target(&walked_path, hop_target) else {
            return false; // an existing local symlink that itself leaves the root
        };
        // The hop destination is a fresh workspace-relative path, so resolution
        // restarts from the root with the components not yet consumed still queued.
        walked.clear();
        for component in path_components(hop_resolved.as_str())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            pending.push_front(component);
        }
    }
    true
}

fn path_components(path: &str) -> impl Iterator<Item = String> + '_ {
    path.split('/')
        .filter(|component| !component.is_empty())
        .map(str::to_string)
}

/// Read a regular file's bytes, but ONLY if the on-disk object is still exactly
/// the regular file the caller observed. The read is hardened against a leaf or
/// intermediate component being swapped for a symlink (editor save, concurrent
/// tool, or hostile local process) between observation and open, which would
/// otherwise seal bytes from OUTSIDE the workspace into replicated state:
///
/// - intermediate components are verified no-follow ([`prepare_parent_chain`]);
///   a symlinked parent is a [`FileRead::Diverged`], never read through;
/// - the leaf is opened `O_NOFOLLOW` (`ELOOP` when it became a symlink → diverge);
/// - the opened descriptor is fstat'd and its (dev, inode, size, mtime/ctime)
///   compared to `expected` — a mismatch (raced inode, followed intermediate
///   symlink, truncation/growth) diverges rather than returning the bytes.
///
/// Above `max_bytes` the envelope's whole-buffer seal cannot proceed within a
/// bounded budget — the Plan 109 STOP condition surfaces as a typed error.
pub fn read_file_bounded(
    root: &Path,
    path: &WorkspacePath,
    max_bytes: u64,
    expected: &ExpectedFile,
) -> Result<FileRead, PushError> {
    use rustix::fs::{Mode, OFlags};
    use rustix::io::Errno;

    // A symlinked intermediate component would let the open below escape the
    // root; refuse to read through it. Missing components mean the leaf cannot
    // exist — the open then fails NOENT and diverges.
    if let ParentChain::Blocked = prepare_parent_chain(root, path, ParentChainMode::RequireExisting)
    {
        return Ok(FileRead::Diverged);
    }

    let absolute = root.join(path.as_str());
    // O_NOFOLLOW: opening the leaf when it is a symlink fails with ELOOP rather
    // than following it. O_NONBLOCK: a leaf raced into a FIFO opens immediately
    // instead of blocking on a writer (the fstat below then rejects it).
    let fd = match rustix::fs::open(
        &absolute,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        // ELOOP: the leaf is now a symlink. NOENT: it vanished. NOTDIR: an
        // intermediate raced into a non-directory. ISDIR/NXIO: it is no longer a
        // readable regular file. All are divergences, never engine errors.
        Err(Errno::LOOP | Errno::NOENT | Errno::NOTDIR | Errno::ISDIR | Errno::NXIO) => {
            return Ok(FileRead::Diverged);
        }
        Err(errno) => return Err(PushError::Io(io::Error::from(errno))),
    };

    // fstat the descriptor we hold — no path re-resolution — so the identity we
    // validate is the object we will actually read.
    let mut file = fs::File::from(fd);
    let metadata = file.metadata().map_err(PushError::Io)?;
    if !metadata.file_type().is_file()
        || fingerprint_of(&metadata) != expected.fingerprint
        || metadata.len() != expected.size
    {
        // A directory, a followed intermediate symlink's target, a raced inode, or
        // a truncation/growth since the observation: do not seal these bytes.
        return Ok(FileRead::Diverged);
    }

    if metadata.len() > max_bytes {
        return Err(PushError::StreamSealUnsupported {
            byte_len: metadata.len(),
            ceiling: max_bytes,
        });
    }
    let mut buffer = Vec::with_capacity(metadata.len() as usize);
    // Bound the read too: a fingerprint match pins the size, but read defensively
    // so a torn read past the ceiling can never be buffered unboundedly.
    let read = file
        .by_ref()
        .take(max_bytes + 1)
        .read_to_end(&mut buffer)
        .map_err(PushError::Io)?;
    if read as u64 > max_bytes {
        return Err(PushError::StreamSealUnsupported {
            byte_len: read as u64,
            ceiling: max_bytes,
        });
    }
    Ok(FileRead::Bytes(buffer))
}

/// Write `bytes` to `path` as a private (0600) file, replacing any existing
/// content. Used for the sealed spool; apply uses its own no-replace variant.
pub fn write_private_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    options.mode(PRIVATE_FILE_MODE);
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

/// The outcome of an atomic private-file write. `Blocked` mirrors
/// [`ParentChain::Blocked`]: an intermediate component (or the temp leaf) is a
/// symlink or a file, so writing through it would escape the workspace root and
/// the caller must refuse rather than materialize outside the tree.
pub enum AtomicWrite {
    Written,
    Blocked,
}

/// Atomically write `bytes` to workspace-relative `path` as a private (0600)
/// file, replacing any existing content, WITHOUT ever following a symlink. The
/// one primitive product surfaces (the work-view aux index) use to publish a
/// small reserved file into the workspace tree. A naive `create_dir_all` +
/// `fs::write` + `fs::rename` would follow a symlinked `.bowline-meta` (or any
/// parent) and overwrite external files as the Bowline user — the exact escape
/// this boundary exists to deny:
///
/// - the parent chain is validated (and, if missing, created) no-follow
///   ([`prepare_parent_chain`] `CreateMissing`); a symlinked intermediate is
///   [`AtomicWrite::Blocked`], never written through;
/// - the temp sibling is created `O_NOFOLLOW | O_CREAT | O_TRUNC` in that same
///   verified parent, so the final rename is a same-directory atomic replace; a
///   symlink swapped in at the temp name is refused (`ELOOP`), never followed;
/// - the rename targets the final leaf BY NAME — `rename(2)` never follows a
///   final symlink, it replaces it — so a symlinked leaf is overwritten in place,
///   never traversed onto an external target.
pub fn write_private_file_atomic(
    root: &Path,
    path: &WorkspacePath,
    bytes: &[u8],
) -> Result<AtomicWrite, PushError> {
    use rustix::fs::{Mode, OFlags};
    use rustix::io::Errno;
    use std::io::Write;

    if let ParentChain::Blocked = prepare_parent_chain(root, path, ParentChainMode::CreateMissing) {
        return Ok(AtomicWrite::Blocked);
    }

    let absolute = root.join(path.as_str());
    // The temp lives beside the target leaf, inside the parent chain just verified
    // no-follow, so the rename below is a same-directory atomic replace that never
    // crosses a boundary.
    let parent = absolute.parent().ok_or_else(|| {
        PushError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workspace path has no parent directory",
        ))
    })?;
    let leaf = absolute.file_name().ok_or_else(|| {
        PushError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workspace path has no final component",
        ))
    })?;
    let mut temp_name = std::ffi::OsString::from(".");
    temp_name.push(leaf);
    temp_name.push(".tmp");
    let temp = parent.join(&temp_name);

    // O_NOFOLLOW: a symlink swapped in at the temp name fails ELOOP instead of
    // being followed outside the root. O_TRUNC reuses a stale regular temp.
    let fd = match rustix::fs::open(
        &temp,
        OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_bits_truncate(PRIVATE_FILE_MODE as rustix::fs::RawMode),
    ) {
        Ok(fd) => fd,
        // ELOOP: the temp name is a symlink. NOTDIR: an intermediate raced into a
        // non-directory. Both are unusable on-disk shapes — refuse the write.
        Err(Errno::LOOP | Errno::NOTDIR) => return Ok(AtomicWrite::Blocked),
        Err(errno) => return Err(PushError::Io(io::Error::from(errno))),
    };
    let mut file = fs::File::from(fd);
    file.write_all(bytes).map_err(PushError::Io)?;
    file.sync_all().map_err(PushError::Io)?;
    drop(file);
    fs::rename(&temp, &absolute).map_err(PushError::Io)?;
    Ok(AtomicWrite::Written)
}

/// Unix metadata timestamps flattened to a single nanosecond count, so the
/// engine compares one number rather than a (seconds, nanoseconds) pair. Shared
/// with [`super::endpoint`], which reads the endpoint volume's clock off a probe
/// file's mtime and must flatten it exactly the way a fingerprint does.
pub(super) trait MetadataNsecPair {
    fn mtime_nsec_pair(&self) -> i64;
    fn ctime_nsec_pair(&self) -> i64;
}

impl MetadataNsecPair for fs::Metadata {
    fn mtime_nsec_pair(&self) -> i64 {
        use std::os::unix::fs::MetadataExt;
        self.mtime()
            .saturating_mul(1_000_000_000)
            .saturating_add(self.mtime_nsec())
    }

    fn ctime_nsec_pair(&self) -> i64 {
        use std::os::unix::fs::MetadataExt;
        self.ctime()
            .saturating_mul(1_000_000_000)
            .saturating_add(self.ctime_nsec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::TempWorkspace;

    #[test]
    fn an_observed_link_carries_the_canonical_mode_not_the_kernel_constant() {
        // Linux reports 0o120777 for a link it has just created and macOS
        // 0o120755, for the same `symlink(2)` call with no mode argument at all.
        // Carrying whichever one `lstat` said made a single link two different
        // entries across a two-platform fleet, so the observation pins the value.
        let workspace = TempWorkspace::new("observe-link-mode").expect("temp workspace");
        let root = workspace.root();
        std::os::unix::fs::symlink("main.txt", root.join("link.txt")).expect("create link");

        let ObserveOutcome::Present(observed) =
            observe_classified(root, &WorkspacePath::new("link.txt"))
        else {
            panic!("the link is observable");
        };

        assert_eq!(observed.kind, EntryKind::Symlink);
        assert_eq!(observed.mode, FileMode::symlink());
    }
}
