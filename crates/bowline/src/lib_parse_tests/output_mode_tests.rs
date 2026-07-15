use super::{Command, ParseError, parse_args};

#[test]
fn broken_pipe_print_panics_are_classified_as_normal_termination() {
    let message = "failed printing to stdout: Broken pipe (os error 32)".to_string();

    assert!(super::super::is_broken_pipe_panic(&message));
    assert!(!super::super::is_broken_pipe_panic(&"unrelated failure"));
}

#[test]
fn resolves_tty_aware_output_defaults_and_overrides() {
    let automatic = parse_args(["status", "--root", "~/Code"]);
    assert_eq!(
        super::super::dispatch::resolve_output_mode(&automatic, false),
        super::super::OutputMode::Json
    );
    assert_eq!(
        super::super::dispatch::resolve_output_mode(&automatic, true),
        super::super::OutputMode::Human
    );

    let human = parse_args(["status", "--root", "~/Code", "--human"]);
    assert_eq!(
        super::super::dispatch::resolve_output_mode(&human, false),
        super::super::OutputMode::Human
    );

    let tui = parse_args(["tui", "--root", "~/Code"]);
    assert_eq!(
        super::super::dispatch::resolve_output_mode(&tui, false),
        super::super::OutputMode::Human
    );
}

#[test]
fn parses_quiet_list_flags_and_rejects_conflicting_modes() {
    let invocation = parse_args(["events", "--root", "~/Code", "--quiet"]);
    assert!(invocation.quiet);
    assert_eq!(
        super::super::dispatch::resolve_output_mode(&invocation, true),
        super::super::OutputMode::Quiet
    );
    assert!(matches!(invocation.command, Ok(Command::Events(_))));

    let short_quiet = parse_args(["events", "--root", "~/Code", "-q"]);
    assert!(matches!(short_quiet.command, Err(ParseError::Usage { .. })));

    let unsupported = parse_args(["status", "--root", "~/Code", "--quiet"]);
    assert!(matches!(unsupported.command, Err(ParseError::Usage { .. })));

    let conflicting = parse_args(["work", "list", "--json", "--human"]);
    assert!(matches!(conflicting.command, Err(ParseError::Usage { .. })));
}

#[test]
fn invalid_quiet_invocations_resolve_errors_without_quiet_mode() {
    let unsupported = parse_args(["status", "--root", "~/Code", "--quiet"]);
    assert!(matches!(unsupported.command, Err(ParseError::Usage { .. })));
    assert_eq!(
        super::super::dispatch::resolve_output_mode(&unsupported, false),
        super::super::OutputMode::Json
    );
    assert_eq!(
        super::super::dispatch::resolve_output_mode(&unsupported, true),
        super::super::OutputMode::Human
    );

    let explicit_json = parse_args(["events", "--quiet", "--json"]);
    assert!(matches!(
        explicit_json.command,
        Err(ParseError::Usage { .. })
    ));
    assert_eq!(
        super::super::dispatch::resolve_output_mode(&explicit_json, true),
        super::super::OutputMode::Json
    );

    let explicit_human = parse_args(["status", "--quiet", "--human"]);
    assert!(matches!(
        explicit_human.command,
        Err(ParseError::Usage { .. })
    ));
    assert_eq!(
        super::super::dispatch::resolve_output_mode(&explicit_human, false),
        super::super::OutputMode::Human
    );
}
