use std::{
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use super::line_merge::merge_utf8_lines;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictRecord {
    pub id: String,
    #[serde(rename = "conflictKind", default = "default_conflict_kind")]
    pub conflict_kind: ConflictKind,
    pub paths: Vec<String>,
    pub reason: String,
    pub active_view: ConflictActiveView,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spans: Vec<ConflictSpan>,
    pub bundle_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_root: Option<PathBuf>,
    #[serde(
        rename = "baseSnapshotId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub base_snapshot_id: Option<String>,
    #[serde(
        rename = "remoteSnapshotId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub remote_snapshot_id: Option<String>,
    pub contains_secrets: bool,
    pub state: String,
    #[serde(
        rename = "remoteConflictPublishedAt",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub remote_conflict_published_at: Option<String>,
    #[serde(
        rename = "remoteResolutionSyncedAt",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub remote_resolution_synced_at: Option<String>,
}

impl ConflictRecord {
    pub fn same_path(path: &str) -> Self {
        Self::new(
            path,
            ConflictKind::Text,
            "same-path edit could not be merged safely",
        )
    }

    pub fn same_path_span(path: &str, span: ConflictSpan) -> Self {
        let mut record = Self::same_path(path);
        record.spans = vec![span];
        record
    }

    pub fn structured(path: &str) -> Self {
        Self::new(
            path,
            ConflictKind::StructuredText,
            "structured text merge did not validate",
        )
    }

    pub fn binary(path: &str) -> Self {
        Self::new(path, ConflictKind::Binary, "binary file conflict")
    }

    pub fn delete_edit(path: &str) -> Self {
        Self::new(
            path,
            ConflictKind::DeleteEdit,
            "delete-versus-edit conflict",
        )
    }

    pub fn path_conflict(path: &str) -> Self {
        Self::new(path, ConflictKind::PathShape, "path kind conflict")
    }

    pub fn opaque_git(path: &str) -> Self {
        Self::new(path, ConflictKind::OpaqueGit, "opaque Git state conflict")
    }

    pub fn env_key(path: &str) -> Self {
        Self::new(path, ConflictKind::EnvKey, "environment key conflict")
    }

    fn new(path: &str, conflict_kind: ConflictKind, reason: &str) -> Self {
        let id = format!(
            "conflict_{}",
            super::short_hash([path.as_bytes(), reason.as_bytes()])
        );
        Self {
            id,
            conflict_kind,
            paths: vec![path.to_string()],
            reason: reason.to_string(),
            active_view: ConflictActiveView::Local,
            spans: Vec::new(),
            bundle_path: None,
            workspace_root: None,
            base_snapshot_id: None,
            remote_snapshot_id: None,
            contains_secrets: is_secret_bearing_path(path),
            state: "unresolved".to_string(),
            remote_conflict_published_at: None,
            remote_resolution_synced_at: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConflictKind {
    Text,
    StructuredText,
    Binary,
    OpaqueGit,
    DeleteEdit,
    PathShape,
    EnvKey,
}

fn default_conflict_kind() -> ConflictKind {
    ConflictKind::Text
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictSpan {
    pub path: String,
    pub base_start_line: u32,
    pub base_end_line: u32,
    pub local_start_line: u32,
    pub local_end_line: u32,
    pub remote_start_line: u32,
    pub remote_end_line: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_context_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_context_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_context_hash: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConflictActiveView {
    Local,
    Remote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictSide {
    Base,
    Local,
    Remote,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictFile {
    pub relative_path: String,
    pub base: Option<Vec<u8>>,
    pub local: Option<Vec<u8>>,
    pub remote: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictBundle {
    pub record: ConflictRecord,
    pub root: PathBuf,
    pub prompt_path: PathBuf,
    pub resolution_root: PathBuf,
}

pub fn create_conflict_bundle(
    state_root: &Path,
    mut record: ConflictRecord,
    files: &[ConflictFile],
) -> Result<ConflictBundle, ConflictBundleError> {
    let root = state_root.join("conflicts").join(&record.id);
    let base_root = root.join("base");
    let local_root = root.join("local");
    let remote_root = root.join("remote");
    let resolution_root = root.join("resolution");
    for directory in [&base_root, &local_root, &remote_root, &resolution_root] {
        fs::create_dir_all(directory)?;
        set_owner_only(directory)?;
    }
    for file in files {
        write_side(&base_root, &file.relative_path, file.base.as_deref())?;
        write_side(&local_root, &file.relative_path, file.local.as_deref())?;
        write_side(&remote_root, &file.relative_path, file.remote.as_deref())?;
    }
    record.contains_secrets = record.contains_secrets
        || record.paths.iter().any(|path| is_secret_bearing_path(path))
        || files
            .iter()
            .any(|file| is_secret_bearing_path(&file.relative_path));
    record.bundle_path = Some(root.clone());
    let manifest_path = root.join("manifest.json");
    atomic_write_private(&manifest_path, &serde_json::to_vec_pretty(&record)?)?;
    let prompt_path = root.join("prompt.md");
    atomic_write_private(
        &prompt_path,
        prompt_for(&record, &resolution_root).as_bytes(),
    )?;
    Ok(ConflictBundle {
        record,
        root,
        prompt_path,
        resolution_root,
    })
}

fn is_secret_bearing_path(path: &str) -> bool {
    path.split('/')
        .any(|part| part == ".env" || part.starts_with(".env.") || part.ends_with(".env"))
}

pub fn unresolved_conflict_paths(
    state_root: &Path,
) -> Result<BTreeSet<String>, ConflictBundleError> {
    let mut paths = BTreeSet::new();
    let conflicts_root = state_root.join("conflicts");
    let entries = match fs::read_dir(conflicts_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(paths),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let manifest_path = entry.path().join("manifest.json");
        let manifest = match fs::read(&manifest_path) {
            Ok(manifest) => manifest,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let record: ConflictRecord = serde_json::from_slice(&manifest)?;
        if record.state != "unresolved" {
            continue;
        }
        for path in record.paths {
            validate_bundle_relative_path(&path)?;
            paths.insert(path);
        }
    }
    Ok(paths)
}

pub(crate) fn unresolved_conflict_upload_overrides(
    state_root: &Path,
    workspace_root: &Path,
) -> Result<BTreeMap<String, Vec<u8>>, ConflictBundleError> {
    let mut overrides = BTreeMap::new();
    for record in unresolved_conflict_records(state_root)? {
        let Some(path) = continuation_override_path(&record) else {
            continue;
        };
        let root = record
            .bundle_path
            .clone()
            .unwrap_or_else(|| state_root.join("conflicts").join(&record.id));
        let local_recorded = match read_side_bytes(&root, ConflictSide::Local, path)? {
            Some(bytes) => bytes,
            None => continue,
        };
        let remote_recorded = match read_side_bytes(&root, ConflictSide::Remote, path)? {
            Some(bytes) => bytes,
            None => continue,
        };
        let live = match fs::read(workspace_root.join(path)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let Some(merged) = merge_utf8_lines(&local_recorded, &live, &remote_recorded) else {
            continue;
        };
        match overrides.entry(path.to_string()) {
            Entry::Vacant(slot) => {
                slot.insert(merged);
            }
            Entry::Occupied(slot) => {
                slot.remove_entry();
            }
        }
    }
    Ok(overrides)
}

pub(crate) fn resolved_conflict_records(
    state_root: &Path,
) -> Result<Vec<ConflictRecord>, ConflictBundleError> {
    let mut records = Vec::new();
    let conflicts_root = state_root.join("conflicts");
    let entries = match fs::read_dir(conflicts_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(records),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let manifest_path = entry.path().join("manifest.json");
        let manifest = match fs::read(&manifest_path) {
            Ok(manifest) => manifest,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let mut record: ConflictRecord = serde_json::from_slice(&manifest)?;
        if record.state != "accepted" && record.state != "rejected" {
            continue;
        }
        if record.remote_resolution_synced_at.is_some() {
            continue;
        }
        if record.bundle_path.is_none() {
            record.bundle_path = Some(entry.path());
        }
        records.push(record);
    }
    Ok(records)
}

pub(crate) fn unpublished_unresolved_conflict_records(
    state_root: &Path,
) -> Result<Vec<ConflictRecord>, ConflictBundleError> {
    Ok(unresolved_conflict_records(state_root)?
        .into_iter()
        .filter(|record| record.remote_conflict_published_at.is_none())
        .collect())
}

pub(crate) fn mark_conflict_remote_metadata_published(
    record: &ConflictRecord,
    published_at: &str,
) -> Result<(), ConflictBundleError> {
    update_manifest_string_field(record, "remoteConflictPublishedAt", published_at)
}

pub(crate) fn mark_conflict_remote_resolution_synced(
    record: &ConflictRecord,
    synced_at: &str,
) -> Result<(), ConflictBundleError> {
    update_manifest_string_field(record, "remoteResolutionSyncedAt", synced_at)
}

fn update_manifest_string_field(
    record: &ConflictRecord,
    field: &str,
    value: &str,
) -> Result<(), ConflictBundleError> {
    let root = record
        .bundle_path
        .clone()
        .ok_or_else(|| ConflictBundleError::UnsafePath(record.id.clone()))?;
    let manifest_path = root.join("manifest.json");
    let mut manifest: serde_json::Value = serde_json::from_slice(&fs::read(&manifest_path)?)?;
    let object = manifest
        .as_object_mut()
        .ok_or_else(|| ConflictBundleError::UnsafePath(record.id.clone()))?;
    object.insert(
        field.to_string(),
        serde_json::Value::String(value.to_string()),
    );
    atomic_write_private(&manifest_path, &serde_json::to_vec_pretty(&manifest)?)?;
    Ok(())
}

fn unresolved_conflict_records(
    state_root: &Path,
) -> Result<Vec<ConflictRecord>, ConflictBundleError> {
    let mut records = Vec::new();
    let conflicts_root = state_root.join("conflicts");
    let entries = match fs::read_dir(conflicts_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(records),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let manifest_path = entry.path().join("manifest.json");
        let manifest = match fs::read(&manifest_path) {
            Ok(manifest) => manifest,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let mut record: ConflictRecord = serde_json::from_slice(&manifest)?;
        if record.state != "unresolved" {
            continue;
        }
        if record.bundle_path.is_none() {
            record.bundle_path = Some(entry.path());
        }
        records.push(record);
    }
    Ok(records)
}

fn continuation_override_path(record: &ConflictRecord) -> Option<&str> {
    if record.conflict_kind != ConflictKind::Text
        || record.paths.len() != 1
        || record.spans.is_empty()
    {
        return None;
    }
    let path = record.paths.first()?.as_str();
    if record.spans.iter().all(|span| span.path == path) {
        Some(path)
    } else {
        None
    }
}

fn read_side_bytes(
    root: &Path,
    side: ConflictSide,
    relative_path: &str,
) -> Result<Option<Vec<u8>>, ConflictBundleError> {
    validate_bundle_relative_path(relative_path)?;
    let side_dir = match side {
        ConflictSide::Base => "base",
        ConflictSide::Local => "local",
        ConflictSide::Remote => "remote",
    };
    match fs::read(root.join(side_dir).join(relative_path)) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn write_side(
    root: &Path,
    relative_path: &str,
    bytes: Option<&[u8]>,
) -> Result<(), ConflictBundleError> {
    let Some(bytes) = bytes else {
        return Ok(());
    };
    validate_bundle_relative_path(relative_path)?;
    let path = root.join(relative_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        set_owner_only(parent)?;
    }
    atomic_write_private(&path, bytes)
}

fn prompt_for(record: &ConflictRecord, resolution_root: &Path) -> String {
    format!(
        "You are helping resolve a bowline sync conflict.\n\nConflict: {}\nFiles: {}\n\nBundle layout:\n- base/ has the common ancestor bytes.\n- local/ has this device's active view.\n- remote/ has the current workspace head.\n- resolution/ is the only place you may write proposed fixes.\n\nDo not run Git, stage, commit, push, publish, or mutate source control. Do not copy secret values into your response. Write only under `{}` and explain the proposed resolution.\n",
        record.id,
        record.paths.join(", "),
        resolution_root.display()
    )
}

fn validate_bundle_relative_path(relative_path: &str) -> Result<(), ConflictBundleError> {
    let normalized = bowline_core::workspace_graph::normalize_workspace_path(relative_path);
    if normalized != relative_path
        || normalized.is_empty()
        || normalized.starts_with("../")
        || normalized.contains("/../")
        || normalized == "."
    {
        return Err(ConflictBundleError::UnsafePath(relative_path.to_string()));
    }
    Ok(())
}

fn atomic_write_private(path: &Path, bytes: &[u8]) -> Result<(), ConflictBundleError> {
    use std::io::Write;

    let temp = path.with_extension("tmp");
    let mut file = create_private_file(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    set_owner_only(&temp)?;
    fs::rename(temp, path)?;
    Ok(())
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> Result<fs::File, ConflictBundleError> {
    use std::os::unix::fs::OpenOptionsExt;

    Ok(fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?)
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> Result<fs::File, ConflictBundleError> {
    Ok(fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?)
}

#[cfg(unix)]
fn set_owner_only(path: &Path) -> Result<(), ConflictBundleError> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if path.is_dir() { 0o700 } else { 0o600 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_owner_only(_path: &Path) -> Result<(), ConflictBundleError> {
    Ok(())
}

#[derive(Debug)]
pub enum ConflictBundleError {
    Io(std::io::Error),
    Json(serde_json::Error),
    UnsafePath(String),
}

impl fmt::Display for ConflictBundleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "conflict bundle I/O failed: {error}"),
            Self::Json(error) => write!(formatter, "conflict bundle JSON failed: {error}"),
            Self::UnsafePath(path) => {
                write!(formatter, "conflict bundle path `{path}` is unsafe")
            }
        }
    }
}

impl Error for ConflictBundleError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::UnsafePath(_) => None,
        }
    }
}

impl From<std::io::Error> for ConflictBundleError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for ConflictBundleError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}
