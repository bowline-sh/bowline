use super::*;
use crate::WorkspaceControlPlaneClient;

impl WorkspaceControlPlaneClient for FakeControlPlaneClient {
    fn create_workspace_ref(&self, workspace_id: &WorkspaceId) -> ControlPlaneResult<WorkspaceRef> {
        self.ensure_online()?;
        let mut state = self.state.lock().expect("fake control plane poisoned");

        if let Some(existing_ref) = state.workspace_refs.get(workspace_id) {
            return Ok(existing_ref.clone());
        }

        let workspace_ref = WorkspaceRef {
            workspace_id: workspace_id.clone(),
            version: 0,
            snapshot_id: SnapshotId::new("empty"),
            updated_at: self.clock.now(),
            updated_by_device_id: None,
        };

        state
            .workspace_refs
            .insert(workspace_id.clone(), workspace_ref.clone());
        state.workspace_key_epochs.insert(workspace_id.clone(), 1);
        state
            .events
            .entry(workspace_id.clone())
            .or_default()
            .push(self.build_event(
                workspace_id,
                CompactEventKind::WorkspaceCreated,
                &workspace_ref.snapshot_id,
            ));

        Ok(workspace_ref)
    }

    fn get_workspace_ref(
        &self,
        workspace_id: &WorkspaceId,
    ) -> ControlPlaneResult<Option<WorkspaceRef>> {
        self.ensure_online()?;
        Ok(self
            .state
            .lock()
            .expect("fake control plane poisoned")
            .workspace_refs
            .get(workspace_id)
            .cloned())
    }

    fn compare_and_swap_workspace_ref_for_project(
        &self,
        workspace_id: &WorkspaceId,
        expected_version: u64,
        new_snapshot_id: &SnapshotId,
        writer_device_id: &DeviceId,
        project_id: Option<&bowline_core::ids::ProjectId>,
    ) -> Result<WorkspaceRef, CompareAndSwapError> {
        if self.is_offline() {
            return Err(CompareAndSwapError::Storage(
                Self::offline_transport_error().to_string(),
            ));
        }
        let mut state = self.state.lock().expect("fake control plane poisoned");
        if let Some(injected_current) = state.next_workspace_ref_cas_stale.remove(workspace_id) {
            state
                .workspace_refs
                .insert(workspace_id.clone(), injected_current.clone());
            return Err(CompareAndSwapError::StaleRef(StaleWorkspaceRef {
                expected_version,
                current: injected_current,
            }));
        }
        let current = state
            .workspace_refs
            .get(workspace_id)
            .cloned()
            .ok_or_else(|| CompareAndSwapError::WorkspaceMissing {
                workspace_id: workspace_id.clone(),
            })?;

        if current.version != expected_version {
            return Err(CompareAndSwapError::StaleRef(StaleWorkspaceRef {
                expected_version,
                current,
            }));
        }

        let next_ref = WorkspaceRef {
            workspace_id: workspace_id.clone(),
            version: current.version + 1,
            snapshot_id: new_snapshot_id.clone(),
            updated_at: self.clock.now(),
            updated_by_device_id: Some(writer_device_id.clone()),
        };

        state
            .workspace_refs
            .insert(workspace_id.clone(), next_ref.clone());
        let event = self.build_event(
            workspace_id,
            CompactEventKind::WorkspaceRefAdvanced,
            new_snapshot_id,
        );
        state
            .events
            .entry(workspace_id.clone())
            .or_default()
            .push(event.clone());
        state
            .workspace_ref_history
            .entry(workspace_id.clone())
            .or_default()
            .push(WorkspaceRefHistoryRecord {
                workspace_id: workspace_id.clone(),
                version: next_ref.version,
                base_snapshot_id: current.snapshot_id,
                target_snapshot_id: new_snapshot_id.clone(),
                occurred_at: event.at.to_string(),
                advanced_by_device_id: Some(writer_device_id.clone()),
                caused_by_event_id: Some(event.event_id),
                project_id: project_id.cloned(),
            });

        Ok(next_ref)
    }

    fn list_events(&self, workspace_id: &WorkspaceId) -> ControlPlaneResult<Vec<CompactEvent>> {
        self.ensure_online()?;
        Ok(self
            .state
            .lock()
            .expect("fake control plane poisoned")
            .events
            .get(workspace_id)
            .cloned()
            .unwrap_or_default())
    }

    fn list_workspace_ref_history(
        &self,
        workspace_id: &WorkspaceId,
        limit: u32,
    ) -> ControlPlaneResult<Vec<WorkspaceRefHistoryRecord>> {
        self.ensure_online()?;
        let mut rows = self
            .state
            .lock()
            .expect("fake control plane poisoned")
            .workspace_ref_history
            .get(workspace_id)
            .cloned()
            .unwrap_or_default();
        rows.sort_by(|left, right| {
            right
                .version
                .cmp(&left.version)
                .then(right.occurred_at.cmp(&left.occurred_at))
        });
        rows.truncate(limit as usize);
        Ok(rows)
    }

    fn reconcile_conflict_occurrence(
        &self,
        input: ConflictOccurrenceReconcile,
    ) -> ControlPlaneResult<ConflictReconcileResult> {
        self.ensure_workspace(&input.workspace_id)?;
        self.ensure_local_device(&input.device_id)?;
        let mut state = self.state.lock().expect("fake control plane poisoned");
        if let Some(error) = state.conflict_reconcile_failures.pop_front() {
            return Err(error);
        }
        let key = (input.workspace_id.clone(), input.conflict_id.clone());
        if let Some(existing) = state.conflicts.get(&key) {
            if input.occurrence_version < existing.occurrence_version {
                return Ok(ConflictReconcileResult {
                    conflict: existing.clone(),
                    outcome: ConflictReconcileOutcome::Superseded,
                });
            }
            if input.occurrence_version == existing.occurrence_version {
                if !conflict_metadata_same_occurrence(existing, &input) {
                    return Err(ControlPlaneError::Conflict {
                        resource: "conflict occurrence",
                        reason: "conflict occurrence identity does not match its version",
                    });
                }
                if existing.state == input.desired_state {
                    return Ok(ConflictReconcileResult {
                        conflict: existing.clone(),
                        outcome: ConflictReconcileOutcome::Idempotent,
                    });
                }
                if existing.state != ConflictOccurrenceState::Unresolved
                    || input.desired_state == ConflictOccurrenceState::Unresolved
                {
                    return Err(ControlPlaneError::Conflict {
                        resource: "conflict occurrence",
                        reason: "conflict occurrence is already terminal",
                    });
                }
                let resolved_at = self.clock.now();
                let record = state
                    .conflicts
                    .get_mut(&key)
                    .expect("conflict occurrence exists after immutable lookup");
                record.state = input.desired_state;
                record.resolved_by_device_id = Some(input.device_id.clone());
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
                return Ok(ConflictReconcileResult {
                    conflict: record,
                    outcome: ConflictReconcileOutcome::Applied,
                });
            }
        }
        if input.desired_state != ConflictOccurrenceState::Unresolved {
            return Err(ControlPlaneError::Conflict {
                resource: "conflict occurrence",
                reason: "conflict resolution requires an exact existing occurrence",
            });
        }
        let detected_at = self.clock.now();
        let record = ConflictMetadataRecord {
            workspace_id: input.workspace_id.clone(),
            conflict_id: input.conflict_id.clone(),
            conflict_kind: input.conflict_kind,
            paths: input.paths,
            contains_secrets: input.contains_secrets,
            state: ConflictOccurrenceState::Unresolved,
            base_snapshot_id: input.base_snapshot_id,
            remote_snapshot_id: input.remote_snapshot_id,
            occurrence_version: input.occurrence_version,
            reason: input.reason,
            detected_by_device_id: input.device_id,
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
        Ok(ConflictReconcileResult {
            conflict: record,
            outcome: ConflictReconcileOutcome::Applied,
        })
    }

    fn list_workspace_conflicts(
        &self,
        workspace_id: &WorkspaceId,
        requested_by_device_id: &DeviceId,
    ) -> ControlPlaneResult<Vec<ConflictMetadataRecord>> {
        self.ensure_workspace(workspace_id)?;
        self.ensure_local_device(requested_by_device_id)?;
        Ok(self
            .state
            .lock()
            .expect("fake control plane poisoned")
            .conflicts
            .values()
            .filter(|record| {
                &record.workspace_id == workspace_id
                    && record.state == ConflictOccurrenceState::Unresolved
            })
            .cloned()
            .collect())
    }
}
