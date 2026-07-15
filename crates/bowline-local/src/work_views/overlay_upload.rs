use std::{
    collections::BTreeMap,
    io::{Read, Seek as _, SeekFrom},
    path::Path,
};

use bowline_control_plane::{ControlPlaneClient, ObjectRetentionStateUpdate};
use bowline_core::{
    ids::ContentId,
    work_views::WorkView,
    workspace_graph::{FileExecutability, normalize_workspace_path},
};
use bowline_storage::{
    ByteStore, ContentVerification, LocalContentCache, ObjectKey, RangeHydrationRequest,
    RetentionState as StorageRetentionState,
};

use crate::metadata::MetadataStore;

use super::{
    WorkViewError, WorkViewOverlaySyncError,
    content_identity::{open_stable_regular_file, verify_stable_regular_file},
    overlay,
    overlay_objects::{derive_overlay_payload_pack, upload_overlay_payload_with_checkpoint},
    overlay_receive::hash_overlay_reader_with_checkpoint,
    overlay_sync::{
        OverlayUploadPlan, WorkViewOverlaySyncOptions, overlay_delta_rename_from, overlay_operation,
    },
    overlay_wire::{
        OVERLAY_CHUNK_BYTES, OverlayContent, OverlayContentChunk, OverlayManifest,
        OverlayManifestEntry,
    },
    paths::expand_display_path,
};

pub(super) struct BuiltOverlayManifest {
    pub(super) manifest: OverlayManifest,
    pub(super) entries_completed: usize,
    pub(super) content_objects_uploaded: usize,
    pub(super) content_objects_reused: usize,
    pub(super) plaintext_bytes: u64,
    pub(super) uploaded_bytes: u64,
    pub(super) uploaded_chunk_object_keys: Vec<String>,
}

#[cfg(test)]
pub(super) fn build_overlay_manifest(
    store: &MetadataStore,
    control_plane: &dyn ControlPlaneClient,
    byte_store: &dyn ByteStore,
    options: &WorkViewOverlaySyncOptions,
    work_view: &WorkView,
    upload_plan: &OverlayUploadPlan,
) -> Result<BuiltOverlayManifest, WorkViewOverlaySyncError> {
    build_overlay_manifest_with_checkpoint(
        store,
        control_plane,
        byte_store,
        options,
        work_view,
        upload_plan,
        &mut || Ok(()),
    )
}

pub(super) fn build_overlay_manifest_with_checkpoint(
    store: &MetadataStore,
    control_plane: &dyn ControlPlaneClient,
    byte_store: &dyn ByteStore,
    options: &WorkViewOverlaySyncOptions,
    work_view: &WorkView,
    upload_plan: &OverlayUploadPlan,
    checkpoint: &mut dyn FnMut() -> Result<(), WorkViewOverlaySyncError>,
) -> Result<BuiltOverlayManifest, WorkViewOverlaySyncError> {
    let mut uploaded_chunk_object_keys = Vec::new();
    let result = build_overlay_manifest_inner(OverlayManifestBuildRequest {
        store,
        control_plane,
        byte_store,
        options,
        work_view,
        upload_plan,
        uploaded_chunk_object_keys: &mut uploaded_chunk_object_keys,
        checkpoint,
    });
    match result {
        Ok(built) => Ok(built),
        Err(error) => {
            retire_uploaded_chunks(
                control_plane,
                work_view.workspace_id.as_str(),
                &uploaded_chunk_object_keys,
            )?;
            Err(error)
        }
    }
}

struct OverlayManifestBuildRequest<'a> {
    store: &'a MetadataStore,
    control_plane: &'a dyn ControlPlaneClient,
    byte_store: &'a dyn ByteStore,
    options: &'a WorkViewOverlaySyncOptions,
    work_view: &'a WorkView,
    upload_plan: &'a OverlayUploadPlan,
    uploaded_chunk_object_keys: &'a mut Vec<String>,
    checkpoint: &'a mut dyn FnMut() -> Result<(), WorkViewOverlaySyncError>,
}

fn build_overlay_manifest_inner(
    request: OverlayManifestBuildRequest<'_>,
) -> Result<BuiltOverlayManifest, WorkViewOverlaySyncError> {
    let OverlayManifestBuildRequest {
        store,
        control_plane,
        byte_store,
        options,
        work_view,
        upload_plan,
        uploaded_chunk_object_keys,
        checkpoint,
    } = request;
    let work_root = expand_display_path(&work_view.visible_path);
    let mut entries = Vec::with_capacity(upload_plan.deltas.len());
    let mut content_objects_uploaded = 0_usize;
    let mut content_objects_reused = 0_usize;
    let mut plaintext_bytes = 0_u64;
    let mut uploaded_bytes = 0_u64;
    let mut reusable_chunks = BTreeMap::new();
    for delta in &upload_plan.deltas {
        checkpoint()?;
        let file_path = work_root.join(&delta.path);
        let content = if matches!(
            delta.kind,
            overlay::OverlayDeltaKind::Create
                | overlay::OverlayDeltaKind::Modify
                | overlay::OverlayDeltaKind::Rename { .. }
        ) {
            let prepared = if let Some(content_id) = delta
                .write_id
                .as_ref()
                .and_then(|write_id| upload_plan.staged_content_by_write_id.get(write_id))
            {
                upload_staged_content_tracked(StagedContentUploadRequest {
                    store,
                    control_plane,
                    byte_store,
                    options,
                    work_view,
                    content_id,
                    uploaded_object_keys: uploaded_chunk_object_keys,
                    reusable_chunks: &mut reusable_chunks,
                    checkpoint,
                })?
            } else {
                upload_file_content(FileContentUploadRequest {
                    control_plane,
                    byte_store,
                    options,
                    work_view,
                    path: &file_path,
                    uploaded_object_keys: uploaded_chunk_object_keys,
                    reusable_chunks: &mut reusable_chunks,
                    checkpoint,
                })?
            };
            content_objects_uploaded =
                content_objects_uploaded.saturating_add(prepared.objects_uploaded);
            content_objects_reused = content_objects_reused.saturating_add(prepared.objects_reused);
            plaintext_bytes = plaintext_bytes.saturating_add(prepared.content.byte_len);
            uploaded_bytes = uploaded_bytes.saturating_add(prepared.uploaded_bytes);
            Some(prepared.content)
        } else {
            None
        };
        entries.push(OverlayManifestEntry {
            path: normalize_workspace_path(&delta.path.display().to_string()),
            operation: overlay_operation(&delta.kind)?,
            from: overlay_delta_rename_from(&delta.kind),
            contains_secrets: delta.contains_secrets,
            executability: overlay_file_executability(&file_path, content.is_some())?,
            content,
        });
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    let manifest = OverlayManifest::new(
        work_view.id.clone(),
        work_view.base_snapshot_id.clone(),
        entries,
    )?;
    Ok(BuiltOverlayManifest {
        entries_completed: manifest.operations().len(),
        manifest,
        content_objects_uploaded,
        content_objects_reused,
        plaintext_bytes,
        uploaded_bytes,
        uploaded_chunk_object_keys: uploaded_chunk_object_keys.clone(),
    })
}

fn retire_uploaded_chunks(
    control_plane: &dyn ControlPlaneClient,
    workspace_id: &str,
    object_keys: &[String],
) -> Result<(), WorkViewOverlaySyncError> {
    for object_key in object_keys {
        control_plane.mark_object_retention_state(ObjectRetentionStateUpdate::new(
            workspace_id,
            object_key,
            StorageRetentionState::OrphanCandidate,
        ))?;
    }
    Ok(())
}

#[cfg(unix)]
fn overlay_file_executability(
    path: &Path,
    has_content: bool,
) -> Result<FileExecutability, WorkViewOverlaySyncError> {
    use std::os::unix::fs::PermissionsExt as _;

    if !has_content {
        return Ok(FileExecutability::Regular);
    }
    let metadata = path.symlink_metadata().map_err(WorkViewError::from)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(WorkViewError::UnsafeWorkViewPath {
            path: normalize_workspace_path(&path.display().to_string()),
            reason: "overlay content source is not a regular file",
        }
        .into());
    }
    let mode = metadata.permissions().mode();
    Ok(if mode & 0o111 == 0 {
        FileExecutability::Regular
    } else {
        FileExecutability::Executable
    })
}

#[cfg(not(unix))]
fn overlay_file_executability(
    path: &Path,
    has_content: bool,
) -> Result<FileExecutability, WorkViewOverlaySyncError> {
    if has_content {
        let metadata = path.symlink_metadata().map_err(WorkViewError::from)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(WorkViewError::UnsafeWorkViewPath {
                path: normalize_workspace_path(&path.display().to_string()),
                reason: "overlay content source is not a regular file",
            }
            .into());
        }
    }
    Ok(FileExecutability::Regular)
}

pub(super) struct UploadedFileContent {
    pub(super) content: OverlayContent,
    objects_uploaded: usize,
    objects_reused: usize,
    uploaded_bytes: u64,
}

struct FileContentUploadRequest<'a> {
    control_plane: &'a dyn ControlPlaneClient,
    byte_store: &'a dyn ByteStore,
    options: &'a WorkViewOverlaySyncOptions,
    work_view: &'a WorkView,
    path: &'a Path,
    uploaded_object_keys: &'a mut Vec<String>,
    reusable_chunks: &'a mut BTreeMap<ContentId, OverlayContentChunk>,
    checkpoint: &'a mut dyn FnMut() -> Result<(), WorkViewOverlaySyncError>,
}

fn upload_file_content(
    request: FileContentUploadRequest<'_>,
) -> Result<UploadedFileContent, WorkViewOverlaySyncError> {
    let FileContentUploadRequest {
        control_plane,
        byte_store,
        options,
        work_view,
        path,
        uploaded_object_keys,
        reusable_chunks,
        checkpoint,
    } = request;
    checkpoint()?;
    let (identity, mut file) = open_stable_regular_file(path, None)?;
    let (logical_content_id, expected_len) =
        hash_overlay_reader_with_checkpoint(&mut file, options.workspace_content_key, checkpoint)?;
    file.seek(SeekFrom::Start(0)).map_err(WorkViewError::from)?;
    let uploaded = upload_content_reader(ContentUploadRequest {
        control_plane,
        byte_store,
        options,
        work_view,
        logical_content_id: &logical_content_id,
        expected_len,
        reader: &mut file,
        uploaded_object_keys,
        reusable_chunks,
        checkpoint,
    })?;
    verify_stable_regular_file(path, &file, identity)?;
    Ok(uploaded)
}

#[cfg(test)]
pub(super) fn upload_staged_content(
    store: &MetadataStore,
    control_plane: &dyn ControlPlaneClient,
    byte_store: &dyn ByteStore,
    options: &WorkViewOverlaySyncOptions,
    work_view: &WorkView,
    content_id: &ContentId,
) -> Result<UploadedFileContent, WorkViewOverlaySyncError> {
    let mut uploaded_object_keys = Vec::new();
    let mut reusable_chunks = BTreeMap::new();
    let result = upload_staged_content_tracked(StagedContentUploadRequest {
        store,
        control_plane,
        byte_store,
        options,
        work_view,
        content_id,
        uploaded_object_keys: &mut uploaded_object_keys,
        reusable_chunks: &mut reusable_chunks,
        checkpoint: &mut || Ok(()),
    });
    match result {
        Ok(uploaded) => Ok(uploaded),
        Err(error) => {
            retire_uploaded_chunks(
                control_plane,
                work_view.workspace_id.as_str(),
                &uploaded_object_keys,
            )?;
            Err(error)
        }
    }
}

struct StagedContentUploadRequest<'a> {
    store: &'a MetadataStore,
    control_plane: &'a dyn ControlPlaneClient,
    byte_store: &'a dyn ByteStore,
    options: &'a WorkViewOverlaySyncOptions,
    work_view: &'a WorkView,
    content_id: &'a ContentId,
    uploaded_object_keys: &'a mut Vec<String>,
    reusable_chunks: &'a mut BTreeMap<ContentId, OverlayContentChunk>,
    checkpoint: &'a mut dyn FnMut() -> Result<(), WorkViewOverlaySyncError>,
}

fn upload_staged_content_tracked(
    request: StagedContentUploadRequest<'_>,
) -> Result<UploadedFileContent, WorkViewOverlaySyncError> {
    let StagedContentUploadRequest {
        store,
        control_plane,
        byte_store,
        options,
        work_view,
        content_id,
        uploaded_object_keys,
        reusable_chunks,
        checkpoint,
    } = request;
    let state_root = options
        .db_path
        .parent()
        .ok_or(WorkViewOverlaySyncError::MissingStateRoot)?;
    let cache = LocalContentCache::open(state_root.join("cache"))?;
    let mut reader = match cache.open_previously_verified_content(content_id) {
        Ok(reader) => reader,
        Err(_) => {
            checkpoint()?;
            hydrate_staged_content(store, byte_store, options, work_view, &cache, content_id)?;
            cache
                .open_previously_verified_content(content_id)
                .map_err(|_| WorkViewOverlaySyncError::MissingStagedContent)?
        }
    };
    let expected_len = reader.byte_len();
    upload_content_reader(ContentUploadRequest {
        control_plane,
        byte_store,
        options,
        work_view,
        logical_content_id: content_id,
        expected_len,
        reader: &mut reader,
        uploaded_object_keys,
        reusable_chunks,
        checkpoint,
    })
}

fn hydrate_staged_content(
    store: &MetadataStore,
    byte_store: &dyn ByteStore,
    options: &WorkViewOverlaySyncOptions,
    work_view: &WorkView,
    cache: &LocalContentCache,
    content_id: &ContentId,
) -> Result<(), WorkViewOverlaySyncError> {
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
            content_verification: ContentVerification::WorkspaceKeyed,
            key: options.storage_key,
            key_epoch: pack.key_epoch,
        },
    )?;
    Ok(())
}

struct ContentUploadRequest<'a> {
    control_plane: &'a dyn ControlPlaneClient,
    byte_store: &'a dyn ByteStore,
    options: &'a WorkViewOverlaySyncOptions,
    work_view: &'a WorkView,
    logical_content_id: &'a ContentId,
    expected_len: u64,
    reader: &'a mut dyn Read,
    uploaded_object_keys: &'a mut Vec<String>,
    reusable_chunks: &'a mut BTreeMap<ContentId, OverlayContentChunk>,
    checkpoint: &'a mut dyn FnMut() -> Result<(), WorkViewOverlaySyncError>,
}

fn upload_content_reader(
    request: ContentUploadRequest<'_>,
) -> Result<UploadedFileContent, WorkViewOverlaySyncError> {
    let ContentUploadRequest {
        control_plane,
        byte_store,
        options,
        work_view,
        logical_content_id,
        expected_len,
        reader,
        uploaded_object_keys,
        reusable_chunks,
        checkpoint,
    } = request;
    let mut content_hasher = blake3::Hasher::new_keyed(&options.workspace_content_key);
    let mut chunks = Vec::new();
    let mut total = 0_u64;
    let mut objects_uploaded = 0_usize;
    let mut objects_reused = 0_usize;
    let mut uploaded_bytes = 0_u64;
    let mut buffer = vec![0_u8; OVERLAY_CHUNK_BYTES];
    loop {
        checkpoint()?;
        let mut filled = 0_usize;
        while filled < buffer.len() {
            let read = reader
                .read(&mut buffer[filled..])
                .map_err(WorkViewError::from)?;
            if read == 0 {
                break;
            }
            filled += read;
        }
        if filled == 0 {
            break;
        }
        let bytes = &buffer[..filled];
        content_hasher.update(bytes);
        total = total.saturating_add(filled as u64);
        let chunk_content_id = overlay_chunk_content_id(
            work_view,
            &options.device_id,
            logical_content_id,
            chunks.len() as u64,
            filled as u64,
        );
        if let Some(reused) = reusable_chunks.get(&chunk_content_id) {
            chunks.push(reused.clone());
            objects_reused += 1;
            if filled < buffer.len() {
                break;
            }
            continue;
        }
        let pack = derive_overlay_payload_pack(
            &work_view.workspace_id,
            bytes,
            chunk_content_id.clone(),
            options.storage_key,
            options.key_epoch,
        )?;
        let locator = pack
            .locators
            .first()
            .cloned()
            .ok_or(WorkViewOverlaySyncError::MissingOverlayPack)?;
        let uploaded = upload_overlay_payload_with_checkpoint(
            &work_view.workspace_id,
            &options.device_id,
            pack,
            control_plane,
            byte_store,
            options.key_epoch,
            checkpoint,
        )?;
        if uploaded.reused {
            objects_reused += 1;
        } else {
            objects_uploaded += 1;
            uploaded_bytes = uploaded_bytes.saturating_add(uploaded.pointer.byte_len);
            uploaded_object_keys.push(uploaded.pointer.object_key.clone());
        }
        let chunk = OverlayContentChunk {
            ordinal: u32::try_from(chunks.len()).map_err(|_| {
                WorkViewError::SnapshotMaterialization {
                    snapshot_id: work_view.base_snapshot_id.as_str().to_string(),
                    reason: "overlay content has too many chunks".to_string(),
                }
            })?,
            content_id: chunk_content_id.clone(),
            plaintext_len: filled as u64,
            object_key: uploaded.pointer.object_key,
            key_epoch: uploaded.pointer.key_epoch,
            locator,
        };
        reusable_chunks.insert(chunk_content_id, chunk.clone());
        chunks.push(chunk);
        if filled < buffer.len() {
            break;
        }
    }
    let captured_content_id = ContentId::new(format!("cid_{}", content_hasher.finalize().to_hex()));
    if total != expected_len || &captured_content_id != logical_content_id {
        return Err(WorkViewOverlaySyncError::MissingStagedContent);
    }
    Ok(UploadedFileContent {
        content: OverlayContent {
            content_id: logical_content_id.clone(),
            byte_len: total,
            chunks,
        },
        objects_uploaded,
        objects_reused,
        uploaded_bytes,
    })
}

fn overlay_chunk_content_id(
    work_view: &WorkView,
    device_id: &bowline_core::ids::DeviceId,
    logical_content_id: &ContentId,
    ordinal: u64,
    byte_len: u64,
) -> ContentId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"bowline-overlay-chunk-v2\0");
    hasher.update(work_view.id.as_str().as_bytes());
    hasher.update(&work_view.overlay_version.to_le_bytes());
    hasher.update(device_id.as_str().as_bytes());
    hasher.update(logical_content_id.as_str().as_bytes());
    hasher.update(&ordinal.to_le_bytes());
    hasher.update(&byte_len.to_le_bytes());
    ContentId::new(format!("cid_{}", hasher.finalize().to_hex()))
}
