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

mod error;
mod helpers;
mod persistence;
#[cfg(test)]
mod tests;

pub use error::SyncRunnerError;
use helpers::*;

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
                let Some(base_ref) = ControlPlaneClient::get_workspace_ref(
                    self.control_plane,
                    self.options.workspace_id.as_str(),
                )
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
        ControlPlaneClient::publish_conflict_metadata(
            self.control_plane,
            ConflictMetadataPublish {
                workspace_id: self.options.workspace_id.as_str().to_string(),
                conflict_id: conflict.id.clone(),
                conflict_kind: conflict_kind_name(conflict).to_string(),
                paths: conflict.paths.clone(),
                contains_secrets: conflict.contains_secrets,
                base_snapshot_id: base_snapshot_id.as_str().to_string(),
                remote_snapshot_id: remote_snapshot_id.to_string(),
                detected_by_device_id: self.options.device_id.as_str().to_string(),
                bundle_object: None,
            },
        )?;
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
            ControlPlaneClient::mark_conflict_resolved(
                self.control_plane,
                ConflictResolutionMark {
                    workspace_id: self.options.workspace_id.as_str().to_string(),
                    conflict_id: conflict.id.clone(),
                    resolved_by_device_id: self.options.device_id.as_str().to_string(),
                    resolution,
                },
            )?;
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
}
