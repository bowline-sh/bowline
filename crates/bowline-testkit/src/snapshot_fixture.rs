use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt, path::Path};

use bowline_core::{
    ids::{ProjectId, SnapshotId, WorkspaceId},
    policy::{AccessFlag, MaterializationMode, PathClassification},
    workspace_graph::{
        FileExecutability, HydrationState, NamespaceEntry, NamespaceEntryKind, RefKind,
        SnapshotDraft, SnapshotKind, WorkspaceRef, workspace_content_id,
    },
};
use bowline_local::{metadata::MetadataStore, sync::SnapshotContent};
use bowline_storage::LocalContentCache;

/// Persists a canonical page-backed snapshot for an integration-test project.
///
/// This intentionally lives in the testkit so integration tests cannot fall back to a
/// production-invalid flat or live-tree authority.
pub fn persist_project_snapshot_fixture(
    store: &mut MetadataStore,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    workspace_root: &Path,
    project_path: &str,
    state_root: &Path,
    created_at: &str,
) -> SnapshotId {
    let workspace_content_key = [0x71; 32];
    let mut files = BTreeMap::new();
    let mut entries = Vec::new();
    collect_entries(
        workspace_root,
        &workspace_root.join(project_path),
        workspace_content_key,
        &mut entries,
        &mut files,
    );
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    let identity =
        bowline_local::sync::rebuild_manifest_identity(workspace_id, &entries, "snapshot fixture");
    let snapshot_id = identity.snapshot_id().clone();
    let snapshot = SnapshotContent::new(
        SnapshotDraft {
            schema_version: bowline_core::workspace_graph::SNAPSHOT_SCHEMA_VERSION,
            snapshot_id: snapshot_id.clone(),
            workspace_id: workspace_id.clone(),
            project_id: Some(project_id.clone()),
            kind: SnapshotKind::WorkspaceHead,
            base_snapshot_id: None,
            entries,
            refs: vec![WorkspaceRef {
                name: "workspace".to_string(),
                target_snapshot_id: snapshot_id.clone(),
                kind: RefKind::Workspace,
            }],
        },
        files.clone(),
        workspace_content_key,
    )
    .expect("canonical snapshot fixture");
    let cache = LocalContentCache::open(state_root.join("cache")).expect("content cache");
    for (content_id, bytes) in files {
        cache
            .put_content(&content_id, &bytes)
            .expect("cached fixture content");
        cache
            .get_content(&content_id, workspace_content_key)
            .expect("verified fixture content");
    }
    bowline_local::page_test_support::persist_cached_snapshot(
        store,
        &snapshot,
        &state_root.join("metadata-pages"),
        created_at,
    );
    store
        .set_project_latest_snapshot_id(workspace_id, project_id, &snapshot_id)
        .expect("project latest snapshot");
    snapshot_id
}

fn collect_entries(
    workspace_root: &Path,
    current: &Path,
    workspace_content_key: [u8; 32],
    entries: &mut Vec<NamespaceEntry>,
    files: &mut BTreeMap<bowline_core::ids::ContentId, Vec<u8>>,
) {
    let Ok(metadata) = fs::symlink_metadata(current) else {
        return;
    };
    if current != workspace_root {
        let path = current
            .strip_prefix(workspace_root)
            .expect("fixture path under workspace root")
            .to_string_lossy()
            .replace('\\', "/");
        if metadata.file_type().is_symlink() {
            entries.push(entry(
                path,
                NamespaceEntryKind::Symlink,
                None,
                None,
                Some(
                    fs::read_link(current)
                        .expect("fixture symlink")
                        .to_string_lossy()
                        .into_owned(),
                ),
                FileExecutability::Regular,
            ));
            return;
        }
        if metadata.is_file() {
            let bytes = fs::read(current).expect("fixture file");
            let content_id = workspace_content_id(workspace_content_key, &bytes);
            let executability = if metadata.permissions().mode() & 0o111 == 0 {
                FileExecutability::Regular
            } else {
                FileExecutability::Executable
            };
            entries.push(entry(
                path,
                NamespaceEntryKind::File,
                Some(content_id.clone()),
                Some(bytes.len() as u64),
                None,
                executability,
            ));
            files.insert(content_id, bytes);
            return;
        }
        if metadata.is_dir() {
            entries.push(entry(
                path,
                NamespaceEntryKind::Directory,
                None,
                None,
                None,
                FileExecutability::Regular,
            ));
        }
    }
    if metadata.is_dir() {
        let mut children = fs::read_dir(current)
            .expect("fixture directory")
            .map(|child| child.expect("fixture child").path())
            .collect::<Vec<_>>();
        children.sort();
        for child in children {
            collect_entries(
                workspace_root,
                &child,
                workspace_content_key,
                entries,
                files,
            );
        }
    }
}

fn entry(
    path: String,
    kind: NamespaceEntryKind,
    content_id: Option<bowline_core::ids::ContentId>,
    byte_len: Option<u64>,
    symlink_target: Option<String>,
    executability: FileExecutability,
) -> NamespaceEntry {
    NamespaceEntry {
        path,
        kind,
        classification: PathClassification::WorkspaceSync,
        mode: MaterializationMode::WorkspaceSync,
        access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
        content_id,
        content_layout: None,
        symlink_target,
        byte_len,
        executability,
        hydration_state: HydrationState::Local,
    }
}
