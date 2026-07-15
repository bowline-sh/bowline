use super::generated::HostedWorkspaceRef;
use super::*;

pub(super) fn workspace_ref_from_dto(
    dto: HostedWorkspaceRef,
    verifier_for_device: impl Fn(&str, &str) -> ControlPlaneResult<Option<String>>,
) -> ControlPlaneResult<WorkspaceRef> {
    let HostedWorkspaceRef {
        workspace_id,
        version,
        snapshot_id,
        updated_at,
        updated_by_device_id,
        head_signature,
    } = dto;
    // Re-run the workspace-head signature verification on the typed DTO: a bare
    // serde decode is not verification. This performs the exact same check over
    // the exact same bytes as the former Value-based parser, delegating to the
    // unchanged `verify_workspace_head_signature` verifier.
    if version > 0 {
        let head_signature = head_signature
            .ok_or_else(|| shape_error("signed workspace ref is missing head signature"))?;
        let signer_device_id = updated_by_device_id
            .as_deref()
            .ok_or_else(|| shape_error("signed workspace ref is missing updated device id"))?;
        let verifier = verifier_for_device(&workspace_id, signer_device_id)?
            .ok_or_else(|| shape_error("signed workspace ref verifier is unavailable locally"))?;
        verify_workspace_head_signature(
            &workspace_id,
            version,
            &snapshot_id,
            signer_device_id,
            &verifier,
            &head_signature,
        )?;
    } else if snapshot_id != bowline_core::hosted::EMPTY_SNAPSHOT_ID {
        return Err(shape_error(
            "genesis workspace ref must reference the empty snapshot",
        ));
    }
    Ok(WorkspaceRef {
        workspace_id: WorkspaceId::new(workspace_id),
        version,
        snapshot_id: SnapshotId::new(snapshot_id),
        updated_at: parse_control_timestamp(&updated_at)
            .map_err(|error| add_field_context(error, "updatedAt"))?,
        updated_by_device_id: updated_by_device_id.map(DeviceId::new),
    })
}

fn verify_workspace_head_signature(
    workspace_id: &str,
    version: u64,
    snapshot_id: &str,
    device_id: &str,
    verifier: &str,
    proof: &str,
) -> ControlPlaneResult<()> {
    let subject = workspace_head_proof_subject(workspace_id, version, snapshot_id);
    match crate::verify_device_authorization_proof(
        verifier,
        proof,
        workspace_id,
        device_id,
        "sign-workspace-head",
        &subject,
    ) {
        Ok(()) => Ok(()),
        Err(crate::device_proofs::DeviceAuthorizationProofError::InvalidPrefix) => {
            Err(shape_error("signed workspace ref proof has invalid prefix"))
        }
        Err(crate::device_proofs::DeviceAuthorizationProofError::MalformedVerifier) => {
            Err(shape_error("signed workspace ref verifier is malformed"))
        }
        Err(crate::device_proofs::DeviceAuthorizationProofError::MalformedSignature) => {
            Err(shape_error("signed workspace ref signature is malformed"))
        }
        Err(crate::device_proofs::DeviceAuthorizationProofError::VerificationFailed) => {
            Err(shape_error("workspace ref signed head verification failed"))
        }
    }
}

#[cfg(test)]
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
        "lease.claimed" => Ok(CompactEventKind::LeaseClaimed),
        "lease.completed" => Ok(CompactEventKind::LeaseCompleted),
        "lease.created" => Ok(CompactEventKind::LeaseCreated),
        "lease.dispatched" => Ok(CompactEventKind::LeaseDispatched),
        "lease.review_ready" => Ok(CompactEventKind::LeaseReviewReady),
        "lease.updated" => Ok(CompactEventKind::LeaseUpdated),
        "snapshot_root.committed" => Ok(CompactEventKind::SnapshotRootCommitted),
        "object_pointer.added" => Ok(CompactEventKind::ObjectPointerAdded),
        "overlay.changed" => Ok(CompactEventKind::OverlayChanged),
        "work.accepted" => Ok(CompactEventKind::WorkAccepted),
        "work.cleanup_completed" => Ok(CompactEventKind::WorkCleanupCompleted),
        "work.cleanup_previewed" => Ok(CompactEventKind::WorkCleanupPreviewed),
        "work.created" => Ok(CompactEventKind::WorkCreated),
        "work.discarded" => Ok(CompactEventKind::WorkDiscarded),
        "work.restored" => Ok(CompactEventKind::WorkRestored),
        "work.review_ready" => Ok(CompactEventKind::WorkReviewReady),
        "work.updated" => Ok(CompactEventKind::WorkUpdated),
        "workspace.created" => Ok(CompactEventKind::WorkspaceCreated),
        "workspace_ref.advanced" => Ok(CompactEventKind::WorkspaceRefAdvanced),
        _ => Err(shape_error("unknown compact event kind")),
    }
}

#[cfg(test)]
pub(super) fn parse_object_kind(kind: &str) -> ControlPlaneResult<ObjectKind> {
    match kind {
        "source-pack" => Ok(ObjectKind::SourcePack),
        "locator-index" => Ok(ObjectKind::LocatorIndex),
        "snapshot-metadata-page" => Ok(ObjectKind::SnapshotMetadataPage),
        "snapshot-manifest" => Ok(ObjectKind::SnapshotManifest),
        "overlay-pack" => Ok(ObjectKind::AgentOverlay),
        "conflict-bundle" => Ok(ObjectKind::ConflictBundle),
        _ => Err(shape_error("unknown object kind")),
    }
}

/// Re-validate an optional canonical timestamp string decoded from a typed
/// hosted DTO, attaching the wire field name to any parse error. Shared by the
/// typed hosted boundaries (recovery, devices) that read `Option<String>`
/// timestamp fields rather than raw Convex objects.
pub(super) fn optional_timestamp_from_dto(
    value: Option<String>,
    field: &'static str,
) -> ControlPlaneResult<Option<ControlPlaneTimestamp>> {
    value
        .map(|raw| parse_control_timestamp(&raw).map_err(|error| add_field_context(error, field)))
        .transpose()
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

#[cfg(test)]
pub(super) fn parse_lease_session_state(state: &str) -> ControlPlaneResult<LeaseSessionState> {
    match state {
        "provisional" => Ok(LeaseSessionState::Provisional),
        "open" => Ok(LeaseSessionState::Open),
        "completed" => Ok(LeaseSessionState::Completed),
        _ => Err(shape_error("unknown lease session state")),
    }
}

#[cfg(test)]
pub(super) fn parse_lease_write_target_mode(
    state: &str,
) -> ControlPlaneResult<LeaseWriteTargetMode> {
    match state {
        "direct" => Ok(LeaseWriteTargetMode::Direct),
        "work-view" => Ok(LeaseWriteTargetMode::WorkView),
        _ => Err(shape_error("unknown lease write target mode")),
    }
}

// Retained only for the cfg(test) proof-contract and parser fixtures that
// rebuild a WorkViewLifecycleUpdate from a fixture string; production decoding
// now happens in the typed hosted work_views boundary.
#[cfg(test)]
pub(super) fn parse_work_view_lifecycle(state: &str) -> ControlPlaneResult<WorkViewLifecycleState> {
    match state {
        "active" => Ok(WorkViewLifecycleState::Active),
        "review-ready" => Ok(WorkViewLifecycleState::ReviewReady),
        "accepted" => Ok(WorkViewLifecycleState::Accepted),
        "discarded" => Ok(WorkViewLifecycleState::Discarded),
        _ => Err(shape_error("unknown work view lifecycle state")),
    }
}

#[cfg(test)]
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
