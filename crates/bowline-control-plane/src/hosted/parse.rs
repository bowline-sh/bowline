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
    // unchanged `verify_workspace_head_signature` verifier. Every real head is
    // version >= 1 and must carry a manifest-backed snapshot id; a genesis
    // (version 0) ref has no head, no snapshot id, and no signature.
    if version > 0 {
        let snapshot_id = snapshot_id
            .as_deref()
            .ok_or_else(|| shape_error("signed workspace ref is missing snapshot id"))?;
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
            snapshot_id,
            signer_device_id,
            &verifier,
            &head_signature,
        )?;
    }
    Ok(WorkspaceRef {
        workspace_id: WorkspaceId::new(workspace_id),
        version,
        snapshot_id: snapshot_id.map(SnapshotId::new),
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
#[cfg(test)]
pub(super) fn parse_object_kind(kind: &str) -> ControlPlaneResult<ObjectKind> {
    match kind {
        "blob" => Ok(ObjectKind::Blob),
        "manifest" => Ok(ObjectKind::Manifest),
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
