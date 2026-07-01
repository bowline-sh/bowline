use super::*;

pub(super) fn object_pointer_value(pointer: &ObjectPointer) -> Value {
    Value::Object(args([
        ("byteLength", number_value(pointer.byte_len)),
        ("contentId", Value::from(pointer.content_id.clone())),
        ("hash", Value::from(pointer.hash.clone())),
        ("keyEpoch", number_value(u64::from(pointer.key_epoch))),
        ("kind", Value::from(pointer.kind.as_str())),
        ("objectKey", Value::from(pointer.object_key.clone())),
    ]))
}

pub(super) fn object_pointer_array(pointers: &[ObjectPointer]) -> Value {
    Value::Array(pointers.iter().map(object_pointer_value).collect())
}

pub(super) fn status_event_watermarks_value(watermarks: &StatusEventWatermarks) -> Value {
    let mut object = ConvexArgs::new();
    if let Some(last_event_id) = watermarks.last_event_id.as_ref() {
        object.insert(
            "lastEventId".to_string(),
            Value::from(last_event_id.clone()),
        );
    }
    if let Some(last_scan_at) = watermarks.last_scan_at.as_ref() {
        object.insert("lastScanAt".to_string(), Value::from(last_scan_at.clone()));
    }
    if let Some(sync_state) = watermarks.sync_state.as_ref() {
        object.insert("syncState".to_string(), Value::from(sync_state.clone()));
    }
    if let Some(watcher_state) = watermarks.watcher_state.as_ref() {
        object.insert(
            "watcherState".to_string(),
            Value::from(watcher_state.clone()),
        );
    }
    if let Some(network_state) = watermarks.network_state.as_ref() {
        object.insert(
            "networkState".to_string(),
            Value::from(network_state.clone()),
        );
    }
    Value::Object(object)
}

pub(super) fn status_sync_queue_value(queue: &StatusSyncQueueSnapshot) -> Value {
    Value::Object(args([
        ("attention", number_value(queue.attention)),
        ("blockedOffline", number_value(queue.blocked_offline)),
        ("claimed", number_value(queue.claimed)),
        ("completed", number_value(queue.completed)),
        ("queued", number_value(queue.queued)),
        ("waitingRetry", number_value(queue.waiting_retry)),
    ]))
}

pub(super) fn status_index_value(index: &StatusIndexSnapshot) -> Value {
    Value::Object(args([
        ("fileCount", number_value(index.file_count)),
        ("pathCount", number_value(index.path_count)),
        ("state", Value::from(index.state.clone())),
        ("summary", Value::from(index.summary.clone())),
    ]))
}

pub(super) fn status_workspace_summary_value(summary: &StatusWorkspaceSummarySnapshot) -> Value {
    let mut object = ConvexArgs::new();
    if let Some(env_file_count) = summary.env_file_count {
        object.insert("envFileCount".to_string(), number_value(env_file_count));
    }
    if let Some(repo_count) = summary.repo_count {
        object.insert("repoCount".to_string(), number_value(repo_count));
    }
    if let Some(total_projects) = summary.total_projects {
        object.insert("totalProjects".to_string(), number_value(total_projects));
    }
    Value::Object(object)
}

pub(super) fn status_item_value(item: &StatusItemSnapshot) -> Value {
    let mut object = ConvexArgs::new();
    object.insert("kind".to_string(), Value::from(item.kind.clone()));
    object.insert("summary".to_string(), Value::from(item.summary.clone()));
    if let Some(path) = item.path.as_ref() {
        object.insert("path".to_string(), Value::from(path.clone()));
    }
    if let Some(event_name) = item.event_name.as_ref() {
        object.insert("eventName".to_string(), Value::from(event_name.clone()));
    }
    Value::Object(object)
}

pub(super) fn status_limit_value(limit: &StatusLimitSnapshot) -> Value {
    let mut object = ConvexArgs::new();
    object.insert(
        "capability".to_string(),
        Value::from(limit.capability.clone()),
    );
    object.insert(
        "unavailableBecause".to_string(),
        Value::from(limit.unavailable_because.clone()),
    );
    object.insert(
        "stillWorks".to_string(),
        Value::Array(limit.still_works.iter().cloned().map(Value::from).collect()),
    );
    if let Some(path) = limit.path.as_ref() {
        object.insert("path".to_string(), Value::from(path.clone()));
    }
    Value::Object(object)
}

pub(super) fn conflict_publish_proof_subject(input: &ConflictMetadataPublish) -> String {
    format!(
        "conflictId={}\nconflictKind={}\npaths={}\nbaseSnapshotId={}\nremoteSnapshotId={}\ncontainsSecrets={}",
        input.conflict_id,
        input.conflict_kind,
        input.paths.join(","),
        input.base_snapshot_id,
        input.remote_snapshot_id,
        input.contains_secrets
    )
}

pub(super) fn conflict_resolution_proof_subject(input: &ConflictResolutionMark) -> String {
    format!(
        "conflictId={}\nresolution={}",
        input.conflict_id,
        input.resolution.as_str()
    )
}

pub(super) fn workspace_ref_proof_subject(expected_version: u64, next_snapshot_id: &str) -> String {
    format!("expectedVersion={expected_version}\nnextSnapshotId={next_snapshot_id}")
}

pub(super) fn upload_intent_proof_subject(
    object_key: &str,
    kind: ObjectKind,
    byte_len: u64,
    content_id: Option<&str>,
) -> String {
    format!(
        "objectKey={object_key}\nkind={}\nbyteLength={byte_len}\ncontentId={}",
        kind.as_str(),
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

pub(super) fn delete_intent_proof_subject(request: &DeleteIntentRequest) -> String {
    format!(
        "objectKey={}\nkind={}\nkeyEpoch={}\nretentionState=delete-eligible",
        request.object_key,
        request
            .object_kind
            .map(ObjectKind::as_str)
            .unwrap_or_default(),
        request
            .key_epoch
            .map(|key_epoch| key_epoch.to_string())
            .unwrap_or_default()
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

pub(super) fn object_manifest_proof_subject(commit: &ObjectManifestCommit) -> String {
    let pack_objects = commit
        .pack_objects
        .iter()
        .map(object_pointer_proof_subject)
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "snapshotId={}\nmanifestId={}\nmanifestObject={}\npackObjects={pack_objects}",
        commit.snapshot_id,
        commit.manifest_id,
        object_pointer_proof_subject(&commit.manifest_object),
    )
}

pub(super) fn object_metadata_proof_subject(pointer: &ObjectPointer) -> String {
    format!("object={}", object_pointer_proof_subject(pointer))
}

pub(super) fn snapshot_manifest_pointer_proof_subject(snapshot_id: &str) -> String {
    format!("snapshotId={snapshot_id}")
}

pub(super) fn work_view_create_proof_subject(input: &WorkViewCreate) -> String {
    format!(
        "workViewId={}\nprojectId={}\nname={}\nvisiblePath={}\nbaseSnapshotId={}\nbaseWorkspaceVersion={}",
        input.work_view_id,
        input.project_id,
        input.name,
        input.visible_path,
        input.base_snapshot_id,
        input.base_workspace_version
    )
}

pub(super) fn work_view_lifecycle_proof_subject(input: &WorkViewLifecycleUpdate) -> String {
    format!(
        "workViewId={}\nlifecycle={}",
        input.work_view_id,
        input.lifecycle.as_str()
    )
}

pub(super) fn work_view_overlay_proof_subject(input: &WorkViewOverlayCommit) -> String {
    format!(
        "workViewId={}\nexpectedOverlayVersion={}\noverlayObject={}",
        input.work_view_id,
        input.expected_overlay_version,
        object_pointer_proof_subject(&input.overlay_object)
    )
}

pub(super) fn lease_create_proof_subject(input: &LeaseCreate) -> String {
    [
        format!("leaseId={}", input.lease_id),
        format!("projectId={}", input.project_id),
        format!("writeTargetMode={}", input.write_target_mode.as_str()),
        format!("workViewId={}", input.work_view_id.as_deref().unwrap_or("")),
        format!("baseSnapshotId={}", input.base_snapshot_id),
        format!("executionState={}", input.execution_state.as_str()),
        format!("outputState={}", input.output_state.as_str()),
        format!("statusCode={}", input.status_code),
        format!("expiresAt={}", input.expires_at),
        lease_pointer_proof_subject("outputObject", input.output_object.as_ref()),
        lease_pointer_proof_subject("auditObject", input.audit_object.as_ref()),
    ]
    .join("\n")
}

pub(super) fn lease_update_proof_subject(input: &LeaseUpdate) -> String {
    [
        format!("leaseId={}", input.lease_id),
        format!("expectedVersion={}", input.expected_version),
        format!(
            "eventKind={}",
            input.event_kind.map(CompactEventKind::as_str).unwrap_or("")
        ),
        format!(
            "executionState={}",
            input
                .execution_state
                .map(LeaseExecutionState::as_str)
                .unwrap_or("")
        ),
        format!(
            "outputState={}",
            input
                .output_state
                .map(LeaseOutputState::as_str)
                .unwrap_or("")
        ),
        format!("statusCode={}", input.status_code.as_deref().unwrap_or("")),
        lease_pointer_proof_subject("outputObject", input.output_object.as_ref()),
        lease_pointer_proof_subject("auditObject", input.audit_object.as_ref()),
    ]
    .join("\n")
}

pub(super) fn lease_pointer_proof_subject(label: &str, pointer: Option<&ObjectPointer>) -> String {
    let Some(pointer) = pointer else {
        return format!("{label}=");
    };
    [
        format!("{label}.kind={}", pointer.kind.as_str()),
        format!("{label}.objectKey={}", pointer.object_key),
        format!("{label}.contentId={}", pointer.content_id),
        format!("{label}.hash={}", pointer.hash),
        format!("{label}.byteLength={}", pointer.byte_len),
        format!("{label}.keyEpoch={}", pointer.key_epoch),
    ]
    .join("\n")
}

pub(super) fn bootstrap_session_proof_subject(
    input: &BootstrapSessionInput,
    bootstrap_token_hash: &str,
) -> String {
    [
        format!("workspaceId={}", input.workspace_id),
        format!("host={}", input.host.as_deref().unwrap_or_default()),
        format!("root={}", input.root.as_deref().unwrap_or_default()),
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
        "{}:{}:{}:{}:{}",
        pointer.object_key,
        pointer.kind.as_str(),
        pointer.byte_len,
        pointer.hash,
        pointer.key_epoch
    )
}

pub(super) fn number_value(value: u64) -> Value {
    Value::Float64(value as f64)
}

pub(super) fn generated_object_key(kind: ObjectKind, seed: &str) -> String {
    let suffix = blake3::hash(seed.as_bytes()).to_hex()[..16].to_string();
    match kind {
        ObjectKind::SourcePack => format!("packs_pk_{suffix}"),
        ObjectKind::IndexPack | ObjectKind::LocatorIndex => format!("indexes_ix_{suffix}"),
        ObjectKind::SnapshotManifest => format!("manifests_mf_{suffix}"),
        ObjectKind::AgentOverlay => format!("packs_pk_{suffix}"),
    }
}

pub(super) fn generate_bootstrap_token() -> ControlPlaneResult<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| ControlPlaneError::Storage(error.to_string()))?;
    Ok(format!("bowline_bootstrap_{}", BASE64_URL.encode(bytes)))
}

pub(super) fn current_timestamp() -> ControlPlaneTimestamp {
    let tick = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default();
    ControlPlaneTimestamp { tick }
}
