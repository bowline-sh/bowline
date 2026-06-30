#![deny(unsafe_code)]

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use bowline_control_plane::{
    ControlPlaneClient, ControlPlaneError, HostedControlPlaneClient, SignedUrlByteStore,
    WorkspaceRef, hosted_function_call_counts,
};
use bowline_core::{
    commands::{AgentToolInvokeRequest, AgentToolTransport, CONTRACT_VERSION},
    events::{
        EventName, EventRedaction, EventSeverity, EventSubject, EventSubjectKind, WorkspaceEvent,
    },
    hosted::{DEFAULT_CONVEX_URL, DEFAULT_WORKOS_CLIENT_ID},
    ids::{DeviceId, EventId, WorkspaceId},
    policy::{MaterializationMode, PathClassification},
    workspace_graph::normalize_workspace_path,
};
use bowline_local::{
    account::workos,
    agents::invoke_agent_tool_from_local_daemon,
    device_keys::{DeviceKeyStore, KeyringDeviceKeyStore, ServerLocalSecretStore},
    metadata::{
        DEFAULT_DATABASE_FILE, LocalWriteLogRecord, MetadataStore, RemoteRefCursorRecord,
        SyncOperationCounts, SyncOperationRecord,
    },
    notifications::{
        DesktopNotificationSender, NotificationDedupe, NotificationDispatchReport,
        NotificationSender, dispatch_new_notifications, pending_device_payloads,
    },
    policy::{PathFacts, UserPolicy, classify_path},
    status::StatusOptions,
    sync::{SyncRunner, SyncRunnerOptions, SyncTickOutcome, UploadOutcome},
    trust::grants,
};
use bowline_storage::{ByteStore, StorageKey};
use notify::{
    Event, RecommendedWatcher, RecursiveMode, Watcher,
    event::{EventKind, ModifyKind, RemoveKind},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uds::UnixStreamExt;

const PHASE: &str = "0D";
const PROTOCOL: &str = "bowline.local";
const PROTOCOL_VERSION: u32 = 1;
const DEFAULT_SOCKET: &str = "/tmp/bowline-daemon.sock";
const ENV_METADATA_DB: &str = "BOWLINE_METADATA_DB";
const EXIT_USAGE: u8 = 2;
const EXIT_FAILURE: u8 = 1;
const WATCHER_SETTLE_WINDOW: Duration = Duration::from_millis(250);
const DEFAULT_SYNC_INTERVAL: Duration = Duration::from_secs(600);
const NOTIFICATION_POLL_INTERVAL: Duration = Duration::from_secs(30);
const STATUS_PUBLISH_INTERVAL: Duration = Duration::from_secs(60);
const REMOTE_OBSERVER_DRAIN_INTERVAL: Duration = Duration::from_secs(1);
const REMOTE_OBSERVER_RECONNECT_INITIAL: Duration = Duration::from_secs(30);
const REMOTE_OBSERVER_RECONNECT_MAX: Duration = Duration::from_secs(900);
const SYNC_CLAIM_TIMEOUT_SECONDS: i64 = 60;
const SYNC_RETRY_INITIAL_SECONDS: i64 = 2;
const SYNC_RETRY_MAX_SECONDS: i64 = 60;
const SYNC_RETRY_JITTER_SECONDS: i64 = 3;
const WATCHER_DRAIN_BUDGET: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Cli {
    json: bool,
    socket: PathBuf,
    continuous_sync: Option<ContinuousSyncOptions>,
    notify_approvals: bool,
    command: Command,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    Help,
    Serve { once: bool },
    SyncOnce(SyncOnceArgs),
    Stop,
    Status,
    Version,
    UsageError(String),
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SyncOnceArgs {
    root: PathBuf,
    state_root: PathBuf,
    workspace_id: String,
    device_id: String,
    sync_operation_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ContinuousSyncOptions {
    args: SyncOnceArgs,
    interval: Duration,
    max_ticks: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Handshake {
    daemon_version: String,
    sync_json: Option<String>,
}

struct DaemonRuntime {
    sync: Option<ContinuousSyncRuntime>,
    notify_approvals: bool,
    notification_dedupe: NotificationDedupe,
    next_notification_poll: Instant,
}

struct ContinuousSyncRuntime {
    options: ContinuousSyncOptions,
    next_tick: Instant,
    next_remote_observe: Instant,
    tick_count: u64,
    last_json: String,
    watcher: Option<RecommendedWatcher>,
    change_rx: Option<Receiver<WatcherSignal>>,
    watcher_state: WatcherRuntimeState,
    sync_once: SyncExecutor,
    remote_ref_observer: RemoteRefObserver,
    latest_observed_ref: Option<WorkspaceRef>,
    status_publisher: StatusPublisher,
    next_status_publish: Instant,
}

type SyncExecutor = Box<
    dyn FnMut(
            SyncOnceArgs,
            Option<WorkspaceRef>,
        ) -> Result<SyncOnceSummary, Box<dyn std::error::Error>>
        + 'static,
>;
type RemoteRefObserver = Box<
    dyn FnMut(SyncOnceArgs) -> Result<Option<WorkspaceRef>, Box<dyn std::error::Error>> + 'static,
>;
type StatusPublisher =
    Box<dyn FnMut(StatusPublishRequest) -> Result<(), Box<dyn std::error::Error>> + 'static>;

/// Inputs for one redacted status publish. The daemon attaches its live
/// in-memory component states; `None` lets the composed snapshot's
/// store-derived state stand.
#[derive(Debug, Clone)]
struct StatusPublishRequest {
    args: SyncOnceArgs,
    sync_state: Option<String>,
    watcher_state: Option<String>,
    network_state: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WatcherRuntimeState {
    Ready,
    Limited(String),
}

#[derive(Debug)]
enum WatcherSignal {
    Changed(Event),
    Limited(String),
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct WatcherDrain {
    changed: bool,
    sync_now: bool,
}

struct SocketGuard {
    path: PathBuf,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn main() -> ExitCode {
    install_panic_hook();
    let cli = parse_args(env::args().skip(1));
    run(cli)
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|_| {
        eprintln!(
            "bowline-daemon hit an internal error. Run `bowline daemon status`; environment values were not printed."
        );
    }));
}

fn parse_args<I, S>(args: I) -> Cli
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut json = false;
    let mut socket = PathBuf::from(DEFAULT_SOCKET);
    let mut once = false;
    let mut sync_root = None;
    let mut sync_state_root = None;
    let mut sync_workspace_id = "ws_code".to_string();
    let mut sync_device_id = "device-daemon".to_string();
    let mut sync_interval = DEFAULT_SYNC_INTERVAL;
    let mut sync_max_ticks = None;
    let mut notify_approvals = false;
    let mut positionals = Vec::new();
    let mut iter = args.into_iter().map(Into::into);

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--once" => once = true,
            "--notify-approvals" => notify_approvals = true,
            "--socket" => match iter.next() {
                Some(path) => socket = PathBuf::from(path),
                None => {
                    return Cli {
                        json,
                        socket,
                        continuous_sync: None,
                        notify_approvals,
                        command: Command::UsageError("missing value for --socket".to_string()),
                    };
                }
            },
            "--sync-root" => match iter.next() {
                Some(path) => sync_root = Some(PathBuf::from(path)),
                None => {
                    return Cli {
                        json,
                        socket,
                        continuous_sync: None,
                        notify_approvals,
                        command: Command::UsageError("missing value for --sync-root".to_string()),
                    };
                }
            },
            "--sync-state-root" => match iter.next() {
                Some(path) => sync_state_root = Some(PathBuf::from(path)),
                None => {
                    return Cli {
                        json,
                        socket,
                        continuous_sync: None,
                        notify_approvals,
                        command: Command::UsageError(
                            "missing value for --sync-state-root".to_string(),
                        ),
                    };
                }
            },
            "--sync-workspace" => match iter.next() {
                Some(value) => sync_workspace_id = value,
                None => {
                    return Cli {
                        json,
                        socket,
                        continuous_sync: None,
                        notify_approvals,
                        command: Command::UsageError(
                            "missing value for --sync-workspace".to_string(),
                        ),
                    };
                }
            },
            "--sync-device" => match iter.next() {
                Some(value) => sync_device_id = value,
                None => {
                    return Cli {
                        json,
                        socket,
                        continuous_sync: None,
                        notify_approvals,
                        command: Command::UsageError("missing value for --sync-device".to_string()),
                    };
                }
            },
            "--sync-interval-ms" => match iter.next() {
                Some(value) => match value.parse::<u64>() {
                    Ok(ms) if ms > 0 => sync_interval = Duration::from_millis(ms),
                    _ => {
                        return Cli {
                            json,
                            socket,
                            continuous_sync: None,
                            notify_approvals,
                            command: Command::UsageError(
                                "--sync-interval-ms must be a positive integer".to_string(),
                            ),
                        };
                    }
                },
                None => {
                    return Cli {
                        json,
                        socket,
                        continuous_sync: None,
                        notify_approvals,
                        command: Command::UsageError(
                            "missing value for --sync-interval-ms".to_string(),
                        ),
                    };
                }
            },
            "--sync-max-ticks" => match iter.next() {
                Some(value) => match value.parse::<u64>() {
                    Ok(ticks) => sync_max_ticks = Some(ticks),
                    _ => {
                        return Cli {
                            json,
                            socket,
                            continuous_sync: None,
                            notify_approvals,
                            command: Command::UsageError(
                                "--sync-max-ticks must be an integer".to_string(),
                            ),
                        };
                    }
                },
                None => {
                    return Cli {
                        json,
                        socket,
                        continuous_sync: None,
                        notify_approvals,
                        command: Command::UsageError(
                            "missing value for --sync-max-ticks".to_string(),
                        ),
                    };
                }
            },
            "-h" | "--help" => positionals.push("help".to_string()),
            "-V" | "--version" => positionals.push("version".to_string()),
            _ => positionals.push(arg),
        }
    }

    let command = match positionals.as_slice() {
        [] => Command::Help,
        [command] if command == "help" => Command::Help,
        [command] if command == "serve" => Command::Serve { once },
        [command, rest @ ..] if command == "sync-once" => parse_sync_once_command(rest),
        [command] if command == "stop" => Command::Stop,
        [command] if command == "status" => Command::Status,
        [command] if command == "version" => Command::Version,
        [command, ..] => Command::Unknown(command.clone()),
    };

    Cli {
        json,
        socket,
        continuous_sync: continuous_sync_options(
            sync_root,
            sync_state_root,
            sync_workspace_id,
            sync_device_id,
            sync_interval,
            sync_max_ticks,
        ),
        notify_approvals,
        command,
    }
}

fn continuous_sync_options(
    root: Option<PathBuf>,
    state_root: Option<PathBuf>,
    workspace_id: String,
    device_id: String,
    interval: Duration,
    max_ticks: Option<u64>,
) -> Option<ContinuousSyncOptions> {
    Some(ContinuousSyncOptions {
        args: SyncOnceArgs {
            root: root?,
            state_root: state_root?,
            workspace_id,
            device_id,
            sync_operation_id: None,
        },
        interval,
        max_ticks,
    })
}

fn parse_sync_once_command(args: &[String]) -> Command {
    let mut root = None;
    let mut state_root = None;
    let mut workspace_id = "ws_code".to_string();
    let mut device_id = "device-daemon".to_string();
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "--root" => {
                let Some(value) = args.get(index + 1) else {
                    return Command::UsageError("missing value for --root".to_string());
                };
                root = Some(PathBuf::from(value));
                index += 2;
            }
            "--state-root" => {
                let Some(value) = args.get(index + 1) else {
                    return Command::UsageError("missing value for --state-root".to_string());
                };
                state_root = Some(PathBuf::from(value));
                index += 2;
            }
            "--workspace" => {
                let Some(value) = args.get(index + 1) else {
                    return Command::UsageError("missing value for --workspace".to_string());
                };
                workspace_id = value.to_string();
                index += 2;
            }
            "--device" => {
                let Some(value) = args.get(index + 1) else {
                    return Command::UsageError("missing value for --device".to_string());
                };
                device_id = value.to_string();
                index += 2;
            }
            flag if flag.starts_with("--") => {
                return Command::UsageError(format!("unknown sync-once option `{flag}`"));
            }
            value => {
                return Command::UsageError(format!("unexpected sync-once argument `{value}`"));
            }
        }
    }

    let Some(root) = root else {
        return Command::UsageError("sync-once requires --root <path>".to_string());
    };
    let Some(state_root) = state_root else {
        return Command::UsageError("sync-once requires --state-root <path>".to_string());
    };
    Command::SyncOnce(SyncOnceArgs {
        root,
        state_root,
        workspace_id,
        device_id,
        sync_operation_id: None,
    })
}

fn run(cli: Cli) -> ExitCode {
    match cli.command {
        Command::Help => {
            print_help(cli.json);
            ExitCode::SUCCESS
        }
        Command::Serve { once } => match serve(
            &cli.socket,
            once,
            DaemonRuntime {
                sync: cli.continuous_sync.map(ContinuousSyncRuntime::new),
                notify_approvals: cli.notify_approvals,
                notification_dedupe: NotificationDedupe::default(),
                next_notification_poll: Instant::now(),
            },
        ) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                print_runtime_error("serve", &error, cli.json);
                ExitCode::from(EXIT_FAILURE)
            }
        },
        Command::SyncOnce(args) => print_sync_once(args, cli.json),
        Command::Stop => print_stop(&cli.socket, cli.json),
        Command::Status => {
            print_status(&cli.socket, cli.json);
            ExitCode::SUCCESS
        }
        Command::Version => {
            print_version(cli.json);
            ExitCode::SUCCESS
        }
        Command::UsageError(message) => {
            print_usage_error(&message, cli.json);
            ExitCode::from(EXIT_USAGE)
        }
        Command::Unknown(command) => {
            print_unknown_command(&command, cli.json);
            ExitCode::from(EXIT_USAGE)
        }
    }
}

fn print_help(json: bool) {
    if json {
        println!(
            "{{\"ok\":true,\"command\":\"help\",\"phase\":\"{PHASE}\",\"commands\":[\"serve\",\"sync-once\",\"stop\",\"status\",\"version\"],\"socket\":{{\"protocol\":\"{PROTOCOL}\",\"version\":{PROTOCOL_VERSION}}}}}"
        );
        return;
    }
    println!(
        "bowline daemon\n\nCommands:\n  bowline-daemon serve [--sync-root <path> --sync-state-root <path>] [--notify-approvals]\n  bowline-daemon sync-once --root <path> --state-root <path>\n  bowline-daemon stop\n  bowline-daemon status\n  bowline-daemon version\n\nGlobal options:\n  --json\n  --socket <path>"
    );
}

fn print_sync_once(args: SyncOnceArgs, json: bool) -> ExitCode {
    match run_sync_once(args) {
        Ok(summary) => {
            if json {
                println!(
                    "{{\"ok\":true,\"command\":\"sync-once\",\"workspaceId\":{},\"snapshotId\":{},\"version\":{},\"objectManifestId\":{},\"manifestObjectKey\":{},\"packObjectCount\":{},\"packObjectKeys\":{},\"stale\":{},\"merged\":{},\"conflictCount\":{}}}",
                    json_string(&summary.workspace_id),
                    json_string(&summary.snapshot_id),
                    summary.version,
                    json_string(&summary.object_manifest_id),
                    json_string(&summary.manifest_object_key),
                    summary.pack_object_keys.len(),
                    json_string_array(&summary.pack_object_keys),
                    summary.stale,
                    summary.merged,
                    summary.conflict_count,
                );
            } else {
                println!(
                    "sync-once: workspace {} at snapshot {} (version {}, manifest {}, stale: {}, merged: {}, conflicts: {})",
                    summary.workspace_id,
                    summary.snapshot_id,
                    summary.version,
                    summary.object_manifest_id,
                    summary.stale,
                    summary.merged,
                    summary.conflict_count
                );
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            if json {
                println!(
                    "{{\"ok\":false,\"command\":\"sync-once\",\"status\":\"error\",\"error\":{{\"code\":\"sync_once_failed\",\"message\":{}}}}}",
                    json_string(&error.to_string())
                );
            } else {
                eprintln!("bowline-daemon sync-once failed: {error}");
            }
            ExitCode::from(EXIT_FAILURE)
        }
    }
}

struct SyncOnceSummary {
    workspace_id: String,
    snapshot_id: String,
    version: u64,
    object_manifest_id: String,
    manifest_object_key: String,
    pack_object_keys: Vec<String>,
    stale: bool,
    merged: bool,
    conflict_count: usize,
    conflicts: Vec<ConflictSummary>,
}

struct ConflictSummary {
    id: String,
    paths: Vec<String>,
}

fn run_sync_once(args: SyncOnceArgs) -> Result<SyncOnceSummary, Box<dyn std::error::Error>> {
    run_sync_once_observed(args, None)
}

fn run_sync_once_observed(
    args: SyncOnceArgs,
    observed_base_ref: Option<WorkspaceRef>,
) -> Result<SyncOnceSummary, Box<dyn std::error::Error>> {
    let workspace_id = WorkspaceId::new(args.workspace_id.clone());
    let device_id = DeviceId::new(args.device_id.clone());
    require_convex_url()?;
    let key_store = key_store()?;
    let workspace_key = key_store
        .load_workspace_key(&workspace_id)?
        .ok_or_else(|| runtime_error("workspace key is missing; approve this device or recover the workspace before daemon sync"))?;
    let workspace_key_bytes = workspace_key_bytes(&workspace_key.key_bytes)?;
    let control_plane = hosted_control_plane(&*key_store, workspace_id.clone(), device_id.clone())?;
    let base_ref = match observed_base_ref {
        Some(workspace_ref) => workspace_ref,
        None => match control_plane.get_workspace_ref(workspace_id.as_str())? {
            Some(workspace_ref) => workspace_ref,
            None => control_plane.create_workspace_ref(workspace_id.as_str())?,
        },
    };
    let byte_store = SignedUrlByteStore::new(&control_plane, workspace_id.as_str());

    run_sync_once_with(
        args,
        &control_plane,
        &byte_store,
        base_ref,
        workspace_id,
        device_id,
        workspace_key_bytes,
    )
}

fn hosted_sync_executor() -> SyncExecutor {
    Box::new(run_sync_once_observed)
}

fn hosted_status_publisher() -> StatusPublisher {
    Box::new(publish_workspace_status_once)
}

/// Compose the local status, redact it, attach the daemon's live component
/// states, and publish it to the hosted control plane. Returns an error on any
/// failure so the caller can log and continue; status publishing must never
/// break the sync loop.
fn publish_workspace_status_once(
    request: StatusPublishRequest,
) -> Result<(), Box<dyn std::error::Error>> {
    let args = &request.args;
    let output = bowline_local::status::compose_status(StatusOptions {
        db_path: Some(args.state_root.join(DEFAULT_DATABASE_FILE)),
        requested_path: Some(args.root.display().to_string()),
        workspace_scope: true,
        generated_at: current_timestamp(),
    })
    .map_err(|error| runtime_error(error.to_string()))?;
    let mut snapshot = bowline_local::status::redacted_status_snapshot(&output, &args.device_id);
    if let Some(sync_state) = request.sync_state {
        snapshot.event_watermarks.sync_state = Some(sync_state);
    }
    if let Some(watcher_state) = request.watcher_state {
        snapshot.event_watermarks.watcher_state = Some(watcher_state);
    }
    if let Some(network_state) = request.network_state {
        snapshot.event_watermarks.network_state = Some(network_state);
    }
    let key_store = key_store()?;
    let control_plane = hosted_control_plane(
        &*key_store,
        args.workspace_id(),
        DeviceId::new(args.device_id.clone()),
    )?;
    control_plane.publish_workspace_status(&snapshot)?;
    Ok(())
}

fn stream_remote_workspace_refs(
    args: SyncOnceArgs,
) -> Result<
    Receiver<bowline_control_plane::ControlPlaneResult<Option<WorkspaceRef>>>,
    Box<dyn std::error::Error>,
> {
    let workspace_id = WorkspaceId::new(args.workspace_id.clone());
    let device_id = DeviceId::new(args.device_id.clone());
    let (sender, receiver) = mpsc::channel();
    std::thread::Builder::new()
        .name("bowline-remote-ref-observer".to_string())
        .spawn(move || {
            let result = stream_remote_workspace_refs_on_thread(
                args,
                workspace_id,
                device_id,
                sender.clone(),
            );
            if let Err(error) = result {
                let _ = sender.send(Err(ControlPlaneError::Storage(error.to_string())));
            }
        })?;
    Ok(receiver)
}

fn stream_remote_workspace_refs_on_thread(
    _args: SyncOnceArgs,
    workspace_id: WorkspaceId,
    device_id: DeviceId,
    sender: Sender<bowline_control_plane::ControlPlaneResult<Option<WorkspaceRef>>>,
) -> Result<(), Box<dyn std::error::Error>> {
    require_convex_url()?;
    let key_store = key_store()?;
    let control_plane = hosted_control_plane(&*key_store, workspace_id.clone(), device_id)?;
    control_plane.stream_workspace_ref_updates(workspace_id.as_str(), sender)?;
    Ok(())
}

fn hosted_remote_ref_observer() -> RemoteRefObserver {
    let mut receiver = None;
    let mut latest = None;
    let mut reconnect_failure_count = 0_u32;
    let mut next_reconnect_attempt = Instant::now();
    let mut last_error = None::<String>;
    Box::new(move |args| {
        if receiver.is_none() {
            let now = Instant::now();
            if now < next_reconnect_attempt {
                let reason = last_error
                    .clone()
                    .unwrap_or_else(|| "remote ref observer is reconnecting".to_string());
                return Err(runtime_error(format!(
                    "remote ref observer reconnecting after failure: {reason}"
                )));
            }
            match stream_remote_workspace_refs(args) {
                Ok(stream) => {
                    receiver = Some(stream);
                    reconnect_failure_count = 0;
                    last_error = None;
                }
                Err(error) => {
                    reconnect_failure_count = reconnect_failure_count.saturating_add(1);
                    next_reconnect_attempt =
                        now + remote_observer_reconnect_delay(reconnect_failure_count);
                    last_error = Some(error.to_string());
                    return Err(error);
                }
            }
        }
        let mut disconnected = false;
        let mut observer_error = None;
        if let Some(receiver) = &receiver {
            loop {
                match receiver.try_recv() {
                    Ok(Ok(Some(workspace_ref))) => latest = Some(workspace_ref),
                    Ok(Ok(None)) => latest = None,
                    Ok(Err(error)) => {
                        observer_error = Some(error);
                        break;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        if let Some(error) = observer_error {
            reconnect_failure_count = reconnect_failure_count.saturating_add(1);
            next_reconnect_attempt =
                Instant::now() + remote_observer_reconnect_delay(reconnect_failure_count);
            last_error = Some(error.to_string());
            receiver = None;
            latest = None;
            return Err(Box::new(error));
        }
        if disconnected {
            reconnect_failure_count = reconnect_failure_count.saturating_add(1);
            next_reconnect_attempt =
                Instant::now() + remote_observer_reconnect_delay(reconnect_failure_count);
            last_error = Some("remote ref subscription disconnected".to_string());
            receiver = None;
            latest = None;
        }
        Ok(latest.clone())
    })
}

fn remote_observer_reconnect_delay(failure_count: u32) -> Duration {
    let exponent = failure_count.saturating_sub(1).min(6);
    let multiplier = 1_u64 << exponent;
    let delay_seconds = REMOTE_OBSERVER_RECONNECT_INITIAL
        .as_secs()
        .saturating_mul(multiplier)
        .min(REMOTE_OBSERVER_RECONNECT_MAX.as_secs());
    Duration::from_secs(delay_seconds)
}

fn run_sync_once_with(
    args: SyncOnceArgs,
    control_plane: &dyn ControlPlaneClient,
    byte_store: &dyn ByteStore,
    base_ref: bowline_control_plane::WorkspaceRef,
    workspace_id: WorkspaceId,
    device_id: DeviceId,
    workspace_key_bytes: [u8; 32],
) -> Result<SyncOnceSummary, Box<dyn std::error::Error>> {
    let runner = SyncRunner::new_with_base_ref(
        control_plane,
        byte_store,
        SyncRunnerOptions {
            root: args.root,
            state_root: args.state_root,
            workspace_id,
            device_id,
            workspace_content_key: workspace_key_bytes,
            storage_key: StorageKey::from_bytes(workspace_key_bytes),
            key_epoch: 1,
            generated_at: current_timestamp(),
            sync_operation_id: args.sync_operation_id.clone(),
        },
        base_ref.clone(),
    );
    match runner.tick()? {
        SyncTickOutcome::NoWorkspaceRef => Ok(SyncOnceSummary {
            workspace_id: base_ref.workspace_id,
            snapshot_id: base_ref.snapshot_id,
            version: base_ref.version,
            object_manifest_id: "none".to_string(),
            manifest_object_key: "none".to_string(),
            pack_object_keys: Vec::new(),
            stale: false,
            merged: false,
            conflict_count: 0,
            conflicts: Vec::new(),
        }),
        SyncTickOutcome::NoChanges => Ok(SyncOnceSummary {
            workspace_id: base_ref.workspace_id,
            snapshot_id: base_ref.snapshot_id,
            version: base_ref.version,
            object_manifest_id: "none".to_string(),
            manifest_object_key: "none".to_string(),
            pack_object_keys: Vec::new(),
            stale: false,
            merged: false,
            conflict_count: 0,
            conflicts: Vec::new(),
        }),
        SyncTickOutcome::Imported(workspace_ref) => Ok(SyncOnceSummary {
            workspace_id: workspace_ref.workspace_id,
            snapshot_id: workspace_ref.snapshot_id,
            version: workspace_ref.version,
            object_manifest_id: "none".to_string(),
            manifest_object_key: "none".to_string(),
            pack_object_keys: Vec::new(),
            stale: false,
            merged: false,
            conflict_count: 0,
            conflicts: Vec::new(),
        }),
        SyncTickOutcome::Uploaded(outcome) => match *outcome {
            UploadOutcome::Advanced {
                workspace_ref,
                object_manifest,
            } => Ok(summary_from_uploaded(
                workspace_ref,
                object_manifest,
                false,
                false,
                0,
            )),
            UploadOutcome::Stale {
                stale,
                object_manifest,
            } => Ok(summary_from_uploaded(
                stale.current,
                object_manifest,
                true,
                false,
                0,
            )),
        },
        SyncTickOutcome::Merged(outcome) => match *outcome {
            UploadOutcome::Advanced {
                workspace_ref,
                object_manifest,
            } => Ok(summary_from_uploaded(
                workspace_ref,
                object_manifest,
                false,
                true,
                0,
            )),
            UploadOutcome::Stale {
                stale,
                object_manifest,
            } => Ok(summary_from_uploaded(
                stale.current,
                object_manifest,
                true,
                true,
                0,
            )),
        },
        SyncTickOutcome::Conflicted(conflicts) => Ok(SyncOnceSummary {
            workspace_id: base_ref.workspace_id,
            snapshot_id: base_ref.snapshot_id,
            version: base_ref.version,
            object_manifest_id: "none".to_string(),
            manifest_object_key: "none".to_string(),
            pack_object_keys: Vec::new(),
            stale: true,
            merged: false,
            conflict_count: conflicts.len(),
            conflicts: conflicts
                .into_iter()
                .map(|conflict| ConflictSummary {
                    id: conflict.id,
                    paths: conflict.paths,
                })
                .collect(),
        }),
    }
}

fn summary_from_uploaded(
    workspace_ref: bowline_control_plane::WorkspaceRef,
    object_manifest: bowline_control_plane::ObjectManifestRecord,
    stale: bool,
    merged: bool,
    conflict_count: usize,
) -> SyncOnceSummary {
    SyncOnceSummary {
        workspace_id: workspace_ref.workspace_id,
        snapshot_id: workspace_ref.snapshot_id,
        version: workspace_ref.version,
        object_manifest_id: object_manifest.manifest_id,
        manifest_object_key: object_manifest.manifest_object.object_key,
        pack_object_keys: object_manifest
            .pack_objects
            .into_iter()
            .map(|object| object.object_key)
            .collect(),
        stale,
        merged,
        conflict_count,
        conflicts: Vec::new(),
    }
}

fn print_status(socket: &Path, json: bool) {
    match handshake(socket) {
        Ok(handshake) => {
            let sync_json = handshake
                .sync_json
                .as_ref()
                .map(|sync| format!(",\"sync\":{sync}"))
                .unwrap_or_default();
            if json {
                println!(
                    "{{\"ok\":true,\"command\":\"status\",\"daemon\":{{\"state\":\"running\",\"socket\":{},\"protocol\":\"{PROTOCOL}\",\"version\":{PROTOCOL_VERSION},\"daemonVersion\":{}}}{sync_json}}}",
                    json_string(&socket.display().to_string()),
                    json_string(&handshake.daemon_version)
                );
            } else {
                println!(
                    "bowline-daemon: running ({PROTOCOL} v{PROTOCOL_VERSION}, daemon {})",
                    handshake.daemon_version
                );
            }
        }
        Err(_) => {
            if json {
                println!(
                    "{{\"ok\":true,\"command\":\"status\",\"daemon\":{{\"state\":\"stopped\",\"socket\":{},\"protocol\":\"{PROTOCOL}\",\"version\":{PROTOCOL_VERSION}}}}}",
                    json_string(&socket.display().to_string())
                );
            } else {
                println!("bowline-daemon: stopped");
            }
        }
    }
}

fn print_stop(socket: &Path, json: bool) -> ExitCode {
    match request_shutdown(socket) {
        Ok(()) => {
            if json {
                println!(
                    "{{\"ok\":true,\"command\":\"stop\",\"daemon\":{{\"state\":\"stopping\",\"socket\":{}}}}}",
                    json_string(&socket.display().to_string())
                );
            } else {
                println!("bowline-daemon: stopping");
            }
            ExitCode::SUCCESS
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if json {
                println!(
                    "{{\"ok\":true,\"command\":\"stop\",\"daemon\":{{\"state\":\"stopped\",\"socket\":{}}}}}",
                    json_string(&socket.display().to_string())
                );
            } else {
                println!("bowline-daemon: stopped");
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_runtime_error("stop", &error, json);
            ExitCode::from(EXIT_FAILURE)
        }
    }
}

fn hosted_control_plane(
    key_store: &dyn DeviceKeyStore,
    workspace_id: WorkspaceId,
    device_id: DeviceId,
) -> Result<HostedControlPlaneClient, Box<dyn std::error::Error>> {
    let convex_url = require_convex_url()?;
    let control_plane_token = env::var("BOWLINE_CONTROL_PLANE_TOKEN")
        .ok()
        .filter(|value| !value.is_empty());
    let has_control_plane_token = control_plane_token.is_some();
    let account_session_id = account_session_id(key_store).or_else(|| {
        ensure_durable_account_session(key_store, &workspace_id)
            .ok()
            .flatten()
    });
    let workos_access_token = if has_control_plane_token || account_session_id.is_some() {
        None
    } else {
        workos_access_token(key_store)
    };
    if control_plane_token.is_none()
        && account_session_id.is_none()
        && workos_access_token.is_none()
    {
        return Err(runtime_error(
            "daemon sync requires BOWLINE_ACCOUNT_SESSION_ID, BOWLINE_CONTROL_PLANE_TOKEN, or a stored account session",
        ));
    }

    let identity = key_store.load_or_create_device_identity()?;
    let signer_device_id = device_id.clone();
    let signer_workspace_id = workspace_id.clone();
    let mut client = HostedControlPlaneClient::try_new_with_token(
        convex_url,
        control_plane_token.unwrap_or_default(),
    )?
    .with_device_id(device_id.as_str())
    .with_device_proof_signer(move |workspace_id, proof_device_id, action, subject| {
        if workspace_id != signer_workspace_id.as_str() {
            return Err(ControlPlaneError::Storage(
                "daemon refused to sign for a different workspace".to_string(),
            ));
        }
        if proof_device_id != signer_device_id.as_str() {
            return Err(ControlPlaneError::Storage(
                "daemon refused to sign for a different device id".to_string(),
            ));
        }
        Ok(grants::device_authorization_proof(
            &identity,
            &signer_workspace_id,
            &signer_device_id,
            action,
            subject,
        ))
    });
    if !has_control_plane_token && let Some(access_token) = workos_access_token {
        client = client.with_workos_access_token(access_token);
    }
    if let Some(session_id) = account_session_id {
        client = client.with_account_session_id(session_id);
    }
    Ok(client)
}

fn account_session_id(key_store: &dyn DeviceKeyStore) -> Option<String> {
    nonempty_env_value(env::var("BOWLINE_ACCOUNT_SESSION_ID").ok())
        .filter(|session_id| durable_account_session_id(session_id))
        .or_else(|| {
            key_store
                .load_account_tokens()
                .ok()
                .flatten()
                .and_then(|tokens| tokens.account_session_id)
                .filter(|session_id| durable_account_session_id(session_id))
        })
}

fn durable_account_session_id(session_id: &str) -> bool {
    session_id.starts_with("bowline_session_")
}

fn ensure_durable_account_session(
    key_store: &dyn DeviceKeyStore,
    workspace_id: &WorkspaceId,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    if let Some(session_id) = account_session_id(key_store) {
        return Ok(Some(session_id));
    }
    if key_store.load_account_tokens()?.is_none() {
        return Ok(None);
    }
    let Some(access_token) = workos_access_token(key_store) else {
        return Ok(None);
    };
    let mut tokens = match key_store.load_account_tokens()? {
        Some(tokens) => tokens,
        None => return Ok(None),
    };
    let client =
        HostedControlPlaneClient::try_new_with_token(require_convex_url()?, String::new())?;
    let session_id =
        client.register_account_session_id(access_token, Some(workspace_id.as_str()))?;
    tokens.account_session_id = Some(session_id.clone());
    key_store.store_account_tokens(tokens)?;
    Ok(Some(session_id))
}

fn workos_access_token(key_store: &dyn DeviceKeyStore) -> Option<String> {
    if let Some(token) = nonempty_env_value(env::var("BOWLINE_WORKOS_ACCESS_TOKEN").ok())
        && workos_token_is_not_expired(&token)
    {
        return Some(token);
    }
    if let Some(token) = refresh_env_workos_token(key_store) {
        return Some(token);
    }
    let tokens = key_store.load_account_tokens().ok().flatten()?;
    if workos_token_is_not_expired(&tokens.access_token) {
        return Some(tokens.access_token);
    }
    let client_id = nonempty_env_value(env::var("BOWLINE_WORKOS_CLIENT_ID").ok())
        .unwrap_or_else(|| DEFAULT_WORKOS_CLIENT_ID.to_string());
    workos::refresh_and_store(key_store, &client_id, &tokens.refresh_token)
        .ok()
        .map(|tokens| tokens.access_token)
}

fn refresh_env_workos_token(key_store: &dyn DeviceKeyStore) -> Option<String> {
    let client_id = nonempty_env_value(env::var("BOWLINE_WORKOS_CLIENT_ID").ok())
        .unwrap_or_else(|| DEFAULT_WORKOS_CLIENT_ID.to_string());
    let refresh_token = nonempty_env_value(env::var("BOWLINE_WORKOS_REFRESH_TOKEN").ok())?;
    workos::refresh_and_store(key_store, &client_id, &refresh_token)
        .ok()
        .map(|tokens| tokens.access_token)
}

fn nonempty_env_value(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

fn workos_token_is_not_expired(token: &str) -> bool {
    let Some(payload) = token.split('.').nth(1) else {
        return true;
    };
    let Some(bytes) = decode_base64url(payload) else {
        return true;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return true;
    };
    let Some(exp) = value.get("exp").and_then(|value| value.as_i64()) else {
        return true;
    };
    exp > OffsetDateTime::now_utc().unix_timestamp() + 30
}

fn decode_base64url(input: &str) -> Option<Vec<u8>> {
    let mut bits = 0_u32;
    let mut bit_count = 0_u8;
    let mut output = Vec::new();
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            b'=' => break,
            _ => return None,
        } as u32;
        bits = (bits << 6) | value;
        bit_count += 6;
        if bit_count >= 8 {
            bit_count -= 8;
            output.push(((bits >> bit_count) & 0xff) as u8);
        }
    }
    Some(output)
}

fn require_convex_url() -> Result<String, Box<dyn std::error::Error>> {
    Ok(env::var("CONVEX_URL")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_CONVEX_URL.to_string()))
}

fn key_store() -> Result<Box<dyn DeviceKeyStore>, Box<dyn std::error::Error>> {
    if let Ok(path) = env::var("BOWLINE_SECRET_STORE_PATH")
        && !path.is_empty()
    {
        return Ok(Box::new(ServerLocalSecretStore::new(path)));
    }
    if keychain_secret_store_allowed() {
        return Ok(Box::new(KeyringDeviceKeyStore::new("default")));
    }
    Ok(Box::new(ServerLocalSecretStore::new(
        ServerLocalSecretStore::default_path()?,
    )))
}

fn keychain_secret_store_allowed() -> bool {
    env::var("BOWLINE_SECRET_STORE").as_deref() == Ok("keychain")
        && matches!(
            env::var("BOWLINE_ALLOW_KEYCHAIN_PROBE").as_deref(),
            Ok("1") | Ok("true") | Ok("yes")
        )
}

fn workspace_key_bytes(bytes: &[u8]) -> Result<[u8; 32], Box<dyn std::error::Error>> {
    bytes
        .try_into()
        .map_err(|_| runtime_error("workspace key material must be exactly 32 bytes"))
}

fn runtime_error(message: impl Into<String>) -> Box<dyn std::error::Error> {
    Box::new(io::Error::other(message.into()))
}

fn print_version(json: bool) {
    if json {
        println!(
            "{{\"ok\":true,\"command\":\"version\",\"daemonVersion\":{},\"socket\":{{\"protocol\":\"{PROTOCOL}\",\"version\":{PROTOCOL_VERSION}}}}}",
            json_string(env!("CARGO_PKG_VERSION"))
        );
    } else {
        println!(
            "bowline-daemon {} ({PROTOCOL} v{PROTOCOL_VERSION})",
            env!("CARGO_PKG_VERSION")
        );
    }
}

fn print_usage_error(message: &str, json: bool) {
    if json {
        println!(
            "{{\"ok\":false,\"status\":\"usage_error\",\"error\":{{\"code\":\"usage_error\",\"message\":{},\"phase\":\"{PHASE}\"}}}}",
            json_string(message)
        );
    } else {
        eprintln!("bowline-daemon usage error: {message}");
    }
}

fn print_unknown_command(command: &str, json: bool) {
    if json {
        println!(
            "{{\"ok\":false,\"command\":{},\"status\":\"usage_error\",\"error\":{{\"code\":\"unknown_command\",\"message\":{},\"phase\":\"{PHASE}\"}}}}",
            json_string(command),
            json_string("unknown command")
        );
    } else {
        eprintln!("bowline-daemon unknown command: {command}");
    }
}

fn print_runtime_error(command: &str, error: &io::Error, json: bool) {
    if json {
        println!(
            "{{\"ok\":false,\"command\":{},\"status\":\"error\",\"error\":{{\"code\":\"daemon_error\",\"message\":{},\"phase\":\"{PHASE}\"}}}}",
            json_string(command),
            json_string(&error.to_string())
        );
    } else {
        eprintln!("bowline-daemon {command} failed: {error}");
    }
}

fn serve(socket: &Path, once: bool, mut runtime: DaemonRuntime) -> io::Result<()> {
    prepare_socket(socket)?;
    let listener = UnixListener::bind(socket)?;
    listener.set_nonblocking(true)?;
    let socket_owner_uid = fs::metadata(socket).ok().map(|metadata| metadata.uid());
    let _guard = SocketGuard {
        path: socket.to_path_buf(),
    };

    loop {
        runtime.poll_sync();
        runtime.poll_notifications();
        match listener.accept() {
            Ok((stream, _)) => {
                let shutdown = match handle_client(stream, &runtime, socket_owner_uid) {
                    Ok(shutdown) => shutdown,
                    Err(error) => {
                        eprintln!("bowline-daemon ignored client error: {error}");
                        false
                    }
                };
                if once || shutdown {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread_sleep_short();
            }
            Err(error) => return Err(error),
        }
    }

    Ok(())
}

impl DaemonRuntime {
    fn poll_sync(&mut self) {
        if let Some(sync) = &mut self.sync {
            sync.poll();
        }
    }

    fn poll_notifications(&mut self) {
        if !self.notify_approvals {
            return;
        }
        let now = Instant::now();
        if now < self.next_notification_poll {
            return;
        }
        self.next_notification_poll = now + NOTIFICATION_POLL_INTERVAL;
        let sender = DesktopNotificationSender;
        match self.poll_notifications_with(&sender) {
            Ok(report) if !report.failures.is_empty() => {
                for failure in report.failures {
                    eprintln!(
                        "bowline-daemon notification failed for {}: {}",
                        failure.title, failure.message
                    );
                }
            }
            Err(error) => eprintln!("bowline-daemon notifications unavailable: {error}"),
            _ => {}
        }
    }

    fn poll_notifications_with<S>(
        &mut self,
        sender: &S,
    ) -> Result<NotificationDispatchReport, String>
    where
        S: NotificationSender,
    {
        if !self.notify_approvals {
            return Ok(NotificationDispatchReport::default());
        }
        let Some(sync) = self.sync.as_ref() else {
            return Ok(NotificationDispatchReport::default());
        };
        let args = &sync.options.args;
        let status = bowline_local::status::compose_status(StatusOptions {
            db_path: Some(args.state_root.join(DEFAULT_DATABASE_FILE)),
            requested_path: Some(args.root.display().to_string()),
            workspace_scope: true,
            generated_at: current_timestamp(),
        })
        .map_err(|error| error.to_string())?;
        let payloads = pending_device_payloads(&status);
        Ok(dispatch_new_notifications(
            &payloads,
            &mut self.notification_dedupe,
            sender,
        ))
    }

    fn sync_json_field(&self) -> String {
        self.sync
            .as_ref()
            .map(|sync| {
                format!(
                    ",\"sync\":{}",
                    sync_status_with_hosted_calls(sync.status_json())
                )
            })
            .unwrap_or_default()
    }
}

impl ContinuousSyncRuntime {
    fn new(options: ContinuousSyncOptions) -> Self {
        requeue_startup_sync_claims(&options);
        let (watcher, change_rx, watcher_state) = match start_sync_watcher(&options.args.root) {
            Ok((watcher, change_rx)) => {
                (Some(watcher), Some(change_rx), WatcherRuntimeState::Ready)
            }
            Err(error) => (None, None, WatcherRuntimeState::Limited(error.to_string())),
        };
        let last_json = initial_sync_status_json(&watcher_state);
        Self {
            options,
            next_tick: Instant::now(),
            next_remote_observe: Instant::now(),
            tick_count: 0,
            last_json,
            watcher,
            change_rx,
            watcher_state,
            sync_once: hosted_sync_executor(),
            remote_ref_observer: hosted_remote_ref_observer(),
            latest_observed_ref: None,
            status_publisher: hosted_status_publisher(),
            next_status_publish: Instant::now(),
        }
    }

    fn poll(&mut self) {
        let now = Instant::now();
        self.maybe_publish_status_heartbeat(now);
        let watcher_drain = self.drain_changes();
        if watcher_drain.sync_now {
            self.next_tick = now;
        } else if watcher_drain.changed {
            self.next_tick = now + WATCHER_SETTLE_WINDOW;
        }
        let remote_observe_due = now >= self.next_remote_observe;
        if now < self.next_tick && !remote_observe_due {
            return;
        }
        if self
            .options
            .max_ticks
            .is_some_and(|max_ticks| self.tick_count >= max_ticks)
        {
            self.next_tick = now + self.options.interval;
            return;
        }
        if remote_observe_due && self.observe_remote_ref_cursor() {
            self.next_tick = now;
        }
        if Instant::now() < self.next_tick {
            return;
        }

        self.tick_count += 1;
        self.requeue_expired_sync_claims();
        let Some(claimed_operation) = self.claim_daemon_sync_operation() else {
            self.record_component_states("ready", self.watcher_component_state(), "online");
            self.last_json = self.waiting_for_sync_queue_json();
            self.next_tick = Instant::now() + self.options.interval;
            return;
        };
        let mut sync_args = self.options.args.clone();
        sync_args.sync_operation_id = Some(claimed_operation.clone());
        match (self.sync_once)(sync_args, self.latest_observed_ref.clone()) {
            Ok(summary) => {
                self.complete_daemon_sync_operation(&claimed_operation, &summary);
                self.record_remote_ref_cursor(&summary);
                self.record_component_states("ready", self.watcher_component_state(), "online");
                // Publish live status right after a successful ref advance so the
                // dashboard reflects the new head immediately.
                self.publish_status(
                    Some("ready"),
                    Some(self.watcher_component_state()),
                    Some("online"),
                );
                let queue_json = self.queue_counts_json();
                let head_json = self.local_head_json();
                let remote_head_json = self.remote_head_json();
                self.last_json = format!(
                    "{{\"state\":\"{}\",\"tickCount\":{},\"watcherState\":{},\"lastOutcome\":\"{}\",\"workspaceId\":{},\"snapshotId\":{},\"version\":{},\"conflictCount\":{},\"queueCounts\":{},\"localHead\":{},\"remoteHead\":{}}}",
                    summary.daemon_state(),
                    self.tick_count,
                    self.watcher_state_json(),
                    summary.sync_state(),
                    json_string(&summary.workspace_id),
                    json_string(&summary.snapshot_id),
                    summary.version,
                    summary.conflict_count,
                    queue_json,
                    head_json,
                    remote_head_json,
                );
            }
            Err(error) => {
                self.fail_daemon_sync_operation(&claimed_operation, &error.to_string());
                if self.queue_counts().has_no_pending_work() {
                    self.record_component_states("ready", self.watcher_component_state(), "online");
                    self.last_json = self.waiting_for_sync_queue_json();
                } else {
                    self.record_component_states(
                        "degraded",
                        self.watcher_component_state(),
                        "degraded",
                    );
                    let queue_json = self.queue_counts_json();
                    let head_json = self.local_head_json();
                    let remote_head_json = self.remote_head_json();
                    self.last_json = format!(
                        "{{\"state\":\"limited\",\"tickCount\":{},\"watcherState\":{},\"limitedCapability\":\"continuous sync\",\"unavailableBecause\":{},\"blockedAction\":\"sync ~/Code\",\"stillWorks\":[\"local edits\",\"status\",\"manual sync-once diagnostics\"],\"queueCounts\":{},\"localHead\":{},\"remoteHead\":{}}}",
                        self.tick_count,
                        self.watcher_state_json(),
                        json_string(&error.to_string()),
                        queue_json,
                        head_json,
                        remote_head_json,
                    );
                }
            }
        }
        self.next_tick = Instant::now() + self.options.interval;
    }

    fn status_json(&self) -> &str {
        &self.last_json
    }

    fn waiting_for_sync_queue_json(&self) -> String {
        let counts = self.queue_counts();
        let queue_json = sync_operation_counts_json(&counts);
        let head_json = self.local_head_json();
        let remote_head_json = self.remote_head_json();
        let (state, unavailable_because, blocked_action, still_works) =
            waiting_queue_status_parts(&counts);
        format!(
            "{{\"state\":{},\"tickCount\":{},\"watcherState\":{},\"limitedCapability\":\"continuous sync\",\"unavailableBecause\":{},\"blockedAction\":{},\"stillWorks\":{},\"queueCounts\":{},\"localHead\":{},\"remoteHead\":{}}}",
            json_string(state),
            self.tick_count,
            self.watcher_state_json(),
            json_string(unavailable_because),
            json_string(blocked_action),
            json_string_array(&still_works),
            queue_json,
            head_json,
            remote_head_json,
        )
    }

    fn drain_changes(&mut self) -> WatcherDrain {
        let Some(change_rx) = &self.change_rx else {
            return WatcherDrain::default();
        };
        let mut drain = WatcherDrain::default();
        let mut drained_count = 0;
        for _ in 0..WATCHER_DRAIN_BUDGET {
            let Ok(signal) = change_rx.try_recv() else {
                break;
            };
            drained_count += 1;
            match signal {
                WatcherSignal::Changed(event) => {
                    drain.changed = true;
                    if let Err(error) = self.record_watcher_event(&event) {
                        self.watcher_state = WatcherRuntimeState::Limited(error.to_string());
                        drain.sync_now = true;
                    }
                }
                WatcherSignal::Limited(reason) => {
                    self.watcher_state = WatcherRuntimeState::Limited(reason);
                    drain.changed = true;
                    drain.sync_now = true;
                }
            }
        }
        if drained_count == WATCHER_DRAIN_BUDGET && change_rx.try_recv().is_ok() {
            self.watcher_state =
                WatcherRuntimeState::Limited("watch queue saturated; watcher disabled".to_string());
            self.change_rx = None;
            self.watcher = None;
            drain.changed = true;
            drain.sync_now = true;
        }
        drain
    }

    fn watcher_state_json(&self) -> String {
        let _keep_watcher_alive = self.watcher.as_ref();
        watcher_runtime_state_json(&self.watcher_state)
    }

    fn record_watcher_event(&self, event: &Event) -> Result<(), Box<dyn std::error::Error>> {
        let operation = watcher_operation(&event.kind);
        let store = self.metadata_store()?;
        let workspace_id = self.options.args.workspace_id();
        let device_id = DeviceId::new(self.options.args.device_id.clone());
        let now = current_timestamp();
        let causation_id = format!("watch_{}_{}", self.tick_count, stable_token(&now));

        let paths = watcher_event_paths(&self.options.args.root, operation, event);
        for (index, path, source_path) in paths {
            let Some(relative_path) = watcher_relative_path(&self.options.args.root, path) else {
                continue;
            };
            if relative_path.is_empty() || is_private_state_path(&relative_path) {
                continue;
            }
            let metadata = fs::symlink_metadata(path).ok();
            let is_dir = metadata.as_ref().is_some_and(|metadata| metadata.is_dir());
            let byte_len = metadata
                .as_ref()
                .filter(|metadata| !metadata.is_dir())
                .map(|metadata| metadata.len());
            let policy = UserPolicy::load_for_path(&self.options.args.root, &relative_path)
                .unwrap_or_else(|_| UserPolicy::empty());
            let decision = classify_path(
                &PathFacts {
                    relative_path: relative_path.clone(),
                    is_dir,
                    byte_len,
                },
                &policy,
            );
            if !watcher_should_record(decision.classification, decision.mode) {
                continue;
            }
            store.append_local_write_log(&LocalWriteLogRecord {
                id: format!(
                    "watch_{}_{}_{}",
                    stable_token(&relative_path),
                    stable_token(operation),
                    stable_token(&format!("{now}-{index}")),
                ),
                workspace_id: workspace_id.clone(),
                device_id: device_id.clone(),
                project_id: None,
                path: relative_path,
                source_path,
                operation: operation.to_string(),
                staged_content_id: None,
                policy_classification: decision.classification,
                causation_id: causation_id.clone(),
                settled_at: now.clone(),
                created_at: now.clone(),
            })?;
        }
        Ok(())
    }

    fn watcher_component_state(&self) -> &'static str {
        match self.watcher_state {
            WatcherRuntimeState::Ready => "ready",
            WatcherRuntimeState::Limited(_) => "degraded",
        }
    }

    fn metadata_store(&self) -> Result<MetadataStore, bowline_local::metadata::MetadataError> {
        MetadataStore::open(self.options.args.state_root.join(DEFAULT_DATABASE_FILE))
    }

    fn record_component_states(&self, sync: &str, watcher: &str, network: &str) {
        let Ok(store) = self.metadata_store() else {
            return;
        };
        let now = current_timestamp();
        let _ = store.set_component_state("sync", sync, &now);
        let _ = store.set_component_state("watcher", watcher, &now);
        let _ = store.set_component_state("network", network, &now);
    }

    /// Publish a redacted status snapshot. Any of the component states may be
    /// supplied to attach the daemon's live in-memory view; `None` lets the
    /// composed snapshot keep whatever state it read from the store. Failures are
    /// logged and swallowed so publishing never breaks the sync loop.
    fn publish_status(
        &mut self,
        sync_state: Option<&str>,
        watcher_state: Option<&str>,
        network_state: Option<&str>,
    ) {
        let request = StatusPublishRequest {
            args: self.options.args.clone(),
            sync_state: sync_state.map(str::to_string),
            watcher_state: watcher_state.map(str::to_string),
            network_state: network_state.map(str::to_string),
        };
        if let Err(error) = (self.status_publisher)(request) {
            eprintln!("bowline-daemon status publish skipped: {error}");
        }
        self.next_status_publish = Instant::now() + STATUS_PUBLISH_INTERVAL;
    }

    fn maybe_publish_status_heartbeat(&mut self, now: Instant) {
        if now < self.next_status_publish {
            return;
        }
        let watcher_state = self.watcher_component_state();
        self.publish_status(None, Some(watcher_state), None);
    }

    fn queue_counts_json(&self) -> String {
        let counts = self.queue_counts();
        sync_operation_counts_json(&counts)
    }

    fn queue_counts(&self) -> SyncOperationCounts {
        self.metadata_store()
            .and_then(|store| {
                self.complete_obsolete_daemon_reconciles_if_heads_match(&store);
                store.sync_operation_counts_for_device(
                    &self.options.args.workspace_id(),
                    &DeviceId::new(self.options.args.device_id.clone()),
                )
            })
            .unwrap_or_default()
    }

    fn complete_obsolete_daemon_reconciles_if_heads_match(&self, store: &MetadataStore) {
        let workspace_id = self.options.args.workspace_id();
        let Ok(Some(local_head)) = store.workspace_sync_head(&workspace_id) else {
            return;
        };
        let Ok(Some(remote_head)) = store.remote_ref_cursor(&workspace_id) else {
            return;
        };
        if remote_head.last_observed_version != Some(local_head.workspace_ref.version)
            || remote_head.last_observed_snapshot_id.as_deref()
                != Some(local_head.workspace_ref.snapshot_id.as_str())
        {
            return;
        }
        let now = current_timestamp();
        let payload = format!(
            "{{\"repaired\":\"heads-match\",\"workspaceId\":{},\"snapshotId\":{},\"version\":{}}}",
            json_string(workspace_id.as_str()),
            json_string(local_head.workspace_ref.snapshot_id.as_str()),
            local_head.workspace_ref.version,
        );
        let _ = store.complete_obsolete_daemon_reconciles_for_device(
            &workspace_id,
            &DeviceId::new(self.options.args.device_id.clone()),
            &payload,
            &now,
        );
    }

    fn local_head_json(&self) -> String {
        match self
            .metadata_store()
            .and_then(|store| store.workspace_sync_head(&self.options.args.workspace_id()))
        {
            Ok(Some(head)) => format!(
                "{{\"workspaceId\":{},\"snapshotId\":{},\"version\":{},\"updatedAtTick\":{}}}",
                json_string(&head.workspace_ref.workspace_id),
                json_string(&head.workspace_ref.snapshot_id),
                head.workspace_ref.version,
                head.workspace_ref.updated_at.tick,
            ),
            _ => "null".to_string(),
        }
    }

    fn remote_head_json(&self) -> String {
        match self
            .metadata_store()
            .and_then(|store| store.remote_ref_cursor(&self.options.args.workspace_id()))
        {
            Ok(Some(cursor)) => format!(
                "{{\"workspaceId\":{},\"snapshotId\":{},\"version\":{}}}",
                json_string(cursor.workspace_id.as_str()),
                json_string(
                    cursor
                        .last_observed_snapshot_id
                        .as_deref()
                        .unwrap_or_default()
                ),
                cursor.last_observed_version.unwrap_or_default(),
            ),
            _ => "null".to_string(),
        }
    }

    fn claim_daemon_sync_operation(&self) -> Option<String> {
        let store = self.metadata_store().ok()?;
        let now = current_timestamp();
        let workspace_id = self.options.args.workspace_id();
        let device_id = DeviceId::new(self.options.args.device_id.clone());
        let has_active_reconcile = store
            .active_sync_operation_for_device(&workspace_id, "daemon-reconcile", &device_id)
            .ok()
            .flatten()
            .is_some();
        if !has_active_reconcile
            && self.should_enqueue_daemon_reconcile(&store, &workspace_id, &device_id, &now)
        {
            let operation_nonce = stable_token(&format!(
                "{}:{}:{}:{}",
                self.options.args.device_id,
                self.tick_count,
                now,
                std::process::id()
            ));
            let operation_id = format!("daemon-sync-{}", operation_nonce);
            let idempotency_key = format!(
                "daemon-sync:{}:{}:{}",
                self.options.args.device_id, self.tick_count, operation_nonce
            );
            let record = SyncOperationRecord {
                id: operation_id,
                workspace_id: workspace_id.clone(),
                kind: "daemon-reconcile".to_string(),
                state: "queued".to_string(),
                idempotency_key,
                base_version: None,
                base_snapshot_id: None,
                target_snapshot_id: None,
                device_id: Some(device_id),
                payload_json: format!(
                    "{{\"root\":{},\"stateRoot\":{},\"tickCount\":{}}}",
                    json_string(&self.options.args.root.display().to_string()),
                    json_string(&self.options.args.state_root.display().to_string()),
                    self.tick_count,
                ),
                attempt_count: 0,
                claimed_by: None,
                heartbeat_at: None,
                next_attempt_at: None,
                last_error: None,
                created_at: now.clone(),
                updated_at: now.clone(),
            };
            let _ = store.enqueue_sync_operation(&record);
        }
        store
            .claim_next_sync_operation(&workspace_id, &self.options.args.device_id, &now)
            .ok()
            .flatten()
            .map(|operation| operation.id)
    }

    fn should_enqueue_daemon_reconcile(
        &self,
        store: &MetadataStore,
        workspace_id: &WorkspaceId,
        device_id: &DeviceId,
        now: &str,
    ) -> bool {
        let Some(last_completed) =
            latest_completed_daemon_reconcile(store, workspace_id, device_id)
        else {
            return true;
        };
        if local_writes_after(store, workspace_id, device_id, &last_completed.updated_at) {
            return true;
        }
        if remote_cursor_ahead_of_local_head(store, workspace_id) {
            return true;
        }
        safety_reconcile_due(&last_completed.updated_at, self.options.interval, now)
    }

    fn requeue_expired_sync_claims(&self) {
        let Ok(store) = self.metadata_store() else {
            return;
        };
        let now = OffsetDateTime::now_utc();
        let expired_before =
            format_timestamp(now - time::Duration::seconds(SYNC_CLAIM_TIMEOUT_SECONDS));
        let updated_at = format_timestamp(now);
        let _ = store.requeue_expired_sync_claims(
            &self.options.args.workspace_id(),
            &expired_before,
            &updated_at,
        );
    }

    fn complete_daemon_sync_operation(&self, operation_id: &str, summary: &SyncOnceSummary) {
        let Ok(store) = self.metadata_store() else {
            return;
        };
        let now = current_timestamp();
        let payload = format!(
            "{{\"outcome\":\"{}\",\"workspaceId\":{},\"snapshotId\":{},\"version\":{},\"conflictCount\":{}}}",
            summary.sync_state(),
            json_string(&summary.workspace_id),
            json_string(&summary.snapshot_id),
            summary.version,
            summary.conflict_count,
        );
        let _ = store.complete_sync_operation(operation_id, &payload, &now);
        self.append_sync_completed_event(&store, operation_id, summary, &now);
    }

    fn record_remote_ref_cursor(&self, summary: &SyncOnceSummary) {
        let Ok(store) = self.metadata_store() else {
            return;
        };
        let _ = store.put_remote_ref_cursor(&RemoteRefCursorRecord {
            workspace_id: WorkspaceId::new(summary.workspace_id.clone()),
            cursor: None,
            last_observed_version: Some(summary.version),
            last_observed_snapshot_id: Some(summary.snapshot_id.clone()),
            updated_at: current_timestamp(),
        });
    }

    fn observe_remote_ref_cursor(&mut self) -> bool {
        self.next_remote_observe = Instant::now() + REMOTE_OBSERVER_DRAIN_INTERVAL;
        let workspace_id = self.options.args.workspace_id();
        let observed = match (self.remote_ref_observer)(self.options.args.clone()) {
            Ok(observed) => observed,
            Err(error) => {
                self.latest_observed_ref = None;
                self.record_component_states("idle", self.watcher_component_state(), "degraded");
                self.last_json = format!(
                    "{{\"state\":\"limited\",\"tickCount\":{},\"unavailableBecause\":{},\"nextAction\":\"check network or hosted auth\",\"queue\":{},\"localHead\":{},\"remoteHead\":{}}}",
                    self.tick_count,
                    json_string(&error.to_string()),
                    self.queue_counts_json(),
                    self.local_head_json(),
                    self.remote_head_json(),
                );
                return false;
            }
        };
        let Some(remote_ref) = observed else {
            self.latest_observed_ref = None;
            return false;
        };
        self.latest_observed_ref = Some(remote_ref.clone());
        let Ok(store) = self.metadata_store() else {
            return false;
        };
        let _ = store.put_remote_ref_cursor(&RemoteRefCursorRecord {
            workspace_id: workspace_id.clone(),
            cursor: None,
            last_observed_version: Some(remote_ref.version),
            last_observed_snapshot_id: Some(remote_ref.snapshot_id),
            updated_at: current_timestamp(),
        });
        remote_cursor_ahead_of_local_head(&store, &workspace_id)
    }

    fn fail_daemon_sync_operation(&self, operation_id: &str, message: &str) {
        let Ok(store) = self.metadata_store() else {
            return;
        };
        let now = current_timestamp();
        let action = sync_failure_action(message);
        match action {
            SyncFailureAction::Attention => {
                let _ = store.mark_sync_operation_attention(operation_id, message, &now);
            }
            SyncFailureAction::Offline => {
                let retry_at = self.next_sync_attempt_at(&store, operation_id);
                let _ = store.block_sync_operation_offline(operation_id, message, &retry_at, &now);
            }
            SyncFailureAction::Retry => {
                let retry_at = self.next_sync_attempt_at(&store, operation_id);
                let _ = store.fail_sync_operation_for_retry(operation_id, message, &retry_at, &now);
            }
        }
        self.append_sync_failure_event(&store, operation_id, action, &now);
    }

    fn next_sync_attempt_at(&self, store: &MetadataStore, operation_id: &str) -> String {
        let attempt_count = store
            .sync_operation_by_id(operation_id)
            .ok()
            .flatten()
            .map(|operation| operation.attempt_count)
            .unwrap_or(1);
        format_timestamp(
            OffsetDateTime::now_utc()
                + time::Duration::seconds(retry_delay_seconds(operation_id, attempt_count)),
        )
    }

    fn append_sync_completed_event(
        &self,
        store: &MetadataStore,
        operation_id: &str,
        summary: &SyncOnceSummary,
        now: &str,
    ) {
        let workspace_id = self.options.args.workspace_id();
        let mut event = sync_event(
            EventName::SyncCompleted,
            EventSeverity::Info,
            format!(
                "Continuous sync completed with outcome `{}`.",
                summary.sync_state()
            ),
            &workspace_id,
            &self.options.args.device_id,
            operation_id,
            now,
        );
        event.payload.insert(
            "outcome".to_string(),
            serde_json::Value::String(summary.sync_state().to_string()),
        );
        event.payload.insert(
            "snapshotId".to_string(),
            serde_json::Value::String(summary.snapshot_id.clone()),
        );
        event.payload.insert(
            "version".to_string(),
            serde_json::Value::from(summary.version),
        );
        event.payload.insert(
            "conflictCount".to_string(),
            serde_json::Value::from(summary.conflict_count),
        );
        let _ = store.append_event(event);
        for conflict in &summary.conflicts {
            self.append_conflict_created_event(store, operation_id, conflict, now);
        }
    }

    fn append_conflict_created_event(
        &self,
        store: &MetadataStore,
        operation_id: &str,
        conflict: &ConflictSummary,
        now: &str,
    ) {
        let workspace_id = self.options.args.workspace_id();
        let event_operation_id = format!("{operation_id}:{}", conflict.id);
        let mut event = WorkspaceEvent::new(
            sync_event_id(EventName::ConflictCreated, &event_operation_id, now),
            EventName::ConflictCreated,
            now,
            EventSeverity::Attention,
            format!(
                "Continuous sync detected a conflict in {} path(s).",
                conflict.paths.len()
            ),
            workspace_id,
        );
        event.device_id = Some(DeviceId::new(self.options.args.device_id.clone()));
        event.path = conflict.paths.first().cloned();
        event.subject = Some(EventSubject {
            kind: EventSubjectKind::Conflict,
            id: conflict.id.clone(),
            path: event.path.clone(),
        });
        event.payload.insert(
            "operationId".to_string(),
            serde_json::Value::String(operation_id.to_string()),
        );
        event.payload.insert(
            "conflictId".to_string(),
            serde_json::Value::String(conflict.id.clone()),
        );
        event.payload.insert(
            "pathCount".to_string(),
            serde_json::Value::from(conflict.paths.len()),
        );
        let _ = store.append_event(event);
    }

    fn append_sync_failure_event(
        &self,
        store: &MetadataStore,
        operation_id: &str,
        action: SyncFailureAction,
        now: &str,
    ) {
        let (name, severity, outcome) = match action {
            SyncFailureAction::Attention => (
                EventName::SyncDegraded,
                EventSeverity::Attention,
                "attention",
            ),
            SyncFailureAction::Offline => {
                (EventName::SyncLimited, EventSeverity::Limited, "offline")
            }
            SyncFailureAction::Retry => (EventName::SyncLimited, EventSeverity::Limited, "retry"),
        };
        let workspace_id = self.options.args.workspace_id();
        let mut event = sync_event(
            name,
            severity,
            format!("Continuous sync is waiting for {outcome}."),
            &workspace_id,
            &self.options.args.device_id,
            operation_id,
            now,
        );
        event.payload.insert(
            "outcome".to_string(),
            serde_json::Value::String(outcome.to_string()),
        );
        event.redaction = EventRedaction::applied(["error-message-not-included"]);
        let _ = store.append_event(event);
    }
}

fn requeue_startup_sync_claims(options: &ContinuousSyncOptions) {
    let workspace_id = options.args.workspace_id();
    let workspace_key_available = key_store()
        .and_then(|store| {
            store
                .load_workspace_key(&workspace_id)
                .map_err(|error| Box::new(error) as Box<dyn std::error::Error>)
        })
        .ok()
        .flatten()
        .is_some();
    requeue_startup_sync_claims_with_resolved_attention(
        options,
        require_convex_url().is_ok(),
        workspace_key_available,
    );
}

fn requeue_startup_sync_claims_with_resolved_attention(
    options: &ContinuousSyncOptions,
    hosted_config_available: bool,
    workspace_key_available: bool,
) {
    let Ok(store) = MetadataStore::open(options.args.state_root.join(DEFAULT_DATABASE_FILE)) else {
        return;
    };
    let workspace_id = options.args.workspace_id();
    let device_id = DeviceId::new(options.args.device_id.clone());
    let now = current_timestamp();
    let _ = store.requeue_claimed_sync_operations_for_device_kind(
        &workspace_id,
        "daemon-reconcile",
        &device_id,
        &now,
    );
    let _ = store.requeue_waiting_retry_sync_operations_for_device_kind(
        &workspace_id,
        "daemon-reconcile",
        &device_id,
        &now,
    );
    if hosted_config_available {
        let _ = store.requeue_attention_sync_operations_for_device_kind_with_error(
            &workspace_id,
            "daemon-reconcile",
            &device_id,
            "CONVEX_URL is required for daemon sync",
            &now,
        );
    }
    if workspace_key_available {
        let _ = store.requeue_attention_sync_operations_for_device_kind_with_error(
            &workspace_id,
            "daemon-reconcile",
            &device_id,
            "workspace key is missing",
            &now,
        );
    }
}

fn sync_event(
    name: EventName,
    severity: EventSeverity,
    summary: String,
    workspace_id: &WorkspaceId,
    device_id: &str,
    operation_id: &str,
    now: &str,
) -> WorkspaceEvent {
    let mut event = WorkspaceEvent::new(
        sync_event_id(name, operation_id, now),
        name,
        now,
        severity,
        summary,
        workspace_id.clone(),
    );
    event.device_id = Some(DeviceId::new(device_id.to_string()));
    event.subject = Some(EventSubject {
        kind: EventSubjectKind::Component,
        id: "sync".to_string(),
        path: None,
    });
    event.payload.insert(
        "operationId".to_string(),
        serde_json::Value::String(operation_id.to_string()),
    );
    event
}

fn sync_event_id(name: EventName, operation_id: &str, now: &str) -> EventId {
    EventId::new(format!(
        "evt_sync_{}_{}_{}",
        stable_token(&format!("{name:?}")),
        stable_token(operation_id),
        stable_token(now)
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncFailureAction {
    Attention,
    Offline,
    Retry,
}

fn sync_failure_action(message: &str) -> SyncFailureAction {
    if message.contains("CONVEX_URL")
        || message.contains("workspace key is missing")
        || message.contains("trusted")
        || message.contains("approve this device")
    {
        return SyncFailureAction::Attention;
    }
    if message.contains("offline")
        || message.contains("network")
        || message.contains("timed out")
        || message.contains("connection")
        || message.contains("snapshot manifest")
        || message.contains("missing object")
        || message.contains("missing metadata for object")
        || (message.contains("R2 download for object") && message.contains("HTTP 404"))
    {
        return SyncFailureAction::Offline;
    }
    SyncFailureAction::Retry
}

fn retry_delay_seconds(operation_id: &str, attempt_count: u32) -> i64 {
    let exponent = attempt_count.saturating_sub(1).min(5);
    let base = (SYNC_RETRY_INITIAL_SECONDS * 2_i64.pow(exponent)).min(SYNC_RETRY_MAX_SECONDS);
    let jitter = operation_id.bytes().fold(0_u64, |state, byte| {
        state.wrapping_mul(31).wrapping_add(byte as u64)
    }) % (SYNC_RETRY_JITTER_SECONDS as u64 + 1);
    (base + jitter as i64).min(SYNC_RETRY_MAX_SECONDS)
}

fn initial_sync_status_json(watcher_state: &WatcherRuntimeState) -> String {
    format!(
        "{{\"state\":\"queued\",\"tickCount\":0,\"watcherState\":{}}}",
        watcher_runtime_state_json(watcher_state)
    )
}

fn watcher_runtime_state_json(watcher_state: &WatcherRuntimeState) -> String {
    match watcher_state {
        WatcherRuntimeState::Ready => "{\"state\":\"ready\"}".to_string(),
        WatcherRuntimeState::Limited(reason) => format!(
            "{{\"state\":\"limited\",\"unavailableBecause\":{}}}",
            json_string(reason)
        ),
    }
}

fn start_sync_watcher(
    root: &Path,
) -> Result<(RecommendedWatcher, Receiver<WatcherSignal>), notify::Error> {
    let (change_tx, change_rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        send_watcher_signal(&change_tx, event);
    })?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    Ok((watcher, change_rx))
}

fn send_watcher_signal(change_tx: &Sender<WatcherSignal>, event: notify::Result<notify::Event>) {
    match event {
        Ok(event) => {
            let _ = change_tx.send(WatcherSignal::Changed(event));
        }
        Err(error) => {
            let _ = change_tx.send(WatcherSignal::Limited(error.to_string()));
        }
    }
}

fn watcher_operation(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::Create(_) => "create",
        EventKind::Remove(
            RemoveKind::Any | RemoveKind::File | RemoveKind::Folder | RemoveKind::Other,
        ) => "delete",
        EventKind::Modify(ModifyKind::Name(_)) => "rename",
        EventKind::Modify(ModifyKind::Metadata(_)) => "chmod",
        _ => "modify",
    }
}

fn watcher_event_paths<'a>(
    root: &Path,
    operation: &str,
    event: &'a Event,
) -> Vec<(usize, &'a Path, Option<String>)> {
    if operation == "rename" && event.paths.len() >= 2 {
        return vec![(
            1,
            event.paths[1].as_path(),
            watcher_relative_path(root, &event.paths[0]),
        )];
    }
    event
        .paths
        .iter()
        .enumerate()
        .map(|(index, path)| (index, path.as_path(), None))
        .collect()
}

fn watcher_relative_path(root: &Path, path: &Path) -> Option<String> {
    let relative = match path.strip_prefix(root) {
        Ok(relative) => relative,
        Err(_) if path.is_absolute() => return None,
        Err(_) => path,
    };
    let normalized = normalize_workspace_path(&relative.display().to_string());
    if normalized.starts_with("..") {
        return None;
    }
    Some(normalized)
}

fn watcher_should_record(classification: PathClassification, mode: MaterializationMode) -> bool {
    matches!(
        (classification, mode),
        (PathClassification::WorkspaceSync, _)
            | (PathClassification::ProjectEnv, _)
            | (PathClassification::SecretLooking, _)
            | (PathClassification::LargeFile, MaterializationMode::Lazy)
    )
}

fn is_private_state_path(path: &str) -> bool {
    path == ".bowline"
        || path.starts_with(".bowline/")
        || path == ".bowline-conflicts"
        || path.starts_with(".bowline-conflicts/")
}

fn stable_token(value: &str) -> String {
    let token = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    token.trim_matches('_').chars().take(80).collect()
}

impl SyncOnceSummary {
    fn sync_state(&self) -> &'static str {
        if self.conflict_count > 0 {
            "conflicted"
        } else if self.merged {
            "merged"
        } else if self.stale {
            "stale"
        } else if self.object_manifest_id == "none" {
            "no-changes"
        } else {
            "advanced"
        }
    }

    fn daemon_state(&self) -> &'static str {
        if self.conflict_count > 0 {
            "attention"
        } else if self.stale {
            "retrying"
        } else {
            "idle"
        }
    }
}

impl SyncOnceArgs {
    fn workspace_id(&self) -> WorkspaceId {
        WorkspaceId::new(self.workspace_id.clone())
    }
}

fn latest_completed_daemon_reconcile(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    device_id: &DeviceId,
) -> Option<SyncOperationRecord> {
    store
        .sync_operations(workspace_id)
        .ok()?
        .into_iter()
        .filter(|operation| {
            operation.kind == "daemon-reconcile"
                && operation.state == "completed"
                && operation.device_id.as_ref() == Some(device_id)
        })
        .max_by(|left, right| {
            left.updated_at
                .cmp(&right.updated_at)
                .then(left.id.cmp(&right.id))
        })
}

fn local_writes_after(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    device_id: &DeviceId,
    completed_at: &str,
) -> bool {
    store
        .local_write_log(workspace_id)
        .map(|writes| {
            writes.into_iter().any(|write| {
                write.device_id == *device_id && write.created_at.as_str() > completed_at
            })
        })
        .unwrap_or(false)
}

fn remote_cursor_ahead_of_local_head(store: &MetadataStore, workspace_id: &WorkspaceId) -> bool {
    let Ok(Some(cursor)) = store.remote_ref_cursor(workspace_id) else {
        return false;
    };
    let Some(remote_version) = cursor.last_observed_version else {
        return false;
    };
    match store.workspace_sync_head(workspace_id) {
        Ok(Some(head)) => remote_version > head.workspace_ref.version,
        Ok(None) => cursor
            .last_observed_snapshot_id
            .as_deref()
            .is_some_and(|snapshot_id| snapshot_id != "empty"),
        Err(_) => false,
    }
}

fn safety_reconcile_due(completed_at: &str, interval: Duration, now: &str) -> bool {
    let Ok(completed_at) = OffsetDateTime::parse(completed_at, &Rfc3339) else {
        return true;
    };
    let Ok(now) = OffsetDateTime::parse(now, &Rfc3339) else {
        return true;
    };
    let Ok(interval) = time::Duration::try_from(interval) else {
        return true;
    };
    completed_at + interval <= now
}

fn sync_operation_counts_json(counts: &SyncOperationCounts) -> String {
    format!(
        "{{\"queued\":{},\"claimed\":{},\"waitingRetry\":{},\"blockedOffline\":{},\"attention\":{},\"completed\":{}}}",
        counts.queued,
        counts.claimed,
        counts.waiting_retry,
        counts.blocked_offline,
        counts.attention,
        counts.completed,
    )
}

trait SyncOperationCountsExt {
    fn has_no_pending_work(&self) -> bool;
}

impl SyncOperationCountsExt for SyncOperationCounts {
    fn has_no_pending_work(&self) -> bool {
        self.queued == 0
            && self.claimed == 0
            && self.waiting_retry == 0
            && self.blocked_offline == 0
            && self.attention == 0
    }
}

fn sync_status_with_hosted_calls(status_json: &str) -> String {
    let mut output = status_json.to_string();
    if output.ends_with('}') {
        output.pop();
        output.push_str(",\"hostedCalls\":");
        output.push_str(&hosted_call_counts_json());
        output.push('}');
    }
    output
}

fn hosted_call_counts_json() -> String {
    let counts = hosted_function_call_counts();
    let total = counts.iter().map(|count| count.call_count).sum::<u64>();
    let functions = counts
        .iter()
        .map(|count| {
            format!(
                "{{\"name\":{},\"count\":{}}}",
                json_string(&count.function_name),
                count.call_count
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("{{\"total\":{},\"functions\":[{}]}}", total, functions)
}

fn waiting_queue_status_parts(
    counts: &SyncOperationCounts,
) -> (&'static str, &'static str, &'static str, Vec<String>) {
    if counts.attention > 0 {
        return (
            "attention",
            "sync queue needs attention",
            "resolve sync queue attention",
            vec!["local edits".to_string(), "status".to_string()],
        );
    }
    if counts.blocked_offline > 0 {
        return (
            "limited",
            "sync queue is waiting for offline recovery",
            "sync ~/Code",
            vec![
                "local edits".to_string(),
                "status".to_string(),
                "scheduled retry".to_string(),
            ],
        );
    }
    if counts.waiting_retry > 0 {
        return (
            "limited",
            "sync queue is waiting for retry",
            "sync ~/Code",
            vec![
                "local edits".to_string(),
                "status".to_string(),
                "scheduled retry".to_string(),
            ],
        );
    }
    if counts.queued > 0 || counts.claimed > 0 {
        return (
            "syncing",
            "sync queue has pending work",
            "finish sync work",
            vec!["local edits".to_string(), "status".to_string()],
        );
    }
    (
        "idle",
        "no sync work is queued",
        "wait for local or remote changes",
        vec!["local edits".to_string(), "status".to_string()],
    )
}

fn thread_sleep_short() {
    std::thread::sleep(Duration::from_millis(20));
}

fn prepare_socket(socket: &Path) -> io::Result<()> {
    if let Some(parent) = socket.parent() {
        fs::create_dir_all(parent)?;
    }

    if socket.exists() {
        if UnixStream::connect(socket).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "daemon socket is already in use",
            ));
        }
        fs::remove_file(socket)?;
    }

    Ok(())
}

fn handle_client(
    mut stream: UnixStream,
    runtime: &DaemonRuntime,
    socket_owner_uid: Option<u32>,
) -> io::Result<bool> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;

    let request = read_line(&mut stream)?;
    let mut shutdown = false;
    let response = match daemon_request_type(&request).as_deref() {
        Some("hello") if is_hello_request(&request) => format!(
            "{{\"type\":\"hello_ack\",\"protocol\":\"{PROTOCOL}\",\"version\":{PROTOCOL_VERSION},\"daemonVersion\":{},\"status\":\"ok\"{}}}\n",
            json_string(env!("CARGO_PKG_VERSION")),
            runtime.sync_json_field()
        ),
        Some("shutdown") if is_shutdown_request(&request) => {
            shutdown = true;
            format!(
                "{{\"type\":\"shutdown_ack\",\"protocol\":\"{PROTOCOL}\",\"version\":{PROTOCOL_VERSION},\"status\":\"stopping\"}}\n"
            )
        }
        Some("agent.tool.invoke") => handle_agent_tool_request(
            &request,
            local_peer_credential_checked(&stream, socket_owner_uid),
        ),
        _ => format!(
            "{{\"type\":\"error\",\"protocol\":\"{PROTOCOL}\",\"version\":{PROTOCOL_VERSION},\"error\":{{\"code\":\"unsupported_request\",\"message\":\"supported request types: hello, shutdown, agent.tool.invoke\"}}}}\n"
        ),
    };

    stream.write_all(response.as_bytes())?;
    stream.flush()?;
    Ok(shutdown)
}

fn daemon_request_type(request: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(request).ok()?;
    value.get("type")?.as_str().map(ToOwned::to_owned)
}

fn is_hello_request(request: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(request) else {
        return false;
    };
    value.get("type").and_then(serde_json::Value::as_str) == Some("hello")
        && value.get("protocol").and_then(serde_json::Value::as_str) == Some(PROTOCOL)
        && value.get("version").and_then(serde_json::Value::as_u64) == Some(PROTOCOL_VERSION.into())
}

fn is_shutdown_request(request: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(request) else {
        return false;
    };
    value.get("type").and_then(serde_json::Value::as_str) == Some("shutdown")
        && value.get("protocol").and_then(serde_json::Value::as_str) == Some(PROTOCOL)
        && value.get("version").and_then(serde_json::Value::as_u64) == Some(PROTOCOL_VERSION.into())
}

fn validate_agent_tool_contract(request: &AgentToolInvokeRequest) -> Result<(), &'static str> {
    if request.message_type != "agent.tool.invoke" {
        return Err("agent tool request type is unsupported");
    }
    if request.protocol_version != CONTRACT_VERSION {
        return Err("agent tool protocol version is unsupported");
    }
    Ok(())
}

fn handle_agent_tool_request(request: &str, peer_credential_checked: bool) -> String {
    let request = match serde_json::from_str::<AgentToolInvokeRequest>(request) {
        Ok(request) => request,
        Err(error) => {
            return format!(
                "{{\"type\":\"error\",\"protocol\":\"{PROTOCOL}\",\"version\":{PROTOCOL_VERSION},\"error\":{{\"code\":\"invalid_agent_tool_request\",\"message\":{}}}}}\n",
                json_string(&error.to_string())
            );
        }
    };
    if let Err(message) = validate_agent_tool_contract(&request) {
        return format!(
            "{{\"type\":\"error\",\"protocol\":\"{PROTOCOL}\",\"version\":{PROTOCOL_VERSION},\"error\":{{\"code\":\"unsupported_agent_tool_protocol\",\"message\":{}}}}}\n",
            json_string(message)
        );
    }
    let local_daemon_peer_checked =
        peer_credential_checked && request.authority.transport == AgentToolTransport::LocalDaemon;
    match invoke_agent_tool_from_local_daemon(
        env::var_os(ENV_METADATA_DB).map(PathBuf::from),
        request,
        local_daemon_peer_checked,
        current_timestamp(),
    ) {
        Ok(result) => {
            let result_json = serde_json::to_string(&result).expect("agent result serializes");
            format!(
                "{{\"type\":\"agent.tool.result\",\"protocol\":\"{PROTOCOL}\",\"version\":{PROTOCOL_VERSION},\"result\":{result_json}}}\n"
            )
        }
        Err(error) => format!(
            "{{\"type\":\"error\",\"protocol\":\"{PROTOCOL}\",\"version\":{PROTOCOL_VERSION},\"error\":{{\"code\":\"agent_tool_failed\",\"message\":{}}}}}\n",
            json_string(&error.to_string())
        ),
    }
}

fn current_timestamp() -> String {
    format_timestamp(OffsetDateTime::now_utc())
}

fn format_timestamp(timestamp: OffsetDateTime) -> String {
    timestamp
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

fn local_peer_credential_checked(stream: &UnixStream, socket_owner_uid: Option<u32>) -> bool {
    let Some(socket_owner_uid) = socket_owner_uid else {
        return false;
    };
    stream
        .initial_peer_credentials()
        .is_ok_and(|credentials| credentials.euid() == socket_owner_uid)
}

fn handshake(socket: &Path) -> io::Result<Handshake> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(
        format!(
            "{{\"type\":\"hello\",\"protocol\":\"{PROTOCOL}\",\"version\":{PROTOCOL_VERSION}}}\n"
        )
        .as_bytes(),
    )?;
    stream.flush()?;

    let response = read_line(&mut stream)?;
    if !response.contains("\"type\":\"hello_ack\"")
        || !response.contains(&format!("\"protocol\":\"{PROTOCOL}\""))
        || !response.contains(&format!("\"version\":{PROTOCOL_VERSION}"))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "daemon handshake response did not match the expected protocol",
        ));
    }

    Ok(Handshake {
        daemon_version: extract_json_string(&response, "daemonVersion")
            .unwrap_or_else(|| "unknown".to_string()),
        sync_json: extract_json_object(&response, "sync"),
    })
}

fn request_shutdown(socket: &Path) -> io::Result<()> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(
        format!(
            "{{\"type\":\"shutdown\",\"protocol\":\"{PROTOCOL}\",\"version\":{PROTOCOL_VERSION}}}\n"
        )
        .as_bytes(),
    )?;
    stream.flush()?;

    let response = read_line(&mut stream)?;
    if !response.contains("\"type\":\"shutdown_ack\"")
        || !response.contains(&format!("\"protocol\":\"{PROTOCOL}\""))
        || !response.contains(&format!("\"version\":{PROTOCOL_VERSION}"))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "daemon shutdown response did not match the expected protocol",
        ));
    }
    Ok(())
}

fn read_line(stream: &mut UnixStream) -> io::Result<String> {
    let mut bytes = Vec::new();
    let mut one = [0_u8; 1];
    loop {
        match stream.read(&mut one) {
            Ok(0) => break,
            Ok(_) if one[0] == b'\n' => break,
            Ok(_) => bytes.push(one[0]),
            Err(error) => return Err(error),
        }
    }
    String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn extract_json_string(input: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let start = input.find(&needle)? + needle.len();
    let mut value = String::new();
    let mut escaped = false;

    for character in input[start..].chars() {
        if escaped {
            value.push(character);
            escaped = false;
            continue;
        }
        match character {
            '\\' => escaped = true,
            '"' => return Some(value),
            _ => value.push(character),
        }
    }

    None
}

fn extract_json_object(input: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":");
    let marker_start = input.find(&needle)?;
    let object_start =
        marker_start + needle.len() + input[marker_start + needle.len()..].find('{')?;
    let mut depth = 0_usize;
    let mut in_string = false;
    let mut escaped = false;

    for (offset, character) in input[object_start..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let end = object_start + offset + character.len_utf8();
                    return Some(input[object_start..end].to_string());
                }
            }
            _ => {}
        }
    }

    None
}

fn json_string(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len() + 2);
    escaped.push('"');
    for character in input.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character.is_control() => {
                escaped.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => escaped.push(character),
        }
    }
    escaped.push('"');
    escaped
}

fn json_string_array(values: &[String]) -> String {
    format!(
        "[{}]",
        values
            .iter()
            .map(|value| json_string(value))
            .collect::<Vec<_>>()
            .join(",")
    )
}

#[cfg(test)]
#[allow(clippy::arc_with_non_send_sync)]
mod tests {
    use super::{
        Command, ConflictSummary, ContinuousSyncOptions, ContinuousSyncRuntime,
        DEFAULT_DATABASE_FILE, DeviceId, LocalWriteLogRecord, MetadataStore, RemoteRefObserver,
        STATUS_PUBLISH_INTERVAL, StatusPublisher, SyncExecutor, SyncFailureAction, SyncOnceArgs,
        SyncOnceSummary, SyncOperationRecord, WATCHER_DRAIN_BUDGET, WatcherRuntimeState,
        WatcherSignal, WorkspaceId, current_timestamp, hosted_sync_executor,
        initial_sync_status_json, parse_args, remote_observer_reconnect_delay,
        requeue_startup_sync_claims_with_resolved_attention, retry_delay_seconds,
        run_sync_once_with, runtime_error, sync_failure_action, sync_status_with_hosted_calls,
        watcher_relative_path,
    };
    use bowline_control_plane::{
        ControlPlaneClient, ControlPlaneTimestamp, FakeControlPlaneClient, WorkspaceRef,
    };
    use bowline_core::{
        events::{EventName, EventSubjectKind},
        policy::PathClassification,
    };
    use bowline_local::metadata::WorkspaceSyncHeadRecord;
    use bowline_storage::LocalByteStore;
    use notify::{
        Event, EventKind,
        event::{CreateKind, ModifyKind, RemoveKind, RenameMode},
    };
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::{Duration, Instant};
    use std::time::{SystemTime, UNIX_EPOCH};
    use time::OffsetDateTime;

    fn noop_remote_ref_observer() -> RemoteRefObserver {
        Box::new(|_| Ok(None))
    }

    fn noop_status_publisher() -> StatusPublisher {
        Box::new(|_| Ok(()))
    }

    #[test]
    fn parses_serve_once_socket() {
        let cli = parse_args([
            "serve",
            "--once",
            "--socket",
            "/tmp/bowline-daemon-test.sock",
        ]);

        assert_eq!(cli.socket, PathBuf::from("/tmp/bowline-daemon-test.sock"));
        assert_eq!(cli.command, Command::Serve { once: true });
    }

    #[test]
    fn parses_version_flags() {
        let cli = parse_args(["--version"]);
        assert_eq!(cli.command, Command::Version);

        let cli = parse_args(["-V", "--json"]);
        assert!(cli.json);
        assert_eq!(cli.command, Command::Version);
    }

    #[test]
    fn parses_continuous_sync_for_serve() {
        let cli = parse_args([
            "serve",
            "--sync-root",
            "/tmp/code",
            "--sync-state-root",
            "/tmp/state",
            "--sync-workspace",
            "ws_custom",
            "--sync-device",
            "device_custom",
            "--sync-interval-ms",
            "250",
            "--sync-max-ticks",
            "3",
        ]);
        let sync = cli
            .continuous_sync
            .expect("sync options should be configured");

        assert_eq!(sync.args.root, PathBuf::from("/tmp/code"));
        assert_eq!(sync.args.state_root, PathBuf::from("/tmp/state"));
        assert_eq!(sync.args.workspace_id, "ws_custom");
        assert_eq!(sync.args.device_id, "device_custom");
        assert_eq!(sync.interval, std::time::Duration::from_millis(250));
        assert_eq!(sync.max_ticks, Some(3));
    }

    #[test]
    fn parses_notify_approvals_for_continuous_serve() {
        let cli = parse_args([
            "serve",
            "--sync-root",
            "/tmp/code",
            "--sync-state-root",
            "/tmp/state",
            "--notify-approvals",
        ]);

        assert!(cli.notify_approvals);
        assert_eq!(cli.command, Command::Serve { once: false });
        assert!(cli.continuous_sync.is_some());
    }

    #[test]
    fn watcher_error_wakes_reconciliation_and_marks_watcher_limited() {
        let (signal_tx, signal_rx) = mpsc::channel();
        signal_tx
            .send(WatcherSignal::Limited("watch queue overflow".to_string()))
            .expect("watcher signal sends");
        let mut runtime = ContinuousSyncRuntime {
            options: ContinuousSyncOptions {
                args: SyncOnceArgs {
                    root: PathBuf::from("/tmp/bowline-root"),
                    state_root: PathBuf::from("/tmp/bowline-state"),
                    workspace_id: "ws_code".to_string(),
                    device_id: "device-test".to_string(),
                    sync_operation_id: None,
                },
                interval: Duration::from_secs(2),
                max_ticks: None,
            },
            next_tick: Instant::now() + Duration::from_secs(60),
            next_remote_observe: Instant::now() + Duration::from_secs(60),
            tick_count: 0,
            last_json: "{\"state\":\"queued\",\"tickCount\":0}".to_string(),
            watcher: None,
            change_rx: Some(signal_rx),
            watcher_state: WatcherRuntimeState::Ready,
            sync_once: hosted_sync_executor(),
            remote_ref_observer: noop_remote_ref_observer(),
            latest_observed_ref: None,
            status_publisher: noop_status_publisher(),
            next_status_publish: Instant::now() + STATUS_PUBLISH_INTERVAL,
        };

        let drained = runtime.drain_changes();
        assert!(drained.changed);
        assert!(drained.sync_now);
        assert!(matches!(
            runtime.watcher_state,
            WatcherRuntimeState::Limited(ref reason) if reason.contains("overflow")
        ));
    }

    #[test]
    fn watcher_drain_disables_saturated_queue_to_keep_daemon_responsive() {
        let (signal_tx, signal_rx) = mpsc::channel();
        for index in 0..=WATCHER_DRAIN_BUDGET {
            signal_tx
                .send(WatcherSignal::Limited(format!(
                    "watch queue overflow {index}"
                )))
                .expect("watcher signal sends");
        }
        let mut runtime = ContinuousSyncRuntime {
            options: ContinuousSyncOptions {
                args: SyncOnceArgs {
                    root: PathBuf::from("/tmp/bowline-root"),
                    state_root: PathBuf::from("/tmp/bowline-state"),
                    workspace_id: "ws_code".to_string(),
                    device_id: "device-test".to_string(),
                    sync_operation_id: None,
                },
                interval: Duration::from_secs(2),
                max_ticks: None,
            },
            next_tick: Instant::now() + Duration::from_secs(60),
            next_remote_observe: Instant::now() + Duration::from_secs(60),
            tick_count: 0,
            last_json: "{\"state\":\"queued\",\"tickCount\":0}".to_string(),
            watcher: None,
            change_rx: Some(signal_rx),
            watcher_state: WatcherRuntimeState::Ready,
            sync_once: hosted_sync_executor(),
            remote_ref_observer: noop_remote_ref_observer(),
            latest_observed_ref: None,
            status_publisher: noop_status_publisher(),
            next_status_publish: Instant::now() + STATUS_PUBLISH_INTERVAL,
        };

        let first = runtime.drain_changes();
        let second = runtime.drain_changes();

        assert!(first.changed);
        assert!(first.sync_now);
        assert!(matches!(
            runtime.watcher_state,
            WatcherRuntimeState::Limited(ref reason) if reason.contains("saturated")
        ));
        assert!(runtime.change_rx.is_none());
        assert!(!second.changed);
    }

    #[test]
    fn initial_sync_status_reports_limited_watcher() {
        let status = initial_sync_status_json(&WatcherRuntimeState::Limited(
            "watch backend unavailable".to_string(),
        ));

        assert_eq!(
            status,
            "{\"state\":\"queued\",\"tickCount\":0,\"watcherState\":{\"state\":\"limited\",\"unavailableBecause\":\"watch backend unavailable\"}}"
        );
    }

    #[test]
    fn sync_status_includes_hosted_call_budget_snapshot() {
        let status = sync_status_with_hosted_calls(
            "{\"state\":\"idle\",\"tickCount\":1,\"watcherState\":{\"state\":\"ready\"}}",
        );
        let parsed: serde_json::Value = serde_json::from_str(&status).expect("status remains json");

        assert_eq!(parsed["state"], "idle");
        assert!(parsed["hostedCalls"]["total"].is_u64());
        assert!(parsed["hostedCalls"]["functions"].is_array());
    }

    #[test]
    fn watcher_edit_sets_settle_window_without_immediate_sync() {
        let fixture = watcher_fixture("bowline-daemon-watch-settle", "ws_watch_settle");
        let root = fixture.root.clone();
        fs::create_dir_all(root.join("apps/web/src")).expect("root dirs");
        let changed_path = root.join("apps/web/src/auth.ts");
        fs::write(&changed_path, "export const ok = true;\n").expect("file");
        let (signal_tx, signal_rx) = mpsc::channel();
        signal_tx
            .send(WatcherSignal::Changed(
                Event::new(EventKind::Create(CreateKind::File)).add_path(changed_path),
            ))
            .expect("watcher signal sends");
        let original_tick = Instant::now() + Duration::from_secs(60);
        let mut runtime =
            watcher_test_runtime(root, fixture.state_root, fixture.workspace_id.as_str());
        runtime.next_tick = original_tick;
        runtime.change_rx = Some(signal_rx);

        runtime.poll();

        assert_eq!(runtime.tick_count, 0);
        assert!(runtime.next_tick > Instant::now());
        assert!(runtime.next_tick < original_tick);

        let writes = fixture
            .store
            .local_write_log(&fixture.workspace_id)
            .expect("write log");
        assert_eq!(writes.len(), 1);

        let _ = fs::remove_dir_all(fixture.temp);
    }

    #[test]
    fn watcher_event_records_durable_local_write_observation() {
        let fixture = watcher_fixture("bowline-daemon-watch-write", "ws_watch");
        let root = fixture.root.clone();
        fs::create_dir_all(root.join("apps/web/src")).expect("root dirs");
        let changed_path = root.join("apps/web/src/auth.ts");
        fs::write(&changed_path, "export const ok = true;\n").expect("file");

        let runtime = watcher_test_runtime(root, fixture.state_root, fixture.workspace_id.as_str());
        runtime
            .record_watcher_event(
                &Event::new(EventKind::Create(CreateKind::File)).add_path(changed_path),
            )
            .expect("event records");

        let writes = fixture
            .store
            .local_write_log(&fixture.workspace_id)
            .expect("write log");
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].path, "apps/web/src/auth.ts");
        assert_eq!(writes[0].operation, "create");
        assert_eq!(
            writes[0].policy_classification,
            PathClassification::WorkspaceSync
        );

        let _ = fs::remove_dir_all(fixture.temp);
    }

    #[test]
    fn watcher_event_ignores_private_bowline_state() {
        let fixture = watcher_fixture("bowline-daemon-watch-private", "ws_watch_private");
        let root = fixture.root.clone();
        fs::create_dir_all(root.join(".bowline")).expect("private dir");
        let private_path = root.join(".bowline/local.sqlite3");
        fs::write(&private_path, "state").expect("private file");

        let runtime = watcher_test_runtime(root, fixture.state_root, fixture.workspace_id.as_str());
        runtime
            .record_watcher_event(
                &Event::new(EventKind::Remove(RemoveKind::File)).add_path(private_path),
            )
            .expect("private event ignored");

        let writes = fixture
            .store
            .local_write_log(&fixture.workspace_id)
            .expect("write log");
        assert!(writes.is_empty());

        let _ = fs::remove_dir_all(fixture.temp);
    }

    #[test]
    fn watcher_rename_records_source_and_target_once() {
        let fixture = watcher_fixture("bowline-daemon-watch-rename", "ws_watch_rename");
        let root = fixture.root.clone();
        fs::create_dir_all(root.join("apps/web/src")).expect("root dirs");
        let old_path = root.join("apps/web/src/old.ts");
        let new_path = root.join("apps/web/src/new.ts");
        fs::write(&new_path, "renamed\n").expect("renamed file");

        let runtime = watcher_test_runtime(root, fixture.state_root, fixture.workspace_id.as_str());
        runtime
            .record_watcher_event(
                &Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                    .add_path(old_path)
                    .add_path(new_path),
            )
            .expect("rename records");

        let writes = fixture
            .store
            .local_write_log(&fixture.workspace_id)
            .expect("write log");
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].operation, "rename");
        assert_eq!(
            writes[0].source_path.as_deref(),
            Some("apps/web/src/old.ts")
        );
        assert_eq!(writes[0].path, "apps/web/src/new.ts");

        let _ = fs::remove_dir_all(fixture.temp);
    }

    #[test]
    fn watcher_relative_path_rejects_absolute_paths_outside_root() {
        assert_eq!(
            watcher_relative_path(
                PathBuf::from("/tmp/Code").as_path(),
                PathBuf::from("/etc/passwd").as_path()
            ),
            None
        );
    }

    #[test]
    fn completed_sync_records_remote_ref_cursor() {
        let temp = unique_temp_dir("bowline-daemon-remote-cursor");
        let state_root = temp.join(".state");
        let workspace_id = WorkspaceId::new("ws_remote_cursor");
        let runtime = ContinuousSyncRuntime {
            options: ContinuousSyncOptions {
                args: SyncOnceArgs {
                    root: temp.join("Code"),
                    state_root: state_root.clone(),
                    workspace_id: workspace_id.as_str().to_string(),
                    device_id: "device-a".to_string(),
                    sync_operation_id: None,
                },
                interval: Duration::from_secs(60),
                max_ticks: None,
            },
            next_tick: Instant::now(),
            next_remote_observe: Instant::now(),
            tick_count: 0,
            last_json: String::new(),
            watcher: None,
            change_rx: None,
            watcher_state: WatcherRuntimeState::Ready,
            sync_once: hosted_sync_executor(),
            remote_ref_observer: noop_remote_ref_observer(),
            latest_observed_ref: None,
            status_publisher: noop_status_publisher(),
            next_status_publish: Instant::now() + STATUS_PUBLISH_INTERVAL,
        };

        let summary = SyncOnceSummary {
            workspace_id: workspace_id.as_str().to_string(),
            snapshot_id: "snap-42".to_string(),
            version: 42,
            object_manifest_id: "none".to_string(),
            manifest_object_key: "none".to_string(),
            pack_object_keys: Vec::new(),
            stale: false,
            merged: false,
            conflict_count: 0,
            conflicts: Vec::new(),
        };
        runtime.record_remote_ref_cursor(&summary);
        runtime.complete_daemon_sync_operation("op-complete-sync-event", &summary);

        let store = MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata");
        let cursor = store
            .remote_ref_cursor(&workspace_id)
            .expect("cursor reads")
            .expect("cursor stored");
        assert_eq!(cursor.last_observed_version, Some(42));
        assert_eq!(cursor.last_observed_snapshot_id.as_deref(), Some("snap-42"));
        assert_eq!(
            runtime.remote_head_json(),
            "{\"workspaceId\":\"ws_remote_cursor\",\"snapshotId\":\"snap-42\",\"version\":42}"
        );
        let events = store.list_events(20).expect("events read");
        let event = events
            .iter()
            .find(|event| event.name == EventName::SyncCompleted)
            .expect("sync completed event");
        assert_eq!(event.payload["outcome"], "no-changes");
        assert_eq!(event.payload["version"], 42);

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn conflicted_sync_emits_conflict_created_event() {
        let temp = unique_temp_dir("bowline-daemon-conflict-event");
        let state_root = temp.join(".state");
        fs::create_dir_all(&state_root).expect("state root");
        let workspace_id = WorkspaceId::new("ws_conflict_event");
        let runtime = ContinuousSyncRuntime {
            options: ContinuousSyncOptions {
                args: SyncOnceArgs {
                    root: temp.join("Code"),
                    state_root: state_root.clone(),
                    workspace_id: workspace_id.as_str().to_string(),
                    device_id: "device-a".to_string(),
                    sync_operation_id: None,
                },
                interval: Duration::from_secs(60),
                max_ticks: None,
            },
            next_tick: Instant::now(),
            next_remote_observe: Instant::now(),
            tick_count: 0,
            last_json: String::new(),
            watcher: None,
            change_rx: None,
            watcher_state: WatcherRuntimeState::Ready,
            sync_once: hosted_sync_executor(),
            remote_ref_observer: noop_remote_ref_observer(),
            latest_observed_ref: None,
            status_publisher: noop_status_publisher(),
            next_status_publish: Instant::now() + STATUS_PUBLISH_INTERVAL,
        };

        let summary = SyncOnceSummary {
            workspace_id: workspace_id.as_str().to_string(),
            snapshot_id: "snap-base".to_string(),
            version: 7,
            object_manifest_id: "none".to_string(),
            manifest_object_key: "none".to_string(),
            pack_object_keys: Vec::new(),
            stale: true,
            merged: false,
            conflict_count: 1,
            conflicts: vec![ConflictSummary {
                id: "conflict_app_src_main".to_string(),
                paths: vec!["app/src/main.ts".to_string()],
            }],
        };

        let store = MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata");
        store
            .insert_workspace(&workspace_id, "Code", "2026-06-27T00:00:00Z")
            .expect("workspace");
        store
            .insert_root(
                "root_code",
                &workspace_id,
                &runtime.options.args.root.display().to_string(),
                "2026-06-27T00:00:00Z",
            )
            .expect("root");
        runtime.append_sync_completed_event(
            &store,
            "op-conflicted-sync-event",
            &summary,
            "2026-06-27T00:00:00Z",
        );

        let events = store.list_events(20).expect("events read");
        assert!(
            events
                .iter()
                .any(|event| event.name == EventName::SyncCompleted
                    && event.payload["outcome"] == "conflicted"
                    && event.payload["conflictCount"] == 1),
            "{events:?}"
        );
        let conflict = events
            .iter()
            .find(|event| event.name == EventName::ConflictCreated)
            .expect("conflict event");
        assert_eq!(conflict.path.as_deref(), Some("app/src/main.ts"));
        assert_eq!(conflict.payload["conflictId"], "conflict_app_src_main");
        assert!(
            conflict
                .subject
                .as_ref()
                .is_some_and(|subject| subject.kind == EventSubjectKind::Conflict
                    && subject.id == "conflict_app_src_main")
        );

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn daemon_requeues_expired_claims_before_next_sync() {
        let temp = unique_temp_dir("bowline-daemon-requeue-expired");
        let state_root = temp.join(".state");
        let workspace_id = WorkspaceId::new("ws_requeue");
        let store =
            MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
        store
            .enqueue_sync_operation(&SyncOperationRecord {
                id: "op-expired".to_string(),
                workspace_id: workspace_id.clone(),
                kind: "upload".to_string(),
                state: "claimed".to_string(),
                idempotency_key: "expired".to_string(),
                base_version: Some(1),
                base_snapshot_id: Some("snap-1".to_string()),
                target_snapshot_id: Some("snap-2".to_string()),
                device_id: Some(DeviceId::new("device-a")),
                payload_json: "{}".to_string(),
                attempt_count: 1,
                claimed_by: Some("dead-daemon".to_string()),
                heartbeat_at: Some("1970-01-01T00:00:00Z".to_string()),
                next_attempt_at: None,
                last_error: None,
                created_at: "1970-01-01T00:00:00Z".to_string(),
                updated_at: "1970-01-01T00:00:00Z".to_string(),
            })
            .expect("operation queued");
        let runtime = ContinuousSyncRuntime {
            options: ContinuousSyncOptions {
                args: SyncOnceArgs {
                    root: temp.join("Code"),
                    state_root: state_root.clone(),
                    workspace_id: workspace_id.as_str().to_string(),
                    device_id: "device-a".to_string(),
                    sync_operation_id: None,
                },
                interval: Duration::from_secs(60),
                max_ticks: None,
            },
            next_tick: Instant::now(),
            next_remote_observe: Instant::now(),
            tick_count: 0,
            last_json: String::new(),
            watcher: None,
            change_rx: None,
            watcher_state: WatcherRuntimeState::Ready,
            sync_once: hosted_sync_executor(),
            remote_ref_observer: noop_remote_ref_observer(),
            latest_observed_ref: None,
            status_publisher: noop_status_publisher(),
            next_status_publish: Instant::now() + STATUS_PUBLISH_INTERVAL,
        };

        runtime.requeue_expired_sync_claims();

        let operations = store
            .sync_operations(&workspace_id)
            .expect("operations read");
        assert_eq!(operations[0].state, "queued");
        assert_eq!(operations[0].claimed_by, None);
        assert_eq!(operations[0].heartbeat_at, None);

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn daemon_restart_idles_after_recent_completed_tick_operation() {
        let temp = unique_temp_dir("bowline-daemon-restart-operation-id");
        let state_root = temp.join(".state");
        let workspace_id = WorkspaceId::new("ws_restart_operation_id");
        let store =
            MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
        store
            .enqueue_sync_operation(&SyncOperationRecord {
                id: "daemon-sync-tick-1".to_string(),
                workspace_id: workspace_id.clone(),
                kind: "daemon-reconcile".to_string(),
                state: "completed".to_string(),
                idempotency_key: "daemon-sync:device-test:1".to_string(),
                base_version: None,
                base_snapshot_id: None,
                target_snapshot_id: None,
                device_id: Some(DeviceId::new("device-test")),
                payload_json: "{}".to_string(),
                attempt_count: 1,
                claimed_by: None,
                heartbeat_at: None,
                next_attempt_at: None,
                last_error: None,
                created_at: current_timestamp(),
                updated_at: current_timestamp(),
            })
            .expect("completed operation inserted");
        let runtime = watcher_test_runtime(
            temp.join("Code"),
            state_root.clone(),
            "ws_restart_operation_id",
        );

        assert_eq!(runtime.claim_daemon_sync_operation(), None);

        let operations = store
            .sync_operations(&workspace_id)
            .expect("operations read");
        assert_eq!(operations.len(), 1);
        assert_eq!(operations[0].state, "completed");

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn daemon_poll_idles_without_running_sync_once_when_no_work_exists() {
        let temp = unique_temp_dir("bowline-daemon-poll-idle-budget");
        let root = temp.join("Code");
        let state_root = temp.join(".state");
        fs::create_dir_all(&root).expect("root");
        let workspace_id = WorkspaceId::new("ws_poll_idle_budget");
        let store =
            MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
        store
            .enqueue_sync_operation(&SyncOperationRecord {
                id: "daemon-sync-completed".to_string(),
                workspace_id: workspace_id.clone(),
                kind: "daemon-reconcile".to_string(),
                state: "completed".to_string(),
                idempotency_key: "daemon-sync:device-a:completed".to_string(),
                base_version: None,
                base_snapshot_id: None,
                target_snapshot_id: None,
                device_id: Some(DeviceId::new("device-a")),
                payload_json: "{}".to_string(),
                attempt_count: 1,
                claimed_by: None,
                heartbeat_at: None,
                next_attempt_at: None,
                last_error: None,
                created_at: current_timestamp(),
                updated_at: current_timestamp(),
            })
            .expect("completed operation inserted");
        let sync_calls = Arc::new(Mutex::new(0_u64));
        let sync_calls_for_executor = Arc::clone(&sync_calls);
        let mut runtime = ContinuousSyncRuntime {
            options: ContinuousSyncOptions {
                args: SyncOnceArgs {
                    root,
                    state_root: state_root.clone(),
                    workspace_id: workspace_id.as_str().to_string(),
                    device_id: "device-a".to_string(),
                    sync_operation_id: None,
                },
                interval: Duration::from_secs(3600),
                max_ticks: None,
            },
            next_tick: Instant::now(),
            next_remote_observe: Instant::now(),
            tick_count: 0,
            last_json: String::new(),
            watcher: None,
            change_rx: None,
            watcher_state: WatcherRuntimeState::Ready,
            sync_once: Box::new(move |_, _| {
                *sync_calls_for_executor
                    .lock()
                    .expect("sync call count lock") += 1;
                Err(runtime_error("idle poll must not run sync-once"))
            }),
            remote_ref_observer: noop_remote_ref_observer(),
            latest_observed_ref: None,
            status_publisher: noop_status_publisher(),
            next_status_publish: Instant::now() + STATUS_PUBLISH_INTERVAL,
        };

        runtime.poll();

        assert_eq!(
            *sync_calls.lock().expect("sync call count lock"),
            0,
            "idle daemon poll must not call hosted sync work"
        );
        assert_eq!(runtime.tick_count, 1);
        assert!(
            runtime.status_json().contains("\"state\":\"idle\""),
            "{}",
            runtime.status_json()
        );
        let operations = store
            .sync_operations(&workspace_id)
            .expect("operations read");
        assert_eq!(operations.len(), 1);
        assert_eq!(operations[0].state, "completed");

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn daemon_claims_reconcile_when_local_write_is_newer_than_completed_tick() {
        let temp = unique_temp_dir("bowline-daemon-local-write-reconcile");
        let state_root = temp.join(".state");
        let workspace_id = WorkspaceId::new("ws_local_write_reconcile");
        let device_id = DeviceId::new("device-test");
        let store =
            MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
        store
            .enqueue_sync_operation(&SyncOperationRecord {
                id: "daemon-sync-completed".to_string(),
                workspace_id: workspace_id.clone(),
                kind: "daemon-reconcile".to_string(),
                state: "completed".to_string(),
                idempotency_key: "daemon-sync:device-test:completed".to_string(),
                base_version: None,
                base_snapshot_id: None,
                target_snapshot_id: None,
                device_id: Some(device_id.clone()),
                payload_json: "{}".to_string(),
                attempt_count: 1,
                claimed_by: None,
                heartbeat_at: None,
                next_attempt_at: None,
                last_error: None,
                created_at: "2999-01-01T00:00:00Z".to_string(),
                updated_at: "2999-01-01T00:00:00Z".to_string(),
            })
            .expect("completed operation inserted");
        store
            .append_local_write_log(&LocalWriteLogRecord {
                id: "write-after-completed".to_string(),
                workspace_id: workspace_id.clone(),
                device_id,
                project_id: None,
                path: "apps/web/src/main.ts".to_string(),
                source_path: None,
                operation: "modify".to_string(),
                staged_content_id: None,
                policy_classification: PathClassification::WorkspaceSync,
                causation_id: "watch-test".to_string(),
                settled_at: "2999-01-01T00:00:01Z".to_string(),
                created_at: "2999-01-01T00:00:01Z".to_string(),
            })
            .expect("local write inserted");
        let runtime = watcher_test_runtime(
            temp.join("Code"),
            state_root.clone(),
            "ws_local_write_reconcile",
        );

        let claimed = runtime
            .claim_daemon_sync_operation()
            .expect("local write queues sync");

        assert_ne!(claimed, "daemon-sync-completed");
        let operations = store
            .sync_operations(&workspace_id)
            .expect("operations read");
        assert_eq!(operations.len(), 2);
        assert!(
            operations
                .iter()
                .any(|operation| operation.state == "claimed")
        );

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn daemon_claims_reconcile_when_remote_observer_advances_cursor() {
        let temp = unique_temp_dir("bowline-daemon-remote-observer-reconcile");
        let state_root = temp.join(".state");
        let workspace_id = WorkspaceId::new("ws_remote_observer_reconcile");
        let device_id = DeviceId::new("device-test");
        let store =
            MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
        store
            .enqueue_sync_operation(&SyncOperationRecord {
                id: "daemon-sync-completed".to_string(),
                workspace_id: workspace_id.clone(),
                kind: "daemon-reconcile".to_string(),
                state: "completed".to_string(),
                idempotency_key: "daemon-sync:device-test:completed".to_string(),
                base_version: None,
                base_snapshot_id: None,
                target_snapshot_id: None,
                device_id: Some(device_id),
                payload_json: "{}".to_string(),
                attempt_count: 1,
                claimed_by: None,
                heartbeat_at: None,
                next_attempt_at: None,
                last_error: None,
                created_at: "2999-01-01T00:00:00Z".to_string(),
                updated_at: "2999-01-01T00:00:00Z".to_string(),
            })
            .expect("completed operation inserted");
        store
            .upsert_workspace_sync_head(&WorkspaceSyncHeadRecord {
                workspace_ref: WorkspaceRef {
                    workspace_id: workspace_id.as_str().to_string(),
                    version: 1,
                    snapshot_id: "snap-local".to_string(),
                    updated_at: ControlPlaneTimestamp { tick: 1 },
                    updated_by_device_id: Some("device-a".to_string()),
                },
                observed_at: "2999-01-01T00:00:00Z".to_string(),
            })
            .expect("local head inserted");
        let mut runtime = watcher_test_runtime(
            temp.join("Code"),
            state_root.clone(),
            "ws_remote_observer_reconcile",
        );
        runtime.remote_ref_observer = Box::new(|_| {
            Ok(Some(WorkspaceRef {
                workspace_id: "ws_remote_observer_reconcile".to_string(),
                version: 2,
                snapshot_id: "snap-remote".to_string(),
                updated_at: ControlPlaneTimestamp { tick: 2 },
                updated_by_device_id: Some("device-b".to_string()),
            }))
        });

        assert!(runtime.observe_remote_ref_cursor());
        let claimed = runtime
            .claim_daemon_sync_operation()
            .expect("remote cursor advance queues sync");

        assert_ne!(claimed, "daemon-sync-completed");
        let cursor = store
            .remote_ref_cursor(&workspace_id)
            .expect("cursor reads")
            .expect("cursor exists");
        assert_eq!(cursor.last_observed_version, Some(2));
        assert_eq!(
            cursor.last_observed_snapshot_id.as_deref(),
            Some("snap-remote")
        );

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn daemon_clears_observed_base_ref_when_remote_observer_has_no_ref() {
        let temp = unique_temp_dir("bowline-daemon-clear-observed-ref");
        let state_root = temp.join(".state");
        let workspace_id = "ws_clear_observed_ref";
        let mut runtime = watcher_test_runtime(temp.join("Code"), state_root, workspace_id);
        runtime.latest_observed_ref = Some(WorkspaceRef {
            workspace_id: workspace_id.to_string(),
            version: 2,
            snapshot_id: "snap-stale".to_string(),
            updated_at: ControlPlaneTimestamp { tick: 2 },
            updated_by_device_id: Some("device-b".to_string()),
        });
        runtime.remote_ref_observer = Box::new(|_| Ok(None));

        assert!(!runtime.observe_remote_ref_cursor());
        assert_eq!(runtime.latest_observed_ref, None);

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn daemon_startup_requeues_own_claimed_tick_without_waiting_for_timeout() {
        let temp = unique_temp_dir("bowline-daemon-startup-requeue-claimed");
        let root = temp.join("Code");
        let state_root = temp.join(".state");
        fs::create_dir_all(&root).expect("root");
        let workspace_id = WorkspaceId::new("ws_startup_requeue_claimed");
        let store =
            MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
        store
            .enqueue_sync_operation(&SyncOperationRecord {
                id: "daemon-sync-before-restart".to_string(),
                workspace_id: workspace_id.clone(),
                kind: "daemon-reconcile".to_string(),
                state: "claimed".to_string(),
                idempotency_key: "daemon-sync:device-test:claimed-before-restart".to_string(),
                base_version: None,
                base_snapshot_id: None,
                target_snapshot_id: None,
                device_id: Some(DeviceId::new("device-test")),
                payload_json: "{}".to_string(),
                attempt_count: 1,
                claimed_by: Some("old-daemon-process".to_string()),
                heartbeat_at: Some("2999-01-01T00:00:00Z".to_string()),
                next_attempt_at: None,
                last_error: None,
                created_at: "2026-06-26T00:00:00Z".to_string(),
                updated_at: "2026-06-26T00:00:00Z".to_string(),
            })
            .expect("claimed operation inserted");

        let runtime = ContinuousSyncRuntime::new(ContinuousSyncOptions {
            args: SyncOnceArgs {
                root,
                state_root: state_root.clone(),
                workspace_id: workspace_id.as_str().to_string(),
                device_id: "device-test".to_string(),
                sync_operation_id: None,
            },
            interval: Duration::from_secs(60),
            max_ticks: None,
        });

        let operation = store
            .sync_operation_by_id("daemon-sync-before-restart")
            .expect("operation reads")
            .expect("operation exists");
        assert_eq!(operation.state, "queued");
        assert_eq!(operation.claimed_by, None);
        assert_eq!(operation.heartbeat_at, None);

        let claimed = runtime
            .claim_daemon_sync_operation()
            .expect("restarted daemon claims abandoned operation");
        assert_eq!(claimed, "daemon-sync-before-restart");
        let operations = store
            .sync_operations(&workspace_id)
            .expect("operations read");
        assert_eq!(operations.len(), 1);
        assert_eq!(operations[0].state, "claimed");
        assert_eq!(operations[0].claimed_by.as_deref(), Some("device-test"));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn daemon_startup_requeues_own_retry_after_restart() {
        let temp = unique_temp_dir("bowline-daemon-startup-requeue-retry");
        let root = temp.join("Code");
        let state_root = temp.join(".state");
        fs::create_dir_all(&root).expect("root");
        let workspace_id = WorkspaceId::new("ws_startup_requeue_retry");
        let store =
            MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
        store
            .enqueue_sync_operation(&SyncOperationRecord {
                id: "daemon-sync-before-repair".to_string(),
                workspace_id: workspace_id.clone(),
                kind: "daemon-reconcile".to_string(),
                state: "waiting_retry".to_string(),
                idempotency_key: "daemon-sync:device-test:retry-before-repair".to_string(),
                base_version: None,
                base_snapshot_id: None,
                target_snapshot_id: None,
                device_id: Some(DeviceId::new("device-test")),
                payload_json: "{}".to_string(),
                attempt_count: 4,
                claimed_by: None,
                heartbeat_at: None,
                next_attempt_at: Some("2999-01-01T00:00:00Z".to_string()),
                last_error: Some(
                    "daemon sync requires BOWLINE_ACCOUNT_SESSION_ID, BOWLINE_CONTROL_PLANE_TOKEN, or a stored account session"
                        .to_string(),
                ),
                created_at: "2026-06-26T00:00:00Z".to_string(),
                updated_at: "2026-06-26T00:00:00Z".to_string(),
            })
            .expect("retry operation inserted");

        let options = ContinuousSyncOptions {
            args: SyncOnceArgs {
                root,
                state_root: state_root.clone(),
                workspace_id: workspace_id.as_str().to_string(),
                device_id: "device-test".to_string(),
                sync_operation_id: None,
            },
            interval: Duration::from_secs(60),
            max_ticks: None,
        };
        requeue_startup_sync_claims_with_resolved_attention(&options, true, false);

        let operation = store
            .sync_operation_by_id("daemon-sync-before-repair")
            .expect("operation reads")
            .expect("operation exists");
        assert_eq!(operation.state, "queued");
        assert_eq!(operation.next_attempt_at, None);
        assert_eq!(
            operation.last_error.as_deref(),
            Some(
                "daemon sync requires BOWLINE_ACCOUNT_SESSION_ID, BOWLINE_CONTROL_PLANE_TOKEN, or a stored account session"
            )
        );

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn daemon_startup_requeues_resolved_missing_convex_attention() {
        let temp = unique_temp_dir("bowline-daemon-startup-requeue-attention");
        let root = temp.join("Code");
        let state_root = temp.join(".state");
        fs::create_dir_all(&root).expect("root");
        let workspace_id = WorkspaceId::new("ws_startup_requeue_attention");
        let store =
            MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
        store
            .enqueue_sync_operation(&SyncOperationRecord {
                id: "daemon-sync-missing-convex".to_string(),
                workspace_id: workspace_id.clone(),
                kind: "daemon-reconcile".to_string(),
                state: "attention".to_string(),
                idempotency_key: "daemon-sync:device-test:missing-convex".to_string(),
                base_version: None,
                base_snapshot_id: None,
                target_snapshot_id: None,
                device_id: Some(DeviceId::new("device-test")),
                payload_json: "{}".to_string(),
                attempt_count: 1,
                claimed_by: None,
                heartbeat_at: None,
                next_attempt_at: None,
                last_error: Some("CONVEX_URL is required for daemon sync".to_string()),
                created_at: "2026-06-26T00:00:00Z".to_string(),
                updated_at: "2026-06-26T00:00:00Z".to_string(),
            })
            .expect("attention operation inserted");

        let options = ContinuousSyncOptions {
            args: SyncOnceArgs {
                root,
                state_root: state_root.clone(),
                workspace_id: workspace_id.as_str().to_string(),
                device_id: "device-test".to_string(),
                sync_operation_id: None,
            },
            interval: Duration::from_secs(60),
            max_ticks: None,
        };
        requeue_startup_sync_claims_with_resolved_attention(&options, true, false);

        let operation = store
            .sync_operation_by_id("daemon-sync-missing-convex")
            .expect("operation reads")
            .expect("operation exists");
        assert_eq!(operation.state, "queued");
        assert_eq!(operation.last_error, None);
        assert_eq!(operation.next_attempt_at, None);

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn daemon_startup_requeues_resolved_missing_workspace_key_attention() {
        let temp = unique_temp_dir("bowline-daemon-startup-requeue-workspace-key");
        let root = temp.join("Code");
        let state_root = temp.join(".state");
        fs::create_dir_all(&root).expect("root");
        let workspace_id = WorkspaceId::new("ws_startup_requeue_workspace_key");
        let store =
            MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
        store
            .enqueue_sync_operation(&SyncOperationRecord {
                id: "daemon-sync-missing-workspace-key".to_string(),
                workspace_id: workspace_id.clone(),
                kind: "daemon-reconcile".to_string(),
                state: "attention".to_string(),
                idempotency_key: "daemon-sync:device-test:missing-workspace-key".to_string(),
                base_version: None,
                base_snapshot_id: None,
                target_snapshot_id: None,
                device_id: Some(DeviceId::new("device-test")),
                payload_json: "{}".to_string(),
                attempt_count: 1,
                claimed_by: None,
                heartbeat_at: None,
                next_attempt_at: None,
                last_error: Some("workspace key is missing; approve this device".to_string()),
                created_at: "2026-06-26T00:00:00Z".to_string(),
                updated_at: "2026-06-26T00:00:00Z".to_string(),
            })
            .expect("attention operation inserted");

        let options = ContinuousSyncOptions {
            args: SyncOnceArgs {
                root,
                state_root: state_root.clone(),
                workspace_id: workspace_id.as_str().to_string(),
                device_id: "device-test".to_string(),
                sync_operation_id: None,
            },
            interval: Duration::from_secs(60),
            max_ticks: None,
        };
        requeue_startup_sync_claims_with_resolved_attention(&options, false, true);

        let operation = store
            .sync_operation_by_id("daemon-sync-missing-workspace-key")
            .expect("operation reads")
            .expect("operation exists");
        assert_eq!(operation.state, "queued");
        assert_eq!(operation.last_error, None);
        assert_eq!(operation.next_attempt_at, None);

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn missing_remote_bytes_are_reported_as_offline_sync_work() {
        assert_eq!(
            sync_failure_action("snapshot manifest `snap_missing` was not found"),
            SyncFailureAction::Offline
        );
        assert_eq!(
            sync_failure_action("missing object for object `packs_pk_missing`"),
            SyncFailureAction::Offline
        );
        assert_eq!(
            sync_failure_action(
                "R2 download for object `packs_pk_missing` returned HTTP 404 Not Found"
            ),
            SyncFailureAction::Offline
        );
        assert_eq!(
            sync_failure_action(
                "corrupt object `packs_pk_bad`: object bytes did not match metadata"
            ),
            SyncFailureAction::Retry
        );
        assert_eq!(
            sync_failure_action("CONVEX_URL is required for daemon sync"),
            SyncFailureAction::Attention
        );
    }

    #[test]
    fn retry_backoff_is_bounded_and_increases() {
        let first = retry_delay_seconds("op-retry", 1);
        let second = retry_delay_seconds("op-retry", 2);
        let late = retry_delay_seconds("op-retry", 99);

        assert!((2..=5).contains(&first));
        assert!(second >= first);
        assert_eq!(late, 60);
    }

    #[test]
    fn remote_observer_reconnect_backoff_is_bounded() {
        assert_eq!(remote_observer_reconnect_delay(1), Duration::from_secs(30));
        assert_eq!(remote_observer_reconnect_delay(2), Duration::from_secs(60));
        assert_eq!(
            remote_observer_reconnect_delay(99),
            Duration::from_secs(900)
        );
    }

    #[test]
    fn daemon_routes_missing_remote_bytes_to_offline_queue_state() {
        let temp = unique_temp_dir("bowline-daemon-missing-remote");
        let state_root = temp.join(".state");
        let workspace_id = WorkspaceId::new("ws_missing_remote");
        let operation_id = "op-missing-remote";
        let store =
            MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
        store
            .enqueue_sync_operation(&SyncOperationRecord {
                id: operation_id.to_string(),
                workspace_id: workspace_id.clone(),
                kind: "download".to_string(),
                state: "claimed".to_string(),
                idempotency_key: "missing-remote".to_string(),
                base_version: Some(1),
                base_snapshot_id: Some("snap-1".to_string()),
                target_snapshot_id: Some("snap-2".to_string()),
                device_id: Some(DeviceId::new("device-a")),
                payload_json: "{}".to_string(),
                attempt_count: 1,
                claimed_by: Some("daemon-test".to_string()),
                heartbeat_at: Some("2026-06-26T12:00:00Z".to_string()),
                next_attempt_at: None,
                last_error: None,
                created_at: "2026-06-26T12:00:00Z".to_string(),
                updated_at: "2026-06-26T12:00:00Z".to_string(),
            })
            .expect("operation queued");

        let runtime = ContinuousSyncRuntime {
            options: ContinuousSyncOptions {
                args: SyncOnceArgs {
                    root: temp.join("Code"),
                    state_root: state_root.clone(),
                    workspace_id: workspace_id.as_str().to_string(),
                    device_id: "device-a".to_string(),
                    sync_operation_id: None,
                },
                interval: Duration::from_secs(60),
                max_ticks: None,
            },
            next_tick: Instant::now(),
            next_remote_observe: Instant::now(),
            tick_count: 0,
            last_json: String::new(),
            watcher: None,
            change_rx: None,
            watcher_state: WatcherRuntimeState::Ready,
            sync_once: hosted_sync_executor(),
            remote_ref_observer: noop_remote_ref_observer(),
            latest_observed_ref: None,
            status_publisher: noop_status_publisher(),
            next_status_publish: Instant::now() + STATUS_PUBLISH_INTERVAL,
        };

        let before = OffsetDateTime::now_utc();
        runtime.fail_daemon_sync_operation(
            operation_id,
            "snapshot manifest `snap_missing` was not found",
        );

        let counts = store
            .sync_operation_counts(&workspace_id)
            .expect("counts read");
        assert_eq!(counts.blocked_offline, 1);
        let operation = store
            .sync_operation_by_id(operation_id)
            .expect("operation reads")
            .expect("operation exists");
        assert_eq!(operation.state, "blocked_offline");
        let next_attempt = OffsetDateTime::parse(
            operation
                .next_attempt_at
                .as_deref()
                .expect("offline retry time is set"),
            &time::format_description::well_known::Rfc3339,
        )
        .expect("offline retry time parses");
        assert!(next_attempt > before);
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn daemon_does_not_bypass_pending_backoff_with_fresh_reconcile_rows() {
        let temp = unique_temp_dir("bowline-daemon-no-backoff-bypass");
        let state_root = temp.join(".state");
        let workspace_id = WorkspaceId::new("ws_no_backoff_bypass");
        let store =
            MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
        store
            .enqueue_sync_operation(&SyncOperationRecord {
                id: "op-blocked".to_string(),
                workspace_id: workspace_id.clone(),
                kind: "daemon-reconcile".to_string(),
                state: "blocked_offline".to_string(),
                idempotency_key: "blocked-reconcile".to_string(),
                base_version: None,
                base_snapshot_id: None,
                target_snapshot_id: None,
                device_id: Some(DeviceId::new("device-a")),
                payload_json: "{}".to_string(),
                attempt_count: 1,
                claimed_by: None,
                heartbeat_at: None,
                next_attempt_at: Some("2999-01-01T00:00:00Z".to_string()),
                last_error: Some("offline".to_string()),
                created_at: "2026-06-26T12:00:00Z".to_string(),
                updated_at: "2026-06-26T12:00:00Z".to_string(),
            })
            .expect("operation queued");

        let runtime = ContinuousSyncRuntime {
            options: ContinuousSyncOptions {
                args: SyncOnceArgs {
                    root: temp.join("Code"),
                    state_root: state_root.clone(),
                    workspace_id: workspace_id.as_str().to_string(),
                    device_id: "device-a".to_string(),
                    sync_operation_id: None,
                },
                interval: Duration::from_secs(60),
                max_ticks: None,
            },
            next_tick: Instant::now(),
            next_remote_observe: Instant::now(),
            tick_count: 42,
            last_json: String::new(),
            watcher: None,
            change_rx: None,
            watcher_state: WatcherRuntimeState::Ready,
            sync_once: hosted_sync_executor(),
            remote_ref_observer: noop_remote_ref_observer(),
            latest_observed_ref: None,
            status_publisher: noop_status_publisher(),
            next_status_publish: Instant::now() + STATUS_PUBLISH_INTERVAL,
        };

        assert_eq!(runtime.claim_daemon_sync_operation(), None);
        let operations = store
            .sync_operations(&workspace_id)
            .expect("operations read");
        assert_eq!(operations.len(), 1);
        assert_eq!(operations[0].id, "op-blocked");
        assert_eq!(operations[0].state, "blocked_offline");

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn daemon_poll_waits_for_backoff_instead_of_running_sync_once() {
        let temp = unique_temp_dir("bowline-daemon-poll-backoff");
        let root = temp.join("Code");
        let state_root = temp.join(".state");
        fs::create_dir_all(&root).expect("root");
        let workspace_id = WorkspaceId::new("ws_poll_backoff");
        let store =
            MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
        store
            .enqueue_sync_operation(&SyncOperationRecord {
                id: "op-blocked".to_string(),
                workspace_id: workspace_id.clone(),
                kind: "daemon-reconcile".to_string(),
                state: "blocked_offline".to_string(),
                idempotency_key: "blocked-reconcile".to_string(),
                base_version: None,
                base_snapshot_id: None,
                target_snapshot_id: None,
                device_id: Some(DeviceId::new("device-a")),
                payload_json: "{}".to_string(),
                attempt_count: 1,
                claimed_by: None,
                heartbeat_at: None,
                next_attempt_at: Some("2999-01-01T00:00:00Z".to_string()),
                last_error: Some("offline".to_string()),
                created_at: "2026-06-26T12:00:00Z".to_string(),
                updated_at: "2026-06-26T12:00:00Z".to_string(),
            })
            .expect("operation queued");

        let mut runtime = ContinuousSyncRuntime {
            options: ContinuousSyncOptions {
                args: SyncOnceArgs {
                    root,
                    state_root: state_root.clone(),
                    workspace_id: workspace_id.as_str().to_string(),
                    device_id: "device-a".to_string(),
                    sync_operation_id: None,
                },
                interval: Duration::from_secs(60),
                max_ticks: None,
            },
            next_tick: Instant::now(),
            next_remote_observe: Instant::now(),
            tick_count: 0,
            last_json: String::new(),
            watcher: None,
            change_rx: None,
            watcher_state: WatcherRuntimeState::Ready,
            sync_once: hosted_sync_executor(),
            remote_ref_observer: noop_remote_ref_observer(),
            latest_observed_ref: None,
            status_publisher: noop_status_publisher(),
            next_status_publish: Instant::now() + STATUS_PUBLISH_INTERVAL,
        };

        runtime.poll();

        assert!(runtime.status_json().contains("\"state\":\"limited\""));
        assert!(
            runtime
                .status_json()
                .contains("sync queue is waiting for offline recovery")
        );
        let operations = store
            .sync_operations(&workspace_id)
            .expect("operations read");
        assert_eq!(operations.len(), 1);
        assert_eq!(operations[0].state, "blocked_offline");
        assert_eq!(operations[0].last_error.as_deref(), Some("offline"));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn daemon_poll_reports_attention_queue_truthfully() {
        let temp = unique_temp_dir("bowline-daemon-poll-attention");
        let root = temp.join("Code");
        let state_root = temp.join(".state");
        fs::create_dir_all(&root).expect("root");
        let workspace_id = WorkspaceId::new("ws_poll_attention");
        let store =
            MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
        store
            .enqueue_sync_operation(&SyncOperationRecord {
                id: "op-attention".to_string(),
                workspace_id: workspace_id.clone(),
                kind: "daemon-reconcile".to_string(),
                state: "attention".to_string(),
                idempotency_key: "attention-reconcile".to_string(),
                base_version: None,
                base_snapshot_id: None,
                target_snapshot_id: None,
                device_id: Some(DeviceId::new("device-a")),
                payload_json: "{}".to_string(),
                attempt_count: 1,
                claimed_by: None,
                heartbeat_at: None,
                next_attempt_at: None,
                last_error: Some("trusted device required".to_string()),
                created_at: "2026-06-26T12:00:00Z".to_string(),
                updated_at: "2026-06-26T12:00:00Z".to_string(),
            })
            .expect("operation queued");

        let mut runtime = ContinuousSyncRuntime {
            options: ContinuousSyncOptions {
                args: SyncOnceArgs {
                    root,
                    state_root: state_root.clone(),
                    workspace_id: workspace_id.as_str().to_string(),
                    device_id: "device-a".to_string(),
                    sync_operation_id: None,
                },
                interval: Duration::from_secs(60),
                max_ticks: None,
            },
            next_tick: Instant::now(),
            next_remote_observe: Instant::now(),
            tick_count: 0,
            last_json: String::new(),
            watcher: None,
            change_rx: None,
            watcher_state: WatcherRuntimeState::Ready,
            sync_once: hosted_sync_executor(),
            remote_ref_observer: noop_remote_ref_observer(),
            latest_observed_ref: None,
            status_publisher: noop_status_publisher(),
            next_status_publish: Instant::now() + STATUS_PUBLISH_INTERVAL,
        };

        runtime.poll();

        assert!(runtime.status_json().contains("\"state\":\"attention\""));
        assert!(runtime.status_json().contains("sync queue needs attention"));
        assert!(
            runtime
                .status_json()
                .contains("\"blockedAction\":\"resolve sync queue attention\"")
        );

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn daemon_retry_failures_wait_before_next_attempt() {
        let temp = unique_temp_dir("bowline-daemon-retry-backoff");
        let state_root = temp.join(".state");
        let workspace_id = WorkspaceId::new("ws_retry_backoff");
        let operation_id = "op-retry-backoff";
        let store =
            MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
        store
            .enqueue_sync_operation(&SyncOperationRecord {
                id: operation_id.to_string(),
                workspace_id: workspace_id.clone(),
                kind: "upload".to_string(),
                state: "claimed".to_string(),
                idempotency_key: "retry-backoff".to_string(),
                base_version: Some(1),
                base_snapshot_id: Some("snap-1".to_string()),
                target_snapshot_id: Some("snap-2".to_string()),
                device_id: Some(DeviceId::new("device-a")),
                payload_json: "{}".to_string(),
                attempt_count: 3,
                claimed_by: Some("daemon-test".to_string()),
                heartbeat_at: Some("2026-06-26T12:00:00Z".to_string()),
                next_attempt_at: None,
                last_error: None,
                created_at: "2026-06-26T12:00:00Z".to_string(),
                updated_at: "2026-06-26T12:00:00Z".to_string(),
            })
            .expect("operation queued");

        let runtime = ContinuousSyncRuntime {
            options: ContinuousSyncOptions {
                args: SyncOnceArgs {
                    root: temp.join("Code"),
                    state_root: state_root.clone(),
                    workspace_id: workspace_id.as_str().to_string(),
                    device_id: "device-a".to_string(),
                    sync_operation_id: None,
                },
                interval: Duration::from_secs(60),
                max_ticks: None,
            },
            next_tick: Instant::now(),
            next_remote_observe: Instant::now(),
            tick_count: 0,
            last_json: String::new(),
            watcher: None,
            change_rx: None,
            watcher_state: WatcherRuntimeState::Ready,
            sync_once: hosted_sync_executor(),
            remote_ref_observer: noop_remote_ref_observer(),
            latest_observed_ref: None,
            status_publisher: noop_status_publisher(),
            next_status_publish: Instant::now() + STATUS_PUBLISH_INTERVAL,
        };

        let before = OffsetDateTime::now_utc();
        runtime.fail_daemon_sync_operation(
            operation_id,
            "corrupt object `packs_pk_bad`: object bytes did not match metadata",
        );

        let operation = store
            .sync_operation_by_id(operation_id)
            .expect("operation reads")
            .expect("operation exists");
        assert_eq!(operation.state, "waiting_retry");
        let events = store.list_events(20).expect("events read");
        let event = events
            .iter()
            .find(|event| event.name == EventName::SyncLimited)
            .expect("sync limited event");
        assert_eq!(event.payload["outcome"], "retry");
        assert!(
            !serde_json::to_string(event)
                .expect("event json")
                .contains("corrupt object"),
            "sync event must not include raw error text"
        );
        let next_attempt = OffsetDateTime::parse(
            operation
                .next_attempt_at
                .as_deref()
                .expect("retry time is set"),
            &time::format_description::well_known::Rfc3339,
        )
        .expect("retry time parses");
        assert!(next_attempt > before + time::Duration::seconds(7));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn two_fake_daemon_loops_sync_edit_without_manual_sync_once() {
        let temp = unique_temp_dir("bowline-daemon-two-loop");
        let workspace_id = "ws_two_daemon_loop";
        let a_root = temp.join("device-a").join("Code");
        let b_root = temp.join("device-b").join("Code");
        let a_state = temp.join("device-a").join("state");
        let b_state = temp.join("device-b").join("state");
        let note_path = PathBuf::from("project/notes/loop.txt");
        fs::create_dir_all(a_root.join("project/notes")).expect("a project dirs");
        fs::create_dir_all(&b_root).expect("b root");
        fs::write(a_root.join(&note_path), "initial daemon loop\n").expect("initial file");

        let control_plane = Arc::new(Mutex::new(FakeControlPlaneClient::default()));
        let byte_store = Arc::new(
            LocalByteStore::open_deterministic(temp.join("objects"), 41).expect("byte store"),
        );
        let workspace_key = [41_u8; 32];
        let mut daemon_a = fake_daemon_runtime(
            a_root.clone(),
            a_state.clone(),
            workspace_id,
            "device-a",
            Arc::clone(&control_plane),
            Arc::clone(&byte_store),
            workspace_key,
        );
        let mut daemon_b = fake_daemon_runtime(
            b_root.clone(),
            b_state.clone(),
            workspace_id,
            "device-b",
            Arc::clone(&control_plane),
            Arc::clone(&byte_store),
            workspace_key,
        );

        poll_until(
            &mut daemon_a,
            |runtime| sync_status_version(runtime) >= 1,
            "device A initial upload",
        );
        poll_until(
            &mut daemon_b,
            |_| file_contains(&b_root.join(&note_path), "initial daemon loop"),
            "device B initial materialization",
        );

        fs::write(
            a_root.join(&note_path),
            "initial daemon loop\nlive edit from daemon A\n",
        )
        .expect("edit file");

        poll_until(
            &mut daemon_a,
            |runtime| sync_status_version(runtime) >= 2,
            "device A edit upload",
        );
        poll_until(
            &mut daemon_b,
            |_| file_contains(&b_root.join(&note_path), "live edit from daemon A"),
            "device B edit materialization",
        );

        assert!(
            daemon_a.status_json().contains("\"state\":\"idle\""),
            "{}",
            daemon_a.status_json()
        );
        assert!(
            daemon_b.status_json().contains("\"state\":\"idle\""),
            "{}",
            daemon_b.status_json()
        );
        let a_checkpoints = checkpoint_steps(&a_state);
        for expected in [
            "snapshot-candidate-built",
            "source-pack-uploaded",
            "snapshot-manifest-uploaded",
            "object-manifest-committed",
            "workspace-ref-advanced",
        ] {
            assert!(
                a_checkpoints.iter().any(|step| step == expected),
                "missing device A checkpoint {expected}; got {a_checkpoints:?}"
            );
        }
        let b_checkpoints = checkpoint_steps(&b_state);
        for expected in ["remote-import-started", "remote-materialized"] {
            assert!(
                b_checkpoints.iter().any(|step| step == expected),
                "missing device B checkpoint {expected}; got {b_checkpoints:?}"
            );
        }

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn restarted_daemon_reconciles_real_directory_edit_without_data_loss() {
        let temp = unique_temp_dir("bowline-daemon-restart-real-root-edit");
        let workspace_id = "ws_two_daemon_loop_restart";
        let a_root = temp.join("device-a").join("Code");
        let b_root = temp.join("device-b").join("Code");
        let a_state = temp.join("device-a").join("state");
        let b_state = temp.join("device-b").join("state");
        let note_path = PathBuf::from("project/notes/restart.txt");
        fs::create_dir_all(a_root.join("project/notes")).expect("a project dirs");
        fs::create_dir_all(&b_root).expect("b root");
        fs::write(a_root.join(&note_path), "initial before restart\n").expect("initial file");

        let control_plane = Arc::new(Mutex::new(FakeControlPlaneClient::default()));
        let byte_store = Arc::new(
            LocalByteStore::open_deterministic(temp.join("objects"), 43).expect("byte store"),
        );
        let workspace_key = [43_u8; 32];
        let mut daemon_a = fake_daemon_runtime(
            a_root.clone(),
            a_state.clone(),
            workspace_id,
            "device-a",
            Arc::clone(&control_plane),
            Arc::clone(&byte_store),
            workspace_key,
        );
        let mut daemon_b = fake_daemon_runtime(
            b_root.clone(),
            b_state,
            workspace_id,
            "device-b",
            Arc::clone(&control_plane),
            Arc::clone(&byte_store),
            workspace_key,
        );

        poll_until(
            &mut daemon_a,
            |runtime| sync_status_version(runtime) >= 1,
            "device A initial upload",
        );
        poll_until(
            &mut daemon_b,
            |_| file_contains(&b_root.join(&note_path), "initial before restart"),
            "device B initial materialization",
        );

        drop(daemon_a);
        fs::write(
            a_root.join(&note_path),
            "initial before restart\nedit while daemon was down\n",
        )
        .expect("edit real file while daemon is down");
        let mut restarted_daemon_a = fake_daemon_runtime(
            a_root.clone(),
            a_state,
            workspace_id,
            "device-a",
            Arc::clone(&control_plane),
            Arc::clone(&byte_store),
            workspace_key,
        );

        poll_until(
            &mut restarted_daemon_a,
            |runtime| sync_status_version(runtime) >= 2,
            "restarted device A upload",
        );
        poll_until(
            &mut daemon_b,
            |_| file_contains(&b_root.join(&note_path), "edit while daemon was down"),
            "device B materializes edit from restarted daemon",
        );

        assert!(
            file_contains(&a_root.join(&note_path), "edit while daemon was down"),
            "restarted sync must never roll back the local real-directory edit"
        );
        assert!(
            restarted_daemon_a
                .status_json()
                .contains("\"state\":\"idle\""),
            "{}",
            restarted_daemon_a.status_json()
        );
        assert!(
            daemon_b.status_json().contains("\"state\":\"idle\""),
            "{}",
            daemon_b.status_json()
        );

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn restarted_daemon_adopts_materialized_remote_head_without_reupload() {
        let temp = unique_temp_dir("bowline-daemon-restart-adopt-materialized");
        let workspace_id = "ws_two_daemon_loop_adopt";
        let a_root = temp.join("device-a").join("Code");
        let b_root = temp.join("device-b").join("Code");
        let a_state = temp.join("device-a").join("state");
        let b_state = temp.join("device-b").join("state");
        let note_path = PathBuf::from("project/notes/adopt.txt");
        fs::create_dir_all(a_root.join("project/notes")).expect("a project dirs");
        fs::create_dir_all(&b_root).expect("b root");
        fs::write(a_root.join(&note_path), "remote materialized bytes\n").expect("initial file");

        let control_plane = Arc::new(Mutex::new(FakeControlPlaneClient::default()));
        let byte_store = Arc::new(
            LocalByteStore::open_deterministic(temp.join("objects"), 44).expect("byte store"),
        );
        let workspace_key = [44_u8; 32];
        let mut daemon_a = fake_daemon_runtime(
            a_root.clone(),
            a_state,
            workspace_id,
            "device-a",
            Arc::clone(&control_plane),
            Arc::clone(&byte_store),
            workspace_key,
        );
        let mut daemon_b = fake_daemon_runtime(
            b_root.clone(),
            b_state.clone(),
            workspace_id,
            "device-b",
            Arc::clone(&control_plane),
            Arc::clone(&byte_store),
            workspace_key,
        );

        poll_until(
            &mut daemon_a,
            |runtime| sync_status_version(runtime) >= 1,
            "device A initial upload",
        );
        poll_until(
            &mut daemon_b,
            |_| file_contains(&b_root.join(&note_path), "remote materialized bytes"),
            "device B initial materialization",
        );
        let remote_before = control_plane
            .lock()
            .expect("fake control plane lock")
            .get_workspace_ref(workspace_id)
            .expect("remote ref reads")
            .expect("remote ref exists");
        assert_eq!(remote_before.version, 1);

        drop(daemon_b);
        fs::remove_file(b_state.join(DEFAULT_DATABASE_FILE)).expect("remove local metadata db");
        let mut restarted_daemon_b = fake_daemon_runtime(
            b_root.clone(),
            b_state.clone(),
            workspace_id,
            "device-b",
            Arc::clone(&control_plane),
            Arc::clone(&byte_store),
            workspace_key,
        );

        poll_until(
            &mut restarted_daemon_b,
            |runtime| sync_status_version(runtime) >= 1,
            "restarted device B adopts materialized remote head",
        );

        let remote_after = control_plane
            .lock()
            .expect("fake control plane lock")
            .get_workspace_ref(workspace_id)
            .expect("remote ref reads")
            .expect("remote ref exists");
        assert_eq!(
            remote_after.version, remote_before.version,
            "materialized remote bytes must not be uploaded as a new workspace version"
        );
        assert_eq!(remote_after.snapshot_id, remote_before.snapshot_id);
        let recovered_store =
            MetadataStore::open(b_state.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
        let recovered_head = recovered_store
            .workspace_sync_head(&WorkspaceId::new(workspace_id))
            .expect("head reads")
            .expect("head restored");
        assert_eq!(
            recovered_head.workspace_ref.snapshot_id,
            remote_before.snapshot_id
        );
        assert_eq!(recovered_head.workspace_ref.version, remote_before.version);
        assert!(
            file_contains(&b_root.join(&note_path), "remote materialized bytes"),
            "restart must preserve the real-directory bytes"
        );

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn two_fake_daemon_loops_sync_safe_save_without_temp_churn() {
        let temp = unique_temp_dir("bowline-daemon-two-loop-safe-save");
        let workspace_id = "ws_two_daemon_loop_safe_save";
        let a_root = temp.join("device-a").join("Code");
        let b_root = temp.join("device-b").join("Code");
        let a_state = temp.join("device-a").join("state");
        let b_state = temp.join("device-b").join("state");
        let note_path = PathBuf::from("project/notes/safe-save.txt");
        let temp_path = PathBuf::from("project/notes/.safe-save.txt.tmp");
        fs::create_dir_all(a_root.join("project/notes")).expect("a project dirs");
        fs::create_dir_all(&b_root).expect("b root");
        fs::write(a_root.join(&note_path), "initial safe save\n").expect("initial file");

        let control_plane = Arc::new(Mutex::new(FakeControlPlaneClient::default()));
        let byte_store = Arc::new(
            LocalByteStore::open_deterministic(temp.join("objects"), 42).expect("byte store"),
        );
        let workspace_key = [42_u8; 32];
        let mut daemon_a = fake_daemon_runtime(
            a_root.clone(),
            a_state,
            workspace_id,
            "device-a",
            Arc::clone(&control_plane),
            Arc::clone(&byte_store),
            workspace_key,
        );
        let mut daemon_b = fake_daemon_runtime(
            b_root.clone(),
            b_state,
            workspace_id,
            "device-b",
            Arc::clone(&control_plane),
            Arc::clone(&byte_store),
            workspace_key,
        );

        poll_until(
            &mut daemon_a,
            |runtime| sync_status_version(runtime) >= 1,
            "device A initial upload",
        );
        poll_until(
            &mut daemon_b,
            |_| file_contains(&b_root.join(&note_path), "initial safe save"),
            "device B initial materialization",
        );

        fs::write(a_root.join(&temp_path), "safe-save final bytes\n").expect("temp write");
        fs::rename(a_root.join(&temp_path), a_root.join(&note_path)).expect("safe-save rename");

        poll_until(
            &mut daemon_a,
            |runtime| sync_status_version(runtime) >= 2,
            "device A safe-save upload",
        );
        poll_until(
            &mut daemon_b,
            |_| file_contains(&b_root.join(&note_path), "safe-save final bytes"),
            "device B safe-save materialization",
        );

        assert!(
            !b_root.join(&temp_path).exists(),
            "safe-save temp path should not materialize remotely"
        );

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn parses_status_json() {
        let cli = parse_args(["status", "--json"]);

        assert!(cli.json);
        assert_eq!(cli.command, Command::Status);
    }

    fn watcher_test_runtime(
        root: PathBuf,
        state_root: PathBuf,
        workspace_id: &str,
    ) -> ContinuousSyncRuntime {
        ContinuousSyncRuntime {
            options: ContinuousSyncOptions {
                args: SyncOnceArgs {
                    root,
                    state_root,
                    workspace_id: workspace_id.to_string(),
                    device_id: "device-test".to_string(),
                    sync_operation_id: None,
                },
                interval: Duration::from_secs(60),
                max_ticks: None,
            },
            next_tick: Instant::now(),
            next_remote_observe: Instant::now(),
            tick_count: 0,
            last_json: String::new(),
            watcher: None,
            change_rx: None,
            watcher_state: WatcherRuntimeState::Ready,
            sync_once: hosted_sync_executor(),
            remote_ref_observer: noop_remote_ref_observer(),
            latest_observed_ref: None,
            status_publisher: noop_status_publisher(),
            next_status_publish: Instant::now() + STATUS_PUBLISH_INTERVAL,
        }
    }

    fn fake_daemon_runtime(
        root: PathBuf,
        state_root: PathBuf,
        workspace_id: &str,
        device_id: &str,
        control_plane: Arc<Mutex<FakeControlPlaneClient>>,
        byte_store: Arc<LocalByteStore>,
        workspace_key: [u8; 32],
    ) -> ContinuousSyncRuntime {
        ContinuousSyncRuntime {
            options: ContinuousSyncOptions {
                args: SyncOnceArgs {
                    root,
                    state_root,
                    workspace_id: workspace_id.to_string(),
                    device_id: device_id.to_string(),
                    sync_operation_id: None,
                },
                interval: Duration::from_millis(0),
                max_ticks: None,
            },
            next_tick: Instant::now(),
            next_remote_observe: Instant::now(),
            tick_count: 0,
            last_json: String::new(),
            watcher: None,
            change_rx: None,
            watcher_state: WatcherRuntimeState::Ready,
            sync_once: fake_sync_executor(control_plane, byte_store, workspace_key),
            remote_ref_observer: noop_remote_ref_observer(),
            latest_observed_ref: None,
            status_publisher: noop_status_publisher(),
            next_status_publish: Instant::now() + STATUS_PUBLISH_INTERVAL,
        }
    }

    fn fake_sync_executor(
        control_plane: Arc<Mutex<FakeControlPlaneClient>>,
        byte_store: Arc<LocalByteStore>,
        workspace_key: [u8; 32],
    ) -> SyncExecutor {
        Box::new(move |args, observed_base_ref| {
            let workspace_id = WorkspaceId::new(args.workspace_id.clone());
            let device_id = DeviceId::new(args.device_id.clone());
            let control_plane = control_plane
                .lock()
                .map_err(|_| runtime_error("fake control plane lock poisoned"))?;
            let base_ref = match observed_base_ref {
                Some(workspace_ref) => workspace_ref,
                None => match control_plane.get_workspace_ref(workspace_id.as_str())? {
                    Some(workspace_ref) => workspace_ref,
                    None => control_plane.create_workspace_ref(workspace_id.as_str())?,
                },
            };
            run_sync_once_with(
                args,
                &*control_plane,
                &*byte_store,
                base_ref,
                workspace_id,
                device_id,
                workspace_key,
            )
        })
    }

    fn poll_until(
        runtime: &mut ContinuousSyncRuntime,
        condition: impl Fn(&ContinuousSyncRuntime) -> bool,
        label: &str,
    ) {
        for _ in 0..20 {
            runtime.next_tick = Instant::now();
            runtime.poll();
            if condition(runtime) {
                return;
            }
        }
        panic!(
            "{label} did not complete; last status {}",
            runtime.status_json()
        );
    }

    fn sync_status_version(runtime: &ContinuousSyncRuntime) -> u64 {
        serde_json::from_str::<serde_json::Value>(runtime.status_json())
            .ok()
            .and_then(|value| value["version"].as_u64())
            .unwrap_or_default()
    }

    fn file_contains(path: &std::path::Path, needle: &str) -> bool {
        fs::read_to_string(path).is_ok_and(|content| content.contains(needle))
    }

    fn checkpoint_steps(state_root: &Path) -> Vec<String> {
        let store = MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE))
            .expect("daemon metadata opens");
        store
            .sync_operations(&WorkspaceId::new("ws_two_daemon_loop"))
            .expect("sync operations")
            .into_iter()
            .flat_map(|operation| {
                store
                    .sync_operation_checkpoints(&operation.id)
                    .expect("checkpoints")
                    .into_iter()
                    .map(|checkpoint| checkpoint.step)
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    struct WatcherFixture {
        temp: PathBuf,
        root: PathBuf,
        state_root: PathBuf,
        workspace_id: WorkspaceId,
        store: MetadataStore,
    }

    fn watcher_fixture(label: &str, workspace_id: &str) -> WatcherFixture {
        let temp = unique_temp_dir(label);
        let root = temp.join("Code");
        let state_root = temp.join(".state");
        fs::create_dir_all(&root).expect("root dir");
        let workspace_id = WorkspaceId::new(workspace_id);
        let store =
            MetadataStore::open(state_root.join(DEFAULT_DATABASE_FILE)).expect("metadata opens");
        store
            .insert_workspace(&workspace_id, "Code", "2026-06-26T12:00:00Z")
            .expect("workspace");
        store
            .insert_root(
                "root-code",
                &workspace_id,
                &root.display().to_string(),
                "2026-06-26T12:00:00Z",
            )
            .expect("root");
        WatcherFixture {
            temp,
            root,
            state_root,
            workspace_id,
            store,
        }
    }

    fn unique_temp_dir(label: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("{label}-{suffix}"));
        fs::create_dir_all(&path).expect("temp dir");
        path
    }
}
