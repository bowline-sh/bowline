//! The projection between a manifest entry and the ancestor row that records it.
//!
//! Split out of `pull_apply` at the seam between *deciding* a reconciliation and
//! *representing* the two shapes it reconciles. Everything here is a pure total
//! function of one entry (plus, for [`entry_matches_record`], one row): no
//! filesystem, no store, no transport. It lives beside the engine's other shared
//! vocabulary rather than inside the merge module because push, pull, apply, the
//! intent journal, and the crash-recovery replay all have to answer "what does
//! this entry say?" identically — a second copy of any of these would be a
//! second opinion about identity.
//!
//! One rule is not a plain field read: a symlink's mode is the kernel's own
//! per-platform constant, so identity is its kind and target alone and the mode
//! is answered canonically (see [`FileMode::symlink`]).

use bowline_core::ids::ContentId;

use super::manifest::{EntryKind, FileMode, ManifestEntry};
use super::push::now_unix_ns;
use super::store::{FileRecord, StatFingerprint};

/// The ancestor row that records `entry`, stamped with the local `fingerprint`
/// the caller observed for the bytes already on disk.
pub(crate) fn record_for_entry(entry: &ManifestEntry, fingerprint: StatFingerprint) -> FileRecord {
    match entry {
        ManifestEntry::File {
            size,
            mode,
            content_id,
            blob_key,
            key_epoch,
        } => FileRecord {
            kind: EntryKind::File,
            size: *size,
            mode: *mode,
            symlink_target: None,
            content_id: Some(content_id.clone()),
            blob_key: Some(blob_key.clone()),
            key_epoch: Some(*key_epoch),
            fingerprint,
            hashed_at: Some(now_unix_ns()),
            // `endpoint::prove_rows` stamps this at commit time.
            verified_at: None,
        },
        ManifestEntry::Directory { mode } => FileRecord {
            kind: EntryKind::Directory,
            size: 0,
            mode: *mode,
            symlink_target: None,
            content_id: None,
            blob_key: None,
            key_epoch: None,
            fingerprint,
            hashed_at: None,
            verified_at: None,
        },
        ManifestEntry::Symlink { target, .. } => FileRecord {
            kind: EntryKind::Symlink,
            size: 0,
            // The ancestor row carries the canonical link mode, never the
            // published one, so a row this device writes from a remote entry
            // compares equal to the row it writes from its own observation.
            mode: FileMode::symlink(),
            symlink_target: Some(target.clone()),
            content_id: None,
            blob_key: None,
            key_epoch: None,
            fingerprint,
            hashed_at: None,
            verified_at: None,
        },
    }
}

/// The mode an entry is compared and applied under.
///
/// A symlink answers with the canonical [`FileMode::symlink`] rather than the
/// mode the manifest carries, so an entry a peer published before this rule — or
/// one published from a platform whose kernel picks a different constant — reads
/// as unchanged instead of as an endless mode conflict.
pub(crate) fn entry_mode(entry: &ManifestEntry) -> FileMode {
    match entry {
        ManifestEntry::File { mode, .. } | ManifestEntry::Directory { mode } => *mode,
        ManifestEntry::Symlink { .. } => FileMode::symlink(),
    }
}

/// The content identity an entry carries, for the kinds that have one.
pub(crate) fn entry_content_id(entry: &ManifestEntry) -> Option<&ContentId> {
    match entry {
        ManifestEntry::File { content_id, .. } => Some(content_id),
        _ => None,
    }
}

/// Whether `entry` and `record` name the same object — content for a file, kind
/// for a directory, target for a symlink. Deliberately says nothing about mode:
/// a mode difference is a mode change, which the caller classifies separately.
pub(crate) fn entry_matches_record(entry: &ManifestEntry, record: &FileRecord) -> bool {
    match entry {
        ManifestEntry::File { content_id, .. } => {
            record.kind == EntryKind::File && record.content_id.as_ref() == Some(content_id)
        }
        ManifestEntry::Directory { .. } => record.kind == EntryKind::Directory,
        ManifestEntry::Symlink { target, .. } => {
            record.kind == EntryKind::Symlink && record.symlink_target.as_deref() == Some(target)
        }
    }
}
