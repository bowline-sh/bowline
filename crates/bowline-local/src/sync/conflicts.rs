use std::{
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
};

use bowline_control_plane::ObjectPointer;
use bowline_core::fs_atomic::{AtomicWriteOptions, write_atomic};
use bowline_core::ids::ContentId;
use serde::{Deserialize, Serialize};

use super::line_merge::{TextMergeOutcome, merge_text_lines};
use super::paths::is_secret_bearing_path;

const STATUS_REVISION_FILE: &str = ".status-revision";

mod record_lock;

use record_lock::ConflictRecordLock;
pub(crate) use record_lock::{ConflictStatusRevision, conflict_status_revision};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictRecord {
    pub id: String,
    #[serde(rename = "conflictKind")]
    pub conflict_kind: ConflictKind,
    pub occurrence_version: u64,
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
    #[serde(
        rename = "bundleObject",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub bundle_object: Option<ObjectPointer>,
    pub contains_secrets: bool,
    pub state: ConflictState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accepted_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejected_at: Option<String>,
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

    pub fn text_merge_span(path: &str, reason_code: &str, span: ConflictSpan) -> Self {
        let mut record = Self::new(
            path,
            ConflictKind::Text,
            &format!("text merge failed: {reason_code}"),
        );
        record.spans = vec![span];
        record
    }

    pub fn binary_text_merge(path: &str, reason_code: &str) -> Self {
        Self::new(
            path,
            ConflictKind::Binary,
            &format!("text classification failed: {reason_code}"),
        )
    }

    pub fn env_text_merge(path: &str, reason_code: &str) -> Self {
        Self::new(
            path,
            ConflictKind::EnvKey,
            &format!("environment text merge failed: {reason_code}"),
        )
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

    pub fn merge_plugin(path: &str, reason: &str) -> Self {
        Self::new(path, ConflictKind::MergePlugin, reason)
    }

    fn new(path: &str, conflict_kind: ConflictKind, reason: &str) -> Self {
        let id = format!(
            "conflict_{}",
            super::short_hash([path.as_bytes(), reason.as_bytes()])
        );
        Self {
            id,
            conflict_kind,
            occurrence_version: 1,
            paths: vec![path.to_string()],
            reason: reason.to_string(),
            active_view: ConflictActiveView::Local,
            spans: Vec::new(),
            bundle_path: None,
            workspace_root: None,
            base_snapshot_id: None,
            remote_snapshot_id: None,
            bundle_object: None,
            contains_secrets: is_secret_bearing_path(path),
            state: ConflictState::Unresolved,
            accepted_at: None,
            rejected_at: None,
            remote_conflict_published_at: None,
            remote_resolution_synced_at: None,
        }
    }
}

pub fn conflict_bundle_object_id(record: &ConflictRecord) -> ContentId {
    ContentId::new(format!("{}{:016x}", record.id, record.occurrence_version))
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
    MergePlugin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConflictState {
    Unresolved,
    Accepted,
    Rejected,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictFile {
    pub relative_path: String,
    pub base: Option<Vec<u8>>,
    pub local: Option<Vec<u8>>,
    pub remote: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConflictBundlePayload {
    pub record: ConflictRecord,
    pub files: Vec<ConflictFile>,
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
    let conflicts_root = state_root.join("conflicts");
    let _lock = ConflictRecordLock::acquire(&conflicts_root, &record.id)?;
    let root = conflicts_root.join(&record.id);
    bind_conflict_occurrence_version(&root, &mut record)?;
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
    advance_conflict_status_revision(state_root)?;
    Ok(ConflictBundle {
        record,
        root,
        prompt_path,
        resolution_root,
    })
}

fn bind_conflict_occurrence_version(
    root: &Path,
    record: &mut ConflictRecord,
) -> Result<(), ConflictBundleError> {
    let manifest_path = root.join("manifest.json");
    let previous = match fs::read(&manifest_path) {
        Ok(bytes) => Some(decode_persisted_conflict_record(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    record.occurrence_version = match previous {
        Some(previous) if same_conflict_occurrence_identity(&previous, record) => {
            previous.occurrence_version
        }
        Some(previous) => previous.occurrence_version.checked_add(1).ok_or_else(|| {
            ConflictBundleError::UnsafePath(format!(
                "conflict occurrence version exhausted for {}",
                record.id
            ))
        })?,
        None => 1,
    };
    Ok(())
}

fn same_conflict_occurrence_identity(left: &ConflictRecord, right: &ConflictRecord) -> bool {
    left.id == right.id
        && left.conflict_kind == right.conflict_kind
        && left.paths == right.paths
        && left.reason == right.reason
        && left.base_snapshot_id == right.base_snapshot_id
        && left.remote_snapshot_id == right.remote_snapshot_id
}

pub fn set_conflict_bundle_object(
    record: &ConflictRecord,
    bundle_object: ObjectPointer,
) -> Result<bool, ConflictBundleError> {
    let root = record
        .bundle_path
        .clone()
        .ok_or_else(|| ConflictBundleError::UnsafePath(record.id.clone()))?;
    let conflicts_root = root
        .parent()
        .ok_or_else(|| ConflictBundleError::UnsafePath(record.id.clone()))?;
    let _lock = ConflictRecordLock::acquire(conflicts_root, &record.id)?;
    let Some(mut current) = load_conflict_record_from_root(&root)? else {
        return Ok(false);
    };
    if current.occurrence_version != record.occurrence_version
        || !same_conflict_occurrence_identity(&current, record)
    {
        return Ok(false);
    }
    current.bundle_object = Some(bundle_object);
    persist_conflict_record_manifest_locked(&current, &root)?;
    Ok(true)
}

fn persist_conflict_record_manifest_locked(
    record: &ConflictRecord,
    root: &Path,
) -> Result<(), ConflictBundleError> {
    atomic_write_private(
        &root.join("manifest.json"),
        &serde_json::to_vec_pretty(record)?,
    )?;
    let state_root = root
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| ConflictBundleError::UnsafePath(root.display().to_string()))?;
    advance_conflict_status_revision(state_root)
}

pub fn unresolved_conflict_paths(
    state_root: &Path,
) -> Result<BTreeSet<String>, ConflictBundleError> {
    let mut paths = BTreeSet::new();
    for record in load_conflict_records(state_root)? {
        if record.state != ConflictState::Unresolved {
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
        let merged = match merge_text_lines(&local_recorded, &live, &remote_recorded) {
            TextMergeOutcome::Clean { bytes, .. } => bytes,
            TextMergeOutcome::Conflict { .. }
            | TextMergeOutcome::NotText { .. }
            | TextMergeOutcome::ResourceLimit { .. }
            | TextMergeOutcome::InternalError { .. } => continue,
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

pub fn conflict_occurrence_is_current(
    state_root: &Path,
    conflict_id: &str,
    occurrence_version: u64,
    desired_remote_state: ConflictState,
) -> Result<bool, ConflictBundleError> {
    Ok(
        load_conflict_record(state_root, conflict_id)?.is_some_and(|record| {
            record.occurrence_version == occurrence_version
                && reconcile_step_is_pending(&record, desired_remote_state)
        }),
    )
}

pub fn mark_conflict_occurrence_reconciled(
    state_root: &Path,
    conflict_id: &str,
    occurrence_version: u64,
    desired_remote_state: ConflictState,
    reconciled_at: &str,
) -> Result<bool, ConflictBundleError> {
    mark_conflict_occurrence_reconciled_inner(
        state_root,
        conflict_id,
        occurrence_version,
        desired_remote_state,
        reconciled_at,
        || {},
    )
}

pub fn transition_conflict_occurrence_state(
    bundle_root: &Path,
    conflict_id: &str,
    occurrence_version: u64,
    desired_state: ConflictState,
    transitioned_at: &str,
) -> Result<bool, ConflictBundleError> {
    if desired_state == ConflictState::Unresolved {
        return Ok(false);
    }
    let conflicts_root = bundle_root
        .parent()
        .ok_or_else(|| ConflictBundleError::UnsafePath(conflict_id.to_string()))?;
    let _lock = ConflictRecordLock::acquire(conflicts_root, conflict_id)?;
    let Some(mut record) = load_conflict_record_from_root(bundle_root)? else {
        return Ok(false);
    };
    if record.id != conflict_id
        || record.occurrence_version != occurrence_version
        || record.state != ConflictState::Unresolved
    {
        return Ok(false);
    }
    record.state = desired_state;
    match desired_state {
        ConflictState::Accepted => record.accepted_at = Some(transitioned_at.to_string()),
        ConflictState::Rejected => record.rejected_at = Some(transitioned_at.to_string()),
        ConflictState::Unresolved => return Ok(false),
    }
    persist_conflict_record_manifest_locked(&record, bundle_root)?;
    Ok(true)
}

fn mark_conflict_occurrence_reconciled_inner(
    state_root: &Path,
    conflict_id: &str,
    occurrence_version: u64,
    desired_remote_state: ConflictState,
    reconciled_at: &str,
    before_write: impl FnOnce(),
) -> Result<bool, ConflictBundleError> {
    let conflicts_root = state_root.join("conflicts");
    let _lock = ConflictRecordLock::acquire(&conflicts_root, conflict_id)?;
    let Some(mut record) = load_conflict_record(state_root, conflict_id)? else {
        return Ok(false);
    };
    if record.occurrence_version != occurrence_version
        || !reconcile_step_is_pending(&record, desired_remote_state)
    {
        return Ok(false);
    }
    match desired_remote_state {
        ConflictState::Unresolved => {
            record.remote_conflict_published_at = Some(reconciled_at.to_string());
        }
        ConflictState::Accepted | ConflictState::Rejected => {
            record.remote_resolution_synced_at = Some(reconciled_at.to_string());
        }
    }
    before_write();
    persist_conflict_record_manifest_locked(&record, &conflicts_root.join(conflict_id))?;
    Ok(true)
}

fn reconcile_step_is_pending(record: &ConflictRecord, desired_remote_state: ConflictState) -> bool {
    match desired_remote_state {
        ConflictState::Unresolved => record.remote_conflict_published_at.is_none(),
        ConflictState::Accepted | ConflictState::Rejected => {
            record.remote_conflict_published_at.is_some()
                && record.remote_resolution_synced_at.is_none()
                && record.state == desired_remote_state
        }
    }
}

fn advance_conflict_status_revision(state_root: &Path) -> Result<(), ConflictBundleError> {
    let conflicts_root = state_root.join("conflicts");
    fs::create_dir_all(&conflicts_root)?;
    set_owner_only(&conflicts_root)?;
    let revision = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_string();
    atomic_write_private(
        &conflicts_root.join(STATUS_REVISION_FILE),
        revision.as_bytes(),
    )
}

fn unresolved_conflict_records(
    state_root: &Path,
) -> Result<Vec<ConflictRecord>, ConflictBundleError> {
    Ok(load_conflict_records(state_root)?
        .into_iter()
        .filter(|record| record.state == ConflictState::Unresolved)
        .collect())
}

pub fn load_conflict_records(
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
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let manifest_path = entry.path().join("manifest.json");
        let manifest = match fs::read(&manifest_path) {
            Ok(manifest) => manifest,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let mut record = decode_persisted_conflict_record(&manifest)?;
        if record.bundle_path.is_none() {
            record.bundle_path = Some(entry.path());
        }
        records.push(record);
    }
    Ok(records)
}

pub fn load_conflict_record(
    state_root: &Path,
    conflict_id: &str,
) -> Result<Option<ConflictRecord>, ConflictBundleError> {
    load_conflict_record_from_root(&state_root.join("conflicts").join(conflict_id))
}

pub(crate) fn load_conflict_files(
    record: &ConflictRecord,
) -> Result<Vec<ConflictFile>, ConflictBundleError> {
    let root = record
        .bundle_path
        .clone()
        .ok_or_else(|| ConflictBundleError::UnsafePath(record.id.clone()))?;
    record
        .paths
        .iter()
        .map(|relative_path| {
            Ok(ConflictFile {
                relative_path: relative_path.clone(),
                base: read_side_bytes(&root, ConflictSide::Base, relative_path)?,
                local: read_side_bytes(&root, ConflictSide::Local, relative_path)?,
                remote: read_side_bytes(&root, ConflictSide::Remote, relative_path)?,
            })
        })
        .collect()
}

fn load_conflict_record_from_root(
    bundle_root: &Path,
) -> Result<Option<ConflictRecord>, ConflictBundleError> {
    let manifest_path = bundle_root.join("manifest.json");
    match fs::read(manifest_path) {
        Ok(bytes) => Ok(Some(decode_persisted_conflict_record(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn decode_persisted_conflict_record(bytes: &[u8]) -> Result<ConflictRecord, ConflictBundleError> {
    let record: ConflictRecord = serde_json::from_slice(bytes)?;
    if record.occurrence_version == 0 {
        return Err(ConflictBundleError::InvalidOccurrenceVersion {
            conflict_id: record.id,
        });
    }
    Ok(record)
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
    validate_bundle_relative_path(relative_path)?;
    let path = root.join(relative_path);
    let Some(bytes) = bytes else {
        return match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        };
    };
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
    write_atomic(
        path,
        bytes,
        AtomicWriteOptions {
            unix_mode: Some(0o600),
            reject_symlink: false,
            replace_existing: true,
        },
    )?;
    Ok(())
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
    MissingOccurrenceField {
        conflict_id: String,
        field: &'static str,
    },
    OccurrenceSuperseded {
        conflict_id: String,
        occurrence_version: u64,
    },
    InvalidOccurrenceVersion {
        conflict_id: String,
    },
    RecordLockTimeout {
        conflict_id: String,
    },
    UnsafePath(String),
}

impl fmt::Display for ConflictBundleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "conflict bundle I/O failed: {error}"),
            Self::Json(error) => write!(formatter, "conflict bundle JSON failed: {error}"),
            Self::MissingOccurrenceField { conflict_id, field } => write!(
                formatter,
                "conflict occurrence `{conflict_id}` is missing required field `{field}`"
            ),
            Self::OccurrenceSuperseded {
                conflict_id,
                occurrence_version,
            } => write!(
                formatter,
                "conflict occurrence `{conflict_id}` version {occurrence_version} was superseded"
            ),
            Self::InvalidOccurrenceVersion { conflict_id } => write!(
                formatter,
                "conflict occurrence `{conflict_id}` has invalid occurrence version zero"
            ),
            Self::RecordLockTimeout { conflict_id } => write!(
                formatter,
                "conflict occurrence `{conflict_id}` record lock timed out"
            ),
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
            Self::MissingOccurrenceField { .. }
            | Self::OccurrenceSuperseded { .. }
            | Self::InvalidOccurrenceVersion { .. }
            | Self::RecordLockTimeout { .. }
            | Self::UnsafePath(_) => None,
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

#[cfg(test)]
mod tests;
