use super::*;

pub(super) fn parse_workspace_ref(value: &Value) -> ControlPlaneResult<WorkspaceRef> {
    let object = value_object(value)?;
    Ok(WorkspaceRef {
        workspace_id: string_field(object, "workspaceId")?,
        version: u64_field(object, "version")?,
        snapshot_id: string_field(object, "snapshotId")?,
        updated_at: current_timestamp(),
        updated_by_device_id: optional_string_field(object, "updatedByDeviceId")?,
    })
}

pub(super) fn parse_compact_event(value: &Value) -> ControlPlaneResult<CompactEvent> {
    let object = value_object(value)?;
    Ok(CompactEvent {
        event_id: string_field(object, "eventId")?,
        workspace_id: string_field(object, "workspaceId")?,
        at: current_timestamp(),
        kind: parse_event_kind(&string_field(object, "kind")?)?,
        subject: string_field(object, "subject")?,
    })
}

pub(super) fn parse_event_kind(kind: &str) -> ControlPlaneResult<CompactEventKind> {
    match kind {
        "device.harness_approved" => Ok(CompactEventKind::DeviceHarnessApproved),
        "device.approval_requested" => Ok(CompactEventKind::DeviceApprovalRequested),
        "device.approved" => Ok(CompactEventKind::DeviceApproved),
        "device.denied" => Ok(CompactEventKind::DeviceDenied),
        "device.revoked" => Ok(CompactEventKind::DeviceRevoked),
        "device.requested" => Ok(CompactEventKind::DeviceRequested),
        "recovery_key.created" => Ok(CompactEventKind::RecoveryKeyCreated),
        "recovery_key.verified" => Ok(CompactEventKind::RecoveryKeyVerified),
        "recovery_key.rotated" => Ok(CompactEventKind::RecoveryKeyRotated),
        "recovery_key.revoked" => Ok(CompactEventKind::RecoveryKeyRevoked),
        "auth.login_started" => Ok(CompactEventKind::AuthLoginStarted),
        "auth.login_completed" => Ok(CompactEventKind::AuthLoginCompleted),
        "conflict.detected" => Ok(CompactEventKind::ConflictDetected),
        "conflict.resolved" => Ok(CompactEventKind::ConflictResolved),
        "lease.blocked" => Ok(CompactEventKind::LeaseBlocked),
        "lease.cleanup_completed" => Ok(CompactEventKind::LeaseCleanupCompleted),
        "lease.completed" => Ok(CompactEventKind::LeaseCompleted),
        "lease.created" => Ok(CompactEventKind::LeaseCreated),
        "lease.expired" => Ok(CompactEventKind::LeaseExpired),
        "lease.hydration_requested" => Ok(CompactEventKind::LeaseHydrationRequested),
        "lease.revoked" => Ok(CompactEventKind::LeaseRevoked),
        "lease.review_ready" => Ok(CompactEventKind::LeaseReviewReady),
        "lease.tool_denied" => Ok(CompactEventKind::LeaseToolDenied),
        "lease.tool_invoked" => Ok(CompactEventKind::LeaseToolInvoked),
        "lease.updated" => Ok(CompactEventKind::LeaseUpdated),
        "object_manifest.committed" => Ok(CompactEventKind::ObjectManifestCommitted),
        "object_pointer.added" => Ok(CompactEventKind::ObjectPointerAdded),
        "overlay.changed" => Ok(CompactEventKind::OverlayChanged),
        "publish.requested" => Ok(CompactEventKind::PublishRequested),
        "work.accepted" => Ok(CompactEventKind::WorkAccepted),
        "work.archived" => Ok(CompactEventKind::WorkArchived),
        "work.cleanup_completed" => Ok(CompactEventKind::WorkCleanupCompleted),
        "work.cleanup_previewed" => Ok(CompactEventKind::WorkCleanupPreviewed),
        "work.created" => Ok(CompactEventKind::WorkCreated),
        "work.discarded" => Ok(CompactEventKind::WorkDiscarded),
        "work.expired" => Ok(CompactEventKind::WorkExpired),
        "work.restored" => Ok(CompactEventKind::WorkRestored),
        "work.review_ready" => Ok(CompactEventKind::WorkReviewReady),
        "work.updated" => Ok(CompactEventKind::WorkUpdated),
        "workspace.created" => Ok(CompactEventKind::WorkspaceCreated),
        "workspace_ref.advanced" => Ok(CompactEventKind::WorkspaceRefAdvanced),
        _ => Err(shape_error("unknown compact event kind")),
    }
}

pub(super) fn parse_device_request(value: &Value) -> ControlPlaneResult<DeviceRequest> {
    let object = value_object(value)?;
    Ok(DeviceRequest {
        request_id: string_field(object, "requestId")?,
        workspace_id: string_field(object, "workspaceId")?,
        device_id: string_field(object, "deviceId")?,
        device_name: string_field(object, "deviceName")?,
        platform: string_field(object, "platform")?,
        device_public_key: string_field(object, "devicePublicKey")?,
        device_fingerprint: string_field(object, "deviceFingerprint")?,
        matching_code: string_field(object, "matchingCode")?,
        account_id: optional_string_field(object, "accountId")?,
        host: optional_string_field(object, "host")?,
        root: optional_string_field(object, "root")?,
        requested_at: current_timestamp(),
        expires_at: current_timestamp(),
        state: parse_device_request_state(
            &string_field(object, "state").unwrap_or_else(|_| "pending".to_string()),
        )?,
    })
}

pub(super) fn parse_device_request_state(state: &str) -> ControlPlaneResult<DeviceRequestState> {
    match state {
        "pending" => Ok(DeviceRequestState::Pending),
        "approved" => Ok(DeviceRequestState::Approved),
        "denied" => Ok(DeviceRequestState::Denied),
        "expired" => Ok(DeviceRequestState::Expired),
        _ => Err(shape_error("unknown device request state")),
    }
}

pub(super) fn parse_bootstrap_session(
    value: &Value,
    token: String,
) -> ControlPlaneResult<BootstrapSession> {
    let object = value_object(value)?;
    Ok(BootstrapSession {
        session_id: string_field(object, "sessionId")?,
        workspace_id: string_field(object, "workspaceId")?,
        token,
        expires_at: current_timestamp(),
    })
}

pub(super) fn parse_authorized_device(value: &Value) -> ControlPlaneResult<AuthorizedDeviceRecord> {
    let object = value_object(value)?;
    Ok(AuthorizedDeviceRecord {
        workspace_id: string_field(object, "workspaceId")?,
        device_id: string_field(object, "deviceId")?,
        device_name: string_field(object, "deviceName")?,
        platform: string_field(object, "platform")?,
        device_fingerprint: string_field(object, "deviceFingerprint")?,
        authorized_at: current_timestamp(),
        authorized_by_device_id: optional_string_field(object, "authorizedByDeviceId")?,
        revoked_at: None,
    })
}

pub(super) fn parse_revoked_device(value: &Value) -> ControlPlaneResult<RevokedDeviceRecord> {
    let object = value_object(value)?;
    Ok(RevokedDeviceRecord {
        workspace_id: string_field(object, "workspaceId")?,
        device_id: string_field(object, "deviceId")?,
        device_name: string_field(object, "deviceName")?,
        platform: string_field(object, "platform")?,
        device_fingerprint: string_field(object, "deviceFingerprint")?,
        revoked_at: current_timestamp(),
        revoked_by_device_id: string_field(object, "revokedByDeviceId")?,
        reason: string_field(object, "reason")?,
    })
}

pub(super) fn parse_device_approval(value: &Value) -> ControlPlaneResult<DeviceApproval> {
    let object = value_object(value)?;
    Ok(DeviceApproval {
        grant_id: string_field(object, "grantId")?,
        request_id: string_field(object, "requestId")?,
        workspace_id: string_field(object, "workspaceId")?,
        device_id: string_field(object, "deviceId")
            .or_else(|_| string_field(object, "requesterDeviceId"))?,
        device_name: string_field(object, "deviceName")?,
        platform: string_field(object, "platform")?,
        device_fingerprint: string_field(object, "deviceFingerprint")
            .or_else(|_| string_field(object, "requesterDeviceFingerprint"))?,
        approved_by_device_id: string_field(object, "approverDeviceId")?,
        encrypted_grant_ciphertext: string_field(object, "ciphertext")?,
        key_epoch: u64_field(object, "keyEpoch")? as u32,
        granted_at: current_timestamp(),
        expires_at: current_timestamp(),
        accepted_at: optional_string_field(object, "acceptedAt")?.map(|_| current_timestamp()),
        harness_only: false,
    })
}

pub(super) fn parse_device_denial(value: &Value) -> ControlPlaneResult<DeviceDenial> {
    let object = value_object(value)?;
    Ok(DeviceDenial {
        request_id: string_field(object, "requestId")?,
        workspace_id: string_field(object, "workspaceId")?,
        device_id: string_field(object, "deviceId")?,
        denied_by_device_id: string_field(object, "deniedByDeviceId")?,
        denied_at: current_timestamp(),
        reason: string_field(object, "reason")?,
    })
}

pub(super) fn parse_recovery_envelope(value: &Value) -> ControlPlaneResult<RecoveryEnvelopeRecord> {
    let object = value_object(value)?;
    Ok(RecoveryEnvelopeRecord {
        workspace_id: string_field(object, "workspaceId")?,
        envelope_id: string_field(object, "envelopeId")?,
        created_by_device_id: string_field(object, "createdByDeviceId")?,
        ciphertext: string_field(object, "ciphertext")?,
        fingerprint: string_field(object, "fingerprint")?,
        state: parse_recovery_envelope_state(&string_field(object, "state")?)?,
        created_at: current_timestamp(),
        verified_at: optional_string_field(object, "verifiedAt")?.map(|_| current_timestamp()),
        rotated_at: optional_string_field(object, "rotatedAt")?.map(|_| current_timestamp()),
        revoked_at: optional_string_field(object, "revokedAt")?.map(|_| current_timestamp()),
    })
}

pub(super) fn parse_recovery_envelope_state(
    state: &str,
) -> ControlPlaneResult<RecoveryEnvelopeState> {
    match state {
        "generated-unverified" => Ok(RecoveryEnvelopeState::GeneratedUnverified),
        "active" => Ok(RecoveryEnvelopeState::Active),
        "rotated" => Ok(RecoveryEnvelopeState::Rotated),
        "revoked" => Ok(RecoveryEnvelopeState::Revoked),
        _ => Err(shape_error("unknown recovery envelope state")),
    }
}

pub(super) fn parse_object_kind(kind: &str) -> ControlPlaneResult<ObjectKind> {
    match kind {
        "source-pack" => Ok(ObjectKind::SourcePack),
        "index-pack" => Ok(ObjectKind::IndexPack),
        "locator-index" => Ok(ObjectKind::LocatorIndex),
        "snapshot-manifest" => Ok(ObjectKind::SnapshotManifest),
        "overlay-pack" | "agent-overlay" => Ok(ObjectKind::AgentOverlay),
        _ => Err(shape_error("unknown object kind")),
    }
}

pub(super) fn parse_storage_metadata(value: &Value) -> ControlPlaneResult<ObjectMetadata> {
    let object = value_object(value)?;
    let object_key = string_field(object, "objectKey")?;
    Ok(ObjectMetadata {
        key: StorageObjectKey::new(object_key).map_err(|_| {
            ControlPlaneError::InvalidObjectKey {
                reason: "object keys must be generated opaque pack, manifest, or overlay keys",
            }
        })?,
        kind: parse_storage_object_kind(&string_field(object, "kind")?)?,
        byte_len: u64_field(object, "byteLength")?,
        hash: string_field(object, "hash")?,
        key_epoch: u64_field(object, "keyEpoch")? as u32,
        created_by_device_id: None,
        created_at_unix_ms: current_timestamp().tick,
        retention_state: parse_retention_state(
            &string_field(object, "retentionState").unwrap_or_else(|_| "current".to_string()),
        )?,
        retain_until_unix_ms: None,
    })
}

pub(super) fn parse_object_manifest_record(
    value: &Value,
) -> ControlPlaneResult<ObjectManifestRecord> {
    let object = value_object(value)?;
    Ok(ObjectManifestRecord {
        workspace_id: string_field(object, "workspaceId")?,
        snapshot_id: string_field(object, "snapshotId")?,
        manifest_id: string_field(object, "manifestId")?,
        manifest_object: parse_object_pointer(required_control_field(object, "manifestObject")?)?,
        pack_objects: array_field(object, "packObjects")?
            .iter()
            .map(parse_object_pointer)
            .collect::<ControlPlaneResult<Vec<_>>>()?,
        committed_by_device_id: string_field(object, "committedByDeviceId")?,
        committed_at: current_timestamp(),
    })
}

pub(super) fn parse_object_pointer(value: &Value) -> ControlPlaneResult<ObjectPointer> {
    let object = value_object(value)?;
    Ok(ObjectPointer {
        object_key: string_field(object, "objectKey")?,
        content_id: string_field(object, "contentId")?,
        byte_len: u64_field(object, "byteLength")?,
        hash: string_field(object, "hash")?,
        key_epoch: u64_field(object, "keyEpoch")? as u32,
        kind: parse_object_kind(&string_field(object, "kind")?)?,
        created_at: current_timestamp(),
    })
}

pub(super) fn parse_conflict_metadata_record(
    value: &Value,
) -> ControlPlaneResult<ConflictMetadataRecord> {
    let object = value_object(value)?;
    Ok(ConflictMetadataRecord {
        workspace_id: string_field(object, "workspaceId")?,
        conflict_id: string_field(object, "conflictId")?,
        conflict_kind: string_field(object, "conflictKind")?,
        paths: array_field(object, "paths")?
            .iter()
            .map(value_string)
            .collect::<ControlPlaneResult<Vec<_>>>()?,
        contains_secrets: bool_field(object, "containsSecrets")?,
        state: string_field(object, "state")?,
        base_snapshot_id: string_field(object, "baseSnapshotId")?,
        remote_snapshot_id: string_field(object, "remoteSnapshotId")?,
        detected_by_device_id: string_field(object, "detectedByDeviceId")?,
        bundle_object: match object.get("bundleObject") {
            Some(Value::Null) | None => None,
            Some(value) => Some(parse_object_pointer(value)?),
        },
        detected_at: current_timestamp(),
        resolved_by_device_id: optional_string_field(object, "resolvedByDeviceId")?,
        resolved_at: optional_string_field(object, "resolvedAt")?.map(|_| current_timestamp()),
    })
}

pub(super) fn parse_work_view_record(value: &Value) -> ControlPlaneResult<WorkViewRecord> {
    let object = value_object(value)?;
    if let Some(work_view) = object.get("workView") {
        return parse_work_view_record(work_view);
    }
    Ok(WorkViewRecord {
        workspace_id: string_field(object, "workspaceId")?,
        work_view_id: string_field(object, "workViewId")?,
        project_id: string_field(object, "projectId")?,
        name: string_field(object, "name")?,
        visible_path: string_field(object, "visiblePath")?,
        base_snapshot_id: string_field(object, "baseSnapshotId")?,
        base_workspace_version: u64_field(object, "baseWorkspaceVersion")?,
        overlay_head: optional_object_pointer_field(object, "overlayHead")?,
        overlay_version: u64_field(object, "overlayVersion")?,
        lifecycle: parse_work_view_lifecycle(&string_field(object, "lifecycle")?)?,
        created_by_device_id: string_field(object, "createdByDeviceId")?,
        updated_by_device_id: string_field(object, "updatedByDeviceId")?,
        created_at: current_timestamp(),
        updated_at: current_timestamp(),
    })
}

pub(super) fn parse_lease(value: &Value) -> ControlPlaneResult<Lease> {
    let object = value_object(value)?;
    if let Some(lease) = object.get("lease") {
        return parse_lease(lease);
    }
    Ok(Lease {
        lease_id: string_field(object, "leaseId")?,
        workspace_id: string_field(object, "workspaceId")?,
        project_id: string_field(object, "projectId")?,
        device_id: string_field(object, "deviceId")?,
        write_target_mode: parse_lease_write_target_mode(&string_field(
            object,
            "writeTargetMode",
        )?)?,
        work_view_id: optional_string_field(object, "workViewId")?,
        base_snapshot_id: string_field(object, "baseSnapshotId")?,
        version: u64_field(object, "version")?,
        execution_state: parse_lease_execution_state(&string_field(object, "executionState")?)?,
        output_state: parse_lease_output_state(&string_field(object, "outputState")?)?,
        status_code: string_field(object, "statusCode")?,
        output_object: optional_object_pointer_field(object, "outputObject")?,
        audit_object: optional_object_pointer_field(object, "auditObject")?,
        created_at: parse_control_timestamp_field(object, "createdAt")?,
        updated_at: parse_control_timestamp_field(object, "updatedAt")?,
        expires_at: parse_control_timestamp_field(object, "expiresAt")?,
    })
}

pub(super) fn parse_control_timestamp_field(
    object: &BTreeMap<String, Value>,
    field: &'static str,
) -> ControlPlaneResult<ControlPlaneTimestamp> {
    parse_control_timestamp(&string_field(object, field)?)
}

pub(super) fn parse_control_timestamp(value: &str) -> ControlPlaneResult<ControlPlaneTimestamp> {
    if let Some(tick) = value.strip_prefix('t') {
        return Ok(ControlPlaneTimestamp {
            tick: tick
                .parse::<u64>()
                .map_err(|_| shape_error("timestamp tick is invalid"))?,
        });
    }
    let parsed = OffsetDateTime::parse(value, &Rfc3339)
        .map_err(|_| shape_error("timestamp must be RFC3339 or compact tick format"))?;
    let millis = parsed.unix_timestamp_nanos() / 1_000_000;
    if millis < 0 {
        return Err(shape_error("timestamp is before Unix epoch"));
    }
    Ok(ControlPlaneTimestamp {
        tick: u64::try_from(millis).map_err(|_| shape_error("timestamp is out of range"))?,
    })
}

pub(super) fn parse_unix_timestamp(value: &str) -> ControlPlaneResult<i64> {
    let timestamp = parse_control_timestamp(value)?;
    i64::try_from(timestamp.tick / 1000).map_err(|_| shape_error("timestamp is out of range"))
}

pub(super) fn account_session_cache_key(workspace_id: Option<&str>) -> String {
    workspace_id.unwrap_or("").to_string()
}

pub(super) fn parse_lease_execution_state(state: &str) -> ControlPlaneResult<LeaseExecutionState> {
    match state {
        "active" => Ok(LeaseExecutionState::Active),
        "blocked" => Ok(LeaseExecutionState::Blocked),
        "completed" => Ok(LeaseExecutionState::Completed),
        "expired" => Ok(LeaseExecutionState::Expired),
        "revoked" => Ok(LeaseExecutionState::Revoked),
        _ => Err(shape_error("unknown lease execution state")),
    }
}

pub(super) fn parse_lease_output_state(state: &str) -> ControlPlaneResult<LeaseOutputState> {
    match state {
        "empty" => Ok(LeaseOutputState::Empty),
        "dirty" => Ok(LeaseOutputState::Dirty),
        "review-ready" => Ok(LeaseOutputState::ReviewReady),
        "accepted" => Ok(LeaseOutputState::Accepted),
        "discarded" => Ok(LeaseOutputState::Discarded),
        "conflicted" => Ok(LeaseOutputState::Conflicted),
        "retained" => Ok(LeaseOutputState::Retained),
        _ => Err(shape_error("unknown lease output state")),
    }
}

pub(super) fn parse_lease_write_target_mode(
    state: &str,
) -> ControlPlaneResult<LeaseWriteTargetMode> {
    match state {
        "direct" => Ok(LeaseWriteTargetMode::Direct),
        "work-view" => Ok(LeaseWriteTargetMode::WorkView),
        _ => Err(shape_error("unknown lease write target mode")),
    }
}

pub(super) fn optional_object_pointer_field(
    object: &BTreeMap<String, Value>,
    field: &'static str,
) -> ControlPlaneResult<Option<ObjectPointer>> {
    match object.get(field) {
        Some(Value::Null) | None => Ok(None),
        Some(value) => parse_object_pointer(value).map(Some),
    }
}

pub(super) fn parse_work_view_lifecycle(state: &str) -> ControlPlaneResult<WorkViewLifecycleState> {
    match state {
        "active" => Ok(WorkViewLifecycleState::Active),
        "review-ready" => Ok(WorkViewLifecycleState::ReviewReady),
        "accepted" => Ok(WorkViewLifecycleState::Accepted),
        "discarded" => Ok(WorkViewLifecycleState::Discarded),
        "expired" => Ok(WorkViewLifecycleState::Expired),
        "archived" => Ok(WorkViewLifecycleState::Archived),
        _ => Err(shape_error("unknown work view lifecycle state")),
    }
}

pub(super) fn required_control_field<'a>(
    object: &'a BTreeMap<String, Value>,
    field: &'static str,
) -> ControlPlaneResult<&'a Value> {
    object
        .get(field)
        .ok_or_else(|| shape_error("expected Convex object field"))
}

pub(super) fn parse_storage_object_kind(kind: &str) -> ControlPlaneResult<StorageObjectKind> {
    match kind {
        "source-pack" => Ok(StorageObjectKind::SourcePack),
        "index-pack" => Ok(StorageObjectKind::IndexPack),
        "locator-index" => Ok(StorageObjectKind::LocatorIndex),
        "snapshot-manifest" => Ok(StorageObjectKind::SnapshotManifest),
        "overlay-pack" | "agent-overlay" => Ok(StorageObjectKind::AgentOverlay),
        _ => Err(shape_error("unknown storage object kind")),
    }
}

pub(super) fn parse_retention_state(state: &str) -> ControlPlaneResult<RetentionState> {
    match state {
        "pending" => Ok(RetentionState::Pending),
        "current" => Ok(RetentionState::Current),
        "orphan-candidate" => Ok(RetentionState::OrphanCandidate),
        "retained" => Ok(RetentionState::Retained),
        "delete-eligible" => Ok(RetentionState::DeleteEligible),
        _ => Err(shape_error("unknown retention state")),
    }
}
