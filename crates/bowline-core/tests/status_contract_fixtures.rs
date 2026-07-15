// Integration-test crate: fixture helpers may panic (clippy only exempts #[test] fns).
#![allow(clippy::panic)]

use bowline_core::commands::{CONTRACT_VERSION, CommandName, StatusCommandOutput, WatchFrame};
use serde_json::json;

#[path = "support/contract_fixtures.rs"]
mod contract_fixtures;

use contract_fixtures::{fixture_json, manifest_entries_for_rust};

const STATUS_DECODER: &str = "StatusCommandOutput";

#[test]
fn rust_status_json_matches_shared_contract_fixtures() {
    for fixture in manifest_entries_for_rust("status", Some(STATUS_DECODER)) {
        assert_eq!(fixture.format, "json");
        assert_eq!(fixture.kind, STATUS_DECODER);

        let expected = fixture_json(&fixture.path);
        let output: StatusCommandOutput =
            serde_json::from_value(expected.clone()).expect("fixture parses as status output");
        let actual = serde_json::to_value(&output).expect("status output serializes");

        assert_eq!(output.contract_version, CONTRACT_VERSION);
        assert_eq!(output.command, CommandName::Status);
        assert_eq!(
            actual, expected,
            "{} fixture changed on round trip",
            fixture.id
        );
    }
}

#[test]
fn rust_status_watch_json_matches_shared_contract_fixture() {
    let fixtures = manifest_entries_for_rust("streams", Some("WatchFrame"));
    assert_eq!(fixtures.len(), 1, "status watch has one canonical stream");
    let fixture = &fixtures[0];
    assert_eq!(fixture.format, "ndjson");
    assert_eq!(fixture.kind, "WatchFrame");

    let text = contract_fixtures::fixture_text(&fixture.path);
    let values = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("frame is valid json"))
        .collect::<Vec<_>>();
    let frames = values
        .iter()
        .cloned()
        .map(|value| serde_json::from_value::<WatchFrame>(value).expect("frame parses"))
        .collect::<Vec<_>>();
    let actual = frames
        .iter()
        .map(|frame| serde_json::to_value(frame).expect("frame serializes"))
        .collect::<Vec<_>>();

    assert_eq!(actual, values, "watch fixture changed on round trip");
    assert!(matches!(
        frames.as_slice(),
        [
            WatchFrame::Status { .. },
            WatchFrame::Event { .. },
            WatchFrame::Error { .. }
        ]
    ));
}

#[test]
fn unknown_status_level_fails_to_deserialize() {
    let mut fixture = fixture_json("status/healthy.json");
    fixture["status"]["level"] = json!("blocked");

    let error = serde_json::from_value::<StatusCommandOutput>(fixture).unwrap_err();

    assert!(error.to_string().contains("unknown variant"));
}

#[test]
fn unknown_event_name_round_trips_without_losing_the_raw_value() {
    let mut fixture = fixture_json("status/healthy.json");
    fixture["items"][0]["eventName"] = json!("status.unknown");

    let output: StatusCommandOutput =
        serde_json::from_value(fixture).expect("unknown event name is preserved");
    let serialized = serde_json::to_value(output).expect("status output serializes");

    assert_eq!(serialized["items"][0]["eventName"], "status.unknown");
}

#[test]
fn device_approval_without_device_name_fails_to_deserialize() {
    let mut fixture = fixture_json("status/pending-device.json");
    fixture["deviceApprovals"][0]
        .as_object_mut()
        .expect("approval fixture is an object")
        .remove("deviceName");

    let error = serde_json::from_value::<StatusCommandOutput>(fixture).unwrap_err();

    assert!(error.to_string().contains("deviceName"));
}
