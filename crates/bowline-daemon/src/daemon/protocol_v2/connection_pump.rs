use std::{
    collections::HashMap,
    io::{self, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::Arc,
    time::{Duration, Instant},
};

use bowline_core::wire::generated::{
    DaemonRpcCancel, DaemonRpcErrorCode, DaemonRpcRequest, DaemonRpcResponse,
};
use bowline_daemon_rpc::FrameCodec;
use crossbeam_channel::{Sender, after, bounded, never, select_biased};

use super::{codec_io_error, response_for, route_connection_request, rpc_error};
use super::{
    request_context::{
        CancellationDisposition, CancellationReason, RequestContext, RpcConnectionId, RpcRequestId,
    },
    rpc_executor::{RequestRouter, RpcCompletion, RpcExecutor, RpcLane, SubmissionError},
};
use crate::daemon::{DaemonServerState, StatusSubscription};

mod reader;
mod status_delivery;
#[cfg(test)]
mod tests;

use reader::ReaderEvent;
use status_delivery::{flush_status_events, next_wakeup};

const RESPONSE_IO_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_IN_FLIGHT_REQUESTS: usize = 16;

struct InFlightRequest {
    context: RequestContext,
}

struct PayloadContext<'a> {
    state: &'a Arc<DaemonServerState>,
    request_router: &'a Arc<RequestRouter>,
    executor: &'a RpcExecutor,
    completed_tx: &'a Sender<RpcCompletion>,
    status_wake: &'a Sender<()>,
    connection_id: RpcConnectionId,
    subscriptions: &'a mut HashMap<String, Arc<StatusSubscription>>,
    in_flight: &'a mut HashMap<String, InFlightRequest>,
    next_event_sequence: &'a mut u64,
    codec: FrameCodec,
    stream: &'a mut UnixStream,
}

struct DispatchContext<'a> {
    state: &'a DaemonServerState,
    request_router: &'a Arc<RequestRouter>,
    executor: &'a RpcExecutor,
    completed_tx: &'a Sender<RpcCompletion>,
    connection_id: RpcConnectionId,
    in_flight: &'a mut HashMap<String, InFlightRequest>,
    codec: FrameCodec,
    stream: &'a mut UnixStream,
}

pub(super) fn run_connection_loop(
    mut stream: UnixStream,
    state: &Arc<DaemonServerState>,
    codec: FrameCodec,
    heartbeat_interval: Duration,
    request_router: Arc<RequestRouter>,
    executor: Arc<RpcExecutor>,
    connection_id: RpcConnectionId,
) -> io::Result<()> {
    stream.set_write_timeout(Some(RESPONSE_IO_TIMEOUT))?;
    let reader_stream = stream.try_clone()?;
    let (reader_rx, reader) = reader::spawn(reader_stream, connection_id)?;
    state.record_connection_reader_started();
    let mut subscriptions = HashMap::<String, Arc<StatusSubscription>>::new();
    let mut in_flight = HashMap::<String, InFlightRequest>::new();
    let (completed_tx, completed_rx) = bounded(MAX_IN_FLIGHT_REQUESTS);
    let (status_wake_tx, status_wake_rx) = bounded(1);
    let (control_wake_tx, control_wake_rx) = bounded(1);
    state.register_connection_wake(connection_id.get(), control_wake_tx);
    let mut next_event_sequence = state
        .snapshot()
        .map(|snapshot| snapshot.sequence.saturating_add(1))
        .unwrap_or(1);
    let mut last_heartbeat = Instant::now();

    let result = (|| -> io::Result<()> {
        loop {
            if state.should_stop_connections() {
                break;
            }
            let timer = next_wakeup(
                &in_flight,
                &subscriptions,
                last_heartbeat,
                heartbeat_interval,
            )
            .map(after)
            .unwrap_or_else(never);
            select_biased! {
                recv(control_wake_rx) -> _ => {},
                recv(completed_rx) -> completion => {
                    if let Ok(completion) = completion {
                        complete_request(completion, &mut in_flight, &executor, codec, &mut stream)?;
                    }
                },
                recv(timer) -> _ => {
                    expire_request_deadlines(
                        &mut in_flight,
                        &executor,
                        connection_id,
                        codec,
                        &mut stream,
                    )?;
                    flush_status_events(
                        &mut subscriptions,
                        &mut next_event_sequence,
                        &mut last_heartbeat,
                        heartbeat_interval,
                        codec,
                        &mut stream,
                    )?;
                },
                recv(status_wake_rx) -> _ => {
                    flush_status_events(
                        &mut subscriptions,
                        &mut next_event_sequence,
                        &mut last_heartbeat,
                        heartbeat_interval,
                        codec,
                        &mut stream,
                    )?;
                },
                recv(reader_rx) -> event => match event {
                    Ok(ReaderEvent::Payload(payload)) => {
                        handle_payload(
                            &payload,
                            PayloadContext {
                                state,
                                request_router: &request_router,
                                executor: &executor,
                                completed_tx: &completed_tx,
                                status_wake: &status_wake_tx,
                                connection_id,
                                subscriptions: &mut subscriptions,
                                in_flight: &mut in_flight,
                                next_event_sequence: &mut next_event_sequence,
                                codec,
                                stream: &mut stream,
                            },
                        )?;
                    }
                    Ok(ReaderEvent::CleanEof) | Err(_) => break,
                    Ok(ReaderEvent::Failed(error)) => return Err(codec_io_error(error)),
                },
            }
        }
        Ok(())
    })();
    for request in in_flight.values() {
        request
            .context
            .request_cancellation(CancellationReason::Disconnected);
    }
    executor.cancel_connection(connection_id);
    state.unregister_connection_wake(connection_id.get());
    for subscription_id in subscriptions.keys() {
        state.cancel_subscription(subscription_id);
    }
    drop(reader_rx);
    let _reader_shutdown = stream.shutdown(Shutdown::Read);
    let reader_result = reader.join();
    state.record_connection_reader_joined();
    if reader_result.is_err() {
        return Err(io::Error::other("bowline RPC connection reader panicked"));
    }
    result
}

fn handle_payload(payload: &[u8], context: PayloadContext<'_>) -> io::Result<()> {
    let value = serde_json::from_slice::<serde_json::Value>(payload)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if value.get("method").is_none() {
        let cancel = serde_json::from_value::<DaemonRpcCancel>(value)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        return cancel_in_flight_request(
            context.in_flight,
            context.executor,
            context.connection_id,
            &cancel.request_id,
            context.codec,
            context.stream,
        );
    }
    let request = serde_json::from_value::<DaemonRpcRequest>(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if connection_owns_method(&request.method) {
        if context.in_flight.contains_key(&request.request_id) {
            return write_active_request_conflict(
                context.codec,
                context.stream,
                request.request_id,
            );
        }
        let response = route_connection_request(
            request,
            context.state,
            context.subscriptions,
            context.next_event_sequence,
            context.status_wake,
        );
        write_response(context.codec, context.stream, &response)
    } else {
        dispatch_request(
            request,
            DispatchContext {
                state: context.state,
                request_router: context.request_router,
                executor: context.executor,
                completed_tx: context.completed_tx,
                connection_id: context.connection_id,
                in_flight: context.in_flight,
                codec: context.codec,
                stream: context.stream,
            },
        )
    }
}

fn connection_owns_method(method: &str) -> bool {
    matches!(
        method,
        "status.subscribe" | "subscription.cancel" | "daemon.shutdown"
    )
}

fn dispatch_request(request: DaemonRpcRequest, context: DispatchContext<'_>) -> io::Result<()> {
    if context.in_flight.contains_key(&request.request_id) {
        return write_active_request_conflict(context.codec, context.stream, request.request_id);
    }
    if context.in_flight.len() >= MAX_IN_FLIGHT_REQUESTS {
        return write_response(
            context.codec,
            context.stream,
            &response_for(
                request.request_id,
                Err(rpc_error(
                    DaemonRpcErrorCode::Overloaded,
                    "the connection has reached its concurrent request limit",
                    true,
                )),
            ),
        );
    }
    if !context.state.accepts_mutations()
        && RpcLane::for_method(&request.method) == Some(RpcLane::Mutation)
    {
        return write_response(
            context.codec,
            context.stream,
            &response_for(
                request.request_id,
                Err(rpc_error(
                    DaemonRpcErrorCode::Unavailable,
                    "the daemon is shutting down and no longer accepts mutations",
                    true,
                )),
            ),
        );
    }

    let request_id = request.request_id.clone();
    let deadline = request
        .deadline_ms
        .map(|millis| Instant::now() + Duration::from_millis(u64::from(millis)));
    let request_context = context.executor.request_context(
        context.connection_id,
        RpcRequestId::new(request_id.clone()),
        deadline,
    );
    context.in_flight.insert(
        request_id.clone(),
        InFlightRequest {
            context: request_context.clone(),
        },
    );
    match context.executor.submit(
        context.connection_id,
        request_context,
        request,
        Arc::clone(context.request_router),
        context.completed_tx.clone(),
    ) {
        Ok(()) => Ok(()),
        Err(error) => {
            context.in_flight.remove(&request_id);
            write_submission_error(context.codec, context.stream, request_id, error)
        }
    }
}

fn write_active_request_conflict(
    codec: FrameCodec,
    stream: &mut UnixStream,
    request_id: String,
) -> io::Result<()> {
    write_response(
        codec,
        stream,
        &response_for(
            request_id,
            Err(rpc_error(
                DaemonRpcErrorCode::Conflict,
                "the request id is already active on this connection",
                false,
            )),
        ),
    )
}

fn cancel_in_flight_request(
    in_flight: &mut HashMap<String, InFlightRequest>,
    executor: &RpcExecutor,
    connection_id: RpcConnectionId,
    request_id: &str,
    codec: FrameCodec,
    stream: &mut UnixStream,
) -> io::Result<()> {
    terminal_cancellation(
        in_flight,
        TerminalCancellation {
            executor,
            connection_id,
            request_id,
            reason: CancellationReason::Cancelled,
            code: DaemonRpcErrorCode::Cancelled,
            message: "the daemon request was cancelled",
        },
        codec,
        stream,
    )
}

fn complete_request(
    completion: RpcCompletion,
    in_flight: &mut HashMap<String, InFlightRequest>,
    executor: &RpcExecutor,
    codec: FrameCodec,
    stream: &mut UnixStream,
) -> io::Result<()> {
    executor.record_cancellation_checkpoint(&completion.cancellation);
    let request_id = completion.request_id.as_str();
    let is_current = in_flight.get(request_id).is_some_and(|request| {
        request
            .context
            .cancellation()
            .same_request(&completion.cancellation)
    });
    if !is_current {
        return Ok(());
    }
    in_flight.remove(request_id);
    write_response(codec, stream, &completion.response)
}

fn expire_request_deadlines(
    in_flight: &mut HashMap<String, InFlightRequest>,
    executor: &RpcExecutor,
    connection_id: RpcConnectionId,
    codec: FrameCodec,
    stream: &mut UnixStream,
) -> io::Result<()> {
    let now = Instant::now();
    let expired = in_flight
        .iter()
        .filter(|(_, request)| {
            !request.context.commit_fence_started()
                && request
                    .context
                    .deadline()
                    .is_some_and(|deadline| deadline <= now)
        })
        .map(|(request_id, _)| request_id.clone())
        .collect::<Vec<_>>();
    for request_id in expired {
        terminal_cancellation(
            in_flight,
            TerminalCancellation {
                executor,
                connection_id,
                request_id: &request_id,
                reason: CancellationReason::DeadlineExceeded,
                code: DaemonRpcErrorCode::DeadlineExceeded,
                message: "the daemon request deadline elapsed",
            },
            codec,
            stream,
        )?;
    }
    Ok(())
}

struct TerminalCancellation<'a> {
    executor: &'a RpcExecutor,
    connection_id: RpcConnectionId,
    request_id: &'a str,
    reason: CancellationReason,
    code: DaemonRpcErrorCode,
    message: &'static str,
}

fn terminal_cancellation(
    in_flight: &mut HashMap<String, InFlightRequest>,
    terminal: TerminalCancellation<'_>,
    codec: FrameCodec,
    stream: &mut UnixStream,
) -> io::Result<()> {
    let Some(request) = in_flight.get(terminal.request_id) else {
        return Ok(());
    };
    if request.context.request_cancellation(terminal.reason)
        == CancellationDisposition::DeferredUntilCompletion
    {
        return Ok(());
    }
    let cancellation = request.context.cancellation().clone();
    in_flight.remove(terminal.request_id);
    terminal
        .executor
        .cancel_request(terminal.connection_id, &cancellation);
    terminal
        .executor
        .record_terminal_cancellation(&cancellation);
    write_response(
        codec,
        stream,
        &response_for(
            terminal.request_id.to_string(),
            Err(rpc_error(terminal.code, terminal.message, false)),
        ),
    )
}

fn write_submission_error(
    codec: FrameCodec,
    stream: &mut UnixStream,
    request_id: String,
    error: SubmissionError,
) -> io::Result<()> {
    if error == SubmissionError::UnknownMethod {
        return write_response(
            codec,
            stream,
            &response_for(
                request_id,
                Err(rpc_error(
                    DaemonRpcErrorCode::MethodNotFound,
                    "the requested daemon RPC method is not supported",
                    false,
                )),
            ),
        );
    }
    let lane = match error {
        SubmissionError::LaneQueueFull(lane) => Some(lane.as_str()),
        _ => None,
    };
    let mut busy = rpc_error(
        DaemonRpcErrorCode::Overloaded,
        "the daemon RPC executor is busy",
        true,
    );
    busy.details = Some(serde_json::json!({
        "kind": "busy",
        "scope": error.scope(),
        "lane": lane,
    }));
    write_response(codec, stream, &response_for(request_id, Err(busy)))
}

fn write_response(
    codec: FrameCodec,
    stream: &mut UnixStream,
    response: &DaemonRpcResponse,
) -> io::Result<()> {
    codec.write(stream, response).map_err(codec_io_error)?;
    stream.flush()
}
