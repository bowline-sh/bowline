use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
};

use super::conflicts::{
    mark_conflict_remote_metadata_published, mark_conflict_remote_resolution_synced,
    resolved_conflict_records, unpublished_unresolved_conflict_records,
    unresolved_conflict_upload_overrides,
};
use super::{
    CandidateBase, CoalesceError, CoalesceExclusions, ConflictBundleError, ConflictFile,
    ConflictRecord, DownloadError, ImportedSnapshot, MergeError, MergeOutcome, SnapshotContent,
    UploadError, UploadOutcome, create_conflict_bundle, import_snapshot_by_id, merge_snapshots,
    unresolved_conflict_paths, upload_snapshot_candidate_with_checkpoints,
};
use crate::env::import::{EnvImportError, import_env_records_from_scan};
use crate::hydration_budget::reconcile_materialized_hydration_queue;
use crate::metadata::{
    DEFAULT_DATABASE_FILE, MetadataError, MetadataStore, ObservedLocalPath, ProjectedNodeRecord,
    SyncOperationCheckpointRecord, WorkspaceSyncHeadRecord,
};
use crate::work_views::{WorkViewOverlaySyncError, WorkViewOverlaySyncOptions};
use bowline_control_plane::{
    ConflictMetadataPublish, ConflictResolutionMark, ConflictResolutionState, ControlPlaneClient,
    ControlPlaneError, WorkspaceRef,
};
use bowline_core::{
    events::{EventName, EventSeverity, EventSubject, EventSubjectKind, WorkspaceEvent},
    ids::{ContentId, DeviceId, EventId, ProjectId, SnapshotId, WorkspaceId},
    policy::MaterializationMode,
    workspace_graph::{
        HydrationState, NamespaceEntry, NamespaceEntryKind, RefKind, SnapshotKind,
        SnapshotManifest, WorkspaceRef as SnapshotRef, normalize_workspace_path,
    },
};
use bowline_storage::{
    ByteStore, CacheError, LocalContentCache, ObjectKey, RangeHydrationRequest, StorageKey,
};

#[derive(Debug, Clone)]
pub struct SyncRunnerOptions {
    pub root: PathBuf,
    pub state_root: PathBuf,
    pub workspace_id: WorkspaceId,
    pub device_id: DeviceId,
    pub workspace_content_key: [u8; 32],
    pub storage_key: StorageKey,
    pub key_epoch: u32,
    pub generated_at: String,
    pub sync_operation_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncTickOutcome {
    NoWorkspaceRef,
    NoChanges,
    Imported(WorkspaceRef),
    Uploaded(Box<UploadOutcome>),
    Merged(Box<UploadOutcome>),
    Conflicted(Vec<ConflictRecord>),
}

pub struct SyncRunner<'a> {
    control_plane: &'a dyn ControlPlaneClient,
    byte_store: &'a dyn ByteStore,
    options: SyncRunnerOptions,
    observed_base_ref: Option<WorkspaceRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImportedHydrationSelection {
    AllFiles,
    EagerFiles,
}

impl<'a> SyncRunner<'a> {
    pub fn new(
        control_plane: &'a dyn ControlPlaneClient,
        byte_store: &'a dyn ByteStore,
        options: SyncRunnerOptions,
    ) -> Self {
        Self {
            control_plane,
            byte_store,
            options,
            observed_base_ref: None,
        }
    }

    pub fn new_with_base_ref(
        control_plane: &'a dyn ControlPlaneClient,
        byte_store: &'a dyn ByteStore,
        options: SyncRunnerOptions,
        base_ref: WorkspaceRef,
    ) -> Self {
        Self {
            control_plane,
            byte_store,
            options,
            observed_base_ref: Some(base_ref),
        }
    }

    pub fn tick(&self) -> Result<SyncTickOutcome, SyncRunnerError> {
        let base_ref = match &self.observed_base_ref {
            Some(base_ref) => base_ref.clone(),
            None => {
                let Some(base_ref) = self
                    .control_plane
                    .get_workspace_ref(self.options.workspace_id.as_str())
                    .map_err(UploadError::ControlPlane)?
                else {
                    return Ok(SyncTickOutcome::NoWorkspaceRef);
                };
                base_ref
            }
        };
        self.record_sync_checkpoint(
            "remote-ref-observed",
            "completed",
            &format!(
                "{{\"snapshotId\":{},\"version\":{}}}",
                json_string(&base_ref.snapshot_id),
                base_ref.version,
            ),
        )?;
        self.publish_unresolved_conflict_metadata()?;
        let local_head = self.read_local_head()?;
        let candidate_base_ref = match &local_head {
            Some(head) if head.snapshot_id != base_ref.snapshot_id => head.clone(),
            None if base_ref.snapshot_id != "empty" => {
                empty_workspace_ref(self.options.workspace_id.clone())
            }
            _ => base_ref.clone(),
        };
        let excluded_paths = unresolved_conflict_paths(&self.options.state_root)?;
        let conflict_upload_overrides =
            unresolved_conflict_upload_overrides(&self.options.state_root, &self.options.root)?;
        let preserved_entries =
            self.preserved_base_entries(&candidate_base_ref, &excluded_paths)?;
        let candidate = super::coalescer::coalesce_workspace_scan_excluding(
            &self.options.root,
            self.options.workspace_id.clone(),
            &candidate_base_ref,
            self.options.device_id.clone(),
            self.options.workspace_content_key,
            self.options.generated_at.clone(),
            CoalesceExclusions {
                paths: &excluded_paths,
                preserved_entries: &preserved_entries,
                file_overrides: &conflict_upload_overrides,
            },
        )?;
        self.record_sync_checkpoint(
            "snapshot-candidate-built",
            "completed",
            &format!(
                "{{\"snapshotId\":{},\"fileCount\":{}}}",
                json_string(candidate.snapshot.manifest.snapshot_id.as_str()),
                candidate.snapshot.manifest.entries.len(),
            ),
        )?;
        if candidate.snapshot.manifest.snapshot_id.as_str() == base_ref.snapshot_id {
            self.write_local_head(&base_ref)?;
            self.persist_scan_metadata_if_committed(&candidate, &base_ref)?;
            self.mark_resolved_conflict_metadata()?;
            self.sync_work_view_overlays(&base_ref)?;
            return Ok(SyncTickOutcome::NoChanges);
        }

        if candidate.snapshot.manifest.snapshot_id.as_str() == candidate_base_ref.snapshot_id {
            if candidate_base_ref.snapshot_id != base_ref.snapshot_id {
                self.import_remote_structure(&base_ref, Some(&candidate_base_ref))?;
                self.write_local_head(&base_ref)?;
                self.persist_fresh_scan_metadata_for_head(&base_ref)?;
                self.mark_resolved_conflict_metadata()?;
                self.sync_work_view_overlays(&base_ref)?;
                return Ok(SyncTickOutcome::Imported(base_ref));
            }
            self.write_local_head(&base_ref)?;
            self.persist_scan_metadata_if_committed(&candidate, &base_ref)?;
            self.mark_resolved_conflict_metadata()?;
            self.sync_work_view_overlays(&base_ref)?;
            return Ok(SyncTickOutcome::NoChanges);
        }

        if local_head.is_none()
            && base_ref.snapshot_id != "empty"
            && candidate.snapshot.manifest.entries.is_empty()
        {
            self.import_and_materialize_remote(&base_ref, None)?;
            self.write_local_head(&base_ref)?;
            self.persist_fresh_scan_metadata_for_head(&base_ref)?;
            self.mark_resolved_conflict_metadata()?;
            self.sync_work_view_overlays(&base_ref)?;
            return Ok(SyncTickOutcome::Imported(base_ref));
        }

        if candidate_base_ref.snapshot_id != base_ref.snapshot_id {
            return self.resolve_stale_candidate(candidate, base_ref);
        }

        let outcome = self.upload_candidate_with_checkpoints(&candidate)?;
        match outcome {
            UploadOutcome::Advanced {
                ref workspace_ref, ..
            } => {
                self.write_local_head(workspace_ref)?;
                self.persist_scan_metadata_if_committed(&candidate, workspace_ref)?;
                self.mark_resolved_conflict_metadata()?;
                self.sync_work_view_overlays(workspace_ref)?;
                Ok(SyncTickOutcome::Uploaded(Box::new(outcome)))
            }
            UploadOutcome::Stale { stale, .. } => {
                self.resolve_stale_candidate(candidate, stale.current)
            }
        }
    }

    fn resolve_stale_candidate(
        &self,
        candidate: super::SnapshotCandidate,
        current_ref: WorkspaceRef,
    ) -> Result<SyncTickOutcome, SyncRunnerError> {
        let base = self.import_full_snapshot(&candidate.base.snapshot_id)?;
        let remote =
            self.import_full_snapshot(&SnapshotId::new(current_ref.snapshot_id.clone()))?;
        let remote_base = CandidateBase::from_remote(&current_ref);
        match merge_snapshots(
            &base,
            &candidate,
            &remote,
            remote_base,
            self.options.workspace_content_key,
            self.options.generated_at.clone(),
        )? {
            MergeOutcome::Clean(merged) => {
                let outcome = self.upload_candidate_with_checkpoints(&merged)?;
                if let UploadOutcome::Advanced { workspace_ref, .. } = &outcome {
                    materialize_snapshot(&self.options.root, Some(&base), &merged.snapshot)?;
                    self.write_local_head(workspace_ref)?;
                    self.persist_fresh_scan_metadata_for_head(workspace_ref)?;
                    self.mark_resolved_conflict_metadata()?;
                    self.sync_work_view_overlays(workspace_ref)?;
                }
                Ok(SyncTickOutcome::Merged(Box::new(outcome)))
            }
            MergeOutcome::Conflicted(conflicts) => {
                let conflicted_paths = conflicts
                    .iter()
                    .flat_map(|conflict| conflict.paths.iter().cloned())
                    .collect::<BTreeSet<_>>();
                materialize_snapshot_excluding(
                    &self.options.root,
                    Some(&base),
                    &remote,
                    &conflicted_paths,
                )?;
                let mut records = Vec::with_capacity(conflicts.len());
                for mut conflict in conflicts {
                    conflict.workspace_root = Some(self.options.root.clone());
                    conflict.base_snapshot_id =
                        Some(candidate.base.snapshot_id.as_str().to_string());
                    conflict.remote_snapshot_id = Some(current_ref.snapshot_id.clone());
                    let files = conflict_files(&conflict, &base, &candidate.snapshot, &remote);
                    let bundle =
                        create_conflict_bundle(&self.options.state_root, conflict, &files)?;
                    self.publish_conflict_metadata(
                        &bundle.record,
                        &candidate.base.snapshot_id,
                        &current_ref.snapshot_id,
                    )?;
                    mark_conflict_remote_metadata_published(
                        &bundle.record,
                        &self.options.generated_at,
                    )?;
                    records.push(bundle.record);
                }
                self.write_local_head(&current_ref)?;
                Ok(SyncTickOutcome::Conflicted(records))
            }
        }
    }

    fn sync_work_view_overlays(&self, workspace_ref: &WorkspaceRef) -> Result<(), SyncRunnerError> {
        let work_view_overlay_report = crate::work_views::sync_local_work_view_overlays(
            WorkViewOverlaySyncOptions {
                db_path: self.metadata_db_path(),
                device_id: self.options.device_id.clone(),
                storage_key: self.options.storage_key,
                key_epoch: self.options.key_epoch,
                generated_at: self.options.generated_at.clone(),
            },
            self.control_plane,
            self.byte_store,
            workspace_ref,
        )?;
        if work_view_overlay_report.uploaded > 0 || work_view_overlay_report.attention > 0 {
            self.record_sync_checkpoint(
                "work-view-overlays-synced",
                "completed",
                &format!(
                    "{{\"uploaded\":{},\"attention\":{}}}",
                    work_view_overlay_report.uploaded, work_view_overlay_report.attention,
                ),
            )?;
        }
        Ok(())
    }

    fn publish_conflict_metadata(
        &self,
        conflict: &ConflictRecord,
        base_snapshot_id: &SnapshotId,
        remote_snapshot_id: &str,
    ) -> Result<(), SyncRunnerError> {
        self.control_plane
            .publish_conflict_metadata(ConflictMetadataPublish {
                workspace_id: self.options.workspace_id.as_str().to_string(),
                conflict_id: conflict.id.clone(),
                conflict_kind: conflict_kind_name(conflict).to_string(),
                paths: conflict.paths.clone(),
                contains_secrets: conflict.contains_secrets,
                base_snapshot_id: base_snapshot_id.as_str().to_string(),
                remote_snapshot_id: remote_snapshot_id.to_string(),
                detected_by_device_id: self.options.device_id.as_str().to_string(),
                bundle_object: None,
            })?;
        Ok(())
    }

    fn publish_unresolved_conflict_metadata(&self) -> Result<(), SyncRunnerError> {
        for conflict in unpublished_unresolved_conflict_records(&self.options.state_root)? {
            let Some(base_snapshot_id) = conflict.base_snapshot_id.as_deref() else {
                continue;
            };
            let Some(remote_snapshot_id) = conflict.remote_snapshot_id.as_deref() else {
                continue;
            };
            self.publish_conflict_metadata(
                &conflict,
                &SnapshotId::new(base_snapshot_id.to_string()),
                remote_snapshot_id,
            )?;
            mark_conflict_remote_metadata_published(&conflict, &self.options.generated_at)?;
        }
        Ok(())
    }

    fn mark_resolved_conflict_metadata(&self) -> Result<(), SyncRunnerError> {
        for conflict in resolved_conflict_records(&self.options.state_root)? {
            let Some(resolution) = conflict_resolution_state(&conflict.state) else {
                continue;
            };
            if conflict.remote_conflict_published_at.is_none() {
                let Some(base_snapshot_id) = conflict.base_snapshot_id.as_deref() else {
                    mark_conflict_remote_resolution_synced(&conflict, &self.options.generated_at)?;
                    continue;
                };
                let Some(remote_snapshot_id) = conflict.remote_snapshot_id.as_deref() else {
                    mark_conflict_remote_resolution_synced(&conflict, &self.options.generated_at)?;
                    continue;
                };
                self.publish_conflict_metadata(
                    &conflict,
                    &SnapshotId::new(base_snapshot_id.to_string()),
                    remote_snapshot_id,
                )?;
                mark_conflict_remote_metadata_published(&conflict, &self.options.generated_at)?;
            }
            self.control_plane
                .mark_conflict_resolved(ConflictResolutionMark {
                    workspace_id: self.options.workspace_id.as_str().to_string(),
                    conflict_id: conflict.id.clone(),
                    resolved_by_device_id: self.options.device_id.as_str().to_string(),
                    resolution,
                })?;
            mark_conflict_remote_resolution_synced(&conflict, &self.options.generated_at)?;
        }
        Ok(())
    }

    fn import_full_snapshot(
        &self,
        snapshot_id: &SnapshotId,
    ) -> Result<SnapshotContent, SyncRunnerError> {
        if snapshot_id.as_str() == "empty" {
            return Ok(empty_snapshot_content(
                self.options.workspace_id.clone(),
                snapshot_id.clone(),
            ));
        }
        let imported = import_snapshot_by_id(
            &self.options.workspace_id,
            snapshot_id,
            self.control_plane,
            self.byte_store,
            self.options.storage_key,
            self.options.key_epoch,
        )?;
        self.hydrate_imported_snapshot(imported, ImportedHydrationSelection::AllFiles)
    }

    fn hydrate_imported_snapshot(
        &self,
        imported: ImportedSnapshot,
        selection: ImportedHydrationSelection,
    ) -> Result<SnapshotContent, SyncRunnerError> {
        let pack_epochs = pack_epochs_by_id(&imported.pack_objects)?;
        let cache = LocalContentCache::open(self.options.state_root.join("cache"))?;
        let mut pack_entry_counts = BTreeMap::<bowline_core::ids::PackId, usize>::new();
        let mut pack_hydration_counts = BTreeMap::<bowline_core::ids::PackId, usize>::new();
        for entry in &imported.manifest.entries {
            if entry.kind != NamespaceEntryKind::File {
                continue;
            }
            let Some(locator) = &entry.locator else {
                continue;
            };
            let Some(pack_id) = locator.pack_id.as_ref() else {
                continue;
            };
            *pack_entry_counts.entry(pack_id.clone()).or_default() += 1;
            if should_hydrate_imported_entry(entry, selection) {
                *pack_hydration_counts.entry(pack_id.clone()).or_default() += 1;
            }
        }
        for (pack_id, hydrate_count) in &pack_hydration_counts {
            if pack_entry_counts.get(pack_id) == Some(hydrate_count) {
                cache.prefetch_pack(self.byte_store, &ObjectKey::from_pack_id(pack_id)?)?;
            }
        }

        let mut files = BTreeMap::<ContentId, Vec<u8>>::new();
        let mut manifest = imported.manifest;
        for entry in &mut manifest.entries {
            if entry.kind != NamespaceEntryKind::File {
                continue;
            }
            if !should_hydrate_imported_entry(entry, selection) {
                entry.hydration_state = HydrationState::Cold;
                continue;
            }
            let Some(content_id) = &entry.content_id else {
                continue;
            };
            let Some(locator) = &entry.locator else {
                continue;
            };
            let pack_id = locator
                .pack_id
                .as_ref()
                .ok_or(SyncRunnerError::MissingPackedLocator("pack_id"))?;
            let key_epoch = pack_epochs
                .get(pack_id.as_str())
                .copied()
                .ok_or(SyncRunnerError::MissingPackedLocator("pack_object"))?;
            let object_key = ObjectKey::from_pack_id(pack_id)?;
            let bytes = cache.hydrate_record_from_range(
                self.byte_store,
                RangeHydrationRequest {
                    object_key: &object_key,
                    workspace_id: &self.options.workspace_id,
                    locator,
                    content_key: self.options.workspace_content_key,
                    key: self.options.storage_key,
                    key_epoch,
                },
            )?;
            files.insert(content_id.clone(), bytes);
            entry.hydration_state = HydrationState::Local;
        }
        Ok(SnapshotContent::new(manifest, files))
    }

    fn import_and_materialize_remote(
        &self,
        remote_ref: &WorkspaceRef,
        local_head: Option<&WorkspaceRef>,
    ) -> Result<(), SyncRunnerError> {
        self.import_remote_structure(remote_ref, local_head)?;
        Ok(())
    }

    fn import_remote_structure(
        &self,
        remote_ref: &WorkspaceRef,
        base_ref: Option<&WorkspaceRef>,
    ) -> Result<(), SyncRunnerError> {
        let store = MetadataStore::open(self.metadata_db_path())?;
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
        self.record_sync_checkpoint(
            "remote-import-started",
            "started",
            &format!(
                "{{\"snapshotId\":{},\"version\":{}}}",
                json_string(&remote_ref.snapshot_id),
                remote_ref.version,
            ),
        )?;
        let result: Result<(), SyncRunnerError> = (|| {
            let imported = import_snapshot_by_id(
                &self.options.workspace_id,
                &SnapshotId::new(remote_ref.snapshot_id.clone()),
                self.control_plane,
                self.byte_store,
                self.options.storage_key,
                self.options.key_epoch,
            )?;
            append_hydration_event(
                &store,
                EventName::HydrationStarted,
                EventSeverity::Info,
                &self.options,
                remote_ref,
                Some(&imported.manifest),
                None,
            );
            let base = base_ref
                .filter(|base_ref| base_ref.snapshot_id != "empty")
                .map(|base_ref| {
                    self.import_full_snapshot(&SnapshotId::new(base_ref.snapshot_id.clone()))
                })
                .transpose()?;
            let remote = self.hydrate_imported_snapshot(
                imported.clone(),
                ImportedHydrationSelection::EagerFiles,
            )?;
            materialize_snapshot(&self.options.root, base.as_ref(), &remote)?;
            self.record_sync_checkpoint(
                "remote-materialized",
                "completed",
                &format!(
                    "{{\"snapshotId\":{},\"entryCount\":{}}}",
                    json_string(&remote_ref.snapshot_id),
                    remote.manifest.entries.len(),
                ),
            )?;
            for pointer in &imported.pack_objects {
                let pack_id = pack_id_from_object_key(&pointer.object_key)?;
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
            for entry in &remote.manifest.entries {
                store.upsert_projected_node(&projected_node_for_entry(
                    &self.options.workspace_id,
                    entry,
                    &self.options.generated_at,
                ))?;
            }
            reconcile_materialized_hydration_queue(
                &store,
                &self.options.workspace_id,
                &self.options.generated_at,
            )?;
            let retained_paths = imported
                .manifest
                .entries
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<BTreeSet<_>>();
            store.delete_unlisted_workspace_projected_nodes(
                &self.options.workspace_id,
                &retained_paths,
            )?;
            append_hydration_event(
                &store,
                EventName::HydrationCompleted,
                EventSeverity::Info,
                &self.options,
                remote_ref,
                Some(&imported.manifest),
                None,
            );
            self.record_sync_checkpoint(
                "remote-import-completed",
                "completed",
                &format!(
                    "{{\"snapshotId\":{},\"packCount\":{},\"locatorCount\":{}}}",
                    json_string(&remote_ref.snapshot_id),
                    imported.pack_objects.len(),
                    imported.locators.len(),
                ),
            )?;
            Ok(())
        })();
        if let Err(error) = &result {
            let _ = self.record_sync_checkpoint(
                "remote-import-blocked",
                "blocked",
                &format!(
                    "{{\"snapshotId\":{},\"reason\":{}}}",
                    json_string(&remote_ref.snapshot_id),
                    json_string(&error.to_string()),
                ),
            );
            append_hydration_event(
                &store,
                EventName::HydrationBlocked,
                EventSeverity::Limited,
                &self.options,
                remote_ref,
                None,
                Some(&error.to_string()),
            );
        }
        result
    }

    fn upload_candidate_with_checkpoints(
        &self,
        candidate: &super::SnapshotCandidate,
    ) -> Result<UploadOutcome, SyncRunnerError> {
        upload_snapshot_candidate_with_checkpoints(
            candidate,
            self.control_plane,
            self.byte_store,
            self.options.storage_key,
            self.options.key_epoch,
            |step, payload| {
                self.record_sync_checkpoint(step, "completed", &payload)
                    .map_err(|error| UploadError::Checkpoint(error.to_string()))
            },
        )
        .map_err(Into::into)
    }

    fn persist_scan_metadata_if_committed(
        &self,
        candidate: &super::SnapshotCandidate,
        workspace_ref: &WorkspaceRef,
    ) -> Result<(), SyncRunnerError> {
        if candidate.snapshot.manifest.snapshot_id.as_str() != workspace_ref.snapshot_id {
            return Ok(());
        }
        self.persist_scan_metadata(candidate)
    }

    fn persist_fresh_scan_metadata_for_head(
        &self,
        workspace_ref: &WorkspaceRef,
    ) -> Result<(), SyncRunnerError> {
        let candidate = super::coalescer::coalesce_workspace_scan(
            &self.options.root,
            self.options.workspace_id.clone(),
            workspace_ref,
            self.options.device_id.clone(),
            self.options.workspace_content_key,
            self.options.generated_at.clone(),
        )?;
        self.persist_scan_metadata_if_committed(&candidate, workspace_ref)
    }

    fn record_sync_checkpoint(
        &self,
        step: &str,
        state: &str,
        payload_json: &str,
    ) -> Result<(), SyncRunnerError> {
        let Some(operation_id) = &self.options.sync_operation_id else {
            return Ok(());
        };
        let store = MetadataStore::open(self.metadata_db_path())?;
        store.append_sync_operation_checkpoint(&SyncOperationCheckpointRecord {
            id: sync_checkpoint_id(operation_id, step, state, payload_json),
            workspace_id: self.options.workspace_id.clone(),
            operation_id: operation_id.clone(),
            step: step.to_string(),
            state: state.to_string(),
            payload_json: payload_json.to_string(),
            created_at: self.options.generated_at.clone(),
            updated_at: self.options.generated_at.clone(),
        })?;
        Ok(())
    }

    fn preserved_base_entries(
        &self,
        base_ref: &WorkspaceRef,
        excluded_paths: &BTreeSet<String>,
    ) -> Result<Vec<bowline_core::workspace_graph::NamespaceEntry>, SyncRunnerError> {
        if base_ref.snapshot_id == "empty" {
            return Ok(Vec::new());
        }
        let mut preserved_paths = excluded_paths.clone();
        let metadata_path = self.metadata_db_path();
        if metadata_path.exists() {
            let store = MetadataStore::open(metadata_path)?;
            for node in store.projected_nodes_for_workspace(&self.options.workspace_id)? {
                if node.kind != NamespaceEntryKind::File
                    || node.hydration_state != HydrationState::Cold
                {
                    continue;
                }
                let local_path = self.options.root.join(Path::new(&node.path));
                if cold_placeholder_is_absent(&local_path)? {
                    for ancestor in ancestor_paths(&node.path) {
                        preserved_paths.insert(ancestor);
                    }
                    preserved_paths.insert(node.path);
                }
            }
        }
        if preserved_paths.is_empty() {
            return Ok(Vec::new());
        }
        let imported = import_snapshot_by_id(
            &self.options.workspace_id,
            &SnapshotId::new(base_ref.snapshot_id.clone()),
            self.control_plane,
            self.byte_store,
            self.options.storage_key,
            self.options.key_epoch,
        )?;
        Ok(imported
            .manifest
            .entries
            .into_iter()
            .filter(|entry| preserved_paths.contains(&entry.path))
            .collect())
    }

    fn persist_scan_metadata(
        &self,
        candidate: &super::SnapshotCandidate,
    ) -> Result<(), SyncRunnerError> {
        let metadata_path = self.metadata_db_path();
        if !metadata_path.exists() {
            return Ok(());
        }
        let mut store = MetadataStore::open(metadata_path)?;
        let report =
            workspace_scoped_scan_report(&self.options.workspace_id, &candidate.scan_report);
        if report.root.as_os_str().is_empty()
            && report.projects.is_empty()
            && report.paths.is_empty()
        {
            // Synthetic merge/test candidates may not originate from a live scan.
            return Ok(());
        }
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
        let projects = report
            .projects
            .iter()
            .map(|project| (project.id.clone(), project.path.clone()))
            .collect::<Vec<_>>();
        store.replace_projects(
            &self.options.workspace_id,
            &root_id,
            &projects,
            &self.options.generated_at,
        )?;
        let latest_snapshot_id =
            SnapshotId::new(candidate.snapshot.manifest.snapshot_id.as_str().to_string());
        for (project_id, _) in &projects {
            store.set_project_latest_snapshot_id(
                &self.options.workspace_id,
                project_id,
                &latest_snapshot_id,
            )?;
        }
        let paths = report
            .paths
            .iter()
            .map(|path| ObservedLocalPath {
                project_id: path.project_id.clone(),
                path: path.path.clone(),
                classification: path.policy.classification,
                mode: path.policy.mode,
                access: path.policy.access.clone(),
                matched_rule: path.policy.matched_rule.clone(),
                rule_source: path.policy.rule_source.clone(),
                risk: path.policy.risk.clone(),
                summary: path.policy.summary.clone(),
            })
            .collect::<Vec<_>>();
        store.replace_observed_paths(
            &self.options.workspace_id,
            &paths,
            &self.options.generated_at,
        )?;
        store.set_observed_summary(
            &self.options.workspace_id,
            &report.summary,
            &self.options.generated_at,
        )?;
        if let Err(error) = import_env_records_from_scan(
            &mut store,
            &self.options.workspace_id,
            &self.options.root,
            &report,
            self.options.workspace_content_key,
            &self.options.generated_at,
        ) {
            let _ = self.record_sync_checkpoint(
                "scan-env-metadata-import-skipped",
                "blocked",
                &format!("{{\"reason\":{}}}", json_string(&error.to_string())),
            );
        }
        let retained_paths = candidate
            .snapshot
            .manifest
            .entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect::<BTreeSet<_>>();
        store.delete_unlisted_workspace_projected_nodes(
            &self.options.workspace_id,
            &retained_paths,
        )?;
        for entry in &candidate.snapshot.manifest.entries {
            if entry.hydration_state != HydrationState::Local {
                continue;
            }
            store.upsert_projected_node(&projected_node_for_observed_entry(
                &self.options.workspace_id,
                entry,
                &self.options.generated_at,
            ))?;
        }
        Ok(())
    }

    fn read_local_head(&self) -> Result<Option<WorkspaceRef>, SyncRunnerError> {
        let metadata_path = self.metadata_db_path();
        if !metadata_path.exists() {
            return Ok(None);
        }
        let store = MetadataStore::open(metadata_path)?;
        Ok(store
            .workspace_sync_head(&self.options.workspace_id)?
            .map(|record| record.workspace_ref))
    }

    fn write_local_head(&self, workspace_ref: &WorkspaceRef) -> Result<(), SyncRunnerError> {
        let store = MetadataStore::open(self.metadata_db_path())?;
        store.upsert_workspace_sync_head(&WorkspaceSyncHeadRecord {
            workspace_ref: workspace_ref.clone(),
            observed_at: self.options.generated_at.clone(),
        })?;
        Ok(())
    }

    fn metadata_db_path(&self) -> PathBuf {
        self.options.state_root.join(DEFAULT_DATABASE_FILE)
    }
}

fn materialize_snapshot(
    root: &Path,
    base: Option<&SnapshotContent>,
    target: &SnapshotContent,
) -> Result<(), SyncRunnerError> {
    materialize_snapshot_excluding(root, base, target, &BTreeSet::new())
}

fn append_hydration_event(
    store: &MetadataStore,
    name: EventName,
    severity: EventSeverity,
    options: &SyncRunnerOptions,
    remote_ref: &WorkspaceRef,
    manifest: Option<&SnapshotManifest>,
    reason: Option<&str>,
) {
    let (file_count, byte_count) = manifest
        .map(|manifest| materialization_counts(&manifest.entries))
        .unwrap_or((0, 0));
    let summary = match name {
        EventName::HydrationStarted => format!(
            "Remote snapshot materialization started: {byte_count} byte(s) across {file_count} file(s)."
        ),
        EventName::HydrationCompleted => format!(
            "Remote snapshot materialization completed: {byte_count} byte(s) across {file_count} file(s)."
        ),
        EventName::HydrationBlocked => format!(
            "Remote snapshot materialization blocked: {}",
            reason.unwrap_or("unknown reason")
        ),
        _ => "Remote snapshot materialization updated.".to_string(),
    };
    let mut event = WorkspaceEvent::new(
        hydration_event_id(name, &remote_ref.snapshot_id, &options.generated_at),
        name,
        options.generated_at.clone(),
        severity,
        summary,
        options.workspace_id.clone(),
    );
    event.path = Some(options.root.display().to_string());
    event.device_id = Some(options.device_id.clone());
    event.subject = Some(EventSubject {
        kind: EventSubjectKind::Root,
        id: workspace_scoped_root_id(&options.workspace_id),
        path: Some(options.root.display().to_string()),
    });
    event.payload.insert(
        "cause".to_string(),
        serde_json::Value::String("remote-import".to_string()),
    );
    event.payload.insert(
        "snapshotId".to_string(),
        serde_json::Value::String(remote_ref.snapshot_id.clone()),
    );
    event
        .payload
        .insert("bytes".to_string(), serde_json::Value::from(byte_count));
    event
        .payload
        .insert("fileCount".to_string(), serde_json::Value::from(file_count));
    if let Some(reason) = reason {
        event.payload.insert(
            "reason".to_string(),
            serde_json::Value::String(reason.to_string()),
        );
    }
    let _ = store.append_event(event);
}

fn materialization_counts(entries: &[NamespaceEntry]) -> (usize, u64) {
    entries
        .iter()
        .filter(|entry| entry.kind == NamespaceEntryKind::File)
        .fold((0, 0), |(files, bytes), entry| {
            (files + 1, bytes + entry.byte_len.unwrap_or(0))
        })
}

fn should_hydrate_imported_entry(
    entry: &NamespaceEntry,
    selection: ImportedHydrationSelection,
) -> bool {
    match selection {
        ImportedHydrationSelection::AllFiles => true,
        ImportedHydrationSelection::EagerFiles => entry.mode != MaterializationMode::Lazy,
    }
}

fn hydration_event_id(name: EventName, snapshot_id: &str, now: &str) -> EventId {
    EventId::new(format!(
        "evt_hydration_{}_{}_{}",
        hydration_event_name(name),
        snapshot_id,
        event_id_component(now)
    ))
}

fn sync_checkpoint_id(operation_id: &str, step: &str, state: &str, payload_json: &str) -> String {
    let hash = blake3::hash(format!("{operation_id}:{step}:{state}:{payload_json}").as_bytes());
    format!(
        "sync-checkpoint-{}-{}-{}",
        event_id_component(operation_id),
        event_id_component(step),
        hash.to_hex().chars().take(12).collect::<String>(),
    )
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"<invalid>\"".to_string())
}

fn hydration_event_name(name: EventName) -> &'static str {
    match name {
        EventName::HydrationStarted => "started",
        EventName::HydrationCompleted => "completed",
        EventName::HydrationBlocked => "blocked",
        _ => "updated",
    }
}

fn event_id_component(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn projected_node_for_entry(
    workspace_id: &WorkspaceId,
    entry: &NamespaceEntry,
    updated_at: &str,
) -> ProjectedNodeRecord {
    ProjectedNodeRecord {
        workspace_id: workspace_id.clone(),
        node_id: format!("node:{}", entry.path),
        project_id: None,
        parent_node_id: parent_path(&entry.path).map(|path| format!("node:{path}")),
        path: entry.path.clone(),
        kind: entry.kind,
        content_id: entry.content_id.clone(),
        hydration_state: entry.hydration_state,
        updated_at: updated_at.to_string(),
    }
}

fn projected_node_for_observed_entry(
    workspace_id: &WorkspaceId,
    entry: &NamespaceEntry,
    updated_at: &str,
) -> ProjectedNodeRecord {
    ProjectedNodeRecord {
        workspace_id: workspace_id.clone(),
        node_id: format!("node:{}", entry.path),
        project_id: None,
        parent_node_id: parent_path(&entry.path).map(|path| format!("node:{path}")),
        path: entry.path.clone(),
        kind: entry.kind,
        content_id: entry.content_id.clone(),
        hydration_state: entry.hydration_state,
        updated_at: updated_at.to_string(),
    }
}

fn parent_path(path: &str) -> Option<&str> {
    path.rsplit_once('/')
        .map(|(parent, _)| parent)
        .filter(|parent| !parent.is_empty())
}

fn ancestor_paths(path: &str) -> Vec<String> {
    let mut ancestors = Vec::new();
    let mut current = path;
    while let Some(parent) = parent_path(current) {
        ancestors.push(parent.to_string());
        current = parent;
    }
    ancestors
}

fn cold_placeholder_is_absent(path: &Path) -> Result<bool, SyncRunnerError> {
    match path.try_exists() {
        Ok(exists) => Ok(!exists),
        Err(error) if error.kind() == io::ErrorKind::NotADirectory => Ok(false),
        Err(error) => Err(SyncRunnerError::StateIo(error)),
    }
}

fn pack_id_from_object_key(object_key: &str) -> Result<bowline_core::ids::PackId, SyncRunnerError> {
    let pack_id = object_key
        .strip_prefix("packs_")
        .ok_or(SyncRunnerError::MissingPackedLocator("object_key"))?;
    Ok(bowline_core::ids::PackId::new(pack_id))
}

fn pack_epochs_by_id(
    pack_objects: &[bowline_control_plane::ObjectPointer],
) -> Result<BTreeMap<String, u32>, SyncRunnerError> {
    pack_objects
        .iter()
        .map(|pointer| {
            let pack_id = pack_id_from_object_key(&pointer.object_key)?;
            Ok((pack_id.as_str().to_string(), pointer.key_epoch))
        })
        .collect()
}

fn materialize_snapshot_excluding(
    root: &Path,
    base: Option<&SnapshotContent>,
    target: &SnapshotContent,
    excluded_paths: &BTreeSet<String>,
) -> Result<(), SyncRunnerError> {
    let target_paths = target
        .manifest
        .entries
        .iter()
        .map(|entry| entry.path.as_str())
        .collect::<BTreeSet<_>>();

    if let Some(base) = base {
        let mut removed = base
            .manifest
            .entries
            .iter()
            .filter(|entry| {
                !target_paths.contains(entry.path.as_str())
                    && !is_excluded_materialization_path(&entry.path, excluded_paths)
            })
            .collect::<Vec<_>>();
        removed.sort_by_key(|entry| std::cmp::Reverse(entry.path.len()));
        for entry in removed {
            let absolute = root.join(&entry.path);
            match entry.kind {
                NamespaceEntryKind::File | NamespaceEntryKind::Symlink => {
                    remove_file_if_present(&absolute)?
                }
                NamespaceEntryKind::Directory => remove_empty_dir_if_present(&absolute)?,
                NamespaceEntryKind::Placeholder | NamespaceEntryKind::Tombstone => {}
            }
        }
    }

    let mut dirs = target
        .manifest
        .entries
        .iter()
        .filter(|entry| {
            entry.kind == NamespaceEntryKind::Directory
                && !is_excluded_materialization_path(&entry.path, excluded_paths)
        })
        .collect::<Vec<_>>();
    dirs.sort_by(|left, right| left.path.cmp(&right.path));
    for entry in dirs {
        ensure_directory_without_symlink(root, Path::new(&entry.path))?;
    }

    for entry in &target.manifest.entries {
        if is_excluded_materialization_path(&entry.path, excluded_paths) {
            continue;
        }
        match entry.kind {
            NamespaceEntryKind::File => {
                let Some(bytes) = target.file_bytes_for_path(&entry.path) else {
                    continue;
                };
                let relative_path = Path::new(&entry.path);
                prepare_parent_dirs(root, relative_path)?;
                let absolute = root.join(relative_path);
                write_materialized_file(
                    &absolute,
                    bytes,
                    materialized_file_requires_owner_only(&entry.path, entry.mode),
                )?;
            }
            NamespaceEntryKind::Symlink => {
                let Some(target_path) = &entry.symlink_target else {
                    continue;
                };
                validate_materialized_symlink_target(target_path)?;
                let relative_path = Path::new(&entry.path);
                prepare_parent_dirs(root, relative_path)?;
                let absolute = root.join(relative_path);
                write_materialized_symlink(&absolute, target_path)?;
            }
            NamespaceEntryKind::Directory => {}
            NamespaceEntryKind::Placeholder | NamespaceEntryKind::Tombstone => {}
        }
    }
    Ok(())
}

fn write_materialized_symlink(path: &Path, target: &str) -> Result<(), SyncRunnerError> {
    let temp_path = materialization_temp_path(path)?;
    remove_file_if_present(&temp_path)?;
    std::os::unix::fs::symlink(target, &temp_path).map_err(SyncRunnerError::StateIo)?;
    remove_directory_for_file_materialization(path)?;
    fs::rename(&temp_path, path).map_err(SyncRunnerError::StateIo)?;
    Ok(())
}

fn is_excluded_materialization_path(path: &str, excluded_paths: &BTreeSet<String>) -> bool {
    excluded_paths.iter().any(|excluded| {
        path == excluded
            || path
                .strip_prefix(excluded)
                .is_some_and(|suffix| suffix.starts_with('/'))
    })
}

fn materialized_file_requires_owner_only(path: &str, mode: MaterializationMode) -> bool {
    matches!(
        mode,
        MaterializationMode::ProjectEnv | MaterializationMode::EncryptedSync
    ) || is_secret_bearing_path(path)
}

fn is_secret_bearing_path(path: &str) -> bool {
    path.split('/')
        .any(|part| part == ".env" || part.starts_with(".env.") || part.ends_with(".env"))
}

fn write_materialized_file(
    path: &Path,
    bytes: &[u8],
    owner_only: bool,
) -> Result<(), SyncRunnerError> {
    let temp_path = materialization_temp_path(path)?;
    remove_file_if_present(&temp_path)?;
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let mode = if owner_only { 0o600 } else { 0o644 };
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temp_path)
            .map_err(SyncRunnerError::StateIo)?;
        file.write_all(bytes).map_err(SyncRunnerError::StateIo)?;
        file.sync_all().map_err(SyncRunnerError::StateIo)?;
    }

    #[cfg(not(unix))]
    {
        let _ = owner_only;
        fs::write(&temp_path, bytes).map_err(SyncRunnerError::StateIo)?;
    }
    remove_directory_for_file_materialization(path)?;
    fs::rename(&temp_path, path).map_err(SyncRunnerError::StateIo)?;
    Ok(())
}

fn materialization_temp_path(path: &Path) -> Result<PathBuf, SyncRunnerError> {
    let Some(parent) = path.parent() else {
        return Err(SyncRunnerError::UnsafeMaterializationPath(
            path.display().to_string(),
        ));
    };
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let slug = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let hash = blake3::hash(path.to_string_lossy().as_bytes());
    let suffix = hash.to_hex().chars().take(12).collect::<String>();
    Ok(parent.join(format!(".bowline-materialize-{slug}-{suffix}.tmp")))
}

fn remove_directory_for_file_materialization(path: &Path) -> Result<(), SyncRunnerError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            remove_empty_dir_if_present(path)
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(SyncRunnerError::StateIo(error)),
    }
}

fn validate_materialized_symlink_target(target: &str) -> Result<(), SyncRunnerError> {
    let normalized = normalize_workspace_path(target);
    if Path::new(target).is_absolute()
        || normalized != target
        || normalized.is_empty()
        || normalized == "."
        || normalized.starts_with("../")
        || normalized.contains("/../")
    {
        return Err(SyncRunnerError::UnsafeMaterializationPath(
            target.to_string(),
        ));
    }
    Ok(())
}

fn prepare_parent_dirs(root: &Path, relative_path: &Path) -> Result<(), SyncRunnerError> {
    let Some(parent) = relative_path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() {
        return Ok(());
    }
    ensure_directory_without_symlink(root, parent)
}

fn ensure_directory_without_symlink(
    root: &Path,
    relative_path: &Path,
) -> Result<(), SyncRunnerError> {
    let mut current = root.to_path_buf();
    for component in relative_path.components() {
        let std::path::Component::Normal(segment) = component else {
            return Err(SyncRunnerError::UnsafeMaterializationPath(
                relative_path.display().to_string(),
            ));
        };
        current.push(segment);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                fs::remove_file(&current).map_err(SyncRunnerError::StateIo)?;
                fs::create_dir(&current).map_err(SyncRunnerError::StateIo)?;
            }
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                fs::remove_file(&current).map_err(SyncRunnerError::StateIo)?;
                fs::create_dir(&current).map_err(SyncRunnerError::StateIo)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&current).map_err(SyncRunnerError::StateIo)?;
            }
            Err(error) => return Err(SyncRunnerError::StateIo(error)),
        }
    }
    Ok(())
}

fn remove_file_if_present(path: &Path) -> Result<(), SyncRunnerError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(SyncRunnerError::StateIo(error)),
    }
}

fn remove_empty_dir_if_present(path: &Path) -> Result<(), SyncRunnerError> {
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(SyncRunnerError::StateIo(error)),
    }
}

fn workspace_scoped_scan_report(
    workspace_id: &WorkspaceId,
    report: &crate::scanner::ScanReport,
) -> crate::scanner::ScanReport {
    let mut scoped = report.clone();
    let project_ids = scoped
        .projects
        .iter_mut()
        .map(|project| {
            let original = project.id.clone();
            project.id = workspace_scoped_project_id(workspace_id, &original);
            (original, project.id.clone())
        })
        .collect::<BTreeMap<_, _>>();
    for path in &mut scoped.paths {
        if let Some(project_id) = &path.project_id
            && let Some(scoped_project_id) = project_ids.get(project_id)
        {
            path.project_id = Some(scoped_project_id.clone());
        }
    }
    scoped
}

fn workspace_scoped_project_id(workspace_id: &WorkspaceId, project_id: &ProjectId) -> ProjectId {
    ProjectId::new(format!(
        "proj_{}_{}",
        id_component(workspace_id.as_str()),
        id_component(project_id.as_str())
    ))
}

fn workspace_scoped_root_id(workspace_id: &WorkspaceId) -> String {
    format!("root_{}", id_component(workspace_id.as_str()))
}

fn id_component(value: &str) -> String {
    let mut output = String::new();
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            output.push(character.to_ascii_lowercase());
        } else {
            output.push('_');
        }
    }
    while output.contains("__") {
        output = output.replace("__", "_");
    }
    output.trim_matches('_').to_string()
}

fn empty_snapshot_content(workspace_id: WorkspaceId, snapshot_id: SnapshotId) -> SnapshotContent {
    SnapshotContent::new(
        SnapshotManifest {
            schema_version: 1,
            snapshot_id: snapshot_id.clone(),
            workspace_id,
            project_id: None,
            kind: SnapshotKind::WorkspaceHead,
            base_snapshot_id: None,
            entries: Vec::new(),
            refs: vec![SnapshotRef {
                name: "workspace".to_string(),
                target_snapshot_id: snapshot_id,
                kind: RefKind::Workspace,
            }],
        },
        BTreeMap::new(),
    )
}

fn empty_workspace_ref(workspace_id: WorkspaceId) -> WorkspaceRef {
    WorkspaceRef {
        workspace_id: workspace_id.as_str().to_string(),
        version: 0,
        snapshot_id: "empty".to_string(),
        updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 0 },
        updated_by_device_id: None,
    }
}

fn conflict_files(
    record: &ConflictRecord,
    base: &SnapshotContent,
    local: &SnapshotContent,
    remote: &SnapshotContent,
) -> Vec<ConflictFile> {
    record
        .paths
        .iter()
        .map(|path| ConflictFile {
            relative_path: path.clone(),
            base: base.file_bytes_for_path(path).map(Vec::from),
            local: local.file_bytes_for_path(path).map(Vec::from),
            remote: remote.file_bytes_for_path(path).map(Vec::from),
        })
        .collect()
}

fn conflict_kind_name(record: &ConflictRecord) -> &'static str {
    match record.conflict_kind {
        super::ConflictKind::Text => "text",
        super::ConflictKind::StructuredText => "structured-text",
        super::ConflictKind::Binary => "binary",
        super::ConflictKind::OpaqueGit => "opaque-git",
        super::ConflictKind::DeleteEdit => "delete-edit",
        super::ConflictKind::PathShape => "path-shape",
        super::ConflictKind::EnvKey => "env-key",
    }
}

fn conflict_resolution_state(state: &str) -> Option<ConflictResolutionState> {
    match state {
        "accepted" => Some(ConflictResolutionState::Accepted),
        "rejected" => Some(ConflictResolutionState::Rejected),
        _ => None,
    }
}

#[derive(Debug)]
pub enum SyncRunnerError {
    Coalesce(CoalesceError),
    Upload(UploadError),
    Download(DownloadError),
    Cache(CacheError),
    Merge(MergeError),
    ConflictBundle(ConflictBundleError),
    WorkViewOverlay(WorkViewOverlaySyncError),
    ControlPlane(ControlPlaneError),
    Metadata(MetadataError),
    StateIo(io::Error),
    StateJson(serde_json::Error),
    UnsafeMaterializationPath(String),
    MissingPackedLocator(&'static str),
}

impl fmt::Display for SyncRunnerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Coalesce(error) => error.fmt(formatter),
            Self::Upload(error) => error.fmt(formatter),
            Self::Download(error) => error.fmt(formatter),
            Self::Cache(error) => error.fmt(formatter),
            Self::Merge(error) => error.fmt(formatter),
            Self::ConflictBundle(error) => error.fmt(formatter),
            Self::WorkViewOverlay(error) => error.fmt(formatter),
            Self::ControlPlane(error) => error.fmt(formatter),
            Self::Metadata(error) => error.fmt(formatter),
            Self::StateIo(error) => write!(formatter, "sync state I/O failed: {error}"),
            Self::StateJson(error) => write!(formatter, "sync state JSON failed: {error}"),
            Self::UnsafeMaterializationPath(path) => {
                write!(formatter, "unsafe materialization path: {path}")
            }
            Self::MissingPackedLocator(field) => {
                write!(formatter, "packed locator is missing {field}")
            }
        }
    }
}

impl Error for SyncRunnerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Coalesce(error) => Some(error),
            Self::Upload(error) => Some(error),
            Self::Download(error) => Some(error),
            Self::Cache(error) => Some(error),
            Self::Merge(error) => Some(error),
            Self::ConflictBundle(error) => Some(error),
            Self::WorkViewOverlay(error) => Some(error),
            Self::ControlPlane(error) => Some(error),
            Self::Metadata(error) => Some(error),
            Self::StateIo(error) => Some(error),
            Self::StateJson(error) => Some(error),
            Self::UnsafeMaterializationPath(_) => None,
            Self::MissingPackedLocator(_) => None,
        }
    }
}

impl From<CoalesceError> for SyncRunnerError {
    fn from(error: CoalesceError) -> Self {
        Self::Coalesce(error)
    }
}

impl From<UploadError> for SyncRunnerError {
    fn from(error: UploadError) -> Self {
        Self::Upload(error)
    }
}

impl From<DownloadError> for SyncRunnerError {
    fn from(error: DownloadError) -> Self {
        Self::Download(error)
    }
}

impl From<CacheError> for SyncRunnerError {
    fn from(error: CacheError) -> Self {
        Self::Cache(error)
    }
}

impl From<MergeError> for SyncRunnerError {
    fn from(error: MergeError) -> Self {
        Self::Merge(error)
    }
}

impl From<ConflictBundleError> for SyncRunnerError {
    fn from(error: ConflictBundleError) -> Self {
        Self::ConflictBundle(error)
    }
}

impl From<WorkViewOverlaySyncError> for SyncRunnerError {
    fn from(error: WorkViewOverlaySyncError) -> Self {
        Self::WorkViewOverlay(error)
    }
}

impl From<EnvImportError> for SyncRunnerError {
    fn from(error: EnvImportError) -> Self {
        Self::Metadata(MetadataError::InvalidStorageMetadata(error.to_string()))
    }
}

impl From<ControlPlaneError> for SyncRunnerError {
    fn from(error: ControlPlaneError) -> Self {
        Self::ControlPlane(error)
    }
}

impl From<bowline_storage::ByteStoreError> for SyncRunnerError {
    fn from(error: bowline_storage::ByteStoreError) -> Self {
        Self::Cache(CacheError::Store(error))
    }
}

impl From<MetadataError> for SyncRunnerError {
    fn from(error: MetadataError) -> Self {
        Self::Metadata(error)
    }
}

impl From<serde_json::Error> for SyncRunnerError {
    fn from(error: serde_json::Error) -> Self {
        Self::StateJson(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    use bowline_core::{
        policy::{AccessFlag, MaterializationMode, PathClassification},
        workspace_graph::{ContentLocator, ContentStorage, HydrationState, NamespaceEntry},
    };

    use crate::{
        metadata::{MetadataStore, WorkspaceSyncHeadRecord},
        workspace::TempWorkspace,
    };

    #[test]
    fn materialize_snapshot_replaces_symlink_parents_without_following_them() {
        let workspace = TempWorkspace::new("sync-materialize-symlink-parent").expect("workspace");
        let outside = TempWorkspace::new("sync-materialize-outside").expect("outside");
        std::os::unix::fs::symlink(outside.root(), workspace.root().join("app"))
            .expect("symlink parent");
        let snapshot = snapshot_with_file(
            WorkspaceId::new("ws_code"),
            "app/src/main.ts",
            b"export const value = 1;\n",
        );

        materialize_snapshot(workspace.root(), None, &snapshot).expect("materialize");

        assert!(
            outside
                .root()
                .join("src")
                .join("main.ts")
                .metadata()
                .is_err(),
            "materialization must not write through a local symlink parent"
        );
        assert_eq!(
            fs::read(workspace.root().join("app").join("src").join("main.ts"))
                .expect("workspace file"),
            b"export const value = 1;\n"
        );
        assert!(
            !fs::symlink_metadata(workspace.root().join("app"))
                .expect("app metadata")
                .file_type()
                .is_symlink(),
            "symlink parent should be replaced with a real directory"
        );
    }

    #[test]
    fn materialize_snapshot_rejects_symlink_targets_outside_workspace() {
        let workspace = TempWorkspace::new("sync-materialize-bad-symlink").expect("workspace");
        let snapshot = snapshot_with_symlink(
            WorkspaceId::new("ws_code"),
            "app/config",
            "/workspace/user/.ssh/config",
        );

        let error =
            materialize_snapshot(workspace.root(), None, &snapshot).expect_err("unsafe symlink");

        assert!(matches!(
            error,
            SyncRunnerError::UnsafeMaterializationPath(_)
        ));
        assert!(
            fs::symlink_metadata(workspace.root().join("app").join("config")).is_err(),
            "unsafe symlink target must not be materialized"
        );
    }

    #[test]
    fn materialize_snapshot_writes_secret_bearing_files_owner_only() {
        let workspace = TempWorkspace::new("sync-materialize-env-permissions").expect("workspace");
        let snapshot = snapshot_with_file(
            WorkspaceId::new("ws_code"),
            "app/.env.local",
            b"SECRET=value\n",
        );

        materialize_snapshot(workspace.root(), None, &snapshot).expect("materialize");

        let mode = fs::metadata(workspace.root().join("app").join(".env.local"))
            .expect("env metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn materialize_snapshot_replaces_files_with_atomic_temp_rename() {
        let workspace = TempWorkspace::new("sync-materialize-atomic-file").expect("workspace");
        let destination = workspace.root().join("app/src/index.ts");
        fs::create_dir_all(destination.parent().expect("destination parent")).expect("parent");
        fs::write(&destination, b"old bytes stay until rename\n").expect("old file");
        let stale_temp = materialization_temp_path(&destination).expect("temp path");
        fs::write(&stale_temp, b"crashed temp bytes\n").expect("stale temp");
        let snapshot = snapshot_with_file(
            WorkspaceId::new("ws_code"),
            "app/src/index.ts",
            b"new materialized bytes\n",
        );

        materialize_snapshot(workspace.root(), None, &snapshot).expect("materialize");

        assert_eq!(
            fs::read(&destination).expect("destination bytes"),
            b"new materialized bytes\n"
        );
        assert!(
            fs::symlink_metadata(&stale_temp).is_err(),
            "stale materialization temp file should be removed"
        );
    }

    #[test]
    fn materialize_snapshot_replaces_symlinks_with_atomic_temp_rename() {
        let workspace = TempWorkspace::new("sync-materialize-atomic-symlink").expect("workspace");
        let destination = workspace.root().join("app/current");
        fs::create_dir_all(destination.parent().expect("destination parent")).expect("parent");
        std::os::unix::fs::symlink("old-target", &destination).expect("old symlink");
        let stale_temp = materialization_temp_path(&destination).expect("temp path");
        std::os::unix::fs::symlink("crashed-target", &stale_temp).expect("stale temp symlink");
        let snapshot = snapshot_with_symlink(WorkspaceId::new("ws_code"), "app/current", "src");

        materialize_snapshot(workspace.root(), None, &snapshot).expect("materialize");

        assert_eq!(
            fs::read_link(&destination).expect("destination symlink"),
            PathBuf::from("src")
        );
        assert!(
            fs::symlink_metadata(&stale_temp).is_err(),
            "stale materialization temp symlink should be removed"
        );
    }

    #[test]
    fn sync_runner_persists_fresh_scan_metadata_for_status_and_work_views() {
        let workspace = TempWorkspace::new("sync-persists-scan-metadata").expect("workspace");
        let state = TempWorkspace::new("sync-persists-scan-state").expect("state");
        let project = workspace.root().join("app");
        fs::create_dir_all(project.join(".git")).expect("git marker");
        fs::write(project.join("README.md"), b"hello\n").expect("readme");
        fs::write(project.join(".env.local"), b"SECRET=value\n").expect("env");

        let workspace_id = WorkspaceId::new("ws_code");
        let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
        store
            .insert_workspace(&workspace_id, "User Code", "2026-06-29T04:00:00Z")
            .expect("workspace");
        store
            .insert_root(
                "root_code",
                &workspace_id,
                &workspace.root().display().to_string(),
                "2026-06-29T04:00:00Z",
            )
            .expect("root");
        store
            .upsert_workspace_sync_head(&WorkspaceSyncHeadRecord {
                workspace_ref: empty_workspace_ref(workspace_id.clone()),
                observed_at: "2026-06-29T04:00:00Z".to_string(),
            })
            .expect("head");
        drop(store);

        let candidate = super::super::coalescer::coalesce_workspace_scan(
            workspace.root(),
            workspace_id.clone(),
            &empty_workspace_ref(workspace_id.clone()),
            DeviceId::new("device_local"),
            [7_u8; 32],
            "2026-06-29T04:01:00Z",
        )
        .expect("candidate");
        let control_plane = bowline_control_plane::FakeControlPlaneClient::default();
        let byte_store = bowline_storage::LocalByteStore::open(state.root().join("objects"))
            .expect("byte store");
        let runner = SyncRunner::new(
            &control_plane,
            &byte_store,
            SyncRunnerOptions {
                root: workspace.root().to_path_buf(),
                state_root: state.root().to_path_buf(),
                workspace_id: workspace_id.clone(),
                device_id: DeviceId::new("device_local"),
                workspace_content_key: [7_u8; 32],
                storage_key: StorageKey::from_bytes([8_u8; 32]),
                key_epoch: 1,
                generated_at: "2026-06-29T04:01:00Z".to_string(),
                sync_operation_id: None,
            },
        );

        runner
            .persist_scan_metadata(&candidate)
            .expect("scan metadata persisted");

        let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
        let summary = store
            .observed_summary(&workspace_id)
            .expect("summary")
            .expect("summary present");
        assert_eq!(summary.repo_count, 1);
        assert_eq!(summary.env_file_count, 1);
        assert_eq!(
            store
                .current_project_by_path(&project.display().to_string())
                .expect("project lookup")
                .expect("project")
                .path,
            "app"
        );
        let project = store
            .current_project_by_path(&project.display().to_string())
            .expect("project lookup")
            .expect("project");
        assert!(project.id.as_str().contains(workspace_id.as_str()));
        assert_eq!(
            store
                .project_latest_snapshot_id(&workspace_id, &project.id)
                .expect("latest snapshot"),
            Some(candidate.snapshot.manifest.snapshot_id.clone())
        );
        assert_eq!(
            store
                .env_records(&workspace_id)
                .expect("env records")
                .into_iter()
                .map(|record| record.key_name)
                .collect::<Vec<_>>(),
            vec!["SECRET".to_string()]
        );
    }

    #[test]
    fn sync_runner_skips_scan_metadata_for_uncommitted_candidate() {
        let workspace =
            TempWorkspace::new("sync-skips-uncommitted-scan-metadata").expect("workspace");
        let state = TempWorkspace::new("sync-skips-uncommitted-scan-state").expect("state");
        let project = workspace.root().join("app");
        fs::create_dir_all(project.join(".git")).expect("git marker");
        fs::write(project.join("README.md"), b"local-only\n").expect("readme");

        let workspace_id = WorkspaceId::new("ws_code");
        let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
        store
            .insert_workspace(&workspace_id, "User Code", "2026-06-29T04:00:00Z")
            .expect("workspace");
        store
            .insert_root(
                "root_code",
                &workspace_id,
                &workspace.root().display().to_string(),
                "2026-06-29T04:00:00Z",
            )
            .expect("root");
        drop(store);

        let candidate = super::super::coalescer::coalesce_workspace_scan(
            workspace.root(),
            workspace_id.clone(),
            &empty_workspace_ref(workspace_id.clone()),
            DeviceId::new("device_local"),
            [7_u8; 32],
            "2026-06-29T04:01:00Z",
        )
        .expect("candidate");
        let control_plane = bowline_control_plane::FakeControlPlaneClient::default();
        let byte_store = bowline_storage::LocalByteStore::open(state.root().join("objects"))
            .expect("byte store");
        let runner = SyncRunner::new(
            &control_plane,
            &byte_store,
            SyncRunnerOptions {
                root: workspace.root().to_path_buf(),
                state_root: state.root().to_path_buf(),
                workspace_id: workspace_id.clone(),
                device_id: DeviceId::new("device_local"),
                workspace_content_key: [7_u8; 32],
                storage_key: StorageKey::from_bytes([8_u8; 32]),
                key_epoch: 1,
                generated_at: "2026-06-29T04:01:00Z".to_string(),
                sync_operation_id: None,
            },
        );
        let accepted_remote = WorkspaceRef {
            workspace_id: workspace_id.as_str().to_string(),
            version: 7,
            snapshot_id: "snap_remote_committed".to_string(),
            updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 7 },
            updated_by_device_id: Some("device_remote".to_string()),
        };

        runner
            .persist_scan_metadata_if_committed(&candidate, &accepted_remote)
            .expect("mismatched scan metadata is skipped");

        let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
        assert!(
            store
                .observed_summary(&workspace_id)
                .expect("summary lookup")
                .is_none()
        );
        assert!(
            store
                .current_project_by_path(&project.display().to_string())
                .expect("project lookup")
                .is_none()
        );
    }

    #[test]
    fn sync_runner_tolerates_stale_env_file_during_scan_metadata_persistence() {
        let workspace = TempWorkspace::new("sync-stale-env-metadata").expect("workspace");
        let state = TempWorkspace::new("sync-stale-env-state").expect("state");
        let project = workspace.root().join("app");
        fs::create_dir_all(project.join(".git")).expect("git marker");
        fs::write(project.join(".env.local"), b"SECRET=value\n").expect("env");

        let workspace_id = WorkspaceId::new("ws_code");
        let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
        store
            .insert_workspace(&workspace_id, "User Code", "2026-06-29T04:00:00Z")
            .expect("workspace");
        store
            .insert_root(
                "root_code",
                &workspace_id,
                &workspace.root().display().to_string(),
                "2026-06-29T04:00:00Z",
            )
            .expect("root");
        drop(store);

        let candidate = super::super::coalescer::coalesce_workspace_scan(
            workspace.root(),
            workspace_id.clone(),
            &empty_workspace_ref(workspace_id.clone()),
            DeviceId::new("device_local"),
            [7_u8; 32],
            "2026-06-29T04:01:00Z",
        )
        .expect("candidate");
        fs::remove_file(project.join(".env.local")).expect("remove stale env");
        let control_plane = bowline_control_plane::FakeControlPlaneClient::default();
        let byte_store = bowline_storage::LocalByteStore::open(state.root().join("objects"))
            .expect("byte store");
        let runner = SyncRunner::new(
            &control_plane,
            &byte_store,
            SyncRunnerOptions {
                root: workspace.root().to_path_buf(),
                state_root: state.root().to_path_buf(),
                workspace_id: workspace_id.clone(),
                device_id: DeviceId::new("device_local"),
                workspace_content_key: [7_u8; 32],
                storage_key: StorageKey::from_bytes([8_u8; 32]),
                key_epoch: 1,
                generated_at: "2026-06-29T04:01:00Z".to_string(),
                sync_operation_id: None,
            },
        );
        let accepted = WorkspaceRef {
            workspace_id: workspace_id.as_str().to_string(),
            version: 1,
            snapshot_id: candidate.snapshot.manifest.snapshot_id.as_str().to_string(),
            updated_at: bowline_control_plane::ControlPlaneTimestamp { tick: 1 },
            updated_by_device_id: Some("device_local".to_string()),
        };

        runner
            .persist_scan_metadata_if_committed(&candidate, &accepted)
            .expect("stale env metadata import does not fail committed scan persistence");

        let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
        assert!(
            store
                .observed_summary(&workspace_id)
                .expect("summary lookup")
                .is_some()
        );
        assert!(
            store
                .env_records(&workspace_id)
                .expect("env records")
                .is_empty()
        );
    }

    fn snapshot_with_file(workspace_id: WorkspaceId, path: &str, bytes: &[u8]) -> SnapshotContent {
        let content_id = bowline_core::workspace_graph::workspace_content_id([3_u8; 32], bytes);
        SnapshotContent::new(
            SnapshotManifest {
                schema_version: 1,
                snapshot_id: SnapshotId::new("snap_remote"),
                workspace_id,
                project_id: None,
                kind: SnapshotKind::WorkspaceHead,
                base_snapshot_id: None,
                entries: vec![NamespaceEntry {
                    path: path.to_string(),
                    kind: NamespaceEntryKind::File,
                    classification: PathClassification::WorkspaceSync,
                    mode: MaterializationMode::WorkspaceSync,
                    access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
                    content_id: Some(content_id.clone()),
                    locator: Some(ContentLocator {
                        content_id: content_id.clone(),
                        storage: ContentStorage::Packed,
                        raw_size: bytes.len() as u64,
                        pack_id: None,
                        offset: None,
                        length: None,
                        chunk_ids: Vec::new(),
                    }),
                    symlink_target: None,
                    byte_len: Some(bytes.len() as u64),
                    hydration_state: HydrationState::Local,
                }],
                refs: Vec::new(),
            },
            [(content_id, bytes.to_vec())].into_iter().collect(),
        )
    }

    fn snapshot_with_symlink(
        workspace_id: WorkspaceId,
        path: &str,
        target: &str,
    ) -> SnapshotContent {
        SnapshotContent::new(
            SnapshotManifest {
                schema_version: 1,
                snapshot_id: SnapshotId::new("snap_remote"),
                workspace_id,
                project_id: None,
                kind: SnapshotKind::WorkspaceHead,
                base_snapshot_id: None,
                entries: vec![NamespaceEntry {
                    path: path.to_string(),
                    kind: NamespaceEntryKind::Symlink,
                    classification: PathClassification::WorkspaceSync,
                    mode: MaterializationMode::WorkspaceSync,
                    access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
                    content_id: None,
                    locator: None,
                    symlink_target: Some(target.to_string()),
                    byte_len: None,
                    hydration_state: HydrationState::Local,
                }],
                refs: Vec::new(),
            },
            Default::default(),
        )
    }
}
