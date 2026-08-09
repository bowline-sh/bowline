use std::io::Write;

use super::server_state::ProjectStatusScope;

use crate::daemon::{DaemonServerState, StatusSubscription};
use bowline_core::ids::WorkspaceId;
use std::collections::HashMap;
use std::io;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

mod connection_pump;
mod deletions;
mod device_actions;
mod method_registry;
mod request_context;
mod rpc_executor;
mod work_views;

use bowline_core::wire::generated::{
    DaemonClientHello, DaemonRpcError, DaemonRpcErrorCode, DaemonRpcRequest, DaemonRpcResponse,
    DaemonStatusScopeParams, DaemonStatusSnapshotResult, DaemonStatusSubscribeResult,
    DaemonSyncBarrierConvergenceSource, DaemonSyncBarrierCoordinatorFrontier,
    DaemonSyncBarrierDarwinCoverage, DaemonSyncBarrierDarwinCoverageStart,
    DaemonSyncBarrierDarwinCursorReplay, DaemonSyncBarrierDarwinFreshStream,
    DaemonSyncBarrierEngineConvergence, DaemonSyncBarrierLinuxCoverage,
    DaemonSyncBarrierNativeCoverage, DaemonSyncBarrierNativeLoss,
    DaemonSyncBarrierObserverConvergence, DaemonSyncBarrierObserverLiveness,
    DaemonSyncBarrierObserverState, DaemonSyncBarrierProcessIdentity,
    DaemonSyncBarrierRecoveryClosure, DaemonSyncBarrierRecoveryLifecycle,
    DaemonSyncBarrierRefIdentity, DaemonSyncBarrierResult, DaemonSyncBarrierWorkspaceState,
    DaemonSyncBarrierWorkspaceStatus,
};
use bowline_core::wire::{StatusTransportError, status_command_to_wire};
use bowline_daemon_rpc::{
    CodecError, DEFAULT_MAX_FRAME_BYTES, FrameCodec, ServerNegotiation, negotiate,
};
use crossbeam_channel::Sender;
use method_registry::{RpcMethod, supported_capabilities};
use request_context::{CancellationPoint, RequestContext, RequestContextError};
use rpc_executor::RequestRouter;
pub(super) use rpc_executor::{RpcExecutor, RpcExecutorConfig, RpcExecutorMetricsSnapshot};

const HELLO_IO_TIMEOUT: Duration = Duration::from_secs(2);

/// The longest convergence wait the daemon will hold an RPC worker for. A
/// barrier occupies one bounded query-lane worker, and the client clamps its own
/// deadline well below an hour, so accepting the caller's arbitrary timeout only
/// ever parked workers past the point anyone was still listening.
const SYNC_BARRIER_MAX_TIMEOUT: Duration = Duration::from_secs(60);

type RpcResult<T> = Result<T, Box<DaemonRpcError>>;

pub(super) fn handle_rpc_connection(
    mut stream: UnixStream,
    state: &Arc<DaemonServerState>,
    socket_owner_uid: Option<u32>,
    executor: Arc<RpcExecutor>,
) -> io::Result<()> {
    let Some(codec) = negotiate_v2_connection(&mut stream, state)? else {
        return Ok(());
    };
    stream.set_read_timeout(None)?;
    let peer_credential_checked =
        super::socket_server::local_peer_credential_checked(&stream, socket_owner_uid);
    let request_state = Arc::clone(state);
    let request_router: Arc<RequestRouter> = Arc::new(move |context, request| {
        route_request(&context, request, &request_state, peer_credential_checked)
    });
    let connection_id = executor.next_connection_id();
    connection_pump::run_connection_loop(
        stream,
        state,
        codec,
        state.heartbeat_interval(),
        request_router,
        executor,
        connection_id,
    )
}

pub(super) fn reject_overloaded_connection(
    mut stream: UnixStream,
    state: &Arc<DaemonServerState>,
    _socket_owner_uid: Option<u32>,
    retry_after: Duration,
) -> io::Result<()> {
    let Some(codec) = negotiate_v2_connection(&mut stream, state)? else {
        return Ok(());
    };
    let request: DaemonRpcRequest = codec.read(&mut stream).map_err(codec_io_error)?;
    let mut busy = rpc_error(
        DaemonRpcErrorCode::Overloaded,
        "the daemon connection executor is busy",
        true,
    );
    busy.retry_after_ms = Some(
        retry_after
            .as_millis()
            .min(u128::from(u32::MAX))
            .try_into()
            .expect("retry delay is bounded to u32"),
    );
    busy.details = Some(serde_json::json!({
        "kind": "busy",
        "scope": "connection",
    }));
    codec
        .write(&mut stream, &response_for(request.request_id, Err(busy)))
        .map_err(codec_io_error)?;
    stream.flush()
}

fn negotiate_v2_connection(
    stream: &mut UnixStream,
    state: &DaemonServerState,
) -> io::Result<Option<FrameCodec>> {
    stream.set_read_timeout(Some(HELLO_IO_TIMEOUT))?;
    stream.set_write_timeout(Some(HELLO_IO_TIMEOUT))?;
    let codec = FrameCodec::new(DEFAULT_MAX_FRAME_BYTES);
    let hello: DaemonClientHello = codec.read(stream).map_err(codec_io_error)?;
    let session = match negotiate(
        &hello,
        &ServerNegotiation {
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            capabilities: supported_capabilities(),
            instance_id: state.instance_id().to_string(),
        },
    ) {
        Ok(session) => session,
        Err(error) => {
            codec
                .write(stream, &version_rejection(&error))
                .map_err(codec_io_error)?;
            stream.flush()?;
            return Ok(None);
        }
    };
    codec
        .write(stream, &session.hello)
        .map_err(codec_io_error)?;
    stream.flush()?;
    Ok(Some(codec))
}

/// The rejection frame for a client outside this build's compatibility windows.
///
/// It carries the daemon's own version so the client can say which half of the
/// install is stale instead of reporting the running daemon as stopped.
fn version_rejection(error: &bowline_daemon_rpc::NegotiationError) -> Box<DaemonRpcError> {
    let mut rejection = rpc_error(
        DaemonRpcErrorCode::UnsupportedVersion,
        &error.to_string(),
        false,
    );
    rejection.required_client_version = Some(env!("CARGO_PKG_VERSION").to_string());
    rejection.details = Some(serde_json::json!({
        "kind": "version-skew",
        "dimension": error.dimension().map(bowline_daemon_rpc::VersionDimension::as_str),
        "daemonVersion": env!("CARGO_PKG_VERSION"),
        "protocolWindow": bowline_daemon_rpc::DAEMON_RPC_PROTOCOL_WINDOW.to_string(),
        "machineContractWindow": bowline_daemon_rpc::MACHINE_CONTRACT_WINDOW.to_string(),
    }));
    rejection
}

fn route_request(
    context: &RequestContext,
    request: DaemonRpcRequest,
    state: &Arc<DaemonServerState>,
    peer_credential_checked: bool,
) -> DaemonRpcResponse {
    let request_id = request.request_id;
    let result = context
        .checkpoint(CancellationPoint::HandlerStart)
        .map_err(|error| request_context_error(context, error))
        .and_then(|()| {
            let method = RpcMethod::from_wire(&request.method).ok_or_else(method_not_found)?;
            match method {
                RpcMethod::DaemonPing => Ok(serde_json::json!({"ok": true})),
                RpcMethod::DaemonInfo => Ok(serde_json::json!({
                    "daemonVersion": env!("CARGO_PKG_VERSION"),
                    "instanceId": state.instance_id(),
                    "capabilities": supported_capabilities(),
                })),
                RpcMethod::DaemonMetrics => Ok(state.runtime_metrics()),
                RpcMethod::StatusGetSnapshot => snapshot_result(context, state, request.params),
                RpcMethod::SyncBarrier => sync_barrier_result(context, state, request.params),
                RpcMethod::SyncGetBlockedDeletions => {
                    deletions::get_blocked_deletions(context, state)
                }
                RpcMethod::SyncConfirmDeletions => {
                    deletions::confirm_deletions(context, state, peer_credential_checked)
                }
                RpcMethod::WorkCreate => {
                    work_views::work_create(context, state, request.params, peer_credential_checked)
                }
                RpcMethod::WorkReview => {
                    work_views::work_review(context, state, request.params, peer_credential_checked)
                }
                RpcMethod::WorkAccept => {
                    work_views::work_accept(context, state, request.params, peer_credential_checked)
                }
                RpcMethod::DeviceApprove => device_actions::device_action(
                    context,
                    state,
                    request.params,
                    peer_credential_checked,
                    true,
                ),
                RpcMethod::DeviceDeny => device_actions::device_action(
                    context,
                    state,
                    request.params,
                    peer_credential_checked,
                    false,
                ),
                // Connection-owned methods never reach a lane worker; the
                // connection pump answers them before dispatch.
                RpcMethod::StatusSubscribe
                | RpcMethod::SubscriptionCancel
                | RpcMethod::DaemonShutdown => Err(method_not_found()),
            }
        });
    response_for(request_id, result)
}

fn method_not_found() -> Box<DaemonRpcError> {
    rpc_error(
        DaemonRpcErrorCode::MethodNotFound,
        "the requested daemon RPC method is not supported",
        false,
    )
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SyncBarrierParams {
    workspace_id: WorkspaceId,
    timeout_ms: u64,
}

fn sync_barrier_result(
    context: &RequestContext,
    state: &DaemonServerState,
    params: serde_json::Value,
) -> RpcResult<serde_json::Value> {
    let params: SyncBarrierParams = serde_json::from_value(params).map_err(|_| {
        rpc_error(
            DaemonRpcErrorCode::InvalidRequest,
            "sync.barrier requires workspaceId and timeoutMs",
            false,
        )
    })?;
    if !state.serves_workspace(&params.workspace_id) {
        return Err(rpc_error(
            DaemonRpcErrorCode::InvalidRequest,
            "daemon is serving a different workspace",
            false,
        ));
    }
    checkpoint(context, CancellationPoint::BeforeExternalCall)?;
    let timeout = Duration::from_millis(params.timeout_ms.max(1)).min(SYNC_BARRIER_MAX_TIMEOUT);
    let receipt = state
        .request_sync_barrier(timeout, || {
            context
                .checkpoint(CancellationPoint::BeforeExternalCall)
                .is_err()
        })
        .map_err(|error| barrier_error(context, error))?;
    let observer_frontier = receipt
        .engine_observer()
        .observer_frontier()
        .ok_or_else(exact_receipt_encoding_error)?;
    let process_identity = process_identity(receipt.recovery())?;
    let workspace_identity = receipt
        .recovery()
        .source_identity()
        .workspace_id()
        .as_str()
        .to_string();
    let source = DaemonSyncBarrierConvergenceSource {
        process_identity: process_identity.clone(),
        workspace_identity: workspace_identity.clone(),
    };
    let engine = receipt.engine_observer().engine();
    let engine_convergence = live_engine_convergence(receipt.engine_observer(), source.clone());
    let observer = live_observer_convergence(receipt.engine_observer(), observer_frontier, source)?;
    let recovery_frontier = receipt.recovery().frontier();
    let closing_recovery_revision = recovery_frontier
        .last_closure()
        .map(|closure| closure.closing_recovery_revision().get())
        .unwrap_or(0);
    let result = DaemonSyncBarrierResult {
        barrier_id: format!("workspace-barrier:{}", engine.barrier_id().0),
        process_identity,
        workspace_identity,
        engine_convergence,
        observer,
        coordinator_nominal_frontier: DaemonSyncBarrierCoordinatorFrontier {
            lifecycle: DaemonSyncBarrierRecoveryLifecycle::Nominal,
            recovery_snapshot_revision: recovery_frontier.recovery_revision().get(),
            closing_recovery_revision,
            activity_watermark: recovery_frontier.activity_watermark().get(),
        },
        recovery_closure: recovery_closure(receipt.recovery())?,
        workspace_status: DaemonSyncBarrierWorkspaceStatus {
            state: DaemonSyncBarrierWorkspaceState::Ready,
            observed_at: receipt.linearized_at().to_string(),
        },
        linearized_at: receipt.linearized_at().to_string(),
    };
    serde_json::to_value(result).map_err(|_| {
        rpc_error(
            DaemonRpcErrorCode::Internal,
            "failed to encode exact workspace barrier receipt",
            false,
        )
    })
}

fn engine_ref_identity(
    value: &bowline_local::sync::manifest_engine::EngineRef,
) -> DaemonSyncBarrierRefIdentity {
    use bowline_local::sync::manifest_engine::EngineRef;
    match value {
        EngineRef::Genesis => DaemonSyncBarrierRefIdentity {
            version: 0,
            manifest_key: None,
        },
        EngineRef::Head(observation) => DaemonSyncBarrierRefIdentity {
            version: observation.version,
            manifest_key: Some(observation.manifest_key.as_str().to_string()),
        },
    }
}

fn process_identity(
    attestation: &bowline_daemon::watcher_recovery::RecoveryAttestation,
) -> RpcResult<DaemonSyncBarrierProcessIdentity> {
    let identity = attestation.source_identity().process_identity();
    Ok(DaemonSyncBarrierProcessIdentity {
        boot_id: identity.boot_id().to_string(),
        session_id: identity.session_id().to_string(),
        started_at: recovery_timestamp(identity.started_at())?,
    })
}

fn live_engine_convergence(
    receipt: &bowline_daemon::manifest_driver::EngineObserverConvergenceReceipt,
    source: DaemonSyncBarrierConvergenceSource,
) -> DaemonSyncBarrierEngineConvergence {
    let engine = receipt.engine();
    DaemonSyncBarrierEngineConvergence {
        source,
        barrier_id: format!("barrier:{}", engine.barrier_id().0),
        endpoint_generation: engine.endpoint_generation().0,
        engine_revision: engine.engine_revision(),
        materialization_revision: engine.materialization_revision().get(),
        admitted_at: receipt.engine_admitted_at().as_str().to_string(),
        completed_at: receipt.engine_completed_at().as_str().to_string(),
        observed_ref: engine_ref_identity(engine.observed_ref()),
        applied_ref: engine_ref_identity(engine.applied_ref()),
    }
}

fn live_observer_convergence(
    receipt: &bowline_daemon::manifest_driver::EngineObserverConvergenceReceipt,
    frontier: &bowline_daemon::manifest_transport::RefObserverFrontier,
    source: DaemonSyncBarrierConvergenceSource,
) -> RpcResult<DaemonSyncBarrierObserverConvergence> {
    let admitted_at = receipt
        .observer_admitted_at()
        .ok_or_else(exact_receipt_encoding_error)?;
    let completed_at = receipt
        .observer_completed_at()
        .ok_or_else(exact_receipt_encoding_error)?;
    let liveness = |observed_at: &str| DaemonSyncBarrierObserverLiveness {
        state: DaemonSyncBarrierObserverState::Live,
        endpoint_generation: frontier.authority_source.endpoint_generation().get(),
        lifecycle_revision: frontier.lifecycle_revision.get(),
        observed_at: observed_at.to_string(),
        observed_ref: observer_ref_identity(&frontier.verified_ref),
    };
    Ok(DaemonSyncBarrierObserverConvergence {
        source,
        admission: liveness(admitted_at.as_str()),
        completion: liveness(completed_at.as_str()),
    })
}

fn recovery_closure(
    attestation: &bowline_daemon::watcher_recovery::RecoveryAttestation,
) -> RpcResult<Option<DaemonSyncBarrierRecoveryClosure>> {
    let Some(closure) = attestation.last_closure() else {
        return Ok(None);
    };
    let source_identity = attestation.source_identity();
    let process = source_identity.process_identity();
    let process_identity = DaemonSyncBarrierProcessIdentity {
        boot_id: process.boot_id().to_string(),
        session_id: process.session_id().to_string(),
        started_at: recovery_timestamp(process.started_at())?,
    };
    Ok(Some(DaemonSyncBarrierRecoveryClosure {
        process_identity,
        incident_id: closure.incident_id().to_string(),
        closing_attempt_id: closure.attempt_id().to_string(),
        attempt_count: closure.attempt_count().get(),
        scan_count: closure.scan_count().get(),
        captured_activity_watermark: closure.native_boundary().activity_watermark().get(),
        final_activity_watermark: closure.activity_watermark().get(),
        authoritative_scan_revision: closure.authoritative_scan_revision().get(),
        native_boundary: native_coverage(closure.native_boundary().proof()),
        closing_recovery_revision: closure.closing_recovery_revision().get(),
        closed_at: recovery_timestamp(closure.completed_at())?,
    }))
}

fn observer_ref_identity(
    value: &bowline_daemon::manifest_transport::VerifiedWorkspaceRef,
) -> DaemonSyncBarrierRefIdentity {
    use bowline_daemon::manifest_transport::VerifiedWorkspaceRefView;
    match value.view() {
        VerifiedWorkspaceRefView::Genesis => DaemonSyncBarrierRefIdentity {
            version: 0,
            manifest_key: None,
        },
        VerifiedWorkspaceRefView::Head {
            version,
            manifest_key,
        } => DaemonSyncBarrierRefIdentity {
            version,
            manifest_key: Some(manifest_key.as_str().to_string()),
        },
    }
}

fn native_coverage(
    boundary: bowline_daemon::watcher_coverage::WatcherCoverageBoundary,
) -> DaemonSyncBarrierNativeCoverage {
    use bowline_daemon::watcher_coverage::{DarwinCoverageStart, WatcherCoverageBoundary};
    match boundary {
        WatcherCoverageBoundary::Darwin(boundary) => {
            let coverage_start = match boundary.start() {
                DarwinCoverageStart::CursorReplay {
                    covered_last_safe,
                    replay_from,
                    recovery_cause,
                } => DaemonSyncBarrierDarwinCoverageStart::CursorReplay(Box::new(
                    DaemonSyncBarrierDarwinCursorReplay {
                        covered_last_safe: covered_last_safe.get(),
                        replay_from: replay_from.get(),
                        recovery_cause: recovery_cause.map(native_loss),
                    },
                )),
                DarwinCoverageStart::FreshStream {
                    fresh_from,
                    discontinuity,
                } => DaemonSyncBarrierDarwinCoverageStart::FreshStream(Box::new(
                    DaemonSyncBarrierDarwinFreshStream {
                        fresh_from: fresh_from.get(),
                        discontinuity: native_loss(discontinuity),
                    },
                )),
            };
            DaemonSyncBarrierNativeCoverage::FseventsPostScanSeal(Box::new(
                DaemonSyncBarrierDarwinCoverage {
                    boundary_id: boundary.boundary_id().get(),
                    covered_epoch: boundary.covered_epoch().get(),
                    live_epoch: boundary.live_epoch().get(),
                    coverage_start,
                    history_through: boundary.history_through().get(),
                    history_done: true,
                    must_scan_subdirs: boundary.must_scan_subdirs(),
                    sealed_through: boundary.sealed_through().get(),
                    flush_generation: boundary.flush_generation().get(),
                    loss_generation: boundary.loss_generation().get(),
                    callback_generation: boundary.callback_generation().get(),
                },
            ))
        }
        WatcherCoverageBoundary::Linux(boundary) => {
            DaemonSyncBarrierNativeCoverage::InotifyLiveDrain(Box::new(
                DaemonSyncBarrierLinuxCoverage {
                    boundary_id: boundary.boundary_id().get(),
                    stream_epoch: boundary.stream_epoch().get(),
                    watcher_ready_control_id: boundary.watcher_ready().control_id().get(),
                    callback_drain_control_id: boundary.callback_drain().control_id().get(),
                },
            ))
        }
    }
}

fn native_loss(
    loss: bowline_daemon::watcher_coverage::WatcherCoverageLoss,
) -> DaemonSyncBarrierNativeLoss {
    use bowline_daemon::watcher_coverage::WatcherCoverageLoss;
    match loss {
        WatcherCoverageLoss::UserDropped => DaemonSyncBarrierNativeLoss::UserDropped,
        WatcherCoverageLoss::KernelDropped => DaemonSyncBarrierNativeLoss::KernelDropped,
        WatcherCoverageLoss::EventIdsWrapped => DaemonSyncBarrierNativeLoss::EventIdsWrapped,
        WatcherCoverageLoss::RootChanged => DaemonSyncBarrierNativeLoss::RootChanged,
        WatcherCoverageLoss::StreamStopped => DaemonSyncBarrierNativeLoss::StreamStopped,
        WatcherCoverageLoss::NonMonotonicCursor => DaemonSyncBarrierNativeLoss::NonMonotonicCursor,
        WatcherCoverageLoss::QueueOverflow => DaemonSyncBarrierNativeLoss::QueueOverflow,
        WatcherCoverageLoss::BackendFailure => DaemonSyncBarrierNativeLoss::BackendFailure,
    }
}

fn recovery_timestamp(
    value: bowline_daemon::watcher_recovery::RecoveryTimestamp,
) -> RpcResult<String> {
    value
        .to_rfc3339()
        .map_err(|_| exact_receipt_encoding_error())
}

fn exact_receipt_encoding_error() -> Box<DaemonRpcError> {
    rpc_error(
        DaemonRpcErrorCode::Internal,
        "failed to compose exact workspace barrier receipt",
        false,
    )
}

fn barrier_error(
    context: &RequestContext,
    error: bowline_daemon::manifest_driver::SyncBarrierError,
) -> Box<DaemonRpcError> {
    use bowline_daemon::manifest_driver::SyncBarrierError;
    match error {
        // Re-run the checkpoint so the client receives the concrete cancellation
        // reason (disconnect, deadline) rather than a generic abort.
        SyncBarrierError::Cancelled => match context.checkpoint(CancellationPoint::HandlerStart) {
            Err(cancellation) => request_context_error(context, cancellation),
            Ok(()) => rpc_error(DaemonRpcErrorCode::Cancelled, error.as_str(), true),
        },
        SyncBarrierError::TimedOut => {
            rpc_error(DaemonRpcErrorCode::DeadlineExceeded, error.as_str(), true)
        }
        // `unavailable` is the daemon telling a waiting client "not yet": the
        // engine slot exists and something will attach to it. A daemon that
        // serves no sync workspace owes a different answer, or every wait would
        // sit out its whole budget on a condition that never clears.
        SyncBarrierError::Unavailable { .. } | SyncBarrierError::EngineStopped => {
            rpc_error(DaemonRpcErrorCode::Unavailable, error.as_str(), true)
        }
        SyncBarrierError::ObserverBlocked { .. } | SyncBarrierError::RecoveryBlocked { .. } => {
            rpc_error(DaemonRpcErrorCode::PermissionDenied, error.as_str(), false)
        }
        SyncBarrierError::FatalContract { .. } => {
            rpc_error(DaemonRpcErrorCode::Internal, error.as_str(), false)
        }
        SyncBarrierError::WorkspaceNotServed => {
            rpc_error(DaemonRpcErrorCode::NotFound, error.as_str(), false)
        }
    }
}

fn route_connection_request(
    request: DaemonRpcRequest,
    state: &Arc<DaemonServerState>,
    subscriptions: &mut HashMap<String, Arc<StatusSubscription>>,
    next_event_sequence: &mut u64,
    status_wake: &Sender<()>,
) -> DaemonRpcResponse {
    let request_id = request.request_id;
    let result = match RpcMethod::from_wire(&request.method) {
        Some(RpcMethod::StatusSubscribe) => subscribe_result(
            state,
            subscriptions,
            next_event_sequence,
            status_wake,
            request.params,
        ),
        Some(RpcMethod::SubscriptionCancel) => {
            cancel_subscription(state, subscriptions, request.params)
        }
        Some(RpcMethod::DaemonShutdown) => {
            state.request_shutdown();
            Ok(serde_json::json!({"state": "stopping"}))
        }
        Some(_) | None => Err(rpc_error(
            DaemonRpcErrorCode::MethodNotFound,
            "the requested daemon RPC method is not connection-owned",
            false,
        )),
    };
    response_for(request_id, result)
}

fn response_for(request_id: String, result: RpcResult<serde_json::Value>) -> DaemonRpcResponse {
    match result {
        Ok(result) => DaemonRpcResponse {
            request_id,
            result: Some(result),
            error: None,
        },
        Err(error) => DaemonRpcResponse {
            request_id,
            result: None,
            error: Some(*error),
        },
    }
}

fn snapshot_result(
    context: &RequestContext,
    state: &DaemonServerState,
    params: serde_json::Value,
) -> RpcResult<serde_json::Value> {
    checkpoint(context, CancellationPoint::BeforeProjectionRead)?;
    let scope = resolve_status_scope(state, params)?;
    let snapshot = state.snapshot_for_scope(scope.as_ref()).ok_or_else(|| {
        rpc_error(
            DaemonRpcErrorCode::Unavailable,
            "the daemon status projection is unavailable",
            true,
        )
    })?;
    let status =
        status_command_to_wire(&snapshot.status).map_err(internal_status_transport_error)?;
    serde_json::to_value(DaemonStatusSnapshotResult {
        instance_id: snapshot.instance_id,
        sequence: snapshot.sequence,
        snapshot: status,
    })
    .map_err(internal_serialization_error)
}

fn subscribe_result(
    state: &DaemonServerState,
    subscriptions: &mut HashMap<String, Arc<StatusSubscription>>,
    next_event_sequence: &mut u64,
    status_wake: &Sender<()>,
    params: serde_json::Value,
) -> RpcResult<serde_json::Value> {
    let scope = resolve_status_scope(state, params)?;
    let (subscription, snapshot) = state
        .subscribe_with_snapshot(Some(status_wake.clone()), scope)
        .ok_or_else(|| {
            rpc_error(
                DaemonRpcErrorCode::Internal,
                "the daemon status subscription registry is unavailable",
                true,
            )
        })?;
    *next_event_sequence = (*next_event_sequence).max(snapshot.sequence.saturating_add(1));
    let status =
        status_command_to_wire(&snapshot.status).map_err(internal_status_transport_error)?;
    let result = DaemonStatusSubscribeResult {
        subscription_id: subscription.id.clone(),
        instance_id: snapshot.instance_id,
        sequence: snapshot.sequence,
        snapshot: status,
    };
    subscriptions.insert(subscription.id.clone(), subscription);
    serde_json::to_value(result).map_err(internal_serialization_error)
}

fn resolve_status_scope(
    state: &DaemonServerState,
    params: serde_json::Value,
) -> RpcResult<Option<ProjectStatusScope>> {
    let params = serde_json::from_value::<DaemonStatusScopeParams>(params).map_err(|_| {
        rpc_error(
            DaemonRpcErrorCode::InvalidRequest,
            "status scope params are invalid",
            false,
        )
    })?;
    state
        .resolve_status_scope(
            params.workspace_root.as_deref(),
            params.project_path.as_deref(),
            params.requested_path.as_deref(),
        )
        .map_err(|_| {
            rpc_error(
                DaemonRpcErrorCode::InvalidRequest,
                "the requested status scope is not served by this daemon instance",
                false,
            )
        })
}

fn cancel_subscription(
    state: &DaemonServerState,
    subscriptions: &mut HashMap<String, Arc<StatusSubscription>>,
    params: serde_json::Value,
) -> RpcResult<serde_json::Value> {
    let subscription_id = required_string_param(&params, "subscriptionId")?;
    subscriptions.remove(subscription_id);
    let cancelled = state.cancel_subscription(subscription_id);
    Ok(serde_json::json!({"cancelled": cancelled}))
}

fn checkpoint(context: &RequestContext, point: CancellationPoint) -> RpcResult<()> {
    context
        .checkpoint(point)
        .map_err(|error| request_context_error(context, error))
}

fn request_context_error(
    context: &RequestContext,
    error: RequestContextError,
) -> Box<DaemonRpcError> {
    let mut rpc = rpc_error(error.code(), error.message(), false);
    rpc.details = Some(serde_json::json!({
        "correlationId": context.correlation_id().as_str(),
        "cancellationPoint": error.point().as_str(),
    }));
    rpc
}

fn required_string_param<'a>(params: &'a serde_json::Value, field: &str) -> RpcResult<&'a str> {
    params
        .get(field)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            rpc_error(
                DaemonRpcErrorCode::InvalidRequest,
                &format!("{field} is required"),
                false,
            )
        })
}

fn rpc_error(code: DaemonRpcErrorCode, message: &str, retryable: bool) -> Box<DaemonRpcError> {
    Box::new(DaemonRpcError {
        code,
        message: message.chars().take(4096).collect(),
        retryable,
        retry_after_ms: retryable.then_some(250),
        operation_id: None,
        required_client_version: None,
        details: None,
    })
}

fn internal_serialization_error(error: serde_json::Error) -> Box<DaemonRpcError> {
    rpc_error(
        DaemonRpcErrorCode::Internal,
        &format!("daemon response serialization failed: {error}"),
        false,
    )
}

fn internal_status_transport_error(error: StatusTransportError) -> Box<DaemonRpcError> {
    rpc_error(
        DaemonRpcErrorCode::Internal,
        &format!("daemon status transport conversion failed: {error}"),
        false,
    )
}

fn codec_io_error(error: CodecError) -> io::Error {
    let kind = match error {
        CodecError::CleanEof | CodecError::UnexpectedEof { .. } => io::ErrorKind::UnexpectedEof,
        CodecError::FrameTooLarge { .. }
        | CodecError::InvalidMagic { .. }
        | CodecError::MalformedJson(_)
        | CodecError::Serialize(_) => io::ErrorKind::InvalidData,
        CodecError::Io { ref source, .. } => source.kind(),
    };
    io::Error::new(kind, error)
}
