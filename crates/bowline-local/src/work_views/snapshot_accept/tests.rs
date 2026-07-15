use bowline_core::{
    policy::{AccessFlag, MaterializationMode, PathClassification},
    workspace_graph::workspace_content_id,
};

use super::*;

fn entry(path: &str, kind: NamespaceEntryKind) -> NamespaceEntry {
    NamespaceEntry {
        path: path.to_string(),
        kind,
        classification: PathClassification::WorkspaceSync,
        mode: MaterializationMode::WorkspaceSync,
        access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
        content_id: None,
        content_layout: None,
        symlink_target: None,
        byte_len: (kind == NamespaceEntryKind::File).then_some(5),
        executability: FileExecutability::Regular,
        hydration_state: HydrationState::Local,
    }
}

#[test]
fn exposed_reader_preserves_page_backed_directories_and_content_identity() {
    let key = [41_u8; 32];
    let workspace_id = bowline_core::ids::WorkspaceId::new("ws_reader");
    let mut file = entry("apps/web/value.txt", NamespaceEntryKind::File);
    file.content_id = Some(workspace_content_id(key, b"value"));
    let entries = vec![entry("apps/web/empty", NamespaceEntryKind::Directory), file];
    let identity = crate::sync::rebuild_manifest_identity(&workspace_id, &entries, "test");
    let snapshot = crate::sync::SnapshotContent::new(
        bowline_core::workspace_graph::SnapshotDraft {
            schema_version: bowline_core::workspace_graph::SNAPSHOT_SCHEMA_VERSION,
            snapshot_id: identity.snapshot_id,
            workspace_id,
            project_id: None,
            kind: bowline_core::workspace_graph::SnapshotKind::Base,
            base_snapshot_id: None,
            entries,
            refs: Vec::new(),
        },
        BTreeMap::new(),
        key,
    )
    .expect("page-backed exposed snapshot");

    let reader = Reader::from_exposed_snapshot(&snapshot, "apps/web", &BTreeSet::new(), None)
        .expect("reader");

    assert!(
        reader
            .entries
            .iter()
            .any(|entry| entry.path == "empty" && entry.kind == NamespaceEntryKind::Directory)
    );
    let file = reader
        .entries
        .iter()
        .find(|entry| entry.path == "value.txt")
        .expect("file");
    assert_eq!(file.content_id, Some(workspace_content_id(key, b"value")));
}
