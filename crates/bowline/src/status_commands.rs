use super::*;

pub(super) fn print_status(args: StatusArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let options = StatusOptions {
        db_path: metadata_db_path(),
        requested_path: requested_path(args.path),
        workspace_scope: args.workspace,
        generated_at: generated_at.clone(),
    };

    if args.watch {
        return print_status_watch(options, generated_at, json);
    }

    match compose_status_for_cli(options) {
        Ok(output) if json => {
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            let human = bowline_local::status::render_status_human(&output);
            print!("{human}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(CommandName::Status, generated_at, &error.to_string(), json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

pub(super) fn print_actions(args: ActionsArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let options = StatusOptions {
        db_path: metadata_db_path(),
        requested_path: requested_path(args.path),
        workspace_scope: args.workspace,
        generated_at: generated_at.clone(),
    };
    match compose_status_for_cli(options) {
        Ok(output) if json => {
            let output = surface::actions::from_status(&output);
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(output) => {
            let output = surface::actions::from_status(&output);
            let human = surface::human::render_actions(&output);
            write_human_or_exit(CommandName::Actions, generated_at, &human)
        }
        Err(error) => {
            print_runtime_error(CommandName::Actions, generated_at, &error.to_string(), json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

pub(super) fn print_tui(args: TuiArgs, json: bool, socket: &Path) -> ExitCode {
    let generated_at = generated_at();
    if json {
        print_command_usage_error(
            CommandUsageError {
                command: CommandName::Tui,
                code: "usage_error",
                message: "bowline tui is an interactive command; use `bowline status --json`"
                    .to_string(),
                next_actions: vec![SafeAction {
                    label: "Inspect status as JSON".to_string(),
                    command: Some("bowline status --json".to_string()),
                }],
            },
            generated_at,
            true,
        );
        return ExitCode::from(EXIT_USAGE);
    }
    let options = StatusOptions {
        db_path: metadata_db_path(),
        requested_path: requested_path(args.path),
        workspace_scope: false,
        generated_at: generated_at.clone(),
    };
    match compose_status_for_cli(options) {
        Ok(output) if !io::stdin().is_terminal() || !io::stdout().is_terminal() => {
            let output = surface::actions::from_status(&output);
            let human = surface::human::render_actions(&output);
            write_human_or_exit(CommandName::Tui, generated_at, &human)
        }
        Ok(output) => {
            let model =
                surface::tui::TuiModel::from_actions(&surface::actions::from_status(&output));
            match surface::tui::run_app(model) {
                Ok(Some(command)) => run_confirmed_tui_command(&command, socket),
                Ok(None) => ExitCode::SUCCESS,
                Err(error) => {
                    print_runtime_error(CommandName::Tui, generated_at, &error.to_string(), false);
                    ExitCode::from(EXIT_RUNTIME)
                }
            }
        }
        Err(error) => {
            print_runtime_error(CommandName::Tui, generated_at, &error.to_string(), json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

pub(super) fn compose_status_for_cli(
    options: StatusOptions,
) -> Result<StatusCommandOutput, bowline_local::status::LocalStatusError> {
    let mut output = bowline_local::status::compose_status(options)?;
    attach_device_status_if_available(&mut output);
    abbreviate_status_requested_path(&mut output);
    Ok(output)
}

pub(super) fn print_search(args: SearchArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let offset = args.cursor.unwrap_or(0);
    let page_limit = args.limit;
    let options = bowline_local::search::SearchCommandOptions {
        db_path: metadata_db_path(),
        query: args.query,
        requested_path: requested_path(args.path),
        path_prefix: args.path_prefix,
        generated_at: generated_at.clone(),
        limit: page_limit,
        project_identity: None,
    };
    match bowline_local::search::search_workspace_page(options, offset) {
        Ok(mut output) if json => {
            page_search_output(&mut output, offset, page_limit);
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(mut output) => {
            page_search_output(&mut output, offset, page_limit);
            print!("{}", render_search_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(CommandName::Search, generated_at, &error.to_string(), json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

pub(super) fn print_symbols(args: SymbolsArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let offset = args.cursor.unwrap_or(0);
    let page_limit = args.limit;
    let options = bowline_local::symbols::SymbolCommandOptions {
        db_path: metadata_db_path(),
        query: args.query,
        requested_path: requested_path(args.path),
        path_prefix: args.path_prefix,
        generated_at: generated_at.clone(),
        limit: page_limit,
        project_identity: None,
    };
    match bowline_local::symbols::lookup_symbols_page(options, offset) {
        Ok(mut output) if json => {
            page_symbol_output(&mut output, offset, page_limit);
            print_json(&output);
            ExitCode::SUCCESS
        }
        Ok(mut output) => {
            page_symbol_output(&mut output, offset, page_limit);
            print!("{}", render_symbols_human(&output));
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error(CommandName::Symbols, generated_at, &error.to_string(), json);
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

pub(super) fn page_search_output(
    output: &mut bowline_core::commands::SearchCommandOutput,
    offset: usize,
    limit: usize,
) {
    let previous_truncated = output.truncated;
    let mut results = std::mem::take(&mut output.results);
    let has_more = previous_truncated || results.len() > limit;
    results.truncate(limit);
    output.results = results;
    output.truncated = has_more;
    output.next_cursor = next_exploration_cursor(offset, limit, has_more);
}

pub(super) fn page_symbol_output(
    output: &mut bowline_core::commands::SymbolCommandOutput,
    offset: usize,
    limit: usize,
) {
    let previous_truncated = output.truncated;
    let mut symbols = std::mem::take(&mut output.symbols);
    let has_more = previous_truncated || symbols.len() > limit;
    symbols.truncate(limit);
    output.symbols = symbols;
    output.truncated = has_more;
    output.next_cursor = next_exploration_cursor(offset, limit, has_more);
}

pub(super) fn next_exploration_cursor(
    offset: usize,
    limit: usize,
    has_more: bool,
) -> Option<String> {
    if !has_more {
        return None;
    }
    let next_offset = offset.saturating_add(limit);
    (next_offset <= MAX_EXPLORATION_CURSOR_OFFSET).then(|| format!("v1:{next_offset}"))
}

pub(super) fn attach_device_status_if_available(output: &mut StatusCommandOutput) {
    if !runtime::passive_secret_store_probe_allowed() {
        return;
    }

    let Ok(key_store) = runtime::key_store() else {
        return;
    };
    if !matches!(key_store.load_account_tokens(), Ok(Some(_))) {
        return;
    }
    let Ok(control_plane) = runtime::control_plane() else {
        return;
    };
    let Ok(trust) = control_plane.list_device_trust(output.workspace_id.as_str()) else {
        return;
    };

    let local_device_id = runtime::daemon_device_id(&output.workspace_id);
    let local_id = local_device_id.as_str();
    if let Some(revoked) = trust
        .revoked_devices
        .iter()
        .find(|device| device.device_id == local_id)
    {
        output.status.level = StatusLevel::Limited;
        output.status.attention_items.push(format!(
            "This device was revoked from workspace {}.",
            output.workspace_id.as_str()
        ));
        let item = device_status_item(
            output,
            StatusSubjectKind::Device,
            revoked.device_id.as_str(),
            Some(DeviceId::new(revoked.device_id.clone())),
            format!(
                "This device is revoked; future sync and trust operations are blocked. Reason: {}",
                revoked.reason
            ),
        );
        output.items.push(item);
        output.next_actions.push(SafeAction {
            label: "Inspect workspace status".to_string(),
            command: Some("bowline status".to_string()),
        });
        return;
    }

    if let Some(device) = trust
        .authorized_devices
        .iter()
        .find(|device| device.device_id == local_id)
    {
        let item = device_status_item(
            output,
            StatusSubjectKind::Device,
            device.device_id.as_str(),
            Some(DeviceId::new(device.device_id.clone())),
            trusted_device_summary(device.device_id.as_str(), device.device_name.as_str()),
        );
        output.items.push(item);
    } else if let Some(request) = trust
        .pending_requests
        .iter()
        .find(|request| request.device_id == local_id)
    {
        if output.status.level == StatusLevel::Healthy {
            output.status.level = StatusLevel::Limited;
        }
        output
            .status
            .attention_items
            .push("This device is waiting for approval before it can sync.".to_string());
        let item = device_status_item(
            output,
            StatusSubjectKind::DeviceApprovalRequest,
            request.request_id.as_str(),
            Some(DeviceId::new(request.device_id.clone())),
            "This device has a pending approval request.".to_string(),
        );
        output.items.push(item);
    } else if !trust.authorized_devices.is_empty() {
        if output.status.level == StatusLevel::Healthy {
            output.status.level = StatusLevel::Limited;
        }
        output
            .status
            .attention_items
            .push("This device is not trusted for the workspace yet.".to_string());
        let item = device_status_item(
            output,
            StatusSubjectKind::Device,
            local_device_id.as_str(),
            Some(local_device_id.clone()),
            "Run `bowline login` to request workspace trust.".to_string(),
        );
        output.items.push(item);
    }

    if !trust.pending_requests.is_empty() {
        if output.status.level == StatusLevel::Healthy {
            output.status.level = StatusLevel::Attention;
        }
        output.status.attention_items.push(format!(
            "{} device approval request(s) are waiting.",
            trust.pending_requests.len()
        ));
        let pending_items = trust
            .pending_requests
            .into_iter()
            .map(|request| {
                output.next_actions.push(SafeAction {
                    label: format!("Approve {}", request.device_name),
                    command: Some(format!("bowline approve {}", request.request_id)),
                });
                device_status_item(
                    output,
                    StatusSubjectKind::DeviceApprovalRequest,
                    request.request_id.as_str(),
                    Some(DeviceId::new(request.device_id.clone())),
                    format!(
                        "{} is waiting for approval with matching code {}.",
                        request.device_name, request.matching_code
                    ),
                )
            })
            .collect::<Vec<_>>();
        output.items.extend(pending_items);
        output.next_actions.push(SafeAction {
            label: "Review workspace status".to_string(),
            command: Some("bowline status".to_string()),
        });
    }
}

pub(super) fn trusted_device_summary(device_id: &str, device_name: &str) -> String {
    if device_name == device_id {
        return format!("This device is trusted as {device_id}.");
    }
    format!("This device is trusted as {device_id} ({device_name}).")
}

pub(super) fn device_status_item(
    output: &StatusCommandOutput,
    subject_kind: StatusSubjectKind,
    subject_id: impl Into<String>,
    device_id: Option<DeviceId>,
    summary: String,
) -> StatusItem {
    StatusItem {
        kind: StatusItemKind::Device,
        summary,
        subject: Some(StatusSubject {
            kind: subject_kind,
            id: subject_id.into(),
            path: None,
        }),
        path: None,
        classification: None,
        mode: None,
        access: Vec::new(),
        event_id: None,
        event_name: None,
        device_id,
        lease_id: None,
        project_id: output.project_id.clone(),
        snapshot_id: None,
        policy_version: None,
        env_record_id: None,
    }
}

pub(super) fn print_status_watch(
    options: StatusOptions,
    generated_at: String,
    json: bool,
) -> ExitCode {
    let mut sequence = 1;
    let mut last_output = None;

    loop {
        let output = match bowline_local::status::compose_status(options.clone()) {
            Ok(mut output) => {
                attach_device_status_if_available(&mut output);
                abbreviate_status_requested_path(&mut output);
                output
            }
            Err(error) => {
                print_runtime_error(CommandName::Status, generated_at, &error.to_string(), json);
                return ExitCode::from(EXIT_RUNTIME);
            }
        };

        if last_output.as_ref() != Some(&output) {
            let frame = status_watch_frame(output.clone(), sequence);
            let write_result = if json {
                write_json_line(&frame)
            } else {
                write_text(&surface::human::render_watch_frame(&frame))
            };
            if let Err(error) = write_result {
                return if error.kind() == io::ErrorKind::BrokenPipe {
                    ExitCode::SUCCESS
                } else {
                    print_runtime_error(
                        CommandName::Status,
                        generated_at,
                        &error.to_string(),
                        json,
                    );
                    ExitCode::from(EXIT_RUNTIME)
                };
            }
            last_output = Some(output);
            sequence += 1;
        }

        thread::sleep(Duration::from_secs(1));
    }
}

pub(super) fn status_watch_frame(status: StatusCommandOutput, sequence: u64) -> WatchFrame {
    WatchFrame::Status {
        contract_version: CONTRACT_VERSION,
        sequence,
        generated_at: status.generated_at.clone(),
        workspace_id: status.workspace_id.clone(),
        project_id: status.project_id.clone(),
        last_event_id: status.event_watermarks.last_event_id.clone(),
        watermark: status.event_watermarks.clone(),
        status: Box::new(status),
    }
}
