use super::*;
use crate::LeaseControlPlaneClient;

impl LeaseControlPlaneClient for FakeControlPlaneClient {
    fn create_lease(&self, input: LeaseCreate) -> ControlPlaneResult<Lease> {
        self.ensure_workspace(&input.workspace_id)?;
        validate_lease_create(&input)?;
        let mut state = self.state.lock().expect("fake control plane poisoned");
        Self::ensure_trusted_device_if_configured(
            &state,
            &input.workspace_id,
            Some(&input.device_id),
        )?;
        validate_optional_lease_pointer(&state, &input.workspace_id, input.output_object.as_ref())?;
        validate_optional_lease_pointer(&state, &input.workspace_id, input.audit_object.as_ref())?;

        if let Some(existing) = state.leases.get(&input.lease_id) {
            if lease_create_matches(existing, &input) {
                return Ok(existing.clone());
            }
            return Err(ControlPlaneError::Conflict {
                resource: "agent lease",
                reason: "lease ID already exists with different metadata",
            });
        }

        let created_at = self.clock.now();
        let lease = Lease {
            lease_id: input.lease_id,
            workspace_id: input.workspace_id,
            project_id: input.project_id,
            device_id: input.device_id,
            write_target_mode: input.write_target_mode,
            work_view_id: input.work_view_id,
            base_snapshot_id: input.base_snapshot_id,
            version: 0,
            execution_state: input.execution_state,
            output_state: input.output_state,
            status_code: input.status_code,
            output_object: input.output_object,
            audit_object: input.audit_object,
            created_at,
            updated_at: created_at,
            expires_at: input.expires_at,
        };
        state.leases.insert(lease.lease_id.clone(), lease.clone());
        state
            .events
            .entry(lease.workspace_id.clone())
            .or_default()
            .push(self.build_event(
                &lease.workspace_id,
                CompactEventKind::LeaseCreated,
                &lease.lease_id,
            ));
        Ok(lease)
    }

    fn update_lease(&self, input: LeaseUpdate) -> ControlPlaneResult<Lease> {
        self.ensure_workspace(&input.workspace_id)?;
        validate_lease_update(&input)?;
        let mut state = self.state.lock().expect("fake control plane poisoned");
        Self::ensure_trusted_device_if_configured(
            &state,
            &input.workspace_id,
            Some(&input.updated_by_device_id),
        )?;
        validate_optional_lease_pointer(&state, &input.workspace_id, input.output_object.as_ref())?;
        validate_optional_lease_pointer(&state, &input.workspace_id, input.audit_object.as_ref())?;

        let key = input.lease_id.clone();
        let existing =
            state
                .leases
                .get(&key)
                .cloned()
                .ok_or_else(|| ControlPlaneError::LeaseMissing {
                    lease_id: input.lease_id.clone(),
                })?;
        if existing.workspace_id != input.workspace_id {
            return Err(ControlPlaneError::LeaseMissing {
                lease_id: input.lease_id,
            });
        }
        if existing.version != input.expected_version {
            return Err(ControlPlaneError::Conflict {
                resource: "agent lease",
                reason: "lease version is stale",
            });
        }

        let updated_at = self.clock.now();
        let lease = state
            .leases
            .get_mut(&key)
            .expect("lease exists after lookup");
        if let Some(execution_state) = input.execution_state {
            lease.execution_state = execution_state;
        }
        if let Some(output_state) = input.output_state {
            lease.output_state = output_state;
        }
        if let Some(status_code) = input.status_code {
            lease.status_code = status_code;
        }
        if let Some(output_object) = input.output_object {
            lease.output_object = Some(output_object);
        }
        if let Some(audit_object) = input.audit_object {
            lease.audit_object = Some(audit_object);
        }
        lease.version += 1;
        lease.updated_at = updated_at;
        let lease = lease.clone();
        state
            .events
            .entry(lease.workspace_id.clone())
            .or_default()
            .push(
                self.build_event(
                    &lease.workspace_id,
                    input
                        .event_kind
                        .unwrap_or_else(|| lease_event_for_update(&lease)),
                    &lease.lease_id,
                ),
            );
        Ok(lease)
    }

    fn list_leases(&self, workspace_id: &str) -> ControlPlaneResult<Vec<Lease>> {
        self.ensure_workspace(workspace_id)?;
        let state = self.state.lock().expect("fake control plane poisoned");
        Self::ensure_trusted_device_if_configured(
            &state,
            workspace_id,
            self.local_device_id.as_deref(),
        )?;
        let mut leases = state
            .leases
            .values()
            .filter(|lease| lease.workspace_id == workspace_id)
            .cloned()
            .collect::<Vec<_>>();
        leases.sort_by(|left, right| left.lease_id.cmp(&right.lease_id));
        Ok(leases)
    }
}
