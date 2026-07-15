use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bowline_control_plane::{
    ControlPlaneClient, WorkViewCreate, WorkViewOverlayCommit,
    WorkViewRecord as RemoteWorkViewRecord, WorkspaceRef,
};
use bowline_core::{
    ids::{ContentId, DeviceId, ProjectId, SnapshotId, WorkViewId, WorkspaceId},
    work_views::{OVERLAY_HEAD_EMPTY, WorkView, WorkViewLifecycle, WorkViewSyncState},
    workspace_graph::{normalize_workspace_path, workspace_content_id},
};
use bowline_storage::{ByteStore, StorageKey};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::metadata::MetadataStore;

#[cfg(test)]
pub(super) use super::overlay_delta_operation::overlay_delta_kind_name;
pub(super) use super::overlay_delta_operation::{overlay_delta_rename_from, overlay_operation};

use super::{
    WorkViewError, WorkViewOverlaySyncError, overlay,
    overlay_commit::{OverlayCommitRequest, handle_overlay_commit},
    overlay_objects::{
        derive_overlay_payload_pack, overlay_pointer_matches_pack,
        upload_overlay_payload_with_checkpoint,
    },
    overlay_receive::{
        RemoteOverlayMaterializationRequest, materialize_remote_overlay,
        overlay_manifest_matches_local_with_checkpoint, read_overlay_manifest_with_checkpoint,
    },
    overlay_retention::{
        fail_unpublished_overlay, fail_unpublished_overlay_objects, persist_overlay_receipt,
        retire_superseded_overlay_chunks, stored_overlay_manifest,
    },
    overlay_upload::{BuiltOverlayManifest, build_overlay_manifest_with_checkpoint},
    overlay_wire::OverlayManifest,
    paths::{
        clean_accept_policy, expand_display_path, is_clean_accept_policy_eligible,
        is_ignored_clean_accept_policy, workspace_path_for_project_file,
    },
};

#[cfg(test)]
use super::overlay_objects::overlay_pack_payload_content_id;

pub struct WorkViewOverlaySyncOptions {
    pub db_path: PathBuf,
    pub device_id: DeviceId,
    pub workspace_content_key: [u8; 32],
    pub storage_key: StorageKey,
    pub key_epoch: u32,
    pub generated_at: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkViewOverlaySyncReport {
    pub uploaded: usize,
    pub attention: usize,
    pub entries_total: usize,
    pub entries_completed: usize,
    pub content_objects_uploaded: usize,
    pub content_objects_reused: usize,
    pub plaintext_bytes: u64,
    pub uploaded_bytes: u64,
}

impl WorkViewOverlaySyncReport {
    fn record_manifest_build(&mut self, built: &BuiltOverlayManifest) {
        self.entries_completed = self
            .entries_completed
            .saturating_add(built.entries_completed);
        self.content_objects_uploaded = self
            .content_objects_uploaded
            .saturating_add(built.content_objects_uploaded);
        self.content_objects_reused = self
            .content_objects_reused
            .saturating_add(built.content_objects_reused);
        self.plaintext_bytes = self.plaintext_bytes.saturating_add(built.plaintext_bytes);
        self.uploaded_bytes = self.uploaded_bytes.saturating_add(built.uploaded_bytes);
    }
}

#[derive(Debug, Clone)]
pub(super) struct OverlayUploadPlan {
    pub(super) deltas: Vec<overlay::OverlayDelta>,
    pub(super) staged_content_by_write_id: BTreeMap<String, ContentId>,
}

fn collect_pending_overlays(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    options: &WorkViewOverlaySyncOptions,
    report: &mut WorkViewOverlaySyncReport,
    checkpoint: &mut dyn FnMut() -> Result<(), WorkViewOverlaySyncError>,
) -> Result<Vec<(WorkView, OverlayUploadPlan)>, WorkViewOverlaySyncError> {
    let mut pending = Vec::new();
    for work_view in store.work_views(workspace_id, true, None)? {
        checkpoint()?;
        if work_view.lifecycle != WorkViewLifecycle::Active
            || matches!(
                work_view.sync_state,
                WorkViewSyncState::Attention | WorkViewSyncState::Conflicted
            )
        {
            continue;
        }
        let upload_plan =
            match overlay_deltas_for_upload_with_checkpoint(store, &work_view, checkpoint) {
                Ok(upload_plan) => upload_plan,
                Err(
                    error @ (WorkViewOverlaySyncError::CancellationRequested
                    | WorkViewOverlaySyncError::ClaimOwnershipLost),
                ) => return Err(error),
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
        if upload_plan.deltas.is_empty()
            && work_view.sync_state != WorkViewSyncState::LocalOnly
            && !work_view.has_overlay()
        {
            continue;
        }
        pending.push((work_view, upload_plan));
    }
    Ok(pending)
}

struct RemoteReconcileRequest<'a> {
    store: &'a MetadataStore,
    control_plane: &'a dyn ControlPlaneClient,
    byte_store: &'a dyn ByteStore,
    options: &'a WorkViewOverlaySyncOptions,
    work_view: &'a mut WorkView,
    upload_plan: &'a OverlayUploadPlan,
    remote: &'a RemoteWorkViewRecord,
    report: &'a mut WorkViewOverlaySyncReport,
    checkpoint: &'a mut dyn FnMut() -> Result<(), WorkViewOverlaySyncError>,
}

fn reconcile_remote_overlay(
    request: RemoteReconcileRequest<'_>,
) -> Result<bool, WorkViewOverlaySyncError> {
    let RemoteReconcileRequest {
        store,
        control_plane,
        byte_store,
        options,
        work_view,
        upload_plan,
        remote,
        report,
        checkpoint,
    } = request;
    if remote.overlay_version > work_view.overlay_version {
        let prior_materialization_is_clean = if upload_plan.deltas.is_empty() {
            true
        } else {
            let prior_manifest = stored_overlay_manifest(store, work_view)?;
            match prior_manifest {
                Some(manifest) => overlay_manifest_matches_local_with_checkpoint(
                    &manifest,
                    options,
                    work_view,
                    upload_plan,
                    checkpoint,
                )?,
                None => false,
            }
        };
        if prior_materialization_is_clean && let Some(pointer) = remote.overlay_head.as_ref() {
            let completed = match materialize_remote_overlay(
                RemoteOverlayMaterializationRequest {
                    store,
                    control_plane,
                    byte_store,
                    options,
                    work_view,
                    pointer,
                    remote_overlay_version: remote.overlay_version,
                },
                checkpoint,
            ) {
                Ok(completed) => completed,
                Err(error) if overlay_failure_requires_attention(&error) => {
                    work_view.sync_state = WorkViewSyncState::Attention;
                    work_view.attention = vec![
                        "Remote overlay failed integrity or policy validation; the last verified materialization was preserved."
                            .to_string(),
                    ];
                    work_view.updated_at = options.generated_at.clone();
                    store.upsert_work_view(work_view)?;
                    report.attention += 1;
                    return Ok(true);
                }
                Err(error) => return Err(error),
            };
            report.entries_total = report.entries_total.saturating_add(completed);
            report.entries_completed = report.entries_completed.saturating_add(completed);
            return Ok(true);
        }
        if let Some(pointer) = remote.overlay_head.as_ref() {
            checkpoint()?;
            let manifest = read_overlay_manifest_with_checkpoint(
                byte_store, options, work_view, pointer, checkpoint,
            )?;
            if overlay_manifest_matches_local_with_checkpoint(
                &manifest,
                options,
                work_view,
                upload_plan,
                checkpoint,
            )? {
                checkpoint()?;
                persist_matching_remote_overlay(MatchingRemoteOverlayRequest {
                    store,
                    control_plane,
                    options,
                    work_view,
                    pointer,
                    overlay_version: remote.overlay_version,
                    manifest: &manifest,
                })?;
                return Ok(true);
            }
        }
        work_view.sync_state = WorkViewSyncState::Attention;
        work_view.attention = vec![format!(
            "Remote work view overlay is at version {}, but this device last synced version {}.",
            remote.overlay_version, work_view.overlay_version
        )];
        work_view.updated_at = options.generated_at.clone();
        store.upsert_work_view(work_view)?;
        report.attention += 1;
        return Ok(true);
    }
    if remote.overlay_version == work_view.overlay_version
        && let Some(pointer) = remote
            .overlay_head
            .as_ref()
            .filter(|pointer| pointer.content_id.as_str() == work_view.overlay_head)
    {
        checkpoint()?;
        let manifest = read_overlay_manifest_with_checkpoint(
            byte_store, options, work_view, pointer, checkpoint,
        )?;
        if overlay_manifest_matches_local_with_checkpoint(
            &manifest,
            options,
            work_view,
            upload_plan,
            checkpoint,
        )? {
            checkpoint()?;
            persist_matching_remote_overlay(MatchingRemoteOverlayRequest {
                store,
                control_plane,
                options,
                work_view,
                pointer,
                overlay_version: remote.overlay_version,
                manifest: &manifest,
            })?;
            return Ok(true);
        }
    }
    Ok(false)
}

struct MatchingRemoteOverlayRequest<'a> {
    store: &'a MetadataStore,
    control_plane: &'a dyn ControlPlaneClient,
    options: &'a WorkViewOverlaySyncOptions,
    work_view: &'a mut WorkView,
    pointer: &'a bowline_control_plane::ObjectPointer,
    overlay_version: u64,
    manifest: &'a OverlayManifest,
}

fn persist_matching_remote_overlay(
    request: MatchingRemoteOverlayRequest<'_>,
) -> Result<(), WorkViewOverlaySyncError> {
    let prior_manifest = stored_overlay_manifest(request.store, request.work_view)?;
    request.work_view.overlay_head = request.pointer.content_id.as_str().to_string();
    request.work_view.overlay_version = request.overlay_version;
    request.work_view.sync_state = WorkViewSyncState::Synced;
    request.work_view.attention.clear();
    request.work_view.updated_at = request.options.generated_at.clone();
    retire_superseded_overlay_chunks(
        request.control_plane,
        request.work_view,
        prior_manifest.as_ref(),
        request.manifest,
    )?;
    persist_overlay_receipt(
        request.store,
        request.work_view,
        request.pointer.content_id.as_str(),
        &serde_json::to_string(request.manifest)?,
    )
}

fn overlay_failure_requires_attention(error: &WorkViewOverlaySyncError) -> bool {
    match error {
        WorkViewOverlaySyncError::Wire(_)
        | WorkViewOverlaySyncError::MissingOverlayPack
        | WorkViewOverlaySyncError::MissingStagedContent
        | WorkViewOverlaySyncError::ControlPlane(
            bowline_control_plane::ControlPlaneError::Conflict { .. }
            | bowline_control_plane::ControlPlaneError::InvalidObjectKey { .. },
        )
        | WorkViewOverlaySyncError::WorkView(WorkViewError::UnsafeWorkViewPath { .. }) => true,
        WorkViewOverlaySyncError::ByteStore(
            bowline_storage::ByteStoreError::CorruptObject { .. }
            | bowline_storage::ByteStoreError::CorruptJournal { .. }
            | bowline_storage::ByteStoreError::RangeOutOfBounds { .. }
            | bowline_storage::ByteStoreError::InvalidObjectKey { .. },
        ) => true,
        WorkViewOverlaySyncError::Cache(error) => !matches!(
            error,
            bowline_storage::CacheError::Io(_)
                | bowline_storage::CacheError::Store(
                    bowline_storage::ByteStoreError::Io(_)
                        | bowline_storage::ByteStoreError::Network { .. }
                        | bowline_storage::ByteStoreError::HttpStatus { .. }
                        | bowline_storage::ByteStoreError::IntentFailed { .. }
                        | bowline_storage::ByteStoreError::MissingObject { .. }
                )
        ),
        _ => false,
    }
}

pub fn sync_local_work_view_overlays(
    options: WorkViewOverlaySyncOptions,
    control_plane: &dyn ControlPlaneClient,
    byte_store: &dyn ByteStore,
    workspace_ref: &WorkspaceRef,
) -> Result<WorkViewOverlaySyncReport, WorkViewOverlaySyncError> {
    sync_local_work_view_overlays_with_checkpoint(
        options,
        control_plane,
        byte_store,
        workspace_ref,
        || Ok(()),
    )
}

pub fn sync_local_work_view_overlays_with_checkpoint(
    options: WorkViewOverlaySyncOptions,
    control_plane: &dyn ControlPlaneClient,
    byte_store: &dyn ByteStore,
    workspace_ref: &WorkspaceRef,
    mut checkpoint: impl FnMut() -> Result<(), WorkViewOverlaySyncError>,
) -> Result<WorkViewOverlaySyncReport, WorkViewOverlaySyncError> {
    checkpoint()?;
    let (store, workspace_id) = open_overlay_sync_store(&options)?;
    let mut report = WorkViewOverlaySyncReport::default();
    checkpoint()?;
    let pending = collect_pending_overlays(
        &store,
        &workspace_id,
        &options,
        &mut report,
        &mut checkpoint,
    )?;
    if pending.is_empty() {
        return Ok(report);
    }

    checkpoint()?;
    let mut remote_views = control_plane.list_work_views(&workspace_id, true)?;
    for (mut work_view, upload_plan) in pending {
        checkpoint()?;
        if upload_plan
            .deltas
            .iter()
            .any(|delta| delta.kind.requires_review())
        {
            work_view.sync_state = WorkViewSyncState::Attention;
            work_view.attention =
                vec!["Work view has changes that need review before overlay sync.".into()];
            work_view.updated_at = options.generated_at.clone();
            store.upsert_work_view(&work_view)?;
            report.attention += 1;
            continue;
        }

        checkpoint()?;
        if let Some(remote) = remote_views
            .iter()
            .find(|remote| remote.work_view_id == work_view.id.as_str())
            && reconcile_remote_overlay(RemoteReconcileRequest {
                store: &store,
                control_plane,
                byte_store,
                options: &options,
                work_view: &mut work_view,
                upload_plan: &upload_plan,
                remote,
                report: &mut report,
                checkpoint: &mut checkpoint,
            })?
        {
            continue;
        }

        report.entries_total = report
            .entries_total
            .saturating_add(upload_plan.deltas.len());
        checkpoint()?;
        let built = build_overlay_manifest_with_checkpoint(
            &store,
            control_plane,
            byte_store,
            &options,
            &work_view,
            &upload_plan,
            &mut checkpoint,
        )?;
        report.record_manifest_build(&built);
        let payload_bytes = match built.manifest.encode() {
            Ok(bytes) => bytes,
            Err(error) => {
                return fail_unpublished_overlay(
                    control_plane,
                    workspace_id.as_str(),
                    &built.uploaded_chunk_object_keys,
                    error.into(),
                );
            }
        };
        let encoded_overlay = match serde_json::to_string(&built.manifest) {
            Ok(json) => json,
            Err(error) => {
                return fail_unpublished_overlay(
                    control_plane,
                    workspace_id.as_str(),
                    &built.uploaded_chunk_object_keys,
                    error.into(),
                );
            }
        };
        let overlay_root_id = workspace_content_id(options.workspace_content_key, &payload_bytes)
            .as_str()
            .to_string();
        let overlay_pack = match derive_overlay_payload_pack(
            &workspace_id,
            &payload_bytes,
            workspace_content_id(options.workspace_content_key, &payload_bytes),
            options.storage_key,
            options.key_epoch,
        ) {
            Ok(pack) => pack,
            Err(error) => {
                return fail_unpublished_overlay(
                    control_plane,
                    workspace_id.as_str(),
                    &built.uploaded_chunk_object_keys,
                    error,
                );
            }
        };

        checkpoint()?;
        let remote_record = match remote_views
            .iter()
            .find(|remote| remote.work_view_id == work_view.id.as_str())
            .cloned()
        {
            Some(record) => record,
            None => {
                checkpoint()?;
                let base_workspace_version =
                    if workspace_ref.snapshot_id == work_view.base_snapshot_id.as_str() {
                        workspace_ref.version
                    } else {
                        0
                    };
                let created = match control_plane.create_work_view(WorkViewCreate {
                    workspace_id: workspace_id.clone(),
                    work_view_id: WorkViewId::new(work_view.id.as_str()),
                    project_id: ProjectId::new(work_view.project_id.as_str()),
                    name: work_view.id.as_str().to_string(),
                    visible_path: format!(".work/{}", work_view.id.as_str()),
                    base_snapshot_id: SnapshotId::new(work_view.base_snapshot_id.as_str()),
                    base_workspace_version,
                    expires_at: None,
                    retain_until: None,
                    created_by_device_id: options.device_id.clone(),
                }) {
                    Ok(created) => created,
                    Err(error) => {
                        return fail_unpublished_overlay(
                            control_plane,
                            workspace_id.as_str(),
                            &built.uploaded_chunk_object_keys,
                            error.into(),
                        );
                    }
                };
                remote_views.push(created.clone());
                created
            }
        };

        if upload_plan.deltas.is_empty() && work_view.overlay_head == OVERLAY_HEAD_EMPTY {
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
                work_view.overlay_head = overlay_root_id.clone();
                work_view.overlay_version = remote_record.overlay_version;
                work_view.sync_state = WorkViewSyncState::Synced;
                work_view.attention.clear();
                work_view.updated_at = options.generated_at.clone();
                persist_overlay_receipt(&store, &work_view, &overlay_root_id, &encoded_overlay)?;
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
        if work_view.overlay_head == overlay_root_id {
            persist_overlay_receipt(&store, &work_view, &overlay_root_id, &encoded_overlay)?;
            continue;
        }

        checkpoint()?;
        let uploaded_overlay = match upload_overlay_payload_with_checkpoint(
            &workspace_id,
            &options.device_id,
            overlay_pack,
            control_plane,
            byte_store,
            options.key_epoch,
            &mut checkpoint,
        ) {
            Ok(uploaded) => uploaded,
            Err(error) => {
                return fail_unpublished_overlay(
                    control_plane,
                    workspace_id.as_str(),
                    &built.uploaded_chunk_object_keys,
                    error,
                );
            }
        };
        if let Err(error) = checkpoint() {
            return fail_unpublished_overlay_objects(
                control_plane,
                workspace_id.as_str(),
                (!uploaded_overlay.reused).then_some(uploaded_overlay.pointer.object_key.as_str()),
                &built.uploaded_chunk_object_keys,
                error,
            );
        }
        let overlay_object = uploaded_overlay.pointer.clone();
        let commit_result = control_plane.commit_work_view_overlay(WorkViewOverlayCommit {
            workspace_id: workspace_id.clone(),
            work_view_id: WorkViewId::new(work_view.id.as_str()),
            expected_overlay_version: work_view.overlay_version,
            overlay_object: overlay_object.clone(),
            committed_by_device_id: options.device_id.clone(),
        });
        handle_overlay_commit(
            OverlayCommitRequest {
                store: &store,
                control_plane,
                byte_store,
                options: &options,
                workspace_id: &workspace_id,
                work_view: &mut work_view,
                remote_views: &mut remote_views,
                overlay_root_id: &overlay_root_id,
                encoded_overlay: &encoded_overlay,
                overlay_object: &overlay_object,
                root_was_uploaded: !uploaded_overlay.reused,
                built: &built,
                report: &mut report,
            },
            commit_result,
        )?;
    }

    Ok(report)
}

fn open_overlay_sync_store(
    options: &WorkViewOverlaySyncOptions,
) -> Result<(MetadataStore, WorkspaceId), WorkViewOverlaySyncError> {
    let store = MetadataStore::open(&options.db_path)?;
    let workspace_id = store
        .current_workspace()?
        .ok_or(WorkViewError::MissingWorkspace)?
        .id;
    Ok((store, workspace_id))
}

#[cfg(test)]
pub(super) fn overlay_deltas_for_upload(
    store: &MetadataStore,
    work_view: &WorkView,
) -> Result<OverlayUploadPlan, WorkViewError> {
    overlay_deltas_for_upload_with_bool_checkpoint(store, work_view, &mut || true)
}

fn overlay_deltas_for_upload_with_checkpoint(
    store: &MetadataStore,
    work_view: &WorkView,
    checkpoint: &mut dyn FnMut() -> Result<(), WorkViewOverlaySyncError>,
) -> Result<OverlayUploadPlan, WorkViewOverlaySyncError> {
    checkpoint()?;
    let mut checkpoint_error = None;
    let result = {
        let mut bool_checkpoint = || match checkpoint() {
            Ok(()) => true,
            Err(error) => {
                checkpoint_error = Some(error);
                false
            }
        };
        overlay_deltas_for_upload_with_bool_checkpoint(store, work_view, &mut bool_checkpoint)
    };
    if let Some(error) = checkpoint_error {
        return Err(error);
    }
    result.map_err(WorkViewOverlaySyncError::from)
}

fn overlay_deltas_for_upload_with_bool_checkpoint(
    store: &MetadataStore,
    work_view: &WorkView,
    checkpoint: &mut dyn FnMut() -> bool,
) -> Result<OverlayUploadPlan, WorkViewError> {
    let work_root = expand_display_path(&work_view.visible_path);
    let mut deltas = overlay::filesystem_overlay_deltas_with_checkpoint(
        store, work_view, &work_root, checkpoint,
    )?;
    let local_writes =
        store.local_writes_for_path_prefix(&work_view.workspace_id, &work_view.visible_path)?;
    let staged_write_created_at = local_writes
        .iter()
        .filter_map(|write| {
            write
                .staged_content_id
                .as_ref()
                .map(|_| (write.id.clone(), write.created_at.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    let staged_content_by_write_id = local_writes
        .into_iter()
        .filter_map(|write| {
            write
                .staged_content_id
                .map(|content_id| (write.id, content_id))
        })
        .collect::<BTreeMap<_, _>>();
    let staged_write_ids = staged_write_created_at
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    deltas.extend(
        overlay::logged_overlay_deltas(store, work_view)?
            .into_iter()
            .filter(|delta| {
                delta.kind.requires_review()
                    || delta
                        .write_id
                        .as_ref()
                        .is_some_and(|id| staged_write_ids.contains(id))
            }),
    );
    let workspace_root = expand_display_path(
        store
            .current_workspace_root()?
            .ok_or(WorkViewError::MissingWorkspaceRoot)?,
    );
    let exposed_base_paths = exposed_base_file_paths(store, work_view)?;
    let mut policy_filtered = Vec::with_capacity(deltas.len());
    for mut delta in deltas {
        super::paths::cancellation_checkpoint(checkpoint)?;
        if overlay_delta_is_ignored_by_policy(
            store,
            work_view,
            &workspace_root,
            &work_root,
            &delta,
        )? {
            continue;
        }
        if matches!(
            &delta.kind,
            overlay::OverlayDeltaKind::Rename { from } if !exposed_base_paths.contains(from)
        ) {
            delta.kind = overlay::OverlayDeltaKind::Create;
        }
        policy_filtered.push(delta);
    }
    let rename_sources = policy_filtered
        .iter()
        .filter_map(|delta| match &delta.kind {
            overlay::OverlayDeltaKind::Rename { from } if !from.as_os_str().is_empty() => {
                Some(from.clone())
            }
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    let mut deduped = BTreeMap::<PathBuf, overlay::OverlayDelta>::new();
    for delta in policy_filtered {
        super::paths::cancellation_checkpoint(checkpoint)?;
        if matches!(delta.kind, overlay::OverlayDeltaKind::Delete)
            && rename_sources.contains(&delta.path)
        {
            continue;
        }
        match deduped.entry(delta.path.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(delta);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let current = entry.get();
                if current.kind.requires_review() {
                    continue;
                }
                if delta.kind.requires_review() {
                    entry.insert(delta);
                    continue;
                }
                if matches!(delta.kind, overlay::OverlayDeltaKind::Rename { .. }) {
                    let mut rename = delta;
                    if !staged_delta_is_current(&rename, &staged_write_created_at, &work_root) {
                        rename.write_id = None;
                    }
                    entry.insert(rename);
                    continue;
                }
                if matches!(current.kind, overlay::OverlayDeltaKind::Rename { .. }) {
                    continue;
                }
                let staged = delta
                    .write_id
                    .as_ref()
                    .is_some_and(|id| staged_write_ids.contains(id));
                if staged && staged_delta_is_current(&delta, &staged_write_created_at, &work_root) {
                    entry.insert(delta);
                }
            }
        }
    }
    Ok(OverlayUploadPlan {
        deltas: deduped.into_values().collect(),
        staged_content_by_write_id,
    })
}

fn exposed_base_file_paths(
    store: &MetadataStore,
    work_view: &WorkView,
) -> Result<std::collections::BTreeSet<PathBuf>, WorkViewError> {
    let descriptor = store
        .work_view_exposed_base(&work_view.workspace_id, &work_view.id)?
        .ok_or_else(|| WorkViewError::SnapshotMaterialization {
            snapshot_id: work_view.base_snapshot_id.as_str().to_string(),
            reason: "authoritative exposed base is missing".to_string(),
        })?;
    let project_prefix = descriptor.project_prefix.trim_end_matches('/');
    let exposed = super::namespace::load_exposed_snapshot(store, &descriptor)?;
    let entries = super::namespace::collect_prefix(
        &exposed,
        &bowline_core::workspace_graph::WorkspaceRelativePath::new(project_prefix),
    )?;
    Ok(entries
        .into_iter()
        .filter(|entry| entry.kind == bowline_core::workspace_graph::NamespaceEntryKind::File)
        .filter_map(|entry| {
            let relative = entry
                .path
                .strip_prefix(project_prefix)?
                .trim_start_matches('/');
            (!relative.is_empty()).then(|| PathBuf::from(relative))
        })
        .collect())
}

fn staged_delta_is_current(
    delta: &overlay::OverlayDelta,
    staged_write_created_at: &BTreeMap<String, String>,
    work_root: &Path,
) -> bool {
    let Some(write_id) = delta.write_id.as_ref() else {
        return false;
    };
    let Some(write_time) = staged_write_created_at
        .get(write_id)
        .and_then(|created_at| rfc3339_system_time(created_at))
    else {
        return true;
    };
    let Ok(modified) =
        fs::metadata(work_root.join(&delta.path)).and_then(|metadata| metadata.modified())
    else {
        return true;
    };
    modified <= write_time
}

fn rfc3339_system_time(value: &str) -> Option<SystemTime> {
    let parsed = OffsetDateTime::parse(value, &Rfc3339).ok()?;
    let seconds = parsed.unix_timestamp();
    if seconds < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::new(seconds as u64, parsed.nanosecond()))
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

#[cfg(test)]
#[path = "overlay_sync_tests.rs"]
mod tests;
