// Integration-test crate: fixture helpers may panic (clippy only exempts #[test] fns).
#![allow(clippy::panic)]

use bowline_core::{
    commands::WatchFrame,
    events::{EVENT_SCHEMA_VERSION, WorkspaceEvent},
    wire::{KNOWN_EVENT_NAMES, WIRE_SCHEMA_HASH},
};
use serde_json::Value;
use std::{fs, path::Path};

#[path = "support/contract_fixtures.rs"]
mod contract_fixtures;

use contract_fixtures::{
    contract_manifest, fixture_json, fixture_text, fixtures_dir, manifest_entries_for_rust,
};

#[test]
fn manifest_lists_every_contract_json_or_ndjson_fixture() {
    let manifest = contract_manifest();
    let mut listed = manifest
        .fixtures
        .iter()
        .map(|fixture| {
            assert!(
                matches!(fixture.format.as_str(), "json" | "ndjson"),
                "{} has unsupported fixture format {}",
                fixture.id,
                fixture.format
            );
            assert!(
                fixtures_dir().join(&fixture.path).exists(),
                "{} lists missing fixture {}",
                fixture.id,
                fixture.path
            );
            fixture.path.clone()
        })
        .collect::<Vec<_>>();
    listed.sort();

    let mut discovered = Vec::new();
    collect_fixture_paths(&fixtures_dir(), &fixtures_dir(), &mut discovered);
    discovered.sort();

    assert_eq!(
        listed, discovered,
        "contract manifest must exactly list every JSON and NDJSON fixture"
    );
}

#[test]
fn rust_event_json_matches_shared_contract_fixture() {
    for fixture in manifest_entries_for_rust("events", Some("WorkspaceEvent")) {
        assert_eq!(fixture.format, "json");
        assert_eq!(fixture.kind, "WorkspaceEvent");

        let expected = fixture_json(&fixture.path);
        let output: WorkspaceEvent =
            serde_json::from_value(expected.clone()).expect("fixture parses as event");
        let actual = serde_json::to_value(&output).expect("event serializes");

        assert_eq!(output.schema_version, EVENT_SCHEMA_VERSION);
        assert_eq!(actual, expected, "{} changed on round trip", fixture.id);
    }
}

#[test]
fn rust_watch_frames_match_shared_ndjson_fixture() {
    for fixture in manifest_entries_for_rust("streams", Some("WatchFrame")) {
        assert_eq!(fixture.format, "ndjson");
        assert_eq!(fixture.kind, "WatchFrame");
        let frames = fixture_text(&fixture.path)
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("watch frame json"))
            .collect::<Vec<_>>();

        assert!(!frames.is_empty());
        for expected in frames {
            let frame: WatchFrame =
                serde_json::from_value(expected.clone()).expect("fixture parses as watch frame");
            let actual = serde_json::to_value(&frame).expect("watch frame serializes");
            assert_eq!(actual, expected, "{} frame changed", fixture.id);
        }
    }
}

#[test]
fn generated_registry_matches_rust_known_event_names_and_schema_hash() {
    let registry = fixture_json("generated/registry.json");
    let names = registry["enumValues"]["EventName"]
        .as_array()
        .expect("generated EventName registry is an array")
        .iter()
        .map(|value| value.as_str().expect("event name is a string"))
        .collect::<Vec<_>>();

    assert_eq!(names, KNOWN_EVENT_NAMES);
    assert_eq!(registry["schemaHash"], WIRE_SCHEMA_HASH);
}

fn collect_fixture_paths(root: &Path, dir: &Path, found: &mut Vec<String>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|error| {
        panic!(
            "fixture directory is readable at {}: {error}",
            dir.display()
        )
    }) {
        let path = entry.expect("fixture directory entry is readable").path();
        if path.is_dir() {
            collect_fixture_paths(root, &path, found);
            continue;
        }

        if !is_contract_fixture(&path) {
            continue;
        }

        let relative = path
            .strip_prefix(root)
            .expect("fixture path is under root")
            .to_string_lossy()
            .replace('\\', "/");
        // manifest.json indexes the decoder fixtures; timestamps.json holds
        // shared RFC 3339 policy vectors for the timestamp-guard parity tests,
        // not a per-language decoder fixture; generated/ is emitted separately.
        if relative != "manifest.json"
            && relative != "timestamps.json"
            && !relative.starts_with("generated/")
        {
            found.push(relative);
        }
    }
}

fn is_contract_fixture(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|value| value.to_str()),
        Some("json" | "ndjson")
    )
}
