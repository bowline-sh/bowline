use super::*;
use std::collections::BTreeSet;

use bowline_core::ids::{DeviceApprovalRequestId, DeviceId, RecoveryEnvelopeId};

#[test]
fn public_definitions_exactly_cover_generated_command_names() {
    let defined = command_specs()
        .map(|spec| spec.command_name())
        .collect::<BTreeSet<_>>();
    let generated = CommandName::ALL
        .iter()
        .copied()
        .filter(|command| *command != CommandName::Unknown)
        .collect::<BTreeSet<_>>();

    assert_eq!(defined, generated);
}

#[test]
fn recovery_actions_are_registered_subcommands() {
    let recovery = command_specs()
        .filter(|spec| matches!(spec.target(), DefinitionTarget::Recovery(_)))
        .map(|spec| spec.name)
        .collect::<BTreeSet<_>>();

    assert_eq!(
        recovery,
        BTreeSet::from([
            "recover create",
            "recover revoke",
            "recover rotate",
            "recover status",
            "recover use",
            "recover verify",
        ])
    );
    assert!(command_specs().all(|spec| spec.name != "recover"));
}

#[test]
fn topic_help_resolves_family_prefixes_and_subcommands() {
    for (topic, expected) in [
        ("recover", "recover status"),
        ("device", "device approve"),
        ("work", "work create"),
    ] {
        let descriptors = command_descriptors_for_topic(Some(topic));
        assert!(
            descriptors
                .iter()
                .any(|descriptor| descriptor.name == expected),
            "help topic `{topic}` did not list `{expected}`"
        );
    }
    assert_eq!(
        command_descriptors_for_topic(Some("recover use"))
            .iter()
            .map(|descriptor| descriptor.name.clone())
            .collect::<Vec<_>>(),
        vec!["recover use".to_string()]
    );
}

#[test]
fn bare_family_token_reports_its_subcommands() {
    for family in ["work", "device", "recover", "daemon"] {
        let error = crate::cli::parse_args([family])
            .command
            .expect_err("a family token is not a command");
        let ParseError::Command(error) = error else {
            panic!("{family} should report a missing subcommand, got {error:?}");
        };
        assert_eq!(error.code, "missing_subcommand");
        assert!(!error.next_actions.is_empty());
    }
}

#[test]
fn absent_required_option_names_the_option_not_a_missing_value() {
    let error = crate::cli::parse_args(["device", "revoke"])
        .command
        .expect_err("device revoke requires --device");
    let ParseError::Command(error) = error else {
        panic!("expected a structured usage error, got {error:?}");
    };
    assert_eq!(error.code, "missing_required_option");
    assert_eq!(
        error.message,
        "bowline device revoke requires --device <id>"
    );
    assert!(!error.next_actions.is_empty());
}

#[test]
fn empty_required_option_still_reports_a_missing_value() {
    let error = crate::cli::parse_args(["device", "revoke", "--device"])
        .command
        .expect_err("--device at end of argv has no value");
    assert!(
        matches!(&error, ParseError::Usage { message, .. } if message.contains("requires a value")),
        "expected a missing-value error, got {error:?}"
    );
}

#[test]
fn diagnostics_collect_infers_the_workspace_root() {
    // Asserted against the spec, not against this machine: parsing bare
    // `diagnostics collect` succeeds only where a root can actually be inferred,
    // so the old form of this test passed or failed on whether the developer
    // happened to have an accepted root — and broke the moment ~/Code was
    // removed. The real invariant is that the command does not demand --root.
    let spec = command_specs()
        .find(|spec| spec.command_name() == CommandName::DiagnosticsCollect)
        .expect("diagnostics collect is registered");
    let root = spec
        .options
        .iter()
        .find(|option| option.name == "--root")
        .expect("diagnostics collect accepts --root");
    assert!(
        !root.required,
        "diagnostics collect must infer the root like its siblings"
    );

    let invocation = crate::cli::parse_args(["diagnostics", "collect", "--root", "~/Code"]);
    assert!(matches!(
        invocation.command,
        Ok(Command::DiagnosticsCollect(_))
    ));
}

#[test]
fn definitions_have_valid_argument_graphs_and_typed_samples() {
    for spec in all_command_specs() {
        let option_names = spec
            .options
            .iter()
            .map(|option| option.name)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            option_names.len(),
            spec.options.len(),
            "{} repeats an option definition",
            spec.name
        );
        let positional_names = spec
            .positionals
            .iter()
            .map(|positional| positional.name)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            positional_names.len(),
            spec.positionals.len(),
            "{} repeats a positional definition",
            spec.name
        );
        assert!(
            spec.positionals
                .iter()
                .enumerate()
                .all(|(index, positional)| !positional.repeatable
                    || index + 1 == spec.positionals.len()),
            "{} has a non-terminal repeatable positional",
            spec.name
        );
        if let DefinitionTarget::Public(command_name) = spec.target() {
            let command = sample_for_command_name(command_name).expect("typed command sample");
            assert_eq!(command.name(), command_name, "{} sample drifted", spec.name);
        }
    }
}

#[test]
fn dry_run_gate_matches_registry() {
    for spec in command_specs() {
        let Some(command) = sample_for_spec(spec) else {
            continue;
        };
        let runtime_supports_dry_run = crate::idempotency::dry_run_plan(&command).is_some();
        assert_eq!(
            spec.supports_dry_run(),
            runtime_supports_dry_run,
            "dry-run gate disagrees for {}",
            spec.name
        );
    }
}

#[test]
fn every_registered_command_has_a_wire_identity() {
    for spec in command_specs() {
        assert_ne!(
            spec.command_name(),
            CommandName::Unknown,
            "registered command `{}` has no CommandName variant",
            spec.name
        );
    }
}

fn sample_for_spec(spec: &CommandSpec) -> Option<Command> {
    match spec.target() {
        DefinitionTarget::Public(command_name) => sample_for_command_name(command_name),
        DefinitionTarget::Recovery(action) => Some(Command::Recovery(match action {
            RecoveryCommandAction::Status => recovery::RecoveryArgs::Status,
            RecoveryCommandAction::Create => recovery::RecoveryArgs::Create,
            RecoveryCommandAction::Rotate => recovery::RecoveryArgs::Rotate,
            RecoveryCommandAction::Verify => recovery::RecoveryArgs::Verify {
                envelope_id: RecoveryEnvelopeId::new("env_sample".to_string()),
            },
            RecoveryCommandAction::Revoke => recovery::RecoveryArgs::Revoke {
                envelope_id: RecoveryEnvelopeId::new("env_sample".to_string()),
            },
            RecoveryCommandAction::Use => recovery::RecoveryArgs::Use {
                envelope_id: RecoveryEnvelopeId::new("env_sample".to_string()),
            },
        })),
        DefinitionTarget::DebugClassify | DefinitionTarget::SyncWait => None,
    }
}

fn sample_for_command_name(command: CommandName) -> Option<Command> {
    match command {
        CommandName::Help => Some(Command::Help(None)),
        CommandName::Version => Some(Command::Version),
        CommandName::Contract => Some(Command::Contract(ContractMode::Full)),
        CommandName::Update => Some(Command::Update(UpdateArgs {
            check: true,
            version: None,
        })),
        CommandName::Unknown => None,
        CommandName::Login => Some(Command::Login(login::LoginArgs {
            no_poll: true,
            headless: false,
        })),
        CommandName::Logout => Some(Command::Logout),
        CommandName::Approve => Some(Command::Approve(ApproveArgs {
            selection: sample_selection(),
            selector: TrustRequestSelector::Request(DeviceApprovalRequestId::new(
                "req_sample".to_string(),
            )),
            yes: true,
        })),
        CommandName::Deny => Some(Command::Deny(ApproveArgs {
            selection: sample_selection(),
            selector: TrustRequestSelector::Request(DeviceApprovalRequestId::new(
                "req_sample".to_string(),
            )),
            yes: true,
        })),
        CommandName::Revoke => Some(Command::Revoke(RevokeArgs {
            selection: sample_selection(),
            device_id: DeviceId::new("dev_sample".to_string()),
        })),
        CommandName::Recover => Some(Command::Recovery(recovery::RecoveryArgs::Create)),
        CommandName::Deletions => Some(Command::Deletions(
            crate::deletion_commands::DeletionsArgs { confirm: true },
        )),
        CommandName::Setup => Some(Command::Setup(SetupArgs {
            mode: SetupMode::Machine { root: None },
        })),
        CommandName::Status => Some(Command::Status(StatusArgs {
            selection: sample_selection(),
            watch: false,
            include_all: false,
        })),
        CommandName::Devices => Some(Command::Devices(devices::DevicesArgs::List {
            selection: sample_selection(),
        })),
        CommandName::DeviceRequest => Some(Command::Devices(devices::DevicesArgs::Request {
            selection: sample_selection(),
        })),
        CommandName::DeviceAccept => Some(Command::Devices(devices::DevicesArgs::Accept {
            selection: sample_selection(),
            request_id: "req_sample".to_string(),
        })),
        CommandName::DeviceKeyStatus => {
            Some(Command::DeviceKeyStatus(devices::DeviceKeyStatusArgs {
                workspace_id: WorkspaceId::new("ws_sample"),
            }))
        }
        CommandName::Events => Some(Command::Events(EventsArgs {
            selection: sample_selection(),
            limit: 10,
        })),
        CommandName::Conflicts => Some(Command::Conflicts(
            crate::conflict_commands::ConflictsArgs {
                selection: sample_selection(),
            },
        )),
        CommandName::Resolve => Some(Command::Resolve(crate::conflict_commands::ResolveArgs {
            root: "~/Code".to_string(),
            aside_path: "apps/web/src/auth.ts.bowline-conflict.9f3a2c1d".to_string(),
            action: bowline_core::commands::ConflictAction::Diff,
        })),
        CommandName::Tui => Some(Command::Tui(TuiArgs {
            selection: sample_selection(),
        })),
        CommandName::WorkCreate => Some(Command::WorkCreate(work::WorkCreateArgs {
            project_path: "apps/web".to_string(),
            name: "sample".to_string(),
            from: None,
        })),
        CommandName::Review => Some(Command::Review(work_selector())),
        CommandName::Work => Some(Command::Work(work::WorkListArgs {
            include_hidden: false,
        })),
        CommandName::Diff => Some(Command::WorkDiff(work_selector())),
        CommandName::Accept => Some(Command::WorkAccept(work_selector())),
        CommandName::Discard => Some(Command::WorkDiscard(work_selector())),
        CommandName::Restore => Some(Command::WorkRestore(work_selector())),
        CommandName::Cleanup => Some(Command::WorkCleanup(work::WorkCleanupArgs { apply: true })),
        CommandName::ForgetLocal => Some(Command::ForgetLocal(ForgetLocalArgs {
            project_path: "apps/web".to_string(),
            yes: true,
        })),
        CommandName::Archive => Some(Command::Archive(ArchiveArgs {
            project_path: "apps/web".to_string(),
            restore: false,
        })),
        CommandName::Purge => Some(Command::Purge(PurgeArgs {
            project_path: "apps/web".to_string(),
            cancel: false,
            grace_days: Some(14),
        })),
        CommandName::DaemonStart => Some(Command::Daemon(DaemonCommand::Start)),
        CommandName::DaemonStop => Some(Command::Daemon(DaemonCommand::Stop)),
        CommandName::DaemonStatus => Some(Command::Daemon(DaemonCommand::Status)),
        CommandName::DaemonInstall => Some(Command::Daemon(DaemonCommand::Install)),
        CommandName::DaemonRestart => Some(Command::Daemon(DaemonCommand::Restart)),
        CommandName::DaemonUninstall => Some(Command::Daemon(DaemonCommand::Uninstall)),
        CommandName::DiagnosticsCollect => Some(Command::DiagnosticsCollect(sample_selection())),
        CommandName::Doctor => Some(Command::Doctor(DoctorArgs {
            engine: bowline_core::commands::DoctorEngine::Manifest,
        })),
        CommandName::Connect => Some(Command::BootstrapSsh(bootstrap::BootstrapSshArgs {
            host: "linux-home".to_string(),
            root: "~/Code".to_string(),
            artifact: None,
        })),
    }
}

fn sample_selection() -> WorkspaceSelection {
    WorkspaceSelection {
        root: "~/Code".to_string(),
        project: None,
    }
}

fn work_selector() -> work::WorkSelectorArgs {
    work::WorkSelectorArgs {
        selector: "sample".to_string(),
        paths: Vec::new(),
    }
}
