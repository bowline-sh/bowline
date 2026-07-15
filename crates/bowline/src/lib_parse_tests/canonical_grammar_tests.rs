use super::{ParseError, parse_args};

#[test]
fn noncanonical_command_paths_are_rejected() {
    for args in [
        &["--version"][..],
        &["--help"][..],
        &["-h"][..],
        &["diff"][..],
        &["review", "work-view"][..],
        &["accept", "work-view"][..],
        &["discard", "work-view"][..],
        &["restore", "work-view"][..],
        &["cleanup"][..],
        &["approve", "--request", "request-1"][..],
        &["deny", "--request", "request-1"][..],
        &["revoke", "--device", "device-1"][..],
        &["devices"][..],
        &["devices", "request"][..],
        &["work"][..],
        &["recover"][..],
        &["handoff", "install-bundle"][..],
        &["--json", "status"][..],
    ] {
        let invocation = parse_args(args.iter().copied());
        assert!(
            invocation.command.is_err(),
            "noncanonical command path unexpectedly resolved: {args:?}"
        );
    }
}

#[test]
fn noncanonical_option_spellings_are_rejected() {
    for args in [
        &["events", "-q"][..],
        &["archive", "apps/web", "--unarchive"][..],
    ] {
        let invocation = parse_args(args.iter().copied());
        assert!(
            matches!(invocation.command, Err(ParseError::Usage { .. })),
            "noncanonical option unexpectedly resolved: {args:?}"
        );
    }
}
