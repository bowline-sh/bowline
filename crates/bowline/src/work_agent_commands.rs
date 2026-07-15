use super::*;
use crate::command_error_classification::{print_agent_error, print_work_error};

mod remote_dispatch;

pub(super) fn print_events(args: EventsArgs, json: bool, quiet: bool) -> ExitCode {
    let generated_at = generated_at();
    let options = EventsOptions {
        db_path: metadata_db_path(),
        requested_path: selected_workspace_path(args.selection),
        workspace_scope: false,
        generated_at: generated_at.clone(),
        limit: args.limit,
    };

    match bowline_local::status::compose_events(options) {
        Ok(mut output) if json => {
            abbreviate_events_requested_path(&mut output);
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) if quiet => write_human_or_exit(
            CommandName::Events,
            generated_at,
            &render_events_quiet(&output),
        ),
        Ok(mut output) => {
            abbreviate_events_requested_path(&mut output);
            print!("{}", bowline_local::status::render_events_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(CommandName::Events, generated_at, &error.to_string(), json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

pub(super) fn print_history(args: HistoryArgs, json: bool, quiet: bool) -> ExitCode {
    let generated_at = generated_at();
    let mode = match args.mode {
        HistoryArgMode::Timeline => bowline_local::history::HistoryMode::Timeline,
        HistoryArgMode::Path => bowline_local::history::HistoryMode::Path,
        HistoryArgMode::Diff { from, to } => bowline_local::history::HistoryMode::Diff { from, to },
    };
    let options = bowline_local::history::HistoryOptions {
        db_path: metadata_db_path(),
        target_path: resolve_explicit_path(args.target_path),
        mode,
        generated_at: generated_at.clone(),
        limit: args.limit,
        cursor: args.cursor,
        since: args.since,
        until: args.until,
    };

    match bowline_local::history::compose_history(options) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) if quiet => write_human_or_exit(
            CommandName::History,
            generated_at,
            &render_history_quiet(&output),
        ),
        Ok(output) => {
            print!("{}", bowline_local::history::render_history_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(CommandName::History, generated_at, &error.to_string(), json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

pub(super) fn print_work_create(args: work::WorkCreateArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let project_path = io_helpers::resolve_project_path(args.project_path);
    let args = work::WorkCreateArgs {
        project_path,
        name: args.name,
        from: args.from,
    };
    match work::run_work_create(
        args,
        metadata_db_path(),
        runtime::device_id(),
        generated_at.clone(),
    ) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", work::render_work_create_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => print_work_error(CommandName::WorkCreate, generated_at, &error, json).into(),
    }
}

pub(super) fn print_work(args: work::WorkListArgs, json: bool, quiet: bool) -> ExitCode {
    let generated_at = generated_at();
    match work::run_list(
        args,
        metadata_db_path(),
        runtime::device_id(),
        generated_at.clone(),
    ) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) if quiet => {
            write_human_or_exit(CommandName::Work, generated_at, &render_work_quiet(&output))
        }
        Ok(output) => {
            print!("{}", work::render_list_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => print_work_error(CommandName::Work, generated_at, &error, json).into(),
    }
}

pub(super) fn print_work_diff(args: work::WorkSelectorArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    match work::run_diff(args, metadata_db_path(), generated_at.clone()) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", work::render_diff_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => print_work_error(CommandName::Diff, generated_at, &error, json).into(),
    }
}

pub(super) fn print_work_review(args: work::WorkSelectorArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    match work::run_diff(args, metadata_db_path(), generated_at.clone()) {
        Ok(mut output) if json => {
            output.command = CommandName::Review;
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(mut output) => {
            output.command = CommandName::Review;
            print!("{}", work::render_diff_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => print_work_error(CommandName::Review, generated_at, &error, json).into(),
    }
}

pub(super) fn print_work_lifecycle(
    lifecycle: work::WorkLifecycle,
    args: work::WorkSelectorArgs,
    json: bool,
) -> ExitCode {
    let generated_at = generated_at();
    let workspace_id = runtime::active_workspace_id();
    let result = if json {
        work::run_lifecycle(
            lifecycle,
            args,
            metadata_db_path(),
            runtime::daemon_device_id(&workspace_id),
            generated_at.clone(),
        )
    } else {
        let mut last_progress = None;
        work::run_lifecycle_with_progress(
            lifecycle,
            args,
            metadata_db_path(),
            runtime::daemon_device_id(&workspace_id),
            generated_at.clone(),
            |progress| {
                let rendered = work::render_accept_progress_human(progress);
                if rendered != last_progress {
                    if let Some(rendered) = &rendered {
                        eprint!("{rendered}");
                    }
                    last_progress = rendered;
                }
            },
        )
    };
    match result {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", work::render_lifecycle_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            let command = lifecycle.command_name();
            print_work_error(command, generated_at, &error, json).into()
        }
    }
}

pub(super) fn print_work_cleanup(args: work::WorkCleanupArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    match work::run_cleanup(args, metadata_db_path(), generated_at.clone()) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", work::render_cleanup_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => print_work_error(CommandName::Cleanup, generated_at, &error, json).into(),
    }
}

pub(super) fn print_agent_lease_create(args: agent::AgentLeaseCreateArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let on_device = args.on_device.clone();
    let remote_runtime = args.remote_runtime.clone();
    let remote_root = args.remote_root.clone();
    let args = agent::AgentLeaseCreateArgs {
        project_path: io_helpers::resolve_project_path(args.project_path),
        task: args.task,
        base: args.base,
        work_view: args.work_view || remote_runtime.is_some() || on_device.is_some(),
        force_stale: args.force_stale,
        on_device: on_device.clone(),
        remote_runtime: remote_runtime.clone(),
        remote_root,
    };
    let remote_root_for_handoff = args.remote_root.clone();
    if (remote_runtime.is_some() || on_device.is_some())
        && let Err(error) = runtime::control_plane()
    {
        print_runtime_error(CommandName::AgentStart, generated_at, &error, json);
        return ExitCode::from(EXIT_RUNTIME);
    }
    match agent::run_lease_create(
        args,
        metadata_db_path(),
        runtime::device_id(),
        generated_at.clone(),
    ) {
        Ok(mut output) => {
            if let Some(target) = on_device.as_deref() {
                match attach_authorized_dispatch_or_remote_bootstrap(
                    &mut output,
                    target,
                    remote_root_for_handoff.clone(),
                    &generated_at,
                ) {
                    Ok(()) => {}
                    Err(error) => {
                        rollback_remote_lease(&output, &generated_at);
                        let message = format!(
                            "agent dispatch failed after creating lease {}: {error}",
                            output.lease.id.as_str()
                        );
                        let exit_code = if error.requires_user_action() {
                            print_user_action_error(
                                CommandName::AgentStart,
                                generated_at,
                                "agent_device_selector_ambiguous",
                                &message,
                                "Run `bowline device list --json` and retry with an exact device ID.",
                                json,
                            )
                        } else {
                            print_runtime_error(
                                CommandName::AgentStart,
                                generated_at,
                                &message,
                                json,
                            )
                        };
                        return exit_code.into();
                    }
                }
            } else if remote_runtime.is_some()
                && let Err(error) = set_remote_lease_expiry(&mut output, &generated_at)
            {
                rollback_remote_lease(&output, &generated_at);
                print_runtime_error(
                    CommandName::AgentStart,
                    generated_at,
                    &format!(
                        "remote lease expiry setup failed after creating lease {}: {error}",
                        output.lease.id.as_str()
                    ),
                    json,
                );
                return ExitCode::from(EXIT_RUNTIME);
            }
            if let Err(error) = attach_remote_bootstrap_actions(
                &mut output,
                remote_runtime,
                remote_root_for_handoff,
            ) {
                rollback_remote_lease(&output, &generated_at);
                print_runtime_error(
                    CommandName::AgentStart,
                    generated_at,
                    &format!(
                        "remote bootstrap failed after creating lease {}: {error}",
                        output.lease.id.as_str()
                    ),
                    json,
                );
                return ExitCode::from(EXIT_RUNTIME);
            }
            if json {
                print_json(&output);
                return ExitCode::SUCCESS;
            }
            print!("{}", agent::render_lease_create_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => print_agent_error(CommandName::AgentStart, generated_at, &error, json).into(),
    }
}

pub(super) fn print_agent_complete(args: agent::AgentLeaseSelectorArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    match agent::run_complete(args, metadata_db_path(), generated_at.clone()) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", agent::render_complete_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_agent_error(CommandName::AgentComplete, generated_at, &error, json).into()
        }
    }
}

pub(super) fn print_agent_cancel(args: agent::AgentLeaseSelectorArgs, json: bool) -> ExitCode {
    print_agent_update(
        CommandName::AgentCancel,
        agent::run_cancel(args, metadata_db_path(), generated_at()),
        json,
    )
}

pub(super) fn print_agent_extend(args: agent::AgentLeaseExtendArgs, json: bool) -> ExitCode {
    print_agent_update(
        CommandName::AgentExtend,
        agent::run_extend(args, metadata_db_path(), generated_at()),
        json,
    )
}

fn print_agent_update(
    command: CommandName,
    result: Result<
        bowline_core::commands::AgentLeaseUpdateCommandOutput,
        bowline_local::agents::AgentError,
    >,
    json: bool,
) -> ExitCode {
    match result {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", agent::render_lease_update_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => print_agent_error(command, generated_at(), &error, json).into(),
    }
}

const REMOTE_BOOTSTRAP_LEASE_TICKS: u64 = 600;

fn attach_authorized_dispatch_or_remote_bootstrap(
    output: &mut bowline_core::commands::AgentLeaseCreateCommandOutput,
    target: &str,
    remote_root: Option<String>,
    generated_at: &str,
) -> Result<(), remote_dispatch::DispatchTargetError> {
    match authorized_dispatch_target(output.workspace_id.as_str(), target)? {
        Some(target_device_id) => {
            set_remote_lease_expiry(output, generated_at)?;
            mark_output_as_pending_dispatch(output, &target_device_id, generated_at)?;
            publish_dispatched_lease(output, &target_device_id, generated_at)?;
            output.next_actions.clear();
            output.next_actions.push(RepairCommand::inspect(
                format!("Watch dispatched lease on {target_device_id}"),
                Some(format!(
                    "bowline agent context --lease {} --json",
                    io_helpers::shell_word(output.lease.id.as_str())
                )),
            ));
            Ok(())
        }
        None => {
            set_remote_lease_expiry(output, generated_at)?;
            attach_remote_bootstrap_actions(output, Some(target.to_string()), remote_root)?;
            Ok(())
        }
    }
}

fn authorized_dispatch_target(
    workspace_id: &str,
    target: &str,
) -> Result<Option<String>, remote_dispatch::DispatchTargetError> {
    let control_plane =
        runtime::control_plane().map_err(remote_dispatch::DispatchTargetError::from)?;
    let trust = control_plane
        .list_device_trust(&bowline_core::ids::WorkspaceId::new(workspace_id))
        .map_err(|error| remote_dispatch::DispatchTargetError::Runtime(error.to_string()))?;
    let authorized_devices = trust.authorized_devices;
    if let Some(device) = authorized_devices
        .iter()
        .find(|device| device.device_id == target)
    {
        return Ok(Some(device.device_id.as_str().to_string()));
    }
    let name_matches = authorized_devices
        .iter()
        .filter(|device| device.device_name == target)
        .collect::<Vec<_>>();
    match name_matches.as_slice() {
        [] => Ok(None),
        [device] => Ok(Some(device.device_id.as_str().to_string())),
        devices => Err(remote_dispatch::DispatchTargetError::AmbiguousDeviceName {
            target: target.to_string(),
            device_ids: devices
                .iter()
                .map(|device| device.device_id.as_str())
                .map(str::to_string)
                .collect(),
        }),
    }
}

fn mark_output_as_pending_dispatch(
    output: &mut bowline_core::commands::AgentLeaseCreateCommandOutput,
    target_device_id: &str,
    generated_at: &str,
) -> Result<(), String> {
    output.lease.dispatch_state = bowline_core::commands::AgentLeaseDispatchState::Pending;
    output.lease.target_device_ref = Some(bowline_core::ids::DeviceId::new(
        target_device_id.to_string(),
    ));
    output.lease.origin_device_ref = Some(output.lease.device_id.clone());
    output.lease.session_state = bowline_core::commands::AgentSessionState::Provisional;
    output.lease.status_summary = format!("pending dispatch to {target_device_id}");
    output.lease.updated_at = generated_at.to_string();
    let store = bowline_local::metadata::MetadataStore::open(
        metadata_db_path().ok_or_else(|| "metadata database path is missing".to_string())?,
    )
    .map_err(|error| error.to_string())?;
    store
        .upsert_agent_lease(&output.lease)
        .map_err(|error| error.to_string())
}

fn publish_dispatched_lease(
    output: &bowline_core::commands::AgentLeaseCreateCommandOutput,
    target_device_id: &str,
    generated_at: &str,
) -> Result<(), String> {
    let control_plane = runtime::control_plane()?;
    let write_target_mode = match output.lease.write_target_mode {
        bowline_core::commands::AgentWriteTargetMode::Direct => {
            bowline_control_plane::LeaseWriteTargetMode::Direct
        }
        bowline_core::commands::AgentWriteTargetMode::WorkView => {
            bowline_control_plane::LeaseWriteTargetMode::WorkView
        }
    };
    control_plane
        .create_lease(bowline_control_plane::LeaseCreate {
            workspace_id: output.workspace_id.clone(),
            lease_id: output.lease.id.clone(),
            project_id: output.project_id.clone(),
            device_id: output.lease.device_id.clone(),
            target_device_ref: Some(target_device_id.to_string()),
            origin_device_ref: Some(output.lease.device_id.as_str().to_string()),
            write_target_mode,
            work_view_id: Some(output.lease.work_view_id.clone()),
            base_snapshot_id: output.lease.base_snapshot_id.clone(),
            task_label: Some(remote_dispatch::compact_task_label(&output.lease.task)),
            session_state: bowline_control_plane::LeaseSessionState::Provisional,
            status_code: "pending".to_string(),
            expires_at: control_plane_lease_expiry(generated_at),
        })
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn control_plane_lease_expiry(generated_at: &str) -> bowline_control_plane::ControlPlaneTimestamp {
    let generated_at =
        time::OffsetDateTime::parse(generated_at, &time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    let expires_at = generated_at + time::Duration::seconds(REMOTE_BOOTSTRAP_LEASE_TICKS as i64);
    let millis = expires_at.unix_timestamp_nanos() / 1_000_000;
    bowline_control_plane::ControlPlaneTimestamp {
        tick: if millis < 0 {
            0
        } else {
            u64::try_from(millis).unwrap_or(u64::MAX)
        },
    }
}

fn set_remote_lease_expiry(
    output: &mut bowline_core::commands::AgentLeaseCreateCommandOutput,
    generated_at: &str,
) -> Result<(), String> {
    let generated_at =
        time::OffsetDateTime::parse(generated_at, &time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    output.lease.expires_at = (generated_at
        + time::Duration::seconds(REMOTE_BOOTSTRAP_LEASE_TICKS as i64))
    .format(&time::format_description::well_known::Rfc3339)
    .map_err(|error| error.to_string())?;
    output.lease.updated_at = generated_at
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|error| error.to_string())?;
    let store = bowline_local::metadata::MetadataStore::open(
        metadata_db_path().ok_or_else(|| "metadata database path is missing".to_string())?,
    )
    .map_err(|error| error.to_string())?;
    store
        .upsert_agent_lease(&output.lease)
        .map_err(|error| error.to_string())
}

fn rollback_remote_lease(
    output: &bowline_core::commands::AgentLeaseCreateCommandOutput,
    _generated_at: &str,
) {
    let Some(db_path) = metadata_db_path() else {
        return;
    };
    let Ok(store) = bowline_local::metadata::MetadataStore::open(db_path) else {
        return;
    };
    if let Err(error) = store.delete_agent_mcp_tokens_for_lease(&output.lease.id) {
        eprintln!(
            "bowline work lease rollback failed to delete MCP tokens for {}: {error}",
            output.lease.id.as_str()
        );
    }
    if let Err(error) = store.delete_agent_lease(&output.lease.id) {
        eprintln!(
            "bowline work lease rollback failed to delete lease {}: {error}",
            output.lease.id.as_str()
        );
    }
}

fn attach_remote_bootstrap_actions(
    output: &mut bowline_core::commands::AgentLeaseCreateCommandOutput,
    remote_runtime: Option<String>,
    remote_root: Option<String>,
) -> Result<(), String> {
    let Some(runtime_name) = remote_runtime else {
        return Ok(());
    };
    let control_plane = runtime::control_plane()?;
    let remote_root = remote_root.unwrap_or_else(|| "~/Code".to_string());
    let mut input = bowline_control_plane::BootstrapSessionInput::new(output.workspace_id.as_str());
    input.host = Some(runtime_name.clone());
    input.runtime = Some(runtime_name.clone());
    input.lease_id = Some(output.lease.id.clone());
    input.root = Some(remote_root.clone());
    input.expires_in_ticks = REMOTE_BOOTSTRAP_LEASE_TICKS;
    let receipts_json = remote_setup_receipts_json(output)?;
    input.setup_receipts_digest = Some(setup_receipts_digest(&receipts_json));
    let lease_json = serde_json::to_string(&output.lease).map_err(|error| error.to_string())?;
    input.lease_handoff_digest = Some(lease_handoff_digest(&lease_json));
    let session = control_plane
        .create_bootstrap_session(input)
        .map_err(|error| error.to_string())?;
    let handoff_file = write_remote_handoff_file(
        output.lease.id.as_str(),
        &session.session_id,
        &session.token,
        &receipts_json,
        &lease_json,
    )?;
    let approver_root =
        runtime::active_workspace_root().unwrap_or_else(|| output.lease.write_target_path.clone());
    let remote_join_command = remote_lease_join_command(
        output.workspace_id.as_str(),
        &approver_root,
        &remote_root,
        output.lease.id.as_str(),
        &runtime_name,
    );
    let join_command =
        remote_join_command_with_cleanup(&handoff_file, &runtime_name, &remote_join_command);
    output.next_actions.push(RepairCommand::mutating(
        format!("Join {runtime_name} sandbox to this lease"),
        Some(join_command),
    ));
    Ok(())
}

fn remote_lease_join_command(
    workspace_id: &str,
    approver_root: &str,
    remote_root: &str,
    lease_id: &str,
    runtime_name: &str,
) -> String {
    format!(
        "IFS= read -r BOWLINE_BOOTSTRAP_TOKEN; export BOWLINE_BOOTSTRAP_TOKEN; IFS= read -r BOWLINE_AGENT_RECEIPTS_JSON; export BOWLINE_AGENT_RECEIPTS_JSON; IFS= read -r BOWLINE_AGENT_LEASE_JSON; export BOWLINE_AGENT_LEASE_JSON; BOWLINE_WORKSPACE_ID={} BOWLINE_APPROVER_ROOT={} bowline lease join --root {} --lease {} --runtime {} --json",
        io_helpers::shell_word(workspace_id),
        io_helpers::shell_word(approver_root),
        io_helpers::shell_word(remote_root),
        io_helpers::shell_word(lease_id),
        io_helpers::shell_word(runtime_name),
    )
}

fn remote_join_command_with_cleanup(
    handoff_file: &std::path::Path,
    runtime_name: &str,
    remote_join_command: &str,
) -> String {
    let script = "handoff=$1; host=$2; remote=$3; trap 'rm -f \"$handoff\"' EXIT; cat \"$handoff\" | ssh \"$host\" \"$remote\"";
    format!(
        "sh -c {} sh {} {} {}",
        io_helpers::shell_word(script),
        io_helpers::shell_word(&handoff_file.display().to_string()),
        io_helpers::shell_word(runtime_name),
        io_helpers::shell_word(remote_join_command),
    )
}

fn remote_setup_receipts_json(
    output: &bowline_core::commands::AgentLeaseCreateCommandOutput,
) -> Result<String, String> {
    let store = bowline_local::metadata::MetadataStore::open(
        metadata_db_path().ok_or_else(|| "metadata database path is missing".to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let receipts = store
        .setup_receipts(&output.workspace_id)
        .map_err(|error| error.to_string())?
        .into_iter()
        .filter(|receipt| {
            receipt.project_id.as_ref() == Some(&output.project_id)
                && matches!(receipt.state.as_str(), "completed" | "approved")
        })
        .collect::<Vec<_>>();
    serde_json::to_string(&receipts).map_err(|error| error.to_string())
}

fn setup_receipts_digest(receipts_json: &str) -> String {
    format!(
        "setup_receipts_blake3:{}",
        blake3::hash(receipts_json.as_bytes()).to_hex()
    )
}

fn lease_handoff_digest(lease_json: &str) -> String {
    format!(
        "lease_handoff_blake3:{}",
        blake3::hash(lease_json.as_bytes()).to_hex()
    )
}

fn write_remote_handoff_file(
    lease_id: &str,
    session_id: &str,
    token: &str,
    receipts_json: &str,
    lease_json: &str,
) -> Result<PathBuf, String> {
    let state_dir = default_database_path()
        .map_err(|error| error.to_string())?
        .parent()
        .ok_or_else(|| "metadata database path has no parent".to_string())?
        .join("bootstrap");
    std::fs::create_dir_all(&state_dir).map_err(|error| error.to_string())?;
    let token_path = state_dir.join(format!("{lease_id}-{session_id}.handoff"));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write;
    let mut file = options
        .open(&token_path)
        .map_err(|error| error.to_string())?;
    if let Err(error) = (|| {
        writeln!(file, "{token}").map_err(|error| error.to_string())?;
        file.write_all(receipts_json.as_bytes())
            .map_err(|error| error.to_string())?;
        writeln!(file).map_err(|error| error.to_string())?;
        file.write_all(lease_json.as_bytes())
            .map_err(|error| error.to_string())?;
        writeln!(file).map_err(|error| error.to_string())
    })() {
        let _ = std::fs::remove_file(&token_path);
        return Err(error);
    }
    Ok(token_path)
}

pub(super) fn print_agent_context(args: agent::AgentLeaseSelectorArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    match agent::run_context(args, metadata_db_path(), generated_at.clone()) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", agent::render_context_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_agent_error(CommandName::AgentContext, generated_at, &error, json).into()
        }
    }
}

pub(super) fn print_agent_prompt(args: agent::AgentLeaseSelectorArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    match agent::run_prompt(args, metadata_db_path(), generated_at.clone()) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            print!("{}", agent::render_prompt_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_agent_error(CommandName::AgentPrompt, generated_at, &error, json).into()
        }
    }
}

pub(super) fn print_agent_mcp_token(args: agent::AgentMcpTokenArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    match agent::run_mcp_token(args, metadata_db_path(), generated_at.clone()) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            println!(
                "MCP token file created for lease {}\n{}\n",
                output.lease_id.as_str(),
                output.token_file
            );
            println!("Pass `leaseId` and `mcpTokenFile` to Bowline MCP tool calls.");
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_agent_error(CommandName::AgentMcpToken, generated_at, &error, json).into()
        }
    }
}

pub(super) fn print_bootstrap_ssh(args: bootstrap::BootstrapSshArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let output = bootstrap::run(args, generated_at);
    let success = bootstrap_ssh_succeeded(&output);
    if json {
        print_json(&output);
    } else {
        print!("{}", render_bootstrap_ssh_human(&output));
    }
    if success {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(EXIT_RUNTIME)
    }
}

pub(super) fn bootstrap_ssh_succeeded(
    output: &bowline_core::commands::BootstrapSshCommandOutput,
) -> bool {
    output.trusted
        && output
            .steps
            .iter()
            .all(|step| step.state != bowline_core::commands::BootstrapStepState::Blocked)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_plane_lease_expiry_uses_millisecond_ticks() {
        assert_eq!(
            control_plane_lease_expiry("1970-01-01T00:00:00Z").tick,
            REMOTE_BOOTSTRAP_LEASE_TICKS * 1000
        );
    }

    #[test]
    fn remote_join_command_removes_handoff_file_on_exit() {
        let command = remote_join_command_with_cleanup(
            std::path::Path::new("/tmp/bowline-bootstrap/lease-session.handoff"),
            "codex-cloud",
            "bowline lease join --json",
        );

        // Assert with the same quoting helper the builder uses so the test
        // tracks semantics (cleanup trap + ssh pipe), not shell escaping.
        let script = "handoff=$1; host=$2; remote=$3; trap 'rm -f \"$handoff\"' EXIT; \
                      cat \"$handoff\" | ssh \"$host\" \"$remote\"";
        assert!(command.starts_with("sh -c "));
        assert!(command.contains(&io_helpers::shell_word(script)));
        assert!(command.contains(&io_helpers::shell_word(
            "/tmp/bowline-bootstrap/lease-session.handoff"
        )));
        assert!(command.contains("codex-cloud"));
        assert!(command.contains(&io_helpers::shell_word("bowline lease join --json")));
    }

    #[test]
    fn remote_join_command_reads_lease_json_from_handoff_stream() {
        let command = remote_lease_join_command(
            "ws_code",
            "~/Code/app",
            "/workspace/Code",
            "lease_remote_json",
            "codex-cloud",
        );

        assert!(command.contains("IFS= read -r BOWLINE_BOOTSTRAP_TOKEN"));
        assert!(command.contains("IFS= read -r BOWLINE_AGENT_RECEIPTS_JSON"));
        assert!(command.contains("IFS= read -r BOWLINE_AGENT_LEASE_JSON"));
        assert!(command.contains("export BOWLINE_AGENT_LEASE_JSON"));
        assert!(command.contains("bowline lease join --root /workspace/Code"));
        assert!(command.contains("--lease lease_remote_json"));
        assert!(!command.contains("BOWLINE_AGENT_LEASE_JSON="));
        assert!(!command.contains("{\"id\""));
    }
}
