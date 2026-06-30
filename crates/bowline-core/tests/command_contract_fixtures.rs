use std::{fs, path::PathBuf};

use bowline_core::commands::{
    AgentContextCommandOutput, AgentLeaseCreateCommandOutput, AgentPromptCommandOutput,
    CommandName, ContractCommandOutput, DryRunCommandOutput, DryRunStatus, ExplainCommandOutput,
    HelpCommandOutput, PrewarmCommandOutput, VersionCommandOutput, WorkDiffCommandOutput,
    WorkLifecycleCommandOutput, WorkonCommandOutput,
};
use serde_json::Value;

#[test]
fn rust_explain_json_matches_shared_contract_fixture() {
    let expected = fixture_json("explain-env");
    let output: ExplainCommandOutput =
        serde_json::from_value(expected.clone()).expect("fixture parses as explain output");
    let actual = serde_json::to_value(&output).expect("explain output serializes");

    assert_eq!(output.command, CommandName::Explain);
    assert_eq!(
        actual, expected,
        "explain-env fixture changed on round trip"
    );
}

#[test]
fn rust_setup_json_matches_shared_contract_fixture() {
    let expected = fixture_json("setup-blocked");
    let output: PrewarmCommandOutput =
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
    let workon_expected = fixture_json("workon-created");
    let workon: WorkonCommandOutput =
        serde_json::from_value(workon_expected.clone()).expect("fixture parses as workon output");
    assert_eq!(workon.command, CommandName::Workon);
    assert_eq!(
        serde_json::to_value(&workon).expect("workon output serializes"),
        workon_expected
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
fn rust_agent_json_matches_shared_contract_fixtures() {
    let lease_expected = fixture_json("agent-lease-create");
    let lease: AgentLeaseCreateCommandOutput = serde_json::from_value(lease_expected.clone())
        .expect("fixture parses as agent lease create output");
    assert_eq!(lease.command, CommandName::AgentStart);
    assert_eq!(
        serde_json::to_value(&lease).expect("agent lease create serializes"),
        lease_expected
    );

    let context_expected = fixture_json("agent-context");
    let context: AgentContextCommandOutput = serde_json::from_value(context_expected.clone())
        .expect("fixture parses as agent context output");
    assert_eq!(context.command, CommandName::AgentContext);
    assert_eq!(
        serde_json::to_value(&context).expect("agent context serializes"),
        context_expected
    );

    let prompt_expected = fixture_json("agent-prompt");
    let prompt: AgentPromptCommandOutput = serde_json::from_value(prompt_expected.clone())
        .expect("fixture parses as agent prompt output");
    assert_eq!(prompt.command, CommandName::AgentPrompt);
    assert_eq!(
        serde_json::to_value(&prompt).expect("agent prompt serializes"),
        prompt_expected
    );
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
    assert_eq!(output.command, CommandName::Workon);
    assert_eq!(output.status, DryRunStatus::DryRun);
    assert_eq!(
        serde_json::to_value(&output).expect("dry-run output serializes"),
        expected
    );
}

fn fixture_json(name: &str) -> Value {
    let path = fixtures_dir().join(format!("{name}.json"));
    let json = fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!("command fixture is readable at {}: {error}", path.display())
    });

    serde_json::from_str(&json).expect("command fixture is valid JSON")
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/contracts/commands")
}
