use std::{collections::BTreeSet, fs};

use bowline_core::{
    policy::{AccessFlag, MaterializationMode, PathClassification},
    workspace_graph::{
        FileExecutability, HydrationState, NamespaceEntry, NamespaceEntryKind, SnapshotDraft,
        SnapshotKind,
    },
};

use super::*;
use crate::sync::rebuild_manifest_identity;

#[test]
fn committed_env_merge_rederives_records_from_merged_bytes() {
    let workspace = TempWorkspace::new("sync-env-merge-records").expect("workspace");
    let state = TempWorkspace::new("sync-env-merge-records-state").expect("state");
    fs::write(
        workspace.root().join(".env.local"),
        b"API_KEY=placeholder-local\n",
    )
    .expect("local env");

    let workspace_id = WorkspaceId::new("ws_code");
    let content_key = [7_u8; 32];
    let base = env_snapshot(
        workspace_id.clone(),
        "base",
        ".env.local",
        b"API_KEY=placeholder-old\n",
        content_key,
    );
    let local = super::super::super::coalescer::coalesce_workspace_scan(
        workspace.root(),
        workspace_id.clone(),
        &empty_workspace_ref(workspace_id.clone()),
        DeviceId::new("device_local"),
        content_key,
        "2026-06-29T05:01:00Z",
    )
    .expect("candidate");
    let remote = env_snapshot(
        workspace_id.clone(),
        "remote",
        ".env.local",
        b"API_KEY=placeholder-old\nREMOTE_ONLY=placeholder-remote\n",
        content_key,
    );
    let merged = crate::sync::merge_snapshots(
        &base,
        &local,
        &remote,
        CandidateBase {
            workspace_id: workspace_id.clone(),
            version: 3,
            snapshot_id: SnapshotId::new("remote"),
        },
        content_key,
        "2026-06-29T05:02:00Z",
    )
    .expect("merge");
    let crate::sync::MergeOutcome::Clean(mut merged) = merged else {
        panic!("different env keys should merge");
    };
    assert_eq!(
        merged.snapshot.file_bytes_for_path(".env.local"),
        Some(&b"API_KEY=placeholder-local\nREMOTE_ONLY=placeholder-remote\n"[..])
    );
    merged.base = CandidateBase {
        workspace_id: workspace_id.clone(),
        version: 0,
        snapshot_id: SnapshotId::new(EMPTY_SNAPSHOT_ID),
    };

    let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
    store
        .insert_workspace(&workspace_id, "User Code", "2026-06-29T05:00:00Z")
        .expect("workspace");
    store
        .insert_root(
            "root_code",
            &workspace_id,
            &workspace.root().display().to_string(),
            "2026-06-29T05:00:00Z",
        )
        .expect("root");
    drop(store);

    let control_plane = bowline_control_plane::FakeControlPlaneClient::default();
    control_plane.create_workspace(workspace_id.as_str());
    let byte_store =
        bowline_storage::LocalByteStore::open(state.root().join("objects")).expect("byte store");
    let runner = SyncRunner::new(
        &control_plane,
        &byte_store,
        SyncRunnerOptions {
            root: workspace.root().to_path_buf(),
            state_root: state.root().to_path_buf(),
            workspace_id: workspace_id.clone(),
            device_id: DeviceId::new("device_local"),
            workspace_content_key: content_key,
            storage_key: StorageKey::from_bytes([8_u8; 32]),
            key_epoch: 1,
            generated_at: "2026-06-29T05:03:00Z".to_string(),
            sync_claim: None,
            scan_scope: Default::default(),
        },
    );
    runner
        .persist_scan_metadata(&merged, Some(&merged.snapshot))
        .expect("persist merged scan metadata");

    let store = MetadataStore::open(state.root().join(DEFAULT_DATABASE_FILE)).expect("store");
    let records = store.env_records(&workspace_id).expect("records");
    let keys = records
        .iter()
        .map(|record| record.key_name.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(keys, BTreeSet::from(["API_KEY", "REMOTE_ONLY"]));
    assert!(
        records
            .iter()
            .all(|record| record.value_ciphertext_ref.is_some())
    );
    let debug = format!("{records:?}");
    assert!(!debug.contains("placeholder-local"));
    assert!(!debug.contains("placeholder-remote"));
}

fn env_snapshot(
    workspace_id: WorkspaceId,
    _snapshot_id: &str,
    path: &str,
    bytes: &[u8],
    workspace_content_key: [u8; 32],
) -> SnapshotContent {
    let content_id =
        bowline_core::workspace_graph::workspace_content_id(workspace_content_key, bytes);
    let entries = vec![NamespaceEntry {
        path: path.to_string(),
        kind: NamespaceEntryKind::File,
        classification: PathClassification::ProjectEnv,
        mode: MaterializationMode::WorkspaceSync,
        access: vec![AccessFlag::HumanReadable, AccessFlag::AgentReadable],
        content_id: Some(content_id.clone()),
        content_layout: None,
        symlink_target: None,
        byte_len: Some(bytes.len() as u64),
        executability: FileExecutability::Regular,
        hydration_state: HydrationState::Local,
    }];
    let snapshot_id = rebuild_manifest_identity(&workspace_id, &entries, "test").snapshot_id;
    SnapshotContent::new(
        SnapshotDraft {
            schema_version: bowline_core::workspace_graph::SNAPSHOT_SCHEMA_VERSION,
            snapshot_id,
            workspace_id,
            project_id: None,
            kind: SnapshotKind::WorkspaceHead,
            base_snapshot_id: None,
            entries,
            refs: Vec::new(),
        },
        [(content_id, bytes.to_vec())].into_iter().collect(),
        [7; 32],
    )
    .expect("page-backed env snapshot")
}
