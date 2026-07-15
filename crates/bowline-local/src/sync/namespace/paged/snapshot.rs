use std::{collections::BTreeSet, sync::Arc};

use bowline_core::{
    ids::{ManifestDigest, NamespacePageId, SnapshotId},
    namespace_snapshot::{NamespaceReadError, SnapshotMetadata},
    workspace_graph::SnapshotManifest,
};

use super::types::PageStore;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangedPageSummary {
    pub mutations_applied: u64,
    pub semantic_entries_hashed: u64,
    pub namespace_pages_created: u64,
    pub namespace_pages_reused: u64,
    pub namespace_pages_removed: u64,
    pub content_layouts_created: u64,
    pub content_layouts_reused: u64,
    pub segment_pages_created: u64,
    pub segment_pages_reused: u64,
    pub metadata_bytes_created: u64,
    pub namespace_pages_loaded_during_build: u64,
    pub namespace_pages_encoded: u64,
    pub content_layouts_encoded: u64,
    pub segment_pages_encoded: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltPagedNamespaceSnapshot {
    pub metadata: SnapshotMetadata,
    pub namespace_root_id: NamespacePageId,
    pub semantic_manifest_digest: ManifestDigest,
    pub snapshot_id: SnapshotId,
    pub store: PageStore,
    pub changed: ChangedPageSummary,
    pub(crate) reachable_namespace_ids: Arc<BTreeSet<NamespacePageId>>,
    pub(crate) reachable_content_layouts: u64,
    pub(crate) reachable_segment_pages: u64,
}

impl BuiltPagedNamespaceSnapshot {
    pub fn from_manifest(manifest: SnapshotManifest, store: PageStore) -> Self {
        let reachable_namespace_ids = Arc::new(store.namespace_page_ids());
        let reachable_content_layouts = store.content_layout_count();
        let reachable_segment_pages = store.segment_page_count();
        let metadata = SnapshotMetadata {
            schema_version: manifest.schema_version,
            snapshot_id: manifest.snapshot_id.clone(),
            workspace_id: manifest.workspace_id,
            project_id: manifest.project_id,
            kind: manifest.kind,
            base_snapshot_id: manifest.base_snapshot_id,
            semantic_manifest_digest: manifest.semantic_manifest_digest.clone(),
            entry_count: manifest.entry_count,
            refs: manifest.refs,
        };
        Self {
            metadata,
            namespace_root_id: manifest.namespace_root_id,
            semantic_manifest_digest: manifest.semantic_manifest_digest,
            snapshot_id: manifest.snapshot_id,
            store,
            changed: ChangedPageSummary::default(),
            reachable_namespace_ids,
            reachable_content_layouts,
            reachable_segment_pages,
        }
    }

    pub fn manifest(&self) -> SnapshotManifest {
        SnapshotManifest {
            schema_version: self.metadata.schema_version,
            snapshot_id: self.snapshot_id.clone(),
            workspace_id: self.metadata.workspace_id.clone(),
            project_id: self.metadata.project_id.clone(),
            kind: self.metadata.kind,
            base_snapshot_id: self.metadata.base_snapshot_id.clone(),
            namespace_root_id: self.namespace_root_id.clone(),
            semantic_manifest_digest: self.semantic_manifest_digest.clone(),
            entry_count: self.metadata.entry_count,
            refs: self.metadata.refs.clone(),
        }
    }

    pub fn reachable_namespace_page_ids(
        &self,
    ) -> Result<BTreeSet<NamespacePageId>, NamespaceReadError> {
        Ok((*self.reachable_namespace_ids).clone())
    }
}
