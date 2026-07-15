use super::*;

#[test]
fn root_shallow_stands_when_head_present_and_deep_files_bound() {
    let harness = Harness::new("effective-root-shallow-ok", ScanScope::RootShallow);
    let head = harness.seed_head(vec![
        file_entry("README.md", false),
        file_entry("app/src/main.rs", true),
    ]);
    let head_snapshot = harness.head_snapshot(&head);

    let effective = harness
        .runner
        .effective_scan_scope(Some(&head), Some(&head_snapshot))
        .expect("scope");

    assert_eq!(effective, ScanScope::RootShallow);
}

#[test]
fn root_shallow_degrades_on_unbound_deep_file() {
    let harness = Harness::with_operation("effective-root-shallow-unbound", ScanScope::RootShallow);
    let head = harness.seed_head(vec![file_entry("app/src/main.rs", false)]);
    let head_snapshot = harness.head_snapshot(&head);

    let effective = harness
        .runner
        .effective_scan_scope(Some(&head), Some(&head_snapshot))
        .expect("scope");

    assert_eq!(
        effective,
        ScanScope::Full(FullScanReason::HeadManifestUnavailable)
    );
    assert!(
        harness.degraded_with_reason("unbound-deep-file-entry"),
        "unbound deep file must emit the scoped-scan-degraded checkpoint"
    );
    assert!(
        harness.checkpoint_payloads_are_pathless(),
        "degrade checkpoints must not leak entry paths"
    );
}
