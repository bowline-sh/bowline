use std::{fs, path::PathBuf};

use bowline_core::workspace_graph::{
    NamespaceEntryKind, SnapshotManifest, normalize_workspace_path, workspace_content_id,
};
use serde_json::Value;

#[test]
fn rust_snapshot_json_matches_shared_contract_fixture() {
    let expected = fixture_json("snapshots/mixed-tree");
    let manifest: SnapshotManifest =
        serde_json::from_value(expected.clone()).expect("fixture parses as snapshot manifest");
    let actual = serde_json::to_value(&manifest).expect("snapshot manifest serializes");

    assert_eq!(actual, expected);
    assert!(manifest.entries.iter().any(|entry| {
        entry.path == "acme/web/.git/HEAD" && entry.kind == NamespaceEntryKind::File
    }));
    assert!(manifest.entries.iter().any(|entry| {
        entry.path == "acme/web/docs/latest" && entry.kind == NamespaceEntryKind::Symlink
    }));
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

fn fixture_json(name: &str) -> Value {
    let path = fixtures_dir().join(format!("{name}.json"));
    let json = fs::read_to_string(&path).expect("snapshot fixture is readable");

    serde_json::from_str(&json).expect("snapshot fixture is valid JSON")
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/contracts")
}
