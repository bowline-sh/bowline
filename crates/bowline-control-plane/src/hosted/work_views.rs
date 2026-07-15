use super::generated::{
    HostedObjectKind, HostedObjectPointer, HostedOverlayObjectInput, HostedWorkView,
    HostedWorkViewLifecycleState, HostedWorkViewOverlayCommitOutcome,
    HostedWorkViewsCommitOverlayPointerRequest, HostedWorkViewsCreateWorkViewRequest,
    HostedWorkViewsListWorkViewsRequest, HostedWorkViewsRestoreWorkViewRequest,
    HostedWorkViewsUpdateWorkViewLifecycleRequest, WorkViewsCommitOverlayPointer,
    WorkViewsCreateWorkView, WorkViewsListWorkViews, WorkViewsRestoreWorkView,
    WorkViewsUpdateWorkViewLifecycle,
};
use super::*;
use crate::WorkViewControlPlaneClient;

impl WorkViewControlPlaneClient for HostedControlPlaneClient {
    fn create_work_view(&self, input: WorkViewCreate) -> ControlPlaneResult<WorkViewRecord> {
        self.require_local_device(&input.created_by_device_id)?;
        // The proof subject is hand-assembled from the domain input, independent of
        // the transport DTO; the signed proof rides the typed request unchanged.
        let proof_subject = work_view_create_proof_subject(&input);
        let created_by_device_proof =
            self.device_proof(&input.workspace_id, "create-work-view", &proof_subject)?;
        let request = HostedWorkViewsCreateWorkViewRequest {
            base_snapshot_id: input.base_snapshot_id.as_str().to_string(),
            base_workspace_version: input.base_workspace_version,
            created_by_device_id: input.created_by_device_id.as_str().to_string(),
            created_by_device_proof,
            name: input.name.clone(),
            project_id: input.project_id.as_str().to_string(),
            visible_path: input.visible_path.clone(),
            work_view_id: input.work_view_id.as_str().to_string(),
            workspace_id: input.workspace_id.as_str().to_string(),
            expires_at: input.expires_at.clone(),
            retain_until: input.retain_until.clone(),
        };
        WorkViewRecord::try_from(self.call::<WorkViewsCreateWorkView>(&request)?)
    }

    fn list_work_views(
        &self,
        workspace_id: &WorkspaceId,
        include_all: bool,
    ) -> ControlPlaneResult<Vec<WorkViewRecord>> {
        let proof_subject = work_view_list_proof_subject(include_all);
        let requested_by_device_proof =
            self.device_proof(workspace_id, "list-work-views", &proof_subject)?;
        let request = HostedWorkViewsListWorkViewsRequest {
            include_all,
            requested_by_device_id: self.device_id.clone(),
            requested_by_device_proof,
            workspace_id: workspace_id.as_str().to_string(),
        };
        self.call::<WorkViewsListWorkViews>(&request)?
            .into_iter()
            .map(WorkViewRecord::try_from)
            .collect()
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
        let request = HostedWorkViewsUpdateWorkViewLifecycleRequest {
            lifecycle: lifecycle_to_dto(input.lifecycle),
            updated_by_device_id: input.updated_by_device_id.as_str().to_string(),
            updated_by_device_proof,
            work_view_id: input.work_view_id.as_str().to_string(),
            workspace_id: input.workspace_id.as_str().to_string(),
        };
        WorkViewRecord::try_from(self.call::<WorkViewsUpdateWorkViewLifecycle>(&request)?)
    }

    fn restore_work_view(
        &self,
        workspace_id: &WorkspaceId,
        work_view_id: &WorkViewId,
        restored_by_device_id: &DeviceId,
    ) -> ControlPlaneResult<WorkViewRecord> {
        self.require_local_device(restored_by_device_id)?;
        let proof_subject = work_view_restore_proof_subject(work_view_id.as_str());
        let restored_by_device_proof =
            self.device_proof(workspace_id, "restore-work-view", &proof_subject)?;
        let request = HostedWorkViewsRestoreWorkViewRequest {
            restored_by_device_id: restored_by_device_id.as_str().to_string(),
            restored_by_device_proof,
            work_view_id: work_view_id.as_str().to_string(),
            workspace_id: workspace_id.as_str().to_string(),
        };
        WorkViewRecord::try_from(self.call::<WorkViewsRestoreWorkView>(&request)?)
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
        let request = HostedWorkViewsCommitOverlayPointerRequest {
            committed_by_device_id: input.committed_by_device_id.as_str().to_string(),
            committed_by_device_proof,
            expected_overlay_version: input.expected_overlay_version,
            overlay_object: overlay_object_input_from_domain(&input.overlay_object),
            work_view_id: input.work_view_id.as_str().to_string(),
            workspace_id: input.workspace_id.as_str().to_string(),
        };
        let response = self
            .call::<WorkViewsCommitOverlayPointer>(&request)
            .map_err(WorkViewUpdateError::from)?;
        match response.outcome {
            HostedWorkViewOverlayCommitOutcome::Committed => {
                let work_view = response.work_view.ok_or_else(|| {
                    WorkViewUpdateError::from(shape_error(
                        "committed overlay response is missing workView",
                    ))
                })?;
                WorkViewRecord::try_from(work_view).map_err(WorkViewUpdateError::from)
            }
            HostedWorkViewOverlayCommitOutcome::StaleOverlayHead => {
                let current_dto = response.current_work_view.ok_or_else(|| {
                    WorkViewUpdateError::from(shape_error(
                        "stale overlay response is missing currentWorkView",
                    ))
                })?;
                let current =
                    WorkViewRecord::try_from(current_dto).map_err(WorkViewUpdateError::from)?;
                Err(WorkViewUpdateError::StaleOverlayHead(Box::new(
                    StaleWorkViewOverlayHead {
                        expected_overlay_version: input.expected_overlay_version,
                        current,
                    },
                )))
            }
        }
    }
}

/// Convert a decoded work view transport record into the control-plane domain
/// type, re-validating the closed lifecycle enum and canonical timestamps at the
/// boundary just as the former `parse_work_view_record` did.
impl TryFrom<HostedWorkView> for WorkViewRecord {
    type Error = ControlPlaneError;

    fn try_from(dto: HostedWorkView) -> Result<Self, Self::Error> {
        Ok(Self {
            workspace_id: WorkspaceId::new(dto.workspace_id),
            work_view_id: WorkViewId::new(dto.work_view_id),
            project_id: ProjectId::new(dto.project_id),
            name: dto.name,
            visible_path: dto.visible_path,
            base_snapshot_id: SnapshotId::new(dto.base_snapshot_id),
            base_workspace_version: dto.base_workspace_version,
            overlay_head: dto.overlay_head.map(object_pointer_from_dto).transpose()?,
            overlay_version: dto.overlay_version,
            lifecycle: work_view_lifecycle_from_dto(dto.lifecycle),
            created_by_device_id: DeviceId::new(dto.created_by_device_id),
            updated_by_device_id: DeviceId::new(dto.updated_by_device_id),
            created_at: parse_control_timestamp(&dto.created_at)
                .map_err(|error| add_field_context(error, "createdAt"))?,
            updated_at: parse_control_timestamp(&dto.updated_at)
                .map_err(|error| add_field_context(error, "updatedAt"))?,
        })
    }
}

fn object_pointer_from_dto(dto: HostedObjectPointer) -> ControlPlaneResult<ObjectPointer> {
    Ok(ObjectPointer {
        object_key: dto.object_key,
        content_id: ContentId::new(dto.content_id),
        byte_len: dto.byte_length,
        hash: dto.hash,
        key_epoch: dto.key_epoch,
        kind: object_kind_from_dto(dto.kind),
        created_at: parse_control_timestamp(&dto.created_at)
            .map_err(|error| add_field_context(error, "createdAt"))?,
    })
}

fn overlay_object_input_from_domain(pointer: &ObjectPointer) -> HostedOverlayObjectInput {
    // Mirrors the fields the daemon signs in the overlay proof subject; createdAt
    // is not signed and is intentionally not transmitted.
    HostedOverlayObjectInput {
        object_key: pointer.object_key.clone(),
        content_id: pointer.content_id.as_str().to_string(),
        byte_length: pointer.byte_len,
        hash: pointer.hash.clone(),
        key_epoch: pointer.key_epoch,
        kind: object_kind_to_dto(pointer.kind),
    }
}

fn lifecycle_to_dto(state: WorkViewLifecycleState) -> HostedWorkViewLifecycleState {
    match state {
        WorkViewLifecycleState::Active => HostedWorkViewLifecycleState::Active,
        WorkViewLifecycleState::ReviewReady => HostedWorkViewLifecycleState::ReviewReady,
        WorkViewLifecycleState::Accepted => HostedWorkViewLifecycleState::Accepted,
        WorkViewLifecycleState::Discarded => HostedWorkViewLifecycleState::Discarded,
    }
}

fn work_view_lifecycle_from_dto(state: HostedWorkViewLifecycleState) -> WorkViewLifecycleState {
    match state {
        HostedWorkViewLifecycleState::Active => WorkViewLifecycleState::Active,
        HostedWorkViewLifecycleState::ReviewReady => WorkViewLifecycleState::ReviewReady,
        HostedWorkViewLifecycleState::Accepted => WorkViewLifecycleState::Accepted,
        HostedWorkViewLifecycleState::Discarded => WorkViewLifecycleState::Discarded,
    }
}

fn object_kind_to_dto(kind: ObjectKind) -> HostedObjectKind {
    match kind {
        ObjectKind::SourcePack => HostedObjectKind::SourcePack,
        ObjectKind::LocatorIndex => HostedObjectKind::LocatorIndex,
        ObjectKind::SnapshotManifest => HostedObjectKind::SnapshotManifest,
        ObjectKind::SnapshotMetadataPage => HostedObjectKind::SnapshotMetadataPage,
        ObjectKind::AgentOverlay => HostedObjectKind::AgentOverlay,
        ObjectKind::ConflictBundle => HostedObjectKind::ConflictBundle,
    }
}

fn object_kind_from_dto(kind: HostedObjectKind) -> ObjectKind {
    match kind {
        HostedObjectKind::SourcePack => ObjectKind::SourcePack,
        HostedObjectKind::LocatorIndex => ObjectKind::LocatorIndex,
        HostedObjectKind::SnapshotManifest => ObjectKind::SnapshotManifest,
        HostedObjectKind::SnapshotMetadataPage => ObjectKind::SnapshotMetadataPage,
        HostedObjectKind::AgentOverlay => ObjectKind::AgentOverlay,
        HostedObjectKind::ConflictBundle => ObjectKind::ConflictBundle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overlay_pointer_dto() -> HostedObjectPointer {
        HostedObjectPointer {
            object_key: "packs_pk_overlay".to_string(),
            content_id: "cid_overlay".to_string(),
            byte_length: 128,
            hash: "b3_overlay".to_string(),
            key_epoch: 1,
            kind: HostedObjectKind::AgentOverlay,
            created_at: "2026-06-23T12:00:00Z".to_string(),
        }
    }

    fn work_view_dto() -> HostedWorkView {
        HostedWorkView {
            workspace_id: "ws_code".to_string(),
            work_view_id: "work_primary".to_string(),
            project_id: "proj_code".to_string(),
            name: "Primary".to_string(),
            visible_path: ".work/primary".to_string(),
            base_snapshot_id: "snap_base".to_string(),
            base_workspace_version: 4,
            overlay_head: Some(overlay_pointer_dto()),
            overlay_version: 3,
            lifecycle: HostedWorkViewLifecycleState::ReviewReady,
            created_by_device_id: "dev_creator".to_string(),
            updated_by_device_id: "dev_updater".to_string(),
            created_at: "2026-06-23T12:00:00Z".to_string(),
            updated_at: "2026-06-23T12:00:01Z".to_string(),
        }
    }

    #[test]
    fn work_view_dto_maps_identity_versions_lifecycle_and_overlay() {
        let record = WorkViewRecord::try_from(work_view_dto()).expect("work view");
        assert_eq!(record.workspace_id.as_str(), "ws_code");
        assert_eq!(record.work_view_id.as_str(), "work_primary");
        assert_eq!(record.project_id.as_str(), "proj_code");
        assert_eq!(record.base_workspace_version, 4);
        assert_eq!(record.overlay_version, 3);
        assert_eq!(record.lifecycle, WorkViewLifecycleState::ReviewReady);
        let overlay = record.overlay_head.expect("overlay");
        assert_eq!(overlay.kind, ObjectKind::AgentOverlay);
        assert_eq!(overlay.byte_len, 128);
        assert_eq!(overlay.content_id.as_str(), "cid_overlay");
    }

    #[test]
    fn work_view_dto_accepts_absent_overlay_head() {
        let mut dto = work_view_dto();
        dto.overlay_head = None;
        let record = WorkViewRecord::try_from(dto).expect("work view");
        assert_eq!(record.overlay_head, None);
    }

    #[test]
    fn work_view_dto_rejects_malformed_timestamps() {
        let mut dto = work_view_dto();
        dto.created_at = "not-a-timestamp".to_string();
        assert_parse_error_field(WorkViewRecord::try_from(dto), "createdAt");

        let mut overlay = work_view_dto();
        overlay.overlay_head = Some(HostedObjectPointer {
            created_at: "bad".to_string(),
            ..overlay_pointer_dto()
        });
        assert_parse_error_field(WorkViewRecord::try_from(overlay), "createdAt");
    }

    #[test]
    fn lifecycle_round_trips_every_variant() {
        for state in [
            WorkViewLifecycleState::Active,
            WorkViewLifecycleState::ReviewReady,
            WorkViewLifecycleState::Accepted,
            WorkViewLifecycleState::Discarded,
        ] {
            assert_eq!(work_view_lifecycle_from_dto(lifecycle_to_dto(state)), state);
        }
    }

    #[test]
    fn object_kind_round_trips_every_variant() {
        for kind in [
            ObjectKind::SourcePack,
            ObjectKind::LocatorIndex,
            ObjectKind::SnapshotManifest,
            ObjectKind::AgentOverlay,
            ObjectKind::ConflictBundle,
        ] {
            assert_eq!(object_kind_from_dto(object_kind_to_dto(kind)), kind);
        }
    }

    #[test]
    fn overlay_object_input_carries_signed_fields_without_created_at() {
        let pointer = ObjectPointer {
            object_key: "packs_pk_overlay".to_string(),
            content_id: ContentId::new("cid_overlay"),
            byte_len: 128,
            hash: "b3_overlay".to_string(),
            key_epoch: 2,
            kind: ObjectKind::AgentOverlay,
            created_at: ControlPlaneTimestamp { tick: 1 },
        };
        let input = overlay_object_input_from_domain(&pointer);
        assert_eq!(input.object_key, "packs_pk_overlay");
        assert_eq!(input.byte_length, 128);
        assert_eq!(input.key_epoch, 2);
        assert_eq!(input.kind, HostedObjectKind::AgentOverlay);
    }

    fn assert_parse_error_field<T: std::fmt::Debug>(result: ControlPlaneResult<T>, field: &str) {
        let error = result.expect_err("malformed value must reject");
        assert!(
            error.to_string().contains(&format!("`{field}`")),
            "error must identify field `{field}`, got: {error}"
        );
    }
}
