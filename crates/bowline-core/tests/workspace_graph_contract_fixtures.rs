// Integration-test crate: fixture helpers may panic (clippy only exempts #[test] fns).
#![allow(clippy::panic)]

use bowline_core::workspace_graph::{
    ContentLayout, SnapshotManifest, normalize_workspace_path, workspace_content_id,
};
use serde_json::Value;

#[path = "support/contract_fixtures.rs"]
mod contract_fixtures;

use contract_fixtures::{fixture_json, manifest_entries_for_rust};

#[test]
fn rust_snapshot_json_matches_shared_contract_fixture() {
    for fixture in manifest_entries_for_rust("snapshots", Some("SnapshotManifest")) {
        assert_eq!(fixture.format, "json");
        assert_eq!(fixture.kind, "SnapshotManifest");

        let expected = fixture_json(&fixture.path);
        let manifest: SnapshotManifest =
            serde_json::from_value(expected.clone()).expect("fixture parses as snapshot manifest");
        let actual = serde_json::to_value(&manifest).expect("snapshot manifest serializes");

        assert_eq!(actual, expected, "{} changed on round trip", fixture.id);
        assert!(manifest.namespace_root_id.as_str().starts_with("nsp_"));
        assert_eq!(manifest.entry_count, 7);
    }
}

#[test]
fn rust_content_layout_json_matches_shared_contract_fixtures() {
    for fixture in manifest_entries_for_rust("snapshots", Some("ContentLayout")) {
        assert_eq!(fixture.format, "json");
        assert_eq!(fixture.kind, "ContentLayout");

        let expected = fixture_json(&fixture.path);
        let layout: ContentLayout =
            serde_json::from_value(expected.clone()).expect("fixture parses as content layout");
        let actual = serde_json::to_value(&layout).expect("content layout serializes");

        assert_eq!(actual, expected, "{} changed on round trip", fixture.id);
    }
}

#[test]
fn content_ids_are_keyed_and_path_independent() {
    let first = workspace_content_id([1_u8; 32], b"hello");
    let second = workspace_content_id([1_u8; 32], b"hello");
    let other_workspace = workspace_content_id([2_u8; 32], b"hello");

    assert_eq!(first, second);
    assert_ne!(first, other_workspace);
    assert!(!first.as_str().contains("acme/web"));
}

#[test]
fn path_normalization_keeps_workspace_relative_shape() {
    assert_eq!(normalize_workspace_path("./acme//web/src/"), "acme/web/src");
}

#[test]
fn rust_snapshot_json_rejects_non_current_layout_grammar() {
    let invalid_kind = mixed_tree_with_first_layout(|layout| {
        layout["kind"] = Value::String("packed-record-v1".to_string());
    });
    serde_json::from_value::<ContentLayout>(invalid_kind)
        .expect_err("non-canonical layout kind should fail");

    let extra_layout_field = mixed_tree_with_first_layout(|layout| {
        layout.insert(
            "unexpectedLayoutField".to_string(),
            Value::String("stale".to_string()),
        );
    });
    serde_json::from_value::<ContentLayout>(extra_layout_field)
        .expect_err("extra layout fields should fail");

    let missing_segment_range = mixed_tree_with_first_layout(|layout| {
        layout["segments"][0]
            .as_object_mut()
            .expect("segment")
            .remove("length");
    });
    serde_json::from_value::<ContentLayout>(missing_segment_range)
        .expect_err("segments without ranges should fail");

    let noncontiguous_segments = mixed_tree_with_first_layout(|layout| {
        layout["segments"][0]["ordinal"] = Value::from(1);
    });
    serde_json::from_value::<ContentLayout>(noncontiguous_segments)
        .expect_err("noncontiguous segments should fail");
}

fn mixed_tree_with_first_layout(edit: impl FnOnce(&mut serde_json::Map<String, Value>)) -> Value {
    let mut fixture = fixture_json("snapshots/content-layout-segmented-v1.json");
    let layout = fixture.as_object_mut().expect("content layout fixture");
    edit(layout);
    fixture
}
