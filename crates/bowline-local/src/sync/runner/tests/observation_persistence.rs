use crate::metadata::ObservedLocalPath;
use crate::metadata::{
    MetadataLogicalId, MetadataObjectBindingRecord, MetadataObjectKey, MetadataRecordKind,
    MetadataVerificationState, SnapshotRecord,
};
use bowline_core::{
    policy::{MaterializationMode, PathClassification},
    status::ObservedWorkspaceSummary,
};
use std::collections::BTreeSet;

use super::*;

#[test]
fn unsafe_symlink_is_counted_only_as_blocked() {
    let mut paths = vec![
        observed_workspace_sync_path("README.md"),
        observed_workspace_sync_path("outside-link"),
    ];
    let mut summary = ObservedWorkspaceSummary::default();
    for path in &paths {
        summary.record_path(path.classification, path.mode);
    }
    let skipped = std::collections::BTreeSet::from(["outside-link".to_string()]);

    super::super::persistence::apply_unsafe_symlink_observation(&mut paths, &skipped, &mut summary);

    assert_eq!(summary.workspace_sync_path_count, 1);
    assert_eq!(summary.blocked_path_count, 1);
    assert_eq!(
        paths
            .iter()
            .find(|path| path.path == "outside-link")
            .map(|path| (path.classification, path.mode)),
        Some((PathClassification::Blocked, MaterializationMode::Blocked))
    );
}

#[test]
fn root_shallow_projection_deletes_removed_root_file_and_preserves_deep_row() {
    let workspace = TempWorkspace::new("root-shallow-projection-workspace").expect("workspace");
    let state = TempWorkspace::new("root-shallow-projection-state").expect("state");
    let workspace_id = WorkspaceId::new("ws_root_shallow_projection");
    let runner = test_runner(&workspace, &state, workspace_id.clone());
    let old = snapshot_with_files(
        workspace_id.clone(),
        &[
            ("deleted.txt", b"old"),
            ("kept.txt", b"kept"),
            ("unowned/deep.txt", b"deep"),
        ],
    );
    let current = snapshot_with_files(
        workspace_id.clone(),
        &[("kept.txt", b"kept"), ("unowned/deep.txt", b"deep")],
    );
    let mut candidate = projection_candidate(&workspace, workspace_id.clone());
    candidate.scan_scope = ScanScope::RootShallow;
    let mut store = projection_store(&state, &workspace_id);
    commit_projection_snapshot(&mut store, &old);
    runner
        .rebuild_current_namespace_projection_full(&mut store, &old)
        .expect("seed projection");
    commit_projection_snapshot(&mut store, &current);

    runner
        .rebuild_current_namespace_for_scan(&mut store, &current, &candidate)
        .expect("replace root-shallow projection");

    assert_runner_projection_paths(&store, &workspace_id, &["kept.txt", "unowned/deep.txt"]);
}

#[test]
fn combined_projection_deletes_removed_root_file_and_only_owned_subtree() {
    let workspace = TempWorkspace::new("combined-projection-workspace").expect("workspace");
    let state = TempWorkspace::new("combined-projection-state").expect("state");
    let workspace_id = WorkspaceId::new("ws_combined_projection");
    let runner = test_runner(&workspace, &state, workspace_id.clone());
    let old = snapshot_with_files(
        workspace_id.clone(),
        &[
            ("deleted.txt", b"old"),
            ("kept.txt", b"kept"),
            ("owned/old.txt", b"old-owned"),
            ("unowned/deep.txt", b"deep"),
        ],
    );
    let current = snapshot_with_files(
        workspace_id.clone(),
        &[
            ("kept.txt", b"kept"),
            ("owned/new.txt", b"new-owned"),
            ("unowned/deep.txt", b"deep"),
        ],
    );
    let mut candidate = projection_candidate(&workspace, workspace_id.clone());
    candidate.scan_scope = ScanScope::DirtySubtrees {
        roots: BTreeSet::from(["owned".to_string()]),
        root_shallow: true,
    };
    let mut store = projection_store(&state, &workspace_id);
    commit_projection_snapshot(&mut store, &old);
    runner
        .rebuild_current_namespace_projection_full(&mut store, &old)
        .expect("seed projection");
    commit_projection_snapshot(&mut store, &current);

    runner
        .rebuild_current_namespace_for_scan(&mut store, &current, &candidate)
        .expect("replace combined projection");

    assert_runner_projection_paths(
        &store,
        &workspace_id,
        &["kept.txt", "owned/new.txt", "unowned/deep.txt"],
    );
}

fn observed_workspace_sync_path(path: &str) -> ObservedLocalPath {
    ObservedLocalPath {
        project_id: None,
        path: path.to_string(),
        classification: PathClassification::WorkspaceSync,
        mode: MaterializationMode::WorkspaceSync,
        access: Vec::new(),
    }
}

fn projection_candidate(
    workspace: &TempWorkspace,
    workspace_id: WorkspaceId,
) -> crate::sync::SnapshotCandidate {
    super::super::super::coalescer::coalesce_workspace_scan(
        workspace.root(),
        workspace_id.clone(),
        &empty_workspace_ref(workspace_id),
        DeviceId::new("device_projection"),
        [7_u8; 32],
        "2026-07-14T12:00:00Z",
    )
    .expect("projection candidate")
}

fn projection_store(state: &TempWorkspace, workspace_id: &WorkspaceId) -> MetadataStore {
    let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
    store
        .insert_workspace(workspace_id, "Projection", "2026-07-14T12:00:00Z")
        .expect("workspace record");
    store
}

fn commit_projection_snapshot(store: &mut MetadataStore, snapshot: &SnapshotContent) {
    let manifest = snapshot.manifest();
    store
        .insert_metadata_object_binding(&MetadataObjectBindingRecord {
            workspace_id: manifest.workspace_id.clone(),
            logical_id: MetadataLogicalId::new(manifest.namespace_root_id.as_str()),
            kind: MetadataRecordKind::NamespacePage,
            object_key: MetadataObjectKey::new(format!(
                "metadata_{}",
                manifest.namespace_root_id.as_str()
            )),
            byte_len: 1,
            object_hash: "hash_projection".to_string(),
            key_epoch: 1,
            verification_state: MetadataVerificationState::Verified,
            created_at: "2026-07-14T12:00:00Z".to_string(),
            verified_at: Some("2026-07-14T12:00:00Z".to_string()),
        })
        .expect("root binding");
    store
        .commit_snapshot_root(
            &SnapshotRecord {
                id: manifest.snapshot_id.clone(),
                workspace_id: manifest.workspace_id.clone(),
                project_id: manifest.project_id.clone(),
                kind: manifest.kind,
                base_snapshot_id: manifest.base_snapshot_id.clone(),
                root_id: manifest.namespace_root_id.clone(),
                semantic_manifest_digest: manifest.semantic_manifest_digest.clone(),
                entry_count: manifest.entry_count,
                refs: manifest.refs.clone(),
                created_at: "2026-07-14T12:00:00Z".to_string(),
            },
            &[],
            "2026-07-14T12:00:00Z",
        )
        .expect("snapshot root");
}

fn assert_runner_projection_paths(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    expected: &[&str],
) {
    let paths = store
        .current_namespace_entries_by_component_prefix(
            workspace_id,
            &WorkspaceRelativePath::new(""),
            20,
        )
        .expect("projection rows")
        .into_iter()
        .map(|entry| entry.path.as_str().to_string())
        .collect::<Vec<_>>();
    assert_eq!(paths, expected);
}
