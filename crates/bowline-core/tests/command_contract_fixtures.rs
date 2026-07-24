// Integration-test crate: fixture helpers may panic (clippy only exempts #[test] fns).
#![allow(clippy::panic)]

use std::collections::BTreeSet;

use bowline_core::commands::{
    CommandName, ContractCommandOutput, ContractSummaryCommandOutput, DoctorCommandOutput,
    DryRunCommandOutput, DryRunStatus, HelpCommandOutput, ScopedContractCommandOutput,
    SetupCommandOutput, SetupProjectOutput, VersionCommandOutput, WorkCreateCommandOutput,
    WorkDiffCommandOutput, WorkLifecycleCommandOutput,
};
use bowline_core::work_views::WorkCommandAction;
use serde_json::Value;

#[path = "support/contract_fixtures.rs"]
mod contract_fixtures;

use contract_fixtures::{ManifestFixture, manifest_entries_for_rust};

#[test]
fn all_command_variants_are_exhaustively_enumerated() {
    let unique = CommandName::ALL.iter().copied().collect::<BTreeSet<_>>();
    assert_eq!(unique.len(), CommandName::ALL.len());
    assert_eq!(unique.len(), command_name_fixture_set().len());
}

#[test]
fn rust_command_names_match_shared_fixture() {
    let fixture_names = command_name_fixture_set();
    let rust_names = CommandName::ALL
        .iter()
        .map(serialized_command_name)
        .collect::<BTreeSet<_>>();

    assert_eq!(rust_names, fixture_names);
}

#[test]
fn rust_command_manifest_decoders_match_shared_contract_fixtures() {
    for fixture in manifest_entries_for_rust("commands", None) {
        assert_eq!(fixture.format, "json");
        let expected = contract_fixtures::fixture_json(&fixture.path);
        match fixture.kind.as_str() {
            "ContractCommandOutput" => {
                round_trip_command::<ContractCommandOutput>(&fixture, expected)
            }
            "ContractSummaryCommandOutput" => {
                round_trip_command::<ContractSummaryCommandOutput>(&fixture, expected)
            }
            "CommandNames" => {
                let names: Vec<String> =
                    serde_json::from_value(expected.clone()).expect("fixture parses as names");
                assert_eq!(
                    serde_json::to_value(&names).expect("command names serialize"),
                    expected
                );
            }
            "DoctorCommandOutput" => round_trip_command::<DoctorCommandOutput>(&fixture, expected),
            "DryRunCommandOutput" => round_trip_command::<DryRunCommandOutput>(&fixture, expected),
            "HelpCommandOutput" => round_trip_command::<HelpCommandOutput>(&fixture, expected),
            "SetupProjectOutput" => round_trip_command::<SetupProjectOutput>(&fixture, expected),
            "SetupCommandOutput" => round_trip_command::<SetupCommandOutput>(&fixture, expected),
            "ScopedContractCommandOutput" => {
                round_trip_command::<ScopedContractCommandOutput>(&fixture, expected)
            }
            "VersionCommandOutput" => {
                round_trip_command::<VersionCommandOutput>(&fixture, expected)
            }
            "WorkDiffCommandOutput" => {
                round_trip_command::<WorkDiffCommandOutput>(&fixture, expected)
            }
            "WorkLifecycleCommandOutput" => {
                round_trip_command::<WorkLifecycleCommandOutput>(&fixture, expected)
            }
            "WorkCreateCommandOutput" => {
                round_trip_command::<WorkCreateCommandOutput>(&fixture, expected)
            }
            other => panic!("unsupported Rust command fixture kind {other}"),
        }
    }
}

#[test]
fn rust_setup_json_matches_shared_contract_fixture() {
    let expected = fixture_json("setup-blocked");
    let output: SetupProjectOutput =
        serde_json::from_value(expected.clone()).expect("fixture parses as setup output");
    let actual = serde_json::to_value(&output).expect("setup output serializes");

    assert_eq!(output.command, CommandName::Setup);
    assert_eq!(
        actual, expected,
        "setup-blocked fixture changed on round trip"
    );
}

#[test]
fn rust_work_view_json_matches_shared_contract_fixtures() {
    let work_create_expected = fixture_json("work-create-created");
    let work_create: WorkCreateCommandOutput = serde_json::from_value(work_create_expected.clone())
        .expect("fixture parses as work_create output");
    assert_eq!(work_create.command, CommandName::WorkCreate);
    assert_eq!(
        serde_json::to_value(&work_create).expect("work_create output serializes"),
        work_create_expected
    );
    let reused_expected = fixture_json("work-create-reused");
    let reused: WorkCreateCommandOutput = serde_json::from_value(reused_expected.clone())
        .expect("fixture parses as reused work_create output");
    assert_eq!(reused.action, WorkCommandAction::Reused);
    assert_eq!(
        serde_json::to_value(&reused).expect("reused work_create output serializes"),
        reused_expected
    );

    let diff_expected = fixture_json("work-review");
    let diff: WorkDiffCommandOutput = serde_json::from_value(diff_expected.clone())
        .expect("fixture parses as work review output");
    assert_eq!(diff.command, CommandName::Review);
    assert_eq!(
        serde_json::to_value(&diff).expect("work diff output serializes"),
        diff_expected
    );

    for (fixture, command) in [
        ("work-accept", CommandName::Accept),
        ("work-accept-partial", CommandName::Accept),
        ("work-accept-review-ready", CommandName::Accept),
        ("work-discard", CommandName::Discard),
    ] {
        let expected = fixture_json(fixture);
        let output: WorkLifecycleCommandOutput = serde_json::from_value(expected.clone())
            .expect("fixture parses as work lifecycle output");
        assert_eq!(output.command, command);
        assert_eq!(
            serde_json::to_value(&output).expect("work lifecycle output serializes"),
            expected
        );
    }
}

#[test]
fn rust_discovery_json_matches_shared_contract_fixtures() {
    let help_expected = fixture_json("help");
    let help: HelpCommandOutput =
        serde_json::from_value(help_expected.clone()).expect("fixture parses as help output");
    assert_eq!(help.command, CommandName::Help);
    assert_eq!(
        serde_json::to_value(&help).expect("help output serializes"),
        help_expected
    );

    let version_expected = fixture_json("version");
    let version: VersionCommandOutput =
        serde_json::from_value(version_expected.clone()).expect("fixture parses as version output");
    assert_eq!(version.command, CommandName::Version);
    assert_eq!(
        serde_json::to_value(&version).expect("version output serializes"),
        version_expected
    );

    let contract_expected = fixture_json("contract");
    let contract: ContractCommandOutput = serde_json::from_value(contract_expected.clone())
        .expect("fixture parses as contract output");
    assert_eq!(contract.command, CommandName::Contract);
    assert_eq!(
        serde_json::to_value(&contract).expect("contract output serializes"),
        contract_expected
    );
}

#[test]
fn rust_dry_run_json_matches_shared_contract_fixture() {
    let expected = fixture_json("dry-run");
    let output: DryRunCommandOutput =
        serde_json::from_value(expected.clone()).expect("fixture parses as dry-run output");
    assert_eq!(output.command, CommandName::WorkCreate);
    assert_eq!(output.status, DryRunStatus::DryRun);
    assert_eq!(
        serde_json::to_value(&output).expect("dry-run output serializes"),
        expected
    );
}

fn fixture_json(name: &str) -> Value {
    contract_fixtures::fixture_json(&format!("commands/{name}.json"))
}

fn command_name_fixture_set() -> BTreeSet<String> {
    serde_json::from_value::<Vec<String>>(contract_fixtures::fixture_json("command-names.json"))
        .expect("command names fixture is a string list")
        .into_iter()
        .collect()
}

fn serialized_command_name(name: &CommandName) -> String {
    serde_json::from_value::<String>(serde_json::to_value(name).expect("command name serializes"))
        .expect("command name serializes to string")
}

fn round_trip_command<T>(fixture: &ManifestFixture, expected: Value)
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    let expected_command: CommandName =
        serde_json::from_value(expected["command"].clone()).expect("fixture command is known");
    let output: T = serde_json::from_value(expected.clone())
        .unwrap_or_else(|error| panic!("{} parses as {}: {error}", fixture.id, fixture.kind));
    let actual = serde_json::to_value(&output)
        .unwrap_or_else(|error| panic!("{} serializes as {}: {error}", fixture.id, fixture.kind));

    if fixture.id == "command.setup-machine" {
        assert_eq!(expected_command, CommandName::Setup);
        assert_eq!(actual["root"], expected["root"]);
        assert_eq!(actual["workspaceId"], expected["workspaceId"]);
        assert_eq!(
            actual["nextActions"][0]["label"],
            expected["nextActions"][0]["label"]
        );
        assert_eq!(actual["nextActions"][0]["mutates"], Value::Bool(false));
        return;
    }

    assert_eq!(
        actual, expected,
        "{} fixture changed on round trip",
        fixture.id
    );
    assert_eq!(
        actual["command"],
        serde_json::to_value(expected_command).expect("command serializes")
    );
}
