use super::*;
use bowline_core::devices::display_matching_code;
use bowline_core::wire::generated::{
    DaemonStatusEventPayload, DaemonStatusScopeParams, DaemonStatusSnapshotResult,
    DaemonStatusSubscribeResult,
};
use bowline_core::wire::status_command_from_wire;
use bowline_daemon_rpc::{ClientOptions, DaemonClient};

type DeviceTrustSnapshot = bowline_control_plane::DeviceApprovalRequestList;

// Device trust is ambient account state; revocations should surface promptly
// without turning a 1 Hz status watch into a 1 Hz control-plane poll.
const TRUST_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
struct CachedDeviceTrust {
    workspace_id: WorkspaceId,
    trust: DeviceTrustSnapshot,
}

pub(super) fn print_status(args: StatusArgs, json: bool) -> ExitCode {
    let generated_at = generated_at();
    let options = StatusOptions {
        db_path: metadata_db_path(),
        requested_path: selected_workspace_path(args.selection),
        workspace_scope: args.include_all,
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
            let pres = surface::style::Presentation::detect(false);
            let human = surface::human::render_status(&output, &pres);
            write_human_or_exit(CommandName::Status, generated_at, &human)
        }
        Err(error) => {
            print_runtime_error(CommandName::Status, generated_at, &error.to_string(), json);
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
                message: "bowline tui is an interactive command; use `bowline status --root <path> --json`"
                    .to_string(),
                next_actions: vec![RepairCommand::inspect(
                    "Inspect status as JSON".to_string(),
                    Some(format!(
                        "bowline status --root {} --json",
                        io_helpers::shell_word(&args.selection.root)
                    )),
                )],
            },
            generated_at,
            true,
        );
        return ExitCode::from(EXIT_USAGE);
    }
    let options = StatusOptions {
        db_path: metadata_db_path(),
        requested_path: selected_workspace_path(args.selection),
        workspace_scope: false,
        generated_at: generated_at.clone(),
    };
    match compose_status_for_cli(options) {
        Ok(output) if !io::stdin().is_terminal() || !io::stdout().is_terminal() => {
            let pres = surface::style::Presentation::detect(false);
            let human = surface::human::render_status(&output, &pres);
            write_human_or_exit(CommandName::Tui, generated_at, &human)
        }
        Ok(output) => {
            let verdict = surface::style::Verdict::from_output(&output);
            let model = surface::tui::TuiModel::from_status(&output).with_verdict(verdict);
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
    attach_update_status_if_available(&mut output, true);
    abbreviate_status_requested_path(&mut output);
    Ok(output)
}

#[derive(Debug)]
struct TrustRefreshSchedule {
    next_fetch: Instant,
}

impl TrustRefreshSchedule {
    fn new(now: Instant) -> Self {
        Self { next_fetch: now }
    }

    fn due(&self, now: Instant) -> bool {
        now >= self.next_fetch
    }

    fn record_attempt(&mut self, now: Instant) {
        self.next_fetch = now + TRUST_REFRESH_INTERVAL;
    }
}

fn update_cached_device_trust(
    cached_trust: &mut Option<CachedDeviceTrust>,
    workspace_id: &WorkspaceId,
    fetched_trust: Option<DeviceTrustSnapshot>,
) {
    if let Some(trust) = fetched_trust {
        *cached_trust = Some(CachedDeviceTrust {
            workspace_id: workspace_id.clone(),
            trust,
        });
    }
}

fn cached_device_trust_for_workspace<'a>(
    cached_trust: &'a Option<CachedDeviceTrust>,
    workspace_id: &WorkspaceId,
) -> Option<&'a DeviceTrustSnapshot> {
    cached_trust
        .as_ref()
        .filter(|cached| cached.workspace_id == *workspace_id)
        .map(|cached| &cached.trust)
}

pub(super) fn attach_device_status_if_available(output: &mut StatusCommandOutput) {
    if let Some(trust) = fetch_device_trust(output.workspace_id.as_str()) {
        apply_device_status(output, &trust);
    }
}

pub(super) fn fetch_device_trust(workspace_id: &str) -> Option<DeviceTrustSnapshot> {
    if !runtime::passive_secret_store_probe_allowed() {
        return None;
    }

    let Ok(key_store) = runtime::key_store() else {
        return None;
    };
    if !matches!(key_store.load_account_tokens(), Ok(Some(_))) {
        return None;
    }
    let Ok(control_plane) = runtime::control_plane() else {
        return None;
    };
    control_plane
        .list_device_trust(&bowline_core::ids::WorkspaceId::new(workspace_id))
        .ok()
}

pub(super) fn apply_device_status(output: &mut StatusCommandOutput, trust: &DeviceTrustSnapshot) {
    let local_device_id = runtime::daemon_device_id(&output.workspace_id);
    apply_device_status_for_local_device(output, trust, &local_device_id);
}

fn apply_device_status_for_local_device(
    output: &mut StatusCommandOutput,
    trust: &DeviceTrustSnapshot,
    local_device_id: &DeviceId,
) {
    let local_id = local_device_id.as_str();
    if let Some(revoked) = trust
        .revoked_devices
        .iter()
        .find(|device| device.device_id == local_id)
    {
        append_status_fact(
            output,
            "device.revoked",
            format!("device-revoked:{local_id}"),
            format!("device-trust:{local_id}"),
            StatusFactScope::Device,
            Some(local_id),
            None,
        );
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
        output.next_actions.push(RepairCommand::inspect(
            "Inspect workspace status".to_string(),
            Some(status_command(output, &[])),
        ));
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
        append_status_fact(
            output,
            "device.untrusted",
            format!("device-pending:{local_id}"),
            format!("device-trust:{local_id}"),
            StatusFactScope::Device,
            Some(local_id),
            None,
        );
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
        append_status_fact(
            output,
            "device.untrusted",
            format!("device-untrusted:{local_id}"),
            format!("device-trust:{local_id}"),
            StatusFactScope::Device,
            Some(local_id),
            None,
        );
        output
            .status
            .attention_items
            .push("This device is not trusted for the workspace yet.".to_string());
        let setup_command = format!(
            "bowline setup{}",
            io_helpers::root_flag(output.resolved_workspace_root.as_deref())
        );
        let item = device_status_item(
            output,
            StatusSubjectKind::Device,
            local_device_id.as_str(),
            Some(local_device_id.clone()),
            format!("Run `{setup_command}` to request workspace trust."),
        );
        output.items.push(item);
    }

    if !trust.pending_requests.is_empty() {
        for request in &trust.pending_requests {
            append_status_fact(
                output,
                "device.approval_requested",
                format!("device-approval:{}", request.request_id.as_str()),
                format!("device-approval:{}", request.request_id.as_str()),
                StatusFactScope::Device,
                Some(request.device_id.as_str()),
                Some(request.request_id.as_str()),
            );
        }
        output.status.attention_items.push(format!(
            "{} device approval request(s) are waiting.",
            trust.pending_requests.len()
        ));
        // Trusted local surface: the concrete approve affordance (matching code +
        // `bowline device approve --code …`) is local trust material. It rides on
        // `device_approvals`, correlated to its status item by `request_id`, and
        // must never be written to hosted/persisted status payloads.
        let pending_items = trust
            .pending_requests
            .iter()
            .map(|request| {
                let display_code = display_matching_code(&request.matching_code);
                output.device_approvals.push(DeviceApprovalAffordance {
                    request_id: request.request_id.as_str().to_string(),
                    device_name: request.device_name.clone(),
                    code: display_code.clone(),
                    approve_command: approve_command(
                        output,
                        io_helpers::shell_word(display_code.as_str()),
                    ),
                });
                device_status_item(
                    output,
                    StatusSubjectKind::DeviceApprovalRequest,
                    request.request_id.as_str(),
                    Some(DeviceId::new(request.device_id.clone())),
                    format!(
                        "{} is waiting for approval with matching code {}.",
                        request.device_name, display_code
                    ),
                )
            })
            .collect::<Vec<_>>();
        output.items.extend(pending_items);
        output.next_actions.push(RepairCommand::inspect(
            "Review workspace status".to_string(),
            Some(status_command(output, &[])),
        ));
    }
}

pub(super) fn append_status_fact(
    output: &mut StatusCommandOutput,
    kind: &str,
    id: impl Into<String>,
    dedupe_key: impl Into<String>,
    scope: StatusFactScope,
    scope_id: Option<&str>,
    action_target_id: Option<&str>,
) {
    let policy = status_fact_policy(kind);
    let mut fact = StatusFact::new(
        id,
        kind,
        policy.authority,
        scope,
        output.generated_at.clone(),
        dedupe_key,
    );
    if let Some(scope_id) = scope_id {
        fact = fact.with_scope_id(scope_id);
    }
    if let (Some(action), Some(target_id)) = (fact.action.as_mut(), action_target_id) {
        action.target_id = Some(target_id.to_string());
    }
    let mut facts = std::mem::take(&mut output.status_summary.facts);
    facts.push(fact);
    let summary = reduce_status_facts(facts, 1, output.generated_at.clone());
    output.status.level = summary.presentation_level();
    output.status_summary = summary;
}

fn status_command(output: &StatusCommandOutput, extra: &[&str]) -> String {
    let mut command = format!(
        "bowline status{}",
        io_helpers::root_flag(output.resolved_workspace_root.as_deref())
    );
    for arg in extra {
        command.push(' ');
        command.push_str(arg);
    }
    command
}

fn approve_command(output: &StatusCommandOutput, code: String) -> String {
    format!(
        "bowline device approve{} --code {code}",
        io_helpers::root_flag(output.resolved_workspace_root.as_deref())
    )
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
    started_at: String,
    json: bool,
) -> ExitCode {
    let pres = surface::style::Presentation::detect(json);
    if env::var_os(ENV_METADATA_DB).is_none()
        && let Ok(exit_code) = print_daemon_status_watch(&options, &pres, &started_at, json)
    {
        return exit_code;
    }
    let mut state = StatusWatchState::new();

    loop {
        match next_status_watch_tick(&mut state, &options) {
            WatchTick::Frame(frame) => {
                if let Err(exit_code) = write_watch_frame_or_exit(&frame, json, &pres, &started_at)
                {
                    return exit_code;
                }
                thread::sleep(Duration::from_secs(1));
            }
            WatchTick::Unchanged => {
                thread::sleep(Duration::from_secs(1));
            }
            WatchTick::RecoverableError { frame, backoff } => {
                if let Err(exit_code) = write_watch_frame_or_exit(&frame, json, &pres, &started_at)
                {
                    return exit_code;
                }
                thread::sleep(backoff);
            }
            WatchTick::Fatal(error) => {
                print_runtime_error(CommandName::Status, started_at, &error.to_string(), json);
                return ExitCode::from(EXIT_RUNTIME);
            }
        }
    }
}

fn print_daemon_status_watch(
    options: &StatusOptions,
    pres: &surface::style::Presentation,
    started_at: &str,
    json: bool,
) -> Result<ExitCode, ()> {
    let socket = default_control_socket_path().map_err(|_| ())?;
    let client =
        DaemonClient::connect(&socket, ClientOptions::new("cli", CLI_VERSION)).map_err(|_| ())?;
    let scope = DaemonStatusScopeParams {
        workspace_root: options.requested_path.clone(),
        project_path: None,
    };
    let subscription: DaemonStatusSubscribeResult = client
        .call("status.subscribe", &scope, Some(Duration::from_secs(2)))
        .map_err(|_| ())?;
    let initial = status_command_from_wire(subscription.snapshot).map_err(|_| ())?;
    let frame = status_watch_frame(initial, subscription.sequence);
    if let Err(exit_code) = write_watch_frame_or_exit(&frame, json, pres, started_at) {
        return Ok(exit_code);
    }
    let events = client
        .register_events(subscription.subscription_id, 1)
        .map_err(|_| ())?;
    loop {
        let event = events.recv().map_err(|_| ())?;
        let payload: DaemonStatusEventPayload =
            serde_json::from_value(event.payload).map_err(|_| ())?;
        if payload.gap || payload.resync_required {
            let replacement: DaemonStatusSnapshotResult = client
                .call("status.getSnapshot", &scope, Some(Duration::from_secs(2)))
                .map_err(|_| ())?;
            let status = status_command_from_wire(replacement.snapshot).map_err(|_| ())?;
            let frame = status_watch_frame(status, event.sequence);
            if let Err(exit_code) = write_watch_frame_or_exit(&frame, json, pres, started_at) {
                return Ok(exit_code);
            }
            continue;
        }
        let Some(snapshot) = payload.snapshot else {
            continue;
        };
        let status = status_command_from_wire(snapshot).map_err(|_| ())?;
        let frame = status_watch_frame(status, event.sequence);
        if let Err(exit_code) = write_watch_frame_or_exit(&frame, json, pres, started_at) {
            return Ok(exit_code);
        }
    }
}

fn write_watch_frame_or_exit(
    frame: &WatchFrame,
    json: bool,
    pres: &surface::style::Presentation,
    started_at: &str,
) -> Result<(), ExitCode> {
    write_watch_frame(frame, json, pres).map_err(|error| {
        if error.kind() == io::ErrorKind::BrokenPipe {
            return ExitCode::SUCCESS;
        }
        print_runtime_error(
            CommandName::Status,
            started_at.to_string(),
            &error.to_string(),
            json,
        );
        ExitCode::from(EXIT_RUNTIME)
    })
}

#[derive(Debug)]
enum WatchTick {
    Frame(WatchFrame),
    Unchanged,
    RecoverableError {
        frame: WatchFrame,
        backoff: Duration,
    },
    Fatal(bowline_local::status::LocalStatusError),
}

#[derive(Debug)]
struct StatusWatchState {
    sequence: u64,
    last_output: Option<StatusCommandOutput>,
    backoff: Duration,
    cached_trust: Option<CachedDeviceTrust>,
    trust_refresh: TrustRefreshSchedule,
    composer: Option<bowline_local::status::RevisionedStatusComposer>,
    update_revision: Option<update::UpdateStatusRevision>,
}

impl StatusWatchState {
    fn new() -> Self {
        Self {
            sequence: 1,
            last_output: None,
            backoff: Duration::from_secs(1),
            cached_trust: None,
            trust_refresh: TrustRefreshSchedule::new(Instant::now()),
            composer: None,
            update_revision: None,
        }
    }
}

fn next_status_watch_tick(state: &mut StatusWatchState, options: &StatusOptions) -> WatchTick {
    match compose_status_watch_output(options.clone(), state) {
        Ok(Some(output)) => next_status_watch_tick_from_result(state, Ok(output)),
        Ok(None) => WatchTick::Unchanged,
        Err(error) => next_status_watch_tick_from_result(state, Err(error)),
    }
}

#[cfg(test)]
fn next_status_watch_tick_with(
    state: &mut StatusWatchState,
    compose: &mut impl FnMut() -> Result<StatusCommandOutput, bowline_local::status::LocalStatusError>,
) -> WatchTick {
    next_status_watch_tick_from_result(state, compose())
}

fn next_status_watch_tick_from_result(
    state: &mut StatusWatchState,
    result: Result<StatusCommandOutput, bowline_local::status::LocalStatusError>,
) -> WatchTick {
    match result {
        Ok(output) => {
            state.backoff = Duration::from_secs(1);
            if state.last_output.as_ref() == Some(&output) {
                return WatchTick::Unchanged;
            }
            let frame = status_watch_frame(output.clone(), state.sequence);
            state.last_output = Some(output);
            state.sequence += 1;
            WatchTick::Frame(frame)
        }
        Err(error) if error.is_recoverable() => {
            let frame = status_watch_error_frame(&error, state);
            state.last_output = None;
            state.sequence += 1;
            let backoff = state.backoff;
            state.backoff = (state.backoff * 2).min(Duration::from_secs(5));
            WatchTick::RecoverableError { frame, backoff }
        }
        Err(error) => WatchTick::Fatal(error),
    }
}

fn compose_status_watch_output(
    options: StatusOptions,
    state: &mut StatusWatchState,
) -> Result<Option<StatusCommandOutput>, bowline_local::status::LocalStatusError> {
    let now = Instant::now();
    if state.trust_refresh.due(now)
        && let Some(previous) = state.last_output.as_ref()
    {
        let old_trust = state.cached_trust.clone();
        update_cached_device_trust(
            &mut state.cached_trust,
            &previous.workspace_id,
            fetch_device_trust(previous.workspace_id.as_str()),
        );
        state.trust_refresh.record_attempt(now);
        if state.cached_trust != old_trust
            && let Some(composer) = state.composer.as_mut()
        {
            composer.mark_trust_dirty();
        }
    }
    refresh_update_status_revision(state, update::update_status_revision());
    let composer = match state.composer.as_mut() {
        Some(composer) => composer,
        None => state
            .composer
            .insert(bowline_local::status::RevisionedStatusComposer::new(
                options.db_path.clone(),
            )?),
    };
    let mut output = match composer.compose_if_needed(options, now)? {
        bowline_local::status::RevisionedStatus::Composed(output) => *output,
        bowline_local::status::RevisionedStatus::Unchanged => return Ok(None),
    };
    if state.trust_refresh.due(now) {
        update_cached_device_trust(
            &mut state.cached_trust,
            &output.workspace_id,
            fetch_device_trust(output.workspace_id.as_str()),
        );
        state.trust_refresh.record_attempt(now);
    }
    if let Some(trust) =
        cached_device_trust_for_workspace(&state.cached_trust, &output.workspace_id)
    {
        // FUTURE: when the daemon exposes device-trust state over its local
        // socket, watch should read that local state and this polling cache can
        // disappear.
        apply_device_status(&mut output, trust);
    }
    attach_update_status_if_available(&mut output, false);
    abbreviate_status_requested_path(&mut output);
    Ok(Some(output))
}

fn refresh_update_status_revision(
    state: &mut StatusWatchState,
    revision: update::UpdateStatusRevision,
) {
    if state
        .update_revision
        .as_ref()
        .is_some_and(|previous| previous != &revision)
        && let Some(composer) = state.composer.as_mut()
    {
        composer.mark_update_dirty();
    }
    state.update_revision = Some(revision);
}

fn write_watch_frame(
    frame: &WatchFrame,
    json: bool,
    pres: &surface::style::Presentation,
) -> io::Result<()> {
    if json {
        write_json_line(frame)
    } else {
        // Fresh emit-time clock per frame; the composed timestamp is frozen for
        // change-detection, so it must not drive display.
        let display_at = generated_at();
        write_text(&surface::human::render_watch_frame(
            frame,
            &display_at,
            pres,
        ))
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

fn status_watch_error_frame(
    error: &bowline_local::status::LocalStatusError,
    state: &StatusWatchState,
) -> WatchFrame {
    let generated_at = generated_at();
    let workspace_id = state
        .last_output
        .as_ref()
        .map(|output| output.workspace_id.clone())
        .unwrap_or_else(|| WorkspaceId::new("ws_local_uninitialized"));
    let mut error = bowline_local::status::command_error_output(
        CommandName::Status,
        generated_at.clone(),
        "runtime_error",
        error.to_string(),
        CommandRecoverability::Retry,
    );
    error.error.remediation = Some("status watch will retry automatically.".to_string());
    error.error.retry_after_seconds = Some(state.backoff.as_secs());
    WatchFrame::Error {
        contract_version: CONTRACT_VERSION,
        sequence: state.sequence,
        generated_at,
        workspace_id,
        error,
    }
}

#[cfg(test)]
mod tests;
