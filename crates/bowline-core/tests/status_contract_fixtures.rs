use std::{fs, path::PathBuf};

use bowline_core::commands::{CONTRACT_VERSION, CommandName, StatusCommandOutput};
use serde_json::{Value, json};

const STATUS_FIXTURES: [&str; 11] = [
    "healthy",
    "attention",
    "limited",
    "conflict",
    "pending-device",
    "degraded-watcher",
    "active-lease",
    "review-ready-lease",
    "metadata-corrupt-limited",
    "stale-agent-base",
    "work-view-attention",
];
const INDEX_STATUS_FIXTURES: [&str; 2] = ["index-ready", "index-degraded"];

#[test]
fn rust_status_json_matches_shared_contract_fixtures() {
    for fixture_name in STATUS_FIXTURES.into_iter().chain(INDEX_STATUS_FIXTURES) {
        let expected = fixture_json(fixture_name);
        let output: StatusCommandOutput =
            serde_json::from_value(expected.clone()).expect("fixture parses as status output");
        let actual = serde_json::to_value(&output).expect("status output serializes");

        assert_eq!(output.contract_version, CONTRACT_VERSION);
        assert_eq!(output.command, CommandName::Status);
        assert_eq!(
            actual, expected,
            "{fixture_name} fixture changed on round trip"
        );
    }
}

#[test]
fn unknown_status_level_fails_to_deserialize() {
    let mut fixture = fixture_json("healthy");
    fixture["status"]["level"] = json!("blocked");

    let error = serde_json::from_value::<StatusCommandOutput>(fixture).unwrap_err();

    assert!(error.to_string().contains("unknown variant"));
}

#[test]
fn unknown_event_name_fails_to_deserialize() {
    let mut fixture = fixture_json("healthy");
    fixture["items"][0]["eventName"] = json!("status.unknown");

    let error = serde_json::from_value::<StatusCommandOutput>(fixture).unwrap_err();

    assert!(error.to_string().contains("unknown variant"));
}

fn fixture_json(name: &str) -> Value {
    let path = fixtures_dir().join(format!("{name}.json"));
    let json = fs::read_to_string(&path).expect("status fixture is readable");

    serde_json::from_str(&json).expect("status fixture is valid JSON")
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/contracts/status")
}
