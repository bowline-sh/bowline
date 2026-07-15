use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use bowline_control_plane::{ControlPlaneClient, ControlPlaneError, ObjectKind, ObjectPointer};
use bowline_core::{
    ids::ContentId,
    work_views::WorkView,
    workspace_graph::{
        ContentLocator, ContentStorage, FileExecutability, NamespaceEntry, NamespaceEntryKind,
        normalize_workspace_path,
    },
};
use bowline_storage::{
    ByteStore, LocalContentCache, ObjectKey, ObjectKind as StorageObjectKind,
    RangeHydrationRequest, parse_index,
};

use crate::metadata::MetadataStore;

use super::{
    WorkViewError, WorkViewOverlaySyncError,
    content_identity::{
        open_stable_regular_file, verified_content_matches_path, verify_stable_regular_file,
    },
    overlay_preserve::{PreserveLocalOnlyRequest, preserve_local_only_files},
    overlay_publish::{
        overlay_staging_root, publish_overlay_tree, recover_overlay_publish,
        rollback_overlay_publish,
    },
    overlay_retention::retire_superseded_overlay_chunks,
    overlay_sync::{
        OverlayUploadPlan, WorkViewOverlaySyncOptions, overlay_delta_rename_from, overlay_operation,
    },
    overlay_validate::validate_incoming_overlay,
    overlay_wire::{OverlayContent, OverlayManifest, OverlayOperation},
    paths::{expand_display_path, is_owner_only_work_view_policy},
    pending_materialization::{cleanup_pending, create_pending_destination},
};

#[cfg(test)]
use super::overlay_validate::validate_overlay_namespace;

pub(super) struct RemoteOverlayMaterializationRequest<'a> {
    pub(super) store: &'a MetadataStore,
    pub(super) control_plane: &'a dyn ControlPlaneClient,
    pub(super) byte_store: &'a dyn ByteStore,
    pub(super) options: &'a WorkViewOverlaySyncOptions,
    pub(super) work_view: &'a mut WorkView,
    pub(super) pointer: &'a ObjectPointer,
    pub(super) remote_overlay_version: u64,
}

pub(super) fn materialize_remote_overlay(
    request: RemoteOverlayMaterializationRequest<'_>,
    checkpoint: &mut dyn FnMut() -> Result<(), WorkViewOverlaySyncError>,
) -> Result<usize, WorkViewOverlaySyncError> {
    let RemoteOverlayMaterializationRequest {
        store,
        control_plane,
        byte_store,
        options,
        work_view,
        pointer,
        remote_overlay_version,
    } = request;
    checkpoint()?;
    if pointer.kind != ObjectKind::AgentOverlay {
        return Err(ControlPlaneError::Conflict {
            resource: "work-view overlay",
            reason: "overlay root pointer has the wrong object kind",
        }
        .into());
    }
    let manifest =
        read_overlay_manifest_with_checkpoint(byte_store, options, work_view, pointer, checkpoint)?;
    checkpoint()?;
    let descriptor = store
        .work_view_exposed_base(&work_view.workspace_id, &work_view.id)?
        .ok_or_else(|| WorkViewError::SnapshotMaterialization {
            snapshot_id: work_view.base_snapshot_id.as_str().to_string(),
            reason: "authoritative exposed base is missing".to_string(),
        })?;
    let project_prefix = descriptor.project_prefix.trim_matches('/');
    let exposed_snapshot = super::namespace::load_exposed_snapshot(store, &descriptor)?;
    let exposed = super::namespace::collect_prefix(
        &exposed_snapshot,
        &bowline_core::workspace_graph::WorkspaceRelativePath::new(project_prefix),
    )?;
    let exposed_paths = exposed
        .iter()
        .map(|entry| entry.path.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let work_root = expand_display_path(&work_view.visible_path);
    let workspace_root = store
        .current_workspace_root()?
        .map(expand_display_path)
        .ok_or(WorkViewError::MissingWorkspace)?;
    let owner_only_by_path = validate_incoming_overlay(
        &workspace_root,
        work_view,
        project_prefix,
        &exposed,
        &manifest,
    )?;
    checkpoint()?;
    let receipt = store.materialized_overlay_receipt(&work_view.workspace_id, &work_view.id)?;
    let receipt_is_committed = receipt
        .as_ref()
        .is_some_and(|(root_id, _)| root_id == pointer.content_id.as_str());
    let prior_manifest = receipt
        .as_ref()
        .filter(|(root_id, _)| root_id == &work_view.overlay_head)
        .and_then(|(_, encoded)| OverlayManifest::decode(encoded.as_bytes()).ok());
    recover_overlay_publish(
        &work_root,
        pointer.content_id.as_str(),
        receipt_is_committed,
    )?;
    let staging_root = overlay_staging_root(&work_root, pointer.content_id.as_str())?;
    if staging_root.exists() {
        fs::remove_dir_all(&staging_root).map_err(WorkViewError::from)?;
    }
    materialize_exposed_base(
        ExposedBaseMaterializationRequest {
            store,
            byte_store,
            options,
            work_view,
            project_prefix,
            exposed: &exposed,
            work_root: &work_root,
            staging_root: &staging_root,
        },
        checkpoint,
    )?;
    preserve_local_only_files(
        PreserveLocalOnlyRequest {
            store,
            work_view,
            work_root: &work_root,
            staging_root: &staging_root,
            project_prefix,
            exposed: &exposed,
            prior_manifest: prior_manifest.as_ref(),
            incoming_manifest: &manifest,
        },
        checkpoint,
    )?;
    let state_root = options
        .db_path
        .parent()
        .ok_or(WorkViewOverlaySyncError::MissingStateRoot)?;
    let cache = LocalContentCache::open(state_root.join("cache"))?;
    let apply_result = (|| {
        for entry in manifest.operations() {
            checkpoint()?;
            let authority_path = match entry.operation {
                OverlayOperation::Delete => Some(entry.path.as_str()),
                OverlayOperation::Rename => entry.from.as_deref(),
                OverlayOperation::Create | OverlayOperation::Modify => None,
            };
            let outside_exposure = authority_path.is_some_and(|path| {
                let exposed_path = if project_prefix.is_empty() {
                    path.to_string()
                } else {
                    format!("{project_prefix}/{path}")
                };
                !exposed_paths.contains(&exposed_path)
            });
            if outside_exposure {
                return Err(WorkViewError::UnsafeWorkViewPath {
                    path: authority_path.unwrap_or(&entry.path).to_string(),
                    reason: "overlay tombstone is outside the authoritative exposed base",
                }
                .into());
            }
            if let Some(content) = &entry.content {
                let owner_only = owner_only_by_path
                    .get(&entry.path)
                    .copied()
                    .ok_or_else(|| WorkViewError::UnsafeWorkViewPath {
                        path: entry.path.clone(),
                        reason: "overlay content has no receiver policy decision",
                    })?;
                materialize_overlay_content(
                    OverlayMaterializationRequest {
                        byte_store,
                        cache: &cache,
                        options,
                        work_view,
                        work_root: &staging_root,
                        relative_path: &entry.path,
                        content,
                        owner_only,
                        executability: entry.executability,
                    },
                    checkpoint,
                )?;
            }
            if entry.operation == OverlayOperation::Delete {
                remove_overlay_path(&staging_root, &entry.path)?;
            }
            if entry.operation == OverlayOperation::Rename
                && let Some(from) = &entry.from
            {
                remove_overlay_path(&staging_root, from)?;
            }
        }
        Ok::<(), WorkViewOverlaySyncError>(())
    })();
    if let Err(error) = apply_result {
        let _ = fs::remove_dir_all(&staging_root);
        return Err(error);
    }
    let encoded_overlay = serde_json::to_string(&manifest)?;
    checkpoint()?;
    let backup = publish_overlay_tree(&work_root, &staging_root, pointer.content_id.as_str())?;
    let previous_work_view = work_view.clone();
    work_view.overlay_head = pointer.content_id.as_str().to_string();
    work_view.overlay_version = remote_overlay_version;
    work_view.sync_state = bowline_core::work_views::WorkViewSyncState::Synced;
    work_view.attention.clear();
    work_view.updated_at = options.generated_at.clone();
    if let Err(error) = retire_superseded_overlay_chunks(
        control_plane,
        work_view,
        prior_manifest.as_ref(),
        &manifest,
    ) {
        *work_view = previous_work_view;
        rollback_overlay_publish(&work_root, &backup)?;
        return Err(error);
    }
    if let Err(error) =
        store.commit_materialized_overlay(work_view, pointer.content_id.as_str(), &encoded_overlay)
    {
        *work_view = previous_work_view;
        rollback_overlay_publish(&work_root, &backup)?;
        return Err(error.into());
    }
    fs::remove_dir_all(backup).map_err(WorkViewError::from)?;
    Ok(manifest.operations().len())
}

struct ExposedBaseMaterializationRequest<'a> {
    store: &'a MetadataStore,
    byte_store: &'a dyn ByteStore,
    options: &'a WorkViewOverlaySyncOptions,
    work_view: &'a WorkView,
    project_prefix: &'a str,
    exposed: &'a [NamespaceEntry],
    work_root: &'a Path,
    staging_root: &'a Path,
}

fn materialize_exposed_base(
    request: ExposedBaseMaterializationRequest<'_>,
    checkpoint: &mut dyn FnMut() -> Result<(), WorkViewOverlaySyncError>,
) -> Result<(), WorkViewOverlaySyncError> {
    let ExposedBaseMaterializationRequest {
        store,
        byte_store,
        options,
        work_view,
        project_prefix,
        exposed,
        work_root,
        staging_root,
    } = request;
    fs::create_dir(staging_root).map_err(WorkViewError::from)?;
    let cache = LocalContentCache::open(
        options
            .db_path
            .parent()
            .ok_or(WorkViewOverlaySyncError::MissingStateRoot)?
            .join("cache"),
    )?;
    for exposed_entry in exposed {
        checkpoint()?;
        let Some(relative) = exposed_entry
            .path
            .strip_prefix(project_prefix)
            .map(|path| path.trim_start_matches('/'))
        else {
            return Err(WorkViewError::UnsafeWorkViewPath {
                path: exposed_entry.path.clone(),
                reason: "exposed base entry is outside its project prefix",
            }
            .into());
        };
        if relative.is_empty() {
            if exposed_entry.kind == NamespaceEntryKind::Directory {
                continue;
            }
            return Err(WorkViewError::UnsafeWorkViewPath {
                path: exposed_entry.path.clone(),
                reason: "the project root may only be an exposed directory",
            }
            .into());
        }
        let destination = checked_overlay_destination(staging_root, relative)?;
        match exposed_entry.kind {
            NamespaceEntryKind::Directory => {
                fs::create_dir_all(destination).map_err(WorkViewError::from)?;
            }
            NamespaceEntryKind::File => {
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent).map_err(WorkViewError::from)?;
                }
                materialize_exposed_file(
                    ExposedFileMaterializationRequest {
                        store,
                        byte_store,
                        options,
                        work_view,
                        cache: &cache,
                        exposed: exposed_entry,
                        live_source: &work_root.join(relative),
                        destination: &destination,
                    },
                    checkpoint,
                )?;
            }
            NamespaceEntryKind::Symlink
            | NamespaceEntryKind::Placeholder
            | NamespaceEntryKind::Tombstone => {}
        }
    }
    Ok(())
}

struct ExposedFileMaterializationRequest<'a> {
    store: &'a MetadataStore,
    byte_store: &'a dyn ByteStore,
    options: &'a WorkViewOverlaySyncOptions,
    work_view: &'a WorkView,
    cache: &'a LocalContentCache,
    exposed: &'a NamespaceEntry,
    live_source: &'a Path,
    destination: &'a Path,
}

fn materialize_exposed_file(
    request: ExposedFileMaterializationRequest<'_>,
    checkpoint: &mut dyn FnMut() -> Result<(), WorkViewOverlaySyncError>,
) -> Result<(), WorkViewOverlaySyncError> {
    let ExposedFileMaterializationRequest {
        store,
        byte_store,
        options,
        work_view,
        cache,
        exposed,
        live_source,
        destination,
    } = request;
    checkpoint()?;
    let content_id = exposed
        .content_id
        .as_ref()
        .ok_or(WorkViewOverlaySyncError::MissingStagedContent)?;
    let exposed_content_key = options.workspace_content_key;
    if live_source.is_file() {
        let cached = cache.open_previously_verified_content(content_id).is_ok();
        let live_matches = if cached {
            verified_content_matches_path(cache, content_id, live_source)?
        } else {
            let (identity, mut input) = open_stable_regular_file(live_source, None)?;
            let actual = bowline_core::workspace_graph::workspace_content_id_reader(
                exposed_content_key,
                &mut input,
            )
            .map_err(WorkViewError::from)?;
            verify_stable_regular_file(live_source, &input, identity)?;
            &actual == content_id
        };
        if live_matches {
            let (identity, mut input) = open_stable_regular_file(live_source, None)?;
            let mut output = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(destination)
                .map_err(WorkViewError::from)?;
            std::io::copy(&mut input, &mut output).map_err(WorkViewError::from)?;
            output.sync_all().map_err(WorkViewError::from)?;
            verify_stable_regular_file(live_source, &input, identity)?;
            apply_exposed_permissions(
                destination,
                is_owner_only_work_view_policy(exposed.classification, exposed.mode),
                exposed.executability,
            )?;
            return Ok(());
        }
    }
    if cache.open_previously_verified_content(content_id).is_err() {
        hydrate_exposed_content(
            store, byte_store, options, work_view, cache, content_id, checkpoint,
        )?;
    }
    write_exposed_reader(
        cache.open_previously_verified_content(content_id)?,
        destination,
        checkpoint,
    )?;
    apply_exposed_permissions(
        destination,
        is_owner_only_work_view_policy(exposed.classification, exposed.mode),
        exposed.executability,
    )?;
    Ok(())
}

fn hydrate_exposed_content(
    store: &MetadataStore,
    byte_store: &dyn ByteStore,
    options: &WorkViewOverlaySyncOptions,
    work_view: &WorkView,
    cache: &LocalContentCache,
    content_id: &ContentId,
    checkpoint: &mut dyn FnMut() -> Result<(), WorkViewOverlaySyncError>,
) -> Result<(), WorkViewOverlaySyncError> {
    checkpoint()?;
    let locator = store
        .content_locator(&work_view.workspace_id, content_id)?
        .ok_or(WorkViewOverlaySyncError::MissingStagedContent)?
        .locator;
    let pack_id = locator
        .pack_id
        .as_ref()
        .ok_or(WorkViewOverlaySyncError::MissingStagedContent)?;
    let pack = store
        .pack_record_by_id(&work_view.workspace_id, pack_id)?
        .ok_or(WorkViewOverlaySyncError::MissingStagedContent)?;
    let object_key = ObjectKey::from_pack_id(pack_id)?;
    cache.hydrate_record_from_range(
        byte_store,
        RangeHydrationRequest {
            object_key: &object_key,
            workspace_id: &work_view.workspace_id,
            locator: &locator,
            content_key: options.workspace_content_key,
            content_verification: bowline_storage::ContentVerification::WorkspaceKeyed,
            key: options.storage_key,
            key_epoch: pack.key_epoch,
        },
    )?;
    checkpoint()?;
    Ok(())
}

fn write_exposed_reader(
    mut reader: impl Read,
    destination: &Path,
    checkpoint: &mut dyn FnMut() -> Result<(), WorkViewOverlaySyncError>,
) -> Result<(), WorkViewOverlaySyncError> {
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)
        .map_err(WorkViewError::from)?;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        checkpoint()?;
        let read = reader.read(&mut buffer).map_err(WorkViewError::from)?;
        if read == 0 {
            break;
        }
        output
            .write_all(&buffer[..read])
            .map_err(WorkViewError::from)?;
    }
    output.sync_all().map_err(WorkViewError::from)?;
    Ok(())
}

#[cfg(unix)]
fn apply_exposed_permissions(
    destination: &Path,
    owner_only: bool,
    executability: FileExecutability,
) -> Result<(), WorkViewOverlaySyncError> {
    use std::os::unix::fs::PermissionsExt;
    let mode = match (owner_only, executability) {
        (true, FileExecutability::Regular) => 0o600,
        (true, FileExecutability::Executable) => 0o700,
        (false, FileExecutability::Regular) => 0o644,
        (false, FileExecutability::Executable) => 0o755,
    };
    fs::set_permissions(destination, fs::Permissions::from_mode(mode))
        .map_err(WorkViewError::from)?;
    Ok(())
}

#[cfg(not(unix))]
fn apply_exposed_permissions(
    destination: &Path,
    _owner_only: bool,
    _executability: FileExecutability,
) -> Result<(), WorkViewOverlaySyncError> {
    let mut permissions = fs::metadata(destination)
        .map_err(WorkViewError::from)?
        .permissions();
    permissions.set_readonly(false);
    fs::set_permissions(destination, permissions).map_err(WorkViewError::from)?;
    Ok(())
}

pub(super) fn read_overlay_manifest(
    byte_store: &dyn ByteStore,
    options: &WorkViewOverlaySyncOptions,
    work_view: &WorkView,
    pointer: &ObjectPointer,
) -> Result<OverlayManifest, WorkViewOverlaySyncError> {
    read_overlay_manifest_with_checkpoint(byte_store, options, work_view, pointer, &mut || Ok(()))
}

pub(super) fn read_overlay_manifest_with_checkpoint(
    byte_store: &dyn ByteStore,
    options: &WorkViewOverlaySyncOptions,
    work_view: &WorkView,
    pointer: &ObjectPointer,
    checkpoint: &mut dyn FnMut() -> Result<(), WorkViewOverlaySyncError>,
) -> Result<OverlayManifest, WorkViewOverlaySyncError> {
    let object_key = ObjectKey::new(pointer.object_key.clone())?;
    checkpoint()?;
    let metadata = byte_store.head_object(&object_key)?;
    if metadata.kind != StorageObjectKind::AgentOverlay
        || metadata.byte_len != pointer.byte_len
        || metadata.hash != pointer.hash
        || metadata.key_epoch != pointer.key_epoch
    {
        return Err(ControlPlaneError::Conflict {
            resource: "work-view overlay",
            reason: "overlay root metadata does not match the committed pointer",
        }
        .into());
    }
    if metadata.byte_len > (super::overlay_wire::MAX_OVERLAY_MANIFEST_BYTES + 64 * 1024) as u64 {
        return Err(super::overlay_wire::OverlayWireError::ManifestLimitExceeded.into());
    }
    checkpoint()?;
    let pack_bytes = byte_store.get_object(&object_key)?;
    if pack_bytes.len() > super::overlay_wire::MAX_OVERLAY_MANIFEST_BYTES + 64 * 1024 {
        return Err(super::overlay_wire::OverlayWireError::ManifestLimitExceeded.into());
    }
    let index = parse_index(&pack_bytes)?;
    let record = index
        .records
        .get(&pointer.content_id)
        .ok_or(WorkViewOverlaySyncError::MissingOverlayPack)?;
    let locator = ContentLocator {
        content_id: pointer.content_id.clone(),
        storage: ContentStorage::Packed,
        raw_size: record.raw_size,
        pack_id: Some(index.pack_id),
        offset: Some(record.offset),
        length: Some(record.length),
    };
    let cache = LocalContentCache::open(
        options
            .db_path
            .parent()
            .ok_or(WorkViewOverlaySyncError::MissingStateRoot)?
            .join("cache"),
    )?;
    checkpoint()?;
    let bytes = cache.hydrate_record_from_range(
        byte_store,
        RangeHydrationRequest {
            object_key: &object_key,
            workspace_id: &work_view.workspace_id,
            locator: &locator,
            content_key: options.workspace_content_key,
            content_verification: bowline_storage::ContentVerification::WorkspaceKeyed,
            key: options.storage_key,
            key_epoch: pointer.key_epoch,
        },
    )?;
    checkpoint()?;
    let manifest = OverlayManifest::decode(&bytes)?;
    if manifest.work_view_id != work_view.id
        || manifest.base_snapshot_id != work_view.base_snapshot_id
    {
        return Err(ControlPlaneError::Conflict {
            resource: "work-view overlay",
            reason: "overlay root identity does not match the local work view",
        }
        .into());
    }
    Ok(manifest)
}

#[cfg(test)]
pub(super) fn overlay_manifest_matches_local(
    manifest: &OverlayManifest,
    options: &WorkViewOverlaySyncOptions,
    work_view: &WorkView,
    upload_plan: &OverlayUploadPlan,
) -> Result<bool, WorkViewOverlaySyncError> {
    overlay_manifest_matches_local_with_checkpoint(
        manifest,
        options,
        work_view,
        upload_plan,
        &mut || Ok(()),
    )
}

pub(super) fn overlay_manifest_matches_local_with_checkpoint(
    manifest: &OverlayManifest,
    options: &WorkViewOverlaySyncOptions,
    work_view: &WorkView,
    upload_plan: &OverlayUploadPlan,
    checkpoint: &mut dyn FnMut() -> Result<(), WorkViewOverlaySyncError>,
) -> Result<bool, WorkViewOverlaySyncError> {
    if manifest.operations().len() != upload_plan.deltas.len() {
        return Ok(false);
    }
    let work_root = expand_display_path(&work_view.visible_path);
    for (entry, delta) in manifest.operations().iter().zip(&upload_plan.deltas) {
        checkpoint()?;
        if entry.path != normalize_workspace_path(&delta.path.display().to_string())
            || entry.operation != overlay_operation(&delta.kind)?
            || entry.from != overlay_delta_rename_from(&delta.kind)
            || entry.contains_secrets != delta.contains_secrets
            || (entry.content.is_some()
                && entry.executability != overlay_path_executability(&work_root.join(&delta.path))?)
        {
            return Ok(false);
        }
        if let Some(content) = &entry.content {
            if let Some(staged_content_id) = delta
                .write_id
                .as_ref()
                .and_then(|write_id| upload_plan.staged_content_by_write_id.get(write_id))
            {
                if &content.content_id != staged_content_id {
                    return Ok(false);
                }
                continue;
            }
            let path = work_root.join(&delta.path);
            let (identity, mut file) = open_stable_regular_file(&path, None)?;
            let (content_id, byte_len) = hash_overlay_reader_with_checkpoint(
                &mut file,
                options.workspace_content_key,
                checkpoint,
            )?;
            verify_stable_regular_file(&path, &file, identity)?;
            if content.content_id != content_id || content.byte_len != byte_len {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

pub(super) fn hash_overlay_reader_with_checkpoint(
    reader: &mut impl Read,
    workspace_content_key: [u8; 32],
    checkpoint: &mut dyn FnMut() -> Result<(), WorkViewOverlaySyncError>,
) -> Result<(ContentId, u64), WorkViewOverlaySyncError> {
    let mut hasher = blake3::Hasher::new_keyed(&workspace_content_key);
    let mut byte_len = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        checkpoint()?;
        let read = reader.read(&mut buffer).map_err(WorkViewError::from)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        byte_len = byte_len.saturating_add(read as u64);
    }
    Ok((
        ContentId::new(format!("cid_{}", hasher.finalize().to_hex())),
        byte_len,
    ))
}

struct OverlayMaterializationRequest<'a> {
    byte_store: &'a dyn ByteStore,
    cache: &'a LocalContentCache,
    options: &'a WorkViewOverlaySyncOptions,
    work_view: &'a WorkView,
    work_root: &'a Path,
    relative_path: &'a str,
    content: &'a OverlayContent,
    owner_only: bool,
    executability: FileExecutability,
}

fn materialize_overlay_content(
    request: OverlayMaterializationRequest<'_>,
    checkpoint: &mut dyn FnMut() -> Result<(), WorkViewOverlaySyncError>,
) -> Result<(), WorkViewOverlaySyncError> {
    let OverlayMaterializationRequest {
        byte_store,
        cache,
        options,
        work_view,
        work_root,
        relative_path,
        content,
        owner_only,
        executability,
    } = request;
    let destination = checked_overlay_destination(work_root, relative_path)?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(WorkViewError::from)?;
    }
    let (pending, mut output) =
        create_pending_destination(&destination, owner_only).map_err(WorkViewError::from)?;
    let result = (|| -> Result<(), WorkViewOverlaySyncError> {
        let mut hasher = blake3::Hasher::new_keyed(&options.workspace_content_key);
        let mut written = 0_u64;
        for chunk in &content.chunks {
            checkpoint()?;
            let object_key = ObjectKey::new(chunk.object_key.clone())?;
            let bytes = cache.hydrate_record_from_range(
                byte_store,
                RangeHydrationRequest {
                    object_key: &object_key,
                    workspace_id: &work_view.workspace_id,
                    locator: &chunk.locator,
                    content_key: options.workspace_content_key,
                    content_verification:
                        bowline_storage::ContentVerification::AuthenticatedSegment,
                    key: options.storage_key,
                    key_epoch: chunk.key_epoch,
                },
            )?;
            checkpoint()?;
            if bytes.len() as u64 != chunk.plaintext_len {
                return Err(super::overlay_wire::OverlayWireError::InvalidContentLayout.into());
            }
            output.write_all(&bytes).map_err(WorkViewError::from)?;
            hasher.update(&bytes);
            written = written.saturating_add(bytes.len() as u64);
        }
        output.sync_all().map_err(WorkViewError::from)?;
        drop(output);
        let actual = ContentId::new(format!("cid_{}", hasher.finalize().to_hex()));
        if written != content.byte_len || actual != content.content_id {
            return Err(super::overlay_wire::OverlayWireError::InvalidContentLayout.into());
        }
        apply_exposed_permissions(&pending, owner_only, executability)?;
        fs::rename(&pending, destination).map_err(WorkViewError::from)?;
        Ok(())
    })();
    if result.is_err() {
        cleanup_pending(&pending);
    }
    result
}

#[cfg(unix)]
fn overlay_path_executability(path: &Path) -> Result<FileExecutability, WorkViewOverlaySyncError> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = path
        .symlink_metadata()
        .map_err(WorkViewError::from)?
        .permissions()
        .mode();
    Ok(if mode & 0o111 == 0 {
        FileExecutability::Regular
    } else {
        FileExecutability::Executable
    })
}

#[cfg(not(unix))]
fn overlay_path_executability(_path: &Path) -> Result<FileExecutability, WorkViewOverlaySyncError> {
    Ok(FileExecutability::Regular)
}

fn remove_overlay_path(
    work_root: &Path,
    relative_path: &str,
) -> Result<(), WorkViewOverlaySyncError> {
    let path = checked_overlay_destination(work_root, relative_path)?;
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(WorkViewError::from(error).into()),
    }
}

pub(super) fn checked_overlay_destination(
    work_root: &Path,
    relative_path: &str,
) -> Result<PathBuf, WorkViewOverlaySyncError> {
    let normalized = normalize_workspace_path(relative_path);
    if normalized != relative_path
        || normalized.is_empty()
        || normalized.starts_with('/')
        || normalized.split('/').any(|component| component == "..")
    {
        return Err(WorkViewError::UnsafeWorkViewPath {
            path: relative_path.to_string(),
            reason: "overlay path escapes the work-view root",
        }
        .into());
    }
    let mut current = work_root.to_path_buf();
    for component in normalized
        .split('/')
        .take(normalized.split('/').count().saturating_sub(1))
    {
        current.push(component);
        if fs::symlink_metadata(&current).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err(WorkViewError::UnsafeWorkViewPath {
                path: relative_path.to_string(),
                reason: "overlay path traverses a symlink",
            }
            .into());
        }
    }
    Ok(work_root.join(normalized))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::TempWorkspace;

    #[cfg(unix)]
    #[test]
    fn overlay_pending_file_does_not_follow_a_preserved_symlink() {
        use std::os::unix::fs::symlink;

        let temp = TempWorkspace::new("overlay-pending-symlink").expect("temp workspace");
        let staging = temp.root().join("staging");
        fs::create_dir(&staging).expect("staging root");
        let destination = staging.join("payload.txt");
        let outside = temp.root().join("outside.txt");
        fs::write(&outside, b"outside remains unchanged").expect("outside fixture");
        let predictable =
            destination.with_extension(format!("bowline-overlay-{}.pending", std::process::id()));
        symlink(&outside, &predictable).expect("attacker-controlled pending symlink");

        let (pending, mut output) =
            create_pending_destination(&destination, false).expect("exclusive pending file");
        assert_ne!(pending, predictable);
        assert!(
            output
                .metadata()
                .expect("pending metadata")
                .file_type()
                .is_file()
        );
        output
            .write_all(b"authenticated overlay")
            .expect("pending write");
        output.sync_all().expect("pending sync");
        drop(output);
        fs::rename(&pending, &destination).expect("publish pending file");

        assert_eq!(
            fs::read(&outside).expect("outside target"),
            b"outside remains unchanged"
        );
        assert_eq!(
            fs::read(&destination).expect("published overlay"),
            b"authenticated overlay"
        );
        assert!(predictable.is_symlink());
    }

    #[test]
    fn composed_overlay_namespace_rejects_case_only_collisions() {
        let namespace = std::collections::BTreeMap::from([
            ("src/App.rs".to_string(), NamespaceEntryKind::File),
            ("src/app.rs".to_string(), NamespaceEntryKind::File),
        ]);

        let error = validate_overlay_namespace(&namespace).expect_err("case collision");
        assert!(error.to_string().contains("case-folded"));
    }

    #[test]
    fn composed_overlay_namespace_rejects_file_directory_aliases() {
        let namespace = std::collections::BTreeMap::from([
            ("config".to_string(), NamespaceEntryKind::File),
            ("config/dev.toml".to_string(), NamespaceEntryKind::File),
        ]);

        let error = validate_overlay_namespace(&namespace).expect_err("prefix collision");
        assert!(error.to_string().contains("overlay namespace"));
    }
}
