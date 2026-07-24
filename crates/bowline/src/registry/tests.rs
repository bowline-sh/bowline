use super::*;
use std::collections::BTreeSet;

#[test]
fn public_definitions_exactly_cover_generated_command_names() {
    let defined = command_specs()
        .map(|spec| spec.name)
        .collect::<BTreeSet<_>>();
    let generated = CommandName::ALL
        .iter()
        .copied()
        .filter(|command| *command != CommandName::Unknown)
        .map(CommandName::token)
        .collect::<BTreeSet<_>>();

    assert_eq!(defined, generated);
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
        let Some(command_name) = CommandName::from_token(spec.name) else {
            continue;
        };
        let Some(command) = sample_for_command_name(command_name) else {
            continue;
        };
        let runtime_supports_dry_run = crate::idempotency::dry_run_plan(&command).is_some();
        assert_eq!(
            spec.supports_dry_run, runtime_supports_dry_run,
            "dry-run gate disagrees for {}",
            spec.name
        );
    }
}

#[test]
fn every_registered_command_has_a_wire_identity() {
    for spec in command_specs() {
        assert!(
            CommandName::from_token(spec.name).is_some(),
            "registered command `{}` has no CommandName variant",
            spec.name
        );
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
            selector: TrustRequestSelector::Request("req_sample".to_string()),
            yes: true,
        })),
        CommandName::Deny => Some(Command::Deny(ApproveArgs {
            selection: sample_selection(),
            selector: TrustRequestSelector::Request("req_sample".to_string()),
            yes: true,
        })),
        CommandName::Revoke => Some(Command::Revoke(RevokeArgs {
            selection: sample_selection(),
            device_id: "dev_sample".to_string(),
        })),
        CommandName::Recover => Some(Command::Recovery(recovery::RecoveryArgs::Create)),
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
        CommandName::Events => Some(Command::Events(EventsArgs {
            selection: sample_selection(),
            limit: 10,
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
