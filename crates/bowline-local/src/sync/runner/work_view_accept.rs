use std::collections::BTreeSet;

use bowline_core::workspace_graph::{
    RefKind, WorkspaceRef as SnapshotRef, normalize_workspace_path,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::*;
use crate::{
    metadata::{
        WorkViewAcceptCheckpointRecord, WorkViewAcceptCheckpointStep,
        WorkViewAcceptClaimTransition, WorkViewAcceptReviewReason,
    },
    sync::{CandidateBase, SnapshotCandidate, manifest_id_for_snapshot},
    work_views::snapshot_accept::{
        PreparedSnapshotAccept, SnapshotAcceptPrepareOutcome, finalize_snapshot_accept_under_claim,
        prepare_snapshot_accept,
    },
    work_views::{
        PartialExposedBaseAdvance, WorkViewAcceptReview, finalize_review_ready_under_claim,
        prepare_partial_exposed_base, publish_partial_exposed_base_under_claim,
    },
};

mod namespace;
use namespace::splice_current_page_graph;

impl SyncRunner<'_> {
    pub fn execute_work_view_accept(
        &self,
        input: WorkViewAcceptExecutionInput,
    ) -> Result<WorkViewAcceptExecutionOutcome, SyncRunnerError> {
        if !self.renew_accept_claim(&input.claim)? {
            return Ok(WorkViewAcceptExecutionOutcome::OwnershipLost);
        }
        let mut store = MetadataStore::open(self.options.state_root.join(DEFAULT_DATABASE_FILE))?;
        let work_view = store
            .work_view_by_id(&self.options.workspace_id, &input.work_view_id)?
            .ok_or_else(|| work_accept_error("work view is missing"))?;
        let selected = input
            .selected_paths
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let cache_root = self.options.state_root.join("cache");
        let base_ref = self
            .observed_base_ref
            .clone()
            .ok_or_else(|| work_accept_error("canonical workspace ref is missing"))?;
        let imported =
            self.import_snapshot_structure(&SnapshotId::new(base_ref.snapshot_id.clone()))?;
        let pack_pointers = imported.pack_pointers;
        let mut current = imported.snapshot;
        let prepared = loop {
            match prepare_snapshot_accept(
                &store,
                &work_view,
                &selected,
                Some(&cache_root),
                self.options.workspace_content_key,
                Some(&current),
            )
            .map_err(work_accept_view_error)?
            {
                SnapshotAcceptPrepareOutcome::HydrationRequired(paths) => {
                    current = self.hydrate_imported_snapshot(
                        current,
                        &pack_pointers,
                        ImportedHydrationSelection::Paths(paths),
                    )?;
                    continue;
                }
                SnapshotAcceptPrepareOutcome::AlreadyPublished(published) => {
                    let outcome = self.recover_published_accept(&mut store, &input, &work_view)?;
                    published.complete().map_err(work_accept_view_error)?;
                    return Ok(outcome);
                }
                SnapshotAcceptPrepareOutcome::Conflicted(conflicts) => {
                    if !self.publish_accept_review(
                        &store,
                        &input,
                        &work_view,
                        &WorkViewAcceptReview::MergeConflict {
                            path_count: conflicts.len(),
                        },
                    )? {
                        return Ok(WorkViewAcceptExecutionOutcome::OwnershipLost);
                    }
                    return Ok(WorkViewAcceptExecutionOutcome::ReviewRequired {
                        reason: WorkViewAcceptReviewReason::MergeConflict,
                        result_json: bounded_review_result("merge-conflict", conflicts.len()),
                    });
                }
                SnapshotAcceptPrepareOutcome::PolicyDrift(records) => {
                    if !self.publish_accept_review(
                        &store,
                        &input,
                        &work_view,
                        &WorkViewAcceptReview::PolicyDrift {
                            records: records.clone(),
                        },
                    )? {
                        return Ok(WorkViewAcceptExecutionOutcome::OwnershipLost);
                    }
                    return Ok(WorkViewAcceptExecutionOutcome::ReviewRequired {
                        reason: WorkViewAcceptReviewReason::PolicyDrift,
                        result_json: bounded_review_result("policy-drift", records.len()),
                    });
                }
                SnapshotAcceptPrepareOutcome::Prepared(prepared) => break prepared,
            }
        };
        let candidate = self.work_accept_candidate(
            &input.operation_id,
            &work_view,
            &base_ref,
            &current,
            &prepared,
        )?;
        if !self.record_accept_candidate(
            &store,
            &input,
            &candidate,
            &base_ref,
            current.manifest(),
        )? {
            return Ok(WorkViewAcceptExecutionOutcome::OwnershipLost);
        }
        if !self.record_accept_staged(&store, &input, &candidate)? {
            return Ok(WorkViewAcceptExecutionOutcome::OwnershipLost);
        }
        if input.claim.operation_id() == input.operation_id
            && self.accept_ref_already_published(&store, &input, &candidate, &base_ref)?
        {
            self.ensure_recovery_checkpoint(
                &store,
                &input,
                WorkViewAcceptCheckpointStep::WorkspaceRefPublished,
                &candidate.snapshot.manifest().snapshot_id,
                Some(base_ref.version),
            )?;
            return self
                .publish_prepared_accept(&mut store, &input, *prepared, &candidate, base_ref);
        }
        let upload = upload_snapshot_candidate_with_checkpoints(
            &candidate,
            self.control_plane,
            self.byte_store,
            self.options.storage_key,
            self.options.key_epoch,
            |step, payload| {
                if !self
                    .renew_accept_claim(&input.claim)
                    .map_err(|error| UploadError::Checkpoint(error.to_string()))?
                {
                    return Err(UploadError::Checkpoint(
                        "work-view accept claim ownership was lost".to_string(),
                    ));
                }
                if step == "workspace-ref-cas-authorized" {
                    if !prepared
                        .main_fence_unchanged()
                        .map_err(|error| UploadError::Checkpoint(error.to_string()))?
                    {
                        return Err(UploadError::Checkpoint(
                            "work-view accept main fence changed".to_string(),
                        ));
                    }
                    if !self.record_accept_objects_uploaded(&store, &input, &candidate, &payload)?
                        || !self.record_accept_main_fence(&store, &input, &candidate)?
                    {
                        return Err(UploadError::Checkpoint(
                            "work-view accept claim ownership was lost".to_string(),
                        ));
                    }
                }
                Ok(())
            },
        )?;
        match upload {
            UploadOutcome::Stale { stale, .. } => Ok(WorkViewAcceptExecutionOutcome::RetryStale {
                workspace_ref: stale.current,
            }),
            UploadOutcome::Advanced { workspace_ref, .. } => {
                if !self.record_accept_ref_published(&store, &input, &candidate, &workspace_ref)? {
                    return Ok(WorkViewAcceptExecutionOutcome::OwnershipLost);
                }
                self.publish_prepared_accept(
                    &mut store,
                    &input,
                    *prepared,
                    &candidate,
                    workspace_ref,
                )
            }
        }
    }

    fn work_accept_candidate(
        &self,
        operation_id: &str,
        work_view: &bowline_core::work_views::WorkView,
        base_ref: &WorkspaceRef,
        current: &SnapshotContent,
        prepared: &PreparedSnapshotAccept,
    ) -> Result<SnapshotCandidate, SyncRunnerError> {
        let prefix = normalize_workspace_path(&work_view.project_path);
        let mut namespace = splice_current_page_graph(
            current,
            &prefix,
            prepared.branch_paths(),
            prepared.merged_entries(),
            Some(SnapshotId::new(base_ref.snapshot_id.clone())),
        )?;
        let snapshot_id = namespace.snapshot_id.clone();
        namespace.metadata.refs = vec![SnapshotRef {
            name: "workspace".to_string(),
            target_snapshot_id: snapshot_id.clone(),
            kind: RefKind::Workspace,
        }];
        let snapshot = SnapshotContent::from_built(namespace, prepared.prepared_content().clone());
        let identity =
            super::super::manifest_identity::manifest_identity_from_manifest(snapshot.manifest());
        let scan_report = crate::scanner::scan_workspace(&self.options.root)
            .map_err(|error| work_accept_error(&format!("workspace scan failed: {error}")))?;
        Ok(SnapshotCandidate {
            base: CandidateBase {
                workspace_id: self.options.workspace_id.clone(),
                version: base_ref.version,
                snapshot_id: SnapshotId::new(base_ref.snapshot_id.clone()),
            },
            device_id: self.options.device_id.clone(),
            manifest_id: manifest_id_for_snapshot(&snapshot_id),
            snapshot,
            scan_report,
            scan_scope: ScanScope::Full(FullScanReason::ReconcileFallback),
            stat_cache_hit_paths: BTreeSet::new(),
            stat_cache_divergences: Vec::new(),
            scan_stats: super::super::ScanStats::default(),
            manifest_identity: identity,
            stat_cache_write_back: None,
            causation_ids: vec![operation_id.to_string()],
            skipped_unsafe_symlinks: BTreeSet::new(),
            created_at: self.options.generated_at.clone(),
        })
    }

    fn publish_prepared_accept(
        &self,
        store: &mut MetadataStore,
        input: &WorkViewAcceptExecutionInput,
        prepared: PreparedSnapshotAccept,
        candidate: &SnapshotCandidate,
        workspace_ref: WorkspaceRef,
    ) -> Result<WorkViewAcceptExecutionOutcome, SyncRunnerError> {
        let work_view = prepared.work_view().clone();
        let completion_at = store
            .work_view_accept_operation(&input.operation_id)?
            .ok_or_else(|| work_accept_error("accept operation is missing"))?
            .created_at;
        let published = prepared
            .publish(|| {
                if self
                    .renew_accept_claim(&input.claim)
                    .map_err(io::Error::other)?
                {
                    Ok(())
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "work-view accept claim ownership was lost",
                    ))
                }
            })
            .map_err(work_accept_view_error)?;
        if !self.record_accept_main_published(store, input, candidate)? {
            return Ok(WorkViewAcceptExecutionOutcome::OwnershipLost);
        }
        store.upsert_workspace_sync_head(&WorkspaceSyncHeadRecord {
            workspace_ref: workspace_ref.clone(),
            observed_at: now_timestamp()?,
        })?;
        let metadata_published = if input.selected_paths.is_empty() {
            if !self.renew_accept_claim(&input.claim)? {
                return Ok(WorkViewAcceptExecutionOutcome::OwnershipLost);
            }
            finalize_snapshot_accept_under_claim(
                store,
                &work_view,
                &input.claim,
                &completion_at,
                &now_timestamp()?,
            )
            .map_err(work_accept_view_error)?
        } else {
            let selected_paths = input.selected_paths.iter().cloned().collect();
            let prepared_base = prepare_partial_exposed_base(PartialExposedBaseAdvance {
                store,
                work_view: &work_view,
                selected_paths: &selected_paths,
                target_snapshot: &candidate.snapshot,
                cache_root: &self.options.state_root.join("cache"),
                workspace_content_key: self.options.workspace_content_key,
                captured_at: &completion_at,
            })
            .map_err(work_accept_view_error)?;
            if !self.renew_accept_claim(&input.claim)? {
                return Ok(WorkViewAcceptExecutionOutcome::OwnershipLost);
            }
            publish_partial_exposed_base_under_claim(
                store,
                prepared_base,
                &input.claim,
                &now_timestamp()?,
            )
            .map_err(work_accept_view_error)?
            .is_some()
        };
        if !metadata_published {
            return Ok(WorkViewAcceptExecutionOutcome::OwnershipLost);
        }
        if !self.record_accept_lifecycle_published(store, input, candidate)? {
            return Ok(WorkViewAcceptExecutionOutcome::OwnershipLost);
        }
        published.complete().map_err(work_accept_view_error)?;
        Ok(WorkViewAcceptExecutionOutcome::Completed {
            workspace_ref,
            snapshot_id: candidate.snapshot.manifest().snapshot_id.clone(),
            cancelled_late: self.cancellation_requested_after_commit(),
        })
    }

    fn recover_published_accept(
        &self,
        store: &mut MetadataStore,
        input: &WorkViewAcceptExecutionInput,
        work_view: &bowline_core::work_views::WorkView,
    ) -> Result<WorkViewAcceptExecutionOutcome, SyncRunnerError> {
        let operation = store
            .work_view_accept_operation(&input.operation_id)?
            .ok_or_else(|| work_accept_error("accept operation is missing"))?;
        let completion_at = operation.created_at.clone();
        let snapshot_id = operation
            .target_snapshot_id
            .ok_or_else(|| work_accept_error("published accept target snapshot is missing"))?;
        let workspace_ref = self
            .control_plane
            .get_workspace_ref(&self.options.workspace_id)
            .map_err(UploadError::ControlPlane)?
            .ok_or_else(|| work_accept_error("published workspace ref is missing"))?;
        if workspace_ref.snapshot_id != snapshot_id.as_str() {
            return Err(work_accept_error(
                "published local accept does not match hosted workspace ref",
            ));
        }
        self.ensure_recovery_checkpoint(
            store,
            input,
            WorkViewAcceptCheckpointStep::WorkspaceRefPublished,
            &snapshot_id,
            Some(workspace_ref.version),
        )?;
        self.ensure_recovery_checkpoint(
            store,
            input,
            WorkViewAcceptCheckpointStep::MainPublished,
            &snapshot_id,
            None,
        )?;
        store.upsert_workspace_sync_head(&WorkspaceSyncHeadRecord {
            workspace_ref: workspace_ref.clone(),
            observed_at: now_timestamp()?,
        })?;
        let metadata_published = if input.selected_paths.is_empty() {
            if !self.renew_accept_claim(&input.claim)? {
                return Ok(WorkViewAcceptExecutionOutcome::OwnershipLost);
            }
            finalize_snapshot_accept_under_claim(
                store,
                work_view,
                &input.claim,
                &completion_at,
                &now_timestamp()?,
            )
            .map_err(work_accept_view_error)?
        } else {
            let selected_paths = input.selected_paths.iter().cloned().collect();
            let descriptor = store
                .work_view_exposed_base(&work_view.workspace_id, &work_view.id)?
                .ok_or_else(|| work_accept_error("work-view exposed base is missing"))?;
            let selected_workspace_paths = input
                .selected_paths
                .iter()
                .map(|relative| {
                    if descriptor.project_prefix.is_empty() {
                        relative.clone()
                    } else {
                        format!(
                            "{}/{}",
                            descriptor.project_prefix.trim_end_matches('/'),
                            relative
                        )
                    }
                })
                .collect();
            let imported = self.import_snapshot_structure(&snapshot_id)?;
            let target = self.hydrate_imported_snapshot(
                imported.snapshot,
                &imported.pack_pointers,
                ImportedHydrationSelection::Paths(selected_workspace_paths),
            )?;
            let prepared_base = prepare_partial_exposed_base(PartialExposedBaseAdvance {
                store,
                work_view,
                selected_paths: &selected_paths,
                target_snapshot: &target,
                cache_root: &self.options.state_root.join("cache"),
                workspace_content_key: self.options.workspace_content_key,
                captured_at: &completion_at,
            })
            .map_err(work_accept_view_error)?;
            if !self.renew_accept_claim(&input.claim)? {
                return Ok(WorkViewAcceptExecutionOutcome::OwnershipLost);
            }
            publish_partial_exposed_base_under_claim(
                store,
                prepared_base,
                &input.claim,
                &now_timestamp()?,
            )
            .map_err(work_accept_view_error)?
            .is_some()
        };
        if !metadata_published {
            return Ok(WorkViewAcceptExecutionOutcome::OwnershipLost);
        }
        self.ensure_recovery_checkpoint(
            store,
            input,
            WorkViewAcceptCheckpointStep::LifecyclePublished,
            &snapshot_id,
            None,
        )?;
        Ok(WorkViewAcceptExecutionOutcome::Completed {
            workspace_ref,
            snapshot_id,
            cancelled_late: self.cancellation_requested_after_commit(),
        })
    }

    pub(super) fn renew_accept_claim(
        &self,
        claim: &WorkViewAcceptClaimHandle,
    ) -> Result<bool, SyncRunnerError> {
        let store = MetadataStore::open(self.options.state_root.join(DEFAULT_DATABASE_FILE))?;
        let now_time = OffsetDateTime::now_utc();
        let now = format_time(now_time)?;
        let lease_expires_at = format_time(now_time + time::Duration::seconds(60))?;
        Ok(
            store.renew_work_view_accept_claim(claim, &now, &lease_expires_at)?
                == WorkViewAcceptClaimTransition::Applied,
        )
    }

    fn publish_accept_review(
        &self,
        store: &MetadataStore,
        input: &WorkViewAcceptExecutionInput,
        work_view: &bowline_core::work_views::WorkView,
        review: &WorkViewAcceptReview,
    ) -> Result<bool, SyncRunnerError> {
        if !self.renew_accept_claim(&input.claim)? {
            return Ok(false);
        }
        let now = now_timestamp()?;
        Ok(
            finalize_review_ready_under_claim(store, work_view, review, &input.claim, &now)
                .map_err(work_accept_view_error)?
                .is_some(),
        )
    }
}

fn bounded_review_result(code: &str, count: usize) -> String {
    serde_json::json!({ "reasonCode": code, "count": count }).to_string()
}

fn work_accept_error(message: &str) -> SyncRunnerError {
    SyncRunnerError::StateIo(io::Error::other(message.to_string()))
}

fn work_accept_view_error(error: crate::work_views::WorkViewError) -> SyncRunnerError {
    work_accept_error(&error.to_string())
}

fn format_time(timestamp: OffsetDateTime) -> Result<String, SyncRunnerError> {
    timestamp
        .format(&Rfc3339)
        .map_err(|error| work_accept_error(&format!("timestamp format failed: {error}")))
}

pub(super) fn now_timestamp() -> Result<String, SyncRunnerError> {
    format_time(OffsetDateTime::now_utc())
}

fn checkpoint_timestamp() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("UTC timestamps always format as RFC3339")
}

impl SyncRunner<'_> {
    fn checkpoint(
        &self,
        input: &WorkViewAcceptExecutionInput,
        step: WorkViewAcceptCheckpointStep,
        payload_json: String,
    ) -> WorkViewAcceptCheckpointRecord {
        WorkViewAcceptCheckpointRecord {
            id: format!(
                "wvaccept_{}_{}_{}_{}",
                input.operation_id,
                input.claim.generation(),
                format!("{step:?}").to_lowercase(),
                &blake3::hash(payload_json.as_bytes()).to_hex()[..12],
            ),
            workspace_id: self.options.workspace_id.clone(),
            operation_id: input.operation_id.clone(),
            claim_generation: input.claim.generation(),
            step,
            payload_json,
            created_at: checkpoint_timestamp(),
        }
    }

    fn record_accept_candidate(
        &self,
        store: &MetadataStore,
        input: &WorkViewAcceptExecutionInput,
        candidate: &SnapshotCandidate,
        base_ref: &WorkspaceRef,
        current: &SnapshotManifest,
    ) -> Result<bool, SyncRunnerError> {
        let checkpoint = self.checkpoint(
            input,
            WorkViewAcceptCheckpointStep::CandidateBuilt,
            serde_json::json!({
                "observedMainSnapshotId": current.snapshot_id.as_str(),
                "observedRefSnapshotId": base_ref.snapshot_id,
                "observedRefVersion": base_ref.version,
                "targetSnapshotId": candidate.snapshot.manifest().snapshot_id.as_str(),
            })
            .to_string(),
        );
        Ok(store.record_work_view_accept_candidate(
            &input.claim,
            &checkpoint,
            &crate::metadata::WorkViewAcceptCandidateObservation {
                observed_main_snapshot_id: current.snapshot_id.clone(),
                observed_ref_version: base_ref.version,
                observed_ref_snapshot_id: SnapshotId::new(base_ref.snapshot_id.clone()),
                target_snapshot_id: candidate.snapshot.manifest().snapshot_id.clone(),
            },
            &now_timestamp()?,
        )? == WorkViewAcceptClaimTransition::Applied)
    }

    fn record_accept_staged(
        &self,
        store: &MetadataStore,
        input: &WorkViewAcceptExecutionInput,
        candidate: &SnapshotCandidate,
    ) -> Result<bool, SyncRunnerError> {
        let checkpoint = self.checkpoint(
            input,
            WorkViewAcceptCheckpointStep::SnapshotStaged,
            serde_json::json!({"snapshotId": candidate.snapshot.manifest().snapshot_id.as_str()})
                .to_string(),
        );
        Ok(store.mark_work_view_accept_uploaded_or_staged(
            &input.claim,
            &checkpoint,
            &candidate.snapshot.manifest().snapshot_id,
            &now_timestamp()?,
        )? == WorkViewAcceptClaimTransition::Applied)
    }

    fn record_accept_objects_uploaded(
        &self,
        store: &MetadataStore,
        input: &WorkViewAcceptExecutionInput,
        candidate: &SnapshotCandidate,
        payload: &str,
    ) -> Result<bool, UploadError> {
        let checkpoint = self.checkpoint(
            input,
            WorkViewAcceptCheckpointStep::ObjectsUploaded,
            payload.to_string(),
        );
        store
            .mark_work_view_accept_uploaded_or_staged(
                &input.claim,
                &checkpoint,
                &candidate.snapshot.manifest().snapshot_id,
                &checkpoint_timestamp(),
            )
            .map(|transition| transition == WorkViewAcceptClaimTransition::Applied)
            .map_err(|error| UploadError::Checkpoint(error.to_string()))
    }

    fn record_accept_main_fence(
        &self,
        store: &MetadataStore,
        input: &WorkViewAcceptExecutionInput,
        candidate: &SnapshotCandidate,
    ) -> Result<bool, UploadError> {
        let checkpoint = self.checkpoint(
            input,
            WorkViewAcceptCheckpointStep::MainFenceRechecked,
            serde_json::json!({"snapshotId": candidate.snapshot.manifest().snapshot_id.as_str()})
                .to_string(),
        );
        store
            .append_work_view_accept_checkpoint(
                &input.claim,
                &checkpoint,
                &now_timestamp().map_err(|error| UploadError::Checkpoint(error.to_string()))?,
            )
            .map(|transition| transition == WorkViewAcceptClaimTransition::Applied)
            .map_err(|error| UploadError::Checkpoint(error.to_string()))
    }

    fn record_accept_ref_published(
        &self,
        store: &MetadataStore,
        input: &WorkViewAcceptExecutionInput,
        candidate: &SnapshotCandidate,
        workspace_ref: &WorkspaceRef,
    ) -> Result<bool, SyncRunnerError> {
        let checkpoint = self.checkpoint(
            input,
            WorkViewAcceptCheckpointStep::WorkspaceRefPublished,
            serde_json::json!({"snapshotId": candidate.snapshot.manifest().snapshot_id.as_str(), "version": workspace_ref.version}).to_string(),
        );
        Ok(store.append_work_view_accept_checkpoint(
            &input.claim,
            &checkpoint,
            &now_timestamp()?,
        )? == WorkViewAcceptClaimTransition::Applied)
    }

    fn record_accept_lifecycle_published(
        &self,
        store: &MetadataStore,
        input: &WorkViewAcceptExecutionInput,
        candidate: &SnapshotCandidate,
    ) -> Result<bool, SyncRunnerError> {
        let checkpoint = self.checkpoint(
            input,
            WorkViewAcceptCheckpointStep::LifecyclePublished,
            serde_json::json!({"snapshotId": candidate.snapshot.manifest().snapshot_id.as_str()})
                .to_string(),
        );
        Ok(store.append_work_view_accept_checkpoint(
            &input.claim,
            &checkpoint,
            &now_timestamp()?,
        )? == WorkViewAcceptClaimTransition::Applied)
    }

    fn record_accept_main_published(
        &self,
        store: &MetadataStore,
        input: &WorkViewAcceptExecutionInput,
        candidate: &SnapshotCandidate,
    ) -> Result<bool, SyncRunnerError> {
        let checkpoint = self.checkpoint(
            input,
            WorkViewAcceptCheckpointStep::MainPublished,
            serde_json::json!({"snapshotId": candidate.snapshot.manifest().snapshot_id.as_str()})
                .to_string(),
        );
        Ok(store.append_work_view_accept_checkpoint(
            &input.claim,
            &checkpoint,
            &now_timestamp()?,
        )? == WorkViewAcceptClaimTransition::Applied)
    }

    fn accept_ref_already_published(
        &self,
        store: &MetadataStore,
        input: &WorkViewAcceptExecutionInput,
        candidate: &SnapshotCandidate,
        base_ref: &WorkspaceRef,
    ) -> Result<bool, SyncRunnerError> {
        if base_ref.snapshot_id != candidate.snapshot.manifest().snapshot_id.as_str() {
            return Ok(false);
        }
        Ok(store
            .work_view_accept_operation(&input.operation_id)?
            .and_then(|operation| operation.target_snapshot_id)
            .is_some_and(|target| target == candidate.snapshot.manifest().snapshot_id))
    }

    fn ensure_recovery_checkpoint(
        &self,
        store: &MetadataStore,
        input: &WorkViewAcceptExecutionInput,
        step: WorkViewAcceptCheckpointStep,
        snapshot_id: &SnapshotId,
        version: Option<u64>,
    ) -> Result<(), SyncRunnerError> {
        if store
            .work_view_accept_checkpoints(&input.operation_id)?
            .iter()
            .any(|checkpoint| checkpoint.step == step)
        {
            return Ok(());
        }
        let checkpoint = self.checkpoint(
            input,
            step,
            serde_json::json!({
                "snapshotId": snapshot_id.as_str(),
                "version": version,
                "recovered": true,
            })
            .to_string(),
        );
        if store.append_work_view_accept_checkpoint(&input.claim, &checkpoint, &now_timestamp()?)?
            != WorkViewAcceptClaimTransition::Applied
        {
            return Err(work_accept_error(
                "work-view accept ownership was lost during recovery",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod splice_tests {
    use std::collections::BTreeMap;

    use bowline_core::{
        ids::{ContentId, WorkspaceId},
        policy::{AccessFlag, MaterializationMode, PathClassification},
        workspace_graph::{
            FileExecutability, HydrationState, NamespaceEntryKind, SNAPSHOT_SCHEMA_VERSION,
            SnapshotDraft, SnapshotKind,
        },
    };

    use super::*;
    use crate::sync::rebuild_manifest_identity;

    #[test]
    fn target_manifest_preserves_nested_never_exposed_secret_and_ancestor() {
        let hidden_id = ContentId::new("cid_hidden_secret");
        let workspace_id = WorkspaceId::new("ws_code");
        let current_entries = vec![
            entry("apps/web/credentials", NamespaceEntryKind::Directory, None),
            entry(
                "apps/web/credentials/id_rsa",
                NamespaceEntryKind::File,
                Some(hidden_id.clone()),
            ),
            entry(
                "apps/web/src/lib.rs",
                NamespaceEntryKind::File,
                Some(ContentId::new("cid_old")),
            ),
        ];
        let snapshot_id =
            rebuild_manifest_identity(&workspace_id, &current_entries, "test").snapshot_id;
        let current = SnapshotContent::new(
            SnapshotDraft {
                schema_version: SNAPSHOT_SCHEMA_VERSION,
                snapshot_id,
                workspace_id,
                project_id: None,
                kind: SnapshotKind::WorkspaceHead,
                base_snapshot_id: None,
                entries: current_entries,
                refs: Vec::new(),
            },
            BTreeMap::new(),
            [7; 32],
        )
        .expect("page-backed current snapshot");
        let merged = vec![entry(
            "src/lib.rs",
            NamespaceEntryKind::File,
            Some(ContentId::new("cid_new")),
        )];
        let namespace = splice_current_page_graph(
            &current,
            "apps/web",
            &BTreeSet::from(["credentials".to_string(), "src/lib.rs".to_string()]),
            &merged,
            None,
        )
        .expect("splice current page graph");
        let target = SnapshotContent::from_built(namespace, BTreeMap::new());

        assert!(
            target
                .entry_for_path("apps/web/credentials")
                .expect("read preserved ancestor")
                .is_some()
        );
        let hidden = target
            .entry_for_path("apps/web/credentials/id_rsa")
            .expect("read hidden path")
            .expect("hidden secret remains in canonical target manifest");
        assert_eq!(hidden.content_id.as_ref(), Some(&hidden_id));
        assert!(hidden.access.contains(&AccessFlag::AgentHidden));
    }

    fn entry(
        path: &str,
        kind: NamespaceEntryKind,
        content_id: Option<ContentId>,
    ) -> NamespaceEntry {
        NamespaceEntry {
            path: path.to_string(),
            kind,
            classification: PathClassification::WorkspaceSync,
            mode: MaterializationMode::EncryptedSync,
            access: vec![AccessFlag::HumanReadable, AccessFlag::AgentHidden],
            content_id,
            content_layout: None,
            symlink_target: None,
            byte_len: None,
            executability: FileExecutability::Regular,
            hydration_state: HydrationState::Local,
        }
    }
}
