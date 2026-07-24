use super::*;
use bowline_core::wire::generated::{
    DaemonStatusEventPayload, DaemonStatusScopeParams, DaemonStatusSnapshotResult,
    DaemonStatusSubscribeResult,
};
use bowline_core::wire::status_command_from_wire;
use bowline_daemon_rpc::{ClientOptions, DaemonClient};
use std::fs;

mod device_facts;
// Re-export at crate visibility so `crate::status_commands::…` callers resolve
// the device-facts helpers exactly as before the split.
pub(crate) use device_facts::{append_status_fact, apply_device_status};
// Test-only helpers exercised directly by unit tests (here and in lib parse
// tests via `use status_commands::*`); gated so non-test builds see no re-export.
#[cfg(test)]
pub(crate) use device_facts::{apply_device_status_for_local_device, device_status_item};

type DeviceTrustSnapshot = bowline_control_plane::DeviceApprovalRequestList;

// Device trust is ambient account state; revocations should surface promptly
// without turning a 1 Hz status watch into a 1 Hz control-plane poll.
const TRUST_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

// The service-supervisor probe shells out to launchd/systemd, so a `status
// --watch` stream must not run it per frame. Its state changes rarely; refresh
// on the same slow cadence as device trust.
const SERVICE_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
struct CachedDeviceTrust {
    workspace_id: WorkspaceId,
    trust: DeviceTrustSnapshot,
}

/// Outcome of a device-trust fetch. `Unavailable` means the control plane could
/// not be reached or errored; it is NOT proof the device lacks trust. Callers
/// must fail closed on it and never raise the authentication rung, so a transient
/// control-plane error can never upgrade an approval-pending device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DeviceTrustFetch {
    Fetched(DeviceTrustSnapshot),
    Unavailable,
}

pub(super) fn print_status(args: StatusArgs, json: bool, socket: &Path) -> ExitCode {
    let generated_at = generated_at();
    let options = StatusOptions {
        db_path: metadata_db_path(),
        requested_path: selected_workspace_path(args.selection),
        workspace_scope: args.include_all,
        generated_at: generated_at.clone(),
    };

    if args.watch {
        return print_status_watch(options, generated_at, json, socket);
    }

    match compose_status_for_cli(options, socket) {
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
    match compose_status_for_cli(options, socket) {
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
    socket: &Path,
) -> Result<StatusCommandOutput, bowline_local::status::LocalStatusError> {
    let requested_path = options.requested_path.clone();
    let workspace_scope = options.workspace_scope;
    let mut output = bowline_local::status::compose_status(options)?;
    let project_scope =
        project_scope_for_output(workspace_scope, requested_path.as_deref(), &output);
    // Fetch device trust once and drive both the human-facing device facts and
    // the machine-readable introspection block from it.
    let trust = fetch_device_trust(output.workspace_id.as_str());
    if let DeviceTrustFetch::Fetched(snapshot) = &trust {
        apply_device_status(&mut output, snapshot);
    }
    attach_update_status_if_available(&mut output, true);
    attach_machine_introspection(&mut output, &trust, socket, project_scope.as_deref());
    abbreviate_status_requested_path(&mut output);
    Ok(output)
}

/// Attach the compact `service`/`authentication`/`sync` introspection contract
/// (contract: status --json) derived from live service, device-trust, and
/// daemon-sync state. These fields exist only on the live CLI surface. The
/// `socket` is the resolved global `--socket`, so daemon reads honor the override
/// every other daemon-touching command uses.
fn attach_machine_introspection(
    output: &mut StatusCommandOutput,
    trust: &DeviceTrustFetch,
    socket: &Path,
    project_scope: Option<&Path>,
) {
    output.service = Some(daemon_service_introspection());
    let state = authentication_state(&output.workspace_id, trust, account_authenticated());
    output.authentication =
        Some(bowline_core::introspection::AuthenticationIntrospection { state });
    let daemon = match project_scope {
        Some(path) => crate::wire::daemon_status_snapshot_for_project(socket, path),
        None => crate::wire::daemon_status_snapshot(socket),
    };
    if let Some(daemon) = daemon {
        overlay_daemon_sync_facts(output, &daemon);
    }
    output.sync = sync_introspection_for(output, socket);
}

/// Overlay the daemon-owned convergence facts when its active workspace matches
/// the locally composed status. Local metadata still owns inventory and path
/// scope; the live manifest engine alone owns queue and readiness truth.
fn overlay_daemon_sync_facts(output: &mut StatusCommandOutput, daemon: &StatusCommandOutput) {
    if output.workspace_id != daemon.workspace_id {
        return;
    }
    bowline_core::status::overlay_convergence_status(output, daemon);
    output.scope.clone_from(&daemon.scope);
    output
        .resolved_project_root
        .clone_from(&daemon.resolved_project_root);
}

fn find_git_project_root(requested_path: &str) -> Option<PathBuf> {
    let expanded = if let Some(relative) = requested_path.strip_prefix("~/") {
        env::var_os("HOME").map(PathBuf::from)?.join(relative)
    } else {
        PathBuf::from(requested_path)
    };
    let mut existing = expanded.as_path();
    while !existing.exists() {
        existing = existing.parent()?;
    }
    let requested = fs::canonicalize(existing).ok()?;
    let mut candidate = if requested.is_dir() {
        requested.as_path()
    } else {
        requested.parent()?
    };
    loop {
        if candidate.join(".git").exists() {
            return Some(candidate.to_path_buf());
        }
        candidate = candidate.parent()?;
    }
}

fn project_scope_for_output(
    workspace_scope: bool,
    requested_path: Option<&str>,
    output: &StatusCommandOutput,
) -> Option<PathBuf> {
    if workspace_scope {
        return None;
    }
    requested_path
        .or(output.requested_path.as_deref())
        .and_then(find_git_project_root)
        .or_else(|| {
            output
                .resolved_project_root
                .as_deref()
                .and_then(expand_status_path)
        })
}

fn expand_status_path(path: &str) -> Option<PathBuf> {
    path.strip_prefix("~/").map_or_else(
        || Some(PathBuf::from(path)),
        |relative| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(relative))
        },
    )
}

/// Reduce account-login and device-trust state onto the authentication ladder.
pub(super) fn authentication_state(
    workspace_id: &WorkspaceId,
    trust: &DeviceTrustFetch,
    authenticated: bool,
) -> bowline_core::introspection::AuthenticationState {
    use bowline_core::introspection::AuthenticationState;
    if !authenticated {
        return AuthenticationState::Unauthenticated;
    }
    let DeviceTrustFetch::Fetched(trust) = trust else {
        // Logged in, but the control plane could not confirm this device's trust.
        // Fail closed: a failed fetch must never raise the rung to Authenticated,
        // or a transient error during `sync wait` polling would upgrade an
        // approval-pending device. Report the lower approval-pending rung so the
        // wait keeps polling and never satisfies an `authenticated`/`ready` target
        // on error; a successful fetch then resolves the true state.
        return AuthenticationState::ApprovalPending;
    };
    // Resolve the local device id only now: `daemon_device_id` reaches the
    // control plane and materializes local metadata, so deferring it until a
    // trust snapshot must actually be reduced keeps the read-only `status` path
    // free of side effects (no metadata database is created just to report).
    let local_device_id = runtime::daemon_device_id(workspace_id);
    reduce_device_trust(trust, &local_device_id)
}

/// Ladder reduction for a known device-trust snapshot against the resolved local
/// device id. Callers guarantee the account is authenticated with known trust;
/// the two trust-independent states are decided in [`authentication_state`].
fn reduce_device_trust(
    trust: &DeviceTrustSnapshot,
    local_device_id: &DeviceId,
) -> bowline_core::introspection::AuthenticationState {
    use bowline_core::introspection::AuthenticationState;
    let local_id = local_device_id.as_str();
    if trust
        .revoked_devices
        .iter()
        .any(|device| device.device_id == local_id)
    {
        return AuthenticationState::Unauthenticated;
    }
    if trust
        .authorized_devices
        .iter()
        .any(|device| device.device_id == local_id)
    {
        return AuthenticationState::Authenticated;
    }
    if trust
        .pending_requests
        .iter()
        .any(|request| request.device_id == local_id)
        || !trust.authorized_devices.is_empty()
    {
        // Either this device has an open request, or other devices are approved
        // and this one is not yet — both are approval-pending.
        return AuthenticationState::ApprovalPending;
    }
    // Account authenticated with no authorized devices yet (first-device setup).
    AuthenticationState::Authenticated
}

/// Whether a usable account session exists locally, without triggering an
/// interactive secret-store prompt.
pub(super) fn account_authenticated() -> bool {
    account_context_available_from_sources(
        runtime::persisted_account_session_revocation()
            .ok()
            .flatten()
            .is_some(),
        runtime::passive_secret_store_probe_allowed(),
        || {
            let Ok(key_store) = runtime::key_store() else {
                return false;
            };
            runtime::account_session_id(&*key_store).is_some()
                || matches!(key_store.load_account_tokens(), Ok(Some(_)))
        },
    )
}

fn account_context_available_from_sources(
    persisted_session: bool,
    passive_secret_store_probe_allowed: bool,
    load_local_session: impl FnOnce() -> bool,
) -> bool {
    persisted_session || (passive_secret_store_probe_allowed && load_local_session())
}

/// A slow refresh timer for an ambient input a watch loop re-reads periodically
/// (device trust, service state) rather than on every frame. Starts due so the
/// first tick always primes the cache.
#[derive(Debug)]
struct RefreshCadence {
    next_due: Instant,
    interval: Duration,
}

impl RefreshCadence {
    fn new(now: Instant, interval: Duration) -> Self {
        Self {
            next_due: now,
            interval,
        }
    }

    fn due(&self, now: Instant) -> bool {
        now >= self.next_due
    }

    fn record_attempt(&mut self, now: Instant) {
        self.next_due = now + self.interval;
    }
}

/// Caches the cost-heavy service-supervisor probe so a `status --watch` stream
/// can enrich every emitted frame with the machine-introspection block without
/// shelling out to launchd/systemd per frame. Device trust is supplied by the
/// caller (each watch path already caches it on its own cadence); sync activity
/// is derived fresh from each frame's own queue so live changes are never masked.
#[derive(Debug)]
struct WatchIntrospection {
    socket: PathBuf,
    service: Option<bowline_core::introspection::ServiceIntrospection>,
    service_refresh: RefreshCadence,
}

impl WatchIntrospection {
    fn new(socket: PathBuf, now: Instant) -> Self {
        Self {
            socket,
            service: None,
            service_refresh: RefreshCadence::new(now, SERVICE_REFRESH_INTERVAL),
        }
    }

    fn attach(&mut self, output: &mut StatusCommandOutput, trust: &DeviceTrustFetch, now: Instant) {
        if self.service.is_none() || self.service_refresh.due(now) {
            self.service = Some(daemon_service_introspection());
            self.service_refresh.record_attempt(now);
        }
        output.service = self.service.clone();
        output.authentication = Some(bowline_core::introspection::AuthenticationIntrospection {
            state: authentication_state(&output.workspace_id, trust, account_authenticated()),
        });
        output.sync = sync_introspection_for(output, &self.socket);
    }
}

/// Map a watch loop's cached trust snapshot onto the fail-closed fetch outcome
/// the authentication ladder consumes. A cache miss (never fetched, or the last
/// fetch failed) is `Unavailable`, so authentication never claims more than the
/// last confirmed trust state.
fn device_trust_fetch_from_cache(
    cache: &Option<CachedDeviceTrust>,
    workspace_id: &WorkspaceId,
) -> DeviceTrustFetch {
    match cached_device_trust_for_workspace(cache, workspace_id) {
        Some(snapshot) => DeviceTrustFetch::Fetched(snapshot.clone()),
        None => DeviceTrustFetch::Unavailable,
    }
}

/// Resolve the compact sync view: prefer the locally composed queue, and fall
/// back to the daemon's live snapshot over the resolved socket so `sync` is
/// present whenever a daemon is running, not only when the local metadata path
/// happens to carry a queue.
fn sync_introspection_for(
    output: &StatusCommandOutput,
    socket: &Path,
) -> Option<bowline_core::introspection::SyncIntrospection> {
    output
        .sync_queue
        .as_ref()
        .map(bowline_core::introspection::SyncIntrospection::from_queue)
        .or_else(|| crate::wire::daemon_sync_introspection(socket))
}

fn update_cached_device_trust(
    cached_trust: &mut Option<CachedDeviceTrust>,
    workspace_id: &WorkspaceId,
    fetched_trust: DeviceTrustFetch,
) {
    // Only a confirmed fetch overwrites the cache; an `Unavailable` result leaves
    // the last known trust in place rather than clobbering it with a failure.
    if let DeviceTrustFetch::Fetched(trust) = fetched_trust {
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

pub(super) fn fetch_device_trust(workspace_id: &str) -> DeviceTrustFetch {
    if !account_context_available_from_sources(
        runtime::persisted_account_session_revocation()
            .ok()
            .flatten()
            .is_some(),
        runtime::passive_secret_store_probe_allowed(),
        || {
            runtime::key_store()
                .ok()
                .is_some_and(|key_store| matches!(key_store.load_account_tokens(), Ok(Some(_))))
        },
    ) {
        return DeviceTrustFetch::Unavailable;
    }
    let Ok(control_plane) = runtime::control_plane() else {
        return DeviceTrustFetch::Unavailable;
    };
    match control_plane.list_device_trust(&bowline_core::ids::WorkspaceId::new(workspace_id)) {
        Ok(trust) => DeviceTrustFetch::Fetched(trust),
        Err(_) => DeviceTrustFetch::Unavailable,
    }
}

pub(super) fn print_status_watch(
    options: StatusOptions,
    started_at: String,
    json: bool,
    socket: &Path,
) -> ExitCode {
    let pres = surface::style::Presentation::detect(json);
    if env::var_os(ENV_METADATA_DB).is_none()
        && let Ok(exit_code) = print_daemon_status_watch(&options, &pres, &started_at, json, socket)
    {
        return exit_code;
    }
    let mut state = StatusWatchState::new(socket.to_path_buf());

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
    socket: &Path,
) -> Result<ExitCode, ()> {
    let client =
        DaemonClient::connect(socket, ClientOptions::new("cli", CLI_VERSION)).map_err(|_| ())?;
    let local_status = bowline_local::status::compose_status(options.clone()).map_err(|_| ())?;
    let project_scope = project_scope_for_output(
        options.workspace_scope,
        options.requested_path.as_deref(),
        &local_status,
    );
    let scope = daemon_status_scope_params(options, project_scope.as_deref());
    let subscription: DaemonStatusSubscribeResult = client
        .call("status.subscribe", &scope, Some(Duration::from_secs(2)))
        .map_err(|_| ())?;
    // The daemon's snapshot carries no CLI-surface introspection, so enrich every
    // frame with the same service/authentication/sync block as one-shot
    // `status --json`. The enricher caches the service probe and device trust on a
    // slow cadence so an event-driven stream never pays them per frame.
    let mut enricher = DaemonWatchEnricher::new(socket.to_path_buf());
    let mut initial = status_command_from_wire(subscription.snapshot).map_err(|_| ())?;
    enricher.enrich(&mut initial);
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
        let sequence = event.sequence;
        let snapshot = if payload.gap || payload.resync_required {
            let replacement: DaemonStatusSnapshotResult = client
                .call("status.getSnapshot", &scope, Some(Duration::from_secs(2)))
                .map_err(|_| ())?;
            replacement.snapshot
        } else if let Some(snapshot) = payload.snapshot {
            snapshot
        } else {
            continue;
        };
        let mut status = status_command_from_wire(snapshot).map_err(|_| ())?;
        enricher.enrich(&mut status);
        let frame = status_watch_frame(status, sequence);
        if let Err(exit_code) = write_watch_frame_or_exit(&frame, json, pres, started_at) {
            return Ok(exit_code);
        }
    }
}

fn daemon_status_scope_params(
    options: &StatusOptions,
    project_scope: Option<&Path>,
) -> DaemonStatusScopeParams {
    DaemonStatusScopeParams {
        workspace_root: project_scope
            .is_none()
            .then(|| options.requested_path.clone())
            .flatten(),
        project_path: project_scope.map(|path| path.to_string_lossy().into_owned()),
        requested_path: project_scope
            .is_some()
            .then(|| options.requested_path.clone())
            .flatten(),
    }
}

/// Enriches daemon-sourced watch frames with the machine-introspection block.
/// Owns the service cache plus a device-trust cache refreshed on the trust
/// cadence, so the event stream stays cheap.
struct DaemonWatchEnricher {
    introspection: WatchIntrospection,
    trust: Option<CachedDeviceTrust>,
    trust_refresh: RefreshCadence,
}

impl DaemonWatchEnricher {
    fn new(socket: PathBuf) -> Self {
        let now = Instant::now();
        Self {
            introspection: WatchIntrospection::new(socket, now),
            trust: None,
            trust_refresh: RefreshCadence::new(now, TRUST_REFRESH_INTERVAL),
        }
    }

    fn enrich(&mut self, output: &mut StatusCommandOutput) {
        let now = Instant::now();
        if self.trust_refresh.due(now) {
            update_cached_device_trust(
                &mut self.trust,
                &output.workspace_id,
                fetch_device_trust(output.workspace_id.as_str()),
            );
            self.trust_refresh.record_attempt(now);
        }
        let trust = device_trust_fetch_from_cache(&self.trust, &output.workspace_id);
        self.introspection.attach(output, &trust, now);
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
    trust_refresh: RefreshCadence,
    composer: Option<bowline_local::status::RevisionedStatusComposer>,
    update_revision: Option<update::UpdateStatusRevision>,
    introspection: WatchIntrospection,
}

impl StatusWatchState {
    fn new(socket: PathBuf) -> Self {
        let now = Instant::now();
        Self {
            sequence: 1,
            last_output: None,
            backoff: Duration::from_secs(1),
            cached_trust: None,
            trust_refresh: RefreshCadence::new(now, TRUST_REFRESH_INTERVAL),
            composer: None,
            update_revision: None,
            introspection: WatchIntrospection::new(socket, now),
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
    // Carry the same service/authentication/sync introspection as one-shot
    // `status --json`. Trust is already refreshed above (reused, not re-fetched);
    // the enricher caches the service probe on its own cadence, so this runs only
    // on changed frames without a per-frame supervisor shell-out.
    let trust = device_trust_fetch_from_cache(&state.cached_trust, &output.workspace_id);
    state.introspection.attach(&mut output, &trust, now);
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
