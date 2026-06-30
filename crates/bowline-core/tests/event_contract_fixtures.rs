use std::{fs, path::PathBuf};

use bowline_core::{
    commands::{CONTRACT_VERSION, WatchFrame},
    events::WorkspaceEvent,
};
use serde_json::Value;

#[test]
fn rust_event_json_matches_shared_contract_fixture() {
    let expected = fixture_json("events/metadata-corrupt");
    let output: WorkspaceEvent =
        serde_json::from_value(expected.clone()).expect("fixture parses as event");
    let actual = serde_json::to_value(&output).expect("event serializes");

    assert_eq!(output.schema_version, CONTRACT_VERSION);
    assert_eq!(actual, expected);
}

#[test]
fn rust_watch_frames_match_shared_ndjson_fixture() {
    let frames = fixture_text("streams/status-watch.ndjson")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("watch frame json"))
        .collect::<Vec<_>>();

    assert!(!frames.is_empty());
    for expected in frames {
        let frame: WatchFrame =
            serde_json::from_value(expected.clone()).expect("fixture parses as watch frame");
        let actual = serde_json::to_value(&frame).expect("watch frame serializes");
        assert_eq!(actual, expected);
    }
}

fn fixture_json(name: &str) -> Value {
    serde_json::from_str(&fixture_text(&format!("{name}.json"))).expect("fixture is valid JSON")
}

fn fixture_text(name: &str) -> String {
    let path = fixtures_dir().join(name);
    fs::read_to_string(&path).expect("fixture is readable")
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/contracts")
}
