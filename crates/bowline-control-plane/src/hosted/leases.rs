use super::generated::{
    EventsCreateLease, EventsListLeases, EventsUpdateLease, HostedEventsCreateLeaseRequest,
    HostedEventsListLeasesRequest, HostedEventsUpdateLeaseRequest, HostedLease,
    HostedLeaseEventKind, HostedLeaseSessionState, HostedLeaseUpdateError,
    HostedLeaseWriteTargetMode,
};
use super::*;
use crate::LeaseControlPlaneClient;

impl LeaseControlPlaneClient for HostedControlPlaneClient {
    fn create_lease(&self, input: LeaseCreate) -> ControlPlaneResult<Lease> {
        self.require_local_device(&input.device_id)?;
        // The proof subject is hand-assembled from the domain input, independent
        // of the typed transport DTO; the signed proof rides the request field.
        let proof_subject = lease_create_proof_subject(&input);
        let device_proof =
            self.device_proof(&input.workspace_id, "create-lease", &proof_subject)?;
        let request = HostedEventsCreateLeaseRequest {
            base_snapshot_id: input.base_snapshot_id.as_str().to_string(),
            device_id: input.device_id.as_str().to_string(),
            device_proof,
            session_state: Some(lease_session_state_to_dto(input.session_state)),
            expires_at: input.expires_at.to_string(),
            lease_id: input.lease_id.as_str().to_string(),
            project_id: input.project_id.as_str().to_string(),
            status_code: input.status_code.clone(),
            task_label: input.task_label.clone(),
            target_device_ref: input.target_device_ref.clone(),
            origin_device_ref: input.origin_device_ref.clone(),
            write_target_mode: lease_write_target_mode_to_dto(input.write_target_mode),
            work_view_id: input
                .work_view_id
                .as_ref()
                .map(|id| id.as_str().to_string()),
            workspace_id: input.workspace_id.as_str().to_string(),
        };
        Lease::try_from(self.call::<EventsCreateLease>(&request)?.lease)
    }

    fn update_lease(&self, input: LeaseUpdate) -> ControlPlaneResult<Lease> {
        self.require_local_device(&input.updated_by_device_id)?;
        let proof_subject = lease_update_proof_subject(&input);
        let updated_by_device_proof =
            self.device_proof(&input.workspace_id, "update-lease", &proof_subject)?;
        let request = HostedEventsUpdateLeaseRequest {
            event_kind: input.event_kind.map(lease_event_kind_to_dto).transpose()?,
            expected_version: input.expected_version,
            session_state: input.session_state.map(lease_session_state_to_dto),
            lease_id: input.lease_id.as_str().to_string(),
            status_code: input.status_code.clone(),
            updated_by_device_id: input.updated_by_device_id.as_str().to_string(),
            updated_by_device_proof,
            workspace_id: input.workspace_id.as_str().to_string(),
        };
        let response = self.call::<EventsUpdateLease>(&request)?;
        if response.ok {
            let lease = response
                .lease
                .ok_or_else(|| shape_error("events:updateLease ok response is missing lease"))?;
            return Lease::try_from(lease);
        }
        match response.error {
            Some(HostedLeaseUpdateError::LeaseMissing) => Err(ControlPlaneError::LeaseMissing {
                lease_id: input.lease_id,
            }),
            Some(HostedLeaseUpdateError::StaleLease) => Err(ControlPlaneError::Conflict {
                resource: "agent lease",
                reason: "lease version is stale",
            }),
            None => Err(shape_error("events:updateLease returned an unknown shape")),
        }
    }

    fn list_leases(&self, workspace_id: &WorkspaceId) -> ControlPlaneResult<Vec<Lease>> {
        let requested_by_device_proof =
            self.device_proof(workspace_id, "list-leases", LEASE_LIST_PROOF_SUBJECT)?;
        let request = HostedEventsListLeasesRequest {
            requested_by_device_id: self.device_id.clone(),
            requested_by_device_proof,
            workspace_id: workspace_id.as_str().to_string(),
        };
        self.call::<EventsListLeases>(&request)?
            .into_iter()
            .map(Lease::try_from)
            .collect()
    }
}

impl TryFrom<HostedLease> for Lease {
    type Error = ControlPlaneError;

    fn try_from(dto: HostedLease) -> Result<Self, Self::Error> {
        Ok(Lease {
            lease_id: LeaseId::new(dto.lease_id),
            workspace_id: WorkspaceId::new(dto.workspace_id),
            project_id: ProjectId::new(dto.project_id),
            device_id: DeviceId::new(dto.device_id),
            target_device_ref: dto.target_device_ref,
            origin_device_ref: dto.origin_device_ref,
            write_target_mode: lease_write_target_mode_from_dto(dto.write_target_mode),
            work_view_id: dto.work_view_id.map(WorkViewId::new),
            base_snapshot_id: SnapshotId::new(dto.base_snapshot_id),
            task_label: dto.task_label,
            version: dto.version,
            session_state: lease_session_state_from_dto(dto.session_state),
            status_code: dto.status_code,
            created_at: parse_control_timestamp(&dto.created_at)
                .map_err(|error| add_field_context(error, "createdAt"))?,
            updated_at: parse_control_timestamp(&dto.updated_at)
                .map_err(|error| add_field_context(error, "updatedAt"))?,
            expires_at: parse_control_timestamp(&dto.expires_at)
                .map_err(|error| add_field_context(error, "expiresAt"))?,
        })
    }
}

fn lease_session_state_to_dto(state: LeaseSessionState) -> HostedLeaseSessionState {
    match state {
        LeaseSessionState::Provisional => HostedLeaseSessionState::Provisional,
        LeaseSessionState::Open => HostedLeaseSessionState::Open,
        LeaseSessionState::Completed => HostedLeaseSessionState::Completed,
    }
}

fn lease_session_state_from_dto(state: HostedLeaseSessionState) -> LeaseSessionState {
    match state {
        HostedLeaseSessionState::Provisional => LeaseSessionState::Provisional,
        HostedLeaseSessionState::Open => LeaseSessionState::Open,
        HostedLeaseSessionState::Completed => LeaseSessionState::Completed,
    }
}

fn lease_write_target_mode_to_dto(mode: LeaseWriteTargetMode) -> HostedLeaseWriteTargetMode {
    match mode {
        LeaseWriteTargetMode::Direct => HostedLeaseWriteTargetMode::Direct,
        LeaseWriteTargetMode::WorkView => HostedLeaseWriteTargetMode::WorkView,
    }
}

fn lease_write_target_mode_from_dto(mode: HostedLeaseWriteTargetMode) -> LeaseWriteTargetMode {
    match mode {
        HostedLeaseWriteTargetMode::Direct => LeaseWriteTargetMode::Direct,
        HostedLeaseWriteTargetMode::WorkView => LeaseWriteTargetMode::WorkView,
    }
}

// Only the closed lease-event subset may stamp an update; any other compact
// event kind is a caller error, matching the Convex leaseEventKind validator.
fn lease_event_kind_to_dto(kind: CompactEventKind) -> ControlPlaneResult<HostedLeaseEventKind> {
    match kind {
        CompactEventKind::LeaseCreated => Ok(HostedLeaseEventKind::LeaseCreated),
        CompactEventKind::LeaseUpdated => Ok(HostedLeaseEventKind::LeaseUpdated),
        CompactEventKind::LeaseDispatched => Ok(HostedLeaseEventKind::LeaseDispatched),
        CompactEventKind::LeaseClaimed => Ok(HostedLeaseEventKind::LeaseClaimed),
        CompactEventKind::LeaseCompleted => Ok(HostedLeaseEventKind::LeaseCompleted),
        CompactEventKind::LeaseReviewReady => Ok(HostedLeaseEventKind::LeaseReviewReady),
        CompactEventKind::OverlayChanged => Ok(HostedLeaseEventKind::OverlayChanged),
        _ => Err(shape_error("event kind is not a lease event kind")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_dto() -> HostedLease {
        HostedLease {
            lease_id: "lease_1".to_string(),
            workspace_id: "ws_1".to_string(),
            project_id: "proj_1".to_string(),
            device_id: "dev_1".to_string(),
            target_device_ref: None,
            origin_device_ref: None,
            write_target_mode: HostedLeaseWriteTargetMode::WorkView,
            work_view_id: Some("work_1".to_string()),
            base_snapshot_id: "snap_1".to_string(),
            task_label: Some("label".to_string()),
            version: 3,
            session_state: HostedLeaseSessionState::Open,
            status_code: "active".to_string(),
            created_at: "2026-07-12T00:00:00Z".to_string(),
            updated_at: "2026-07-12T00:00:01Z".to_string(),
            expires_at: "t000000000100".to_string(),
        }
    }

    #[test]
    fn lease_dto_maps_enums_and_control_timestamps() {
        let lease = Lease::try_from(sample_dto()).expect("valid lease");
        assert_eq!(lease.session_state, LeaseSessionState::Open);
        assert_eq!(lease.write_target_mode, LeaseWriteTargetMode::WorkView);
        assert_eq!(lease.version, 3);
        assert_eq!(
            lease.work_view_id.as_ref().map(|id| id.as_str()),
            Some("work_1")
        );
        // createdAt is RFC3339, expiresAt is the compact tick form; both decode.
        assert_eq!(lease.expires_at.to_string(), "t000000000100");
    }

    #[test]
    fn lease_dto_rejects_malformed_timestamp() {
        let mut dto = sample_dto();
        dto.created_at = "not-a-timestamp".to_string();
        assert!(Lease::try_from(dto).is_err());
    }

    #[test]
    fn lease_event_kind_rejects_non_lease_kinds() {
        assert!(lease_event_kind_to_dto(CompactEventKind::LeaseClaimed).is_ok());
        assert!(lease_event_kind_to_dto(CompactEventKind::WorkspaceRefAdvanced).is_err());
    }

    #[test]
    fn lease_session_and_write_mode_round_trip() {
        for state in [
            LeaseSessionState::Provisional,
            LeaseSessionState::Open,
            LeaseSessionState::Completed,
        ] {
            assert_eq!(
                lease_session_state_from_dto(lease_session_state_to_dto(state)),
                state
            );
        }
        for mode in [LeaseWriteTargetMode::Direct, LeaseWriteTargetMode::WorkView] {
            assert_eq!(
                lease_write_target_mode_from_dto(lease_write_target_mode_to_dto(mode)),
                mode
            );
        }
    }
}
