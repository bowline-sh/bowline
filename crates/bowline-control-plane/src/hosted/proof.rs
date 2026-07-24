use super::*;
use crate::Sha256Checksum;

// Device-proof subjects are contract-tested against tests/contracts/proofs/device-proof-subjects.json.

pub(super) fn workspace_ref_proof_subject(expected_version: u64, next_snapshot_id: &str) -> String {
    format!("expectedVersion={expected_version}\nnextSnapshotId={next_snapshot_id}")
}

pub(super) fn workspace_head_proof_subject(
    workspace_id: &str,
    version: u64,
    snapshot_id: &str,
) -> String {
    format!("workspaceId={workspace_id}\nversion={version}\nsnapshotId={snapshot_id}")
}

pub(super) fn upload_intent_proof_subject(
    object_key: &str,
    kind: ObjectKind,
    byte_len: u64,
    checksum_sha256: &Sha256Checksum,
    content_id: Option<&str>,
) -> String {
    format!(
        "authorityFormatVersion={CURRENT_SNAPSHOT_AUTHORITY_FORMAT_VERSION}\nobjectKey={object_key}\nkind={}\nbyteLength={byte_len}\nchecksumSha256={}\ncontentId={}",
        kind.as_str(),
        checksum_sha256.as_str(),
        content_id.unwrap_or_default()
    )
}

pub(super) fn download_intent_proof_subject(
    object_key: &str,
    range: Option<bowline_storage::ByteRange>,
) -> String {
    match range {
        Some(range) => format!(
            "objectKey={object_key}\nrange=bounded\noffset={}\nlength={}",
            range.offset, range.length
        ),
        None => format!("objectKey={object_key}\nrange=full\noffset=\nlength="),
    }
}

pub(super) fn upload_verification_proof_subject(
    object_key: &str,
    byte_len: u64,
    content_id: Option<&str>,
) -> String {
    format!(
        "objectKey={object_key}\nbyteLength={byte_len}\ncontentId={}",
        content_id.unwrap_or_default()
    )
}

pub(super) fn object_retention_proof_subject(
    object_key: &str,
    retention_state: RetentionState,
) -> String {
    format!(
        "objectKey={object_key}\nretentionState={}",
        retention_state_value(retention_state)
    )
}

pub(super) fn retention_state_value(state: RetentionState) -> &'static str {
    match state {
        RetentionState::Pending => "pending",
        RetentionState::Current => "current",
        RetentionState::OrphanCandidate => "orphan-candidate",
        RetentionState::Retained => "retained",
        RetentionState::DeleteEligible => "delete-eligible",
    }
}

pub(super) fn object_metadata_proof_subject(pointer: &ObjectPointer) -> String {
    format!("object={}", object_pointer_proof_subject(pointer))
}

pub(super) fn bootstrap_session_proof_subject(
    input: &BootstrapSessionInput,
    bootstrap_token_hash: &str,
) -> String {
    [
        format!("workspaceId={}", input.workspace_id.as_str()),
        format!("host={}", input.host.as_deref().unwrap_or_default()),
        // The hosted subject builder includes the retired lease-handoff fields
        // unconditionally (absent -> empty), so the client signs the same empty
        // lines until the wire contract drops them server-side.
        "leaseHandoffDigest=".to_string(),
        "leaseId=".to_string(),
        format!("root={}", input.root.as_deref().unwrap_or_default()),
        format!("runtime={}", input.runtime.as_deref().unwrap_or_default()),
        format!(
            "setupReceiptsDigest={}",
            input.setup_receipts_digest.as_deref().unwrap_or_default()
        ),
        format!("expiresInTicks={}", input.expires_in_ticks),
        format!("bootstrapTokenHash={bootstrap_token_hash}"),
    ]
    .join("\n")
}

pub(super) fn sha256_token_hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{digest:x}")
}

pub(super) fn object_pointer_proof_subject(pointer: &ObjectPointer) -> String {
    format!(
        "{}:{}:{}:{}:{}:{}",
        pointer.object_key,
        pointer.kind.as_str(),
        pointer.byte_len,
        pointer.hash,
        pointer.key_epoch,
        pointer.content_id.as_str()
    )
}

#[cfg(test)]
pub(super) fn number_value(value: u64) -> Value {
    Value::Float64(value as f64)
}

pub(super) fn generated_object_key(kind: ObjectKind, seed: &str) -> String {
    let digest = blake3::hash(seed.as_bytes()).to_hex().to_string();
    // Manifest-sync keys are the sealed hash: a full 64-hex suffix with no
    // length tolerance, so the whole digest is the key suffix.
    match kind {
        ObjectKind::Blob => format!("b_{digest}"),
        ObjectKind::Manifest => format!("m_{digest}"),
    }
}

pub(super) fn generate_bootstrap_token() -> ControlPlaneResult<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| ControlPlaneError::Storage(error.to_string()))?;
    Ok(format!("bowline_bootstrap_{}", BASE64_URL.encode(bytes)))
}
