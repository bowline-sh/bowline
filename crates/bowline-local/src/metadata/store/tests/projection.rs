use bowline_core::{
    ids::{ManifestDigest, NamespacePageId},
    workspace_graph::SnapshotKind,
};

use super::*;
use crate::metadata::{
    CurrentNamespaceEntryRecord, MetadataLogicalId, MetadataObjectBindingRecord, MetadataObjectKey,
    MetadataRecordKind, MetadataVerificationState, ProjectionRebuildInput, ProjectionSlice,
    SnapshotRecord, WorkspaceRelativePath,
};
use crate::metadata::{
    MaterializationPriorityClass, MaterializationTaskId, MaterializationTaskRecord,
    MaterializationTaskState, WorkspaceSyncHeadRecord,
};
use bowline_control_plane::{ControlPlaneTimestamp, WorkspaceRef};

const NOW: &str = "2026-07-14T10:00:00Z";

#[test]
fn component_projection_replacement_is_bounded_and_preserves_other_components() {
    let (mut store, _temp, workspace_id, snapshot_id) = projection_store("component-replace");
    store
        .rebuild_current_namespace_projection(&ProjectionRebuildInput {
            workspace_id: workspace_id.clone(),
            snapshot_id: snapshot_id.clone(),
            component_prefix: WorkspaceRelativePath::new(""),
            entries: vec![
                entry(&workspace_id, &snapshot_id, "a/file", ""),
                entry(&workspace_id, &snapshot_id, "b/file", ""),
            ],
        })
        .expect("seed full projection");

    let report = store
        .rebuild_current_namespace_projection(&ProjectionRebuildInput {
            workspace_id: workspace_id.clone(),
            snapshot_id: snapshot_id.clone(),
            component_prefix: WorkspaceRelativePath::new("a"),
            entries: vec![entry(&workspace_id, &snapshot_id, "a/next", "a")],
        })
        .expect("replace component");

    assert_eq!(report.rows_deleted, 1);
    assert_eq!(report.rows_written, 1);
    let rows = store
        .current_namespace_entries_by_component_prefix(
            &workspace_id,
            &WorkspaceRelativePath::new(""),
            10,
        )
        .expect("projection rows");
    assert_eq!(
        rows.iter().map(|row| row.path.as_str()).collect::<Vec<_>>(),
        vec!["a/next", "b/file"]
    );
}

#[test]
fn component_projection_treats_sql_wildcards_literally() {
    let (mut store, _temp, workspace_id, snapshot_id) = projection_store("component-wildcards");
    store
        .rebuild_current_namespace_projection(&ProjectionRebuildInput {
            workspace_id: workspace_id.clone(),
            snapshot_id: snapshot_id.clone(),
            component_prefix: WorkspaceRelativePath::new(""),
            entries: vec![
                entry(&workspace_id, &snapshot_id, "a%_/file", ""),
                entry(&workspace_id, &snapshot_id, "aXX/file", ""),
            ],
        })
        .expect("seed");
    store
        .rebuild_current_namespace_projection(&ProjectionRebuildInput {
            workspace_id: workspace_id.clone(),
            snapshot_id: snapshot_id.clone(),
            component_prefix: WorkspaceRelativePath::new("a%_"),
            entries: vec![entry(&workspace_id, &snapshot_id, "a%_/next", "a%_")],
        })
        .expect("replace literal component");
    assert!(
        store
            .current_namespace_entry(&workspace_id, &WorkspaceRelativePath::new("aXX/file"))
            .expect("lookup")
            .is_some()
    );
}

#[test]
fn root_level_replacement_deletes_stale_root_rows_and_preserves_deep_rows() {
    let (mut store, _temp, workspace_id, snapshot_id) = projection_store("root-level-delete");
    store
        .rebuild_current_namespace_projection(&ProjectionRebuildInput {
            workspace_id: workspace_id.clone(),
            snapshot_id: snapshot_id.clone(),
            component_prefix: WorkspaceRelativePath::new(""),
            entries: vec![
                entry(&workspace_id, &snapshot_id, "deleted.txt", ""),
                entry(&workspace_id, &snapshot_id, "kept.txt", ""),
                entry(&workspace_id, &snapshot_id, "unowned/deep.txt", ""),
            ],
        })
        .expect("seed full projection");

    let report = store
        .replace_current_namespace_owned_projection(
            &workspace_id,
            &snapshot_id,
            Some(&[entry(&workspace_id, &snapshot_id, "kept.txt", "kept.txt")]),
            &[],
        )
        .expect("replace root-level ownership");

    assert_eq!(report.rows_deleted, 2);
    assert_eq!(report.rows_written, 1);
    assert_projection_paths(&store, &workspace_id, &["kept.txt", "unowned/deep.txt"]);
}

#[test]
fn combined_replacement_deletes_stale_root_and_owned_subtree_rows_only() {
    let (mut store, _temp, workspace_id, snapshot_id) = projection_store("combined-delete");
    store
        .rebuild_current_namespace_projection(&ProjectionRebuildInput {
            workspace_id: workspace_id.clone(),
            snapshot_id: snapshot_id.clone(),
            component_prefix: WorkspaceRelativePath::new(""),
            entries: vec![
                entry(&workspace_id, &snapshot_id, "deleted.txt", ""),
                entry(&workspace_id, &snapshot_id, "kept.txt", ""),
                entry(&workspace_id, &snapshot_id, "owned/old.txt", ""),
                entry(&workspace_id, &snapshot_id, "unowned/deep.txt", ""),
            ],
        })
        .expect("seed full projection");
    let owned = ProjectionRebuildInput {
        workspace_id: workspace_id.clone(),
        snapshot_id: snapshot_id.clone(),
        component_prefix: WorkspaceRelativePath::new("owned"),
        entries: vec![entry(&workspace_id, &snapshot_id, "owned/new.txt", "owned")],
    };

    store
        .replace_current_namespace_owned_projection(
            &workspace_id,
            &snapshot_id,
            Some(&[entry(&workspace_id, &snapshot_id, "kept.txt", "kept.txt")]),
            &[owned],
        )
        .expect("replace combined ownership");

    assert_projection_paths(
        &store,
        &workspace_id,
        &["kept.txt", "owned/new.txt", "unowned/deep.txt"],
    );
}

#[test]
fn projection_rebuild_rejects_unordered_or_out_of_component_input_before_mutation() {
    let (mut store, _temp, workspace_id, snapshot_id) = projection_store("component-invalid");
    let error = store
        .rebuild_current_namespace_projection(&ProjectionRebuildInput {
            workspace_id: workspace_id.clone(),
            snapshot_id: snapshot_id.clone(),
            component_prefix: WorkspaceRelativePath::new("owned"),
            entries: vec![entry(&workspace_id, &snapshot_id, "outside/file", "owned")],
        })
        .expect_err("outside entry rejected");
    assert!(matches!(
        error,
        MetadataError::InvalidCurrentNamespaceProjection { .. }
    ));
    assert!(
        store
            .current_namespace_entries_by_component_prefix(
                &workspace_id,
                &WorkspaceRelativePath::new(""),
                10,
            )
            .expect("projection")
            .is_empty()
    );
}

#[test]
fn streamed_projection_rolls_back_delete_and_writes_on_late_order_failure() {
    let (mut store, _temp, workspace_id, snapshot_id) = projection_store("stream-rollback");
    store
        .rebuild_current_namespace_projection(&ProjectionRebuildInput {
            workspace_id: workspace_id.clone(),
            snapshot_id: snapshot_id.clone(),
            component_prefix: WorkspaceRelativePath::new(""),
            entries: vec![entry(&workspace_id, &snapshot_id, "owned/old", "")],
        })
        .expect("seed projection");

    let error: MetadataError = store
        .replace_current_namespace_projection_stream(
            &workspace_id,
            &snapshot_id,
            &[ProjectionSlice::Component(WorkspaceRelativePath::new(
                "owned",
            ))],
            |_, sink| {
                sink(entry(&workspace_id, &snapshot_id, "owned/z", "owned"))?;
                sink(entry(&workspace_id, &snapshot_id, "owned/a", "owned"))
            },
        )
        .expect_err("unordered streamed record rejected");
    assert!(matches!(
        error,
        MetadataError::InvalidCurrentNamespaceProjection { .. }
    ));
    assert_projection_paths(&store, &workspace_id, &["owned/old"]);
}

#[test]
fn projection_refresh_does_not_restart_a_live_metadata_gc_generation() {
    let (mut store, _temp, workspace_id, snapshot_id) = projection_store("gc-stable");
    store
        .start_metadata_gc(&workspace_id, "gc-live", NOW, NOW)
        .expect("start metadata gc");

    store
        .rebuild_current_namespace_projection(&ProjectionRebuildInput {
            workspace_id: workspace_id.clone(),
            snapshot_id,
            component_prefix: WorkspaceRelativePath::new(""),
            entries: Vec::new(),
        })
        .expect("refresh rebuildable projection");

    assert_eq!(
        store
            .metadata_gc_checkpoint(&workspace_id)
            .expect("gc checkpoint")
            .expect("live generation")
            .generation,
        "gc-live"
    );
}

#[test]
fn only_successfully_materialized_paths_are_promoted_local() {
    let (mut store, _temp, workspace_id, snapshot_id) = projection_store("hydration-truth");
    let mut staged = materialization_task(&workspace_id, &snapshot_id, "ready.txt");
    staged.id = MaterializationTaskId::new("task-ready");
    let mut blocked = materialization_task(&workspace_id, &snapshot_id, "blocked.txt");
    blocked.id = MaterializationTaskId::new("task-blocked");
    store
        .reconcile_materialization_tasks(&workspace_id, &snapshot_id, &[staged, blocked], NOW)
        .expect("materialization tasks");
    store
        .connection()
        .execute(
            "UPDATE materialization_tasks SET state = CASE path
               WHEN 'ready.txt' THEN 'staged' ELSE 'blocked-missing' END
             WHERE workspace_id = ?1 AND snapshot_id = ?2",
            rusqlite::params![workspace_id.as_str(), snapshot_id.as_str()],
        )
        .expect("seed task outcomes");
    store
        .upsert_workspace_sync_head(&WorkspaceSyncHeadRecord {
            workspace_ref: WorkspaceRef {
                workspace_id: workspace_id.clone(),
                version: 1,
                snapshot_id: snapshot_id.clone(),
                updated_at: ControlPlaneTimestamp { tick: 1 },
                updated_by_device_id: None,
            },
            observed_at: NOW.to_string(),
        })
        .expect("workspace head");
    let mut ready = entry(&workspace_id, &snapshot_id, "ready.txt", "");
    ready.hydration_state = HydrationState::Cold;
    let mut blocked = entry(&workspace_id, &snapshot_id, "blocked.txt", "");
    blocked.hydration_state = HydrationState::Cold;
    store
        .rebuild_current_namespace_projection(&ProjectionRebuildInput {
            workspace_id: workspace_id.clone(),
            snapshot_id: snapshot_id.clone(),
            component_prefix: WorkspaceRelativePath::new(""),
            entries: vec![blocked, ready],
        })
        .expect("cold import projection");

    store
        .complete_materialization_snapshot(&workspace_id, &snapshot_id, NOW)
        .expect("complete staged path");
    store
        .promote_ready_current_namespace_hydration(&workspace_id, &snapshot_id, NOW)
        .expect("promote ready projection rows");

    assert_eq!(
        store
            .current_namespace_entry(&workspace_id, &WorkspaceRelativePath::new("ready.txt"))
            .expect("ready projection")
            .expect("ready row")
            .hydration_state,
        HydrationState::Local
    );
    assert_eq!(
        store
            .current_namespace_entry(&workspace_id, &WorkspaceRelativePath::new("blocked.txt"))
            .expect("blocked projection")
            .expect("blocked row")
            .hydration_state,
        HydrationState::Cold
    );
}

fn projection_store(name: &str) -> (MetadataStore, TempWorkspace, WorkspaceId, SnapshotId) {
    let temp = TempWorkspace::new(name).expect("temp workspace");
    let mut store = MetadataStore::open(temp.root().join("metadata.sqlite3")).expect("metadata");
    let workspace_id = WorkspaceId::new(format!("ws_{name}"));
    let snapshot_id = SnapshotId::new(format!("snap_{name}"));
    let root_id = NamespacePageId::new(format!("page_{name}"));
    store
        .insert_workspace(&workspace_id, "Workspace", NOW)
        .expect("workspace");
    store
        .insert_metadata_object_binding(&MetadataObjectBindingRecord {
            workspace_id: workspace_id.clone(),
            logical_id: MetadataLogicalId::new(root_id.as_str()),
            kind: MetadataRecordKind::NamespacePage,
            object_key: MetadataObjectKey::new(format!("metadata_{name}")),
            byte_len: 128,
            object_hash: format!("hash_{name}"),
            key_epoch: 1,
            verification_state: MetadataVerificationState::Verified,
            created_at: NOW.to_string(),
            verified_at: Some(NOW.to_string()),
        })
        .expect("binding");
    store
        .commit_snapshot_root(
            &SnapshotRecord {
                id: snapshot_id.clone(),
                workspace_id: workspace_id.clone(),
                project_id: None,
                kind: SnapshotKind::WorkspaceHead,
                base_snapshot_id: None,
                root_id,
                semantic_manifest_digest: ManifestDigest::new(format!("digest_{name}")),
                entry_count: 2,
                refs: Vec::new(),
                created_at: NOW.to_string(),
            },
            &[],
            NOW,
        )
        .expect("snapshot root");
    (store, temp, workspace_id, snapshot_id)
}

fn entry(
    workspace_id: &WorkspaceId,
    snapshot_id: &SnapshotId,
    path: &str,
    component: &str,
) -> CurrentNamespaceEntryRecord {
    CurrentNamespaceEntryRecord {
        workspace_id: workspace_id.clone(),
        snapshot_id: snapshot_id.clone(),
        project_id: None,
        component_prefix: WorkspaceRelativePath::new(component),
        path: WorkspaceRelativePath::new(path),
        kind: NamespaceEntryKind::File,
        classification: PathClassification::WorkspaceSync,
        mode: MaterializationMode::WorkspaceSync,
        access: Vec::new(),
        content_id: Some(ContentId::new(format!("content_{path}"))),
        content_layout_id: None,
        symlink_target: None,
        byte_len: Some(1),
        executability: FileExecutability::Regular,
        hydration_state: HydrationState::Local,
        updated_at: NOW.to_string(),
    }
}

fn materialization_task(
    workspace_id: &WorkspaceId,
    snapshot_id: &SnapshotId,
    path: &str,
) -> MaterializationTaskRecord {
    MaterializationTaskRecord {
        id: MaterializationTaskId::new(format!("task-{path}")),
        workspace_id: workspace_id.clone(),
        project_id: None,
        snapshot_id: snapshot_id.clone(),
        path: path.to_string(),
        expected_kind: NamespaceEntryKind::File,
        expected_content_id: Some(ContentId::new(format!("content-{path}"))),
        expected_byte_len: 1,
        expected_executable: false,
        priority_class: MaterializationPriorityClass::SmallFile,
        state: MaterializationTaskState::Queued,
        attempt_count: 0,
        claim_generation: 0,
        not_before: None,
        claim_token: None,
        claimed_by: None,
        claimed_at: None,
        lease_expires_at: None,
        last_error_kind: None,
        last_error: None,
        created_at: NOW.to_string(),
        updated_at: NOW.to_string(),
    }
}

fn assert_projection_paths(store: &MetadataStore, workspace_id: &WorkspaceId, expected: &[&str]) {
    let paths = store
        .current_namespace_entries_by_component_prefix(
            workspace_id,
            &WorkspaceRelativePath::new(""),
            20,
        )
        .expect("projection rows")
        .into_iter()
        .map(|row| row.path.as_str().to_string())
        .collect::<Vec<_>>();
    assert_eq!(paths, expected);
}
