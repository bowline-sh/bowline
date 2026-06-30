use bowline_index::{
    AccessFlags, FileKind, HydrationState, NamespaceEntry, NamespaceIndex, PathClassification,
    StorageMode,
};

fn entry(path: &str, kind: FileKind, source_watermark: u64) -> NamespaceEntry {
    NamespaceEntry {
        workspace_id: "ws_code".to_string(),
        project_id: "proj_acme_web".to_string(),
        path: path.to_string(),
        kind,
        size_bytes: 42,
        classification: PathClassification::Source,
        storage_mode: StorageMode::Remote,
        hydration_state: HydrationState::Cold,
        machine_presence: vec!["macbook".to_string()],
        policy_version: 7,
        snapshot_id: "snap_1".to_string(),
        lineage: vec!["snap_0".to_string()],
        content_id: Some(format!("cid_{path}")),
        access: AccessFlags::readable(),
        source_watermark,
    }
}

#[test]
fn namespace_lists_direct_children_without_bytes() {
    let mut index = NamespaceIndex::new("2026-06-25T10:00:00Z");
    index.upsert(entry("src", FileKind::Directory, 1));
    index.upsert(entry("src/lib.rs", FileKind::File, 2));
    index.upsert(entry("src/nested/mod.rs", FileKind::File, 3));
    index.upsert(entry("README.md", FileKind::File, 4));

    let root = index.list_tree_at_snapshot("snap_1", "");
    assert_eq!(
        root.entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>(),
        vec!["README.md", "src"]
    );

    let src = index.list_tree_at_snapshot("snap_1", "src");
    assert_eq!(src.entries.len(), 1);
    assert_eq!(src.entries[0].path, "src/lib.rs");
    assert_eq!(src.entries[0].hydration_state, HydrationState::Cold);
}

#[test]
fn namespace_upsert_remove_and_path_search_are_policy_scoped() {
    let mut index = NamespaceIndex::new("2026-06-25T10:00:00Z");
    index.upsert(entry("src/auth/callback.rs", FileKind::File, 1));
    let mut hidden = entry("src/auth/.env.local", FileKind::File, 2);
    hidden.access = AccessFlags::hidden();
    index.upsert(hidden);

    let hits = index.path_search("auth", Some("src"), 10);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, "src/auth/callback.rs");

    assert!(index.remove("src/auth/callback.rs", 3).is_some());
    assert!(index.path_search("callback", None, 10).is_empty());
    assert_eq!(index.freshness().source_watermark, 3);
}
