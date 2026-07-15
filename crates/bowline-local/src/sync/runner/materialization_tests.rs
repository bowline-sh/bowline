use super::*;
use crate::workspace::TempWorkspace;

#[test]
fn materialization_plan_writes_git_objects_before_pointer_state() {
    let target = tests::snapshot_with_files(
        WorkspaceId::new("ws_code"),
        &[
            (".git/refs/heads/main", b"abc123\n".as_slice()),
            (".git/HEAD", b"ref: refs/heads/main\n"),
            ("src/main.rs", b"fn main() {}\n"),
            (".git/objects/ab/cd", b"object"),
        ],
    );

    let plan = plan_materialization(None, &target, &BTreeSet::new()).expect("plan");
    let paths = plan
        .writes
        .iter()
        .map(|entry| entry.path.as_str())
        .collect::<Vec<_>>();

    assert_eq!(
        paths,
        vec![
            ".git/objects/ab/cd",
            "src/main.rs",
            ".git/HEAD",
            ".git/refs/heads/main"
        ]
    );
}

#[test]
fn materialization_removes_paths_that_become_intentionally_absent() {
    let workspace = TempWorkspace::new("sync-materialize-intentionally-absent").expect("workspace");
    let workspace_id = WorkspaceId::new("ws_code");
    let base = tests::snapshot_with_files(
        workspace_id.clone(),
        &[("generated/config.json", b"remote bytes".as_slice())],
    );
    materialize_snapshot(workspace.root(), None, &base).expect("materialize base");
    let target = tests::snapshot_with_files(
        workspace_id,
        &[("generated/config.json", b"obsolete remote bytes".as_slice())],
    );
    let intentionally_absent = BTreeSet::from(["generated/config.json".to_string()]);

    materialize_snapshot_omitting(
        workspace.root(),
        Some(&base),
        &target,
        &intentionally_absent,
    )
    .expect("omit excluded target");

    assert!(!workspace.root().join("generated/config.json").exists());
}

#[test]
fn materialization_plan_defers_git_object_deletes_until_after_writes() {
    let workspace_id = WorkspaceId::new("ws_code");
    let base = tests::snapshot_with_files(
        workspace_id.clone(),
        &[
            (".git/objects/pack/pack-old.pack", b"old pack".as_slice()),
            ("src/old.rs", b"old source"),
        ],
    );
    let target = empty_snapshot_content(workspace_id, SnapshotId::new("snap_target"), [7; 32])
        .expect("empty target");

    let plan = plan_materialization(Some(&base), &target, &BTreeSet::new()).expect("plan");

    assert_eq!(
        plan.deletes_first
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>(),
        vec!["src/old.rs"]
    );
    assert_eq!(
        plan.deletes_last
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>(),
        vec![".git/objects/pack/pack-old.pack"]
    );
}

#[test]
fn missing_target_payload_preserves_the_previous_materialized_snapshot() {
    let workspace = TempWorkspace::new("sync-materialize-preflight").expect("workspace");
    let workspace_id = WorkspaceId::new("ws_code");
    let base = tests::snapshot_with_files(
        workspace_id.clone(),
        &[("src/old.rs", b"old source".as_slice())],
    );
    materialize_snapshot(workspace.root(), None, &base).expect("materialize base");
    let target_with_bytes =
        tests::snapshot_with_files(workspace_id, &[("src/new.rs", b"new source".as_slice())]);
    let target = SnapshotContent::new(
        bowline_core::workspace_graph::SnapshotDraft::from_manifest(
            target_with_bytes.manifest().clone(),
            target_with_bytes.entries_for_test(),
        ),
        BTreeMap::new(),
        [7; 32],
    )
    .expect("page-backed target without content");

    let error = materialize_snapshot(workspace.root(), Some(&base), &target)
        .expect_err("missing target payload");

    assert!(matches!(
        error,
        SyncRunnerError::MissingMaterializationContent(path) if path == "src/new.rs"
    ));
    assert_eq!(
        fs::read(workspace.root().join("src/old.rs")).expect("old source remains"),
        b"old source"
    );
    assert!(!workspace.root().join("src/new.rs").exists());
}

#[test]
fn selected_path_planning_reads_one_descriptor_without_loading_layouts() {
    let target = tests::snapshot_with_files(
        WorkspaceId::new("ws_bounded_materialization"),
        &[
            ("a/one.txt", b"one".as_slice()),
            ("b/two.txt", b"two".as_slice()),
            ("c/three.txt", b"three".as_slice()),
        ],
    );
    let mut operation = NamespaceOperationContext::uncancelled(
        NamespaceOperationBudget::new(1, 0, 0).with_metadata_limits(
            target.namespace_store().namespace_page_count(),
            0,
            0,
            target.namespace_store().total_encoded_bytes(),
        ),
    );

    let plan = plan_materialization_for_path_with_context(
        None,
        &target,
        &BTreeSet::new(),
        &BTreeSet::new(),
        "b/two.txt",
        &mut operation,
    )
    .expect("bounded selected-path plan");

    assert_eq!(plan.writes.len(), 1);
    assert_eq!(plan.writes[0].path, "b/two.txt");
    assert!(operation.counters().entries_visited <= 1);
    assert!(
        operation.counters().namespace_pages_loaded
            <= target.namespace_store().namespace_page_count()
    );
    assert_eq!(operation.counters().layout_records_loaded, 0);
}

#[test]
fn production_target_stream_has_exact_selected_path_metadata_bounds() {
    let target = tests::snapshot_with_files(
        WorkspaceId::new("ws_bounded_target_stream"),
        &[
            ("a/one.txt", b"one".as_slice()),
            ("b/two.txt", b"two".as_slice()),
            ("c/three.txt", b"three".as_slice()),
        ],
    );
    let mut paths = Vec::new();

    let counters = visit_materialization_targets(
        &target,
        &BTreeSet::new(),
        &BTreeSet::new(),
        Some("b/two.txt"),
        MaterializationTargetPhase::OrdinaryWrite,
        |entry| {
            paths.push(entry.path);
            Ok(())
        },
    )
    .expect("bounded production target stream");

    assert_eq!(paths, ["b/two.txt"]);
    assert!(counters.entries_visited <= 1);
    assert!(counters.namespace_pages_loaded <= target.namespace_store().namespace_page_count());
    assert_eq!(counters.layout_records_loaded, 0);
    assert!(counters.metadata_bytes <= target.namespace_store().total_encoded_bytes());
}

#[test]
fn materialization_planning_observes_namespace_cancellation() {
    struct Cancelled;
    impl bowline_core::namespace_snapshot::NamespaceCancellation for Cancelled {
        fn is_cancelled(&self) -> bool {
            true
        }
    }

    let target = tests::snapshot_with_files(
        WorkspaceId::new("ws_cancelled_materialization"),
        &[("src/main.rs", b"fn main() {}".as_slice())],
    );
    let mut operation = NamespaceOperationContext::new(
        NamespaceOperationBudget::new(1, 0, 0).with_metadata_limits(
            target.namespace_store().namespace_page_count(),
            0,
            0,
            target.namespace_store().total_encoded_bytes(),
        ),
        &Cancelled,
    );

    let error = plan_materialization_with_context(
        None,
        &target,
        &BTreeSet::new(),
        &BTreeSet::new(),
        &mut operation,
    )
    .expect_err("cancelled namespace plan");

    assert_eq!(error, NamespaceReadError::Cancelled);
    assert!(operation.counters().cancellation_checks > 0);
}
