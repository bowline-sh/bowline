use bowline_core::{
    ids::{ContentId, SnapshotId, WorkspaceId},
    policy::{AccessFlag, MaterializationMode, PathClassification},
    workspace_graph::{
        FileExecutability, HydrationState, NamespaceEntryKind, SNAPSHOT_SCHEMA_VERSION,
    },
};

use super::*;

#[test]
fn target_manifest_preserves_nested_never_exposed_secret_and_ancestor() {
    let hidden_id = ContentId::new("cid_hidden_secret");
    let current = SnapshotManifest {
        schema_version: SNAPSHOT_SCHEMA_VERSION,
        snapshot_id: SnapshotId::new("snap_current"),
        workspace_id: WorkspaceId::new("ws_code"),
        project_id: None,
        kind: SnapshotKind::WorkspaceHead,
        base_snapshot_id: None,
        entries: vec![
            entry("apps/web/credentials", NamespaceEntryKind::Directory, None),
            entry(
                "apps/web/credentials/id_rsa",
                NamespaceEntryKind::File,
                Some(hidden_id.clone()),
            ),
            entry(
                "apps/web/src/lib.rs",
                NamespaceEntryKind::File,
                Some(ContentId::new("cid_old")),
            ),
        ],
        refs: Vec::new(),
    };
    let merged = vec![entry(
        "src/lib.rs",
        NamespaceEntryKind::File,
        Some(ContentId::new("cid_new")),
    )];
    let entries = splice_current_manifest_entries(
        &current,
        "apps/web",
        &BTreeSet::from(["credentials".to_string(), "src/lib.rs".to_string()]),
        &merged,
    );

    assert!(
        entries
            .iter()
            .any(|entry| entry.path == "apps/web/credentials")
    );
    let hidden = entries
        .iter()
        .find(|entry| entry.path == "apps/web/credentials/id_rsa")
        .expect("hidden secret remains in canonical target manifest");
    assert_eq!(hidden.content_id.as_ref(), Some(&hidden_id));
    assert!(hidden.access.contains(&AccessFlag::AgentHidden));
}

fn entry(path: &str, kind: NamespaceEntryKind, content_id: Option<ContentId>) -> NamespaceEntry {
    NamespaceEntry {
        path: path.to_string(),
        kind,
        classification: PathClassification::WorkspaceSync,
        mode: MaterializationMode::EncryptedSync,
        access: vec![AccessFlag::HumanReadable, AccessFlag::AgentHidden],
        content_id,
        content_layout: None,
        symlink_target: None,
        byte_len: None,
        executability: FileExecutability::Regular,
        hydration_state: HydrationState::Local,
    }
}
