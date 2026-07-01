use super::*;
use crate::WorkspaceControlPlaneClient;

impl WorkspaceControlPlaneClient for HostedControlPlaneClient {
    fn create_workspace_ref(&self, workspace_id: &str) -> ControlPlaneResult<WorkspaceRef> {
        let mut request_args = args([
            ("snapshotId", Value::from("empty")),
            ("workspaceId", Value::from(workspace_id)),
        ]);
        if self.account_session_auth_available() {
            request_args.insert(
                "accountSessionId".to_string(),
                Value::from(self.verified_account_session_id(Some(workspace_id))?),
            );
        }
        let value = if self.account_session_auth_available() {
            self.public_mutation("refs:createWorkspaceRef", request_args)?
        } else {
            self.mutation("refs:createWorkspaceRef", request_args)?
        };
        parse_workspace_ref(&value)
    }

    fn get_workspace_ref(&self, workspace_id: &str) -> ControlPlaneResult<Option<WorkspaceRef>> {
        let mut request_args = args([("workspaceId", Value::from(workspace_id))]);
        let value = if self.account_session_auth_available() {
            request_args.insert(
                "accountSessionId".to_string(),
                Value::from(self.verified_account_session_id(Some(workspace_id))?),
            );
            self.public_query("refs:getWorkspaceRef", request_args)?
        } else {
            self.query("refs:getWorkspaceRef", request_args)?
        };
        if matches!(value, Value::Null) {
            Ok(None)
        } else {
            parse_workspace_ref(&value).map(Some)
        }
    }

    fn compare_and_swap_workspace_ref(
        &self,
        workspace_id: &str,
        expected_version: u64,
        new_snapshot_id: &str,
        writer_device_id: &str,
    ) -> Result<WorkspaceRef, CompareAndSwapError> {
        self.require_local_device(writer_device_id)
            .map_err(|error| CompareAndSwapError::Storage(error.to_string()))?;
        let proof_subject = workspace_ref_proof_subject(expected_version, new_snapshot_id);
        let writer_device_proof = self
            .device_proof(
                workspace_id,
                "compare-and-swap-workspace-ref",
                &proof_subject,
            )
            .map_err(|error| CompareAndSwapError::Storage(error.to_string()))?;
        let value = self
            .public_mutation(
                "refs:compareAndSwapWorkspaceRef",
                args([
                    ("expectedVersion", number_value(expected_version)),
                    ("nextSnapshotId", Value::from(new_snapshot_id)),
                    ("workspaceId", Value::from(workspace_id)),
                    ("writerDeviceId", Value::from(writer_device_id)),
                    ("writerDeviceProof", Value::from(writer_device_proof)),
                ]),
            )
            .map_err(|error| CompareAndSwapError::Storage(error.to_string()))?;

        let object = value_object(&value)
            .map_err(|error| CompareAndSwapError::Storage(error.to_string()))?;
        if bool_field(object, "ok").unwrap_or(false) {
            return parse_workspace_ref(required_field(object, "ref")?)
                .map_err(|error| CompareAndSwapError::Storage(error.to_string()));
        }

        match string_field(object, "error").as_deref() {
            Ok("workspace-missing") => Err(CompareAndSwapError::WorkspaceMissing {
                workspace_id: workspace_id.to_string(),
            }),
            Ok("stale-ref") => {
                let current = parse_workspace_ref(required_field(object, "currentRef")?)
                    .map_err(|error| CompareAndSwapError::Storage(error.to_string()))?;
                Err(CompareAndSwapError::StaleRef(StaleWorkspaceRef {
                    expected_version,
                    current,
                }))
            }
            Ok(_) | Err(_) => Err(CompareAndSwapError::Unsupported {
                capability: HOSTED_CAPABILITY,
                reason: "Convex CAS returned an unknown result shape",
            }),
        }
    }

    fn list_events(&self, workspace_id: &str) -> ControlPlaneResult<Vec<CompactEvent>> {
        let mut request_args = args([("workspaceId", Value::from(workspace_id))]);
        let value = if self.account_session_auth_available() {
            request_args.insert(
                "accountSessionId".to_string(),
                Value::from(self.verified_account_session_id(Some(workspace_id))?),
            );
            self.public_query("events:listCompactEvents", request_args)?
        } else {
            self.query("events:listCompactEvents", request_args)?
        };
        let Value::Array(events) = value else {
            return Err(shape_error(
                "events:listCompactEvents did not return an array",
            ));
        };

        events
            .iter()
            .map(parse_compact_event)
            .collect::<ControlPlaneResult<Vec<_>>>()
    }

    fn publish_conflict_metadata(
        &self,
        input: ConflictMetadataPublish,
    ) -> ControlPlaneResult<ConflictMetadataRecord> {
        self.require_local_device(&input.detected_by_device_id)?;
        let proof_subject = conflict_publish_proof_subject(&input);
        let detected_by_device_proof = self.device_proof(
            &input.workspace_id,
            "publish-conflict-metadata",
            &proof_subject,
        )?;
        let mut request_args = args([
            (
                "baseSnapshotId",
                Value::from(input.base_snapshot_id.clone()),
            ),
            ("conflictId", Value::from(input.conflict_id.clone())),
            ("conflictKind", Value::from(input.conflict_kind.clone())),
            ("containsSecrets", Value::Boolean(input.contains_secrets)),
            (
                "detectedByDeviceId",
                Value::from(input.detected_by_device_id.clone()),
            ),
            (
                "detectedByDeviceProof",
                Value::from(detected_by_device_proof),
            ),
            (
                "paths",
                Value::Array(input.paths.iter().cloned().map(Value::from).collect()),
            ),
            (
                "remoteSnapshotId",
                Value::from(input.remote_snapshot_id.clone()),
            ),
            ("workspaceId", Value::from(input.workspace_id.clone())),
        ]);
        if let Some(pointer) = input.bundle_object.as_ref() {
            request_args.insert("bundleObject".to_string(), object_pointer_value(pointer));
        }
        let value = self.public_mutation("conflicts:publishConflictMetadata", request_args)?;
        parse_conflict_metadata_record(&value)
    }

    fn list_workspace_conflicts(
        &self,
        workspace_id: &str,
        requested_by_device_id: &str,
    ) -> ControlPlaneResult<Vec<ConflictMetadataRecord>> {
        self.require_local_device(requested_by_device_id)?;
        let proof_subject = format!("workspaceId={workspace_id}");
        let requested_by_device_proof =
            self.device_proof(workspace_id, "list-workspace-conflicts", &proof_subject)?;
        let value = self.public_query(
            "conflicts:listWorkspaceConflicts",
            args([
                ("requestedByDeviceId", Value::from(requested_by_device_id)),
                (
                    "requestedByDeviceProof",
                    Value::from(requested_by_device_proof),
                ),
                ("workspaceId", Value::from(workspace_id)),
            ]),
        )?;
        let Value::Array(records) = value else {
            return Err(shape_error(
                "conflicts:listWorkspaceConflicts must return an array",
            ));
        };
        records
            .iter()
            .map(parse_conflict_metadata_record)
            .collect::<ControlPlaneResult<Vec<_>>>()
    }

    fn mark_conflict_resolved(
        &self,
        input: ConflictResolutionMark,
    ) -> ControlPlaneResult<ConflictMetadataRecord> {
        self.require_local_device(&input.resolved_by_device_id)?;
        let proof_subject = conflict_resolution_proof_subject(&input);
        let resolved_by_device_proof = self.device_proof(
            &input.workspace_id,
            "mark-conflict-resolved",
            &proof_subject,
        )?;
        let value = self.public_mutation(
            "conflicts:markConflictResolved",
            args([
                ("conflictId", Value::from(input.conflict_id.clone())),
                ("resolution", Value::from(input.resolution.as_str())),
                (
                    "resolvedByDeviceId",
                    Value::from(input.resolved_by_device_id.clone()),
                ),
                (
                    "resolvedByDeviceProof",
                    Value::from(resolved_by_device_proof),
                ),
                ("workspaceId", Value::from(input.workspace_id.clone())),
            ]),
        )?;
        parse_conflict_metadata_record(&value)
    }

    fn publish_workspace_status(
        &self,
        snapshot: &WorkspaceStatusSnapshot,
    ) -> ControlPlaneResult<()> {
        self.require_local_device(&snapshot.published_by_device_id)?;
        let proof_subject = snapshot.proof_subject();
        let published_by_device_proof = self.device_proof(
            &snapshot.workspace_id,
            "publish-workspace-status",
            &proof_subject,
        )?;
        let mut request_args = args([
            (
                "attentionItems",
                Value::Array(
                    snapshot
                        .attention_items
                        .iter()
                        .cloned()
                        .map(Value::from)
                        .collect(),
                ),
            ),
            (
                "eventWatermarks",
                status_event_watermarks_value(&snapshot.event_watermarks),
            ),
            ("generatedAt", Value::from(snapshot.generated_at.clone())),
            (
                "publishedByDeviceId",
                Value::from(snapshot.published_by_device_id.clone()),
            ),
            (
                "publishedByDeviceProof",
                Value::from(published_by_device_proof),
            ),
            ("snapshotId", Value::from(snapshot.snapshot_id.clone())),
            ("statusLevel", Value::from(snapshot.status_level.clone())),
            ("workspaceId", Value::from(snapshot.workspace_id.clone())),
        ]);
        if let Some(sync_queue) = snapshot.sync_queue.as_ref() {
            request_args.insert("syncQueue".to_string(), status_sync_queue_value(sync_queue));
        }
        if let Some(index) = snapshot.index.as_ref() {
            request_args.insert("index".to_string(), status_index_value(index));
        }
        if let Some(summary) = snapshot.workspace_summary.as_ref() {
            request_args.insert(
                "workspaceSummary".to_string(),
                status_workspace_summary_value(summary),
            );
        }
        if !snapshot.items.is_empty() {
            request_args.insert(
                "items".to_string(),
                Value::Array(snapshot.items.iter().map(status_item_value).collect()),
            );
        }
        if !snapshot.limits.is_empty() {
            request_args.insert(
                "limits".to_string(),
                Value::Array(snapshot.limits.iter().map(status_limit_value).collect()),
            );
        }
        self.public_mutation("status:publishWorkspaceStatus", request_args)?;
        Ok(())
    }
}
