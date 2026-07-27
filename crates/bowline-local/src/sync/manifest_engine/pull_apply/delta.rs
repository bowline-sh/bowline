//! What each side did to a path since the ancestor: the two delta types the
//! merge matrix is indexed by, and how each is derived.
//!
//! Split from the parent `pull_apply` module at the seam between *deriving* the
//! deltas (here) and *deciding* what their combination means (there). The split
//! is what makes the pull cost model legible: `remote_delta` is a pure function
//! of the ancestor map and the decoded manifest and touches no filesystem, while
//! every `local_delta` costs at least one `symlink_metadata`. Keeping them in one
//! file hid that asymmetry, which is exactly how a pull came to stat every entry
//! in the workspace to learn that nothing had happened to it.

use bowline_core::ids::ContentId;

use super::super::fs_guard::{
    ExpectedFile, FileRead, ObserveOutcome, Observed, observe_classified,
};
use super::super::manifest::{EntryKind, ManifestEntry, WorkspacePath};
use super::super::push::{EngineContext, PushError};
use super::super::store::FileRecord;
use super::intents::PreimagePayload;
use super::{PullError, entry_matches_record, entry_mode, record_for_entry};

pub(crate) enum LocalDelta {
    Absent,
    /// The path exists but the engine cannot read or represent it (EACCES on the
    /// leaf, a device node, a non-UTF-8 symlink target). Deliberately distinct
    /// from `Absent`: absent authorises a delete, and deleting a file merely
    /// because it could not be opened is unrecoverable data loss. Every merge row
    /// freezes on it — no filesystem op, no ancestor change — and the stat walk
    /// records it as an unsyncable attention item.
    Unreadable,
    Untracked {
        observed: Observed,
        content_id: Option<ContentId>,
    },
    Unchanged {
        record: FileRecord,
    },
    Changed {
        observed: Observed,
        content_id: Option<ContentId>,
    },
    ModeChanged {
        observed: Observed,
    },
    Deleted,
}

impl LocalDelta {
    /// The expected on-disk preimage for the apply-time re-observation.
    pub(crate) fn preimage(&self) -> PreimagePayload {
        match self {
            Self::Absent | Self::Deleted | Self::Unreadable => PreimagePayload::absent(),
            Self::Unchanged { record } => PreimagePayload::from_record(record),
            Self::Untracked {
                observed,
                content_id,
            }
            | Self::Changed {
                observed,
                content_id,
            } => PreimagePayload::from_observed(observed, content_id.clone()),
            Self::ModeChanged { observed } => PreimagePayload::from_observed(observed, None),
        }
    }

    /// Build an ancestor row that adopts the remote identity while carrying the
    /// LOCAL fingerprint (the bytes are already on disk — no rewrite).
    pub(crate) fn record_from_observed(
        &self,
        entry: &ManifestEntry,
    ) -> Result<FileRecord, PullError> {
        let observed = match self {
            Self::Untracked { observed, .. } | Self::Changed { observed, .. } => observed,
            _ => {
                return Err(PullError::Internal {
                    reason: "adopt without local observation",
                });
            }
        };
        Ok(record_for_entry(entry, observed.fingerprint))
    }
}

pub(super) fn local_delta(
    ctx: &EngineContext,
    path: &WorkspacePath,
    ancestor: Option<&FileRecord>,
    verify_file_content: bool,
) -> Result<LocalDelta, PullError> {
    let observed = match observe_classified(&ctx.workspace_root, path) {
        ObserveOutcome::Present(observed) => Some(observed),
        ObserveOutcome::Absent => None,
        ObserveOutcome::Unsyncable(_) => return Ok(LocalDelta::Unreadable),
    };
    match (observed, ancestor) {
        (None, None) => Ok(LocalDelta::Absent),
        (None, Some(_)) => Ok(LocalDelta::Deleted),
        (Some(observed), None) => {
            let content_id = maybe_hash(ctx, path, &observed)?;
            Ok(LocalDelta::Untracked {
                observed,
                content_id,
            })
        }
        (Some(observed), Some(record)) => {
            local_vs_record(ctx, path, observed, record, verify_file_content)
        }
    }
}

pub(crate) fn local_vs_record(
    ctx: &EngineContext,
    path: &WorkspacePath,
    observed: Observed,
    record: &FileRecord,
    verify_file_content: bool,
) -> Result<LocalDelta, PullError> {
    if observed.kind != record.kind {
        let content_id = maybe_hash(ctx, path, &observed)?;
        return Ok(LocalDelta::Changed {
            observed,
            content_id,
        });
    }
    match observed.kind {
        EntryKind::Directory if observed.mode == record.mode => {
            return Ok(LocalDelta::Unchanged {
                record: record.clone(),
            });
        }
        EntryKind::Symlink
            if observed.mode == record.mode && observed.symlink_target == record.symlink_target =>
        {
            return Ok(LocalDelta::Unchanged {
                record: record.clone(),
            });
        }
        EntryKind::File
            if !verify_file_content
                && observed.fingerprint == record.fingerprint
                && observed.size == record.size
                && observed.mode == record.mode =>
        {
            return Ok(LocalDelta::Unchanged {
                record: record.clone(),
            });
        }
        EntryKind::Directory | EntryKind::File | EntryKind::Symlink => {}
    }
    // Ambiguous stat: hash to confirm before manufacturing a conflict.
    match observed.kind {
        EntryKind::File => {
            let bytes = match read_local_content(ctx, path, &observed.expected_file())? {
                LocalRead::Bytes(bytes) => bytes,
                // The leaf changed under us (symlink swap / replaced inode): it no
                // longer matches the record, so it is a Changed delta whose content
                // the next scan re-derives against the settled file.
                LocalRead::Unverifiable => {
                    return Ok(LocalDelta::Changed {
                        observed,
                        content_id: None,
                    });
                }
            };
            let matches_record = record.key_epoch.and_then(|key_epoch| {
                ctx.crypto
                    .content_id_at(key_epoch, &bytes)
                    .map(|content_id| Some(&content_id) == record.content_id.as_ref())
            });
            // Without the entry epoch's key the local bytes cannot be verified
            // against the ancestor identity, so treat this path as changed and let
            // the merge matrix preserve local content.
            if matches_record == Some(true) {
                if observed.mode == record.mode {
                    Ok(LocalDelta::Unchanged {
                        record: record.clone(),
                    })
                } else {
                    Ok(LocalDelta::ModeChanged { observed })
                }
            } else {
                Ok(LocalDelta::Changed {
                    observed,
                    content_id: Some(ctx.crypto.content_id(&bytes)),
                })
            }
        }
        EntryKind::Symlink => {
            if observed.symlink_target == record.symlink_target && observed.mode == record.mode {
                Ok(LocalDelta::Unchanged {
                    record: record.clone(),
                })
            } else {
                Ok(LocalDelta::Changed {
                    observed,
                    content_id: None,
                })
            }
        }
        EntryKind::Directory => Ok(LocalDelta::ModeChanged { observed }),
    }
}

/// The outcome of reading local bytes during a merge decision.
pub(crate) enum LocalRead {
    Bytes(Vec<u8>),
    /// The bytes could not be obtained — the leaf changed under us, or it is
    /// unreadable/oversize. Every caller treats it the same way: refuse to claim
    /// knowledge of this path's content. It is NEVER an engine fault; a single
    /// root-owned file used to end the workspace's sync here.
    Unverifiable,
}

/// Read local bytes for a decision that must not let one path's failure become
/// the cycle's failure. The SINGLE owner of that rule on the pull side — merge
/// classification, the preimage check, recovery's target match, and the
/// conflict-aside content probe all go through it; push has the mirror in
/// `read_rejection`.
///
/// Every read failure answers `Unverifiable`, including an errno the engine does
/// not model: refusing to claim knowledge of a path's content is always a legal
/// answer here (the caller keeps local), while propagating reaches
/// `CycleError::Fatal` and stops the workspace.
pub(crate) fn read_local_content(
    ctx: &EngineContext,
    path: &WorkspacePath,
    expected: &ExpectedFile,
) -> Result<LocalRead, PullError> {
    match super::super::fs_guard::read_file_bounded(
        &ctx.workspace_root,
        path,
        ctx.config.max_seal_bytes,
        expected,
    ) {
        Ok(FileRead::Bytes(bytes)) => Ok(LocalRead::Bytes(bytes)),
        Ok(FileRead::Diverged) => Ok(LocalRead::Unverifiable),
        Err(PushError::StreamSealUnsupported { .. }) | Err(PushError::Io(_)) => {
            Ok(LocalRead::Unverifiable)
        }
        Err(error) => Err(PullError::Push(error)),
    }
}

fn maybe_hash(
    ctx: &EngineContext,
    path: &WorkspacePath,
    observed: &Observed,
) -> Result<Option<ContentId>, PullError> {
    if observed.kind != EntryKind::File {
        return Ok(None);
    }
    match read_local_content(ctx, path, &observed.expected_file())? {
        LocalRead::Bytes(bytes) => Ok(Some(ctx.crypto.content_id(&bytes))),
        // The leaf is no longer the regular file we observed, or cannot be read:
        // its content id is unknown, so surface None rather than guessing.
        LocalRead::Unverifiable => Ok(None),
    }
}

pub(crate) enum RemoteDelta {
    Absent,
    Created(ManifestEntry),
    Unchanged,
    ModeChanged(ManifestEntry),
    Changed(ManifestEntry),
}

impl RemoteDelta {
    pub(super) fn requires_verified_local_content(&self) -> bool {
        !matches!(self, Self::Unchanged)
    }

    /// The remote entry this delta would put on disk, when it carries one.
    pub(super) fn entry(&self) -> Option<&ManifestEntry> {
        match self {
            Self::Created(entry) | Self::ModeChanged(entry) | Self::Changed(entry) => Some(entry),
            Self::Absent | Self::Unchanged => None,
        }
    }
}

pub(super) fn remote_delta(
    remote: Option<&ManifestEntry>,
    ancestor: Option<&FileRecord>,
) -> RemoteDelta {
    match (remote, ancestor) {
        (None, _) => RemoteDelta::Absent,
        (Some(entry), None) => RemoteDelta::Created(entry.clone()),
        (Some(entry), Some(record)) => {
            if entry_matches_record(entry, record) {
                if entry_mode(entry) == record.mode {
                    RemoteDelta::Unchanged
                } else {
                    RemoteDelta::ModeChanged(entry.clone())
                }
            } else {
                RemoteDelta::Changed(entry.clone())
            }
        }
    }
}
