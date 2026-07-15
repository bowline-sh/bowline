use super::{Command, HandoffArgs, ParseError, parse_args};
use bowline_core::commands::{CommandName, HandoffAgent};

#[test]
fn parses_handoff_default_target() {
    let cli = parse_args(["handoff", "linux-home"]);

    assert_eq!(
        cli.command.expect("parsed command"),
        Command::Handoff(HandoffArgs {
            target: "linux-home".to_string(),
            agent: None,
            session: None,
            prompt: None,
            prompt_file: None,
            project: None,
        })
    );
}

#[test]
fn parses_handoff_agent_session_prompt_file_and_project() {
    let cli = parse_args([
        "handoff",
        "linux-home",
        "--agent",
        "codex",
        "--session",
        "sess_123",
        "--project",
        "~/Code/bowline",
    ]);

    assert_eq!(
        cli.command.expect("parsed command"),
        Command::Handoff(HandoffArgs {
            target: "linux-home".to_string(),
            agent: Some(HandoffAgent::Codex),
            session: Some("sess_123".to_string()),
            prompt: None,
            prompt_file: None,
            project: Some("~/Code/bowline".to_string()),
        })
    );

    let prompt_file = parse_args([
        "handoff",
        "linux-home",
        "--agent=claude",
        "--prompt-file",
        "docs/prompts/x.md",
    ]);
    assert_eq!(
        prompt_file.command.expect("parsed command"),
        Command::Handoff(HandoffArgs {
            target: "linux-home".to_string(),
            agent: Some(HandoffAgent::Claude),
            session: None,
            prompt: None,
            prompt_file: Some("docs/prompts/x.md".to_string()),
            project: None,
        })
    );
}

#[test]
fn rejects_handoff_conflicting_prompt_flags_and_unknown_agent() {
    let conflicting = parse_args([
        "handoff",
        "linux-home",
        "--prompt",
        "x",
        "--prompt-file",
        "p",
    ]);
    assert!(
        matches!(conflicting.command, Err(ParseError::Command(error))
        if error.command == CommandName::Handoff
            && error.message.contains("cannot combine --prompt and --prompt-file"))
    );

    let unsupported = parse_args(["handoff", "linux-home", "--agent", "cursor"]);
    assert!(
        matches!(unsupported.command, Err(ParseError::Command(error))
        if error.command == CommandName::Handoff
            && error.code == "unsupported_agent")
    );
}

#[test]
fn parses_hidden_handoff_install_bundle() {
    let cli = parse_args(["internal", "handoff", "install-bundle", "--json"]);

    assert!(cli.json);
    assert_eq!(
        cli.command.expect("parsed command"),
        Command::HandoffInstallBundle
    );
}
