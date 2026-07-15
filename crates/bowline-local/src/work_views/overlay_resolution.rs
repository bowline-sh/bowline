use std::collections::BTreeMap;

use bowline_core::workspace_graph::{NamespaceEntry, NamespaceEntryKind, normalize_workspace_path};

use crate::sync::SnapshotContent;

use super::WorkViewError;

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

#[derive(Debug)]
pub struct WorkViewOverlayResolver<'a> {
    base: &'a SnapshotContent,
    overlay_entries: BTreeMap<String, OverlayEntryKind>,
}

impl<'a> WorkViewOverlayResolver<'a> {
    pub fn new(base: &'a SnapshotContent, overlay: WorkViewOverlay) -> Result<Self, WorkViewError> {
        if overlay.base_snapshot_id != base.manifest().snapshot_id {
            return Err(WorkViewError::SnapshotMaterialization {
                snapshot_id: overlay.base_snapshot_id.as_str().to_string(),
                reason: "work-view overlay base does not match the page-backed snapshot"
                    .to_string(),
            });
        }
        let overlay_entries = overlay
            .entries
            .into_iter()
            .map(|entry| (normalize_workspace_path(&entry.path), entry.kind))
            .collect();
        Ok(Self {
            base,
            overlay_entries,
        })
    }

    pub fn resolve(&self, path: &str) -> Result<Option<ResolvedWorkViewEntry>, WorkViewError> {
        let path = normalize_workspace_path(path);
        if let Some(entry) = self.overlay_entries.get(&path) {
            return Ok(overlay_entry(&path, entry));
        }
        Ok(super::namespace::get_descriptor_entry(self.base, &path)?
            .as_ref()
            .map(base_entry))
    }

    pub fn list_paths(&self) -> Result<Vec<ResolvedWorkViewEntry>, WorkViewError> {
        let mut resolved = super::namespace::collect_descriptor_entries(self.base)?
            .into_iter()
            .map(|entry| (normalize_workspace_path(&entry.path), base_entry(&entry)))
            .collect::<BTreeMap<_, _>>();
        for (path, entry) in &self.overlay_entries {
            if let Some(entry) = overlay_entry(path, entry) {
                resolved.insert(path.clone(), entry);
            } else {
                resolved.remove(path);
            }
        }
        Ok(resolved.into_values().collect())
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
        ids::WorkspaceId,
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
                base_snapshot_id: base.manifest().snapshot_id.clone(),
                overlay_version: 1,
                entries: vec![OverlayEntry {
                    path: "app/src/auth.ts".to_string(),
                    kind: OverlayEntryKind::File { byte_len: 42 },
                }],
            },
        )
        .expect("resolver");

        let auth = resolver
            .resolve("app/src/auth.ts")
            .expect("resolve")
            .expect("auth entry");
        let keep = resolver
            .resolve("app/src/keep.ts")
            .expect("resolve")
            .expect("keep entry");

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
                base_snapshot_id: base.manifest().snapshot_id.clone(),
                overlay_version: 1,
                entries: vec![OverlayEntry {
                    path: "app/src/auth.ts".to_string(),
                    kind: OverlayEntryKind::Tombstone,
                }],
            },
        )
        .expect("resolver");

        assert!(
            resolver
                .resolve("app/src/auth.ts")
                .expect("resolve")
                .is_none()
        );
        assert_eq!(
            resolver
                .resolve("app/src/keep.ts")
                .expect("resolve")
                .expect("keep")
                .source,
            WorkViewEntrySource::Base
        );
        assert_eq!(
            resolver
                .list_paths()
                .expect("list")
                .into_iter()
                .map(|entry| entry.path)
                .collect::<Vec<_>>(),
            vec!["app/src/keep.ts"]
        );
    }

    #[test]
    fn overlay_resolver_rejects_a_different_page_backed_base() {
        let base = manifest_with_entries(vec![file_entry("app/src/auth.ts", 17)]);
        let error = WorkViewOverlayResolver::new(
            &base,
            WorkViewOverlay {
                base_snapshot_id: bowline_core::ids::SnapshotId::new("snap_other"),
                overlay_version: 1,
                entries: Vec::new(),
            },
        )
        .expect_err("mismatched base must fail");

        assert!(matches!(
            error,
            WorkViewError::SnapshotMaterialization { reason, .. }
                if reason.contains("does not match")
        ));
    }

    fn manifest_with_entries(entries: Vec<NamespaceEntry>) -> SnapshotContent {
        let workspace_id = WorkspaceId::new("ws_code");
        let snapshot_id =
            crate::sync::rebuild_manifest_identity(&workspace_id, &entries, "test").snapshot_id;
        SnapshotContent::new(
            bowline_core::workspace_graph::SnapshotDraft {
                schema_version: bowline_core::workspace_graph::SNAPSHOT_SCHEMA_VERSION,
                snapshot_id: snapshot_id.clone(),
                workspace_id,
                project_id: None,
                kind: SnapshotKind::WorkspaceHead,
                base_snapshot_id: None,
                entries,
                refs: vec![WorkspaceRef {
                    name: "workspace".to_string(),
                    target_snapshot_id: snapshot_id,
                    kind: RefKind::Workspace,
                }],
            },
            BTreeMap::new(),
            [7; 32],
        )
        .expect("page-backed snapshot")
    }

    fn file_entry(path: &str, byte_len: u64) -> NamespaceEntry {
        NamespaceEntry {
            path: path.to_string(),
            kind: NamespaceEntryKind::File,
            classification: PathClassification::WorkspaceSync,
            mode: MaterializationMode::WorkspaceSync,
            access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
            content_id: None,
            content_layout: None,
            symlink_target: None,
            byte_len: Some(byte_len),
            executability: bowline_core::workspace_graph::FileExecutability::Regular,
            hydration_state: HydrationState::Cold,
        }
    }
}
