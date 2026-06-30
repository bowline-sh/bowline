use std::{collections::BTreeMap, error::Error, fmt};

use bowline_control_plane::WorkspaceRef as RemoteWorkspaceRef;
use bowline_core::{
    ids::{ContentId, ManifestId, SnapshotId, WorkspaceId},
    workspace_graph::SnapshotManifest,
};

pub mod coalescer;
pub mod conflicts;
pub mod download;
pub(crate) mod line_merge;
pub mod merge;
pub mod runner;
pub mod upload;

pub use coalescer::{
    CoalesceError, CoalesceExclusions, SnapshotCandidate, coalesce_workspace_scan,
};
pub use conflicts::{
    ConflictActiveView, ConflictBundle, ConflictBundleError, ConflictFile, ConflictKind,
    ConflictRecord, ConflictSide, ConflictSpan, create_conflict_bundle, unresolved_conflict_paths,
};
pub use download::{
    DownloadError, ImportedSnapshot, import_snapshot_by_id, import_snapshot_manifest,
};
pub use merge::{MergeError, MergeOutcome, merge_snapshots};
pub use runner::{SyncRunner, SyncRunnerError, SyncRunnerOptions, SyncTickOutcome};
pub use upload::{
    UploadError, UploadOutcome, upload_snapshot_candidate,
    upload_snapshot_candidate_with_checkpoints,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotContent {
    pub manifest: SnapshotManifest,
    pub files: BTreeMap<ContentId, Vec<u8>>,
}

impl SnapshotContent {
    pub fn new(manifest: SnapshotManifest, files: BTreeMap<ContentId, Vec<u8>>) -> Self {
        Self { manifest, files }
    }

    pub fn file_bytes_for_path(&self, path: &str) -> Option<&[u8]> {
        let content_id = self
            .manifest
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .and_then(|entry| entry.content_id.as_ref())?;
        self.files.get(content_id).map(Vec::as_slice)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateBase {
    pub workspace_id: WorkspaceId,
    pub version: u64,
    pub snapshot_id: SnapshotId,
}

impl CandidateBase {
    pub fn from_remote(remote: &RemoteWorkspaceRef) -> Self {
        Self {
            workspace_id: WorkspaceId::new(remote.workspace_id.clone()),
            version: remote.version,
            snapshot_id: SnapshotId::new(remote.snapshot_id.clone()),
        }
    }
}

pub fn snapshot_id_from_hash(
    prefix: &str,
    parts: impl IntoIterator<Item = impl AsRef<[u8]>>,
) -> SnapshotId {
    SnapshotId::new(format!("{prefix}_{}", short_hash(parts)))
}

pub fn manifest_id_for_snapshot(snapshot_id: &SnapshotId) -> ManifestId {
    ManifestId::new(format!(
        "mf_{}",
        short_hash([snapshot_id.as_str().as_bytes()])
    ))
}

pub(crate) fn short_hash(parts: impl IntoIterator<Item = impl AsRef<[u8]>>) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        let part = part.as_ref();
        hasher.update(&(part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    hasher.finalize().to_hex()[..24].to_string()
}

#[derive(Debug)]
pub enum SyncError {
    Coalesce(CoalesceError),
    Upload(UploadError),
    Download(DownloadError),
    Merge(MergeError),
    ConflictBundle(ConflictBundleError),
}

impl fmt::Display for SyncError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Coalesce(error) => error.fmt(formatter),
            Self::Upload(error) => error.fmt(formatter),
            Self::Download(error) => error.fmt(formatter),
            Self::Merge(error) => error.fmt(formatter),
            Self::ConflictBundle(error) => error.fmt(formatter),
        }
    }
}

impl Error for SyncError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Coalesce(error) => Some(error),
            Self::Upload(error) => Some(error),
            Self::Download(error) => Some(error),
            Self::Merge(error) => Some(error),
            Self::ConflictBundle(error) => Some(error),
        }
    }
}
