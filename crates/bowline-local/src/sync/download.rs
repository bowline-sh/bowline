use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    error::Error,
    fmt,
    sync::{Arc, Mutex},
};

use bowline_control_plane::{
    ControlPlaneClient, ControlPlaneError, DownloadIntentRequest, MetadataBindingRecord,
    MetadataRecordKind as ControlMetadataRecordKind, ObjectKind as ControlObjectKind,
    ObjectPointer, SnapshotRootRecord,
};
use bowline_core::{
    ids::{NamespacePageId, SnapshotId, WorkspaceId},
    namespace_snapshot::{
        EntryVisitor, NamespaceOperationBudget, NamespaceOperationContext, NamespaceReadError,
        NamespaceSnapshotReader, NamespaceVisitControl, SnapshotMetadata,
    },
    workspace_graph::{
        ContentLayout, ContentLocator, ContentStorage, NamespaceEntry, NamespaceEntryKind,
        SNAPSHOT_SCHEMA_VERSION, SegmentLocator, SnapshotManifest, WorkspaceRelativePath,
        is_safe_workspace_symlink_target, normalize_workspace_path,
    },
};
use bowline_storage::{
    ByteStore, ByteStoreError, ManifestPointer, ManifestPointerKind, MetadataPageError, ObjectKey,
    SNAPSHOT_METADATA_PAGE_MAX_CANONICAL_BYTES, SNAPSHOT_METADATA_PAGE_MAX_SEALED_BYTES,
    SealedSnapshotManifest, SealedSnapshotMetadataPage, SnapshotMetadataPagePointer,
    SnapshotMetadataRecordId, StorageKey, open_snapshot_manifest, open_snapshot_metadata_page,
};

use super::paths::{case_fold_path_component, validate_case_folded_prefixes};
use super::{SnapshotContent, content_layout_map_from_snapshot};
use crate::sync::metadata_sidecar::{metadata_direct_object_keys, metadata_sidecar_digest};
use crate::sync::namespace::{
    BuiltPagedNamespaceSnapshot, MAX_SEGMENTS_PER_LAYOUT, MetadataIdentityKey, MetadataRecordKind,
    PageNamespaceReader, PageStore, PagedRecordSource,
};

const MAX_IMPORTED_METADATA_RECORDS: usize = 1_000_000;
const MAX_IMPORTED_METADATA_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const REMOTE_METADATA_CACHE_MAX_RECORDS: usize = MAX_IMPORTED_METADATA_RECORDS;
const REMOTE_METADATA_CACHE_MAX_BYTES: u64 = MAX_IMPORTED_METADATA_BYTES;
const MAX_METADATA_BINDINGS_PER_BATCH: usize = 16;
type MetadataCacheKey = (MetadataRecordKind, String);
type CachedMetadataBindings = BTreeMap<(MetadataRecordKind, String), MetadataBindingRecord>;

struct VerifiedMetadataCache {
    records: BTreeMap<MetadataCacheKey, Arc<[u8]>>,
    admission_order: VecDeque<MetadataCacheKey>,
    encoded_bytes: u64,
    maximum_records: usize,
    maximum_bytes: u64,
}

impl VerifiedMetadataCache {
    fn production() -> Self {
        Self::with_limits(
            REMOTE_METADATA_CACHE_MAX_RECORDS,
            REMOTE_METADATA_CACHE_MAX_BYTES,
        )
    }

    fn with_limits(maximum_records: usize, maximum_bytes: u64) -> Self {
        Self {
            records: BTreeMap::new(),
            admission_order: VecDeque::new(),
            encoded_bytes: 0,
            maximum_records,
            maximum_bytes,
        }
    }

    fn get(&self, key: &MetadataCacheKey) -> Option<Arc<[u8]>> {
        self.records.get(key).cloned()
    }

    fn contains_key(&self, key: &MetadataCacheKey) -> bool {
        self.records.contains_key(key)
    }

    fn insert(
        &mut self,
        key: MetadataCacheKey,
        bytes: Arc<[u8]>,
    ) -> Result<(), NamespaceReadError> {
        if self.records.contains_key(&key) {
            return Ok(());
        }
        let encoded_bytes = bytes.len() as u64;
        if self.maximum_records == 0 || encoded_bytes > self.maximum_bytes {
            return Err(NamespaceReadError::OversizedRecord {
                record: "remote metadata cache entry",
                encoded_bytes,
                maximum_bytes: self.maximum_bytes,
            });
        }
        while self.records.len() >= self.maximum_records
            || self.encoded_bytes.saturating_add(encoded_bytes) > self.maximum_bytes
        {
            let oldest =
                self.admission_order
                    .pop_front()
                    .ok_or(NamespaceReadError::CorruptGraph {
                        reason: "remote metadata cache accounting is inconsistent",
                    })?;
            let evicted = self
                .records
                .remove(&oldest)
                .ok_or(NamespaceReadError::CorruptGraph {
                    reason: "remote metadata cache eviction key is missing",
                })?;
            self.encoded_bytes = self.encoded_bytes.saturating_sub(evicted.len() as u64);
        }
        self.encoded_bytes = self.encoded_bytes.saturating_add(encoded_bytes);
        self.admission_order.push_back(key.clone());
        self.records.insert(key, bytes);
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedSnapshot {
    pub snapshot: SnapshotContent,
    pub locators: Vec<ContentLocator>,
    pub pack_pointers: Vec<ObjectPointer>,
}

pub struct RemoteNamespaceSnapshot<'a> {
    metadata: SnapshotMetadata,
    root: NamespacePageId,
    source: RemotePageSource<'a>,
}

impl RemoteNamespaceSnapshot<'_> {
    pub fn reader(&self) -> PageNamespaceReader<'_> {
        PageNamespaceReader::from_source(&self.metadata, &self.root, &self.source)
    }
}

struct RemotePageSource<'a> {
    workspace_id: &'a WorkspaceId,
    identity_key: MetadataIdentityKey,
    control_plane: &'a dyn ControlPlaneClient,
    byte_store: &'a dyn ByteStore,
    storage_key: StorageKey,
    verified: Mutex<VerifiedMetadataCache>,
    bindings: Mutex<CachedMetadataBindings>,
}

impl RemotePageSource<'_> {
    fn materialized_store(&self) -> Result<PageStore, DownloadError> {
        let records = self.verified.lock().map_err(|_| remote_corrupt())?;
        let mut store = PageStore::with_identity_key(self.identity_key);
        for ((kind, logical_id), bytes) in &records.records {
            store.insert_verified(*kind, logical_id, bytes.to_vec())?;
        }
        Ok(store)
    }
}

pub fn import_snapshot_by_id(
    workspace_id: &WorkspaceId,
    snapshot_id: &SnapshotId,
    control_plane: &dyn ControlPlaneClient,
    byte_store: &dyn ByteStore,
    storage_key: StorageKey,
    identity_key: MetadataIdentityKey,
) -> Result<ImportedSnapshot, DownloadError> {
    import_snapshot_by_id_with_checkpoints(
        workspace_id,
        snapshot_id,
        control_plane,
        byte_store,
        storage_key,
        identity_key,
        &mut || Ok(()),
    )
}

pub(crate) fn import_snapshot_by_id_with_checkpoints(
    workspace_id: &WorkspaceId,
    snapshot_id: &SnapshotId,
    control_plane: &dyn ControlPlaneClient,
    byte_store: &dyn ByteStore,
    storage_key: StorageKey,
    identity_key: MetadataIdentityKey,
    checkpoint: &mut dyn FnMut() -> Result<(), DownloadError>,
) -> Result<ImportedSnapshot, DownloadError> {
    checkpoint()?;
    let root = control_plane
        .get_snapshot_root(workspace_id, snapshot_id)?
        .ok_or_else(|| DownloadError::SnapshotManifestMissing(snapshot_id.as_str().to_string()))?;
    import_snapshot_manifest_with_checkpoints(
        workspace_id,
        &root,
        control_plane,
        byte_store,
        storage_key,
        identity_key,
        checkpoint,
    )
}

pub fn open_remote_snapshot_by_id<'a>(
    workspace_id: &'a WorkspaceId,
    snapshot_id: &SnapshotId,
    control_plane: &'a dyn ControlPlaneClient,
    byte_store: &'a dyn ByteStore,
    storage_key: StorageKey,
    identity_key: MetadataIdentityKey,
) -> Result<(SnapshotManifest, RemoteNamespaceSnapshot<'a>), DownloadError> {
    let root = control_plane
        .get_snapshot_root(workspace_id, snapshot_id)?
        .ok_or_else(|| DownloadError::SnapshotManifestMissing(snapshot_id.as_str().to_string()))?;
    if !root.complete {
        return Err(DownloadError::UnsafeManifest("snapshot root is incomplete"));
    }
    open_remote_namespace(
        workspace_id,
        &root,
        control_plane,
        byte_store,
        storage_key,
        identity_key,
    )
}

pub fn import_snapshot_manifest(
    workspace_id: &WorkspaceId,
    root: &SnapshotRootRecord,
    control_plane: &dyn ControlPlaneClient,
    byte_store: &dyn ByteStore,
    storage_key: StorageKey,
    identity_key: MetadataIdentityKey,
) -> Result<ImportedSnapshot, DownloadError> {
    import_snapshot_manifest_with_checkpoints(
        workspace_id,
        root,
        control_plane,
        byte_store,
        storage_key,
        identity_key,
        &mut || Ok(()),
    )
}

fn import_snapshot_manifest_with_checkpoints(
    workspace_id: &WorkspaceId,
    root: &SnapshotRootRecord,
    control_plane: &dyn ControlPlaneClient,
    byte_store: &dyn ByteStore,
    storage_key: StorageKey,
    identity_key: MetadataIdentityKey,
    checkpoint: &mut dyn FnMut() -> Result<(), DownloadError>,
) -> Result<ImportedSnapshot, DownloadError> {
    checkpoint()?;
    if &root.workspace_id != workspace_id || !root.complete {
        return Err(DownloadError::UnsafeManifest(
            "snapshot root workspace or completeness mismatch",
        ));
    }
    let (manifest, remote) = open_remote_namespace(
        workspace_id,
        root,
        control_plane,
        byte_store,
        storage_key,
        identity_key,
    )?;
    checkpoint()?;
    let mut verify_context = NamespaceOperationContext::uncancelled(
        NamespaceOperationBudget::new(
            manifest
                .entry_count
                .saturating_mul(MAX_SEGMENTS_PER_LAYOUT as u64 + 8),
            0,
            0,
        )
        .with_metadata_limits(
            (MAX_IMPORTED_METADATA_RECORDS as u64).saturating_mul(4),
            (MAX_IMPORTED_METADATA_RECORDS as u64).saturating_mul(4),
            (MAX_IMPORTED_METADATA_RECORDS as u64).saturating_mul(4),
            MAX_IMPORTED_METADATA_BYTES,
        ),
    );
    let reader = remote.reader();
    reader.verify(&mut verify_context)?;
    checkpoint()?;
    validate_imported_entries(&reader, &mut verify_context)?;
    checkpoint()?;
    let store = remote.source.materialized_store()?;
    let namespace = BuiltPagedNamespaceSnapshot::from_manifest(manifest, store);
    let snapshot = SnapshotContent::from_built(namespace, BTreeMap::new());
    let layouts = content_layout_map_from_snapshot(&snapshot)?;
    checkpoint()?;
    let locators = layouts
        .values()
        .flat_map(ContentLayout::segments)
        .map(content_locator_for_segment)
        .collect::<Vec<_>>();
    let pack_pointers = pack_pointers(workspace_id, &layouts, control_plane)?;
    checkpoint()?;
    Ok(ImportedSnapshot {
        snapshot,
        locators,
        pack_pointers,
    })
}

fn download_root(
    workspace_id: &WorkspaceId,
    root: &SnapshotRootRecord,
    control_plane: &dyn ControlPlaneClient,
    byte_store: &dyn ByteStore,
    storage_key: StorageKey,
) -> Result<SnapshotManifest, DownloadError> {
    control_plane.create_download_intent(DownloadIntentRequest::full(
        workspace_id.clone(),
        &root.manifest_object.object_key,
    ))?;
    let object_key = ObjectKey::new(root.manifest_object.object_key.clone())?;
    let bytes = byte_store.get_object(&object_key)?;
    let sealed = SealedSnapshotManifest {
        pointer: ManifestPointer {
            manifest_id: root.manifest_id.clone(),
            snapshot_id: root.snapshot_id.clone(),
            object_key,
            byte_len: root.manifest_object.byte_len,
            hash: root.manifest_object.hash.clone(),
            key_epoch: root.manifest_object.key_epoch,
            kind: ManifestPointerKind::Snapshot,
        },
        bytes,
    };
    open_snapshot_manifest(&sealed, storage_key, workspace_id).map_err(Into::into)
}

fn open_remote_namespace<'a>(
    workspace_id: &'a WorkspaceId,
    root: &SnapshotRootRecord,
    control_plane: &'a dyn ControlPlaneClient,
    byte_store: &'a dyn ByteStore,
    storage_key: StorageKey,
    identity_key: MetadataIdentityKey,
) -> Result<(SnapshotManifest, RemoteNamespaceSnapshot<'a>), DownloadError> {
    let manifest = download_root(workspace_id, root, control_plane, byte_store, storage_key)?;
    validate_imported_root(workspace_id, &root.snapshot_id, root, &manifest)?;
    let remote = RemoteNamespaceSnapshot {
        metadata: snapshot_metadata(&manifest),
        root: manifest.namespace_root_id.clone(),
        source: RemotePageSource {
            workspace_id,
            identity_key,
            control_plane,
            byte_store,
            storage_key,
            verified: Mutex::new(VerifiedMetadataCache::production()),
            bindings: Mutex::new(BTreeMap::new()),
        },
    };
    Ok((manifest, remote))
}

impl PagedRecordSource for RemotePageSource<'_> {
    fn metadata_identity_key(&self) -> MetadataIdentityKey {
        self.identity_key
    }

    fn load_record(
        &self,
        kind: MetadataRecordKind,
        logical_id: &str,
        context: &mut NamespaceOperationContext<'_>,
    ) -> Result<Option<Arc<[u8]>>, NamespaceReadError> {
        context.ensure_active()?;
        let cache_key = (kind, logical_id.to_string());
        if let Some(bytes) = self
            .verified
            .lock()
            .map_err(|_| remote_corrupt())?
            .get(&cache_key)
        {
            return Ok(Some(bytes));
        }
        self.prefetch_records(kind, &[logical_id.to_string()], context)?;
        let binding = self
            .bindings
            .lock()
            .map_err(|_| remote_corrupt())?
            .get(&(kind, logical_id.to_string()))
            .cloned()
            .ok_or(NamespaceReadError::MissingRecord {
                record: "hosted metadata binding",
            })?;
        if binding.object.kind != ControlObjectKind::SnapshotMetadataPage
            || binding.object.content_id != binding.logical_id
            || binding.object.byte_len == 0
            || binding.object.byte_len > SNAPSHOT_METADATA_PAGE_MAX_SEALED_BYTES as u64
        {
            return Err(remote_corrupt());
        }
        let maximum_canonical_bytes = SNAPSHOT_METADATA_PAGE_MAX_CANONICAL_BYTES as u64;
        match kind {
            MetadataRecordKind::NamespacePage => {
                context.ensure_namespace_page_capacity(maximum_canonical_bytes)?;
            }
            MetadataRecordKind::ContentLayout => {
                context.ensure_layout_record_capacity(maximum_canonical_bytes)?;
            }
            MetadataRecordKind::SegmentPage => {
                context.ensure_segment_page_capacity(maximum_canonical_bytes)?;
            }
        }
        context.ensure_active()?;
        let canonical = download_metadata_record(
            self.workspace_id,
            &binding,
            self.control_plane,
            self.byte_store,
            self.storage_key,
            context,
        )?;
        let mut verified = PageStore::with_identity_key(self.identity_key);
        verified.insert_verified(kind, logical_id, canonical)?;
        let summary =
            verified
                .metadata_record(logical_id)?
                .ok_or(NamespaceReadError::MissingRecord {
                    record: "verified hosted metadata summary",
                })?;
        validate_sidecar(&summary, &binding).map_err(|_| remote_corrupt())?;
        for object_key in metadata_direct_object_keys(&summary).map_err(|_| remote_corrupt())? {
            context.ensure_active()?;
            let metadata = self
                .control_plane
                .head_object_metadata(self.workspace_id, &object_key)
                .map_err(|_| remote_corrupt())?;
            if metadata.kind != bowline_storage::ObjectKind::SourcePack {
                return Err(remote_corrupt());
            }
        }
        let bytes = Arc::<[u8]>::from(
            verified
                .plaintext_record(logical_id)?
                .map(|record| record.plaintext)
                .ok_or(NamespaceReadError::MissingRecord {
                    record: "verified hosted metadata plaintext",
                })?,
        );
        self.verified
            .lock()
            .map_err(|_| remote_corrupt())?
            .insert(cache_key, Arc::clone(&bytes))?;
        Ok(Some(bytes))
    }

    fn prefetch_records(
        &self,
        kind: MetadataRecordKind,
        logical_ids: &[String],
        context: &mut NamespaceOperationContext<'_>,
    ) -> Result<(), NamespaceReadError> {
        context.ensure_active()?;
        let verified = self.verified.lock().map_err(|_| remote_corrupt())?;
        let bindings = self.bindings.lock().map_err(|_| remote_corrupt())?;
        let pending = logical_ids
            .iter()
            .filter(|logical_id| {
                !verified.contains_key(&(kind, (*logical_id).clone()))
                    && !bindings.contains_key(&(kind, (*logical_id).clone()))
            })
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        drop(bindings);
        drop(verified);
        for batch in pending.chunks(MAX_METADATA_BINDINGS_PER_BATCH) {
            context.ensure_active()?;
            let resolved = self
                .control_plane
                .resolve_metadata_bindings(self.workspace_id, batch)
                .map_err(|_| remote_corrupt())?;
            context.ensure_active()?;
            if &resolved.workspace_id != self.workspace_id {
                return Err(remote_corrupt());
            }
            let requested = batch.iter().cloned().collect::<BTreeSet<_>>();
            let mut received = BTreeMap::new();
            for binding in resolved.bindings {
                if !requested.contains(&binding.logical_id)
                    || local_record_kind(binding.record_kind) != kind
                    || received
                        .insert(binding.logical_id.clone(), binding)
                        .is_some()
                {
                    return Err(remote_corrupt());
                }
            }
            let mut bindings = self.bindings.lock().map_err(|_| remote_corrupt())?;
            for logical_id in batch {
                if let Some(binding) = received.remove(logical_id) {
                    bindings.insert((kind, logical_id.clone()), binding);
                }
            }
        }
        Ok(())
    }
}

fn snapshot_metadata(manifest: &SnapshotManifest) -> SnapshotMetadata {
    SnapshotMetadata {
        schema_version: manifest.schema_version,
        snapshot_id: manifest.snapshot_id.clone(),
        workspace_id: manifest.workspace_id.clone(),
        project_id: manifest.project_id.clone(),
        kind: manifest.kind,
        base_snapshot_id: manifest.base_snapshot_id.clone(),
        semantic_manifest_digest: manifest.semantic_manifest_digest.clone(),
        entry_count: manifest.entry_count,
        refs: manifest.refs.clone(),
    }
}

fn remote_corrupt() -> NamespaceReadError {
    NamespaceReadError::CorruptGraph {
        reason: "hosted metadata binding or object verification failed",
    }
}

fn download_metadata_record(
    workspace_id: &WorkspaceId,
    binding: &MetadataBindingRecord,
    control_plane: &dyn ControlPlaneClient,
    byte_store: &dyn ByteStore,
    storage_key: StorageKey,
    context: &mut NamespaceOperationContext<'_>,
) -> Result<Vec<u8>, NamespaceReadError> {
    context.ensure_active()?;
    control_plane
        .create_download_intent(DownloadIntentRequest::full(
            workspace_id.clone(),
            &binding.object.object_key,
        ))
        .map_err(|_| remote_corrupt())?;
    context.ensure_active()?;
    let object_key =
        ObjectKey::new(binding.object.object_key.clone()).map_err(|_| remote_corrupt())?;
    let bytes = byte_store
        .get_object(&object_key)
        .map_err(|_| remote_corrupt())?;
    context.ensure_active()?;
    let sealed = SealedSnapshotMetadataPage {
        pointer: SnapshotMetadataPagePointer {
            logical_id: storage_record_id(binding.record_kind, &binding.logical_id),
            object_key,
            byte_len: binding.object.byte_len,
            hash: binding.object.hash.clone(),
            key_epoch: binding.object.key_epoch,
            format_version: bowline_storage::SNAPSHOT_METADATA_PAGE_FORMAT_VERSION,
        },
        bytes,
    };
    open_snapshot_metadata_page(&sealed, workspace_id, storage_key).map_err(|_| remote_corrupt())
}

fn validate_imported_root(
    workspace_id: &WorkspaceId,
    expected_snapshot_id: &SnapshotId,
    root: &SnapshotRootRecord,
    manifest: &SnapshotManifest,
) -> Result<(), DownloadError> {
    if &manifest.workspace_id != workspace_id
        || &manifest.snapshot_id != expected_snapshot_id
        || manifest.schema_version != SNAPSHOT_SCHEMA_VERSION
        || manifest.namespace_root_id.as_str() != root.namespace_root_id
        || manifest.snapshot_id != root.snapshot_id
    {
        return Err(DownloadError::UnsafeManifest(
            "snapshot root authenticated identity mismatch",
        ));
    }
    Ok(())
}

fn validate_imported_entries(
    reader: &dyn NamespaceSnapshotReader,
    context: &mut NamespaceOperationContext<'_>,
) -> Result<(), DownloadError> {
    struct Validator {
        folded_paths: BTreeMap<String, String>,
        seen_paths: BTreeSet<String>,
        error: Option<DownloadError>,
    }
    impl EntryVisitor for Validator {
        fn visit(
            &mut self,
            entry: &NamespaceEntry,
            _context: &mut NamespaceOperationContext<'_>,
        ) -> Result<NamespaceVisitControl, NamespaceReadError> {
            if let Err(error) =
                validate_imported_entry(entry, &mut self.folded_paths, &mut self.seen_paths)
            {
                self.error = Some(error);
                return Ok(NamespaceVisitControl::Stop);
            }
            Ok(NamespaceVisitControl::Continue)
        }
    }
    let mut validator = Validator {
        folded_paths: BTreeMap::new(),
        seen_paths: BTreeSet::new(),
        error: None,
    };
    reader.visit_prefix(&WorkspaceRelativePath::new(""), &mut validator, context)?;
    if let Some(error) = validator.error {
        return Err(error);
    }
    Ok(())
}

fn validate_imported_entry(
    entry: &NamespaceEntry,
    folded_paths: &mut BTreeMap<String, String>,
    seen_paths: &mut BTreeSet<String>,
) -> Result<(), DownloadError> {
    let normalized = normalize_workspace_path(&entry.path);
    if normalized != entry.path
        || normalized.is_empty()
        || normalized.starts_with("../")
        || normalized.contains("/../")
        || is_private_state_path(&normalized)
    {
        return Err(DownloadError::UnsafePath(entry.path.clone()));
    }
    if !seen_paths.insert(normalized.clone()) {
        return Err(DownloadError::UnsafeManifest("duplicate namespace path"));
    }
    validate_case_folded_prefixes(&normalized, folded_paths)
        .map_err(|_| DownloadError::UnsafeManifest("case-only path collision"))?;
    if entry.kind == NamespaceEntryKind::File {
        let layout = entry
            .content_layout
            .as_ref()
            .ok_or(DownloadError::UnsafeManifest(
                "file entry missing content layout",
            ))?;
        if Some(layout.logical_content_id()) != entry.content_id.as_ref()
            || entry.byte_len != Some(layout.logical_length())
        {
            return Err(DownloadError::UnsafeManifest(
                "content layout identity mismatch",
            ));
        }
        for segment in layout.segments() {
            validate_packed_locator(&content_locator_for_segment(segment))?;
        }
    } else if entry.kind == NamespaceEntryKind::Symlink {
        let target = entry
            .symlink_target
            .as_deref()
            .ok_or(DownloadError::UnsafeManifest(
                "symlink entry missing target",
            ))?;
        if !is_safe_workspace_symlink_target(target) {
            return Err(DownloadError::UnsafeManifest("unsafe symlink target"));
        }
    }
    Ok(())
}

fn validate_sidecar(
    summary: &crate::sync::namespace::MetadataRecordSummary,
    binding: &MetadataBindingRecord,
) -> Result<(), DownloadError> {
    let expected_keys = metadata_direct_object_keys(summary)?;
    let expected_digest = metadata_sidecar_digest(summary, &expected_keys);
    if summary.child_logical_ids != binding.sidecar.child_logical_ids
        || expected_keys != binding.sidecar.direct_object_keys
        || expected_digest != binding.sidecar.digest
    {
        return Err(DownloadError::UnsafeManifest(
            "metadata reachability sidecar does not match authenticated plaintext",
        ));
    }
    Ok(())
}

fn pack_pointers(
    workspace_id: &WorkspaceId,
    layouts: &BTreeMap<bowline_core::ids::ContentId, ContentLayout>,
    control_plane: &dyn ControlPlaneClient,
) -> Result<Vec<ObjectPointer>, DownloadError> {
    let mut pointers = Vec::new();
    for pack_id in layouts
        .values()
        .flat_map(ContentLayout::segments)
        .map(|segment| segment.pack_id.clone())
        .collect::<BTreeSet<_>>()
    {
        let key = ObjectKey::from_pack_id(&pack_id)?;
        let metadata = control_plane.head_object_metadata(workspace_id, key.as_str())?;
        pointers.push(ObjectPointer {
            object_key: metadata.key.as_str().to_string(),
            content_id: bowline_core::ids::ContentId::new(pack_id.as_str()),
            byte_len: metadata.byte_len,
            hash: metadata.hash,
            key_epoch: metadata.key_epoch,
            kind: bowline_control_plane::ObjectKind::SourcePack,
            created_at: bowline_control_plane::ControlPlaneTimestamp {
                tick: metadata.created_at_unix_ms,
            },
        });
    }
    Ok(pointers)
}

fn storage_record_id(kind: ControlMetadataRecordKind, id: &str) -> SnapshotMetadataRecordId {
    match kind {
        ControlMetadataRecordKind::NamespacePage => {
            SnapshotMetadataRecordId::NamespacePage(bowline_core::ids::NamespacePageId::new(id))
        }
        ControlMetadataRecordKind::ContentLayout => {
            SnapshotMetadataRecordId::ContentLayout(bowline_core::ids::ContentLayoutId::new(id))
        }
        ControlMetadataRecordKind::SegmentPage => {
            SnapshotMetadataRecordId::SegmentPage(bowline_core::ids::SegmentPageId::new(id))
        }
    }
}

fn local_record_kind(kind: ControlMetadataRecordKind) -> MetadataRecordKind {
    match kind {
        ControlMetadataRecordKind::NamespacePage => MetadataRecordKind::NamespacePage,
        ControlMetadataRecordKind::ContentLayout => MetadataRecordKind::ContentLayout,
        ControlMetadataRecordKind::SegmentPage => MetadataRecordKind::SegmentPage,
    }
}

fn validate_packed_locator(locator: &ContentLocator) -> Result<(), DownloadError> {
    if locator.pack_id.is_some()
        && locator.offset.is_some()
        && locator.length.is_some_and(|length| length > 0)
    {
        Ok(())
    } else {
        Err(DownloadError::UnsafeManifest(
            "packed locator missing range fields",
        ))
    }
}

pub(crate) fn content_locator_for_segment(segment: &SegmentLocator) -> ContentLocator {
    ContentLocator {
        content_id: bowline_core::ids::ContentId::new(segment.segment_id.as_str()),
        storage: ContentStorage::Packed,
        raw_size: segment.plaintext_length,
        pack_id: Some(segment.pack_id.clone()),
        offset: Some(segment.offset),
        length: Some(segment.length),
    }
}

fn is_private_state_path(path: &str) -> bool {
    path.split('/')
        .next()
        .is_some_and(|component| case_fold_path_component(component) == ".bowline")
}

#[derive(Debug)]
pub enum DownloadError {
    ControlPlane(ControlPlaneError),
    ByteStore(ByteStoreError),
    Manifest(bowline_storage::ManifestError),
    MetadataPage(MetadataPageError),
    Namespace(NamespaceReadError),
    UnsafePath(String),
    UnsafeManifest(&'static str),
    MissingBinding(String),
    SnapshotManifestMissing(String),
    CancellationRequested,
}

impl fmt::Display for DownloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ControlPlane(error) => error.fmt(formatter),
            Self::ByteStore(error) => error.fmt(formatter),
            Self::Manifest(error) => error.fmt(formatter),
            Self::MetadataPage(error) => error.fmt(formatter),
            Self::Namespace(error) => error.fmt(formatter),
            Self::UnsafePath(path) => write!(formatter, "remote namespace path `{path}` is unsafe"),
            Self::UnsafeManifest(reason) => {
                write!(formatter, "remote snapshot root is unsafe: {reason}")
            }
            Self::MissingBinding(logical_id) => {
                write!(formatter, "metadata binding `{logical_id}` was not found")
            }
            Self::SnapshotManifestMissing(snapshot_id) => {
                write!(formatter, "snapshot root `{snapshot_id}` was not found")
            }
            Self::CancellationRequested => {
                formatter.write_str("snapshot import cancellation was requested")
            }
        }
    }
}

impl Error for DownloadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ControlPlane(error) => Some(error),
            Self::ByteStore(error) => Some(error),
            Self::Manifest(error) => Some(error),
            Self::MetadataPage(error) => Some(error),
            Self::Namespace(error) => Some(error),
            Self::UnsafePath(_)
            | Self::UnsafeManifest(_)
            | Self::MissingBinding(_)
            | Self::SnapshotManifestMissing(_)
            | Self::CancellationRequested => None,
        }
    }
}

impl From<ControlPlaneError> for DownloadError {
    fn from(error: ControlPlaneError) -> Self {
        Self::ControlPlane(error)
    }
}

impl From<ByteStoreError> for DownloadError {
    fn from(error: ByteStoreError) -> Self {
        Self::ByteStore(error)
    }
}

impl From<bowline_storage::ManifestError> for DownloadError {
    fn from(error: bowline_storage::ManifestError) -> Self {
        Self::Manifest(error)
    }
}

impl From<MetadataPageError> for DownloadError {
    fn from(error: MetadataPageError) -> Self {
        Self::MetadataPage(error)
    }
}

impl From<NamespaceReadError> for DownloadError {
    fn from(error: NamespaceReadError) -> Self {
        Self::Namespace(error)
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    #[test]
    fn remote_metadata_cache_has_deterministic_count_and_byte_eviction() {
        let first = (MetadataRecordKind::NamespacePage, "nsp_first".to_string());
        let second = (MetadataRecordKind::NamespacePage, "nsp_second".to_string());
        let third = (MetadataRecordKind::NamespacePage, "nsp_third".to_string());
        let mut cache = VerifiedMetadataCache::with_limits(2, 8);
        cache
            .insert(first.clone(), Arc::from([1_u8, 2, 3].as_slice()))
            .expect("first admission");
        cache
            .insert(second.clone(), Arc::from([4_u8, 5, 6].as_slice()))
            .expect("second admission");
        cache
            .insert(third.clone(), Arc::from([7_u8, 8, 9].as_slice()))
            .expect("third admission evicts oldest");
        assert!(cache.get(&first).is_none());
        assert!(cache.get(&second).is_some());
        assert!(cache.get(&third).is_some());
        assert_eq!(cache.encoded_bytes, 6);

        let oversized = cache
            .insert(
                (
                    MetadataRecordKind::NamespacePage,
                    "nsp_oversized".to_string(),
                ),
                Arc::from([0_u8; 9].as_slice()),
            )
            .expect_err("single cache entry over the byte budget");
        assert!(matches!(
            oversized,
            NamespaceReadError::OversizedRecord {
                record: "remote metadata cache entry",
                ..
            }
        ));
    }
}
