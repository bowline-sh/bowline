use std::{
    fs,
    path::{Path, PathBuf},
};

use bowline_control_plane::{
    ControlPlaneClient, ControlPlaneError, ControlPlaneTimestamp, ObjectKind, ObjectMetadataCommit,
    ObjectPointer, ObjectRetentionStateUpdate, UploadIntentRequest, WorkViewCreate,
    WorkViewOverlayCommit, WorkViewUpdateError, WorkspaceRef,
};
use bowline_core::{
    events::EventName,
    ids::{ContentId, DeviceId},
    work_views::{WorkView, WorkViewLifecycle, WorkViewSyncState},
    workspace_graph::normalize_workspace_path,
};
use bowline_storage::{
    ByteStore, ByteStoreError, ObjectKind as StorageObjectKind, PackRecordInput, PackWriteOutput,
    RetentionState as StorageRetentionState, StorageKey, write_source_packs,
};
use serde::Serialize;

use crate::metadata::MetadataStore;

use super::{
    WorkViewError, WorkViewOverlaySyncError,
    diff::filesystem_overlay_deltas,
    overlay,
    paths::{
        append_work_event, clean_accept_policy, expand_display_path,
        is_clean_accept_policy_eligible, is_ignored_clean_accept_policy,
        workspace_path_for_project_file,
    },
};

pub struct WorkViewOverlaySyncOptions {
    pub db_path: PathBuf,
    pub device_id: DeviceId,
    pub storage_key: StorageKey,
    pub key_epoch: u32,
    pub generated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkViewOverlaySyncReport {
    pub uploaded: usize,
    pub attention: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OverlayPayload {
    schema_version: u32,
    work_view_id: String,
    base_snapshot_id: String,
    entries: Vec<OverlayPayloadEntry>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OverlayPayloadEntry {
    path: String,
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<String>,
    contains_secrets: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes: Option<Vec<u8>>,
}

pub fn sync_local_work_view_overlays(
    options: WorkViewOverlaySyncOptions,
    control_plane: &dyn ControlPlaneClient,
    byte_store: &dyn ByteStore,
    workspace_ref: &WorkspaceRef,
) -> Result<WorkViewOverlaySyncReport, WorkViewOverlaySyncError> {
    let store = MetadataStore::open(&options.db_path)?;
    let workspace = store
        .current_workspace()?
        .ok_or(WorkViewError::MissingWorkspace)?;
    let mut report = WorkViewOverlaySyncReport {
        uploaded: 0,
        attention: 0,
    };
    let mut pending = Vec::new();

    for work_view in store.work_views(&workspace.id, true, None)? {
        if work_view.lifecycle != WorkViewLifecycle::Active {
            continue;
        }
        if matches!(
            work_view.sync_state,
            WorkViewSyncState::Attention | WorkViewSyncState::Conflicted
        ) {
            continue;
        }
        let deltas = match overlay_deltas_for_upload(&store, &work_view) {
            Ok(deltas) => deltas,
            Err(error) => {
                let mut work_view = work_view;
                work_view.sync_state = WorkViewSyncState::Attention;
                work_view.attention = vec![format!(
                    "Work view overlay needs review before sync: {error}"
                )];
                work_view.updated_at = options.generated_at.clone();
                store.upsert_work_view(&work_view)?;
                report.attention += 1;
                continue;
            }
        };
        if deltas.is_empty()
            && work_view.sync_state != WorkViewSyncState::LocalOnly
            && work_view.overlay_head == "overlay_empty"
        {
            continue;
        }
        pending.push((work_view, deltas));
    }
    if pending.is_empty() {
        return Ok(report);
    }

    let mut remote_views = control_plane.list_work_views(workspace.id.as_str(), true)?;
    for (mut work_view, deltas) in pending {
        if deltas
            .iter()
            .any(|delta| delta.kind.requires_review() || delta.contains_secrets)
        {
            work_view.sync_state = WorkViewSyncState::Attention;
            work_view.attention =
                vec!["Work view has changes that need review before overlay sync.".into()];
            work_view.updated_at = options.generated_at.clone();
            store.upsert_work_view(&work_view)?;
            report.attention += 1;
            continue;
        }

        let payload_bytes = overlay_payload_bytes(&work_view, &deltas)?;
        let overlay_digest = format!("b3_{}", blake3::hash(&payload_bytes).to_hex());
        let overlay_pack = derive_overlay_payload_pack(
            &workspace.id,
            &payload_bytes,
            options.storage_key,
            options.key_epoch,
        )?;

        let remote_record = match remote_views
            .iter()
            .find(|remote| remote.work_view_id == work_view.id.as_str())
            .cloned()
        {
            Some(record) => record,
            None => {
                let base_workspace_version =
                    if workspace_ref.snapshot_id == work_view.base_snapshot_id.as_str() {
                        workspace_ref.version
                    } else {
                        0
                    };
                let created = control_plane.create_work_view(WorkViewCreate {
                    workspace_id: workspace.id.as_str().to_string(),
                    work_view_id: work_view.id.as_str().to_string(),
                    project_id: work_view.project_id.as_str().to_string(),
                    name: work_view.id.as_str().to_string(),
                    visible_path: format!(".work/{}", work_view.id.as_str()),
                    base_snapshot_id: work_view.base_snapshot_id.as_str().to_string(),
                    base_workspace_version,
                    created_by_device_id: options.device_id.as_str().to_string(),
                })?;
                remote_views.push(created.clone());
                created
            }
        };

        if deltas.is_empty() && work_view.overlay_head == "overlay_empty" {
            if remote_record.overlay_head.is_some() || remote_record.overlay_version > 0 {
                work_view.sync_state = WorkViewSyncState::Attention;
                work_view.attention =
                    vec!["Remote work view overlay changed; review before syncing.".to_string()];
                report.attention += 1;
            } else {
                work_view.sync_state = WorkViewSyncState::Synced;
                work_view.attention.clear();
                work_view.overlay_version = remote_record.overlay_version;
            }
            work_view.updated_at = options.generated_at.clone();
            store.upsert_work_view(&work_view)?;
            continue;
        }

        if remote_record.overlay_version > work_view.overlay_version {
            if remote_record
                .overlay_head
                .as_ref()
                .is_some_and(|remote| overlay_pointer_matches_pack(remote, &overlay_pack))
            {
                work_view.overlay_head = overlay_digest;
                work_view.overlay_version = remote_record.overlay_version;
                work_view.sync_state = WorkViewSyncState::Synced;
                work_view.attention.clear();
                work_view.updated_at = options.generated_at.clone();
                store.upsert_work_view(&work_view)?;
                continue;
            }
            work_view.sync_state = WorkViewSyncState::Attention;
            work_view.attention = vec![format!(
                "Remote work view overlay is at version {}, but this device last synced version {}.",
                remote_record.overlay_version, work_view.overlay_version
            )];
            work_view.updated_at = options.generated_at.clone();
            store.upsert_work_view(&work_view)?;
            report.attention += 1;
            continue;
        }
        if work_view.overlay_head == overlay_digest {
            continue;
        }

        let overlay_object = upload_overlay_payload(
            &workspace.id,
            &options.device_id,
            overlay_pack,
            control_plane,
            byte_store,
            options.key_epoch,
        )?;
        match control_plane.commit_work_view_overlay(WorkViewOverlayCommit {
            workspace_id: workspace.id.as_str().to_string(),
            work_view_id: work_view.id.as_str().to_string(),
            expected_overlay_version: work_view.overlay_version,
            overlay_object: overlay_object.clone(),
            committed_by_device_id: options.device_id.as_str().to_string(),
        }) {
            Ok(updated) => {
                let updated_overlay_version = updated.overlay_version;
                if let Some(remote) = remote_views
                    .iter_mut()
                    .find(|remote| remote.work_view_id == updated.work_view_id)
                {
                    *remote = updated;
                }
                work_view.overlay_head = overlay_digest;
                work_view.overlay_version = updated_overlay_version;
                work_view.sync_state = WorkViewSyncState::Synced;
                work_view.attention.clear();
                work_view.updated_at = options.generated_at.clone();
                store.upsert_work_view(&work_view)?;
                append_work_event(
                    &store,
                    EventName::OverlayChanged,
                    &work_view,
                    &options.generated_at,
                );
                report.uploaded += 1;
            }
            Err(WorkViewUpdateError::StaleOverlayHead(stale)) => {
                if stale
                    .current
                    .overlay_head
                    .as_ref()
                    .is_some_and(|current| current.object_key == overlay_object.object_key)
                {
                    let current_overlay_version = stale.current.overlay_version;
                    if let Some(remote) = remote_views
                        .iter_mut()
                        .find(|remote| remote.work_view_id == stale.current.work_view_id)
                    {
                        *remote = stale.current;
                    }
                    work_view.overlay_head = overlay_digest;
                    work_view.overlay_version = current_overlay_version;
                    work_view.sync_state = WorkViewSyncState::Synced;
                    work_view.attention.clear();
                    work_view.updated_at = options.generated_at.clone();
                    store.upsert_work_view(&work_view)?;
                    report.uploaded += 1;
                    continue;
                }
                control_plane.mark_object_retention_state(ObjectRetentionStateUpdate::new(
                    workspace.id.as_str(),
                    overlay_object.object_key,
                    StorageRetentionState::OrphanCandidate,
                ))?;
                work_view.sync_state = WorkViewSyncState::Attention;
                work_view.attention = vec![format!(
                    "Remote work view overlay is at version {}, not expected version {}.",
                    stale.current.overlay_version, stale.expected_overlay_version
                )];
                work_view.updated_at = options.generated_at.clone();
                store.upsert_work_view(&work_view)?;
                report.attention += 1;
            }
            Err(error) => return Err(error.into()),
        }
    }

    Ok(report)
}

pub(super) fn overlay_deltas_for_upload(
    store: &MetadataStore,
    work_view: &WorkView,
) -> Result<Vec<overlay::OverlayDelta>, WorkViewError> {
    let mut deltas = filesystem_overlay_deltas(store, work_view)?;
    deltas.extend(
        overlay::logged_overlay_deltas(store, work_view)?
            .into_iter()
            .filter(|delta| delta.kind.requires_review()),
    );
    let workspace_root = expand_display_path(
        store
            .current_workspace_root()?
            .ok_or(WorkViewError::MissingWorkspaceRoot)?,
    );
    let work_root = expand_display_path(&work_view.visible_path);
    let mut policy_filtered = Vec::with_capacity(deltas.len());
    for delta in deltas {
        if overlay_delta_is_ignored_by_policy(
            store,
            work_view,
            &workspace_root,
            &work_root,
            &delta,
        )? {
            continue;
        }
        policy_filtered.push(delta);
    }
    let mut deltas = policy_filtered;
    deltas.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then(overlay_delta_kind_name(&left.kind).cmp(overlay_delta_kind_name(&right.kind)))
    });
    deltas.dedup_by(|left, right| left.path == right.path && left.kind == right.kind);
    Ok(deltas)
}

fn overlay_delta_is_ignored_by_policy(
    store: &MetadataStore,
    work_view: &WorkView,
    workspace_root: &Path,
    work_root: &Path,
    delta: &overlay::OverlayDelta,
) -> Result<bool, WorkViewError> {
    let destination_workspace_path = workspace_path_for_project_file(work_view, &delta.path);
    let work_path = work_root.join(&delta.path);
    let source = work_path.exists().then_some(work_path.as_path());
    let policy = clean_accept_policy(
        store,
        workspace_root,
        &work_view.workspace_id,
        &destination_workspace_path,
        source,
    )?;
    if is_ignored_clean_accept_policy(policy.classification, policy.mode) {
        return Ok(true);
    }
    if delta.contains_secrets {
        return Ok(false);
    }
    if is_clean_accept_policy_eligible(policy.classification, policy.mode) {
        return Ok(false);
    }
    Err(WorkViewError::UnsafeWorkViewPath {
        path: normalize_workspace_path(&delta.path.display().to_string()),
        reason: "work view path policy requires review",
    })
}

fn overlay_payload_bytes(
    work_view: &WorkView,
    deltas: &[overlay::OverlayDelta],
) -> Result<Vec<u8>, WorkViewOverlaySyncError> {
    let work_root = expand_display_path(&work_view.visible_path);
    let mut entries = Vec::with_capacity(deltas.len());
    for delta in deltas {
        let file_path = work_root.join(&delta.path);
        let bytes = if matches!(
            delta.kind,
            overlay::OverlayDeltaKind::Create
                | overlay::OverlayDeltaKind::Modify
                | overlay::OverlayDeltaKind::Rename { .. }
        ) {
            Some(fs::read(&file_path).map_err(WorkViewError::from)?)
        } else {
            None
        };
        let content_hash = bytes
            .as_ref()
            .map(|bytes| format!("b3_{}", blake3::hash(bytes).to_hex()));
        entries.push(OverlayPayloadEntry {
            path: normalize_workspace_path(&delta.path.display().to_string()),
            kind: overlay_delta_kind_name(&delta.kind),
            from: overlay_delta_rename_from(&delta.kind),
            contains_secrets: delta.contains_secrets,
            content_hash,
            bytes,
        });
    }
    let payload = OverlayPayload {
        schema_version: 1,
        work_view_id: work_view.id.as_str().to_string(),
        base_snapshot_id: work_view.base_snapshot_id.as_str().to_string(),
        entries,
    };
    Ok(serde_json::to_vec(&payload)?)
}

pub(super) fn overlay_delta_kind_name(kind: &overlay::OverlayDeltaKind) -> &'static str {
    match kind {
        overlay::OverlayDeltaKind::Create => "create",
        overlay::OverlayDeltaKind::Modify => "modify",
        overlay::OverlayDeltaKind::Delete => "delete",
        overlay::OverlayDeltaKind::Rename { .. } => "rename",
        overlay::OverlayDeltaKind::Symlink => "symlink",
        overlay::OverlayDeltaKind::Chmod => "chmod",
        overlay::OverlayDeltaKind::Unsupported { .. } => "unsupported",
    }
}

fn overlay_delta_rename_from(kind: &overlay::OverlayDeltaKind) -> Option<String> {
    match kind {
        overlay::OverlayDeltaKind::Rename { from } => {
            Some(normalize_workspace_path(&from.display().to_string()))
        }
        _ => None,
    }
}

fn derive_overlay_payload_pack(
    workspace_id: &bowline_core::ids::WorkspaceId,
    payload_bytes: &[u8],
    storage_key: StorageKey,
    key_epoch: u32,
) -> Result<PackWriteOutput, WorkViewOverlaySyncError> {
    let payload_content_id =
        ContentId::new(format!("overlay_{}", blake3::hash(payload_bytes).to_hex()));
    let packs = write_source_packs(
        workspace_id.clone(),
        &[PackRecordInput {
            content_id: payload_content_id,
            bytes: payload_bytes.to_vec(),
        }],
        payload_bytes.len().max(1),
        storage_key,
        key_epoch,
    )?;
    let pack = packs
        .into_iter()
        .next()
        .ok_or(WorkViewOverlaySyncError::MissingOverlayPack)?;
    Ok(pack)
}

fn overlay_pointer_matches_pack(pointer: &ObjectPointer, pack: &PackWriteOutput) -> bool {
    pointer.kind == ObjectKind::AgentOverlay
        && overlay_pack_payload_content_id(pack)
            .is_some_and(|content_id| pointer.content_id == content_id)
}

fn overlay_pack_payload_content_id(pack: &PackWriteOutput) -> Option<&str> {
    pack.locators
        .first()
        .map(|locator| locator.content_id.as_str())
}

fn upload_overlay_payload(
    workspace_id: &bowline_core::ids::WorkspaceId,
    device_id: &DeviceId,
    pack: PackWriteOutput,
    control_plane: &dyn ControlPlaneClient,
    byte_store: &dyn ByteStore,
    key_epoch: u32,
) -> Result<ObjectPointer, WorkViewOverlaySyncError> {
    match control_plane.head_object_metadata(workspace_id.as_str(), pack.object_key.as_str()) {
        Ok(metadata) => {
            validate_overlay_object_metadata(&metadata, &pack.object_key, &pack.bytes, key_epoch)?;
            return Ok(ObjectPointer {
                object_key: pack.object_key.as_str().to_string(),
                content_id: overlay_pack_payload_content_id(&pack)
                    .ok_or(WorkViewOverlaySyncError::MissingOverlayPack)?
                    .to_string(),
                byte_len: metadata.byte_len,
                hash: metadata.hash,
                key_epoch: metadata.key_epoch,
                kind: ObjectKind::AgentOverlay,
                created_at: ControlPlaneTimestamp {
                    tick: metadata.created_at_unix_ms,
                },
            });
        }
        Err(ControlPlaneError::ObjectMissing { .. }) => {}
        Err(error) => return Err(error.into()),
    }
    control_plane.create_upload_intent(
        UploadIntentRequest::new(
            workspace_id.as_str(),
            ObjectKind::AgentOverlay,
            pack.bytes.len() as u64,
        )
        .with_object_key(pack.object_key.as_str())
        .with_content_id(
            overlay_pack_payload_content_id(&pack)
                .ok_or(WorkViewOverlaySyncError::MissingOverlayPack)?,
        ),
    )?;

    let metadata = match byte_store.put_object_with_content_id_at_epoch(
        pack.object_key.clone(),
        StorageObjectKind::AgentOverlay,
        overlay_pack_payload_content_id(&pack)
            .ok_or(WorkViewOverlaySyncError::MissingOverlayPack)?,
        &pack.bytes,
        key_epoch,
        Some(device_id),
    ) {
        Ok(metadata) => metadata,
        Err(ByteStoreError::ObjectAlreadyExists(existing_key))
            if existing_key == pack.object_key =>
        {
            byte_store.head_object(&pack.object_key)?
        }
        Err(error) => return Err(error.into()),
    };
    validate_overlay_object_metadata(&metadata, &pack.object_key, &pack.bytes, key_epoch)?;
    let pointer = ObjectPointer {
        object_key: pack.object_key.as_str().to_string(),
        content_id: overlay_pack_payload_content_id(&pack)
            .ok_or(WorkViewOverlaySyncError::MissingOverlayPack)?
            .to_string(),
        byte_len: metadata.byte_len,
        hash: metadata.hash,
        key_epoch: metadata.key_epoch,
        kind: ObjectKind::AgentOverlay,
        created_at: ControlPlaneTimestamp {
            tick: metadata.created_at_unix_ms,
        },
    };
    control_plane.commit_uploaded_object_metadata(ObjectMetadataCommit {
        workspace_id: workspace_id.as_str().to_string(),
        object: pointer.clone(),
        committed_by_device_id: device_id.as_str().to_string(),
    })?;
    Ok(pointer)
}

fn validate_overlay_object_metadata(
    metadata: &bowline_storage::ObjectMetadata,
    object_key: &bowline_storage::ObjectKey,
    bytes: &[u8],
    key_epoch: u32,
) -> Result<(), WorkViewOverlaySyncError> {
    let expected_hash = format!("b3_{}", blake3::hash(bytes).to_hex());
    if metadata.key != *object_key
        || metadata.kind != StorageObjectKind::AgentOverlay
        || metadata.byte_len != bytes.len() as u64
        || metadata.hash != expected_hash
        || metadata.key_epoch != key_epoch
    {
        return Err(ControlPlaneError::Conflict {
            resource: "overlay object metadata",
            reason: "stored overlay object metadata does not match encrypted pack bytes",
        }
        .into());
    }
    Ok(())
}
