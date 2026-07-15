use super::*;

// Device-proof subjects are contract-tested against tests/contracts/proofs/device-proof-subjects.json.

pub(super) fn conflict_reconcile_proof_subject(input: &ConflictOccurrenceReconcile) -> String {
    let bundle = input.bundle_object.as_ref();
    format!(
        "conflictId={}\nconflictKind={}\n{}\nbaseSnapshotId={}\nremoteSnapshotId={}\noccurrenceVersion={}\ndesiredState={}\ncontainsSecrets={}\nreason={}\nbundleObjectKey={}\nbundleContentId={}\nbundleHash={}\nbundleByteLength={}\nbundleKeyEpoch={}\nbundleKind={}",
        input.conflict_id.as_str(),
        input.conflict_kind,
        string_array_proof_subject("paths", &input.paths),
        input.base_snapshot_id.as_str(),
        input.remote_snapshot_id.as_str(),
        input.occurrence_version,
        input.desired_state.as_str(),
        input.contains_secrets,
        input.reason,
        bundle.map_or("", |pointer| pointer.object_key.as_str()),
        bundle.map_or("", |pointer| pointer.content_id.as_str()),
        bundle.map_or("", |pointer| pointer.hash.as_str()),
        bundle.map_or_else(String::new, |pointer| pointer.byte_len.to_string()),
        bundle.map_or_else(String::new, |pointer| pointer.key_epoch.to_string()),
        bundle.map_or("", |pointer| pointer.kind.as_str()),
    )
}

pub(super) fn conflict_list_proof_subject(workspace_id: &str) -> String {
    format!("workspaceId={workspace_id}")
}

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
    content_id: Option<&str>,
) -> String {
    format!(
        "authorityFormatVersion={CURRENT_SNAPSHOT_AUTHORITY_FORMAT_VERSION}\nobjectKey={object_key}\nkind={}\nbyteLength={byte_len}\ncontentId={}",
        kind.as_str(),
        content_id.unwrap_or_default()
    )
}

pub(super) fn metadata_bindings_proof_subject(bindings: &[MetadataBindingInput]) -> String {
    let values = bindings
        .iter()
        .map(|binding| {
            format!(
                "{}:{}:{}:{}:{}:{}",
                binding.logical_id,
                binding.record_kind.as_str(),
                object_pointer_proof_subject(&binding.object),
                binding.sidecar.digest,
                binding.sidecar.child_logical_ids.join(","),
                binding.sidecar.direct_object_keys.join(","),
            )
        })
        .collect::<Vec<_>>();
    string_array_proof_subject("bindings", &values)
}

pub(super) fn resolve_metadata_bindings_proof_subject(logical_ids: &[String]) -> String {
    string_array_proof_subject("logicalIds", logical_ids)
}

pub(super) fn snapshot_root_proof_subject(commit: &SnapshotRootCommit) -> String {
    format!(
        "snapshotId={}\nmanifestId={}\nmanifestObject={}\nnamespaceRootId={}\n{}",
        commit.snapshot_id.as_str(),
        commit.manifest_id.as_str(),
        object_pointer_proof_subject(&commit.manifest_object),
        commit.namespace_root_id,
        string_array_proof_subject("extraRootLogicalIds", &commit.extra_root_logical_ids),
    )
}

pub(super) fn snapshot_root_query_proof_subject(snapshot_id: &str) -> String {
    format!("snapshotId={snapshot_id}")
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

pub(super) fn work_view_create_proof_subject(input: &WorkViewCreate) -> String {
    format!(
        "workViewId={}\nprojectId={}\nname={}\nvisiblePath={}\nbaseSnapshotId={}\nbaseWorkspaceVersion={}\n{}\n{}",
        input.work_view_id.as_str(),
        input.project_id.as_str(),
        input.name,
        input.visible_path,
        input.base_snapshot_id.as_str(),
        input.base_workspace_version,
        optional_proof_field("expiresAt", input.expires_at.as_deref()),
        optional_proof_field("retainUntil", input.retain_until.as_deref()),
    )
}

pub(super) fn work_view_lifecycle_proof_subject(input: &WorkViewLifecycleUpdate) -> String {
    format!(
        "workViewId={}\nlifecycle={}",
        input.work_view_id.as_str(),
        input.lifecycle.as_str()
    )
}

pub(super) fn work_view_list_proof_subject(include_all: bool) -> String {
    format!("includeAll={include_all}")
}

pub(super) fn work_view_restore_proof_subject(work_view_id: &str) -> String {
    format!("workViewId={work_view_id}")
}

pub(super) fn work_view_overlay_proof_subject(input: &WorkViewOverlayCommit) -> String {
    format!(
        "workViewId={}\nexpectedOverlayVersion={}\noverlayObject={}",
        input.work_view_id.as_str(),
        input.expected_overlay_version,
        object_pointer_proof_subject(&input.overlay_object)
    )
}

pub(super) const LEASE_LIST_PROOF_SUBJECT: &str = "compact=true";

pub(super) fn lease_create_proof_subject(input: &LeaseCreate) -> String {
    [
        format!("leaseId={}", input.lease_id.as_str()),
        format!("projectId={}", input.project_id.as_str()),
        format!(
            "targetDeviceRef={}",
            input.target_device_ref.as_deref().unwrap_or("")
        ),
        format!(
            "originDeviceRef={}",
            input.origin_device_ref.as_deref().unwrap_or("")
        ),
        format!("writeTargetMode={}", input.write_target_mode.as_str()),
        format!(
            "workViewId={}",
            input
                .work_view_id
                .as_ref()
                .map(|id| id.as_str())
                .unwrap_or("")
        ),
        format!("baseSnapshotId={}", input.base_snapshot_id.as_str()),
        format!("taskLabel={}", input.task_label.as_deref().unwrap_or("")),
        format!("sessionState={}", input.session_state.as_str()),
        format!("statusCode={}", input.status_code),
        format!("expiresAt={}", input.expires_at),
    ]
    .join("\n")
}

pub(super) fn lease_update_proof_subject(input: &LeaseUpdate) -> String {
    [
        format!("leaseId={}", input.lease_id.as_str()),
        format!("expectedVersion={}", input.expected_version),
        format!(
            "eventKind={}",
            input.event_kind.map(CompactEventKind::as_str).unwrap_or("")
        ),
        format!(
            "sessionState={}",
            input
                .session_state
                .map(LeaseSessionState::as_str)
                .unwrap_or("")
        ),
        format!("statusCode={}", input.status_code.as_deref().unwrap_or("")),
    ]
    .join("\n")
}

pub(super) fn bootstrap_session_proof_subject(
    input: &BootstrapSessionInput,
    bootstrap_token_hash: &str,
) -> String {
    [
        format!("workspaceId={}", input.workspace_id.as_str()),
        format!("host={}", input.host.as_deref().unwrap_or_default()),
        format!(
            "leaseHandoffDigest={}",
            input.lease_handoff_digest.as_deref().unwrap_or_default()
        ),
        format!(
            "leaseId={}",
            input
                .lease_id
                .as_ref()
                .map(|id| id.as_str())
                .unwrap_or_default()
        ),
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

fn string_array_proof_subject(label: &str, values: &[String]) -> String {
    let mut fields = vec![format!("{label}.count={}", values.len())];
    for (index, value) in values.iter().enumerate() {
        fields.push(format!("{label}.{index}.length={}", value.len()));
        fields.push(format!("{label}.{index}.value={value}"));
    }
    fields.join("\n")
}

fn optional_proof_field(label: &str, value: Option<&str>) -> String {
    format!(
        "{label}.present={}\n{label}.value={}",
        value.is_some(),
        value.unwrap_or_default()
    )
}

#[cfg(test)]
pub(super) fn number_value(value: u64) -> Value {
    Value::Float64(value as f64)
}

pub(super) fn generated_object_key(kind: ObjectKind, seed: &str) -> String {
    let digest = blake3::hash(seed.as_bytes()).to_hex().to_string();
    let suffix = &digest[..16];
    match kind {
        ObjectKind::SourcePack => format!("packs_pk_{suffix}"),
        ObjectKind::LocatorIndex => format!("indexes_ix_{suffix}"),
        ObjectKind::SnapshotManifest => format!("manifests_mf_{suffix}"),
        ObjectKind::SnapshotMetadataPage => format!("metadata_mp_{digest}"),
        ObjectKind::AgentOverlay => format!("packs_pk_{suffix}"),
        ObjectKind::ConflictBundle => format!("conflicts_cb_{suffix}"),
    }
}

pub(super) fn generate_bootstrap_token() -> ControlPlaneResult<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| ControlPlaneError::Storage(error.to_string()))?;
    Ok(format!("bowline_bootstrap_{}", BASE64_URL.encode(bytes)))
}
