use super::{
    Command, ParseError, SetupArgs, SetupMode, StatusArgs, TuiArgs, UpdateArgs, WorkspaceSelection,
    bootstrap::BootstrapSshArgs, devices::DevicesArgs, login, parse_args, recovery::RecoveryArgs,
    work::WorkSelectorArgs,
};

use bowline_core::commands::CommandName;

#[test]
fn parses_command_scoped_json_after_the_command_path() {
    let cli = parse_args(["status", "--root", "~/Code", "--json"]);

    assert!(cli.json);
    assert_eq!(
        cli.command.expect("parsed command"),
        Command::Status(StatusArgs {
            selection: WorkspaceSelection {
                root: "~/Code".to_string(),
                project: None,
            },
            watch: false,
            include_all: false,
        })
    );
}

#[test]
fn json_login_does_not_poll_before_printing_verification_url() {
    let args = super::login_args_for_output(
        login::LoginArgs {
            no_poll: false,
            headless: false,
        },
        true,
    );

    assert!(args.no_poll);
    assert!(!args.headless);
}

#[test]
fn parses_setup_as_machine_onboarding_without_path() {
    let cli = parse_args(["setup"]);

    assert_eq!(
        cli.command.expect("parsed command"),
        Command::Setup(SetupArgs {
            mode: SetupMode::Machine { root: None },
        })
    );
}

#[test]
fn parses_setup_root_as_machine_onboarding() {
    let cli = parse_args(["setup", "--root", "~/Code"]);

    assert_eq!(
        cli.command.expect("parsed command"),
        Command::Setup(SetupArgs {
            mode: SetupMode::Machine {
                root: Some("~/Code".to_string()),
            },
        })
    );
}

#[test]
fn parses_setup_path_as_project_setup() {
    let cli = parse_args(["setup", ".", "--yes"]);

    assert_eq!(
        cli.command.expect("parsed command"),
        Command::Setup(SetupArgs {
            mode: SetupMode::Project {
                project_path: ".".to_string(),
                yes: true,
            },
        })
    );
}

#[test]
fn setup_path_rejects_root_selection() {
    let cli = parse_args(["setup", ".", "--root", "~/Code"]);

    assert!(matches!(cli.command, Err(ParseError::Command(error))
        if error.command == CommandName::Setup
            && error.code == "usage_error"
            && error.message == "bowline setup <path> cannot be combined with --root <path>"));
}

#[test]
fn bare_login_parses_without_root() {
    let cli = parse_args(["login"]);

    assert_eq!(
        cli.command.expect("parsed command"),
        Command::Login(login::LoginArgs {
            no_poll: false,
            headless: false,
        })
    );
}

#[test]
fn login_root_points_to_setup() {
    let cli = parse_args(["login", "--root", "~/Code"]);

    assert!(matches!(cli.command, Err(ParseError::Usage {
        command: CommandName::Login,
        message,
    }) if message == "unknown bowline login option `--root`"));
}

#[test]
fn parses_logout() {
    let cli = parse_args(["logout", "--json"]);

    assert!(cli.json);
    assert_eq!(cli.command.expect("parsed command"), Command::Logout);
}

#[test]
fn parses_update_check_json() {
    let cli = parse_args(["update", "--check", "--json"]);

    assert!(cli.json);
    assert_eq!(
        cli.command.expect("parsed command"),
        Command::Update(UpdateArgs {
            check: true,
            version: None,
        })
    );
}

#[test]
fn parses_update_version() {
    let cli = parse_args(["update", "--version", "0.1.1"]);

    assert_eq!(
        cli.command.expect("parsed command"),
        Command::Update(UpdateArgs {
            check: false,
            version: Some("0.1.1".to_string()),
        })
    );
}

#[test]
fn update_version_requires_value() {
    let cli = parse_args(["update", "--version"]);

    assert_eq!(
        cli.command,
        Err(ParseError::Usage {
            command: CommandName::Update,
            message: "bowline update --version requires a value".to_string(),
        })
    );
}

#[test]
fn parses_status_watch_workspace() {
    let cli = parse_args(["status", "--root", "~/Code", "--watch", "--all"]);

    assert_eq!(
        cli.command.expect("parsed command"),
        Command::Status(StatusArgs {
            selection: WorkspaceSelection {
                root: "~/Code".to_string(),
                project: None,
            },
            watch: true,
            include_all: true,
        })
    );
}

#[test]
fn rejects_bootstrap_ssh_alias() {
    let cli = parse_args(["bootstrap", "ssh", "linux-server-1", "--root", "/tmp/code"]);

    assert!(cli.command.is_err());
}

#[test]
fn parses_connect_agent_handoff() {
    let cli = parse_args(["connect", "linux-server-1", "--root", "~/Code"]);

    assert_eq!(
        cli.command.expect("parsed command"),
        Command::BootstrapSsh(BootstrapSshArgs {
            host: "linux-server-1".to_string(),
            root: "~/Code".to_string(),
            artifact: None,
        })
    );
}

#[test]
fn parses_connect_explicit_root() {
    let cli = parse_args(["connect", "linux-server-1", "--root", "/tmp/code"]);

    assert_eq!(
        cli.command.expect("parsed command"),
        Command::BootstrapSsh(BootstrapSshArgs {
            host: "linux-server-1".to_string(),
            root: "/tmp/code".to_string(),
            artifact: None,
        })
    );
}

#[test]
fn diff_without_selector_defaults_to_cwd() {
    let cli = parse_args(["work", "diff"]);

    assert!(matches!(
        cli.command.expect("parsed command"),
        Command::WorkDiff(_)
    ));
}

#[test]
fn parses_repeatable_work_view_path_selectors() {
    let cli = parse_args([
        "work",
        "accept",
        "agent-output",
        "--path",
        "src/a.ts",
        "--path=src/b.ts",
    ]);

    assert_eq!(
        cli.command.expect("parsed command"),
        Command::WorkAccept(WorkSelectorArgs {
            selector: "agent-output".to_string(),
            paths: vec!["src/a.ts".to_string(), "src/b.ts".to_string()],
        })
    );
}

#[test]
fn review_requires_selector_even_with_path_filter() {
    let cli = parse_args(["work", "review", "--path", "src/a.ts"]);

    assert!(matches!(cli.command, Err(ParseError::Usage { .. })));
}

#[test]
fn parses_device_request_default_and_explicit_root() {
    let default_cli = parse_args(["device", "request"]);
    assert!(matches!(
        default_cli.command,
        Ok(Command::Devices(DevicesArgs::Request { .. })) | Err(ParseError::Command(_))
    ));

    let explicit_cli = parse_args(["device", "request", "--root", "/tmp/code"]);
    assert_eq!(
        explicit_cli.command.expect("parsed command"),
        Command::Devices(DevicesArgs::Request {
            selection: WorkspaceSelection {
                root: "/tmp/code".to_string(),
                project: None,
            },
        })
    );
}

#[test]
fn parses_tui_entrypoint() {
    let tui = parse_args(["tui", "--root", "~/Code", "--project", "app"]);
    assert_eq!(
        tui.command.expect("parsed command"),
        Command::Tui(TuiArgs {
            selection: WorkspaceSelection {
                root: "~/Code".to_string(),
                project: Some("app".to_string()),
            },
        })
    );
}

#[test]
fn splits_tui_action_commands_with_shell_quoted_paths() {
    assert_eq!(
        super::split_tui_command_line("bowline resolve '~/Code/my app' --accept conflict-1"),
        Ok(vec![
            "bowline".to_string(),
            "resolve".to_string(),
            "~/Code/my app".to_string(),
            "--accept".to_string(),
            "conflict-1".to_string(),
        ])
    );
    assert_eq!(
        super::split_tui_command_line("bowline status --root ~/Code --project 'repo'\\''s path'"),
        Ok(vec![
            "bowline".to_string(),
            "status".to_string(),
            "--root".to_string(),
            "~/Code".to_string(),
            "--project".to_string(),
            "repo's path".to_string(),
        ])
    );
    assert_eq!(
        super::split_tui_command_line("bowline connect devbox --root ~/O\\'Connor\\ Code"),
        Ok(vec![
            "bowline".to_string(),
            "connect".to_string(),
            "devbox".to_string(),
            "--root".to_string(),
            "~/O'Connor Code".to_string(),
        ])
    );
    assert_eq!(
        super::split_tui_command_line("bowline status --root ~/Code --project 'unterminated"),
        Err("unterminated quote in TUI action command")
    );
}

#[test]
fn confirmed_tui_child_args_preserve_socket_override() {
    let args = super::confirmed_tui_child_args(
        "bowline resolve '~/Code/my app' --accept conflict-1",
        std::path::Path::new("/tmp/bowline-review.sock"),
    )
    .expect("command should parse");

    assert_eq!(
        args,
        vec![
            std::ffi::OsString::from("--socket"),
            std::ffi::OsString::from("/tmp/bowline-review.sock"),
            std::ffi::OsString::from("resolve"),
            std::ffi::OsString::from("~/Code/my app"),
            std::ffi::OsString::from("--accept"),
            std::ffi::OsString::from("conflict-1"),
        ]
    );
}

#[test]
fn parses_recovery_words_from_stdin_shape_only() {
    let cli = parse_args(["recover", "verify", "rk_123"]);

    assert_eq!(
        cli.command.expect("parsed command"),
        Command::Recovery(RecoveryArgs::Verify {
            envelope_id: "rk_123".to_string(),
        })
    );
}

#[test]
fn rejects_recovery_words_in_argv() {
    let cli = parse_args(["recover", "verify", "rk_123", "secret", "words"]);

    assert!(matches!(cli.command, Err(ParseError::Command(_))));
}

#[test]
fn shell_word_preserves_home_expansion_for_paths_with_spaces() {
    assert_eq!(crate::io_helpers::shell_word("~/Code"), "~/Code");
    assert_eq!(
        crate::io_helpers::shell_word("~/Code Projects"),
        "~/'Code Projects'"
    );
    assert_eq!(
        crate::io_helpers::shell_word("~/O'Connor Code"),
        "~/'O'\\''Connor Code'"
    );
    assert_eq!(
        super::split_tui_command_line("bowline status --root ~/'Code Projects'").unwrap(),
        vec!["bowline", "status", "--root", "~/Code Projects"]
    );
}

mod canonical_grammar_tests;
mod contract_tests;
mod lib_parse_output_tests;
mod output_mode_tests;
