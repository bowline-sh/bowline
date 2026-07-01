use super::*;
use crate::WorkViewControlPlaneClient;

impl WorkViewControlPlaneClient for HostedControlPlaneClient {
    fn create_work_view(&self, input: WorkViewCreate) -> ControlPlaneResult<WorkViewRecord> {
        self.require_local_device(&input.created_by_device_id)?;
        let proof_subject = work_view_create_proof_subject(&input);
        let created_by_device_proof =
            self.device_proof(&input.workspace_id, "create-work-view", &proof_subject)?;
        let value = self.public_mutation(
            "work_views:createWorkView",
            args([
                (
                    "baseSnapshotId",
                    Value::from(input.base_snapshot_id.clone()),
                ),
                (
                    "baseWorkspaceVersion",
                    number_value(input.base_workspace_version),
                ),
                (
                    "createdByDeviceId",
                    Value::from(input.created_by_device_id.clone()),
                ),
                ("createdByDeviceProof", Value::from(created_by_device_proof)),
                ("name", Value::from(input.name.clone())),
                ("projectId", Value::from(input.project_id.clone())),
                ("visiblePath", Value::from(input.visible_path.clone())),
                ("workViewId", Value::from(input.work_view_id.clone())),
                ("workspaceId", Value::from(input.workspace_id.clone())),
            ]),
        )?;
        parse_work_view_record(&value)
    }

    fn list_work_views(
        &self,
        workspace_id: &str,
        include_all: bool,
    ) -> ControlPlaneResult<Vec<WorkViewRecord>> {
        let proof_subject = format!("includeAll={include_all}");
        let requested_by_device_proof =
            self.device_proof(workspace_id, "list-work-views", &proof_subject)?;
        let value = self.public_query(
            "work_views:listWorkViews",
            args([
                ("includeAll", Value::Boolean(include_all)),
                ("requestedByDeviceId", Value::from(self.device_id.clone())),
                (
                    "requestedByDeviceProof",
                    Value::from(requested_by_device_proof),
                ),
                ("workspaceId", Value::from(workspace_id.to_string())),
            ]),
        )?;
        let Value::Array(values) = value else {
            return Err(shape_error("work view list must be an array"));
        };
        values.iter().map(parse_work_view_record).collect()
    }

    fn update_work_view_lifecycle(
        &self,
        input: WorkViewLifecycleUpdate,
    ) -> ControlPlaneResult<WorkViewRecord> {
        self.require_local_device(&input.updated_by_device_id)?;
        let proof_subject = work_view_lifecycle_proof_subject(&input);
        let updated_by_device_proof = self.device_proof(
            &input.workspace_id,
            "update-work-view-lifecycle",
            &proof_subject,
        )?;
        let value = self.public_mutation(
            "work_views:updateWorkViewLifecycle",
            args([
                ("lifecycle", Value::from(input.lifecycle.as_str())),
                (
                    "updatedByDeviceId",
                    Value::from(input.updated_by_device_id.clone()),
                ),
                ("updatedByDeviceProof", Value::from(updated_by_device_proof)),
                ("workViewId", Value::from(input.work_view_id.clone())),
                ("workspaceId", Value::from(input.workspace_id.clone())),
            ]),
        )?;
        parse_work_view_record(&value)
    }

    fn restore_work_view(
        &self,
        workspace_id: &str,
        work_view_id: &str,
        restored_by_device_id: &str,
    ) -> ControlPlaneResult<WorkViewRecord> {
        self.require_local_device(restored_by_device_id)?;
        let proof_subject = format!("workViewId={work_view_id}");
        let restored_by_device_proof =
            self.device_proof(workspace_id, "restore-work-view", &proof_subject)?;
        let value = self.public_mutation(
            "work_views:restoreWorkView",
            args([
                (
                    "restoredByDeviceId",
                    Value::from(restored_by_device_id.to_string()),
                ),
                (
                    "restoredByDeviceProof",
                    Value::from(restored_by_device_proof),
                ),
                ("workViewId", Value::from(work_view_id.to_string())),
                ("workspaceId", Value::from(workspace_id.to_string())),
            ]),
        )?;
        parse_work_view_record(&value)
    }

    fn commit_work_view_overlay(
        &self,
        input: WorkViewOverlayCommit,
    ) -> Result<WorkViewRecord, WorkViewUpdateError> {
        self.require_local_device(&input.committed_by_device_id)
            .map_err(WorkViewUpdateError::from)?;
        let proof_subject = work_view_overlay_proof_subject(&input);
        let committed_by_device_proof = self
            .device_proof(
                &input.workspace_id,
                "commit-work-view-overlay",
                &proof_subject,
            )
            .map_err(WorkViewUpdateError::from)?;
        let value = self
            .public_mutation(
                "work_views:commitOverlayPointer",
                args([
                    (
                        "committedByDeviceId",
                        Value::from(input.committed_by_device_id.clone()),
                    ),
                    (
                        "committedByDeviceProof",
                        Value::from(committed_by_device_proof),
                    ),
                    (
                        "expectedOverlayVersion",
                        number_value(input.expected_overlay_version),
                    ),
                    ("overlayObject", object_pointer_value(&input.overlay_object)),
                    ("workViewId", Value::from(input.work_view_id.clone())),
                    ("workspaceId", Value::from(input.workspace_id.clone())),
                ]),
            )
            .map_err(WorkViewUpdateError::from)?;
        let object = value_object(&value).map_err(WorkViewUpdateError::from)?;
        if object.contains_key("workspaceId") {
            return parse_work_view_record(&value).map_err(WorkViewUpdateError::from);
        }
        if bool_field(object, "ok").unwrap_or(false) {
            return parse_work_view_record(
                required_control_field(object, "workView").map_err(WorkViewUpdateError::from)?,
            )
            .map_err(WorkViewUpdateError::from);
        }

        match string_field(object, "error").as_deref() {
            Ok("work-view-missing") => Err(WorkViewUpdateError::WorkViewMissing {
                work_view_id: input.work_view_id,
            }),
            Ok("stale-overlay-head") => {
                let current = parse_work_view_record(
                    required_control_field(object, "currentWorkView")
                        .map_err(WorkViewUpdateError::from)?,
                )
                .map_err(WorkViewUpdateError::from)?;
                Err(WorkViewUpdateError::StaleOverlayHead(Box::new(
                    StaleWorkViewOverlayHead {
                        expected_overlay_version: input.expected_overlay_version,
                        current,
                    },
                )))
            }
            Ok(_) | Err(_) => Err(WorkViewUpdateError::Unsupported {
                capability: HOSTED_CAPABILITY,
                reason: "Convex work view overlay commit returned an unknown result shape",
            }),
        }
    }
}
