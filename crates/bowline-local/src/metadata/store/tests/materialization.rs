use super::*;
use crate::metadata::{
    MaterializationFailureKind, MaterializationPathState, MaterializationPathStateRecord,
    MaterializationPriorityClass, MaterializationTaskFence, MaterializationTaskFinish,
    MaterializationTaskId, MaterializationTaskRecord, MaterializationTaskState,
    WorkspaceSyncHeadRecord,
};
use crate::sync::{
    ConflictBundle, ConflictRecord, ConflictState, create_conflict_bundle,
    transition_conflict_occurrence_state, unresolved_conflict_paths,
};
use bowline_control_plane::{ControlPlaneTimestamp, WorkspaceRef};
use std::collections::BTreeSet;

#[test]
fn reconcile_is_idempotent_and_cancels_superseded_tasks() {
    let temp = TempWorkspace::new("materialization-reconcile").expect("temp workspace");
    let store = MetadataStore::open(temp.root().join("state.sqlite3")).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_materialization");
    let snapshot_id = SnapshotId::new("snap_current");
    store
        .insert_workspace(&workspace_id, "Code", "2026-07-13T00:00:00Z")
        .expect("workspace insert");

    let first = task(
        "task-a",
        &workspace_id,
        &snapshot_id,
        "app/a.txt",
        MaterializationPriorityClass::SmallFile,
    );
    let initial = store
        .reconcile_materialization_tasks(
            &workspace_id,
            &snapshot_id,
            std::slice::from_ref(&first),
            "2026-07-13T00:00:00Z",
        )
        .expect("initial reconcile");
    assert_eq!(initial.inserted, 1);

    let repeat = store
        .reconcile_materialization_tasks(
            &workspace_id,
            &snapshot_id,
            std::slice::from_ref(&first),
            "2026-07-13T00:01:00Z",
        )
        .expect("repeat reconcile");
    assert_eq!(repeat.inserted, 0);
    assert_eq!(repeat.cancelled, 0);

    let next_snapshot_id = SnapshotId::new("snap_next");
    let next = task(
        "task-b",
        &workspace_id,
        &next_snapshot_id,
        "app/a.txt",
        MaterializationPriorityClass::ActiveProject,
    );
    let superseded = store
        .reconcile_materialization_tasks(
            &workspace_id,
            &next_snapshot_id,
            std::slice::from_ref(&next),
            "2026-07-13T00:02:00Z",
        )
        .expect("superseding reconcile");
    assert_eq!(superseded.inserted, 1);
    assert_eq!(superseded.cancelled, 1);
    assert_eq!(
        store
            .materialization_task(&first.id)
            .expect("old task query")
            .expect("old task")
            .state,
        MaterializationTaskState::Cancelled
    );
    assert_eq!(
        store
            .materialization_tasks(&workspace_id)
            .expect("active task list")
            .iter()
            .map(|task| task.id.as_str())
            .collect::<Vec<_>>(),
        vec!["task-b"]
    );
}

#[test]
fn authoritative_head_cancels_stale_tasks_and_path_blockers() {
    let temp = TempWorkspace::new("materialization-authoritative-head").expect("temp workspace");
    let store = MetadataStore::open(temp.root().join("state.sqlite3")).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_authoritative_head");
    let stale_snapshot_id = SnapshotId::new("snap_stale");
    let current_snapshot_id = SnapshotId::new("snap_current");
    store
        .insert_workspace(&workspace_id, "Code", "2026-07-15T18:00:00Z")
        .expect("workspace insert");
    let blocked = task(
        "task-stale-blocked",
        &workspace_id,
        &stale_snapshot_id,
        "app/a-blocked.txt",
        MaterializationPriorityClass::CorrectnessCritical,
    );
    let ready = task(
        "task-stale-ready",
        &workspace_id,
        &stale_snapshot_id,
        "app/b-ready.txt",
        MaterializationPriorityClass::SmallFile,
    );
    store
        .reconcile_materialization_tasks(
            &workspace_id,
            &stale_snapshot_id,
            &[blocked.clone(), ready.clone()],
            "2026-07-15T18:00:00Z",
        )
        .expect("stale tasks");
    accept_snapshot(&store, &workspace_id, &stale_snapshot_id, 1);

    let claimed_blocked = store
        .claim_next_materialization_task(
            &workspace_id,
            "materializer-test",
            "claim-blocked",
            "2026-07-15T18:00:01Z",
        )
        .expect("claim blocked")
        .expect("blocked task");
    assert_eq!(claimed_blocked.id, blocked.id);
    assert!(
        store
            .finish_materialization_task(&MaterializationTaskFinish {
                id: &blocked.id,
                claim_token: "claim-blocked",
                claim_generation: claimed_blocked.claim_generation,
                state: MaterializationTaskState::BlockedConflict,
                error_kind: Some(crate::metadata::MaterializationFailureKind::PathFenceNotCurrent),
                error: Some("stale local conflict"),
                not_before: None,
                now: "2026-07-15T18:00:02Z",
            })
            .expect("finish blocked")
    );
    let claimed_ready = store
        .claim_next_materialization_task(
            &workspace_id,
            "materializer-test",
            "claim-ready",
            "2026-07-15T18:00:03Z",
        )
        .expect("claim ready")
        .expect("ready task");
    assert_eq!(claimed_ready.id, ready.id);
    assert!(
        store
            .finish_materialization_task(&MaterializationTaskFinish {
                id: &ready.id,
                claim_token: "claim-ready",
                claim_generation: claimed_ready.claim_generation,
                state: MaterializationTaskState::Ready,
                error_kind: None,
                error: None,
                not_before: None,
                now: "2026-07-15T18:00:04Z",
            })
            .expect("finish ready")
    );
    store
        .upsert_materialization_path_state(&MaterializationPathStateRecord {
            workspace_id: workspace_id.clone(),
            project_id: None,
            path: blocked.path.clone(),
            snapshot_id: Some(stale_snapshot_id.clone()),
            expected_content_id: blocked.expected_content_id.clone(),
            state: MaterializationPathState::BlockedConflict,
            observed_content_id: None,
            observed_byte_len: None,
            source_hydration_state: None,
            verified_at: None,
            updated_at: "2026-07-15T18:00:04Z".to_string(),
        })
        .expect("stale path blocker");

    accept_snapshot(&store, &workspace_id, &current_snapshot_id, 2);

    for task_id in [&blocked.id, &ready.id] {
        let stale = store
            .materialization_task(task_id)
            .expect("stale task query")
            .expect("stale task retained as terminal history");
        assert_eq!(stale.state, MaterializationTaskState::Cancelled);
        assert!(stale.last_error_kind.is_none());
        assert!(stale.last_error.is_none());
    }
    assert!(
        store
            .materialization_tasks(&workspace_id)
            .expect("active tasks")
            .is_empty()
    );
    assert!(
        store
            .materialization_path_state(&workspace_id, &blocked.path)
            .expect("path state query")
            .is_none()
    );
}

#[test]
fn ignored_stale_head_write_preserves_authoritative_materialization_tasks() {
    let temp = TempWorkspace::new("materialization-ignored-stale-head").expect("temp workspace");
    let store = MetadataStore::open(temp.root().join("state.sqlite3")).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_ignored_stale_head");
    let current_snapshot_id = SnapshotId::new("snap_current");
    let stale_snapshot_id = SnapshotId::new("snap_stale");
    store
        .insert_workspace(&workspace_id, "Code", "2026-07-15T18:30:00Z")
        .expect("workspace insert");
    accept_snapshot(&store, &workspace_id, &current_snapshot_id, 2);
    let current = task(
        "task-current",
        &workspace_id,
        &current_snapshot_id,
        "app/current.txt",
        MaterializationPriorityClass::SmallFile,
    );
    store
        .reconcile_materialization_tasks(
            &workspace_id,
            &current_snapshot_id,
            std::slice::from_ref(&current),
            "2026-07-15T18:30:01Z",
        )
        .expect("current task");

    accept_snapshot(&store, &workspace_id, &stale_snapshot_id, 1);

    assert_eq!(
        store
            .materialization_task(&current.id)
            .expect("current task query")
            .expect("current task retained")
            .state,
        MaterializationTaskState::Queued
    );
    assert_eq!(
        store
            .workspace_sync_head(&workspace_id)
            .expect("head query")
            .expect("authoritative head")
            .workspace_ref
            .snapshot_id,
        current_snapshot_id
    );
}

#[test]
fn claims_are_deterministic_and_completion_is_token_fenced() {
    let temp = TempWorkspace::new("materialization-claims").expect("temp workspace");
    let store = MetadataStore::open(temp.root().join("state.sqlite3")).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_claims");
    let snapshot_id = SnapshotId::new("snap_claims");
    store
        .insert_workspace(&workspace_id, "Code", "2026-07-13T00:00:00Z")
        .expect("workspace insert");
    let tasks = [
        task(
            "task-background",
            &workspace_id,
            &snapshot_id,
            "large.bin",
            MaterializationPriorityClass::BackgroundLarge,
        ),
        task(
            "task-active",
            &workspace_id,
            &snapshot_id,
            "app/main.rs",
            MaterializationPriorityClass::ActiveProject,
        ),
    ];
    store
        .reconcile_materialization_tasks(
            &workspace_id,
            &snapshot_id,
            &tasks,
            "2026-07-13T00:00:00Z",
        )
        .expect("reconcile");

    let claimed = store
        .claim_next_materialization_task(
            &workspace_id,
            "worker-a",
            "claim-a",
            "2026-07-13T00:01:00Z",
        )
        .expect("claim")
        .expect("ready task");
    assert_eq!(claimed.id, MaterializationTaskId::new("task-active"));
    assert_eq!(claimed.attempt_count, 1);
    assert_eq!(claimed.claim_generation, 1);
    assert!(
        !store
            .finish_materialization_task(&MaterializationTaskFinish {
                id: &claimed.id,
                claim_token: "wrong-token",
                claim_generation: claimed.claim_generation,
                state: MaterializationTaskState::Ready,
                error_kind: None,
                error: None,
                not_before: None,
                now: "2026-07-13T00:01:30Z",
            },)
            .expect("wrong token rejected")
    );
    assert!(
        store
            .finish_materialization_task(&MaterializationTaskFinish {
                id: &claimed.id,
                claim_token: "claim-a",
                claim_generation: claimed.claim_generation,
                state: MaterializationTaskState::Ready,
                error_kind: None,
                error: None,
                not_before: None,
                now: "2026-07-13T00:01:59Z",
            },)
            .expect("right token accepted")
    );
}

#[test]
fn expired_materialization_claim_is_reclaimed_and_stale_worker_is_fenced() {
    let temp = TempWorkspace::new("materialization-claim-expiry").expect("temp workspace");
    let store = MetadataStore::open(temp.root().join("state.sqlite3")).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_claim_expiry");
    let snapshot_id = SnapshotId::new("snap_claim_expiry");
    store
        .insert_workspace(&workspace_id, "Code", "2026-07-13T00:00:00Z")
        .expect("workspace insert");
    let planned = task(
        "task-claim-expiry",
        &workspace_id,
        &snapshot_id,
        "app/main.rs",
        MaterializationPriorityClass::ActiveProject,
    );
    store
        .reconcile_materialization_tasks(
            &workspace_id,
            &snapshot_id,
            std::slice::from_ref(&planned),
            "2026-07-13T00:00:00Z",
        )
        .expect("reconcile");

    let claim_a = store
        .claim_next_materialization_task(
            &workspace_id,
            "worker-a",
            "token-a",
            "2026-07-13T00:01:00Z",
        )
        .expect("claim A")
        .expect("queued task");
    assert_eq!(claim_a.attempt_count, 1);
    assert_eq!(claim_a.claim_generation, 1);
    assert_eq!(
        claim_a.lease_expires_at.as_deref(),
        Some("2026-07-13T00:02:00Z")
    );
    assert!(
        store
            .claim_next_materialization_task(
                &workspace_id,
                "worker-b",
                "token-b",
                "2026-07-13T00:01:59Z",
            )
            .expect("pre-expiry claim attempt")
            .is_none()
    );
    assert!(
        store
            .renew_materialization_task_claim(
                &claim_a.id,
                "token-a",
                claim_a.claim_generation,
                "2026-07-13T00:01:30Z",
            )
            .expect("live claim renews")
    );
    assert!(
        store
            .claim_next_materialization_task(
                &workspace_id,
                "worker-b",
                "token-b",
                "2026-07-13T00:02:00Z",
            )
            .expect("original-expiry claim attempt")
            .is_none(),
        "renewal must prevent a live hydration from being stolen"
    );

    let claim_b = store
        .claim_next_materialization_task(
            &workspace_id,
            "worker-b",
            "token-b",
            "2026-07-13T00:02:30Z",
        )
        .expect("expired claim is reclaimable")
        .expect("claim B");
    assert_eq!(claim_b.attempt_count, 2);
    assert_eq!(claim_b.claim_generation, 2);
    assert_eq!(claim_b.claim_token.as_deref(), Some("token-b"));
    assert!(
        !store
            .materialization_task_fence_is_current(&MaterializationTaskFence {
                id: &claim_a.id,
                claim_token: "token-a",
                claim_generation: claim_a.claim_generation,
                snapshot_id: &snapshot_id,
                path: &claim_a.path,
                expected_kind: claim_a.expected_kind,
                expected_content_id: claim_a.expected_content_id.as_ref(),
                settled_write_matches_base: false,
                unresolved_conflict_paths: &BTreeSet::new(),
                now: "2026-07-13T00:02:31Z",
            })
            .expect("stale fence check")
    );
    assert!(
        !store
            .finish_materialization_task(&MaterializationTaskFinish {
                id: &claim_a.id,
                claim_token: "token-a",
                claim_generation: claim_a.claim_generation,
                state: MaterializationTaskState::Staged,
                error_kind: None,
                error: None,
                not_before: None,
                now: "2026-07-13T00:02:31Z",
            })
            .expect("stale finish is rejected")
    );
    assert!(
        store
            .finish_materialization_task(&MaterializationTaskFinish {
                id: &claim_b.id,
                claim_token: "token-b",
                claim_generation: claim_b.claim_generation,
                state: MaterializationTaskState::Staged,
                error_kind: None,
                error: None,
                not_before: None,
                now: "2026-07-13T00:02:31Z",
            })
            .expect("current worker stages")
    );
}

#[test]
fn reconcile_reactivates_same_snapshot_conflict_for_fresh_fence_evaluation() {
    let temp = TempWorkspace::new("materialization-reactivate-conflict").expect("temp workspace");
    let store = MetadataStore::open(temp.root().join("state.sqlite3")).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_reactivate_conflict");
    let snapshot_id = SnapshotId::new("snap_reactivate_conflict");
    store
        .insert_workspace(&workspace_id, "Code", "2026-07-13T00:00:00Z")
        .expect("workspace insert");
    let planned = task(
        "task-reactivate-conflict",
        &workspace_id,
        &snapshot_id,
        "app/value.txt",
        MaterializationPriorityClass::ActiveProject,
    );
    store
        .reconcile_materialization_tasks(
            &workspace_id,
            &snapshot_id,
            std::slice::from_ref(&planned),
            "2026-07-13T00:00:00Z",
        )
        .expect("initial reconcile");
    let claimed = store
        .claim_next_materialization_task(
            &workspace_id,
            "materializer-reactivate",
            "claim-reactivate",
            "2026-07-13T00:01:00Z",
        )
        .expect("claim")
        .expect("task");
    assert!(
        store
            .finish_materialization_task(&MaterializationTaskFinish {
                id: &claimed.id,
                claim_token: "claim-reactivate",
                claim_generation: claimed.claim_generation,
                state: MaterializationTaskState::BlockedConflict,
                error_kind: Some(MaterializationFailureKind::PathFenceNotCurrent),
                error: Some("old fence result"),
                not_before: None,
                now: "2026-07-13T00:01:01Z",
            })
            .expect("block task")
    );

    let report = store
        .reconcile_materialization_tasks(
            &workspace_id,
            &snapshot_id,
            std::slice::from_ref(&planned),
            "2026-07-13T00:02:00Z",
        )
        .expect("repeat reconcile");
    assert_eq!(report.inserted, 0);
    assert_eq!(report.reactivated, 1);
    let reactivated = store
        .materialization_task(&planned.id)
        .expect("task query")
        .expect("task retained");
    assert_eq!(reactivated.state, MaterializationTaskState::Queued);
    assert!(reactivated.last_error_kind.is_none());
    assert!(reactivated.last_error.is_none());
}

#[test]
fn claimed_task_fence_rejects_newer_local_work_before_snapshot_acceptance() {
    let temp = TempWorkspace::new("materialization-path-fence").expect("temp workspace");
    let store = MetadataStore::open(temp.root().join("state.sqlite3")).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_path_fence");
    let snapshot_id = SnapshotId::new("snap_path_fence");
    store
        .insert_workspace(&workspace_id, "Code", "2026-07-13T00:00:00Z")
        .expect("workspace insert");
    let planned = task(
        "task-path-fence",
        &workspace_id,
        &snapshot_id,
        "app/value.txt",
        MaterializationPriorityClass::ActiveProject,
    );
    store
        .reconcile_materialization_tasks(
            &workspace_id,
            &snapshot_id,
            std::slice::from_ref(&planned),
            "2026-07-13T00:00:00Z",
        )
        .expect("reconcile");
    let claimed = store
        .claim_next_materialization_task(
            &workspace_id,
            "materializer-a",
            "claim-path-fence",
            "2026-07-13T00:01:00Z",
        )
        .expect("claim")
        .expect("task");
    assert!(
        store
            .materialization_task_fence_is_current(&MaterializationTaskFence {
                id: &claimed.id,
                claim_token: "claim-path-fence",
                claim_generation: claimed.claim_generation,
                snapshot_id: &snapshot_id,
                path: &claimed.path,
                expected_kind: claimed.expected_kind,
                expected_content_id: claimed.expected_content_id.as_ref(),
                settled_write_matches_base: false,
                unresolved_conflict_paths: &BTreeSet::new(),
                now: "2026-07-13T00:01:30Z",
            })
            .expect("current fence"),
        "the task may run before the target snapshot becomes the local head"
    );

    // The watcher can share the sync timestamp; insertion order still makes this
    // local write newer than the already-created task.
    record_local_write(
        &store,
        "write-path-fence",
        &workspace_id,
        &claimed.path,
        "2026-07-13T00:01:01Z",
        "2026-07-13T00:00:00Z",
    );
    assert!(
        !store
            .materialization_task_fence_is_current(&MaterializationTaskFence {
                id: &claimed.id,
                claim_token: "claim-path-fence",
                claim_generation: claimed.claim_generation,
                snapshot_id: &snapshot_id,
                path: &claimed.path,
                expected_kind: claimed.expected_kind,
                expected_content_id: claimed.expected_content_id.as_ref(),
                settled_write_matches_base: false,
                unresolved_conflict_paths: &BTreeSet::new(),
                now: "2026-07-13T00:01:30Z",
            })
            .expect("blocked fence"),
        "newer local work must revoke the per-path mutation fence"
    );
    assert!(
        store
            .materialization_task_fence_is_current(&MaterializationTaskFence {
                id: &claimed.id,
                claim_token: "claim-path-fence",
                claim_generation: claimed.claim_generation,
                snapshot_id: &snapshot_id,
                path: &claimed.path,
                expected_kind: claimed.expected_kind,
                expected_content_id: claimed.expected_content_id.as_ref(),
                settled_write_matches_base: true,
                unresolved_conflict_paths: &BTreeSet::new(),
                now: "2026-07-13T00:01:30Z",
            })
            .expect("base-matching fence"),
        "settled watcher noise may not fence unchanged base bytes"
    );
}

#[test]
fn directory_tombstone_fence_rejects_settled_descendant_write() {
    let temp =
        TempWorkspace::new("materialization-directory-delete-fence").expect("temp workspace");
    let store = MetadataStore::open(temp.root().join("state.sqlite3")).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_directory_delete_fence");
    let snapshot_id = SnapshotId::new("snap_directory_delete_fence");
    store
        .insert_workspace(&workspace_id, "Code", "2026-07-13T00:00:00Z")
        .expect("workspace insert");
    let mut planned = task(
        "task-directory-delete-fence",
        &workspace_id,
        &snapshot_id,
        "app/removed",
        MaterializationPriorityClass::CorrectnessCritical,
    );
    planned.expected_kind = NamespaceEntryKind::Tombstone;
    planned.expected_content_id = None;
    planned.expected_byte_len = 0;
    store
        .reconcile_materialization_tasks(
            &workspace_id,
            &snapshot_id,
            std::slice::from_ref(&planned),
            "2026-07-13T00:00:00Z",
        )
        .expect("reconcile");
    let claimed = store
        .claim_next_materialization_task(
            &workspace_id,
            "materializer-directory-delete",
            "claim-directory-delete",
            "2026-07-13T00:01:00Z",
        )
        .expect("claim")
        .expect("task");
    record_local_write(
        &store,
        "write-new-descendant",
        &workspace_id,
        "app/removed/new.txt",
        "2026-07-13T00:01:02Z",
        "2026-07-13T00:01:01Z",
    );

    assert!(
        !store
            .materialization_task_fence_is_current(&MaterializationTaskFence {
                id: &claimed.id,
                claim_token: "claim-directory-delete",
                claim_generation: claimed.claim_generation,
                snapshot_id: &snapshot_id,
                path: &claimed.path,
                expected_kind: claimed.expected_kind,
                expected_content_id: claimed.expected_content_id.as_ref(),
                settled_write_matches_base: false,
                unresolved_conflict_paths: &BTreeSet::new(),
                now: "2026-07-13T00:01:30Z",
            })
            .expect("directory deletion fence"),
        "a settled descendant write must preserve new local work"
    );
}

#[test]
fn claimed_child_task_fence_uses_canonical_exact_and_ancestor_conflicts() {
    let temp = TempWorkspace::new("materialization-ancestor-fence").expect("temp workspace");
    let state_root = temp.root().join("state");
    let store = MetadataStore::open(state_root.join("metadata.sqlite3")).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_ancestor_fence");
    let snapshot_id = SnapshotId::new("snap_ancestor_fence");
    store
        .insert_workspace(&workspace_id, "Code", "2026-07-13T00:00:00Z")
        .expect("workspace insert");
    let planned = task(
        "task-ancestor-fence",
        &workspace_id,
        &snapshot_id,
        "vendor/package/file.txt",
        MaterializationPriorityClass::ActiveProject,
    );
    store
        .reconcile_materialization_tasks(
            &workspace_id,
            &snapshot_id,
            std::slice::from_ref(&planned),
            "2026-07-13T00:00:00Z",
        )
        .expect("reconcile");
    let claimed = store
        .claim_next_materialization_task(
            &workspace_id,
            "materializer-ancestor",
            "claim-ancestor-fence",
            "2026-07-13T00:01:00Z",
        )
        .expect("claim")
        .expect("task");

    record_local_write(
        &store,
        "write-sibling-prefix",
        &workspace_id,
        "vendor-old",
        "2026-07-13T00:01:01Z",
        "2026-07-13T00:01:01Z",
    );
    record_local_write(
        &store,
        "write-wildcard-literal",
        &workspace_id,
        "vendor/%",
        "",
        "2026-07-13T00:01:01Z",
    );
    assert!(task_fence_is_current(
        &store,
        &claimed,
        "claim-ancestor-fence",
        &BTreeSet::new(),
    ));

    record_local_write(
        &store,
        "write-unsettled-ancestor",
        &workspace_id,
        "vendor",
        "",
        "2026-07-12T23:59:00Z",
    );
    assert!(!task_fence_is_current(
        &store,
        &claimed,
        "claim-ancestor-fence",
        &BTreeSet::new(),
    ));
    store
        .delete_local_write(&workspace_id, "write-unsettled-ancestor")
        .expect("delete unsettled ancestor");

    record_local_write(
        &store,
        "write-settled-ancestor",
        &workspace_id,
        "vendor/package",
        "2026-07-13T00:02:00Z",
        "2026-07-13T00:02:00Z",
    );
    assert!(!task_fence_is_current(
        &store,
        &claimed,
        "claim-ancestor-fence",
        &BTreeSet::new(),
    ));
    store
        .delete_local_write(&workspace_id, "write-settled-ancestor")
        .expect("delete settled ancestor");

    let exact = create_test_conflict(&state_root, "vendor/package/file.txt");
    let exact_paths = unresolved_conflict_paths(&state_root).expect("exact conflict paths");
    assert!(!task_fence_is_current(
        &store,
        &claimed,
        "claim-ancestor-fence",
        &exact_paths,
    ));
    resolve_test_conflict(&exact, ConflictState::Accepted);
    assert!(task_fence_is_current(
        &store,
        &claimed,
        "claim-ancestor-fence",
        &unresolved_conflict_paths(&state_root).expect("resolved exact paths"),
    ));

    let ancestor = create_test_conflict(&state_root, "vendor/package");
    let ancestor_paths = unresolved_conflict_paths(&state_root).expect("ancestor conflict paths");
    assert!(!task_fence_is_current(
        &store,
        &claimed,
        "claim-ancestor-fence",
        &ancestor_paths,
    ));
    resolve_test_conflict(&ancestor, ConflictState::Rejected);
    assert!(task_fence_is_current(
        &store,
        &claimed,
        "claim-ancestor-fence",
        &unresolved_conflict_paths(&state_root).expect("resolved ancestor paths"),
    ));

    let sibling = create_test_conflict(&state_root, "vendor-old");
    let wildcard_literal = create_test_conflict(&state_root, "vendor/_");
    assert!(task_fence_is_current(
        &store,
        &claimed,
        "claim-ancestor-fence",
        &unresolved_conflict_paths(&state_root).expect("non-overlapping conflict paths"),
    ));
    resolve_test_conflict(&sibling, ConflictState::Accepted);
    resolve_test_conflict(&wildcard_literal, ConflictState::Accepted);
}

#[test]
fn claimed_tombstone_task_fence_rejects_descendant_local_ownership() {
    let temp = TempWorkspace::new("materialization-delete-fence").expect("temp workspace");
    let state_root = temp.root().join("state");
    let store = MetadataStore::open(state_root.join("metadata.sqlite3")).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_delete_fence");
    let snapshot_id = SnapshotId::new("snap_delete_fence");
    store
        .insert_workspace(&workspace_id, "Code", "2026-07-13T00:00:00Z")
        .expect("workspace insert");
    let mut planned = task(
        "task-delete-fence",
        &workspace_id,
        &snapshot_id,
        "vendor",
        MaterializationPriorityClass::Cleanup,
    );
    planned.expected_kind = NamespaceEntryKind::Tombstone;
    planned.expected_content_id = None;
    store
        .reconcile_materialization_tasks(
            &workspace_id,
            &snapshot_id,
            std::slice::from_ref(&planned),
            "2026-07-13T00:00:00Z",
        )
        .expect("reconcile");
    let claimed = store
        .claim_next_materialization_task(
            &workspace_id,
            "materializer-delete",
            "claim-delete-fence",
            "2026-07-13T00:01:00Z",
        )
        .expect("claim")
        .expect("task");

    record_local_write(
        &store,
        "write-delete-descendant",
        &workspace_id,
        "vendor/local/file.txt",
        "2026-07-13T00:02:00Z",
        "2026-07-13T00:02:00Z",
    );
    assert!(!task_fence_is_current(
        &store,
        &claimed,
        "claim-delete-fence",
        &BTreeSet::new(),
    ));
    store
        .delete_local_write(&workspace_id, "write-delete-descendant")
        .expect("delete descendant write");

    let descendant = create_test_conflict(&state_root, "vendor/local/file.txt");
    let descendant_paths =
        unresolved_conflict_paths(&state_root).expect("descendant conflict paths");
    assert!(!task_fence_is_current(
        &store,
        &claimed,
        "claim-delete-fence",
        &descendant_paths,
    ));
    resolve_test_conflict(&descendant, ConflictState::Accepted);
    assert!(task_fence_is_current(
        &store,
        &claimed,
        "claim-delete-fence",
        &unresolved_conflict_paths(&state_root).expect("resolved descendant paths"),
    ));
}

#[test]
fn snapshot_completion_updates_only_the_exact_snapshot_and_path_state() {
    let temp = TempWorkspace::new("materialization-snapshot-complete").expect("temp workspace");
    let store = MetadataStore::open(temp.root().join("state.sqlite3")).expect("metadata opens");
    let workspace_id = WorkspaceId::new("ws_snapshot_complete");
    let snapshot_id = SnapshotId::new("snap_complete");
    store
        .insert_workspace(&workspace_id, "Code", "2026-07-13T00:00:00Z")
        .expect("workspace insert");
    let planned = task(
        "task-complete",
        &workspace_id,
        &snapshot_id,
        "app/complete.txt",
        MaterializationPriorityClass::ActiveProject,
    );
    store
        .reconcile_materialization_tasks(
            &workspace_id,
            &snapshot_id,
            std::slice::from_ref(&planned),
            "2026-07-13T00:00:00Z",
        )
        .expect("reconcile");

    assert_eq!(
        store
            .complete_materialization_snapshot(
                &workspace_id,
                &SnapshotId::new("snap-other"),
                "2026-07-13T00:01:00Z",
            )
            .expect("other snapshot completion"),
        0
    );
    assert!(
        store
            .materialization_path_state(&workspace_id, "app/complete.txt")
            .expect("path state query")
            .is_none()
    );

    accept_snapshot(&store, &workspace_id, &snapshot_id, 1);
    assert_eq!(
        store
            .complete_materialization_snapshot(&workspace_id, &snapshot_id, "2026-07-13T00:01:30Z")
            .expect("queued snapshot is not ready"),
        0,
        "accepted head alone must not make queued paths ready"
    );
    claim_and_stage(
        &store,
        &workspace_id,
        &planned.id,
        "claim-complete",
        "2026-07-13T00:01:45Z",
    );

    assert_eq!(
        store
            .complete_materialization_snapshot(&workspace_id, &snapshot_id, "2026-07-13T00:02:00Z",)
            .expect("exact snapshot completion"),
        1
    );
    let state = store
        .materialization_path_state(&workspace_id, "app/complete.txt")
        .expect("path state query")
        .expect("ready path state");
    assert_eq!(state.state.as_str(), "ready");
    assert_eq!(state.snapshot_id.as_ref(), Some(&snapshot_id));
    assert_eq!(state.observed_content_id, planned.expected_content_id);
    assert_eq!(state.observed_byte_len, Some(planned.expected_byte_len));

    let next_snapshot_id = SnapshotId::new("snap_complete_next");
    let next = task(
        "task-complete-next",
        &workspace_id,
        &next_snapshot_id,
        "app/complete.txt",
        MaterializationPriorityClass::ActiveProject,
    );
    let report = store
        .reconcile_materialization_tasks(
            &workspace_id,
            &next_snapshot_id,
            std::slice::from_ref(&next),
            "2026-07-13T00:03:00Z",
        )
        .expect("next snapshot reconcile");
    assert_eq!(report.cancelled, 1, "superseded ready task is retired");
    let active = store
        .materialization_tasks(&workspace_id)
        .expect("active tasks");
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].snapshot_id, next_snapshot_id);

    accept_snapshot(&store, &workspace_id, &next_snapshot_id, 2);
    claim_and_stage(
        &store,
        &workspace_id,
        &next.id,
        "claim-next",
        "2026-07-13T00:03:30Z",
    );
    assert_eq!(
        store
            .complete_materialization_snapshot(
                &workspace_id,
                &next_snapshot_id,
                "2026-07-13T00:04:00Z",
            )
            .expect("next snapshot completion"),
        1
    );
    let advanced = store
        .materialization_path_state(&workspace_id, "app/complete.txt")
        .expect("advanced path query")
        .expect("advanced path state");
    assert_eq!(advanced.snapshot_id.as_ref(), Some(&next_snapshot_id));
    assert_eq!(advanced.observed_content_id, next.expected_content_id);
}

fn accept_snapshot(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    snapshot_id: &SnapshotId,
    version: u64,
) {
    store
        .upsert_workspace_sync_head(&WorkspaceSyncHeadRecord {
            workspace_ref: WorkspaceRef {
                workspace_id: workspace_id.clone(),
                version,
                snapshot_id: snapshot_id.clone(),
                updated_at: ControlPlaneTimestamp { tick: version },
                updated_by_device_id: Some(DeviceId::new("device-materialization-test")),
            },
            observed_at: format!("2026-07-13T00:0{version}:00Z"),
        })
        .expect("accepted snapshot head");
}

fn claim_and_stage(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    expected_id: &MaterializationTaskId,
    claim_token: &str,
    now: &str,
) {
    let claimed = store
        .claim_next_materialization_task(workspace_id, "materializer-test", claim_token, now)
        .expect("claim task")
        .expect("queued task");
    assert_eq!(&claimed.id, expected_id);
    assert!(
        store
            .finish_materialization_task(&MaterializationTaskFinish {
                id: &claimed.id,
                claim_token,
                claim_generation: claimed.claim_generation,
                state: MaterializationTaskState::Staged,
                error_kind: None,
                error: None,
                not_before: None,
                now,
            })
            .expect("stage claimed task")
    );
}

fn task_fence_is_current(
    store: &MetadataStore,
    task: &MaterializationTaskRecord,
    claim_token: &str,
    conflict_paths: &BTreeSet<String>,
) -> bool {
    store
        .materialization_task_fence_is_current(&MaterializationTaskFence {
            id: &task.id,
            claim_token,
            claim_generation: task.claim_generation,
            snapshot_id: &task.snapshot_id,
            path: &task.path,
            expected_kind: task.expected_kind,
            expected_content_id: task.expected_content_id.as_ref(),
            settled_write_matches_base: false,
            unresolved_conflict_paths: conflict_paths,
            now: "2026-07-13T00:01:30Z",
        })
        .expect("task fence query")
}

fn record_local_write(
    store: &MetadataStore,
    id: &str,
    workspace_id: &WorkspaceId,
    path: &str,
    settled_at: &str,
    created_at: &str,
) {
    store
        .append_local_write_log(&LocalWriteLogRecord {
            id: id.to_string(),
            workspace_id: workspace_id.clone(),
            device_id: DeviceId::new("device-local-writer"),
            project_id: None,
            path: path.to_string(),
            source_path: None,
            operation: "modify".to_string(),
            staged_content_id: None,
            policy_classification: PathClassification::WorkspaceSync,
            causation_id: "local-edit".to_string(),
            settled_at: settled_at.to_string(),
            created_at: created_at.to_string(),
        })
        .expect("record local write");
}

fn create_test_conflict(state_root: &Path, path: &str) -> ConflictBundle {
    create_conflict_bundle(state_root, ConflictRecord::path_conflict(path), &[])
        .expect("create canonical conflict bundle")
}

fn resolve_test_conflict(bundle: &ConflictBundle, state: ConflictState) {
    assert!(
        transition_conflict_occurrence_state(
            &bundle.root,
            &bundle.record.id,
            bundle.record.occurrence_version,
            state,
            "2026-07-13T00:02:00Z",
        )
        .expect("resolve canonical conflict")
    );
}

fn task(
    id: &str,
    workspace_id: &WorkspaceId,
    snapshot_id: &SnapshotId,
    path: &str,
    priority_class: MaterializationPriorityClass,
) -> MaterializationTaskRecord {
    MaterializationTaskRecord {
        id: MaterializationTaskId::new(id),
        workspace_id: workspace_id.clone(),
        project_id: None,
        snapshot_id: snapshot_id.clone(),
        path: path.to_string(),
        expected_kind: NamespaceEntryKind::File,
        expected_content_id: Some(ContentId::new(format!("content-{id}"))),
        expected_byte_len: 12,
        expected_executable: false,
        priority_class,
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
        created_at: "2026-07-13T00:00:00Z".to_string(),
        updated_at: "2026-07-13T00:00:00Z".to_string(),
    }
}
