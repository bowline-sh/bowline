use super::*;

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
                self.record_sync_checkpoint(step, "completed", &payload)
                    .map_err(|error| UploadError::Checkpoint(error.to_string()))
            },
        )
        .map_err(Into::into)
    }

    pub(super) fn persist_scan_metadata_if_committed(
        &self,
        candidate: &crate::sync::SnapshotCandidate,
        workspace_ref: &WorkspaceRef,
    ) -> Result<(), SyncRunnerError> {
        if candidate.snapshot.manifest.snapshot_id.as_str() != workspace_ref.snapshot_id {
            return Ok(());
        }
        self.persist_scan_metadata(candidate)
    }

    pub(super) fn persist_fresh_scan_metadata_for_head(
        &self,
        workspace_ref: &WorkspaceRef,
    ) -> Result<(), SyncRunnerError> {
        let candidate = crate::sync::coalescer::coalesce_workspace_scan(
            &self.options.root,
            self.options.workspace_id.clone(),
            workspace_ref,
            self.options.device_id.clone(),
            self.options.workspace_content_key,
            self.options.generated_at.clone(),
        )?;
        self.persist_scan_metadata_if_committed(&candidate, workspace_ref)
    }

    pub(super) fn record_sync_checkpoint(
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

    pub(super) fn preserved_base_entries(
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

    pub(super) fn persist_scan_metadata(
        &self,
        candidate: &crate::sync::SnapshotCandidate,
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

    pub(super) fn read_local_head(&self) -> Result<Option<WorkspaceRef>, SyncRunnerError> {
        let metadata_path = self.metadata_db_path();
        if !metadata_path.exists() {
            return Ok(None);
        }
        let store = MetadataStore::open(metadata_path)?;
        Ok(store
            .workspace_sync_head(&self.options.workspace_id)?
            .map(|record| record.workspace_ref))
    }

    pub(super) fn write_local_head(
        &self,
        workspace_ref: &WorkspaceRef,
    ) -> Result<(), SyncRunnerError> {
        let store = MetadataStore::open(self.metadata_db_path())?;
        store.upsert_workspace_sync_head(&WorkspaceSyncHeadRecord {
            workspace_ref: workspace_ref.clone(),
            observed_at: self.options.generated_at.clone(),
        })?;
        Ok(())
    }

    pub(super) fn metadata_db_path(&self) -> PathBuf {
        self.options.state_root.join(DEFAULT_DATABASE_FILE)
    }
}
