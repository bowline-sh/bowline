use std::collections::BTreeMap;

use bowline_core::workspace_graph::{
    NamespaceEntry, NamespaceEntryKind, SnapshotManifest, normalize_workspace_path,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkViewOverlay {
    pub base_snapshot_id: bowline_core::ids::SnapshotId,
    pub overlay_version: u64,
    pub entries: Vec<OverlayEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayEntry {
    pub path: String,
    pub kind: OverlayEntryKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayEntryKind {
    File { byte_len: u64 },
    Directory,
    Symlink { target: String },
    Tombstone,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedWorkViewEntry {
    pub path: String,
    pub source: WorkViewEntrySource,
    pub kind: NamespaceEntryKind,
    pub byte_len: Option<u64>,
    pub symlink_target: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkViewEntrySource {
    Base,
    Overlay,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkViewOverlayResolver {
    base_entries: BTreeMap<String, NamespaceEntry>,
    overlay_entries: BTreeMap<String, OverlayEntryKind>,
}

impl WorkViewOverlayResolver {
    pub fn new(base: &SnapshotManifest, overlay: WorkViewOverlay) -> Self {
        let base_entries = base
            .entries
            .iter()
            .map(|entry| (normalize_workspace_path(&entry.path), entry.clone()))
            .collect();
        let overlay_entries = overlay
            .entries
            .into_iter()
            .map(|entry| (normalize_workspace_path(&entry.path), entry.kind))
            .collect();
        Self {
            base_entries,
            overlay_entries,
        }
    }

    pub fn resolve(&self, path: &str) -> Option<ResolvedWorkViewEntry> {
        let path = normalize_workspace_path(path);
        if let Some(entry) = self.overlay_entries.get(&path) {
            return overlay_entry(&path, entry);
        }
        self.base_entries.get(&path).map(base_entry)
    }

    pub fn list_paths(&self) -> Vec<ResolvedWorkViewEntry> {
        let mut paths = self
            .base_entries
            .keys()
            .chain(self.overlay_entries.keys())
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        paths.retain(|path| self.resolve(path).is_some());
        paths
            .into_iter()
            .filter_map(|path| self.resolve(&path))
            .collect()
    }
}

fn overlay_entry(path: &str, entry: &OverlayEntryKind) -> Option<ResolvedWorkViewEntry> {
    match entry {
        OverlayEntryKind::File { byte_len } => Some(ResolvedWorkViewEntry {
            path: path.to_string(),
            source: WorkViewEntrySource::Overlay,
            kind: NamespaceEntryKind::File,
            byte_len: Some(*byte_len),
            symlink_target: None,
        }),
        OverlayEntryKind::Directory => Some(ResolvedWorkViewEntry {
            path: path.to_string(),
            source: WorkViewEntrySource::Overlay,
            kind: NamespaceEntryKind::Directory,
            byte_len: None,
            symlink_target: None,
        }),
        OverlayEntryKind::Symlink { target } => Some(ResolvedWorkViewEntry {
            path: path.to_string(),
            source: WorkViewEntrySource::Overlay,
            kind: NamespaceEntryKind::Symlink,
            byte_len: None,
            symlink_target: Some(target.clone()),
        }),
        OverlayEntryKind::Tombstone => None,
    }
}

fn base_entry(entry: &NamespaceEntry) -> ResolvedWorkViewEntry {
    ResolvedWorkViewEntry {
        path: entry.path.clone(),
        source: WorkViewEntrySource::Base,
        kind: entry.kind,
        byte_len: entry.byte_len,
        symlink_target: entry.symlink_target.clone(),
    }
}

#[cfg(test)]
mod tests {
    use bowline_core::{
        ids::{SnapshotId, WorkspaceId},
        policy::{AccessFlag, MaterializationMode, PathClassification},
        workspace_graph::{HydrationState, RefKind, SnapshotKind, WorkspaceRef},
    };

    use super::*;

    #[test]
    fn overlay_entries_shadow_base_snapshot_entries() {
        let base = manifest_with_entries(vec![
            file_entry("app/src/auth.ts", 17),
            file_entry("app/src/keep.ts", 9),
        ]);
        let resolver = WorkViewOverlayResolver::new(
            &base,
            WorkViewOverlay {
                base_snapshot_id: base.snapshot_id.clone(),
                overlay_version: 1,
                entries: vec![OverlayEntry {
                    path: "app/src/auth.ts".to_string(),
                    kind: OverlayEntryKind::File { byte_len: 42 },
                }],
            },
        );

        let auth = resolver.resolve("app/src/auth.ts").expect("auth entry");
        let keep = resolver.resolve("app/src/keep.ts").expect("keep entry");

        assert_eq!(auth.source, WorkViewEntrySource::Overlay);
        assert_eq!(auth.byte_len, Some(42));
        assert_eq!(keep.source, WorkViewEntrySource::Base);
        assert_eq!(keep.byte_len, Some(9));
    }

    #[test]
    fn overlay_tombstone_hides_base_entry_only_inside_work_view() {
        let base = manifest_with_entries(vec![
            file_entry("app/src/auth.ts", 17),
            file_entry("app/src/keep.ts", 9),
        ]);
        let resolver = WorkViewOverlayResolver::new(
            &base,
            WorkViewOverlay {
                base_snapshot_id: base.snapshot_id.clone(),
                overlay_version: 1,
                entries: vec![OverlayEntry {
                    path: "app/src/auth.ts".to_string(),
                    kind: OverlayEntryKind::Tombstone,
                }],
            },
        );

        assert!(resolver.resolve("app/src/auth.ts").is_none());
        assert_eq!(
            resolver.resolve("app/src/keep.ts").expect("keep").source,
            WorkViewEntrySource::Base
        );
        assert_eq!(
            resolver
                .list_paths()
                .into_iter()
                .map(|entry| entry.path)
                .collect::<Vec<_>>(),
            vec!["app/src/keep.ts"]
        );
    }

    fn manifest_with_entries(entries: Vec<NamespaceEntry>) -> SnapshotManifest {
        SnapshotManifest {
            schema_version: 1,
            snapshot_id: SnapshotId::new("snap_base"),
            workspace_id: WorkspaceId::new("ws_code"),
            project_id: None,
            kind: SnapshotKind::WorkspaceHead,
            base_snapshot_id: None,
            entries,
            refs: vec![WorkspaceRef {
                name: "workspace".to_string(),
                target_snapshot_id: SnapshotId::new("snap_base"),
                kind: RefKind::Workspace,
            }],
        }
    }

    fn file_entry(path: &str, byte_len: u64) -> NamespaceEntry {
        NamespaceEntry {
            path: path.to_string(),
            kind: NamespaceEntryKind::File,
            classification: PathClassification::WorkspaceSync,
            mode: MaterializationMode::WorkspaceSync,
            access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
            content_id: None,
            locator: None,
            symlink_target: None,
            byte_len: Some(byte_len),
            hydration_state: HydrationState::Cold,
        }
    }
}
