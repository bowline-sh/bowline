use std::collections::BTreeMap;

mod materialization_planning;
mod materialization_worker;
mod segmented_hydration;

const IMPORTED_HYDRATION_BATCH_ENTRIES: usize = 256;

use bowline_control_plane::{ObjectPointer, WorkspaceRef};
use bowline_core::{
    events::{EventName, EventSeverity},
    ids::{ContentId, SnapshotId},
    namespace_snapshot::{
        EntryVisitor, NamespaceOperationBudget, NamespaceOperationContext, NamespaceReadError,
        NamespaceVisitControl,
    },
    workspace_graph::{NamespaceEntry, NamespaceEntryKind, SegmentLocator, WorkspaceRelativePath},
};
use bowline_storage::{
    CacheError, CachedPackReader, ContentVerification, HydrationRecord, LocalContentCache,
    ObjectKey, PackHydrationPlan, PackHydrationSource, PlannedRecord, RangeHydrationRequest,
    plan_pack_hydration,
};

use super::helpers::*;
use super::reason_code::CheckpointReasonCode;
use super::{ImportedHydrationSelection, SyncRunner, SyncRunnerError};
use crate::metadata::{
    MaterializationPathState, MaterializationPathStateRecord, MetadataError, MetadataStore,
};
use crate::sync::coalescer::{PrepareSnapshotReaderRequest, prepare_snapshot_reader};
use crate::sync::materialization::required_in_ordinary_directory;
use crate::sync::prepared_content::retain_one_prepared_source;
use crate::sync::{
    DownloadError, SnapshotContent,
    download::{content_locator_for_segment, import_snapshot_by_id_with_checkpoints},
};
use segmented_hydration::{SegmentedHydrationReader, SegmentedHydrationRequest};

impl<'a> SyncRunner<'a> {
    fn import_snapshot_checked(
        &self,
        snapshot_id: &SnapshotId,
    ) -> Result<crate::sync::ImportedSnapshot, SyncRunnerError> {
        let mut boundary_error = None;
        let mut checkpoint = || match self.check_claim_before_domain_boundary() {
            Ok(()) => Ok(()),
            Err(error) => {
                boundary_error = Some(error);
                Err(DownloadError::CancellationRequested)
            }
        };
        let result = import_snapshot_by_id_with_checkpoints(
            &self.options.workspace_id,
            snapshot_id,
            self.control_plane,
            self.byte_store,
            self.options.storage_key,
            crate::sync::namespace::MetadataIdentityKey::derive(
                &self.options.workspace_id,
                self.options.workspace_content_key,
            ),
            &mut checkpoint,
        );
        if let Some(error) = boundary_error {
            return Err(error);
        }
        result.map_err(|error| match error {
            DownloadError::CancellationRequested => {
                SyncRunnerError::SyncOperationCancellationRequested
            }
            error => error.into(),
        })
    }

    pub(super) fn import_full_snapshot(
        &self,
        snapshot_id: &SnapshotId,
    ) -> Result<SnapshotContent, SyncRunnerError> {
        let imported = self.import_snapshot_structure(snapshot_id)?;
        self.hydrate_imported_snapshot(
            imported.snapshot,
            &imported.pack_pointers,
            ImportedHydrationSelection::AllFiles,
        )
    }

    pub(super) fn import_snapshot_structure(
        &self,
        snapshot_id: &SnapshotId,
    ) -> Result<crate::sync::ImportedSnapshot, SyncRunnerError> {
        if snapshot_id.as_str() == EMPTY_SNAPSHOT_ID {
            let snapshot = empty_snapshot_content(
                self.options.workspace_id.clone(),
                snapshot_id.clone(),
                self.options.workspace_content_key,
            )
            .map_err(|error| match error {
                bowline_core::namespace_snapshot::NamespaceBuildError::Read(error) => {
                    SyncRunnerError::from(error)
                }
            })?;
            return Ok(crate::sync::ImportedSnapshot {
                snapshot,
                locators: Vec::new(),
                pack_pointers: Vec::new(),
            });
        }
        let imported = self.import_snapshot_checked(snapshot_id)?;
        self.persist_imported_snapshot(&imported.snapshot)?;
        Ok(imported)
    }

    fn persist_imported_snapshot(&self, snapshot: &SnapshotContent) -> Result<(), SyncRunnerError> {
        let metadata_path = self.metadata_db_path();
        if !metadata_path.exists() {
            return Ok(());
        }
        self.with_store_sync(|store| {
            store.insert_workspace(
                &snapshot.manifest().workspace_id,
                "Code",
                &self.options.generated_at,
            )?;
            self.persist_snapshot_page_authority(store, snapshot)
        })?;
        Ok(())
    }

    pub(super) fn hydrate_imported_snapshot(
        &self,
        snapshot: SnapshotContent,
        pack_pointers: &[ObjectPointer],
        selection: ImportedHydrationSelection,
    ) -> Result<SnapshotContent, SyncRunnerError> {
        let cache = LocalContentCache::open(self.options.state_root.join("cache"))?;
        self.hydrate_imported_snapshot_with_cache(snapshot, pack_pointers, selection, &cache)
    }

    pub(super) fn hydrate_imported_snapshot_with_cache(
        &self,
        snapshot: SnapshotContent,
        pack_pointers: &[ObjectPointer],
        selection: ImportedHydrationSelection,
        cache: &LocalContentCache,
    ) -> Result<SnapshotContent, SyncRunnerError> {
        self.hydrate_imported_snapshot_matching(snapshot, pack_pointers, cache, |entry| {
            should_hydrate_imported_entry(entry, &selection)
        })
    }

    fn hydrate_imported_materialization_task(
        &self,
        snapshot: SnapshotContent,
        pack_pointers: &[ObjectPointer],
        path: &str,
    ) -> Result<SnapshotContent, SyncRunnerError> {
        let cache = LocalContentCache::open(self.options.state_root.join("cache"))?;
        let pack_epochs = pack_epochs_by_id(pack_pointers)?;
        let pack_byte_lengths = pack_byte_lengths_by_id(pack_pointers)?;
        let mut content = BTreeMap::new();
        if let Some(entry) = snapshot.entry_for_path(path)? {
            self.hydrate_imported_batch(
                &[HydrationBatchEntry {
                    entry,
                    selected: true,
                }],
                &cache,
                &pack_epochs,
                &pack_byte_lengths,
                false,
                &mut content,
            )?;
        }
        Ok(SnapshotContent::from_built(
            snapshot.namespace_snapshot().clone(),
            content,
        ))
    }

    fn hydrate_imported_snapshot_matching(
        &self,
        snapshot: SnapshotContent,
        pack_pointers: &[ObjectPointer],
        cache: &LocalContentCache,
        selected: impl Fn(&bowline_core::workspace_graph::NamespaceEntry) -> bool,
    ) -> Result<SnapshotContent, SyncRunnerError> {
        let pack_epochs = pack_epochs_by_id(pack_pointers)?;
        let pack_byte_lengths = pack_byte_lengths_by_id(pack_pointers)?;
        let mut visitor = ImportedHydrationVisitor {
            runner: self,
            selected: &selected,
            cache,
            pack_epochs: &pack_epochs,
            pack_byte_lengths: &pack_byte_lengths,
            batch: Vec::with_capacity(IMPORTED_HYDRATION_BATCH_ENTRIES),
            bounded_batch_flushed: false,
            content: BTreeMap::new(),
            error: None,
        };
        let entry_count = snapshot.manifest().entry_count;
        let segment_page_visits =
            entry_count.saturating_mul(snapshot.namespace_store().segment_page_count().max(1));
        let mut operation = NamespaceOperationContext::uncancelled(
            NamespaceOperationBudget::new(entry_count, 0, 0).with_metadata_limits(
                snapshot.namespace_store().namespace_page_count(),
                entry_count,
                segment_page_visits,
                snapshot
                    .namespace_store()
                    .total_encoded_bytes()
                    .saturating_mul(entry_count.max(1)),
            ),
        );
        snapshot.visit_entries(&mut operation, &mut visitor)?;
        let complete_batch = !visitor.bounded_batch_flushed;
        visitor.flush(complete_batch);
        if let Some(error) = visitor.error {
            return Err(error);
        }
        Ok(SnapshotContent::from_built(
            snapshot.namespace_snapshot().clone(),
            visitor.content,
        ))
    }

    fn hydrate_imported_batch(
        &self,
        entries: &[HydrationBatchEntry],
        cache: &LocalContentCache,
        pack_epochs: &BTreeMap<String, u32>,
        pack_byte_lengths: &BTreeMap<String, u64>,
        complete_batch: bool,
        content: &mut BTreeMap<ContentId, crate::sync::PreparedContent>,
    ) -> Result<(), SyncRunnerError> {
        self.check_claim_before_domain_boundary()?;
        let mut selections = BTreeMap::new();
        for batch_entry in entries {
            let Some(layout) = &batch_entry.entry.content_layout else {
                continue;
            };
            for segment in layout.segments() {
                let selection = selections
                    .entry(segment.pack_id.clone())
                    .or_insert_with(PackSegmentSelection::default);
                selection.total_segment_count += 1;
                if batch_entry.selected {
                    selection
                        .selected_locators
                        .push(content_locator_for_segment(segment));
                }
            }
        }
        if !complete_batch {
            for selection in selections.values_mut() {
                selection.total_segment_count = selection.total_segment_count.saturating_add(1);
            }
        }
        for (pack_id, selection) in selections {
            if selection.selected_locators.is_empty() {
                continue;
            }
            self.check_claim_before_domain_boundary()?;
            let object_key = ObjectKey::from_pack_id(&pack_id)?;
            let pack_byte_len = pack_byte_lengths
                .get(pack_id.as_str())
                .copied()
                .ok_or(SyncRunnerError::MissingPackedLocator("pack_object"))?;
            let key_epoch = pack_epochs
                .get(pack_id.as_str())
                .copied()
                .ok_or(SyncRunnerError::MissingPackedLocator("pack_object"))?;
            self.prehydrate_pack_segments(cache, &object_key, pack_byte_len, key_epoch, selection)?;
        }
        for batch_entry in entries.iter().filter(|entry| entry.selected) {
            let entry = &batch_entry.entry;
            let Some(content_id) = &entry.content_id else {
                continue;
            };
            let Some(layout) = &entry.content_layout else {
                continue;
            };
            self.check_claim_before_domain_boundary()?;
            let prepared =
                self.prepare_imported_entry(entry, layout.segments(), cache, pack_epochs)?;
            retain_one_prepared_source(content, content_id.clone(), prepared)?;
        }
        Ok(())
    }

    fn prepare_imported_entry(
        &self,
        entry: &NamespaceEntry,
        segments: &[SegmentLocator],
        cache: &LocalContentCache,
        pack_epochs: &BTreeMap<String, u32>,
    ) -> Result<crate::sync::PreparedContent, SyncRunnerError> {
        let content_id = entry
            .content_id
            .as_ref()
            .ok_or(SyncRunnerError::MissingPackedLocator("content_id"))?;
        let mut reader = SegmentedHydrationReader::new(SegmentedHydrationRequest {
            segments,
            pack_epochs,
            cache,
            byte_store: self.byte_store,
            workspace_id: &self.options.workspace_id,
            content_key: self.options.workspace_content_key,
            storage_key: self.options.storage_key,
        });
        let preparation_root = self.options.state_root.join("preparations").join("import");
        let prepared = prepare_snapshot_reader(
            PrepareSnapshotReaderRequest {
                workspace_id: &self.options.workspace_id,
                workspace_content_key: self.options.workspace_content_key,
                workspace_root: &self.options.root,
                preparation_root: &preparation_root,
                relative_path: &entry.path,
                created_at: &self.options.generated_at,
            },
            &mut reader,
        );
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                if let Some(cache_error) = reader.take_cache_failure() {
                    return Err(cache_error.into());
                }
                return Err(error.into());
            }
        };
        if prepared.content_id != *content_id {
            return Err(SyncRunnerError::ImportedContentIdMismatch {
                path: entry.path.clone(),
                expected: content_id.clone(),
                actual: prepared.content_id,
            });
        }
        Ok(prepared)
    }

    fn prehydrate_pack_segments(
        &self,
        cache: &LocalContentCache,
        object_key: &ObjectKey,
        pack_byte_len: u64,
        key_epoch: u32,
        selection: PackSegmentSelection,
    ) -> Result<(), SyncRunnerError> {
        let records = selection
            .selected_locators
            .into_iter()
            .map(|locator| {
                let cached = match cache.get_previously_verified_content(&locator.content_id) {
                    Ok(_) => true,
                    Err(CacheError::ContentIdMismatch { .. }) => {
                        cache.evict_content(&locator.content_id)?;
                        false
                    }
                    Err(CacheError::MissingCachedBytes(_)) => false,
                    Err(error) => return Err(SyncRunnerError::Cache(error)),
                };
                Ok(HydrationRecord { locator, cached })
            })
            .collect::<Result<Vec<_>, SyncRunnerError>>()?;
        let all_segments_cached = records.iter().all(|record| record.cached);
        let mut cached_reader = if all_segments_cached {
            None
        } else {
            match cache.open_cached_pack(object_key) {
                Ok(reader) => Some(reader),
                Err(CacheError::MissingCachedBytes(_)) => None,
                Err(error) => return Err(error.into()),
            }
        };
        let plan = plan_pack_hydration(
            pack_byte_len,
            cached_reader.is_some(),
            selection.total_segment_count == records.len(),
            records,
        )?;
        self.execute_pack_hydration_plan(cache, object_key, key_epoch, plan, &mut cached_reader)
    }

    fn execute_pack_hydration_plan(
        &self,
        cache: &LocalContentCache,
        object_key: &ObjectKey,
        key_epoch: u32,
        plan: PackHydrationPlan,
        cached_reader: &mut Option<CachedPackReader>,
    ) -> Result<(), SyncRunnerError> {
        match plan.source {
            None => Ok(()),
            Some(PackHydrationSource::CachedPack(records)) => {
                self.hydrate_planned_records(cache, object_key, key_epoch, records, cached_reader)
            }
            Some(PackHydrationSource::RemoteFull(records)) => {
                drop(cached_reader.take());
                cache.prefetch_pack(self.byte_store, object_key)?;
                *cached_reader = Some(cache.open_cached_pack(object_key)?);
                self.hydrate_planned_records(cache, object_key, key_epoch, records, cached_reader)
            }
            Some(PackHydrationSource::RemoteRanges(ranges)) => {
                drop(cached_reader.take());
                for range in ranges {
                    let fetched = self.byte_store.get_range(object_key, range.range)?;
                    if fetched.len() as u64 != range.range.length {
                        return Err(CacheError::ShortFetchedRange {
                            expected: range.range.length,
                            actual: fetched.len() as u64,
                        }
                        .into());
                    }
                    for record in range.records {
                        let encrypted_record = fetched_record_slice(&fetched, &record)?;
                        cache.hydrate_record_from_fetched_range(
                            segment_hydration_request(self, object_key, key_epoch, &record.locator),
                            encrypted_record,
                        )?;
                    }
                }
                Ok(())
            }
        }
    }

    fn hydrate_planned_records(
        &self,
        cache: &LocalContentCache,
        object_key: &ObjectKey,
        key_epoch: u32,
        records: Vec<PlannedRecord>,
        reader: &mut Option<CachedPackReader>,
    ) -> Result<(), SyncRunnerError> {
        for record in records {
            cache.hydrate_record_with_cached_pack(
                self.byte_store,
                segment_hydration_request(self, object_key, key_epoch, &record.locator),
                reader,
            )?;
        }
        Ok(())
    }

    fn persist_excluded_materialization_states(
        &self,
        store: &MetadataStore,
        snapshot: &SnapshotContent,
    ) -> Result<(), SyncRunnerError> {
        let reader = snapshot.namespace_reader();
        let mut operation = NamespaceOperationContext::uncancelled(
            NamespaceOperationBudget::new(snapshot.manifest().entry_count, 0, 0)
                .with_metadata_limits(
                    snapshot.namespace_store().namespace_page_count(),
                    0,
                    0,
                    snapshot.namespace_store().total_encoded_bytes(),
                ),
        );
        let mut storage_error = None;
        reader.visit_prefix_descriptors(
            &WorkspaceRelativePath::new(""),
            &mut operation,
            &mut |descriptor| {
                let entry = descriptor.entry_without_layout;
                if required_in_ordinary_directory(&entry) {
                    return Ok(NamespaceVisitControl::Continue);
                }
                if let Err(error) =
                    store.upsert_materialization_path_state(&MaterializationPathStateRecord {
                        workspace_id: self.options.workspace_id.clone(),
                        project_id: snapshot.manifest().project_id.clone(),
                        path: entry.path,
                        snapshot_id: Some(snapshot.manifest().snapshot_id.clone()),
                        expected_content_id: entry.content_id,
                        state: MaterializationPathState::Excluded,
                        observed_content_id: None,
                        observed_byte_len: None,
                        source_hydration_state: None,
                        verified_at: None,
                        updated_at: self.options.generated_at.clone(),
                    })
                {
                    storage_error = Some(error);
                    return Ok(NamespaceVisitControl::Stop);
                }
                Ok(NamespaceVisitControl::Continue)
            },
        )?;
        storage_error.map_or(Ok(()), |error| Err(error.into()))
    }

    pub(super) fn import_and_materialize_remote(
        &self,
        remote_ref: &WorkspaceRef,
        local_head: Option<&WorkspaceRef>,
    ) -> Result<(), SyncRunnerError> {
        self.import_remote_structure(remote_ref, local_head)?;
        Ok(())
    }

    pub(super) fn import_remote_structure(
        &self,
        remote_ref: &WorkspaceRef,
        base_ref: Option<&WorkspaceRef>,
    ) -> Result<(), SyncRunnerError> {
        let result = self.import_remote_structure_inner(remote_ref, base_ref);
        if let Err(error) = &result {
            self.record_remote_import_failure(remote_ref, error);
        }
        result
    }

    fn import_remote_structure_inner(
        &self,
        remote_ref: &WorkspaceRef,
        base_ref: Option<&WorkspaceRef>,
    ) -> Result<(), SyncRunnerError> {
        let imported =
            self.import_snapshot_checked(&SnapshotId::new(remote_ref.snapshot_id.clone()))?;
        let base = base_ref
            .filter(|base_ref| base_ref.snapshot_id != EMPTY_SNAPSHOT_ID)
            .map(|base_ref| {
                let imported =
                    self.import_snapshot_checked(&SnapshotId::new(base_ref.snapshot_id.clone()))?;
                Ok::<_, SyncRunnerError>(imported.snapshot)
            })
            .transpose()?;
        self.with_store(|store| {
            store.insert_workspace(
                &self.options.workspace_id,
                "Code",
                &self.options.generated_at,
            )?;
            let root_path = self.options.root.display().to_string();
            let root_id = store
                .accepted_root_id_for_path(&self.options.workspace_id, &root_path)?
                .unwrap_or_else(|| workspace_scoped_root_id(&self.options.workspace_id));
            store.insert_root(
                &root_id,
                &self.options.workspace_id,
                &root_path,
                &self.options.generated_at,
            )?;
            Ok(())
        })?;
        self.record_sync_checkpoint(
            "remote-import-started",
            "started",
            &checkpoint_payload(&SnapshotVersionPayload {
                snapshot_id: &remote_ref.snapshot_id,
                version: remote_ref.version,
            })?,
        )?;
        let hydration_snapshot = imported.snapshot.clone();
        let traversal_cancellation = self.scan_namespace_cancellation()?;
        let traversal_budget = materialization_planning::materialization_task_budget(
            &imported.snapshot,
            base.as_ref(),
        );
        let mut traversal = match traversal_cancellation.as_ref() {
            Some(cancellation) => NamespaceOperationContext::new(traversal_budget, cancellation),
            None => NamespaceOperationContext::uncancelled(traversal_budget),
        };
        let desired_result = materialization_planning::materialization_task_records_with_context(
            &imported.snapshot,
            base.as_ref(),
            &self.options.generated_at,
            &mut traversal,
        );
        let desired = self.finish_claim_backed_namespace_operation(
            traversal_cancellation.as_ref(),
            desired_result,
        )?;
        self.with_store_sync(|store| {
            self.persist_snapshot_page_authority(store, &imported.snapshot)?;
            store.reconcile_materialization_tasks(
                &self.options.workspace_id,
                &imported.snapshot.manifest().snapshot_id,
                &desired,
                &self.options.generated_at,
            )?;
            append_hydration_event(
                store,
                EventName::HydrationStarted,
                EventSeverity::Info,
                &self.options,
                remote_ref,
                Some(&imported.snapshot),
                None,
            );
            Ok(())
        })?;
        let remote = self.execute_imported_materialization_tasks(
            remote_ref,
            base.as_ref(),
            imported.snapshot,
            &imported.pack_pointers,
        )?;
        self.record_sync_checkpoint(
            "remote-materialized",
            "completed",
            &checkpoint_payload(&SnapshotEntryCountPayload {
                snapshot_id: &remote_ref.snapshot_id,
                entry_count: remote.manifest().entry_count as usize,
            })?,
        )?;
        self.with_store_sync(|store| {
            self.persist_excluded_materialization_states(store, &hydration_snapshot)?;
            for pointer in &imported.pack_pointers {
                let pack_id = pack_id_from_object_key(&pointer.object_key)
                    .map_err(|error| MetadataError::InvalidStorageMetadata(error.to_string()))?;
                store.put_pack_record_with_metadata(
                    &self.options.workspace_id,
                    &pack_id,
                    "source-pack",
                    pointer.byte_len,
                    &pointer.hash,
                    pointer.key_epoch,
                    "current",
                    None,
                    &self.options.generated_at,
                )?;
            }
            for locator in &imported.locators {
                store.put_content_locator(
                    &self.options.workspace_id,
                    locator,
                    &self.options.generated_at,
                )?;
            }
            self.rebuild_current_namespace_projection_full(store, &remote)?;
            append_hydration_event(
                store,
                EventName::HydrationCompleted,
                EventSeverity::Info,
                &self.options,
                remote_ref,
                Some(&hydration_snapshot),
                None,
            );
            Ok(())
        })?;
        self.record_sync_checkpoint(
            "remote-import-completed",
            "completed",
            &checkpoint_payload(&SnapshotPackLocatorCountPayload {
                snapshot_id: &remote_ref.snapshot_id,
                pack_count: imported.pack_pointers.len(),
                locator_count: imported.locators.len(),
            })?,
        )?;
        Ok(())
    }

    fn record_remote_import_failure(&self, remote_ref: &WorkspaceRef, error: &SyncRunnerError) {
        // Redacted: the import error can carry workspace paths, so durable
        // observability is restricted to the snapshot id and a fixed reason.
        match checkpoint_payload(&SnapshotReasonPayload {
            snapshot_id: &remote_ref.snapshot_id,
            reason: CheckpointReasonCode::RemoteImportBlocked.as_code(),
        }) {
            Ok(payload) => {
                if let Err(checkpoint_error) =
                    self.record_sync_checkpoint("remote-import-blocked", "blocked", &payload)
                {
                    report_event_append_failure(
                        "remote import blocked checkpoint append",
                        &checkpoint_error,
                    );
                }
            }
            Err(payload_error) => {
                report_event_append_failure(
                    "remote import blocked checkpoint serialization",
                    &payload_error,
                );
            }
        }
        let error_message = error.to_string();
        if let Err(append_error) = self.with_store(|store| {
            append_hydration_event(
                store,
                EventName::HydrationBlocked,
                EventSeverity::Limited,
                &self.options,
                remote_ref,
                None,
                Some(&error_message),
            );
            Ok(())
        }) {
            report_event_append_failure("hydration blocked event append", &append_error);
        }
    }
}

struct ImportedHydrationVisitor<'a, 'options, Selected> {
    runner: &'a SyncRunner<'options>,
    selected: &'a Selected,
    cache: &'a LocalContentCache,
    pack_epochs: &'a BTreeMap<String, u32>,
    pack_byte_lengths: &'a BTreeMap<String, u64>,
    batch: Vec<HydrationBatchEntry>,
    bounded_batch_flushed: bool,
    content: BTreeMap<ContentId, crate::sync::PreparedContent>,
    error: Option<SyncRunnerError>,
}

struct HydrationBatchEntry {
    entry: NamespaceEntry,
    selected: bool,
}

impl<Selected> ImportedHydrationVisitor<'_, '_, Selected>
where
    Selected: Fn(&NamespaceEntry) -> bool,
{
    fn flush(&mut self, complete_batch: bool) {
        if self.error.is_some() || self.batch.is_empty() {
            return;
        }
        let result = self.runner.hydrate_imported_batch(
            &self.batch,
            self.cache,
            self.pack_epochs,
            self.pack_byte_lengths,
            complete_batch,
            &mut self.content,
        );
        self.batch.clear();
        self.bounded_batch_flushed |= !complete_batch;
        if let Err(error) = result {
            self.error = Some(error);
        }
    }
}

impl<Selected> EntryVisitor for ImportedHydrationVisitor<'_, '_, Selected>
where
    Selected: Fn(&NamespaceEntry) -> bool,
{
    fn visit(
        &mut self,
        entry: &NamespaceEntry,
        _context: &mut NamespaceOperationContext<'_>,
    ) -> Result<NamespaceVisitControl, NamespaceReadError> {
        if entry.kind != NamespaceEntryKind::File {
            return Ok(NamespaceVisitControl::Continue);
        }
        self.batch.push(HydrationBatchEntry {
            entry: entry.clone(),
            selected: (self.selected)(entry),
        });
        if self.batch.len() == IMPORTED_HYDRATION_BATCH_ENTRIES {
            self.flush(false);
        }
        if self.error.is_some() {
            return Ok(NamespaceVisitControl::Stop);
        }
        Ok(NamespaceVisitControl::Continue)
    }
}

fn pack_byte_lengths_by_id(
    pack_pointers: &[ObjectPointer],
) -> Result<BTreeMap<String, u64>, SyncRunnerError> {
    pack_pointers
        .iter()
        .map(|pointer| {
            let pack_id = pack_id_from_object_key(&pointer.object_key)?;
            Ok((pack_id.as_str().to_string(), pointer.byte_len))
        })
        .collect()
}

#[derive(Default)]
struct PackSegmentSelection {
    total_segment_count: usize,
    selected_locators: Vec<bowline_core::workspace_graph::ContentLocator>,
}

fn segment_hydration_request<'a>(
    runner: &'a SyncRunner<'_>,
    object_key: &'a ObjectKey,
    key_epoch: u32,
    locator: &'a bowline_core::workspace_graph::ContentLocator,
) -> RangeHydrationRequest<'a> {
    RangeHydrationRequest {
        object_key,
        workspace_id: &runner.options.workspace_id,
        locator,
        content_key: runner.options.workspace_content_key,
        content_verification: ContentVerification::AuthenticatedSegment,
        key: runner.options.storage_key,
        key_epoch,
    }
}

fn fetched_record_slice<'a>(
    fetched: &'a [u8],
    record: &PlannedRecord,
) -> Result<&'a [u8], CacheError> {
    let expected = record.locator.length.unwrap_or_default();
    let start =
        usize::try_from(record.offset_within_fetch).map_err(|_| CacheError::ShortFetchedRange {
            expected,
            actual: fetched.len() as u64,
        })?;
    let length = usize::try_from(expected).map_err(|_| CacheError::ShortFetchedRange {
        expected,
        actual: fetched.len() as u64,
    })?;
    let end = start
        .checked_add(length)
        .ok_or(CacheError::ShortFetchedRange {
            expected,
            actual: fetched.len() as u64,
        })?;
    fetched
        .get(start..end)
        .ok_or(CacheError::ShortFetchedRange {
            expected,
            actual: fetched.len().saturating_sub(start) as u64,
        })
}
