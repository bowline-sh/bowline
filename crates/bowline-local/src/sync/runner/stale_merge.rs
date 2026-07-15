use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Deref,
};

use bowline_control_plane::{ControlPlaneTimestamp, WorkspaceRef};

use super::{
    CandidateBase, CoalesceContext, ConflictBundleError, LocalHeadMetadataUpdate,
    MaterializationRequest, SyncRunner, SyncRunnerError, SyncTickOutcome, UploadError,
    UploadOutcome, conflict_files, materialize_snapshot_guarded, unresolved_conflict_paths,
    unresolved_conflict_upload_overrides,
};
use crate::sync::conflicts::load_conflict_record;
use crate::sync::merge::{MergeSnapshotsOptions, merge_snapshots_with_plugins};
use crate::sync::{
    MergeOutcome, MergeTreeInput, SnapshotContent, create_conflict_bundle,
    stale_merge_required_content_paths,
};

struct TransientSnapshot {
    snapshot: SnapshotContent,
    cleaned: bool,
}

impl TransientSnapshot {
    fn new(snapshot: SnapshotContent) -> Self {
        Self {
            snapshot,
            cleaned: false,
        }
    }

    fn cleanup(&mut self) -> std::io::Result<()> {
        self.snapshot.remove_lease_owned_files()?;
        self.cleaned = true;
        Ok(())
    }
}

impl Deref for TransientSnapshot {
    type Target = SnapshotContent;

    fn deref(&self) -> &Self::Target {
        &self.snapshot
    }
}

impl Drop for TransientSnapshot {
    fn drop(&mut self) {
        if !self.cleaned {
            let _cleanup_attempt = self.snapshot.remove_lease_owned_files();
        }
    }
}

impl<'a> SyncRunner<'a> {
    pub(super) fn resolve_stale_candidate(
        &self,
        candidate: crate::sync::SnapshotCandidate,
        current_ref: WorkspaceRef,
    ) -> Result<SyncTickOutcome, SyncRunnerError> {
        self.resolve_stale_candidate_with_reuse_retry(candidate, current_ref, true)
    }

    fn resolve_stale_candidate_with_reuse_retry(
        &self,
        candidate: crate::sync::SnapshotCandidate,
        current_ref: WorkspaceRef,
        allow_reuse_retry: bool,
    ) -> Result<SyncTickOutcome, SyncRunnerError> {
        let base_imported = self.import_snapshot_structure(&candidate.base.snapshot_id)?;
        let remote_imported = self.import_snapshot_structure(
            &bowline_core::ids::SnapshotId::new(current_ref.snapshot_id.clone()),
        )?;
        let required_paths = stale_merge_required_content_paths(&MergeTreeInput {
            base: &base_imported.snapshot,
            left: &candidate.snapshot,
            right: &remote_imported.snapshot,
            workspace_content_key: self.options.workspace_content_key,
        })?;
        let mut base = TransientSnapshot::new(self.hydrate_imported_snapshot(
            base_imported.snapshot,
            &base_imported.pack_pointers,
            super::ImportedHydrationSelection::Paths(required_paths.clone()),
        )?);
        let mut remote = TransientSnapshot::new(self.hydrate_imported_snapshot(
            remote_imported.snapshot,
            &remote_imported.pack_pointers,
            super::ImportedHydrationSelection::Paths(required_paths),
        )?);
        let remote_base = CandidateBase::from_remote(&current_ref);
        let project_plugins = self.project_merge_plugins()?;
        self.append_merge_plugin_approval_events(&project_plugins);
        let merge_cancellation = self.scan_namespace_cancellation()?;
        let merged_result = merge_snapshots_with_plugins(
            &base,
            &candidate,
            &remote,
            MergeSnapshotsOptions {
                remote_base,
                workspace_content_key: self.options.workspace_content_key,
                created_at: self.options.generated_at.clone(),
                plugins: &project_plugins.registry,
                cancellation: merge_cancellation.as_ref().map(|cancellation| {
                    cancellation as &dyn bowline_core::namespace_snapshot::NamespaceCancellation
                }),
            },
        );
        let merged = self
            .finish_claim_backed_namespace_operation(merge_cancellation.as_ref(), merged_result)?;
        let result = (|| match merged {
            MergeOutcome::Clean(merged) => {
                let outcome = match self.upload_candidate_with_checkpoints(&merged) {
                    Ok(outcome) => outcome,
                    Err(SyncRunnerError::Upload(UploadError::ReusedPackMissing { pack_id }))
                        if allow_reuse_retry =>
                    {
                        self.record_pack_reuse_disabled(&pack_id)?;
                        let rebuilt = self.rebuild_candidate_without_reuse(&candidate.base)?;
                        return self.resolve_stale_candidate_with_reuse_retry(
                            rebuilt,
                            current_ref,
                            false,
                        );
                    }
                    Err(error) => return Err(error),
                };
                if let UploadOutcome::Advanced { workspace_ref, .. } = &outcome {
                    self.append_merge_plugin_applied_events(
                        &project_plugins.registry.take_audit_records(),
                        &current_ref,
                    );
                    let bound_snapshot = outcome.bound_snapshot();
                    materialize_snapshot_guarded(
                        MaterializationRequest::all(
                            &self.options.state_root,
                            &self.options.root,
                            Some(&base),
                            &merged.snapshot,
                        ),
                        |boundary| self.authorize_materialization(workspace_ref, boundary),
                    )?;
                    self.complete_local_head(
                        workspace_ref,
                        LocalHeadMetadataUpdate::FreshScan { bound_snapshot },
                    )?;
                }
                Ok(SyncTickOutcome::Merged(Box::new(outcome)))
            }
            MergeOutcome::Conflicted(conflicts) => {
                let conflicted_paths = conflicts
                    .iter()
                    .flat_map(|conflict| conflict.paths.iter().cloned())
                    .collect::<BTreeSet<_>>();
                materialize_snapshot_guarded(
                    MaterializationRequest::excluding(
                        &self.options.state_root,
                        &self.options.root,
                        Some(&base),
                        &remote,
                        &conflicted_paths,
                    ),
                    |boundary| self.authorize_materialization(&current_ref, boundary),
                )?;
                let mut records = Vec::with_capacity(conflicts.len());
                for mut conflict in conflicts {
                    conflict.workspace_root = Some(self.options.root.clone());
                    conflict.base_snapshot_id =
                        Some(candidate.base.snapshot_id.as_str().to_string());
                    conflict.remote_snapshot_id =
                        Some(current_ref.snapshot_id.as_str().to_string());
                    let files = conflict_files(&conflict, &base, &candidate.snapshot, &remote)?;
                    let bundle =
                        create_conflict_bundle(&self.options.state_root, conflict, &files)?;
                    records.push(bundle.record);
                }
                self.complete_local_head(
                    &current_ref,
                    LocalHeadMetadataUpdate::FreshScan {
                        bound_snapshot: None,
                    },
                )?;
                for record in &mut records {
                    *record = load_conflict_record(&self.options.state_root, &record.id)?
                        .ok_or_else(|| ConflictBundleError::OccurrenceSuperseded {
                            conflict_id: record.id.clone(),
                            occurrence_version: record.occurrence_version,
                        })?;
                }
                Ok(SyncTickOutcome::Conflicted(records))
            }
        })();
        remote.cleanup().map_err(SyncRunnerError::StateIo)?;
        base.cleanup().map_err(SyncRunnerError::StateIo)?;
        result
    }

    fn rebuild_candidate_without_reuse(
        &self,
        base: &CandidateBase,
    ) -> Result<crate::sync::SnapshotCandidate, SyncRunnerError> {
        let base_ref = workspace_ref_from_candidate_base(base);
        let excluded_paths = unresolved_conflict_paths(&self.options.state_root)?;
        let conflict_upload_overrides =
            unresolved_conflict_upload_overrides(&self.options.state_root, &self.options.root)?;
        let preserved_entries = self.preserved_base_entries(&base_ref, &excluded_paths)?;
        let preparation_root = self.options.state_root.join("preparations");
        let scan_cancellation = self.scan_namespace_cancellation()?;
        let rebuilt = crate::sync::coalescer::coalesce_workspace_scan_excluding(
            &self.options.root,
            self.options.workspace_id.clone(),
            &base_ref,
            self.options.device_id.clone(),
            self.options.workspace_content_key,
            self.options.generated_at.clone(),
            CoalesceContext {
                paths: &excluded_paths,
                prior_snapshot: None,
                namespace_cancellation: scan_cancellation.as_ref().map(|cancellation| {
                    cancellation as &dyn bowline_core::namespace_snapshot::NamespaceCancellation
                }),
                preserved_entries: &preserved_entries,
                file_overrides: &conflict_upload_overrides,
                base_locators: &BTreeMap::new(),
                preparation_root: Some(&preparation_root),
            },
        );
        self.finish_namespace_scan(scan_cancellation.as_ref(), rebuilt)
    }
}

fn workspace_ref_from_candidate_base(base: &CandidateBase) -> WorkspaceRef {
    WorkspaceRef {
        workspace_id: base.workspace_id.clone(),
        version: base.version,
        snapshot_id: base.snapshot_id.clone(),
        updated_at: ControlPlaneTimestamp { tick: base.version },
        updated_by_device_id: None,
    }
}
