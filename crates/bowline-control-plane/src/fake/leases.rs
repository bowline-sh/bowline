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
        // A handoff lease is simply one that names a target host to materialize on;
        // there is no separate dispatch supervisor state anymore.
        let is_handoff = input.target_device_ref.is_some();
        let lease = Lease {
            lease_id: input.lease_id,
            workspace_id: input.workspace_id,
            project_id: input.project_id,
            device_id: input.device_id,
            target_device_ref: input.target_device_ref,
            origin_device_ref: input.origin_device_ref,
            write_target_mode: input.write_target_mode,
            work_view_id: input.work_view_id,
            base_snapshot_id: input.base_snapshot_id,
            task_label: input.task_label,
            version: 0,
            session_state: input.session_state,
            status_code: input.status_code,
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
                if is_handoff {
                    CompactEventKind::LeaseDispatched
                } else {
                    CompactEventKind::LeaseCreated
                },
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
        if let Some(session_state) = input.session_state {
            lease.session_state = session_state;
        }
        if let Some(status_code) = input.status_code {
            lease.status_code = status_code;
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

    fn list_leases(&self, workspace_id: &WorkspaceId) -> ControlPlaneResult<Vec<Lease>> {
        self.ensure_workspace(workspace_id)?;
        let state = self.state.lock().expect("fake control plane poisoned");
        let local_device_id = self.local_device_id.as_deref().map(DeviceId::new);
        Self::ensure_trusted_device_if_configured(&state, workspace_id, local_device_id.as_ref())?;
        let mut leases = state
            .leases
            .values()
            .filter(|lease| &lease.workspace_id == workspace_id)
            .cloned()
            .collect::<Vec<_>>();
        leases.sort_by(|left, right| left.lease_id.cmp(&right.lease_id));
        Ok(leases)
    }
}
