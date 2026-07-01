use super::*;
use crate::WorkViewControlPlaneClient;

impl WorkViewControlPlaneClient for FakeControlPlaneClient {
    fn create_work_view(&self, input: WorkViewCreate) -> ControlPlaneResult<WorkViewRecord> {
        self.ensure_workspace(&input.workspace_id)?;
        let mut state = self.state.lock().expect("fake control plane poisoned");
        Self::ensure_trusted_device_if_configured(
            &state,
            &input.workspace_id,
            Some(&input.created_by_device_id),
        )?;
        let Some(workspace_ref) = state.workspace_refs.get(&input.workspace_id) else {
            return Err(ControlPlaneError::WorkspaceMissing {
                workspace_id: input.workspace_id,
            });
        };
        if workspace_ref.snapshot_id != input.base_snapshot_id
            && !state
                .manifests_by_snapshot
                .contains_key(&(input.workspace_id.clone(), input.base_snapshot_id.clone()))
        {
            return Err(ControlPlaneError::Conflict {
                resource: "work view",
                reason: "base snapshot has not been committed",
            });
        }
        if !input.visible_path.starts_with(".work/") {
            return Err(ControlPlaneError::Conflict {
                resource: "work view",
                reason: "visible path must be a relative .work namespace path",
            });
        }
        let key = (input.workspace_id.clone(), input.work_view_id.clone());
        if let Some(existing) = state.work_views.get(&key) {
            return Ok(existing.clone());
        }
        if state.work_views.values().any(|view| {
            view.workspace_id == input.workspace_id
                && view.project_id == input.project_id
                && view.name.eq_ignore_ascii_case(&input.name)
        }) {
            return Err(ControlPlaneError::Conflict {
                resource: "work view",
                reason: "work view name already exists for this project",
            });
        }

        let now = self.clock.now();
        let record = WorkViewRecord {
            workspace_id: input.workspace_id,
            work_view_id: input.work_view_id,
            project_id: input.project_id,
            name: input.name,
            visible_path: input.visible_path,
            base_snapshot_id: input.base_snapshot_id,
            base_workspace_version: input.base_workspace_version,
            overlay_head: None,
            overlay_version: 0,
            lifecycle: WorkViewLifecycleState::Active,
            created_by_device_id: input.created_by_device_id.clone(),
            updated_by_device_id: input.created_by_device_id,
            created_at: now,
            updated_at: now,
        };
        state.work_views.insert(key, record.clone());
        state
            .events
            .entry(record.workspace_id.clone())
            .or_default()
            .push(self.build_event(
                &record.workspace_id,
                CompactEventKind::WorkCreated,
                &record.work_view_id,
            ));
        Ok(record)
    }

    fn list_work_views(
        &self,
        workspace_id: &str,
        include_all: bool,
    ) -> ControlPlaneResult<Vec<WorkViewRecord>> {
        self.ensure_workspace(workspace_id)?;
        let state = self.state.lock().expect("fake control plane poisoned");
        Self::ensure_trusted_device_if_configured(
            &state,
            workspace_id,
            self.local_device_id.as_deref(),
        )?;
        let mut records = state
            .work_views
            .values()
            .filter(|view| {
                view.workspace_id == workspace_id
                    && (include_all
                        || matches!(
                            view.lifecycle,
                            WorkViewLifecycleState::Active | WorkViewLifecycleState::ReviewReady
                        ))
            })
            .cloned()
            .collect::<Vec<_>>();
        records.sort_by(|left, right| left.visible_path.cmp(&right.visible_path));
        Ok(records)
    }

    fn update_work_view_lifecycle(
        &self,
        input: WorkViewLifecycleUpdate,
    ) -> ControlPlaneResult<WorkViewRecord> {
        self.ensure_workspace(&input.workspace_id)?;
        let mut state = self.state.lock().expect("fake control plane poisoned");
        Self::ensure_trusted_device_if_configured(
            &state,
            &input.workspace_id,
            Some(&input.updated_by_device_id),
        )?;
        let key = (input.workspace_id.clone(), input.work_view_id.clone());
        let record =
            state
                .work_views
                .get_mut(&key)
                .ok_or_else(|| ControlPlaneError::WorkViewMissing {
                    work_view_id: input.work_view_id.clone(),
                })?;
        record.lifecycle = input.lifecycle;
        record.updated_by_device_id = input.updated_by_device_id;
        record.updated_at = self.clock.now();
        let record = record.clone();
        state
            .events
            .entry(record.workspace_id.clone())
            .or_default()
            .push(self.build_event(
                &record.workspace_id,
                work_event_for_lifecycle(record.lifecycle),
                &record.work_view_id,
            ));
        Ok(record)
    }

    fn restore_work_view(
        &self,
        workspace_id: &str,
        work_view_id: &str,
        restored_by_device_id: &str,
    ) -> ControlPlaneResult<WorkViewRecord> {
        self.ensure_workspace(workspace_id)?;
        let mut state = self.state.lock().expect("fake control plane poisoned");
        Self::ensure_trusted_device_if_configured(
            &state,
            workspace_id,
            Some(restored_by_device_id),
        )?;
        let key = (workspace_id.to_string(), work_view_id.to_string());
        let record =
            state
                .work_views
                .get_mut(&key)
                .ok_or_else(|| ControlPlaneError::WorkViewMissing {
                    work_view_id: work_view_id.to_string(),
                })?;
        record.lifecycle = WorkViewLifecycleState::Active;
        record.updated_by_device_id = restored_by_device_id.to_string();
        record.updated_at = self.clock.now();
        let record = record.clone();
        state
            .events
            .entry(record.workspace_id.clone())
            .or_default()
            .push(self.build_event(
                &record.workspace_id,
                CompactEventKind::WorkRestored,
                &record.work_view_id,
            ));
        Ok(record)
    }

    fn commit_work_view_overlay(
        &self,
        input: WorkViewOverlayCommit,
    ) -> Result<WorkViewRecord, WorkViewUpdateError> {
        self.ensure_workspace(&input.workspace_id)?;
        validate_object_key(&input.overlay_object.object_key)?;
        if input.overlay_object.kind != ObjectKind::AgentOverlay {
            return Err(ControlPlaneError::InvalidObjectKey {
                reason: "work view overlays must point at overlay objects",
            }
            .into());
        }

        let mut state = self.state.lock().expect("fake control plane poisoned");
        Self::ensure_trusted_device_if_configured(
            &state,
            &input.workspace_id,
            Some(&input.committed_by_device_id),
        )?;
        let key = (input.workspace_id.clone(), input.work_view_id.clone());
        let current = state.work_views.get(&key).cloned().ok_or_else(|| {
            WorkViewUpdateError::WorkViewMissing {
                work_view_id: input.work_view_id.clone(),
            }
        })?;
        if current.overlay_version != input.expected_overlay_version {
            return Err(WorkViewUpdateError::StaleOverlayHead(Box::new(
                StaleWorkViewOverlayHead {
                    expected_overlay_version: input.expected_overlay_version,
                    current,
                },
            )));
        }
        validate_committed_pointer(
            &state,
            &input.workspace_id,
            &input.overlay_object,
            ObjectKind::AgentOverlay,
        )?;

        if state
            .same_object_stale_overlay_commits
            .remove(&(input.workspace_id.clone(), input.work_view_id.clone()))
        {
            let pointers = state
                .object_pointers
                .entry(input.workspace_id.clone())
                .or_default();
            if !pointers
                .iter()
                .any(|pointer| pointer.object_key == input.overlay_object.object_key)
            {
                pointers.push(input.overlay_object.clone());
            }
            state.object_keys.insert((
                input.workspace_id.clone(),
                input.overlay_object.object_key.clone(),
            ));
            state.committed_object_keys.insert((
                input.workspace_id.clone(),
                input.overlay_object.object_key.clone(),
            ));
            state.object_retention_states.insert(
                (
                    input.workspace_id.clone(),
                    input.overlay_object.object_key.clone(),
                ),
                RetentionState::Current,
            );
            let record = state
                .work_views
                .get_mut(&key)
                .expect("work view exists after lookup");
            record.overlay_head = Some(input.overlay_object);
            record.overlay_version += 1;
            record.updated_by_device_id = input.committed_by_device_id;
            record.updated_at = self.clock.now();
            return Err(WorkViewUpdateError::StaleOverlayHead(Box::new(
                StaleWorkViewOverlayHead {
                    expected_overlay_version: input.expected_overlay_version,
                    current: record.clone(),
                },
            )));
        }

        state.object_keys.insert((
            input.workspace_id.clone(),
            input.overlay_object.object_key.clone(),
        ));
        state.committed_object_keys.insert((
            input.workspace_id.clone(),
            input.overlay_object.object_key.clone(),
        ));
        state.object_retention_states.insert(
            (
                input.workspace_id.clone(),
                input.overlay_object.object_key.clone(),
            ),
            RetentionState::Current,
        );
        let pointers = state
            .object_pointers
            .entry(input.workspace_id.clone())
            .or_default();
        if !pointers
            .iter()
            .any(|pointer| pointer.object_key == input.overlay_object.object_key)
        {
            pointers.push(input.overlay_object.clone());
        }

        let record = state
            .work_views
            .get_mut(&key)
            .expect("work view exists after lookup");
        record.overlay_head = Some(input.overlay_object);
        record.overlay_version += 1;
        record.updated_by_device_id = input.committed_by_device_id;
        record.updated_at = self.clock.now();
        let record = record.clone();
        state
            .events
            .entry(record.workspace_id.clone())
            .or_default()
            .push(self.build_event(
                &record.workspace_id,
                CompactEventKind::WorkUpdated,
                &record.work_view_id,
            ));
        Ok(record)
    }
}
