use std::collections::{BTreeMap, BTreeSet};

use bowline_control_plane::{
    CompareAndSwapError, ControlPlaneClient, ControlPlaneError, ControlPlaneTimestamp,
    MetadataBindingCommit, MetadataBindingInput, MetadataBindingOutcome, MetadataBindingRecord,
    MetadataSidecar, ObjectKind, ObjectMetadataCommit, ObjectPointer, SnapshotRootCommit,
    SnapshotRootRecord, UploadIntentRequest, WorkspaceRef,
};
use bowline_core::{
    ids::{ContentId, DeviceId, PackId, WorkspaceId},
    workspace_graph::{
        ContentLayout, ContentLocator, ContentStorage, HydrationState, SegmentId, SegmentLocator,
    },
};
use bowline_storage::{
    ByteStore, ByteStoreError, EnvelopeContext, ManifestPointer, ObjectKey,
    ObjectKind as StorageObjectKind, ObjectMetadata, PackRecordReader,
    SourcePackUploadJournalEntry, SourcePackUploadJournalKey, StorageKey, seal,
    seal_snapshot_manifest, workspace_id_hash,
};

use super::UploadError;
use super::conflicts::{ConflictBundlePayload, conflict_bundle_object_id};
use super::import_snapshot_manifest;
use super::upload_payloads::{
    ObjectContentPayload, SnapshotManifestPayload, SnapshotRootCommittedPayload,
    SourcePacksWrittenPayload, WorkspaceRefAdvancedPayload, WorkspaceRefStalePayload,
};
use super::{ConflictFile, ConflictRecord, SnapshotCandidate, SnapshotContent};
use crate::sync::content_layout_map_from_snapshot;
#[cfg(feature = "fault-injection")]
use crate::sync::fault::FaultPoint;

const SOURCE_PACK_TARGET_BYTES: usize = 16 * 1024 * 1024;
const CONFLICT_BUNDLE_FORMAT_VERSION: u16 = 1;

mod journal;
mod objects;
mod packs;
mod reused_packs;

use journal::{
    append_journal_source_pack_pointers, locators_by_journal_content, source_pack_journal_entry,
    source_pack_provisional_journal_entry, source_pack_upload_journal_key,
    verified_source_pack_upload_journal,
};
use objects::{
    ReusableSnapshotManifest, UploadObjectRequest, ensure_uploaded_object,
    ensure_uploaded_source_pack,
};
#[cfg(test)]
use objects::{put_or_read_existing, validate_uploaded_metadata};
use packs::{PreparedSourcePack, prepare_segmented_source_packs};
use reused_packs::{
    JournalPackPointerRequest, PackUploadRequest, ReusedPackPointerRequest,
    append_reused_source_pack_pointers, build_bound_snapshot, commit_bound_snapshot,
    reused_pack_count, reused_record_count, upload_source_packs,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadOutcome {
    Advanced {
        workspace_ref: WorkspaceRef,
        snapshot_root: SnapshotRootRecord,
        bound_snapshot: Option<Box<SnapshotContent>>,
    },
    Stale {
        stale: bowline_control_plane::StaleWorkspaceRef,
        snapshot_root: SnapshotRootRecord,
        bound_snapshot: Option<Box<SnapshotContent>>,
    },
}

impl UploadOutcome {
    pub(crate) fn bound_snapshot(&self) -> Option<&SnapshotContent> {
        match self {
            Self::Advanced { bound_snapshot, .. } | Self::Stale { bound_snapshot, .. } => {
                bound_snapshot.as_deref()
            }
        }
    }
}

pub fn upload_snapshot_candidate(
    candidate: &SnapshotCandidate,
    control_plane: &dyn ControlPlaneClient,
    byte_store: &dyn ByteStore,
    storage_key: StorageKey,
    key_epoch: u32,
) -> Result<UploadOutcome, UploadError> {
    upload_snapshot_candidate_with_checkpoints(
        candidate,
        control_plane,
        byte_store,
        storage_key,
        key_epoch,
        |_, _| Ok(()),
    )
}

pub(crate) fn upload_conflict_bundle_object(
    request: UploadConflictBundleRequest<'_>,
) -> Result<ObjectPointer, UploadError> {
    let payload = ConflictBundlePayload {
        record: request.record.clone(),
        files: request.files.to_vec(),
    };
    let plaintext = serde_json::to_vec(&payload)?;
    let bundle_object_id = conflict_bundle_object_id(request.record);
    let object_key = ObjectKey::from_conflict_bundle_id(bundle_object_id.as_str())?;
    match request
        .control_plane
        .head_object_metadata(request.workspace_id, object_key.as_str())
    {
        Ok(metadata) => {
            if metadata.key != object_key
                || metadata.kind != StorageObjectKind::ConflictBundle
                || metadata.key_epoch != request.key_epoch
            {
                return Err(UploadError::ControlPlane(ControlPlaneError::Conflict {
                    resource: "conflict bundle object metadata",
                    reason: "existing conflict bundle object metadata does not match conflict upload",
                }));
            }
            return Ok(object_pointer_from_metadata(
                metadata,
                bundle_object_id.as_str(),
                ObjectKind::ConflictBundle,
            ));
        }
        Err(ControlPlaneError::ObjectMissing { .. }) => {}
        Err(error) => return Err(error.into()),
    }
    let sealed = seal(
        &plaintext,
        request.storage_key,
        &conflict_bundle_envelope_context(
            request.workspace_id,
            bundle_object_id.as_str(),
            request.key_epoch,
        ),
    )?
    .into_bytes();
    let upload = ensure_uploaded_object(
        request.control_plane,
        request.byte_store,
        UploadObjectRequest {
            workspace_id: request.workspace_id,
            storage_kind: StorageObjectKind::ConflictBundle,
            key: object_key,
            content_id: bundle_object_id.as_str(),
            bytes: &sealed,
            key_epoch: request.key_epoch,
            device_id: Some(request.device_id),
            reusable_snapshot_manifest: None,
        },
    )?;
    let pointer = object_pointer_from_metadata(
        upload.metadata,
        bundle_object_id.as_str(),
        ObjectKind::ConflictBundle,
    );
    let committed =
        request
            .control_plane
            .commit_uploaded_object_metadata(ObjectMetadataCommit {
                workspace_id: request.workspace_id.clone(),
                object: pointer,
                committed_by_device_id: request.device_id.clone(),
            })?;
    Ok(object_pointer_from_metadata(
        committed,
        bundle_object_id.as_str(),
        ObjectKind::ConflictBundle,
    ))
}

pub(crate) struct UploadConflictBundleRequest<'a> {
    pub(crate) record: &'a ConflictRecord,
    pub(crate) files: &'a [ConflictFile],
    pub(crate) workspace_id: &'a WorkspaceId,
    pub(crate) device_id: &'a DeviceId,
    pub(crate) control_plane: &'a dyn ControlPlaneClient,
    pub(crate) byte_store: &'a dyn ByteStore,
    pub(crate) storage_key: StorageKey,
    pub(crate) key_epoch: u32,
}

pub(crate) fn conflict_bundle_envelope_context(
    workspace_id: &WorkspaceId,
    conflict_id: &str,
    key_epoch: u32,
) -> EnvelopeContext {
    EnvelopeContext {
        workspace_id_hash: workspace_id_hash(workspace_id.as_str()),
        object_kind: StorageObjectKind::ConflictBundle,
        object_id: conflict_id.to_string(),
        record_id: conflict_id.to_string(),
        key_epoch,
        format_version: CONFLICT_BUNDLE_FORMAT_VERSION,
    }
}

pub fn upload_snapshot_candidate_with_checkpoints(
    candidate: &SnapshotCandidate,
    control_plane: &dyn ControlPlaneClient,
    byte_store: &dyn ByteStore,
    storage_key: StorageKey,
    key_epoch: u32,
    mut checkpoint: impl FnMut(&str, String) -> Result<(), UploadError>,
) -> Result<UploadOutcome, UploadError> {
    if let Some(snapshot_root) = control_plane.get_snapshot_root(
        &candidate.base.workspace_id,
        &candidate.snapshot.manifest.snapshot_id,
    )? {
        if snapshot_root.manifest_id != candidate.manifest_id || !snapshot_root.complete {
            return Err(UploadError::ControlPlane(ControlPlaneError::Conflict {
                resource: "snapshot root",
                reason: "snapshot is already committed with a different manifest ID",
            }));
        }
        checkpoint(
            "snapshot-root-reused",
            checkpoint_payload(&SnapshotManifestPayload {
                snapshot_id: candidate.snapshot.manifest.snapshot_id.as_str(),
                manifest_id: &snapshot_root.manifest_id,
            })?,
        )?;
        let imported = import_snapshot_manifest(
            &candidate.base.workspace_id,
            &snapshot_root,
            control_plane,
            byte_store,
            storage_key,
            candidate.snapshot.namespace_snapshot().store.identity_key(),
        )?;
        return finish_upload(
            candidate,
            control_plane,
            snapshot_root,
            Some(Box::new(imported.snapshot)),
            &mut checkpoint,
        );
    }

    let reusable_layouts = content_layout_map_from_snapshot(&candidate.snapshot)?;
    let upload_journal_key = source_pack_upload_journal_key(candidate, key_epoch);
    let journal_entries = verified_source_pack_upload_journal(
        candidate,
        byte_store,
        storage_key,
        key_epoch,
        &upload_journal_key,
    )?;
    let journal_locators = locators_by_journal_content(&journal_entries);
    let reused_record_count = reused_record_count(&candidate.snapshot, &reusable_layouts)?;
    let reused_pack_count = reused_pack_count(&reusable_layouts);
    let resident_content_bytes = candidate
        .snapshot
        .prepared_content()
        .values()
        .filter_map(|content| content.resident_bytes().map(|bytes| bytes.len() as u64))
        .sum::<u64>();
    let prepared_content_bytes = candidate
        .snapshot
        .prepared_content()
        .values()
        .map(|content| content.logical_len)
        .sum::<u64>();
    let staged_content_bytes = candidate
        .snapshot
        .prepared_content()
        .values()
        .filter(|content| {
            matches!(
                &content.source,
                super::PreparedContentSource::StagedFile { .. }
            )
        })
        .map(|content| content.logical_len)
        .sum::<u64>();
    let largest_content_bytes = candidate
        .snapshot
        .prepared_content()
        .values()
        .map(|content| content.logical_len)
        .max()
        .unwrap_or_default();
    let pending_files = candidate
        .snapshot
        .prepared_content()
        .iter()
        .filter(|(content_id, _)| !reusable_layouts.contains_key(*content_id))
        .collect::<Vec<_>>();
    let pack_readers = pending_files
        .iter()
        .map(|(content_id, content)| PackRecordReader {
            content_id,
            source: *content,
        })
        .collect::<Vec<_>>();
    let prepared = prepare_segmented_source_packs(
        candidate.snapshot.manifest.workspace_id.clone(),
        &pack_readers,
        &journal_locators,
        SOURCE_PACK_TARGET_BYTES,
        storage_key,
        key_epoch,
    )?;
    let newly_packed_segments = prepared
        .packs
        .iter()
        .flat_map(|pack| pack.locators())
        .map(|locator| locator.content_id.as_str())
        .collect::<BTreeSet<_>>();
    let record_count = prepared
        .layouts
        .values()
        .filter(|layout| {
            layout
                .segments()
                .iter()
                .any(|segment| newly_packed_segments.contains(segment.segment_id.as_str()))
        })
        .count();
    let packed_input_bytes = prepared
        .packs
        .iter()
        .flat_map(|pack| pack.locators())
        .map(|locator| locator.raw_size)
        .sum::<u64>();
    checkpoint(
        "source-packs-written",
        checkpoint_payload(&SourcePacksWrittenPayload {
            snapshot_id: candidate.snapshot.manifest.snapshot_id.as_str(),
            preparation_lease_id: candidate
                .snapshot
                .preparation_lease()
                .map(|lease| lease.id.as_str()),
            pack_count: prepared.packs.len(),
            record_count,
            reused_record_count,
            reused_pack_count,
            resident_content_bytes,
            prepared_content_bytes,
            staged_content_bytes,
            largest_content_bytes,
            packed_input_bytes,
        })?,
    )?;

    let mut pack_pointers = Vec::new();
    let mut included_pack_keys = BTreeSet::<String>::new();
    upload_source_packs(PackUploadRequest {
        candidate,
        control_plane,
        byte_store,
        key_epoch,
        packs: &prepared.packs,
        journal_key: Some(&upload_journal_key),
        checkpoint_step: "source-pack-uploaded",
        checkpoint: &mut checkpoint,
        included_pack_keys: &mut included_pack_keys,
        pack_pointers: &mut pack_pointers,
    })?;
    append_journal_source_pack_pointers(JournalPackPointerRequest {
        candidate,
        control_plane,
        byte_store,
        entries: &journal_entries,
        checkpoint: &mut checkpoint,
        included_pack_keys: &mut included_pack_keys,
        pack_pointers: &mut pack_pointers,
    })?;
    let mut layouts_by_content = reusable_layouts.clone();
    layouts_by_content.extend(prepared.layouts);
    append_reused_source_pack_pointers(ReusedPackPointerRequest {
        candidate,
        control_plane,
        byte_store,
        storage_key,
        upload_journal_key: &upload_journal_key,
        key_epoch,
        reusable_layouts: &reusable_layouts,
        layouts_by_content: &mut layouts_by_content,
        checkpoint: &mut checkpoint,
        included_pack_keys: &mut included_pack_keys,
        pack_pointers: &mut pack_pointers,
    })?;
    let bound_snapshot = build_bound_snapshot(candidate, &layouts_by_content, &pack_pointers)?;

    let snapshot_root = commit_bound_snapshot(
        candidate,
        &bound_snapshot,
        control_plane,
        byte_store,
        storage_key,
        key_epoch,
        &mut checkpoint,
    )?;

    finish_upload(
        candidate,
        control_plane,
        snapshot_root,
        Some(Box::new(bound_snapshot)),
        &mut checkpoint,
    )
}

fn checkpoint_payload<T: serde::Serialize>(payload: &T) -> Result<String, UploadError> {
    serde_json::to_string(payload).map_err(|error| UploadError::Checkpoint(error.to_string()))
}

fn object_pointer_from_metadata(
    metadata: ObjectMetadata,
    content_id: &str,
    kind: ObjectKind,
) -> ObjectPointer {
    ObjectPointer {
        object_key: metadata.key.as_str().to_string(),
        content_id: ContentId::new(content_id),
        byte_len: metadata.byte_len,
        hash: metadata.hash,
        key_epoch: metadata.key_epoch,
        kind,
        created_at: ControlPlaneTimestamp {
            tick: metadata.created_at_unix_ms,
        },
    }
}

fn finish_upload(
    candidate: &SnapshotCandidate,
    control_plane: &dyn ControlPlaneClient,
    snapshot_root: SnapshotRootRecord,
    bound_snapshot: Option<Box<SnapshotContent>>,
    checkpoint: &mut impl FnMut(&str, String) -> Result<(), UploadError>,
) -> Result<UploadOutcome, UploadError> {
    checkpoint(
        "workspace-ref-cas-authorized",
        checkpoint_payload(&serde_json::json!({
            "snapshotId": candidate.snapshot.manifest.snapshot_id.as_str(),
            "version": candidate.base.version,
        }))?,
    )?;
    match control_plane.compare_and_swap_workspace_ref_for_project(
        &candidate.base.workspace_id,
        candidate.base.version,
        &candidate.snapshot.manifest.snapshot_id,
        &candidate.device_id,
        candidate.snapshot.manifest.project_id.as_ref(),
    ) {
        Ok(workspace_ref) => {
            checkpoint(
                "workspace-ref-advanced",
                checkpoint_payload(&WorkspaceRefAdvancedPayload {
                    snapshot_id: &workspace_ref.snapshot_id,
                    version: workspace_ref.version,
                })?,
            )?;
            // The CAS is already externally visible. Publish its durable checkpoint
            // before the crash seam so every later failure is reconciled rather than
            // being mistaken for a pre-commit cancellation.
            #[cfg(feature = "fault-injection")]
            crate::sync::fault::trip(FaultPoint::AfterRefCas)?;
            Ok(UploadOutcome::Advanced {
                workspace_ref,
                snapshot_root,
                bound_snapshot,
            })
        }
        Err(CompareAndSwapError::StaleRef(stale)) => {
            checkpoint(
                "workspace-ref-stale",
                checkpoint_payload(&WorkspaceRefStalePayload {
                    attempted_snapshot_id: candidate.snapshot.manifest.snapshot_id.as_str(),
                    current_snapshot_id: &stale.current.snapshot_id,
                    current_version: stale.current.version,
                })?,
            )?;
            Ok(UploadOutcome::Stale {
                stale,
                snapshot_root,
                bound_snapshot,
            })
        }
        Err(error) => Err(UploadError::CompareAndSwap(error)),
    }
}

#[cfg(test)]
#[path = "upload_reuse_tests.rs"]
mod reuse_tests;
#[cfg(test)]
#[path = "upload_tests.rs"]
mod tests;
