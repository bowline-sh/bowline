use super::*;
use crate::sync::download::import_snapshot_by_id_with_checkpoints;
use bowline_control_plane::Capability;
use bowline_core::ids::PackId;

impl<'a> SyncRunner<'a> {
    pub(super) fn upload_candidate_with_checkpoints(
        &self,
        candidate: &crate::sync::SnapshotCandidate,
    ) -> Result<UploadOutcome, SyncRunnerError> {
        upload_snapshot_candidate_with_checkpoints(
            candidate,
            self.control_plane,
            self.byte_store,
            self.options.storage_key,
            self.options.key_epoch,
            |step, payload| {
                if step == "workspace-ref-cas-authorized" {
                    self.check_claim_before_domain_boundary()
                        .map_err(|error| match error {
                            SyncRunnerError::SyncClaimOwnershipLost => {
                                UploadError::ClaimOwnershipLost
                            }
                            SyncRunnerError::SyncOperationCancellationRequested => {
                                UploadError::CancellationRequested
                            }
                            error => UploadError::Checkpoint(error.to_string()),
                        })?;
                }
                if step == "workspace-ref-advanced" {
                    self.remote_domain_committed.set(true);
                    return self
                        .record_sync_checkpoint(step, "completed", &payload)
                        .map_err(|error| UploadError::RemoteCommitCheckpoint(error.to_string()));
                }
                self.record_sync_checkpoint(step, "completed", &payload)
                    .map_err(|error| UploadError::Checkpoint(error.to_string()))
            },
        )
        .map_err(Into::into)
    }

    #[cfg(test)]
    pub(super) fn persist_scan_metadata_if_committed(
        &self,
        candidate: &crate::sync::SnapshotCandidate,
        workspace_ref: &WorkspaceRef,
        bound_snapshot: Option<&SnapshotContent>,
    ) -> Result<(), SyncRunnerError> {
        if candidate.snapshot.manifest.snapshot_id.as_str() != workspace_ref.snapshot_id {
            return Ok(());
        }
        self.persist_scan_metadata(candidate, bound_snapshot)
    }

    #[cfg(test)]
    pub(super) fn persist_fresh_scan_metadata_for_head(
        &self,
        workspace_ref: &WorkspaceRef,
        bound_snapshot: Option<&SnapshotContent>,
    ) -> Result<(), SyncRunnerError> {
        let candidate = crate::sync::coalescer::coalesce_workspace_scan(
            &self.options.root,
            self.options.workspace_id.clone(),
            workspace_ref,
            self.options.device_id.clone(),
            self.options.workspace_content_key,
            self.options.generated_at.clone(),
        )?;
        self.persist_scan_metadata_if_committed(&candidate, workspace_ref, bound_snapshot)
    }

    pub(super) fn prepare_local_head_metadata_update<'metadata>(
        &self,
        workspace_ref: &WorkspaceRef,
        metadata_update: LocalHeadMetadataUpdate<'metadata>,
    ) -> Result<PreparedLocalHeadMetadataUpdate<'metadata>, SyncRunnerError> {
        match metadata_update {
            LocalHeadMetadataUpdate::CommittedScan {
                candidate,
                bound_snapshot,
            } => {
                let scan = if candidate.snapshot.manifest.snapshot_id.as_str()
                    == workspace_ref.snapshot_id
                {
                    self.prepare_scan_metadata(candidate)
                } else {
                    None
                };
                let snapshot_content =
                    self.prepare_snapshot_content(workspace_ref, scan.as_ref(), bound_snapshot)?;
                Ok(PreparedLocalHeadMetadataUpdate::CommittedScan {
                    candidate,
                    scan,
                    snapshot_content,
                })
            }
            LocalHeadMetadataUpdate::FreshScan { bound_snapshot } => {
                let scan_scope = ScanScope::default();
                let mut stat_cache = self.load_stat_cache_session(&scan_scope)?;
                let candidate = crate::sync::coalescer::coalesce_workspace_scan_cached(
                    crate::sync::coalescer::CoalesceScanRequest {
                        root: &self.options.root,
                        workspace_id: self.options.workspace_id.clone(),
                        base_ref: workspace_ref,
                        device_id: self.options.device_id.clone(),
                        workspace_content_key: self.options.workspace_content_key,
                        created_at: self.options.generated_at.clone(),
                        context: CoalesceContext::empty(),
                        stat_cache: Some(&mut stat_cache),
                        scan_scope,
                    },
                )?;
                let scan = if candidate.snapshot.manifest.snapshot_id.as_str()
                    == workspace_ref.snapshot_id
                {
                    self.prepare_scan_metadata(&candidate)
                } else {
                    None
                };
                let snapshot_content =
                    self.prepare_snapshot_content(workspace_ref, scan.as_ref(), bound_snapshot)?;
                Ok(PreparedLocalHeadMetadataUpdate::FreshScan {
                    candidate: Box::new(candidate),
                    scan,
                    snapshot_content,
                })
            }
        }
    }

    fn prepare_snapshot_content<'metadata>(
        &self,
        workspace_ref: &WorkspaceRef,
        scan: Option<&PreparedScanMetadata>,
        bound_snapshot: Option<&'metadata SnapshotContent>,
    ) -> Result<Option<PreparedSnapshotContent<'metadata>>, SyncRunnerError> {
        // Hosted reads must finish before the immediate local-head transaction;
        // the scheduler needs the same SQLite writer to renew this worker's claim.
        if scan.is_none() {
            return Ok(None);
        }
        if let Some(snapshot) = bound_snapshot {
            return Ok(Some(PreparedSnapshotContent::Borrowed(snapshot)));
        }
        self.check_claim_before_domain_boundary()?;
        let claim_error = RefCell::new(None);
        let checkpoint = |_point| match self.check_claim_before_domain_boundary() {
            Ok(()) => Ok(()),
            Err(error) => {
                *claim_error.borrow_mut() = Some(error);
                Err(DownloadError::CancellationRequested)
            }
        };
        let imported = import_snapshot_by_id_with_checkpoints(
            &self.options.workspace_id,
            &SnapshotId::new(workspace_ref.snapshot_id.clone()),
            self.control_plane,
            self.byte_store,
            self.options.storage_key,
            crate::sync::namespace::MetadataIdentityKey::derive(
                &self.options.workspace_id,
                self.options.workspace_content_key,
            ),
            &checkpoint,
        );
        if let Some(error) = claim_error.into_inner() {
            return Err(error);
        }
        let imported = imported.map_err(|error| match error {
            DownloadError::CancellationRequested => {
                SyncRunnerError::SyncOperationCancellationRequested
            }
            error => error.into(),
        })?;
        Ok(Some(PreparedSnapshotContent::Imported(Box::new(
            imported.snapshot,
        ))))
    }

    pub(super) fn commit_local_head_metadata(
        &self,
        workspace_ref: &WorkspaceRef,
        metadata_update: PreparedLocalHeadMetadataUpdate<'_>,
    ) -> Result<(), SyncRunnerError> {
        self.check_claim_before_local_head_commit()?;
        self.with_store_sync(|store| {
            let env_import = self.prepare_env_import(store, &metadata_update)?;
            self.append_sync_checkpoint_with_store(
                store,
                "local-head-commit-authorized",
                "started",
                &checkpoint_payload(&SnapshotVersionPayload {
                    snapshot_id: &workspace_ref.snapshot_id,
                    version: workspace_ref.version,
                })?,
            )?;
            store.with_committed(|store| {
                store.upsert_workspace_sync_head(&WorkspaceSyncHeadRecord {
                    workspace_ref: workspace_ref.clone(),
                    observed_at: self.options.generated_at.clone(),
                })?;
                #[cfg(feature = "fault-injection")]
                crate::sync::fault::trip(crate::sync::fault::FaultPoint::AfterLocalHeadWrite)?;
                match metadata_update {
                    PreparedLocalHeadMetadataUpdate::CommittedScan {
                        candidate,
                        scan,
                        snapshot_content,
                    } => self.write_prepared_scan_metadata(
                        store,
                        candidate,
                        scan.as_ref(),
                        snapshot_content
                            .as_ref()
                            .map(PreparedSnapshotContent::as_ref),
                        env_import.as_ref(),
                    ),
                    PreparedLocalHeadMetadataUpdate::FreshScan {
                        ref candidate,
                        ref scan,
                        ref snapshot_content,
                    } => self.write_prepared_scan_metadata(
                        store,
                        candidate.as_ref(),
                        scan.as_ref(),
                        snapshot_content
                            .as_ref()
                            .map(PreparedSnapshotContent::as_ref),
                        env_import.as_ref(),
                    ),
                }
            })
        })
    }

    fn prepare_env_import(
        &self,
        store: &MetadataStore,
        metadata_update: &PreparedLocalHeadMetadataUpdate<'_>,
    ) -> Result<Option<PreparedEnvImport>, SyncRunnerError> {
        // Env replacement is scoped by the active scan's `ObservationWriteScope`,
        // so a partial scan can refresh the env sources it owns without blanking
        // out env sources it never observed (KTD-13). Every scan scope may import;
        // the scope bounds which existing sources it can prune.
        let (scan, candidate) = match metadata_update {
            PreparedLocalHeadMetadataUpdate::CommittedScan {
                scan, candidate, ..
            } => (scan.as_ref(), *candidate),
            PreparedLocalHeadMetadataUpdate::FreshScan {
                scan, candidate, ..
            } => (scan.as_ref(), candidate.as_ref()),
        };
        let Some(scan) = scan else {
            return Ok(None);
        };
        let mut prepared = prepare_env_records_from_scan(
            store,
            &self.options.workspace_id,
            &self.options.root,
            &scan.report,
            Some(self.options.workspace_content_key),
            ObservationWriteScope::for_scan_scope(&candidate.scan_scope),
            &self.options.generated_at,
        )?;
        self.replace_merged_env_records(candidate, scan, &mut prepared)?;
        Ok(Some(prepared))
    }

    fn replace_merged_env_records(
        &self,
        candidate: &crate::sync::SnapshotCandidate,
        scan: &PreparedScanMetadata,
        prepared: &mut PreparedEnvImport,
    ) -> Result<(), SyncRunnerError> {
        visit_snapshot_entries(&candidate.snapshot, &mut |entry| {
            if entry.kind != NamespaceEntryKind::File
                || entry.classification != PathClassification::ProjectEnv
                || !crate::sync::merge::is_env_path(&entry.path)
            {
                return Ok(true);
            }
            let Some(bytes) = candidate
                .snapshot
                .read_file_for_path(&entry.path)
                .map_err(SyncRunnerError::StateIo)?
            else {
                return Ok(true);
            };
            let project_id = scan
                .report
                .paths
                .iter()
                .find(|observed| observed.path == entry.path)
                .and_then(|observed| observed.project_id.clone());
            let records = records_for_env_bytes(
                &self.options.workspace_id,
                project_id,
                &entry.path,
                &bytes,
                Some(self.options.workspace_content_key),
                &self.options.generated_at,
            )?;
            prepared.replace_source_records(entry.path.clone(), records);
            Ok(true)
        })
    }

    fn prepare_scan_metadata(
        &self,
        candidate: &crate::sync::SnapshotCandidate,
    ) -> Option<PreparedScanMetadata> {
        let report =
            workspace_scoped_scan_report(&self.options.workspace_id, &candidate.scan_report);
        // Only a full scan observed every path, so only a full scan may prune
        // projected state for unobserved paths. RootShallow and DirtySubtrees are
        // partial observations and evaluate to `false` here: a shallow root pass
        // must never cause deep paths it did not visit to be treated as deleted.
        let full_observation = matches!(candidate.scan_scope, ScanScope::Full(_));
        if report.root.as_os_str().is_empty()
            && report.projects.is_empty()
            && report.paths.is_empty()
        {
            return None;
        }
        Some(PreparedScanMetadata {
            report,
            full_observation,
        })
    }

    fn write_prepared_scan_metadata(
        &self,
        store: &mut MetadataStore,
        candidate: &crate::sync::SnapshotCandidate,
        scan: Option<&PreparedScanMetadata>,
        bound_snapshot: Option<&SnapshotContent>,
        env_import: Option<&PreparedEnvImport>,
    ) -> Result<(), SyncRunnerError> {
        if let Some(scan) = scan {
            let snapshot_content = bound_snapshot.ok_or_else(|| {
                SyncRunnerError::StateIo(io::Error::other(
                    "prepared scan metadata is missing snapshot content",
                ))
            })?;
            self.write_committed_scan_metadata(
                store,
                candidate,
                scan,
                snapshot_content,
                env_import,
            )?;
        }
        self.apply_stat_cache_write_back_to_store(store, candidate)?;
        Ok(())
    }

    pub(super) fn record_manifest_identity_checkpoint(
        &self,
        candidate: &crate::sync::SnapshotCandidate,
    ) -> Result<(), SyncRunnerError> {
        let snapshot = candidate.snapshot.namespace_snapshot();
        let changed = &snapshot.changed;
        self.record_sync_checkpoint(
            "namespace-page-build",
            "completed",
            &checkpoint_payload(&NamespaceBuildPayload {
                snapshot_id: candidate.snapshot.manifest.snapshot_id.as_str(),
                namespace_root_id: snapshot.namespace_root_id.as_str(),
                namespace_pages_created: changed.namespace_pages_created,
                namespace_pages_reused: changed.namespace_pages_reused,
                content_layouts_created: changed.content_layouts_created,
                content_layouts_reused: changed.content_layouts_reused,
                semantic_entries_hashed: changed.semantic_entries_hashed,
                namespace_pages_loaded_during_build: changed.namespace_pages_loaded_during_build,
                namespace_pages_encoded: changed.namespace_pages_encoded,
                content_layouts_encoded: changed.content_layouts_encoded,
                segment_pages_encoded: changed.segment_pages_encoded,
            })?,
        )
    }

    fn write_committed_scan_metadata(
        &self,
        store: &mut MetadataStore,
        candidate: &crate::sync::SnapshotCandidate,
        scan: &PreparedScanMetadata,
        snapshot_content: &SnapshotContent,
        env_import: Option<&PreparedEnvImport>,
    ) -> Result<(), SyncRunnerError> {
        store.insert_workspace(
            &self.options.workspace_id,
            "Code",
            &self.options.generated_at,
        )?;
        self.persist_snapshot_page_authority(store, snapshot_content)?;
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
        let latest_snapshot_id =
            SnapshotId::new(candidate.snapshot.manifest.snapshot_id.as_str().to_string());
        // Observed-path/project/summary tables are replaced wholesale, so only a
        // full scan (which observed every path) may rewrite them; a partial scan
        // would erase unrelated deep status facts it never looked at.
        if scan.full_observation {
            self.write_full_observation_metadata(
                store,
                &root_id,
                candidate,
                &scan.report,
                &latest_snapshot_id,
            )?;
        }
        // Env replacement is already scoped by the active `ObservationWriteScope`
        // (see `prepare_env_import`), so it is safe to apply for partial scans:
        // it only replaces env sources the scan owns.
        if let Some(env_import) = env_import {
            apply_prepared_env_records(store, &self.options.workspace_id, env_import)?;
        }
        self.rebuild_current_namespace_for_scan(store, snapshot_content, candidate)?;
        Ok(())
    }

    fn write_full_observation_metadata(
        &self,
        store: &mut MetadataStore,
        root_id: &str,
        candidate: &crate::sync::SnapshotCandidate,
        report: &crate::scanner::ScanReport,
        latest_snapshot_id: &SnapshotId,
    ) -> Result<(), SyncRunnerError> {
        let projects = report
            .projects
            .iter()
            .map(|project| ProjectUpsert {
                id: project.id.clone(),
                path: project.path.clone(),
                git_observer_state: project.observer_state,
            })
            .collect::<Vec<_>>();
        store.replace_projects_uncommitted(
            &self.options.workspace_id,
            root_id,
            &projects,
            &self.options.generated_at,
        )?;
        for project in &projects {
            store.set_project_latest_snapshot_id(
                &self.options.workspace_id,
                &project.id,
                latest_snapshot_id,
            )?;
        }
        let mut paths = report
            .paths
            .iter()
            .map(|path| ObservedLocalPath {
                project_id: path.project_id.clone(),
                path: path.path.clone(),
                classification: path.policy.classification,
                mode: path.policy.mode,
                access: path.policy.access.clone(),
            })
            .collect::<Vec<_>>();
        let mut summary = report.summary.clone();
        apply_unsafe_symlink_observation(
            &mut paths,
            &candidate.skipped_unsafe_symlinks,
            &mut summary,
        );
        store.replace_observed_paths_uncommitted(
            &self.options.workspace_id,
            &paths,
            &self.options.generated_at,
        )?;
        store.set_observed_summary(
            &self.options.workspace_id,
            &summary,
            &self.options.generated_at,
        )?;
        Ok(())
    }

    pub(super) fn record_sync_checkpoint(
        &self,
        step: &str,
        state: &str,
        payload_json: &str,
    ) -> Result<(), SyncRunnerError> {
        self.with_store_sync(|store| {
            self.append_sync_checkpoint_with_store(store, step, state, payload_json)
        })?;
        Ok(())
    }

    pub(super) fn check_claim_before_domain_boundary(&self) -> Result<(), SyncRunnerError> {
        if self.options.sync_claim.is_none() {
            return Ok(());
        }
        self.with_store_sync(|store| self.check_claim_before_domain_boundary_with_store(store))
    }

    pub(super) fn check_claim_before_domain_boundary_with_store(
        &self,
        store: &MetadataStore,
    ) -> Result<(), SyncRunnerError> {
        let Some(claim) = &self.options.sync_claim else {
            return Ok(());
        };
        let reconciliation_required = self.remote_domain_committed.get()
            || self.local_materialization_committed.get()
            || claim.claimed_from_state() == SyncOperationState::ReconciliationRequired;
        let check = if reconciliation_required {
            store.renew_sync_operation_reconciliation_boundary(claim)?
        } else {
            store.authorize_sync_operation_boundary(claim)?
        };
        match check {
            SyncClaimCheck::Owned => Ok(()),
            SyncClaimCheck::CancellationRequested if reconciliation_required => {
                self.cancellation_requested_after_commit.set(true);
                Ok(())
            }
            SyncClaimCheck::CancellationRequested => {
                Err(SyncRunnerError::SyncOperationCancellationRequested)
            }
            SyncClaimCheck::OwnershipLost => Err(SyncRunnerError::SyncClaimOwnershipLost),
        }
    }

    pub(super) fn check_claim_during_page_persistence(
        &self,
        store: &MetadataStore,
    ) -> Result<(), SyncRunnerError> {
        let Some(claim) = &self.options.sync_claim else {
            return Ok(());
        };
        let reconciliation_required = self.remote_domain_committed.get()
            || self.local_materialization_committed.get()
            || claim.claimed_from_state() == SyncOperationState::ReconciliationRequired;
        let check = if reconciliation_required {
            store.renew_sync_operation_reconciliation_boundary(claim)?
        } else {
            store.authorize_sync_operation_boundary(claim)?
        };
        match check {
            SyncClaimCheck::Owned => Ok(()),
            SyncClaimCheck::CancellationRequested if reconciliation_required => {
                self.cancellation_requested_after_commit.set(true);
                Ok(())
            }
            SyncClaimCheck::CancellationRequested => {
                Err(SyncRunnerError::SyncOperationCancellationRequested)
            }
            SyncClaimCheck::OwnershipLost => Err(SyncRunnerError::SyncClaimOwnershipLost),
        }
    }

    fn check_claim_before_local_head_commit(&self) -> Result<(), SyncRunnerError> {
        let Some(claim) = &self.options.sync_claim else {
            return Ok(());
        };
        let remote_domain_committed = self.remote_domain_committed.get();
        let local_materialization_committed = self.local_materialization_committed.get();
        let reconciliation_required =
            claim.claimed_from_state() == SyncOperationState::ReconciliationRequired;
        let check = self.with_store_sync(|store| {
            let check = if remote_domain_committed
                || local_materialization_committed
                || reconciliation_required
            {
                store.renew_sync_operation_reconciliation_boundary(claim)
            } else {
                store.authorize_sync_operation_boundary(claim)
            }?;
            Ok(check)
        })?;
        match check {
            SyncClaimCheck::Owned => Ok(()),
            SyncClaimCheck::CancellationRequested
                if remote_domain_committed
                    || local_materialization_committed
                    || reconciliation_required =>
            {
                self.cancellation_requested_after_commit.set(true);
                Ok(())
            }
            SyncClaimCheck::CancellationRequested => {
                Err(SyncRunnerError::SyncOperationCancellationRequested)
            }
            SyncClaimCheck::OwnershipLost => Err(SyncRunnerError::SyncClaimOwnershipLost),
        }
    }

    fn append_sync_checkpoint_with_store(
        &self,
        store: &MetadataStore,
        step: &str,
        state: &str,
        payload_json: &str,
    ) -> Result<(), SyncRunnerError> {
        let Some(claim) = &self.options.sync_claim else {
            return Ok(());
        };
        let transition = store.append_claimed_sync_operation_checkpoint(
            claim,
            &SyncOperationCheckpointRecord {
                id: sync_checkpoint_id(claim.operation_id(), step, state, payload_json),
                workspace_id: self.options.workspace_id.clone(),
                operation_id: claim.operation_id().to_string(),
                step: step.to_string(),
                state: state.to_string(),
                payload_json: payload_json.to_string(),
                created_at: self.options.generated_at.clone(),
                updated_at: self.options.generated_at.clone(),
            },
        )?;
        match transition {
            SyncClaimTransition::Applied => Ok(()),
            SyncClaimTransition::OwnershipLost => Err(SyncRunnerError::SyncClaimOwnershipLost),
        }
    }

    pub(super) fn record_pack_reuse_disabled(
        &self,
        pack_id: &PackId,
    ) -> Result<(), SyncRunnerError> {
        self.record_sync_checkpoint(
            "source-pack-reuse-disabled",
            "limited",
            &checkpoint_payload(&PackReuseUnavailablePayload {
                reason: CheckpointReasonCode::SourcePackReuseUnavailable.as_code(),
                pack_id: pack_id.as_str(),
            })?,
        )
    }

    pub(super) fn import_remote_ref_history(
        &self,
        current_snapshot_id: &str,
    ) -> Result<(), SyncRunnerError> {
        let rows = match self.control_plane.list_workspace_ref_history(
            &self.options.workspace_id,
            crate::history::MAX_HISTORY_LIMIT,
        ) {
            Ok(rows) => rows,
            Err(ControlPlaneError::Limited {
                capability: Capability::WorkspaceRefHistory,
                ..
            }) => {
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        if rows.is_empty() {
            return Ok(());
        }
        for row in rows {
            if row.workspace_id != self.options.workspace_id.as_str() {
                continue;
            }
            if row.target_snapshot_id != EMPTY_SNAPSHOT_ID
                && row.target_snapshot_id != current_snapshot_id
            {
                let snapshot_id = SnapshotId::new(row.target_snapshot_id.clone());
                if self
                    .with_store(|store| store.snapshot(&self.options.workspace_id, &snapshot_id))?
                    .is_none()
                    && self.import_full_snapshot(&snapshot_id).is_err()
                {
                    // Redacted: the import error can carry workspace paths, so
                    // the checkpoint carries only the snapshot id + fixed code.
                    if let Ok(payload) = checkpoint_payload(&SnapshotReasonPayload {
                        snapshot_id: snapshot_id.as_str(),
                        reason: CheckpointReasonCode::RemoteImportBlocked.as_code(),
                    }) {
                        let _ = self.record_sync_checkpoint(
                            "remote-ref-history-snapshot-skipped",
                            "blocked",
                            &payload,
                        );
                    }
                    continue;
                }
            }
            self.with_store(|store| {
                store.enqueue_sync_operation(&remote_ref_history_operation(
                    &self.options.workspace_id,
                    &row,
                ))
            })?;
        }
        Ok(())
    }

    pub(super) fn preserved_base_entries(
        &self,
        base_ref: &WorkspaceRef,
        excluded_paths: &BTreeSet<String>,
    ) -> Result<Vec<bowline_core::workspace_graph::NamespaceEntry>, SyncRunnerError> {
        if base_ref.snapshot_id == EMPTY_SNAPSHOT_ID {
            return Ok(Vec::new());
        }
        let mut preserved_paths = excluded_paths.clone();
        let metadata_path = self.metadata_db_path();
        if metadata_path.exists() {
            for node in self.with_store(|store| {
                store.current_namespace_entries_by_component_prefix(
                    &self.options.workspace_id,
                    &WorkspaceRelativePath::new(""),
                    1_000_000,
                )
            })? {
                if node.kind != NamespaceEntryKind::File
                    || node.hydration_state != HydrationState::Cold
                {
                    continue;
                }
                let local_path = self.options.root.join(Path::new(node.path.as_str()));
                if cold_placeholder_is_absent(&local_path)? {
                    for ancestor in ancestor_paths(node.path.as_str()) {
                        preserved_paths.insert(ancestor);
                    }
                    preserved_paths.insert(node.path.as_str().to_string());
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
            crate::sync::namespace::MetadataIdentityKey::derive(
                &self.options.workspace_id,
                self.options.workspace_content_key,
            ),
        )?;
        let mut entries = Vec::with_capacity(preserved_paths.len());
        for path in preserved_paths {
            if let Some(entry) = imported.snapshot.entry_for_path(&path)? {
                entries.push(entry);
            }
        }
        Ok(entries)
    }

    #[cfg(test)]
    pub(super) fn persist_scan_metadata(
        &self,
        candidate: &crate::sync::SnapshotCandidate,
        bound_snapshot: Option<&SnapshotContent>,
    ) -> Result<(), SyncRunnerError> {
        let metadata_path = self.metadata_db_path();
        if !metadata_path.exists() {
            return Ok(());
        }
        let Some(scan) = self.prepare_scan_metadata(candidate) else {
            return Ok(());
        };
        self.with_store_sync(|store| {
            let mut env_import = prepare_env_records_from_scan(
                store,
                &self.options.workspace_id,
                &self.options.root,
                &scan.report,
                Some(self.options.workspace_content_key),
                ObservationWriteScope::for_scan_scope(&candidate.scan_scope),
                &self.options.generated_at,
            )?;
            self.replace_merged_env_records(candidate, &scan, &mut env_import)?;
            let env_import = Some(env_import);
            self.write_prepared_scan_metadata(
                store,
                candidate,
                Some(&scan),
                bound_snapshot,
                env_import.as_ref(),
            )
        })
    }

    pub(super) fn read_local_head(&self) -> Result<Option<WorkspaceRef>, SyncRunnerError> {
        let metadata_path = self.metadata_db_path();
        if !metadata_path.exists() {
            return Ok(None);
        }
        Ok(self
            .with_store(|store| store.workspace_sync_head(&self.options.workspace_id))?
            .map(|record| record.workspace_ref))
    }

    pub(super) fn metadata_db_path(&self) -> PathBuf {
        self.options.state_root.join(DEFAULT_DATABASE_FILE)
    }
}

pub(super) fn apply_unsafe_symlink_observation(
    paths: &mut Vec<ObservedLocalPath>,
    skipped_unsafe_symlinks: &std::collections::BTreeSet<String>,
    summary: &mut bowline_core::status::ObservedWorkspaceSummary,
) {
    for skipped_path in skipped_unsafe_symlinks {
        let Some(path) = paths.iter_mut().find(|path| &path.path == skipped_path) else {
            paths.push(ObservedLocalPath {
                project_id: None,
                path: skipped_path.clone(),
                classification: PathClassification::Blocked,
                mode: MaterializationMode::Blocked,
                access: Vec::new(),
            });
            continue;
        };
        path.classification = PathClassification::Blocked;
        path.mode = MaterializationMode::Blocked;
    }

    summary.reset_path_counts();
    for path in paths {
        summary.record_path(path.classification, path.mode);
    }
}

fn remote_ref_history_operation(
    workspace_id: &WorkspaceId,
    row: &WorkspaceRefHistoryRecord,
) -> SyncOperationRecord {
    let id = format!(
        "remote-ref-history-{}-{}",
        row.version,
        id_component(&row.target_snapshot_id)
    );
    let payload_json = match checkpoint_payload(&RemoteRefHistoryPayload {
        source: "hosted-ref-history",
        caused_by_event_id: row.caused_by_event_id.as_deref(),
        project_id: row.project_id.as_deref(),
    }) {
        Ok(payload_json) => payload_json,
        Err(error) => {
            eprintln!("bowline-sync remote ref history payload failed: {error}");
            "{}".to_string()
        }
    };
    SyncOperationRecord {
        id: id.clone(),
        workspace_id: workspace_id.clone(),
        kind: SyncOperationKind::Reconcile,
        resource_key: crate::metadata::SyncResourceKey::workspace_sync(workspace_id.clone()),
        state: SyncOperationState::Completed,
        idempotency_key: id,
        base_version: row.version.checked_sub(1),
        base_snapshot_id: Some(row.base_snapshot_id.as_str().to_string()),
        target_snapshot_id: Some(row.target_snapshot_id.as_str().to_string()),
        device_id: row.advanced_by_device_id.clone(),
        payload_json,
        attempt_count: 1,
        claimed_by: None,
        claim_generation: 0,
        heartbeat_at: None,
        lease_expires_at: None,
        cancellation_requested_at: None,
        next_attempt_at: None,
        result_json: None,
        last_error_code: None,
        last_error: None,
        created_at: row.occurred_at.clone(),
        updated_at: row.occurred_at.clone(),
    }
}
