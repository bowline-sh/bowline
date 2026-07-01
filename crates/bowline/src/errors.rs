use super::*;

pub(super) fn print_ambiguous_init_root(
    candidates: Vec<PathBuf>,
    generated_at: String,
    json: bool,
) {
    let roots = candidates
        .iter()
        .map(|path| abbreviate_requested_path(&path.display().to_string()))
        .collect::<Vec<_>>();
    let message = format!(
        "bare bowline login found existing non-~/Code roots; pass an explicit root: {}",
        roots.join(", ")
    );
    let next_actions = roots
        .iter()
        .map(|root| SafeAction {
            label: format!("Log in with {root}"),
            command: Some(format!("bowline login --root {root}")),
        })
        .collect::<Vec<_>>();

    print_command_usage_error(
        CommandUsageError {
            command: CommandName::Init,
            code: "ambiguous_root",
            message,
            next_actions,
        },
        generated_at,
        json,
    );
}

pub(super) fn print_dev_cloud_spike(args: CloudSpikeArgs, json: bool) -> ExitCode {
    match args.provider {
        CloudSpikeProvider::Fake => match run_fake_cloud_spike() {
            Ok(report) => {
                if json {
                    print_json(&CloudSpikeFakeOutput {
                        ok: true,
                        command: "dev cloud-spike",
                        provider: "fake",
                        workspace_id: &report.workspace_id,
                        starting_version: report.starting_version,
                        advanced_version: report.advanced_version,
                        pack_object_count: report.pack_object_count,
                        source_file_count: report.source_file_count,
                        hydrated_cold_file_byte_len: report.hydrated_cold_file_bytes.len(),
                        stale_ref_detected: report.stale_ref_detected,
                        device_approval_harness_only: report.device_approval_harness_only,
                        event_count: report.event_count,
                    });
                } else {
                    println!(
                        "bowline cloud spike fake: ok ({} pack objects, stale-ref proven)",
                        report.pack_object_count
                    );
                }
                ExitCode::SUCCESS
            }
            Err(error) => {
                print_runtime_error(
                    CommandName::Unknown,
                    generated_at(),
                    &error.to_string(),
                    json,
                );
                ExitCode::from(EXIT_RUNTIME)
            }
        },
        CloudSpikeProvider::Hosted => match skip_hosted_cloud_spike_from_env() {
            Some(skip) => {
                if json {
                    print_json(&CloudSpikeSkipOutput {
                        ok: true,
                        command: "dev cloud-spike",
                        provider: "hosted",
                        skipped: true,
                        missing_env: skip.missing_env,
                    });
                } else {
                    println!(
                        "bowline cloud spike hosted: skipped (missing {})",
                        skip.missing_env.join(", ")
                    );
                }
                ExitCode::SUCCESS
            }
            None => match run_hosted_cloud_spike_from_env() {
                Ok(report) => {
                    if json {
                        print_json(&CloudSpikeFakeOutput {
                            ok: true,
                            command: "dev cloud-spike",
                            provider: "hosted",
                            workspace_id: &report.workspace_id,
                            starting_version: report.starting_version,
                            advanced_version: report.advanced_version,
                            pack_object_count: report.pack_object_count,
                            source_file_count: report.source_file_count,
                            hydrated_cold_file_byte_len: report.hydrated_cold_file_bytes.len(),
                            stale_ref_detected: report.stale_ref_detected,
                            device_approval_harness_only: report.device_approval_harness_only,
                            event_count: report.event_count,
                        });
                    } else {
                        println!(
                            "bowline cloud spike hosted: ok ({} pack objects, stale-ref proven)",
                            report.pack_object_count
                        );
                    }
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    print_runtime_error(
                        CommandName::Unknown,
                        generated_at(),
                        &error.to_string(),
                        json,
                    );
                    ExitCode::from(EXIT_RUNTIME)
                }
            },
        },
    }
}

pub(super) fn print_usage_error(command: CommandName, code: &str, message: &str, json: bool) {
    if json {
        print_json(&CommandErrorOutput {
            contract_version: CONTRACT_VERSION,
            command,
            generated_at: generated_at(),
            status: CommandErrorStatus::UsageError,
            error: CommandError {
                code: code.to_string(),
                message: message.to_string(),
                recoverability: CommandRecoverability::UserAction,
                remediation: Some(
                    "Run `bowline help --json` or `bowline help <topic> --json`.".to_string(),
                ),
                details: None,
                retry_after_seconds: None,
                correlation_id: None,
            },
            next_actions: vec![SafeAction {
                label: "Inspect CLI help".to_string(),
                command: Some("bowline help --json".to_string()),
            }],
        });
    } else {
        eprintln!("bowline usage error: {message}");
    }
}

pub(super) fn print_command_usage_error(
    error: CommandUsageError,
    generated_at: String,
    json: bool,
) {
    if json {
        print_json(&CommandErrorOutput {
            contract_version: CONTRACT_VERSION,
            command: error.command,
            generated_at,
            status: CommandErrorStatus::UsageError,
            error: CommandError {
                code: error.code.to_string(),
                message: error.message,
                recoverability: CommandRecoverability::UserAction,
                remediation: Some(
                    "Inspect command help and retry with valid arguments.".to_string(),
                ),
                details: None,
                retry_after_seconds: None,
                correlation_id: None,
            },
            next_actions: error.next_actions,
        });
    } else {
        eprintln!("bowline usage error: {}", error.message);
    }
}

pub(super) fn print_runtime_error(
    command: CommandName,
    generated_at: String,
    message: &str,
    json: bool,
) {
    if json {
        let output = bowline_local::status::command_error_output(
            command,
            generated_at,
            "runtime_error",
            message,
            CommandRecoverability::Retry,
        );
        print_json(&output);
    } else {
        let command = match command {
            CommandName::Init => "init",
            CommandName::Status => "status",
            CommandName::Explain => "explain",
            CommandName::Events => "events",
            CommandName::DaemonStart => "daemon start",
            CommandName::DaemonStop => "daemon stop",
            CommandName::DaemonInstall => "daemon install",
            CommandName::DaemonRestart => "daemon restart",
            CommandName::DaemonUninstall => "daemon uninstall",
            _ => "command",
        };
        eprintln!("bowline {command} failed: {message}");
    }
}
