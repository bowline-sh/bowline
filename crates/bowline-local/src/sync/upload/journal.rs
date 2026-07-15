use super::packs::prepared_segment_bytes;
use super::*;
use bowline_storage::{
    RecordRangeProofRequest, SourcePackUploadJournalObjectHash, SourcePackUploadJournalPointer,
    verify_record_range,
};

pub(super) fn append_journal_source_pack_pointers<C>(
    request: JournalPackPointerRequest<'_, C>,
) -> Result<(), UploadError>
where
    C: FnMut(&str, String) -> Result<(), UploadError>,
{
    for entry in request.entries {
        let pointer = &entry.pointer;
        if !request
            .included_pack_keys
            .insert(pointer.object_key.as_str().to_string())
        {
            continue;
        }
        let object_pointer = match request.control_plane.head_object_metadata(
            &request.candidate.base.workspace_id,
            pointer.object_key.as_str(),
        ) {
            Ok(metadata) => {
                validate_journal_committed_metadata(&metadata, pointer)?;
                object_pointer_from_metadata(
                    metadata,
                    pointer.pack_id.as_str(),
                    ObjectKind::SourcePack,
                )
            }
            Err(ControlPlaneError::ObjectMissing { .. }) => {
                let metadata = request.byte_store.head_object(&pointer.object_key)?;
                validate_journal_local_metadata(&metadata, pointer)?;
                request.control_plane.create_upload_intent(
                    UploadIntentRequest::new(
                        request.candidate.base.workspace_id.as_str(),
                        ObjectKind::SourcePack,
                        metadata.byte_len,
                    )
                    .with_object_key(pointer.object_key.as_str())
                    .with_content_id(pointer.pack_id.as_str()),
                )?;
                object_pointer_from_metadata(
                    metadata,
                    pointer.pack_id.as_str(),
                    ObjectKind::SourcePack,
                )
            }
            Err(error) => return Err(UploadError::ControlPlane(error)),
        };
        let committed =
            request
                .control_plane
                .commit_uploaded_object_metadata(ObjectMetadataCommit {
                    workspace_id: request.candidate.base.workspace_id.clone(),
                    object: object_pointer,
                    committed_by_device_id: request.candidate.device_id.clone(),
                })?;
        let object_pointer = object_pointer_from_metadata(
            committed,
            pointer.pack_id.as_str(),
            ObjectKind::SourcePack,
        );
        (request.checkpoint)(
            "source-pack-journal-reused",
            checkpoint_payload(&ObjectContentPayload {
                object_key: pointer.object_key.as_str(),
                content_id: pointer.pack_id.as_str(),
                byte_len: pointer.byte_len,
                hash: pointer.hash.as_str(),
            })?,
        )?;
        request.pack_pointers.push(object_pointer);
    }
    Ok(())
}

pub(super) fn source_pack_upload_journal_key(
    candidate: &SnapshotCandidate,
    key_epoch: u32,
) -> SourcePackUploadJournalKey {
    SourcePackUploadJournalKey::new(
        candidate.base.workspace_id.clone(),
        candidate.snapshot.manifest.snapshot_id.clone(),
        key_epoch,
        candidate
            .snapshot
            .prepared_content()
            .iter()
            .map(|(content_id, content)| (content_id.clone(), content.logical_len)),
    )
}

pub(super) fn verified_source_pack_upload_journal(
    candidate: &SnapshotCandidate,
    byte_store: &dyn ByteStore,
    storage_key: StorageKey,
    key_epoch: u32,
    journal_key: &SourcePackUploadJournalKey,
) -> Result<Vec<SourcePackUploadJournalEntry>, UploadError> {
    let mut entries = Vec::new();
    for entry in byte_store.source_pack_upload_journal(journal_key)? {
        if source_pack_journal_entry_is_verified(
            candidate,
            byte_store,
            storage_key,
            key_epoch,
            &entry,
        ) {
            entries.push(entry);
        }
    }
    Ok(entries)
}

pub(super) fn locators_by_journal_content(
    entries: &[SourcePackUploadJournalEntry],
) -> BTreeMap<ContentId, ContentLocator> {
    let mut locators = BTreeMap::new();
    for entry in entries {
        for locator in &entry.locators {
            locators.insert(locator.content_id.clone(), locator.clone());
        }
    }
    locators
}

pub(super) fn source_pack_journal_entry(
    pack: &PreparedSourcePack,
    metadata: &ObjectMetadata,
) -> SourcePackUploadJournalEntry {
    SourcePackUploadJournalEntry {
        pointer: SourcePackUploadJournalPointer {
            object_key: metadata.key.clone(),
            pack_id: pack.pack_id().clone(),
            byte_len: metadata.byte_len,
            hash: SourcePackUploadJournalObjectHash::from_stable_hash(metadata.hash.clone()),
            key_epoch: metadata.key_epoch,
            created_at_unix_ms: metadata.created_at_unix_ms,
        },
        locators: pack.locators().to_vec(),
    }
}

pub(super) fn source_pack_provisional_journal_entry(
    pack: &PreparedSourcePack,
    key_epoch: u32,
) -> SourcePackUploadJournalEntry {
    SourcePackUploadJournalEntry {
        pointer: SourcePackUploadJournalPointer {
            object_key: pack.object_key().clone(),
            pack_id: pack.pack_id().clone(),
            byte_len: pack.byte_len(),
            hash: SourcePackUploadJournalObjectHash::from_stable_hash(pack.hash()),
            key_epoch,
            created_at_unix_ms: 0,
        },
        locators: pack.locators().to_vec(),
    }
}

fn source_pack_journal_entry_is_verified(
    candidate: &SnapshotCandidate,
    byte_store: &dyn ByteStore,
    storage_key: StorageKey,
    key_epoch: u32,
    entry: &SourcePackUploadJournalEntry,
) -> bool {
    if entry.locators.is_empty()
        || entry.pointer.key_epoch != key_epoch
        || entry.pointer.created_at_unix_ms == 0
    {
        return false;
    }
    let Ok(metadata) = byte_store.head_object(&entry.pointer.object_key) else {
        return false;
    };
    if metadata.kind != StorageObjectKind::SourcePack
        || metadata.byte_len != entry.pointer.byte_len
        || metadata.hash != entry.pointer.hash.as_str()
        || metadata.key_epoch != entry.pointer.key_epoch
        || metadata.created_at_unix_ms != entry.pointer.created_at_unix_ms
    {
        return false;
    }
    entry.locators.iter().all(|locator| {
        locator.storage == ContentStorage::Packed
            && locator.pack_id.as_ref() == Some(&entry.pointer.pack_id)
            && locator.offset.is_some()
            && locator.length.is_some()
            && prepared_segment_bytes(
                candidate.snapshot.prepared_content(),
                &locator.content_id,
                locator.raw_size,
            )
            .ok()
            .flatten()
            .is_some_and(|bytes| {
                verify_record_range(
                    byte_store,
                    RecordRangeProofRequest {
                        object_key: &entry.pointer.object_key,
                        workspace_id: &candidate.base.workspace_id,
                        locator,
                        key: storage_key,
                        key_epoch,
                    },
                    &bytes,
                )
                .is_ok()
            })
    })
}

fn validate_journal_local_metadata(
    metadata: &ObjectMetadata,
    pointer: &SourcePackUploadJournalPointer,
) -> Result<(), UploadError> {
    if metadata.created_at_unix_ms == 0
        || metadata.key != pointer.object_key
        || metadata.kind != StorageObjectKind::SourcePack
        || metadata.byte_len != pointer.byte_len
        || metadata.hash != pointer.hash.as_str()
        || metadata.key_epoch != pointer.key_epoch
    {
        return Err(UploadError::ControlPlane(ControlPlaneError::Conflict {
            resource: "source pack journal",
            reason: "local metadata does not match source pack journal pointer",
        }));
    }
    Ok(())
}

fn validate_journal_committed_metadata(
    metadata: &ObjectMetadata,
    pointer: &SourcePackUploadJournalPointer,
) -> Result<(), UploadError> {
    if metadata.key != pointer.object_key
        || metadata.kind != StorageObjectKind::SourcePack
        || metadata.byte_len != pointer.byte_len
        || metadata.hash != pointer.hash.as_str()
        || metadata.key_epoch != pointer.key_epoch
    {
        return Err(UploadError::ControlPlane(ControlPlaneError::Conflict {
            resource: "source pack journal",
            reason: "committed metadata does not match source pack journal pointer",
        }));
    }
    Ok(())
}
