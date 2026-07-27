use super::generated::{HostedWorkspaceRef, Timestamp};
use super::*;

pub(super) fn workspace_ref_from_dto(
    dto: HostedWorkspaceRef,
    verifier_for_device: impl Fn(&WorkspaceId, &DeviceId) -> ControlPlaneResult<Option<String>>,
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
    let workspace_id = WorkspaceId::new(workspace_id);
    let snapshot_id = snapshot_id.map(SnapshotId::new);
    let updated_by_device_id = updated_by_device_id.map(DeviceId::new);
    if version > 0 {
        let snapshot_id = snapshot_id
            .as_ref()
            .ok_or_else(|| shape_error("signed workspace ref is missing snapshot id"))?;
        let head_signature = head_signature
            .ok_or_else(|| shape_error("signed workspace ref is missing head signature"))?;
        let signer_device_id = updated_by_device_id
            .as_ref()
            .ok_or_else(|| shape_error("signed workspace ref is missing updated device id"))?;
        // A signer this host has never learned is a trust-freshness fact, not a
        // malformed ref: a device enrolled after this client was built signs
        // perfectly valid heads. It is named so the caller can refresh trust for
        // exactly that device rather than re-reading the whole workspace blindly.
        let verifier = verifier_for_device(&workspace_id, signer_device_id)?.ok_or_else(|| {
            ControlPlaneError::UnknownSigningDevice {
                workspace_id: workspace_id.clone(),
                device_id: signer_device_id.clone(),
            }
        })?;
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
        workspace_id,
        version,
        snapshot_id,
        updated_at: parse_control_timestamp(updated_at.as_str())
            .map_err(|error| add_field_context(error, "updatedAt"))?,
        updated_by_device_id,
    })
}

fn verify_workspace_head_signature(
    workspace_id: &WorkspaceId,
    version: u64,
    snapshot_id: &SnapshotId,
    device_id: &DeviceId,
    verifier: &str,
    proof: &str,
) -> ControlPlaneResult<()> {
    let subject = workspace_head_proof_subject(workspace_id, version, snapshot_id);
    match crate::verify_device_authorization_proof(
        verifier,
        proof,
        workspace_id.as_str(),
        device_id.as_str(),
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
    value: Option<Timestamp>,
    field: &'static str,
) -> ControlPlaneResult<Option<ControlPlaneTimestamp>> {
    value
        .map(|raw| {
            parse_control_timestamp(raw.as_str()).map_err(|error| add_field_context(error, field))
        })
        .transpose()
}

/// Decode a hosted DTO with one field replaced by a raw wire value. A typed DTO
/// field cannot hold a value the contract rejects, so a test that wants to watch
/// the decoder refuse one has to go back through JSON.
#[cfg(test)]
pub(super) fn decode_dto_with_field<T, U>(
    dto: &T,
    field: &str,
    value: serde_json::Value,
) -> Result<U, String>
where
    T: serde::Serialize,
    U: serde::de::DeserializeOwned,
{
    let mut json = serde_json::to_value(dto).expect("hosted DTO serializes");
    json.as_object_mut()
        .expect("hosted DTO serializes as a JSON object")
        .insert(field.to_string(), value);
    serde_json::from_value::<U>(json).map_err(|error| error.to_string())
}

/// A wire timestamp from a source literal. The caller is asserting the literal
/// is RFC 3339; a value that came off the wire is already a `Timestamp` and
/// must not be round-tripped through this.
pub(super) fn wire_timestamp(value: &str) -> Timestamp {
    Timestamp::new(value).expect("source literal is an RFC 3339 timestamp")
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
