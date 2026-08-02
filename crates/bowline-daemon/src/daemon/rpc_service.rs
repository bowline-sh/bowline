use bowline_control_plane::DeviceControlPlaneClient;
use std::io::Write;

use super::server_state::ProjectStatusScope;

use crate::daemon::{
    DaemonServerState, StatusSubscription, current_timestamp, hosted_control_plane, key_store,
};
use bowline_core::ids::WorkspaceId;
use bowline_local::trust::grants;
use std::collections::HashMap;
use std::io;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

mod connection_pump;
mod deletions;
mod method_registry;
mod request_context;
mod rpc_executor;
mod work_views;

use bowline_core::wire::generated::{
    DaemonClientHello, DaemonDeviceActionParams, DaemonRpcError, DaemonRpcErrorCode,
    DaemonRpcRequest, DaemonRpcResponse, DaemonStatusScopeParams, DaemonStatusSnapshotResult,
    DaemonStatusSubscribeResult,
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
                RpcMethod::DeviceApprove => device_action(
                    context,
                    state,
                    request.params,
                    peer_credential_checked,
                    true,
                ),
                RpcMethod::DeviceDeny => device_action(
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
    let snapshot = state
        .request_sync_barrier(timeout, || {
            context
                .checkpoint(CancellationPoint::BeforeExternalCall)
                .is_err()
        })
        .map_err(|error| barrier_error(context, error))?;
    Ok(serde_json::json!({
        "convergenceRevision": snapshot.revision,
    }))
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
        SyncBarrierError::ObserverUnavailable => {
            rpc_error(DaemonRpcErrorCode::PermissionDenied, error.as_str(), false)
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

fn device_action(
    context: &RequestContext,
    state: &DaemonServerState,
    params: serde_json::Value,
    peer_credential_checked: bool,
    approve: bool,
) -> RpcResult<serde_json::Value> {
    if !peer_credential_checked {
        return Err(rpc_error(
            DaemonRpcErrorCode::PermissionDenied,
            "device actions require a verified same-user local socket peer",
            false,
        ));
    }
    let params = serde_json::from_value::<DaemonDeviceActionParams>(params).map_err(|_| {
        rpc_error(
            DaemonRpcErrorCode::InvalidRequest,
            "device action params are invalid",
            false,
        )
    })?;
    if params.request_id.is_empty()
        || params.request_id.len() > 512
        || params.idempotency_key.is_empty()
        || params.idempotency_key.len() > 128
    {
        return Err(rpc_error(
            DaemonRpcErrorCode::InvalidRequest,
            "device action identifiers are outside their bounded contract",
            false,
        ));
    }
    let Some((workspace_id, device_id)) = state.sync_identity() else {
        return Err(rpc_error(
            DaemonRpcErrorCode::Unavailable,
            "device actions require a configured daemon workspace",
            false,
        ));
    };
    checkpoint(context, CancellationPoint::BeforeExternalCall)?;
    let key_store = key_store().map_err(|error| {
        rpc_error(
            DaemonRpcErrorCode::Unavailable,
            &format!("device key store is unavailable: {error}"),
            true,
        )
    })?;
    let control_plane = hosted_control_plane(&*key_store, workspace_id.clone(), device_id.clone())
        .map_err(|error| {
            rpc_error(
                DaemonRpcErrorCode::Unavailable,
                &format!("device trust service is unavailable: {error}"),
                true,
            )
        })?;
    checkpoint(context, CancellationPoint::BeforeExternalCall)?;
    let trust = control_plane
        .list_device_trust(&workspace_id)
        .map_err(|error| {
            rpc_error(
                DaemonRpcErrorCode::Unavailable,
                &format!("device trust state is unavailable: {error}"),
                true,
            )
        })?;
    if !trust
        .pending_requests
        .iter()
        .any(|request| request.request_id.as_str() == params.request_id)
    {
        return Ok(serde_json::json!({
            "requestId": params.request_id,
            "state": "already-resolved",
        }));
    }

    if approve {
        context
            .begin_commit_fence()
            .map_err(|error| request_context_error(context, error))?;
        bowline_local::trust::approve_device_request(
            &control_plane,
            &*key_store,
            bowline_local::trust::ApproveDeviceOptions {
                workspace_id,
                request_id: bowline_core::ids::DeviceApprovalRequestId::new(
                    params.request_id.clone(),
                ),
                approver_device_id: device_id,
                generated_at: current_timestamp(),
            },
        )
        .map_err(|error| {
            rpc_error(
                DaemonRpcErrorCode::Internal,
                &format!("device approval failed: {error}"),
                false,
            )
        })?;
    } else {
        checkpoint(context, CancellationPoint::BeforeExternalCall)?;
        let identity = key_store
            .load_or_create_device_identity()
            .map_err(|error| {
                rpc_error(
                    DaemonRpcErrorCode::Unavailable,
                    &format!("device identity is unavailable: {error}"),
                    true,
                )
            })?;
        let proof = grants::device_authorization_proof(
            &identity,
            &workspace_id,
            &device_id,
            "deny-device-request",
            &grants::device_request_proof_subject(
                &bowline_core::ids::DeviceApprovalRequestId::new(params.request_id.clone()),
            ),
        )
        .map_err(|error| {
            rpc_error(
                DaemonRpcErrorCode::Internal,
                &format!("device denial proof failed: {error}"),
                false,
            )
        })?;
        context
            .begin_commit_fence()
            .map_err(|error| request_context_error(context, error))?;
        control_plane
            .deny_device_request(bowline_control_plane::DeviceDenialInput {
                request_id: bowline_core::ids::DeviceApprovalRequestId::new(
                    params.request_id.clone(),
                ),
                denied_by_device_id: device_id,
                denied_by_device_proof: proof,
                reason: "denied by Bowline menu bar".to_string(),
            })
            .map_err(|error| {
                rpc_error(
                    DaemonRpcErrorCode::Internal,
                    &format!("device denial failed: {error}"),
                    false,
                )
            })?;
    }
    Ok(serde_json::json!({
        "requestId": params.request_id,
        "state": "resolved",
    }))
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
