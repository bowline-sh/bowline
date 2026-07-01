use super::*;
use crate::LeaseControlPlaneClient;

impl LeaseControlPlaneClient for HostedControlPlaneClient {
    fn create_lease(&self, input: LeaseCreate) -> ControlPlaneResult<Lease> {
        self.require_local_device(&input.device_id)?;
        let proof_subject = lease_create_proof_subject(&input);
        let device_proof =
            self.device_proof(&input.workspace_id, "create-lease", &proof_subject)?;
        let mut request_args = args([
            (
                "baseSnapshotId",
                Value::from(input.base_snapshot_id.clone()),
            ),
            ("deviceId", Value::from(input.device_id.clone())),
            ("deviceProof", Value::from(device_proof)),
            (
                "executionState",
                Value::from(input.execution_state.as_str()),
            ),
            ("expiresAt", Value::from(input.expires_at.to_string())),
            ("leaseId", Value::from(input.lease_id.clone())),
            ("outputState", Value::from(input.output_state.as_str())),
            ("projectId", Value::from(input.project_id.clone())),
            ("statusCode", Value::from(input.status_code.clone())),
            (
                "writeTargetMode",
                Value::from(input.write_target_mode.as_str()),
            ),
            ("workspaceId", Value::from(input.workspace_id.clone())),
        ]);
        if let Some(work_view_id) = input.work_view_id.as_ref() {
            request_args.insert("workViewId".to_string(), Value::from(work_view_id.clone()));
        }
        if let Some(output_object) = input.output_object.as_ref() {
            request_args.insert(
                "outputObject".to_string(),
                object_pointer_value(output_object),
            );
        }
        if let Some(audit_object) = input.audit_object.as_ref() {
            request_args.insert(
                "auditObject".to_string(),
                object_pointer_value(audit_object),
            );
        }
        let value = self.public_mutation("events:createLease", request_args)?;
        let object = value_object(&value)?;
        parse_lease(required_control_field(object, "lease")?)
    }

    fn update_lease(&self, input: LeaseUpdate) -> ControlPlaneResult<Lease> {
        self.require_local_device(&input.updated_by_device_id)?;
        let proof_subject = lease_update_proof_subject(&input);
        let updated_by_device_proof =
            self.device_proof(&input.workspace_id, "update-lease", &proof_subject)?;
        let mut request_args = args([
            ("expectedVersion", number_value(input.expected_version)),
            ("leaseId", Value::from(input.lease_id.clone())),
            (
                "updatedByDeviceId",
                Value::from(input.updated_by_device_id.clone()),
            ),
            ("updatedByDeviceProof", Value::from(updated_by_device_proof)),
            ("workspaceId", Value::from(input.workspace_id.clone())),
        ]);
        if let Some(execution_state) = input.execution_state {
            request_args.insert(
                "executionState".to_string(),
                Value::from(execution_state.as_str()),
            );
        }
        if let Some(output_state) = input.output_state {
            request_args.insert(
                "outputState".to_string(),
                Value::from(output_state.as_str()),
            );
        }
        if let Some(status_code) = input.status_code.as_ref() {
            request_args.insert("statusCode".to_string(), Value::from(status_code.clone()));
        }
        if let Some(output_object) = input.output_object.as_ref() {
            request_args.insert(
                "outputObject".to_string(),
                object_pointer_value(output_object),
            );
        }
        if let Some(audit_object) = input.audit_object.as_ref() {
            request_args.insert(
                "auditObject".to_string(),
                object_pointer_value(audit_object),
            );
        }
        if let Some(event_kind) = input.event_kind {
            request_args.insert("eventKind".to_string(), Value::from(event_kind.as_str()));
        }

        let value = self.public_mutation("events:updateLease", request_args)?;
        let object = value_object(&value)?;
        if bool_field(object, "ok").unwrap_or(false) {
            return parse_lease(required_control_field(object, "lease")?);
        }
        match string_field(object, "error").as_deref() {
            Ok("lease-missing") => Err(ControlPlaneError::LeaseMissing {
                lease_id: input.lease_id,
            }),
            Ok("stale-lease") => Err(ControlPlaneError::Conflict {
                resource: "agent lease",
                reason: "lease version is stale",
            }),
            Ok(_) | Err(_) => Err(shape_error("events:updateLease returned an unknown shape")),
        }
    }

    fn list_leases(&self, workspace_id: &str) -> ControlPlaneResult<Vec<Lease>> {
        let requested_by_device_proof =
            self.device_proof(workspace_id, "list-leases", "compact=true")?;
        let value = self.public_query(
            "events:listLeases",
            args([
                ("requestedByDeviceId", Value::from(self.device_id.clone())),
                (
                    "requestedByDeviceProof",
                    Value::from(requested_by_device_proof),
                ),
                ("workspaceId", Value::from(workspace_id.to_string())),
            ]),
        )?;
        let Value::Array(values) = value else {
            return Err(shape_error("lease list must be an array"));
        };
        values.iter().map(parse_lease).collect()
    }
}
