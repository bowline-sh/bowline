use bowline_control_plane::{ControlPlaneClient, ObjectRetentionStateUpdate};
use bowline_core::work_views::WorkView;
use bowline_storage::RetentionState as StorageRetentionState;

use crate::metadata::MetadataStore;

use super::{WorkViewOverlaySyncError, overlay_wire::OverlayManifest};

pub(super) fn fail_unpublished_overlay<T>(
    control_plane: &dyn ControlPlaneClient,
    workspace_id: &str,
    chunk_object_keys: &[String],
    publication: WorkViewOverlaySyncError,
) -> Result<T, WorkViewOverlaySyncError> {
    fail_unpublished_overlay_objects(
        control_plane,
        workspace_id,
        None,
        chunk_object_keys,
        publication,
    )
}

pub(super) fn fail_unpublished_overlay_objects<T>(
    control_plane: &dyn ControlPlaneClient,
    workspace_id: &str,
    root_object_key: Option<&str>,
    chunk_object_keys: &[String],
    publication: WorkViewOverlaySyncError,
) -> Result<T, WorkViewOverlaySyncError> {
    match mark_overlay_objects_orphan(
        control_plane,
        workspace_id,
        root_object_key,
        chunk_object_keys,
    ) {
        Ok(()) => Err(publication),
        Err(cleanup) => Err(WorkViewOverlaySyncError::PublicationCleanup {
            publication: Box::new(publication),
            cleanup: Box::new(cleanup),
        }),
    }
}

pub(super) fn retire_superseded_overlay_chunks(
    control_plane: &dyn ControlPlaneClient,
    work_view: &WorkView,
    prior_manifest: Option<&OverlayManifest>,
    incoming_manifest: &OverlayManifest,
) -> Result<(), WorkViewOverlaySyncError> {
    let Some(prior_manifest) = prior_manifest else {
        return Ok(());
    };
    let incoming_chunks = manifest_chunk_object_keys(incoming_manifest);
    let retired = manifest_chunk_object_keys(prior_manifest)
        .difference(&incoming_chunks)
        .cloned()
        .collect::<Vec<_>>();
    mark_overlay_objects_orphan(
        control_plane,
        work_view.workspace_id.as_str(),
        None,
        &retired,
    )
}

pub(super) fn stored_overlay_manifest(
    store: &MetadataStore,
    work_view: &WorkView,
) -> Result<Option<OverlayManifest>, WorkViewOverlaySyncError> {
    store
        .materialized_overlay_receipt(&work_view.workspace_id, &work_view.id)?
        .map(|(_, encoded)| OverlayManifest::decode(encoded.as_bytes()))
        .transpose()
        .map_err(Into::into)
}

pub(super) fn manifest_chunk_object_keys(
    manifest: &OverlayManifest,
) -> std::collections::BTreeSet<String> {
    manifest
        .operations()
        .iter()
        .filter_map(|entry| entry.content.as_ref())
        .flat_map(|content| content.chunks.iter())
        .map(|chunk| chunk.object_key.clone())
        .collect()
}

pub(super) fn persist_overlay_receipt(
    store: &MetadataStore,
    work_view: &WorkView,
    overlay_root_id: &str,
    encoded_overlay: &str,
) -> Result<(), WorkViewOverlaySyncError> {
    store.commit_materialized_overlay(work_view, overlay_root_id, encoded_overlay)?;
    Ok(())
}

pub(super) fn mark_overlay_objects_orphan(
    control_plane: &dyn ControlPlaneClient,
    workspace_id: &str,
    root_object_key: Option<&str>,
    chunk_object_keys: &[String],
) -> Result<(), WorkViewOverlaySyncError> {
    for object_key in root_object_key
        .into_iter()
        .chain(chunk_object_keys.iter().map(String::as_str))
    {
        control_plane.mark_object_retention_state(ObjectRetentionStateUpdate::new(
            workspace_id,
            object_key,
            StorageRetentionState::OrphanCandidate,
        ))?;
    }
    Ok(())
}
