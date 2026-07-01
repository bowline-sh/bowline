use super::*;
use crate::WorkspaceControlPlaneClient;

impl WorkspaceControlPlaneClient for FakeControlPlaneClient {
    fn create_workspace_ref(&self, workspace_id: &str) -> ControlPlaneResult<WorkspaceRef> {
        let mut state = self.state.lock().expect("fake control plane poisoned");

        if let Some(existing_ref) = state.workspace_refs.get(workspace_id) {
            return Ok(existing_ref.clone());
        }

        let workspace_ref = WorkspaceRef {
            workspace_id: workspace_id.to_string(),
            version: 0,
            snapshot_id: "empty".to_string(),
            updated_at: self.clock.now(),
            updated_by_device_id: None,
        };

        state
            .workspace_refs
            .insert(workspace_id.to_string(), workspace_ref.clone());
        state
            .events
            .entry(workspace_id.to_string())
            .or_default()
            .push(self.build_event(
                workspace_id,
                CompactEventKind::WorkspaceCreated,
                &workspace_ref.snapshot_id,
            ));

        Ok(workspace_ref)
    }

    fn get_workspace_ref(&self, workspace_id: &str) -> ControlPlaneResult<Option<WorkspaceRef>> {
        Ok(self
            .state
            .lock()
            .expect("fake control plane poisoned")
            .workspace_refs
            .get(workspace_id)
            .cloned())
    }

    fn compare_and_swap_workspace_ref(
        &self,
        workspace_id: &str,
        expected_version: u64,
        new_snapshot_id: &str,
        writer_device_id: &str,
    ) -> Result<WorkspaceRef, CompareAndSwapError> {
        let mut state = self.state.lock().expect("fake control plane poisoned");
        let current = state
            .workspace_refs
            .get(workspace_id)
            .cloned()
            .ok_or_else(|| CompareAndSwapError::WorkspaceMissing {
                workspace_id: workspace_id.to_string(),
            })?;

        if current.version != expected_version {
            return Err(CompareAndSwapError::StaleRef(StaleWorkspaceRef {
                expected_version,
                current,
            }));
        }

        let next_ref = WorkspaceRef {
            workspace_id: workspace_id.to_string(),
            version: current.version + 1,
            snapshot_id: new_snapshot_id.to_string(),
            updated_at: self.clock.now(),
            updated_by_device_id: Some(writer_device_id.to_string()),
        };

        state
            .workspace_refs
            .insert(workspace_id.to_string(), next_ref.clone());
        state
            .events
            .entry(workspace_id.to_string())
            .or_default()
            .push(self.build_event(
                workspace_id,
                CompactEventKind::WorkspaceRefAdvanced,
                new_snapshot_id,
            ));

        Ok(next_ref)
    }

    fn list_events(&self, workspace_id: &str) -> ControlPlaneResult<Vec<CompactEvent>> {
        Ok(self
            .state
            .lock()
            .expect("fake control plane poisoned")
            .events
            .get(workspace_id)
            .cloned()
            .unwrap_or_default())
    }

    fn publish_conflict_metadata(
        &self,
        input: ConflictMetadataPublish,
    ) -> ControlPlaneResult<ConflictMetadataRecord> {
        self.ensure_workspace(&input.workspace_id)?;
        self.ensure_local_device(&input.detected_by_device_id)?;
        let mut state = self.state.lock().expect("fake control plane poisoned");
        let key = (input.workspace_id.clone(), input.conflict_id.clone());
        if let Some(existing) = state.conflicts.get(&key)
            && (conflict_metadata_same_occurrence(
                existing,
                &input.base_snapshot_id,
                &input.remote_snapshot_id,
            ) || existing.state == "unresolved")
        {
            return Ok(existing.clone());
        }
        let detected_at = self.clock.now();
        let record = ConflictMetadataRecord {
            workspace_id: input.workspace_id.clone(),
            conflict_id: input.conflict_id.clone(),
            conflict_kind: input.conflict_kind,
            paths: input.paths,
            contains_secrets: input.contains_secrets,
            state: "unresolved".to_string(),
            base_snapshot_id: input.base_snapshot_id,
            remote_snapshot_id: input.remote_snapshot_id,
            detected_by_device_id: input.detected_by_device_id,
            bundle_object: input.bundle_object,
            detected_at,
            resolved_by_device_id: None,
            resolved_at: None,
        };
        state.conflicts.insert(key, record.clone());
        state
            .events
            .entry(input.workspace_id.clone())
            .or_default()
            .push(self.build_event(
                &input.workspace_id,
                CompactEventKind::ConflictDetected,
                &input.conflict_id,
            ));
        Ok(record)
    }

    fn list_workspace_conflicts(
        &self,
        workspace_id: &str,
        requested_by_device_id: &str,
    ) -> ControlPlaneResult<Vec<ConflictMetadataRecord>> {
        self.ensure_workspace(workspace_id)?;
        self.ensure_local_device(requested_by_device_id)?;
        Ok(self
            .state
            .lock()
            .expect("fake control plane poisoned")
            .conflicts
            .values()
            .filter(|record| record.workspace_id == workspace_id && record.state == "unresolved")
            .cloned()
            .collect())
    }

    fn mark_conflict_resolved(
        &self,
        input: ConflictResolutionMark,
    ) -> ControlPlaneResult<ConflictMetadataRecord> {
        self.ensure_workspace(&input.workspace_id)?;
        self.ensure_local_device(&input.resolved_by_device_id)?;
        let mut state = self.state.lock().expect("fake control plane poisoned");
        let key = (input.workspace_id.clone(), input.conflict_id.clone());
        let record = state
            .conflicts
            .get_mut(&key)
            .ok_or(ControlPlaneError::Conflict {
                resource: "conflict metadata",
                reason: "conflict does not exist",
            })?;
        if record.state == input.resolution.as_str() {
            return Ok(record.clone());
        }
        if record.state != "unresolved" {
            return Err(ControlPlaneError::Conflict {
                resource: "conflict metadata",
                reason: "conflict metadata is already terminal",
            });
        }
        let resolved_at = self.clock.now();
        record.state = input.resolution.as_str().to_string();
        record.resolved_by_device_id = Some(input.resolved_by_device_id.clone());
        record.resolved_at = Some(resolved_at);
        let record = record.clone();
        state
            .events
            .entry(input.workspace_id.clone())
            .or_default()
            .push(self.build_event(
                &input.workspace_id,
                CompactEventKind::ConflictResolved,
                &input.conflict_id,
            ));
        Ok(record)
    }
}
