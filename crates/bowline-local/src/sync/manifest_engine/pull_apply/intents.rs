//! Serde payloads for the intent journal plus the recovery observation helpers
//! (Plan 109 Step 5). The store persists `expected_preimage` / `target_record`
//! as opaque `Option<String>` columns; these typed structs are the single owner
//! of their schema, JSON-encoded via serde (never hand-assembled).

use bowline_core::ids::ContentId;
use serde::{Deserialize, Serialize};

use super::apply::{RecoveryObservation, preimage_matches};
use super::{FsOp, FsOpKind, PullError};
use super::{entry_mode, record_for_entry};
use crate::sync::manifest_engine::fs_guard::{FileRead, Observed, read_file_bounded};
use crate::sync::manifest_engine::manifest::{
    BlobKey, EntryKind, FileMode, KeyEpoch, ManifestEntry, WorkspacePath,
};
use crate::sync::manifest_engine::push::EngineContext;
use crate::sync::manifest_engine::store::{
    FileRecord, Intent, IntentOperationKind, StatFingerprint,
};

// ---- intent payloads (serde; opaque to the store) ---------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PreimagePayload {
    pub(crate) present: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub(crate) kind: Option<EntryKind>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub(crate) mode: Option<FileMode>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub(crate) content_id: Option<ContentId>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub(crate) symlink_target: Option<String>,
}

impl PreimagePayload {
    pub(crate) fn absent() -> Self {
        Self {
            present: false,
            kind: None,
            mode: None,
            content_id: None,
            symlink_target: None,
        }
    }

    pub(crate) fn from_record(record: &FileRecord) -> Self {
        Self {
            present: true,
            kind: Some(record.kind),
            mode: Some(record.mode),
            content_id: record.content_id.clone(),
            symlink_target: record.symlink_target.clone(),
        }
    }

    pub(crate) fn from_observed(observed: &Observed, content_id: Option<ContentId>) -> Self {
        Self {
            present: true,
            kind: Some(observed.kind),
            mode: Some(observed.mode),
            content_id,
            symlink_target: observed.symlink_target.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum IntentOpTag {
    Install,
    Delete,
    ModeChange,
    ConflictAside,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TargetRecordPayload {
    pub(crate) op: IntentOpTag,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    kind: Option<EntryKind>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    mode: Option<FileMode>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    content_id: Option<ContentId>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    blob_key: Option<BlobKey>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    key_epoch: Option<KeyEpoch>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    symlink_target: Option<String>,
}

impl TargetRecordPayload {
    pub(crate) fn to_entry(&self) -> Result<ManifestEntry, PullError> {
        match self.kind {
            Some(EntryKind::File) => Ok(ManifestEntry::File {
                size: self.size.unwrap_or_default(),
                mode: self.mode.unwrap_or(FileMode::new(0o644)),
                content_id: self.content_id.clone().ok_or(missing("content_id"))?,
                blob_key: self.blob_key.clone().ok_or(missing("blob_key"))?,
                key_epoch: self.key_epoch.ok_or(missing("key_epoch"))?,
            }),
            Some(EntryKind::Directory) => Ok(ManifestEntry::Directory {
                mode: self.mode.unwrap_or(FileMode::new(0o755)),
            }),
            Some(EntryKind::Symlink) => Ok(ManifestEntry::Symlink {
                mode: self.mode.unwrap_or(FileMode::new(0o777)),
                target: self
                    .symlink_target
                    .clone()
                    .ok_or(missing("symlink_target"))?,
            }),
            None => Err(missing("kind")),
        }
    }

    pub(crate) fn to_record(&self, fingerprint: StatFingerprint) -> Result<FileRecord, PullError> {
        Ok(record_for_entry(&self.to_entry()?, fingerprint))
    }
}

pub(crate) fn missing(field: &'static str) -> PullError {
    PullError::Internal { reason: field }
}

pub(crate) fn target_payload(op: &FsOp) -> (IntentOperationKind, TargetRecordPayload) {
    match &op.kind {
        FsOpKind::Install(entry) => (
            IntentOperationKind::Install,
            entry_to_target(IntentOpTag::Install, entry),
        ),
        FsOpKind::ConflictAside(entry) => (
            IntentOperationKind::ConflictAside,
            entry_to_target(IntentOpTag::ConflictAside, entry),
        ),
        FsOpKind::Delete => (
            IntentOperationKind::Delete,
            TargetRecordPayload {
                op: IntentOpTag::Delete,
                kind: None,
                size: None,
                mode: None,
                content_id: None,
                blob_key: None,
                key_epoch: None,
                symlink_target: None,
            },
        ),
        FsOpKind::ModeChange(entry) => (
            IntentOperationKind::ModeChange,
            // Carry the full entry (content identity included) so crash recovery
            // rebuilds a complete ancestor row, not a content-less mode stub.
            entry_to_target(IntentOpTag::ModeChange, entry),
        ),
    }
}

pub(crate) fn entry_to_target(op: IntentOpTag, entry: &ManifestEntry) -> TargetRecordPayload {
    let mut payload = TargetRecordPayload {
        op,
        kind: Some(entry.kind()),
        size: None,
        mode: Some(entry_mode(entry)),
        content_id: None,
        blob_key: None,
        key_epoch: None,
        symlink_target: None,
    };
    match entry {
        ManifestEntry::File {
            size,
            content_id,
            blob_key,
            key_epoch,
            ..
        } => {
            payload.size = Some(*size);
            payload.content_id = Some(content_id.clone());
            payload.blob_key = Some(blob_key.clone());
            payload.key_epoch = Some(*key_epoch);
        }
        ManifestEntry::Directory { .. } => {}
        ManifestEntry::Symlink { target, .. } => payload.symlink_target = Some(target.clone()),
    }
    payload
}

pub(crate) fn recovery_facts(
    ctx: &EngineContext,
    path: &WorkspacePath,
    target: &TargetRecordPayload,
    preimage: &PreimagePayload,
    observed: Option<&Observed>,
    intent: &Intent,
) -> Result<RecoveryObservation, PullError> {
    let target_matches_target_record = target_matches(ctx, path, target, observed)?;
    let target_matches_preimage = preimage_matches(ctx, path, preimage, observed)?;
    let temp_exists = intent
        .temp_name
        .as_ref()
        .map(|name| ctx.engine_dir().join("tmp").join(name).exists())
        .unwrap_or(false);
    let quarantine_exists = intent
        .preserved_preimage
        .as_ref()
        .map(|rel| ctx.engine_dir().join(rel).exists())
        .unwrap_or(false);
    Ok(RecoveryObservation {
        target_present: observed.is_some(),
        target_matches_target_record,
        target_matches_preimage,
        temp_exists,
        quarantine_exists,
    })
}

pub(crate) fn target_matches(
    ctx: &EngineContext,
    path: &WorkspacePath,
    target: &TargetRecordPayload,
    observed: Option<&Observed>,
) -> Result<bool, PullError> {
    let Some(observed) = observed else {
        return Ok(matches!(target.op, IntentOpTag::Delete));
    };
    // A mode change shares the target's bytes/identity and differs only in mode,
    // so for that op the mode must also match — otherwise a crash before the
    // chmod (bytes already equal) is misread as complete and recovery finalizes
    // the stale mode. Install/aside keep their historical identity-only match.
    let mode_must_match = matches!(target.op, IntentOpTag::ModeChange);
    if mode_must_match && target.mode.is_some_and(|mode| observed.mode != mode) {
        return Ok(false);
    }
    match target.kind {
        Some(EntryKind::File) => {
            if observed.kind != EntryKind::File {
                return Ok(false);
            }
            // Read no-follow against the observed fingerprint; a leaf raced into a
            // symlink diverges and cannot match the recovery target record.
            match read_file_bounded(
                &ctx.workspace_root,
                path,
                ctx.config.max_seal_bytes,
                &observed.expected_file(),
            )
            .map_err(PullError::Push)?
            {
                FileRead::Bytes(bytes) => {
                    Ok(Some(ctx.crypto.content_id(&bytes)) == target.content_id)
                }
                FileRead::Diverged => Ok(false),
            }
        }
        Some(EntryKind::Symlink) => {
            Ok(observed.kind == EntryKind::Symlink
                && observed.symlink_target == target.symlink_target)
        }
        Some(EntryKind::Directory) => Ok(observed.kind == EntryKind::Directory),
        None => Ok(false),
    }
}

pub(crate) fn encode<T: Serialize>(value: &T) -> String {
    // Payloads are small typed structs; serialization cannot fail in practice,
    // but never unwrap — fall back to an empty object the decoder rejects.
    serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string())
}

pub(crate) fn decode<T: for<'de> Deserialize<'de>>(value: &str) -> Result<T, PullError> {
    serde_json::from_str(value).map_err(|_| PullError::Internal {
        reason: "intent decode failed",
    })
}
