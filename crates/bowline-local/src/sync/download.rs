use std::{collections::BTreeMap, error::Error, fmt, path::Path};

use bowline_control_plane::{
    ControlPlaneClient, ControlPlaneError, DownloadIntentRequest, ObjectManifestRecord,
    ObjectPointer,
};
use bowline_core::{
    ids::{ManifestId, SnapshotId, WorkspaceId},
    workspace_graph::{
        ContentLocator, ContentStorage, NamespaceEntryKind, SnapshotManifest,
        normalize_workspace_path,
    },
};
use bowline_storage::{
    ByteStore, ByteStoreError, ManifestPointer, ManifestPointerKind, ObjectKey,
    SealedSnapshotManifest, StorageKey, open_snapshot_manifest,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedSnapshot {
    pub manifest: SnapshotManifest,
    pub locators: Vec<ContentLocator>,
    pub pack_objects: Vec<ObjectPointer>,
}

pub fn import_snapshot_by_id(
    workspace_id: &WorkspaceId,
    snapshot_id: &SnapshotId,
    control_plane: &dyn ControlPlaneClient,
    byte_store: &dyn ByteStore,
    storage_key: StorageKey,
    key_epoch: u32,
) -> Result<ImportedSnapshot, DownloadError> {
    let object_manifest = control_plane
        .get_snapshot_manifest_pointer(workspace_id.as_str(), snapshot_id.as_str())?
        .ok_or_else(|| DownloadError::SnapshotManifestMissing(snapshot_id.as_str().to_string()))?;
    import_snapshot_manifest(
        workspace_id,
        &object_manifest,
        control_plane,
        byte_store,
        storage_key,
        key_epoch,
    )
}

pub fn import_snapshot_manifest(
    workspace_id: &WorkspaceId,
    object_manifest: &ObjectManifestRecord,
    control_plane: &dyn ControlPlaneClient,
    byte_store: &dyn ByteStore,
    storage_key: StorageKey,
    _key_epoch: u32,
) -> Result<ImportedSnapshot, DownloadError> {
    if object_manifest.workspace_id != workspace_id.as_str() {
        return Err(DownloadError::UnsafeManifest("manifest workspace mismatch"));
    }
    control_plane.create_download_intent(DownloadIntentRequest::full(
        workspace_id.as_str(),
        &object_manifest.manifest_object.object_key,
    ))?;
    let object_key = ObjectKey::new(object_manifest.manifest_object.object_key.clone())?;
    let bytes = byte_store.get_object(&object_key)?;
    let sealed = SealedSnapshotManifest {
        pointer: ManifestPointer {
            manifest_id: ManifestId::new(object_manifest.manifest_id.clone()),
            snapshot_id: bowline_core::ids::SnapshotId::new(object_manifest.snapshot_id.clone()),
            object_key,
            byte_len: object_manifest.manifest_object.byte_len,
            hash: object_manifest.manifest_object.hash.clone(),
            key_epoch: object_manifest.manifest_object.key_epoch,
            kind: ManifestPointerKind::Snapshot,
        },
        bytes,
    };
    let manifest = open_snapshot_manifest(&sealed, storage_key, workspace_id)?;
    validate_imported_manifest(
        workspace_id,
        &SnapshotId::new(object_manifest.snapshot_id.clone()),
        &manifest,
    )?;
    let locators = manifest
        .entries
        .iter()
        .filter_map(|entry| entry.locator.clone())
        .collect();
    Ok(ImportedSnapshot {
        manifest,
        locators,
        pack_objects: object_manifest.pack_objects.clone(),
    })
}

pub fn validate_imported_manifest(
    workspace_id: &WorkspaceId,
    expected_snapshot_id: &SnapshotId,
    manifest: &SnapshotManifest,
) -> Result<(), DownloadError> {
    if &manifest.workspace_id != workspace_id {
        return Err(DownloadError::UnsafeManifest("manifest workspace mismatch"));
    }
    if &manifest.snapshot_id != expected_snapshot_id {
        return Err(DownloadError::UnsafeManifest("manifest snapshot mismatch"));
    }
    let mut folded_paths = BTreeMap::<String, String>::new();
    for entry in &manifest.entries {
        let normalized = normalize_workspace_path(&entry.path);
        if normalized != entry.path
            || normalized.is_empty()
            || normalized.starts_with("../")
            || normalized.contains("/../")
            || is_private_state_path(&normalized)
        {
            return Err(DownloadError::UnsafePath(entry.path.clone()));
        }
        validate_case_folded_prefixes(&normalized, &mut folded_paths)?;
        if entry.kind == NamespaceEntryKind::File {
            let locator = entry
                .locator
                .as_ref()
                .ok_or(DownloadError::UnsafeManifest("file entry missing locator"))?;
            if locator.storage != ContentStorage::Packed {
                return Err(DownloadError::UnsafeManifest(
                    "phase 7 imports packed file locators only",
                ));
            }
            if Some(&locator.content_id) != entry.content_id.as_ref() {
                return Err(DownloadError::UnsafeManifest("locator content mismatch"));
            }
        } else if entry.kind == NamespaceEntryKind::Symlink {
            let target = entry
                .symlink_target
                .as_deref()
                .ok_or(DownloadError::UnsafeManifest(
                    "symlink entry missing target",
                ))?;
            validate_imported_symlink_target(target)?;
        }
    }
    Ok(())
}

fn validate_imported_symlink_target(target: &str) -> Result<(), DownloadError> {
    let normalized = normalize_workspace_path(target);
    if Path::new(target).is_absolute()
        || normalized != target
        || normalized.is_empty()
        || normalized == "."
        || normalized.starts_with("../")
        || normalized.contains("/../")
    {
        return Err(DownloadError::UnsafeManifest("unsafe symlink target"));
    }
    Ok(())
}

fn validate_case_folded_prefixes(
    path: &str,
    folded_paths: &mut BTreeMap<String, String>,
) -> Result<(), DownloadError> {
    let mut prefix = String::new();
    for component in path.split('/') {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(component);
        let folded = prefix.to_lowercase();
        if let Some(existing) = folded_paths.insert(folded, prefix.clone())
            && existing != prefix
        {
            return Err(DownloadError::UnsafeManifest("case-only path collision"));
        }
    }
    Ok(())
}

fn is_private_state_path(path: &str) -> bool {
    path == ".bowline" || path.starts_with(".bowline/")
}

#[derive(Debug)]
pub enum DownloadError {
    ControlPlane(ControlPlaneError),
    ByteStore(ByteStoreError),
    Manifest(bowline_storage::ManifestError),
    UnsafePath(String),
    UnsafeManifest(&'static str),
    SnapshotManifestMissing(String),
}

impl fmt::Display for DownloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ControlPlane(error) => error.fmt(formatter),
            Self::ByteStore(error) => error.fmt(formatter),
            Self::Manifest(error) => error.fmt(formatter),
            Self::UnsafePath(path) => write!(formatter, "remote manifest path `{path}` is unsafe"),
            Self::UnsafeManifest(reason) => {
                write!(formatter, "remote manifest is unsafe: {reason}")
            }
            Self::SnapshotManifestMissing(snapshot_id) => {
                write!(formatter, "snapshot manifest `{snapshot_id}` was not found")
            }
        }
    }
}

impl Error for DownloadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ControlPlane(error) => Some(error),
            Self::ByteStore(error) => Some(error),
            Self::Manifest(error) => Some(error),
            Self::UnsafePath(_) | Self::UnsafeManifest(_) | Self::SnapshotManifestMissing(_) => {
                None
            }
        }
    }
}

impl From<ControlPlaneError> for DownloadError {
    fn from(error: ControlPlaneError) -> Self {
        Self::ControlPlane(error)
    }
}

impl From<ByteStoreError> for DownloadError {
    fn from(error: ByteStoreError) -> Self {
        Self::ByteStore(error)
    }
}

impl From<bowline_storage::ManifestError> for DownloadError {
    fn from(error: bowline_storage::ManifestError) -> Self {
        Self::Manifest(error)
    }
}
