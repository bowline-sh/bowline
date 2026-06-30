use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
};

use bowline_control_plane::WorkspaceRef as RemoteWorkspaceRef;
use bowline_core::{
    ids::{ContentId, DeviceId, ProjectId, WorkspaceId},
    policy::MaterializationMode,
    workspace_graph::{
        HydrationState, NamespaceEntry, NamespaceEntryKind, RefKind, SnapshotKind,
        SnapshotManifest, WorkspaceRef, normalize_workspace_path, workspace_content_id,
    },
};

use crate::scanner::{ScanError, ScanReport, scan_workspace};

use super::{CandidateBase, SnapshotContent, manifest_id_for_snapshot, snapshot_id_from_hash};

const DEFAULT_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotCandidate {
    pub base: CandidateBase,
    pub device_id: DeviceId,
    pub manifest_id: bowline_core::ids::ManifestId,
    pub snapshot: SnapshotContent,
    pub scan_report: ScanReport,
    pub causation_ids: Vec<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy)]
pub struct CoalesceExclusions<'a> {
    pub paths: &'a BTreeSet<String>,
    pub preserved_entries: &'a [NamespaceEntry],
    pub file_overrides: &'a BTreeMap<String, Vec<u8>>,
}

impl<'a> CoalesceExclusions<'a> {
    fn empty() -> Self {
        Self {
            paths: &EMPTY_PATH_SET,
            preserved_entries: &[],
            file_overrides: &EMPTY_FILE_OVERRIDES,
        }
    }
}

static EMPTY_PATH_SET: BTreeSet<String> = BTreeSet::new();
static EMPTY_FILE_OVERRIDES: BTreeMap<String, Vec<u8>> = BTreeMap::new();

pub fn coalesce_workspace_scan(
    root: &Path,
    workspace_id: WorkspaceId,
    base_ref: &RemoteWorkspaceRef,
    device_id: DeviceId,
    workspace_content_key: [u8; 32],
    created_at: impl Into<String>,
) -> Result<SnapshotCandidate, CoalesceError> {
    coalesce_workspace_scan_excluding(
        root,
        workspace_id,
        base_ref,
        device_id,
        workspace_content_key,
        created_at,
        CoalesceExclusions::empty(),
    )
}

pub fn coalesce_workspace_scan_excluding(
    root: &Path,
    workspace_id: WorkspaceId,
    base_ref: &RemoteWorkspaceRef,
    device_id: DeviceId,
    workspace_content_key: [u8; 32],
    created_at: impl Into<String>,
    exclusions: CoalesceExclusions<'_>,
) -> Result<SnapshotCandidate, CoalesceError> {
    let report = scan_workspace(root)?;
    coalesce_workspace_report(
        root,
        report,
        workspace_id,
        base_ref,
        device_id,
        workspace_content_key,
        created_at,
        exclusions,
    )
}

#[allow(clippy::too_many_arguments)]
fn coalesce_workspace_report(
    root: &Path,
    report: ScanReport,
    workspace_id: WorkspaceId,
    base_ref: &RemoteWorkspaceRef,
    device_id: DeviceId,
    workspace_content_key: [u8; 32],
    created_at: impl Into<String>,
    exclusions: CoalesceExclusions<'_>,
) -> Result<SnapshotCandidate, CoalesceError> {
    let mut entries = Vec::new();
    let mut files = BTreeMap::<ContentId, Vec<u8>>::new();
    let created_at = created_at.into();
    let scan_report = report.clone();
    let mut hash_parts = vec![workspace_id.as_str().as_bytes().to_vec()];
    let mut observed_paths = BTreeSet::<String>::new();
    let mut observed_non_directory_paths = BTreeSet::<String>::new();

    for observed in report.paths {
        if !syncs_to_workspace_head(observed.policy.mode) {
            continue;
        }
        let path = normalize_workspace_path(&observed.path);
        if is_private_state_path(&path) {
            continue;
        }
        let override_bytes = exclusions.file_overrides.get(&path);
        if exclusions.paths.contains(&path) && override_bytes.is_none() {
            continue;
        }
        observed_paths.insert(path.clone());
        let (kind, content_id, bytes, byte_len, hydration_state, symlink_target) =
            if observed.is_dir {
                (
                    NamespaceEntryKind::Directory,
                    None,
                    None,
                    None,
                    HydrationState::StructureOnly,
                    None,
                )
            } else if observed.is_symlink {
                let target = match read_workspace_symlink_target(&root.join(&path)) {
                    Ok(target) => target,
                    Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(source) => {
                        return Err(CoalesceError::ReadPath {
                            path: path.clone(),
                            source,
                        });
                    }
                };
                if !is_safe_workspace_symlink_target(&target) {
                    continue;
                }
                let byte_len = Some(target.len() as u64);
                (
                    NamespaceEntryKind::Symlink,
                    None,
                    None,
                    byte_len,
                    HydrationState::Local,
                    Some(target),
                )
            } else {
                let absolute_path = root.join(&path);
                let bytes = if let Some(bytes) = override_bytes {
                    bytes.clone()
                } else {
                    match fs::read(&absolute_path) {
                        Ok(bytes) => bytes,
                        Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(source) => {
                            return Err(CoalesceError::ReadPath {
                                path: path.clone(),
                                source,
                            });
                        }
                    }
                };
                let byte_len = Some(bytes.len() as u64);
                let content_id = workspace_content_id(workspace_content_key, &bytes);
                (
                    NamespaceEntryKind::File,
                    Some(content_id),
                    Some(bytes),
                    byte_len,
                    HydrationState::Local,
                    None,
                )
            };
        if kind != NamespaceEntryKind::Directory {
            observed_non_directory_paths.insert(path.clone());
        }

        if let Some(bytes) = bytes {
            files.insert(content_id.clone().expect("file content id"), bytes);
        }
        entries.push(NamespaceEntry {
            path,
            kind,
            classification: observed.policy.classification,
            mode: observed.policy.mode,
            access: observed.policy.access,
            content_id,
            locator: None,
            symlink_target,
            byte_len,
            hydration_state,
        });
    }
    for entry in exclusions.preserved_entries {
        if has_observed_non_directory_ancestor(&entry.path, &observed_non_directory_paths) {
            continue;
        }
        let path_has_override = exclusions.file_overrides.contains_key(&entry.path);
        if (exclusions.paths.contains(&entry.path) && !path_has_override)
            || !observed_paths.contains(&entry.path)
        {
            entries.push(entry.clone());
        }
    }

    entries.sort_by(|left, right| left.path.cmp(&right.path));
    for entry in &entries {
        hash_parts.push(entry.path.as_bytes().to_vec());
        hash_parts.push(format!("{:?}", entry.kind).into_bytes());
        hash_parts.push(format!("{:?}", entry.classification).into_bytes());
        hash_parts.push(format!("{:?}", entry.mode).into_bytes());
        hash_parts.push(format!("{:?}", entry.access).into_bytes());
        hash_parts.push(
            entry
                .content_id
                .as_ref()
                .map(|content_id| content_id.as_str())
                .unwrap_or_default()
                .as_bytes()
                .to_vec(),
        );
        hash_parts.push(
            entry
                .symlink_target
                .as_deref()
                .unwrap_or_default()
                .as_bytes()
                .to_vec(),
        );
        hash_parts.push(format!("{:?}", entry.byte_len).into_bytes());
    }
    let snapshot_id = snapshot_id_from_hash("snap", hash_parts.iter());
    let manifest = SnapshotManifest {
        schema_version: DEFAULT_SCHEMA_VERSION,
        snapshot_id: snapshot_id.clone(),
        workspace_id: workspace_id.clone(),
        project_id: None::<ProjectId>,
        kind: SnapshotKind::WorkspaceHead,
        base_snapshot_id: Some(bowline_core::ids::SnapshotId::new(
            base_ref.snapshot_id.clone(),
        )),
        entries,
        refs: vec![WorkspaceRef {
            name: "workspace".to_string(),
            target_snapshot_id: snapshot_id.clone(),
            kind: RefKind::Workspace,
        }],
    };

    Ok(SnapshotCandidate {
        base: CandidateBase::from_remote(base_ref),
        device_id,
        manifest_id: manifest_id_for_snapshot(&snapshot_id),
        snapshot: SnapshotContent::new(manifest, files),
        scan_report,
        causation_ids: vec![format!("scan:{}", base_ref.version)],
        created_at,
    })
}

pub fn syncs_to_workspace_head(mode: MaterializationMode) -> bool {
    matches!(
        mode,
        MaterializationMode::WorkspaceSync
            | MaterializationMode::EncryptedSync
            | MaterializationMode::ProjectEnv
            | MaterializationMode::Lazy
    )
}

fn is_private_state_path(path: &str) -> bool {
    path == ".bowline"
        || path.starts_with(".bowline/")
        || path
            .split('/')
            .any(|part| part.starts_with(".bowline-materialize-") && part.ends_with(".tmp"))
}

fn has_observed_non_directory_ancestor(
    path: &str,
    observed_non_directory_paths: &BTreeSet<String>,
) -> bool {
    let mut current = path;
    while let Some((parent, _)) = current.rsplit_once('/') {
        if parent.is_empty() {
            break;
        }
        if observed_non_directory_paths.contains(parent) {
            return true;
        }
        current = parent;
    }
    false
}

fn read_workspace_symlink_target(path: &Path) -> Result<String, std::io::Error> {
    Ok(path_to_slash_string(&fs::read_link(path)?))
}

fn is_safe_workspace_symlink_target(target: &str) -> bool {
    let normalized = normalize_workspace_path(target);
    !Path::new(target).is_absolute()
        && normalized == target
        && !normalized.is_empty()
        && normalized != "."
        && !normalized.starts_with("../")
        && !normalized.contains("/../")
}

fn path_to_slash_string(path: &Path) -> String {
    path.components()
        .collect::<PathBuf>()
        .to_string_lossy()
        .replace('\\', "/")
}

#[derive(Debug)]
pub enum CoalesceError {
    Scan(ScanError),
    ReadPath {
        path: String,
        source: std::io::Error,
    },
}

impl fmt::Display for CoalesceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Scan(error) => error.fmt(formatter),
            Self::ReadPath { path, source } => {
                write!(formatter, "failed to read syncable path `{path}`: {source}")
            }
        }
    }
}

impl Error for CoalesceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Scan(error) => Some(error),
            Self::ReadPath { source, .. } => Some(source),
        }
    }
}

impl From<ScanError> for CoalesceError {
    fn from(error: ScanError) -> Self {
        Self::Scan(error)
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use bowline_control_plane::{ControlPlaneTimestamp, WorkspaceRef as RemoteWorkspaceRef};
    use bowline_core::{
        ids::{DeviceId, WorkspaceId},
        status::ObservedWorkspaceSummary,
    };

    use crate::{
        policy::explain_path_without_policy,
        scanner::{PathObservation, ScanReport},
        workspace::TempWorkspace,
    };

    use super::coalesce_workspace_report;

    #[test]
    fn coalescing_uses_read_bytes_for_file_length_when_writer_races_scan_metadata() {
        let workspace = TempWorkspace::new("coalesce-concurrent-writer").expect("workspace");
        let source_path = workspace.root().join("app/src/main.ts");
        fs::create_dir_all(source_path.parent().expect("source parent")).expect("source parent");
        fs::write(&source_path, b"export const value = 'writer won';\n").expect("source");

        let report = stale_file_report("app/src/main.ts", Some(1));
        let candidate = coalesce_workspace_report(
            workspace.root(),
            report,
            WorkspaceId::new("ws_code"),
            &base_ref(),
            DeviceId::new("device-test"),
            [11_u8; 32],
            "2026-06-26T16:45:00Z",
            super::CoalesceExclusions::empty(),
        )
        .expect("coalesce");
        let entry = candidate
            .snapshot
            .manifest
            .entries
            .iter()
            .find(|entry| entry.path == "app/src/main.ts")
            .expect("source entry");
        let bytes = candidate
            .snapshot
            .file_bytes_for_path("app/src/main.ts")
            .expect("source bytes");

        assert_eq!(bytes, b"export const value = 'writer won';\n");
        assert_eq!(entry.byte_len, Some(bytes.len() as u64));
    }

    #[test]
    fn coalescing_skips_paths_that_vanish_between_scan_and_read() {
        let workspace = TempWorkspace::new("coalesce-vanished-path").expect("workspace");
        fs::create_dir_all(workspace.root().join("app/src")).expect("source parent");
        let report = stale_file_report("app/src/main.ts", Some(128));

        let candidate = coalesce_workspace_report(
            workspace.root(),
            report,
            WorkspaceId::new("ws_code"),
            &base_ref(),
            DeviceId::new("device-test"),
            [12_u8; 32],
            "2026-06-26T16:45:00Z",
            super::CoalesceExclusions::empty(),
        )
        .expect("coalesce");

        assert!(
            candidate.snapshot.manifest.entries.is_empty(),
            "vanished observed paths should not fail or produce stale entries"
        );
        assert!(candidate.snapshot.files.is_empty());
    }

    #[test]
    fn coalescing_ignores_materialization_temp_files() {
        let workspace = TempWorkspace::new("coalesce-materialize-temp").expect("workspace");
        fs::create_dir_all(workspace.root().join("app/src")).expect("source parent");
        fs::write(
            workspace
                .root()
                .join("app/src/.bowline-materialize-index_ts-abcdef123456.tmp"),
            b"stale temp bytes\n",
        )
        .expect("temp file");
        let report = stale_file_report(
            "app/src/.bowline-materialize-index_ts-abcdef123456.tmp",
            Some(17),
        );

        let candidate = coalesce_workspace_report(
            workspace.root(),
            report,
            WorkspaceId::new("ws_code"),
            &base_ref(),
            DeviceId::new("device-test"),
            [13_u8; 32],
            "2026-06-26T16:45:00Z",
            super::CoalesceExclusions::empty(),
        )
        .expect("coalesce");

        assert!(candidate.snapshot.manifest.entries.is_empty());
        assert!(candidate.snapshot.files.is_empty());
    }

    fn stale_file_report(path: &str, byte_len: Option<u64>) -> ScanReport {
        ScanReport {
            root: PathBuf::from("/tmp/bowline-stale-report"),
            projects: Vec::new(),
            paths: vec![PathObservation {
                path: path.to_string(),
                project_id: None,
                is_dir: false,
                is_symlink: false,
                byte_len,
                policy: explain_path_without_policy(path),
            }],
            summary: ObservedWorkspaceSummary::default(),
        }
    }

    fn base_ref() -> RemoteWorkspaceRef {
        RemoteWorkspaceRef {
            workspace_id: "ws_code".to_string(),
            version: 7,
            snapshot_id: "snap_base".to_string(),
            updated_at: ControlPlaneTimestamp { tick: 7 },
            updated_by_device_id: Some("device-peer".to_string()),
        }
    }
}
