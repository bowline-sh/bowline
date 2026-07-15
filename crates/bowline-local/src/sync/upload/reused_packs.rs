use super::*;
use crate::sync::{
    SnapshotContent,
    metadata_sidecar::{metadata_direct_object_keys, metadata_sidecar_digest},
    namespace::{
        MetadataIdentityKey, MetadataRecordKind, PackLengthResolver, PageNamespaceBuilder,
        PageStore,
    },
};
use bowline_core::{
    namespace_snapshot::{
        EntryVisitor, NamespaceMutation, NamespaceOperationContext, NamespaceReadError,
        NamespaceSnapshotBuilder, NamespaceVisitControl,
    },
    workspace_graph::{NamespaceEntry, WorkspaceRelativePath},
};
use bowline_storage::{
    RecordRangeProofRequest, SealedSnapshotMetadataPage, SnapshotMetadataPagePointer,
    SnapshotMetadataRecordId, open_snapshot_metadata_page, seal_snapshot_metadata_page,
    verify_record_range,
};
use std::{
    collections::VecDeque,
    io::{self, Read as _},
    sync::Arc,
};

const METADATA_BINDING_BATCH: usize = 16;

pub(super) fn reused_record_count(
    snapshot: &SnapshotContent,
    reusable_layouts: &BTreeMap<ContentId, ContentLayout>,
) -> Result<usize, UploadError> {
    let mut reused = BTreeSet::new();
    visit_snapshot_entries(snapshot, &mut |entry| {
        if let Some(content_id) = entry.content_id.as_ref()
            && reusable_layouts.contains_key(content_id)
        {
            reused.insert(content_id.clone());
        }
    })?;
    Ok(reused.len())
}

pub(super) fn reused_pack_count(reusable_layouts: &BTreeMap<ContentId, ContentLayout>) -> usize {
    reusable_layouts
        .values()
        .flat_map(ContentLayout::segments)
        .map(|segment| &segment.pack_id)
        .collect::<BTreeSet<_>>()
        .len()
}

pub(super) fn build_bound_snapshot(
    candidate: &SnapshotCandidate,
    layouts_by_content: &BTreeMap<ContentId, ContentLayout>,
    pack_pointers: &[ObjectPointer],
) -> Result<SnapshotContent, UploadError> {
    let limit = candidate.snapshot.manifest().entry_count;
    let mut context = namespace_context(limit, limit);
    let mut builder =
        PageNamespaceBuilder::incremental(candidate.snapshot.namespace_snapshot(), &mut context)?
            .with_pack_length_resolver(Arc::new(BoundPackLengths::from_pointers(pack_pointers)));
    let mut updates = Vec::new();
    visit_snapshot_entries(&candidate.snapshot, &mut |entry| {
        let Some(content_id) = entry.content_id.as_ref() else {
            return;
        };
        let Some(layout) = layouts_by_content.get(content_id) else {
            return;
        };
        if entry.content_layout.as_ref() == Some(layout) {
            return;
        }
        let mut updated = entry.clone();
        updated.content_layout = Some(layout.clone());
        updated.hydration_state = HydrationState::Cold;
        updates.push(updated);
    })?;
    for entry in updates {
        builder.apply(NamespaceMutation::Upsert(entry), &mut context)?;
    }
    let namespace = builder.finish(&mut context)?;
    if namespace.snapshot_id != candidate.snapshot.manifest().snapshot_id {
        return Err(UploadError::ControlPlane(ControlPlaneError::Conflict {
            resource: "snapshot root",
            reason: "binding physical content layouts changed semantic snapshot identity",
        }));
    }
    Ok(SnapshotContent::from_built(
        namespace,
        candidate.snapshot.prepared_content().clone(),
    ))
}

struct BoundPackLengths(BTreeMap<PackId, u64>);

impl BoundPackLengths {
    fn from_pointers(pointers: &[ObjectPointer]) -> Self {
        Self(
            pointers
                .iter()
                .filter(|pointer| pointer.kind == ObjectKind::SourcePack)
                .map(|pointer| (PackId::new(pointer.content_id.as_str()), pointer.byte_len))
                .collect(),
        )
    }
}

impl PackLengthResolver for BoundPackLengths {
    fn pack_length(&self, pack_id: &PackId) -> Result<Option<u64>, NamespaceReadError> {
        Ok(self.0.get(pack_id).copied())
    }
}

pub(super) fn append_reused_source_pack_pointers<C>(
    mut request: ReusedPackPointerRequest<'_, C>,
) -> Result<(), UploadError>
where
    C: FnMut(&str, String) -> Result<(), UploadError>,
{
    let pack_ids = request
        .reusable_layouts
        .values()
        .flat_map(ContentLayout::segments)
        .map(|segment| segment.pack_id.clone())
        .collect::<BTreeSet<_>>();
    for pack_id in pack_ids {
        if !request
            .layouts_by_content
            .values()
            .flat_map(ContentLayout::segments)
            .any(|segment| segment.pack_id == pack_id)
        {
            continue;
        }
        let object_key = ObjectKey::from_pack_id(&pack_id)?;
        if !request
            .included_pack_keys
            .insert(object_key.as_str().to_string())
        {
            continue;
        }
        let metadata = match request
            .control_plane
            .head_object_metadata(&request.candidate.base.workspace_id, object_key.as_str())
        {
            Ok(metadata) => metadata,
            Err(ControlPlaneError::ObjectMissing { .. }) => {
                repack_reused_pack(&mut request, &pack_id)?;
                continue;
            }
            Err(error) => return Err(UploadError::ControlPlane(error)),
        };
        if metadata.kind != StorageObjectKind::SourcePack {
            return Err(UploadError::ControlPlane(ControlPlaneError::Conflict {
                resource: "source pack binding",
                reason: "content layout points at a non-source-pack object",
            }));
        }
        if reused_pack_needs_repack(
            request.candidate,
            request.byte_store,
            &object_key,
            request.reusable_layouts,
            &pack_id,
            request.storage_key,
            metadata.key_epoch,
        ) {
            repack_reused_pack(&mut request, &pack_id)?;
            continue;
        }
        let pointer =
            object_pointer_from_metadata(metadata, pack_id.as_str(), ObjectKind::SourcePack);
        let committed =
            request
                .control_plane
                .commit_uploaded_object_metadata(ObjectMetadataCommit {
                    workspace_id: request.candidate.base.workspace_id.clone(),
                    object: pointer,
                    committed_by_device_id: request.candidate.device_id.clone(),
                })?;
        (request.checkpoint)(
            "source-pack-reused",
            checkpoint_payload(&ObjectContentPayload {
                object_key: committed.key.as_str(),
                content_id: pack_id.as_str(),
                byte_len: committed.byte_len,
                hash: &committed.hash,
            })?,
        )?;
        request.pack_pointers.push(object_pointer_from_metadata(
            committed,
            pack_id.as_str(),
            ObjectKind::SourcePack,
        ));
    }
    Ok(())
}

fn repack_reused_pack<C>(
    request: &mut ReusedPackPointerRequest<'_, C>,
    pack_id: &PackId,
) -> Result<(), UploadError>
where
    C: FnMut(&str, String) -> Result<(), UploadError>,
{
    let replacement = repack_missing_reused_pack(
        request.candidate,
        request.reusable_layouts,
        pack_id,
        request.storage_key,
        request.key_epoch,
    )?;
    upload_source_packs(PackUploadRequest {
        candidate: request.candidate,
        control_plane: request.control_plane,
        byte_store: request.byte_store,
        key_epoch: request.key_epoch,
        packs: &replacement.packs,
        journal_key: Some(request.upload_journal_key),
        checkpoint_step: "source-pack-reuse-repacked",
        checkpoint: &mut *request.checkpoint,
        included_pack_keys: &mut *request.included_pack_keys,
        pack_pointers: &mut *request.pack_pointers,
    })?;
    request.layouts_by_content.extend(replacement.layouts);
    Ok(())
}

pub(super) fn commit_bound_snapshot<C>(
    candidate: &SnapshotCandidate,
    bound: &SnapshotContent,
    control_plane: &dyn ControlPlaneClient,
    byte_store: &dyn ByteStore,
    storage_key: StorageKey,
    key_epoch: u32,
    checkpoint: &mut C,
) -> Result<SnapshotRootRecord, UploadError>
where
    C: FnMut(&str, String) -> Result<(), UploadError>,
{
    let BindingPreparation {
        mut pending,
        mut bound_ids,
        metadata_records_resolved,
        mut metadata_records_fetched,
    } = prepare_metadata_bindings(candidate, bound, control_plane, byte_store, storage_key)?;
    let mut metadata_records_uploaded = 0_usize;
    let mut metadata_plaintext_bytes_uploaded = 0_u64;
    while !pending.is_empty() {
        let ready = pending
            .iter()
            .filter(|(_, record)| {
                record
                    .summary
                    .child_logical_ids
                    .iter()
                    .all(|child| bound_ids.contains(child))
            })
            .map(|(id, _)| id.clone())
            .take(METADATA_BINDING_BATCH)
            .collect::<Vec<_>>();
        if ready.is_empty() {
            return Err(NamespaceReadError::CorruptGraph {
                reason: "metadata binding graph contains a cycle or missing child",
            }
            .into());
        }
        let mut inputs = Vec::with_capacity(ready.len());
        for logical_id in &ready {
            let record = pending
                .get(logical_id)
                .ok_or(NamespaceReadError::MissingRecord {
                    record: "metadata upload record",
                })?;
            let sealed = seal_snapshot_metadata_page(
                &candidate.base.workspace_id,
                storage_record_id(record.summary.kind, logical_id),
                &record.plaintext,
                storage_key,
                key_epoch,
            )?;
            metadata_records_uploaded = metadata_records_uploaded.saturating_add(1);
            metadata_plaintext_bytes_uploaded =
                metadata_plaintext_bytes_uploaded.saturating_add(record.plaintext.len() as u64);
            let uploaded = ensure_uploaded_object(
                control_plane,
                byte_store,
                UploadObjectRequest {
                    workspace_id: &candidate.base.workspace_id,
                    storage_kind: StorageObjectKind::SnapshotMetadataPage,
                    key: sealed.pointer.object_key.clone(),
                    content_id: logical_id,
                    bytes: &sealed.bytes,
                    key_epoch,
                    device_id: Some(&candidate.device_id),
                    reusable_snapshot_manifest: None,
                },
            )?;
            if uploaded.wrote_object {
                #[cfg(feature = "fault-injection")]
                crate::sync::fault::trip(FaultPoint::AfterObjectUpload)?;
            }
            let object = object_pointer_from_metadata(
                uploaded.metadata,
                logical_id,
                ObjectKind::SnapshotMetadataPage,
            );
            let object = control_plane.commit_uploaded_object_metadata(ObjectMetadataCommit {
                workspace_id: candidate.base.workspace_id.clone(),
                object,
                committed_by_device_id: candidate.device_id.clone(),
            })?;
            let direct_object_keys = metadata_direct_object_keys(&record.summary)?;
            let digest = metadata_sidecar_digest(&record.summary, &direct_object_keys);
            inputs.push(MetadataBindingInput {
                logical_id: logical_id.clone(),
                record_kind: control_record_kind(record.summary.kind),
                object: object_pointer_from_metadata(
                    object,
                    logical_id,
                    ObjectKind::SnapshotMetadataPage,
                ),
                sidecar: MetadataSidecar {
                    child_logical_ids: record.summary.child_logical_ids.clone(),
                    direct_object_keys,
                    digest,
                },
            });
        }
        let committed = control_plane.commit_metadata_bindings(MetadataBindingCommit {
            workspace_id: candidate.base.workspace_id.clone(),
            bindings: inputs,
            committed_by_device_id: candidate.device_id.clone(),
        })?;
        for binding in committed.bindings {
            if binding.outcome == Some(MetadataBindingOutcome::ExistingWinner) {
                verify_binding(
                    &candidate.base.workspace_id,
                    candidate.snapshot.namespace_snapshot().store.identity_key(),
                    &binding,
                    byte_store,
                    storage_key,
                )?;
                metadata_records_fetched = metadata_records_fetched.saturating_add(1);
            }
            bound_ids.insert(binding.logical_id.clone());
            pending.remove(&binding.logical_id);
        }
    }

    let manifest = bound.manifest();
    let sealed = seal_snapshot_manifest(
        candidate.manifest_id.clone(),
        manifest,
        storage_key,
        key_epoch,
    )?;
    let upload = ensure_uploaded_object(
        control_plane,
        byte_store,
        UploadObjectRequest {
            workspace_id: &candidate.base.workspace_id,
            storage_kind: StorageObjectKind::SnapshotManifest,
            key: sealed.pointer.object_key.clone(),
            content_id: sealed.pointer.snapshot_id.as_str(),
            bytes: &sealed.bytes,
            key_epoch,
            device_id: Some(&candidate.device_id),
            reusable_snapshot_manifest: Some(ReusableSnapshotManifest {
                pointer: &sealed.pointer,
                manifest,
                storage_key,
            }),
        },
    )?;
    if upload.wrote_object {
        #[cfg(feature = "fault-injection")]
        crate::sync::fault::trip(FaultPoint::AfterObjectUpload)?;
    }
    let manifest_object = object_pointer_from_metadata(
        upload.metadata,
        sealed.pointer.snapshot_id.as_str(),
        ObjectKind::SnapshotManifest,
    );
    let manifest_object = control_plane.commit_uploaded_object_metadata(ObjectMetadataCommit {
        workspace_id: candidate.base.workspace_id.clone(),
        object: manifest_object,
        committed_by_device_id: candidate.device_id.clone(),
    })?;
    let root = control_plane.commit_snapshot_root(SnapshotRootCommit {
        workspace_id: candidate.base.workspace_id.clone(),
        snapshot_id: manifest.snapshot_id.clone(),
        manifest_id: candidate.manifest_id.clone(),
        manifest_object: object_pointer_from_metadata(
            manifest_object,
            manifest.snapshot_id.as_str(),
            ObjectKind::SnapshotManifest,
        ),
        namespace_root_id: manifest.namespace_root_id.as_str().to_string(),
        extra_root_logical_ids: Vec::new(),
        committed_by_device_id: candidate.device_id.clone(),
    })?;
    #[cfg(feature = "fault-injection")]
    crate::sync::fault::trip(FaultPoint::AfterManifestCommit)?;
    checkpoint(
        "snapshot-root-committed",
        checkpoint_payload(&SnapshotRootCommittedPayload {
            snapshot_id: manifest.snapshot_id.as_str(),
            manifest_id: &root.manifest_id,
            metadata_record_count: metadata_records_resolved,
            metadata_records_resolved,
            metadata_records_fetched,
            metadata_records_uploaded,
            metadata_plaintext_bytes_uploaded,
        })?,
    )?;
    Ok(root)
}

struct BindingPreparation {
    pending: BTreeMap<String, crate::sync::namespace::MetadataPlaintextRecord>,
    bound_ids: BTreeSet<String>,
    metadata_records_resolved: usize,
    metadata_records_fetched: usize,
}

fn prepare_metadata_bindings(
    candidate: &SnapshotCandidate,
    bound: &SnapshotContent,
    control_plane: &dyn ControlPlaneClient,
    byte_store: &dyn ByteStore,
    storage_key: StorageKey,
) -> Result<BindingPreparation, UploadError> {
    let store = bound.namespace_store();
    let root = bound.namespace_snapshot().namespace_root_id.as_str();
    let mut discovered = BTreeSet::from([root.to_string()]);
    let mut queue = VecDeque::from([root.to_string()]);
    let mut pending = BTreeMap::new();
    let mut bound_ids = BTreeSet::new();
    let mut metadata_records_resolved = 0_usize;
    let mut metadata_records_fetched = 0_usize;

    while !queue.is_empty() {
        let ids = (0..METADATA_BINDING_BATCH)
            .filter_map(|_| queue.pop_front())
            .collect::<Vec<_>>();
        metadata_records_resolved = metadata_records_resolved.saturating_add(ids.len());
        let resolved = control_plane
            .resolve_metadata_bindings(&candidate.base.workspace_id, &ids)?
            .bindings
            .into_iter()
            .map(|binding| (binding.logical_id.clone(), binding))
            .collect::<BTreeMap<_, _>>();
        for logical_id in ids {
            let record =
                store
                    .plaintext_record(&logical_id)?
                    .ok_or(NamespaceReadError::MissingRecord {
                        record: "metadata binding record",
                    })?;
            if let Some(binding) = resolved.get(&logical_id) {
                verify_binding_sidecar(&record.summary, binding)?;
                if store.record_is_new(&logical_id)? {
                    verify_binding(
                        &candidate.base.workspace_id,
                        candidate.snapshot.namespace_snapshot().store.identity_key(),
                        binding,
                        byte_store,
                        storage_key,
                    )?;
                    metadata_records_fetched = metadata_records_fetched.saturating_add(1);
                }
                bound_ids.insert(logical_id);
                continue;
            }
            for child in &record.summary.child_logical_ids {
                if discovered.insert(child.clone()) {
                    queue.push_back(child.clone());
                }
            }
            pending.insert(logical_id, record);
        }
    }
    Ok(BindingPreparation {
        pending,
        bound_ids,
        metadata_records_resolved,
        metadata_records_fetched,
    })
}

fn verify_binding_sidecar(
    summary: &crate::sync::namespace::MetadataRecordSummary,
    binding: &MetadataBindingRecord,
) -> Result<(), UploadError> {
    let direct_object_keys = metadata_direct_object_keys(summary)?;
    let digest = metadata_sidecar_digest(summary, &direct_object_keys);
    if summary.child_logical_ids != binding.sidecar.child_logical_ids
        || direct_object_keys != binding.sidecar.direct_object_keys
        || digest != binding.sidecar.digest
    {
        return Err(ControlPlaneError::Conflict {
            resource: "metadata binding sidecar",
            reason: "binding sidecar does not match trusted canonical plaintext",
        }
        .into());
    }
    Ok(())
}

pub(super) fn upload_source_packs<C>(request: PackUploadRequest<'_, C>) -> Result<(), UploadError>
where
    C: FnMut(&str, String) -> Result<(), UploadError>,
{
    for pack in request.packs {
        if let Some(journal_key) = request.journal_key {
            request.byte_store.record_source_pack_upload_journal(
                journal_key,
                &source_pack_provisional_journal_entry(pack, request.key_epoch),
            )?;
        }
        let upload = ensure_uploaded_source_pack(&request, pack)?;
        if let Some(journal_key) = request.journal_key {
            request.byte_store.record_source_pack_upload_journal(
                journal_key,
                &source_pack_journal_entry(pack, &upload.metadata),
            )?;
        }
        if upload.wrote_object {
            #[cfg(feature = "fault-injection")]
            crate::sync::fault::trip(FaultPoint::AfterObjectUpload)?;
        }
        let metadata = upload.metadata;
        let pointer =
            object_pointer_from_metadata(metadata, pack.pack_id().as_str(), ObjectKind::SourcePack);
        let committed =
            request
                .control_plane
                .commit_uploaded_object_metadata(ObjectMetadataCommit {
                    workspace_id: request.candidate.base.workspace_id.clone(),
                    object: pointer,
                    committed_by_device_id: request.candidate.device_id.clone(),
                })?;
        (request.checkpoint)(
            request.checkpoint_step,
            checkpoint_payload(&ObjectContentPayload {
                object_key: committed.key.as_str(),
                content_id: pack.pack_id().as_str(),
                byte_len: committed.byte_len,
                hash: &committed.hash,
            })?,
        )?;
        request
            .included_pack_keys
            .insert(committed.key.as_str().to_string());
        request.pack_pointers.push(object_pointer_from_metadata(
            committed,
            pack.pack_id().as_str(),
            ObjectKind::SourcePack,
        ));
    }
    Ok(())
}

pub(super) struct PackUploadRequest<'a, C>
where
    C: FnMut(&str, String) -> Result<(), UploadError>,
{
    pub(super) candidate: &'a SnapshotCandidate,
    pub(super) control_plane: &'a dyn ControlPlaneClient,
    pub(super) byte_store: &'a dyn ByteStore,
    pub(super) key_epoch: u32,
    pub(super) packs: &'a [PreparedSourcePack],
    pub(super) journal_key: Option<&'a SourcePackUploadJournalKey>,
    pub(super) checkpoint_step: &'static str,
    pub(super) checkpoint: &'a mut C,
    pub(super) included_pack_keys: &'a mut BTreeSet<String>,
    pub(super) pack_pointers: &'a mut Vec<ObjectPointer>,
}

pub(super) struct ReusedPackPointerRequest<'a, C>
where
    C: FnMut(&str, String) -> Result<(), UploadError>,
{
    pub(super) candidate: &'a SnapshotCandidate,
    pub(super) control_plane: &'a dyn ControlPlaneClient,
    pub(super) byte_store: &'a dyn ByteStore,
    pub(super) storage_key: StorageKey,
    pub(super) upload_journal_key: &'a SourcePackUploadJournalKey,
    pub(super) key_epoch: u32,
    pub(super) reusable_layouts: &'a BTreeMap<ContentId, ContentLayout>,
    pub(super) layouts_by_content: &'a mut BTreeMap<ContentId, ContentLayout>,
    pub(super) checkpoint: &'a mut C,
    pub(super) included_pack_keys: &'a mut BTreeSet<String>,
    pub(super) pack_pointers: &'a mut Vec<ObjectPointer>,
}

pub(super) struct JournalPackPointerRequest<'a, C>
where
    C: FnMut(&str, String) -> Result<(), UploadError>,
{
    pub(super) candidate: &'a SnapshotCandidate,
    pub(super) control_plane: &'a dyn ControlPlaneClient,
    pub(super) byte_store: &'a dyn ByteStore,
    pub(super) entries: &'a [SourcePackUploadJournalEntry],
    pub(super) checkpoint: &'a mut C,
    pub(super) included_pack_keys: &'a mut BTreeSet<String>,
    pub(super) pack_pointers: &'a mut Vec<ObjectPointer>,
}

fn visit_snapshot_entries(
    snapshot: &SnapshotContent,
    visitor: &mut dyn FnMut(&NamespaceEntry),
) -> Result<(), UploadError> {
    struct Adapter<'a>(&'a mut dyn FnMut(&NamespaceEntry));
    impl EntryVisitor for Adapter<'_> {
        fn visit(
            &mut self,
            entry: &NamespaceEntry,
            _context: &mut NamespaceOperationContext<'_>,
        ) -> Result<NamespaceVisitControl, NamespaceReadError> {
            (self.0)(entry);
            Ok(NamespaceVisitControl::Continue)
        }
    }
    let mut context = namespace_context(snapshot.manifest().entry_count, 0);
    snapshot.visit_prefix(
        &WorkspaceRelativePath::new(""),
        &mut context,
        &mut Adapter(visitor),
    )?;
    Ok(())
}

fn repack_missing_reused_pack(
    candidate: &SnapshotCandidate,
    layouts: &BTreeMap<ContentId, ContentLayout>,
    pack_id: &PackId,
    storage_key: StorageKey,
    key_epoch: u32,
) -> Result<packs::PreparedSegmentedSourcePacks, UploadError> {
    let mut content_ids = BTreeSet::new();
    visit_snapshot_entries(&candidate.snapshot, &mut |entry| {
        let Some(content_id) = entry.content_id.as_ref() else {
            return;
        };
        let Some(layout) = layouts.get(content_id) else {
            return;
        };
        if layout
            .segments()
            .iter()
            .any(|segment| &segment.pack_id == pack_id)
        {
            content_ids.insert(content_id.clone());
        }
    })?;
    let records = content_ids
        .iter()
        .map(|content_id| {
            let source = candidate
                .snapshot
                .prepared_content()
                .get(content_id)
                .ok_or_else(|| UploadError::ReusedPackMissing {
                    pack_id: pack_id.clone(),
                })?;
            Ok(PackRecordReader { content_id, source })
        })
        .collect::<Result<Vec<_>, UploadError>>()?;
    prepare_segmented_source_packs(
        candidate.base.workspace_id.clone(),
        &records,
        &BTreeMap::new(),
        SOURCE_PACK_TARGET_BYTES,
        storage_key,
        key_epoch,
    )
}

fn reused_pack_needs_repack(
    candidate: &SnapshotCandidate,
    byte_store: &dyn ByteStore,
    object_key: &ObjectKey,
    layouts: &BTreeMap<ContentId, ContentLayout>,
    pack_id: &PackId,
    storage_key: StorageKey,
    key_epoch: u32,
) -> bool {
    let mut needs_repack = false;
    if visit_snapshot_entries(&candidate.snapshot, &mut |entry| {
        if needs_repack {
            return;
        }
        let Some(content_id) = entry.content_id.as_ref() else {
            return;
        };
        let Some(layout) = layouts.get(content_id) else {
            return;
        };
        let Some(content) = candidate.snapshot.prepared_content().get(content_id) else {
            return;
        };
        let Ok(mut reader) = content.open() else {
            needs_repack = true;
            return;
        };
        for segment in layout.segments() {
            if &segment.pack_id != pack_id {
                if io::copy(
                    &mut reader.by_ref().take(segment.plaintext_length),
                    &mut io::sink(),
                )
                .ok()
                    != Some(segment.plaintext_length)
                {
                    needs_repack = true;
                    return;
                }
                continue;
            }
            let mut bytes = Vec::with_capacity(segment.plaintext_length as usize);
            if reader
                .by_ref()
                .take(segment.plaintext_length)
                .read_to_end(&mut bytes)
                .is_err()
                || bytes.len() as u64 != segment.plaintext_length
                || verify_record_range(
                    byte_store,
                    RecordRangeProofRequest {
                        object_key,
                        workspace_id: &candidate.base.workspace_id,
                        locator: &content_locator_for_segment(segment),
                        key: storage_key,
                        key_epoch,
                    },
                    &bytes,
                )
                .is_err()
            {
                needs_repack = true;
                return;
            }
        }
    })
    .is_err()
    {
        return true;
    }
    needs_repack
}

fn content_locator_for_segment(segment: &SegmentLocator) -> ContentLocator {
    ContentLocator {
        content_id: ContentId::new(segment.segment_id.as_str()),
        storage: ContentStorage::Packed,
        raw_size: segment.plaintext_length,
        pack_id: Some(segment.pack_id.clone()),
        offset: Some(segment.offset),
        length: Some(segment.length),
    }
}

fn namespace_context(entries: u64, mutations: u64) -> NamespaceOperationContext<'static> {
    NamespaceOperationContext::uncancelled(crate::sync::namespace::operation_budget(
        entries.saturating_mul(8),
        0,
        mutations,
    ))
}

fn storage_record_id(kind: MetadataRecordKind, id: &str) -> SnapshotMetadataRecordId {
    match kind {
        MetadataRecordKind::NamespacePage => {
            SnapshotMetadataRecordId::NamespacePage(bowline_core::ids::NamespacePageId::new(id))
        }
        MetadataRecordKind::ContentLayout => {
            SnapshotMetadataRecordId::ContentLayout(bowline_core::ids::ContentLayoutId::new(id))
        }
        MetadataRecordKind::SegmentPage => {
            SnapshotMetadataRecordId::SegmentPage(bowline_core::ids::SegmentPageId::new(id))
        }
    }
}

fn control_record_kind(kind: MetadataRecordKind) -> bowline_control_plane::MetadataRecordKind {
    match kind {
        MetadataRecordKind::NamespacePage => {
            bowline_control_plane::MetadataRecordKind::NamespacePage
        }
        MetadataRecordKind::ContentLayout => {
            bowline_control_plane::MetadataRecordKind::ContentLayout
        }
        MetadataRecordKind::SegmentPage => bowline_control_plane::MetadataRecordKind::SegmentPage,
    }
}

fn verify_binding(
    workspace_id: &WorkspaceId,
    identity_key: MetadataIdentityKey,
    binding: &MetadataBindingRecord,
    byte_store: &dyn ByteStore,
    storage_key: StorageKey,
) -> Result<(), UploadError> {
    if binding.object.content_id != binding.logical_id {
        return Err(ControlPlaneError::Conflict {
            resource: "metadata binding",
            reason: "metadata object content identity does not match its logical ID",
        }
        .into());
    }
    let object_key = ObjectKey::new(binding.object.object_key.clone())?;
    let bytes = byte_store.get_object(&object_key)?;
    let logical_id = match binding.record_kind {
        bowline_control_plane::MetadataRecordKind::NamespacePage => {
            SnapshotMetadataRecordId::NamespacePage(bowline_core::ids::NamespacePageId::new(
                &binding.logical_id,
            ))
        }
        bowline_control_plane::MetadataRecordKind::ContentLayout => {
            SnapshotMetadataRecordId::ContentLayout(bowline_core::ids::ContentLayoutId::new(
                &binding.logical_id,
            ))
        }
        bowline_control_plane::MetadataRecordKind::SegmentPage => {
            SnapshotMetadataRecordId::SegmentPage(bowline_core::ids::SegmentPageId::new(
                &binding.logical_id,
            ))
        }
    };
    let sealed = SealedSnapshotMetadataPage {
        pointer: SnapshotMetadataPagePointer {
            logical_id,
            object_key,
            byte_len: binding.object.byte_len,
            hash: binding.object.hash.clone(),
            key_epoch: binding.object.key_epoch,
            format_version: bowline_storage::SNAPSHOT_METADATA_PAGE_FORMAT_VERSION,
        },
        bytes,
    };
    let canonical = open_snapshot_metadata_page(&sealed, workspace_id, storage_key)?;
    let mut verified = PageStore::with_identity_key(identity_key);
    verified.insert_verified(
        match binding.record_kind {
            bowline_control_plane::MetadataRecordKind::NamespacePage => {
                MetadataRecordKind::NamespacePage
            }
            bowline_control_plane::MetadataRecordKind::ContentLayout => {
                MetadataRecordKind::ContentLayout
            }
            bowline_control_plane::MetadataRecordKind::SegmentPage => {
                MetadataRecordKind::SegmentPage
            }
        },
        &binding.logical_id,
        canonical,
    )?;
    let summary = verified.metadata_record(&binding.logical_id)?.ok_or(
        NamespaceReadError::MissingRecord {
            record: "verified metadata binding summary",
        },
    )?;
    let direct_object_keys = metadata_direct_object_keys(&summary)?;
    let digest = metadata_sidecar_digest(&summary, &direct_object_keys);
    if summary.child_logical_ids != binding.sidecar.child_logical_ids
        || direct_object_keys != binding.sidecar.direct_object_keys
        || digest != binding.sidecar.digest
    {
        return Err(ControlPlaneError::Conflict {
            resource: "metadata binding sidecar",
            reason: "winning binding sidecar does not match authenticated plaintext",
        }
        .into());
    }
    Ok(())
}
