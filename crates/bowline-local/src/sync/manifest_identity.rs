use bowline_core::{
    ids::{ManifestDigest, SnapshotId, WorkspaceId},
    workspace_graph::{NamespaceEntry, SnapshotManifest},
};

use super::namespace::semantic_manifest_identity;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestIdentityReport {
    pub(crate) snapshot_id: SnapshotId,
    pub(crate) semantic_manifest_digest: ManifestDigest,
    pub(crate) entries_hashed: u64,
}

impl ManifestIdentityReport {
    pub fn snapshot_id(&self) -> &SnapshotId {
        &self.snapshot_id
    }

    pub fn semantic_manifest_digest(&self) -> &ManifestDigest {
        &self.semantic_manifest_digest
    }

    pub fn entries_hashed(&self) -> u64 {
        self.entries_hashed
    }
}

pub(crate) fn build_manifest_identity(
    workspace_id: &WorkspaceId,
    entries: &[NamespaceEntry],
    _created_at: &str,
) -> ManifestIdentityReport {
    let mut canonical_entries = entries.to_vec();
    canonical_entries.sort_by(|left, right| left.path.cmp(&right.path));
    let identity = semantic_manifest_identity(workspace_id, &canonical_entries);
    ManifestIdentityReport {
        snapshot_id: identity.snapshot_id().clone(),
        semantic_manifest_digest: identity.digest().clone(),
        entries_hashed: canonical_entries.len() as u64,
    }
}

pub fn rebuild_manifest_identity(
    workspace_id: &WorkspaceId,
    entries: &[NamespaceEntry],
    created_at: &str,
) -> ManifestIdentityReport {
    build_manifest_identity(workspace_id, entries, created_at)
}

pub(crate) fn manifest_identity_from_manifest(
    manifest: &SnapshotManifest,
) -> ManifestIdentityReport {
    ManifestIdentityReport {
        snapshot_id: manifest.snapshot_id.clone(),
        semantic_manifest_digest: manifest.semantic_manifest_digest.clone(),
        entries_hashed: manifest.entry_count,
    }
}
