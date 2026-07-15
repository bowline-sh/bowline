use bowline_control_plane::{
    ControlPlaneClient, ObjectPointer, StaleWorkViewOverlayHead,
    WorkViewRecord as RemoteWorkViewRecord, WorkViewUpdateError,
};
use bowline_core::{
    events::EventName,
    ids::WorkspaceId,
    work_views::{WorkView, WorkViewSyncState},
};
use bowline_storage::ByteStore;

use crate::metadata::MetadataStore;

use super::{
    WorkViewOverlaySyncError,
    overlay_receive::read_overlay_manifest,
    overlay_retention::{
        manifest_chunk_object_keys, mark_overlay_objects_orphan, persist_overlay_receipt,
        retire_superseded_overlay_chunks,
    },
    overlay_sync::{WorkViewOverlaySyncOptions, WorkViewOverlaySyncReport},
    overlay_upload::BuiltOverlayManifest,
    paths::append_work_event,
};

pub(super) struct OverlayCommitRequest<'a> {
    pub(super) store: &'a MetadataStore,
    pub(super) control_plane: &'a dyn ControlPlaneClient,
    pub(super) byte_store: &'a dyn ByteStore,
    pub(super) options: &'a WorkViewOverlaySyncOptions,
    pub(super) workspace_id: &'a WorkspaceId,
    pub(super) work_view: &'a mut WorkView,
    pub(super) remote_views: &'a mut Vec<RemoteWorkViewRecord>,
    pub(super) overlay_root_id: &'a str,
    pub(super) encoded_overlay: &'a str,
    pub(super) overlay_object: &'a ObjectPointer,
    pub(super) root_was_uploaded: bool,
    pub(super) built: &'a BuiltOverlayManifest,
    pub(super) report: &'a mut WorkViewOverlaySyncReport,
}

pub(super) fn handle_overlay_commit(
    request: OverlayCommitRequest<'_>,
    result: Result<RemoteWorkViewRecord, WorkViewUpdateError>,
) -> Result<(), WorkViewOverlaySyncError> {
    match result {
        Ok(updated) => finish_committed_overlay(request, updated, true),
        Err(WorkViewUpdateError::StaleOverlayHead(stale)) => {
            if stale
                .current
                .overlay_head
                .as_ref()
                .is_some_and(|current| current.object_key == request.overlay_object.object_key)
            {
                return finish_committed_overlay(request, stale.current, false);
            }
            handle_lost_overlay_cas(request, &stale)
        }
        Err(error) => handle_ambiguous_overlay_commit(request, error),
    }
}

fn finish_committed_overlay(
    request: OverlayCommitRequest<'_>,
    updated: RemoteWorkViewRecord,
    append_event: bool,
) -> Result<(), WorkViewOverlaySyncError> {
    let updated_overlay_version = updated.overlay_version;
    let prior_manifest = request
        .remote_views
        .iter()
        .find(|remote| remote.work_view_id == updated.work_view_id)
        .and_then(|remote| remote.overlay_head.as_ref())
        .map(|pointer| {
            read_overlay_manifest(
                request.byte_store,
                request.options,
                request.work_view,
                pointer,
            )
        })
        .transpose()?;
    if let Some(remote) = request
        .remote_views
        .iter_mut()
        .find(|remote| remote.work_view_id == updated.work_view_id)
    {
        *remote = updated;
    }
    request.work_view.overlay_head = request.overlay_root_id.to_string();
    request.work_view.overlay_version = updated_overlay_version;
    request.work_view.sync_state = WorkViewSyncState::Synced;
    request.work_view.attention.clear();
    request.work_view.updated_at = request.options.generated_at.clone();
    retire_superseded_overlay_chunks(
        request.control_plane,
        request.work_view,
        prior_manifest.as_ref(),
        &request.built.manifest,
    )?;
    persist_overlay_receipt(
        request.store,
        request.work_view,
        request.overlay_root_id,
        request.encoded_overlay,
    )?;
    if append_event {
        append_work_event(
            request.store,
            EventName::OverlayChanged,
            request.work_view,
            &request.options.generated_at,
        );
    }
    request.report.uploaded += 1;
    Ok(())
}

fn handle_lost_overlay_cas(
    request: OverlayCommitRequest<'_>,
    stale: &StaleWorkViewOverlayHead,
) -> Result<(), WorkViewOverlaySyncError> {
    let winning_manifest = stale
        .current
        .overlay_head
        .as_ref()
        .map(|pointer| {
            read_overlay_manifest(
                request.byte_store,
                request.options,
                request.work_view,
                pointer,
            )
        })
        .transpose()?;
    let winning_chunks = winning_manifest
        .as_ref()
        .map(manifest_chunk_object_keys)
        .unwrap_or_default();
    let unreferenced_chunks = request
        .built
        .uploaded_chunk_object_keys
        .iter()
        .filter(|key| !winning_chunks.contains(key.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    mark_overlay_objects_orphan(
        request.control_plane,
        request.workspace_id.as_str(),
        request
            .root_was_uploaded
            .then_some(request.overlay_object.object_key.as_str()),
        &unreferenced_chunks,
    )?;
    request.work_view.sync_state = WorkViewSyncState::Attention;
    request.work_view.attention = vec![format!(
        "Remote work view overlay is at version {}, not expected version {}.",
        stale.current.overlay_version, stale.expected_overlay_version
    )];
    request.work_view.updated_at = request.options.generated_at.clone();
    request.store.upsert_work_view(request.work_view)?;
    request.report.attention += 1;
    Ok(())
}

fn handle_ambiguous_overlay_commit(
    request: OverlayCommitRequest<'_>,
    error: WorkViewUpdateError,
) -> Result<(), WorkViewOverlaySyncError> {
    let reconciled = match committed_overlay_after_error(&request) {
        Ok(reconciled) => reconciled,
        Err(_) => return Err(error.into()),
    };
    if let Some(current) = reconciled {
        return finish_committed_overlay(request, current, false);
    }
    if let Err(cleanup) = mark_overlay_objects_orphan(
        request.control_plane,
        request.workspace_id.as_str(),
        request
            .root_was_uploaded
            .then_some(request.overlay_object.object_key.as_str()),
        &request.built.uploaded_chunk_object_keys,
    ) {
        return Err(WorkViewOverlaySyncError::CommitCleanup {
            commit: error,
            cleanup: Box::new(cleanup),
        });
    }
    Err(error.into())
}

fn committed_overlay_after_error(
    request: &OverlayCommitRequest<'_>,
) -> Result<Option<RemoteWorkViewRecord>, bowline_control_plane::ControlPlaneError> {
    Ok(request
        .control_plane
        .list_work_views(request.workspace_id, true)?
        .into_iter()
        .find(|remote| {
            remote.work_view_id == request.work_view.id.as_str()
                && remote
                    .overlay_head
                    .as_ref()
                    .is_some_and(|head| head.object_key == request.overlay_object.object_key)
        }))
}
