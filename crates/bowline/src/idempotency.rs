//! `--dry-run` previews. The apply command is the caller's own argv with
//! `--dry-run` removed, so it can never describe a different command than the
//! one that was previewed; the risk is the registry's declared side-effect
//! level, so there is no second taxonomy to drift.

use super::*;

use bowline_core::commands::ConflictAction;

use crate::registry::SideEffectLevel;

/// What the mutation would touch and change. Everything else in the preview is
/// derived: the risk from the registry, the apply line from argv.
pub(super) struct DryRunPlan {
    command: CommandName,
    target: String,
    would_change: Vec<String>,
}

pub(super) fn print_dry_run(cli: Cli) -> ExitCode {
    let (Some(plan), Some(level)) = (dry_run_plan(&cli.command), cli.side_effect_level) else {
        print_usage_error(
            cli.command.name(),
            "dry_run_unsupported",
            "--dry-run is not supported for this command",
            cli.json,
        );
        return ExitCode::from(EXIT_USAGE);
    };
    let apply_command = apply_command(&cli);
    let risk = resolved_risk(&cli.command, level);
    // A preview whose risk resolved to None changes nothing, so the line that
    // repeats it must not claim to mutate.
    let apply_action = if matches!(risk, SideEffectLevel::None) {
        RepairCommand::inspect(
            "Run the command without --dry-run".to_string(),
            Some(apply_command.clone()),
        )
    } else {
        RepairCommand::mutating(
            "Run the command without --dry-run".to_string(),
            Some(apply_command.clone()),
        )
    };
    let mut next_actions = vec![apply_action];
    // Without this, previewing a flag-gated command hands back another preview
    // and the user has no way to reach the mutation from the output.
    if let Some(gated) = gated_mutation_command(&cli.command, &apply_command) {
        next_actions.push(RepairCommand::mutating(
            "Apply the cleanup it previewed".to_string(),
            Some(gated),
        ));
    }
    let output = DryRunCommandOutput {
        contract_version: CONTRACT_VERSION,
        command: plan.command,
        generated_at: generated_at(),
        status: DryRunStatus::DryRun,
        allowed: true,
        risk,
        target: plan.target,
        would_change: plan.would_change,
        warnings: Vec::new(),
        apply_command,
        next_actions,
    };
    if cli.json {
        print_json(&output);
    } else {
        println!(
            "bowline dry-run: {}\nTarget: {}\nRisk: {}\nWould change:\n  {}",
            command_name_token(output.command),
            output.target,
            output.risk,
            output.would_change.join("\n  ")
        );
        println!("\nApply:\n  {}", output.apply_command);
    }
    ExitCode::SUCCESS
}

/// `conditional-mutation` is the one level that is not a property of the command
/// path alone; every other command's risk is exactly what its spec declares.
fn resolved_risk(command: &Command, level: SideEffectLevel) -> SideEffectLevel {
    match (level, command) {
        (SideEffectLevel::ConditionalMutation, Command::WorkCleanup(args)) if !args.apply => {
            SideEffectLevel::None
        }
        (SideEffectLevel::ConditionalMutation, Command::WorkCleanup(_)) => {
            SideEffectLevel::WorkspaceMetadata
        }
        (SideEffectLevel::ConditionalMutation, Command::Resolve(args))
            if !args.action.changes_files() =>
        {
            SideEffectLevel::None
        }
        (SideEffectLevel::ConditionalMutation, Command::Resolve(_)) => {
            SideEffectLevel::FilesystemWrite
        }
        (SideEffectLevel::ConditionalMutation, Command::Deletions(args)) if !args.confirm => {
            SideEffectLevel::None
        }
        // A confirmed batch is published to every trusted device, so the files it
        // removes are removed there too. Nothing about this is local.
        (SideEffectLevel::ConditionalMutation, Command::Deletions(_)) => {
            SideEffectLevel::FilesystemWrite
        }
        (level, _) => level,
    }
}

/// The real mutation behind a command whose preview is inert because a gating
/// flag is absent. Built from the literal apply line so it stays exactly what
/// the caller typed, plus the flag that arms it.
fn gated_mutation_command(command: &Command, apply_command: &str) -> Option<String> {
    match command {
        Command::WorkCleanup(args) if !args.apply => Some(format!("{apply_command} --apply")),
        _ => None,
    }
}

/// The caller's own argv, minus `--dry-run`. Exact by construction: nothing is
/// re-serialized from parsed arguments, so the apply line cannot disagree with
/// the command that was previewed.
fn apply_command(cli: &Cli) -> String {
    let args = std::iter::once("bowline".to_string())
        .chain(
            cli.argv
                .iter()
                .filter(|argument| argument.as_str() != "--dry-run")
                .cloned(),
        )
        .collect::<Vec<_>>();
    bowline_core::shell::quote_command(args)
}

pub(super) fn dry_run_plan(command: &Command) -> Option<DryRunPlan> {
    let plan = |command: CommandName, target: String, would_change: &[&str]| DryRunPlan {
        command,
        target,
        would_change: would_change
            .iter()
            .map(|line| (*line).to_string())
            .collect(),
    };
    match command {
        Command::Approve(args) => Some(plan(
            CommandName::Approve,
            trust_selector_label(&args.selector),
            &["approve a pending device trust request"],
        )),
        Command::Deny(args) => Some(plan(
            CommandName::Deny,
            trust_selector_label(&args.selector),
            &["deny a pending device trust request"],
        )),
        Command::Revoke(args) => Some(plan(
            CommandName::Revoke,
            args.device_id.as_str().to_string(),
            &["revoke device trust"],
        )),
        Command::Recovery(args) => recovery_plan(args),
        Command::BootstrapSsh(args) => Some(plan(
            CommandName::Connect,
            args.host.clone(),
            &[
                "install or update remote bowline binaries",
                "establish device trust so the remote host materializes the workspace",
            ],
        )),
        Command::Resolve(args) => Some(plan(
            CommandName::Resolve,
            args.aside_path.clone(),
            match args.action {
                ConflictAction::Diff => {
                    &["nothing; --diff prints both versions and leaves them where they are"]
                }
                ConflictAction::KeepLocal => {
                    &["delete the incoming version preserved beside the file"]
                }
                ConflictAction::TakeRemote => {
                    &["replace the file with the incoming version preserved beside it"]
                }
            },
        )),
        Command::Deletions(args) => Some(plan(
            CommandName::Deletions,
            "the refused deletion".to_string(),
            if args.confirm {
                &["authorise one push that deletes the refused files on every trusted device"]
            } else {
                &["nothing; without --confirm this only reports what is refused"]
            },
        )),
        Command::WorkCreate(args) => Some(plan(
            CommandName::WorkCreate,
            format!("{}:{}", args.project_path, args.name),
            &["create or reuse a work view"],
        )),
        Command::WorkAccept(args) => Some(plan(
            CommandName::Accept,
            args.selector.clone(),
            &["apply work-view changes to the target project"],
        )),
        Command::WorkDiscard(args) => Some(plan(
            CommandName::Discard,
            args.selector.clone(),
            &["mark work view as discarded"],
        )),
        Command::WorkRestore(args) => Some(plan(
            CommandName::Restore,
            args.selector.clone(),
            &["restore a discarded work view"],
        )),
        Command::WorkCleanup(args) => Some(plan(
            CommandName::Cleanup,
            "retained work views".to_string(),
            if args.apply {
                &["remove cleanup-eligible work-view metadata and overlays"]
            } else {
                &["nothing; without --apply, cleanup only reports what is eligible"]
            },
        )),
        Command::ForgetLocal(args) => Some(plan(
            CommandName::ForgetLocal,
            args.project_path.clone(),
            &["remove the project's materialized bytes from this device"],
        )),
        Command::Archive(args) => Some(plan(
            CommandName::Archive,
            args.project_path.clone(),
            if args.restore {
                &["restore the project to active namespace listings"]
            } else {
                &["hide the project from default namespace listings without deleting local bytes"]
            },
        )),
        Command::Purge(args) => Some(plan(
            CommandName::Purge,
            args.project_path.clone(),
            if args.cancel {
                &["cancel the purge grace window and keep the archive"]
            } else {
                &["mark archived project objects collectible after the grace window"]
            },
        )),
        Command::Daemon(DaemonCommand::Install) => Some(plan(
            CommandName::DaemonInstall,
            "local OS service".to_string(),
            &["install or update daemon service files"],
        )),
        Command::Daemon(DaemonCommand::Restart) => Some(plan(
            CommandName::DaemonRestart,
            "local OS service".to_string(),
            &["restart daemon service"],
        )),
        Command::Daemon(DaemonCommand::Uninstall) => Some(plan(
            CommandName::DaemonUninstall,
            "local OS service".to_string(),
            &["uninstall daemon service files"],
        )),
        _ => None,
    }
}

fn recovery_plan(args: &recovery::RecoveryArgs) -> Option<DryRunPlan> {
    let plan = |target: String, would_change: &str| DryRunPlan {
        command: CommandName::Recover,
        target,
        would_change: vec![would_change.to_string()],
    };
    match args {
        recovery::RecoveryArgs::Create => Some(plan(
            "current workspace recovery key".to_string(),
            "create a new recovery key envelope",
        )),
        recovery::RecoveryArgs::Rotate => Some(plan(
            "current workspace recovery key".to_string(),
            "rotate recovery key material",
        )),
        recovery::RecoveryArgs::Revoke { envelope_id } => Some(plan(
            envelope_id.as_str().to_string(),
            "revoke recovery key envelope",
        )),
        recovery::RecoveryArgs::Use { envelope_id } => Some(plan(
            envelope_id.as_str().to_string(),
            "submit recovery words from stdin and create a device grant",
        )),
        // `status` and `verify` change nothing, so neither spec declares --dry-run.
        recovery::RecoveryArgs::Status | recovery::RecoveryArgs::Verify { .. } => None,
    }
}

fn trust_selector_label(selector: &TrustRequestSelector) -> String {
    match selector {
        TrustRequestSelector::Request(request_id) => request_id.as_str().to_string(),
        TrustRequestSelector::Code(code) => format!("matching code {code}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli_for(argv: &[&str]) -> Cli {
        let invocation = crate::cli::parse_args(argv.iter().copied());
        Cli {
            json: false,
            quiet: false,
            socket: invocation.socket,
            dry_run: invocation.dry_run,
            side_effect_level: invocation.side_effect_level,
            argv: invocation.argv,
            command: invocation.command.expect("parsed command"),
        }
    }

    #[test]
    fn apply_command_is_the_caller_argv_without_dry_run() {
        let cli = cli_for(&["work", "create", "apps/web", "spike", "--dry-run"]);

        assert_eq!(apply_command(&cli), "bowline work create apps/web spike");
    }

    #[test]
    fn cleanup_preview_never_hands_back_a_destructive_apply_line() {
        let cli = cli_for(&["work", "cleanup", "--dry-run"]);
        let level = cli.side_effect_level.expect("registry level");

        assert_eq!(apply_command(&cli), "bowline work cleanup");
        assert_eq!(resolved_risk(&cli.command, level), SideEffectLevel::None);
    }

    #[test]
    fn cleanup_apply_reports_the_mutation_it_would_perform() {
        let cli = cli_for(&["work", "cleanup", "--apply", "--dry-run"]);
        let level = cli.side_effect_level.expect("registry level");

        assert_eq!(apply_command(&cli), "bowline work cleanup --apply");
        assert_eq!(
            resolved_risk(&cli.command, level),
            SideEffectLevel::WorkspaceMetadata
        );
    }

    /// `--diff` prints and returns `changed: false`, so a harness gating on the
    /// declared level would have prompted, or refused, for a read.
    #[test]
    fn a_conflict_preview_is_not_declared_a_write() {
        // The root is explicit so the subject under test is the declared risk,
        // not whether this machine happens to have a root to infer.
        let aside = "src/auth.ts.bowline-conflict.9f3a2c1d";
        let preview = cli_for(&["resolve", aside, "--root", "~/Code", "--diff", "--dry-run"]);
        let level = preview.side_effect_level.expect("registry level");

        assert_eq!(
            resolved_risk(&preview.command, level),
            SideEffectLevel::None
        );

        let adopt = cli_for(&[
            "resolve",
            aside,
            "--root",
            "~/Code",
            "--take-remote",
            "--dry-run",
        ]);
        let level = adopt.side_effect_level.expect("registry level");

        assert_eq!(
            resolved_risk(&adopt.command, level),
            SideEffectLevel::FilesystemWrite
        );
        assert_eq!(
            apply_command(&adopt),
            format!("bowline resolve {aside} --root '~/Code' --take-remote")
        );
    }

    #[test]
    fn risk_comes_from_the_registry_side_effect_level() {
        // The root is explicit so the subject under test is the risk level, not
        // whether this machine happens to have a root to infer.
        let cli = cli_for(&[
            "device",
            "revoke",
            "--root",
            "~/Code",
            "--device",
            "dev_1",
            "--dry-run",
        ]);
        let level = cli.side_effect_level.expect("registry level");

        assert_eq!(
            resolved_risk(&cli.command, level),
            SideEffectLevel::TrustChange
        );
    }
}
