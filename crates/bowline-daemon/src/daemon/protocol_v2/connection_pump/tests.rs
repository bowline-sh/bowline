use super::*;
use crate::daemon::{DaemonRuntime, NotificationDedupe};
use serde_json::json;
use std::sync::{
    Condvar, Mutex,
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use std::{fs, path::PathBuf, thread};

#[test]
fn slow_request_does_not_block_status_heartbeat_or_second_response() {
    let runtime = DaemonRuntime {
        sync: None,
        notify_approvals: false,
        notification_dedupe: Arc::new(Mutex::new(NotificationDedupe::default())),
        next_notification_poll: Instant::now(),
        pending_notification_status: None,
    };
    let state = Arc::new(DaemonServerState::new(&runtime).expect("daemon state"));
    let (server_stream, mut client_stream) = UnixStream::pair().expect("socket pair");
    client_stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("client read timeout");
    client_stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .expect("client write timeout");

    let slow_started = Arc::new((Mutex::new(false), Condvar::new()));
    let slow_release = Arc::new((Mutex::new(false), Condvar::new()));
    let router_started = Arc::clone(&slow_started);
    let router_release = Arc::clone(&slow_release);
    let router: Arc<RequestRouter> = Arc::new(move |_context, request| {
        if request.method == "agent.tool.invoke" {
            let (started, changed) = &*router_started;
            *started.lock().expect("slow started lock") = true;
            changed.notify_one();
            let (released, changed) = &*router_release;
            let _released = changed
                .wait_while(released.lock().expect("slow release lock"), |released| {
                    !*released
                })
                .expect("slow release wait");
        }
        DaemonRpcResponse {
            request_id: request.request_id,
            result: Some(json!({"ok": true})),
            error: None,
        }
    });
    let executor = Arc::new(
        RpcExecutor::new(super::super::rpc_executor::RpcExecutorConfig::default())
            .expect("executor starts"),
    );
    let connection_id = executor.next_connection_id();
    let server_executor = Arc::clone(&executor);
    let server_state = Arc::clone(&state);
    let server = thread::spawn(move || {
        run_connection_loop(
            server_stream,
            &server_state,
            FrameCodec::default(),
            Duration::from_millis(200),
            router,
            server_executor,
            connection_id,
        )
    });
    let codec = FrameCodec::default();

    write_request(&codec, &mut client_stream, "subscribe", "status.subscribe");
    let subscribe = read_frame(&codec, &mut client_stream);
    assert_eq!(subscribe["requestId"], "subscribe");
    assert!(subscribe["result"]["subscriptionId"].is_string());

    write_request(&codec, &mut client_stream, "slow", "agent.tool.invoke");
    wait_until_started(&slow_started);
    write_request(&codec, &mut client_stream, "ping", "daemon.ping");

    let mut saw_ping = false;
    let mut saw_heartbeat = false;
    while !saw_ping || !saw_heartbeat {
        let frame = read_frame(&codec, &mut client_stream);
        assert_ne!(
            frame["requestId"], "slow",
            "slow response arrived before release"
        );
        saw_ping |= frame["requestId"] == "ping" && frame["result"] == json!({"ok": true});
        saw_heartbeat |=
            frame["eventKind"] == "status.heartbeat" && frame["payload"]["heartbeat"] == true;
    }

    codec
        .write(
            &mut client_stream,
            &DaemonRpcCancel {
                request_id: "slow".to_string(),
            },
        )
        .expect("cancel writes");
    client_stream.flush().expect("cancel flushes");
    loop {
        let frame = read_frame(&codec, &mut client_stream);
        if frame["requestId"] == "slow" {
            assert_eq!(frame["error"]["code"], "cancelled");
            break;
        }
    }
    let (released, changed) = &*slow_release;
    *released.lock().expect("release lock") = true;
    changed.notify_one();
    write_request(&codec, &mut client_stream, "after-cancel", "daemon.ping");
    loop {
        let frame = read_frame(&codec, &mut client_stream);
        assert_ne!(frame["requestId"], "slow", "cancelled response leaked");
        if frame["requestId"] == "after-cancel" {
            assert_eq!(frame["result"], json!({"ok": true}));
            break;
        }
    }

    drop(client_stream);
    server
        .join()
        .expect("connection thread joins")
        .expect("connection loop succeeds");
    assert_eq!(state.connection_reader_thread_counts(), (1, 1));
    executor.shutdown_and_join().expect("executor joins");
}

#[test]
fn connection_pump_contains_no_per_request_spawn_or_fixed_read_poll() {
    let source = include_str!("../connection_pump.rs");
    let dispatch = source
        .split_once("fn dispatch_request")
        .and_then(|(_, tail)| tail.split_once("fn write_active_request_conflict"))
        .map(|(dispatch, _)| dispatch)
        .expect("dispatch_request source section");

    assert!(!dispatch.contains("thread::"));
    assert!(!dispatch.contains(".spawn("));
    assert!(!source.contains("CONNECTION_IO_TIMEOUT"));
    assert!(!source.contains("set_read_timeout"));
}

#[test]
fn cooperative_cancellation_reaches_handler_checkpoint_once() {
    let state = test_state();
    let executor = test_executor();
    let started = Arc::new((Mutex::new(false), Condvar::new()));
    let checkpoint = Arc::new((Mutex::new(false), Condvar::new()));
    let router_started = Arc::clone(&started);
    let router_checkpoint = Arc::clone(&checkpoint);
    let (observed_tx, observed_rx) = mpsc::channel();
    let router: Arc<RequestRouter> = Arc::new(move |context, request| {
        if request.request_id == "cancel-at-checkpoint" {
            signal(&router_started);
            wait_until_released(&router_checkpoint);
            let error = context
                .checkpoint(super::super::request_context::CancellationPoint::BeforeExternalCall)
                .expect_err("cancellation reaches handler");
            observed_tx.send(error.code()).expect("observation sends");
            return DaemonRpcResponse {
                request_id: request.request_id,
                result: None,
                error: Some(*rpc_error(error.code(), error.message(), false)),
            };
        }
        success_response(request.request_id)
    });
    let (mut client, server) = start_connection(&state, &executor, router);
    let codec = FrameCodec::default();

    write_request(
        &codec,
        &mut client,
        "cancel-at-checkpoint",
        "agent.tool.invoke",
    );
    wait_until_started(&started);
    write_cancel(&codec, &mut client, "cancel-at-checkpoint");
    let cancelled = read_until_request(&codec, &mut client, "cancel-at-checkpoint");
    assert_eq!(cancelled["error"]["code"], "cancelled");
    release(&checkpoint);
    assert_eq!(
        observed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("handler observes cancellation"),
        DaemonRpcErrorCode::Cancelled
    );
    write_request(&codec, &mut client, "barrier", "daemon.ping");
    loop {
        let frame = read_frame(&codec, &mut client);
        assert_ne!(frame["requestId"], "cancel-at-checkpoint");
        if frame["requestId"] == "barrier" {
            break;
        }
    }

    close_connection(client, server, &executor);
}

#[test]
fn queued_deadline_prevents_handler_execution_and_responds_once() {
    let state = test_state();
    let executor = test_executor();
    let blocker = Arc::new((Mutex::new(false), Condvar::new()));
    let blocker_release = Arc::new((Mutex::new(false), Condvar::new()));
    let target_calls = Arc::new(AtomicUsize::new(0));
    let router_blocker = Arc::clone(&blocker);
    let router_release = Arc::clone(&blocker_release);
    let router_target_calls = Arc::clone(&target_calls);
    let router: Arc<RequestRouter> = Arc::new(move |_context, request| {
        if request.request_id == "blocker" {
            signal(&router_blocker);
            wait_until_released(&router_release);
        } else if request.request_id == "deadline-target" {
            router_target_calls.fetch_add(1, Ordering::Relaxed);
        }
        success_response(request.request_id)
    });
    let (mut client, server) = start_connection(&state, &executor, router);
    let codec = FrameCodec::default();

    write_request(&codec, &mut client, "blocker", "agent.tool.invoke");
    wait_until_started(&blocker);
    write_request_with_deadline(&codec, &mut client, "deadline-target", "daemon.info", 25);
    let deadline = read_until_request(&codec, &mut client, "deadline-target");
    assert_eq!(deadline["error"]["code"], "deadline_exceeded");
    release(&blocker_release);
    write_request(&codec, &mut client, "barrier", "daemon.ping");
    loop {
        let frame = read_frame(&codec, &mut client);
        assert_ne!(frame["requestId"], "deadline-target");
        if frame["requestId"] == "barrier" {
            break;
        }
    }
    assert_eq!(target_calls.load(Ordering::Relaxed), 0);

    close_connection(client, server, &executor);
}

#[test]
fn running_deadline_reaches_next_handler_checkpoint() {
    let state = test_state();
    let executor = test_executor();
    let started = Arc::new((Mutex::new(false), Condvar::new()));
    let checkpoint = Arc::new((Mutex::new(false), Condvar::new()));
    let router_started = Arc::clone(&started);
    let router_checkpoint = Arc::clone(&checkpoint);
    let (observed_tx, observed_rx) = mpsc::channel();
    let router: Arc<RequestRouter> = Arc::new(move |context, request| {
        if request.request_id == "deadline-at-checkpoint" {
            signal(&router_started);
            wait_until_released(&router_checkpoint);
            let error = context
                .checkpoint(super::super::request_context::CancellationPoint::BeforeExternalCall)
                .expect_err("deadline reaches handler");
            observed_tx.send(error.code()).expect("observation sends");
        }
        success_response(request.request_id)
    });
    let (mut client, server) = start_connection(&state, &executor, router);
    let codec = FrameCodec::default();

    write_request_with_deadline(
        &codec,
        &mut client,
        "deadline-at-checkpoint",
        "agent.tool.invoke",
        25,
    );
    wait_until_started(&started);
    let deadline = read_until_request(&codec, &mut client, "deadline-at-checkpoint");
    assert_eq!(deadline["error"]["code"], "deadline_exceeded");
    release(&checkpoint);
    assert_eq!(
        observed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("handler observes deadline"),
        DaemonRpcErrorCode::DeadlineExceeded
    );
    write_request(&codec, &mut client, "barrier", "daemon.ping");
    loop {
        let frame = read_frame(&codec, &mut client);
        assert_ne!(frame["requestId"], "deadline-at-checkpoint");
        if frame["requestId"] == "barrier" {
            break;
        }
    }

    close_connection(client, server, &executor);
}

#[test]
fn stale_completion_cannot_answer_reused_request_id() {
    let state = test_state();
    let executor = test_executor();
    let first_started = Arc::new((Mutex::new(false), Condvar::new()));
    let first_release = Arc::new((Mutex::new(false), Condvar::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    let router_started = Arc::clone(&first_started);
    let router_release = Arc::clone(&first_release);
    let router_calls = Arc::clone(&calls);
    let router: Arc<RequestRouter> = Arc::new(move |_context, request| {
        let call = router_calls.fetch_add(1, Ordering::Relaxed) + 1;
        if call == 1 {
            signal(&router_started);
            wait_until_released(&router_release);
        }
        DaemonRpcResponse {
            request_id: request.request_id,
            result: Some(json!({"call": call})),
            error: None,
        }
    });
    let (mut client, server) = start_connection(&state, &executor, router);
    let codec = FrameCodec::default();

    write_request(&codec, &mut client, "reused", "agent.tool.invoke");
    wait_until_started(&first_started);
    write_cancel(&codec, &mut client, "reused");
    let cancelled = read_until_request(&codec, &mut client, "reused");
    assert_eq!(cancelled["error"]["code"], "cancelled");
    write_request(&codec, &mut client, "reused", "agent.tool.invoke");
    release(&first_release);
    let replacement = read_until_request(&codec, &mut client, "reused");
    assert_eq!(replacement["result"], json!({"call": 2}));
    write_request(&codec, &mut client, "barrier", "daemon.ping");
    loop {
        let frame = read_frame(&codec, &mut client);
        assert_ne!(frame["requestId"], "reused");
        if frame["requestId"] == "barrier" {
            break;
        }
    }

    close_connection(client, server, &executor);
}

#[test]
fn cancellation_after_commit_fence_returns_real_completion() {
    let state = test_state();
    let executor = test_executor();
    let committed = Arc::new((Mutex::new(false), Condvar::new()));
    let completion_release = Arc::new((Mutex::new(false), Condvar::new()));
    let router_committed = Arc::clone(&committed);
    let router_release = Arc::clone(&completion_release);
    let router: Arc<RequestRouter> = Arc::new(move |context, request| {
        context.begin_commit_fence().expect("commit fence begins");
        signal(&router_committed);
        wait_until_released(&router_release);
        DaemonRpcResponse {
            request_id: request.request_id,
            result: Some(json!({"state": "committed"})),
            error: None,
        }
    });
    let (mut client, server) = start_connection(&state, &executor, router);
    let codec = FrameCodec::default();

    write_request(&codec, &mut client, "committed", "sync.request");
    wait_until_started(&committed);
    write_cancel(&codec, &mut client, "committed");
    release(&completion_release);
    let response = read_until_request(&codec, &mut client, "committed");
    assert_eq!(response["result"], json!({"state": "committed"}));
    assert!(response["error"].is_null());

    close_connection(client, server, &executor);
}

#[test]
fn global_queue_overflow_is_structured_retryable_busy() {
    let state = test_state();
    let mut config = super::super::rpc_executor::RpcExecutorConfig::testing(2, 1);
    config.global_queue_capacity = 1;
    config.mutation_queue_capacity = 1;
    let executor = Arc::new(RpcExecutor::new(config).expect("executor starts"));
    let blocker = Arc::new((Mutex::new(false), Condvar::new()));
    let blocker_release = Arc::new((Mutex::new(false), Condvar::new()));
    let router_blocker = Arc::clone(&blocker);
    let router_release = Arc::clone(&blocker_release);
    let router: Arc<RequestRouter> = Arc::new(move |_context, request| {
        if request.request_id == "active" {
            signal(&router_blocker);
            wait_until_released(&router_release);
        }
        success_response(request.request_id)
    });
    let (mut client, server) = start_connection(&state, &executor, router);
    let codec = FrameCodec::default();

    write_request(&codec, &mut client, "active", "sync.request");
    wait_until_started(&blocker);
    write_request(&codec, &mut client, "queued", "sync.request");
    write_request(&codec, &mut client, "busy", "sync.request");
    let busy = read_until_request(&codec, &mut client, "busy");
    assert_eq!(busy["error"]["code"], "overloaded");
    assert_eq!(busy["error"]["retryable"], true);
    assert_eq!(busy["error"]["retryAfterMs"], 250);
    assert_eq!(busy["error"]["details"]["kind"], "busy");
    assert_eq!(busy["error"]["details"]["scope"], "global");
    release(&blocker_release);
    let mut remaining = 2;
    while remaining > 0 {
        let frame = read_frame(&codec, &mut client);
        if matches!(frame["requestId"].as_str(), Some("active" | "queued")) {
            remaining -= 1;
        }
    }

    close_connection(client, server, &executor);
}

#[test]
fn enqueue_survives_disconnect_after_durable_commit_fence() {
    let state_root = unique_state_root("enqueue-disconnect");
    let sync = crate::daemon::tests::watcher_test_runtime(
        state_root.join("Code"),
        state_root.clone(),
        "ws_enqueue_disconnect",
    );
    let state = Arc::new(
        DaemonServerState::new(&DaemonRuntime {
            sync: Some(sync),
            notify_approvals: false,
            notification_dedupe: Arc::new(Mutex::new(NotificationDedupe::default())),
            next_notification_poll: Instant::now(),
            pending_notification_status: None,
        })
        .expect("daemon state"),
    );
    let executor = test_executor();
    let committed = Arc::new((Mutex::new(false), Condvar::new()));
    let response_release = Arc::new((Mutex::new(false), Condvar::new()));
    let operation_id = Arc::new(Mutex::new(None::<String>));
    let router_state = Arc::clone(&state);
    let router_committed = Arc::clone(&committed);
    let router_release = Arc::clone(&response_release);
    let router_operation_id = Arc::clone(&operation_id);
    let router: Arc<RequestRouter> = Arc::new(move |context, request| {
        context.begin_commit_fence().expect("commit fence begins");
        let (operation, _) = router_state
            .enqueue_sync("disconnect-after-enqueue")
            .expect("durable enqueue succeeds");
        *router_operation_id.lock().expect("operation id lock") = Some(operation.id.clone());
        signal(&router_committed);
        wait_until_released(&router_release);
        DaemonRpcResponse {
            request_id: request.request_id,
            result: Some(json!({"operationId": operation.id})),
            error: None,
        }
    });
    let (mut client, server) = start_connection(&state, &executor, router);
    let codec = FrameCodec::default();

    write_request(&codec, &mut client, "enqueue", "sync.request");
    wait_until_started(&committed);
    drop(client);
    server
        .join()
        .expect("connection thread joins")
        .expect("disconnect cleanup succeeds");
    let operation_id = operation_id
        .lock()
        .expect("operation id lock")
        .clone()
        .expect("operation id recorded");
    assert!(
        state
            .sync_operation(&operation_id)
            .expect("operation reads")
            .is_some(),
        "disconnect cannot roll back the durable enqueue"
    );
    let (retry, coalesced) = state
        .enqueue_sync("disconnect-after-enqueue")
        .expect("idempotent retry reads durable enqueue");
    assert!(coalesced);
    assert_eq!(retry.id, operation_id);
    release(&response_release);
    executor.shutdown_and_join().expect("executor joins");
    let _ = fs::remove_dir_all(state_root);
}

#[test]
fn disconnect_removes_queued_work_and_releases_connection_capacity() {
    let state = test_state();
    let executor = test_executor();
    let active_started = Arc::new((Mutex::new(false), Condvar::new()));
    let active_release = Arc::new((Mutex::new(false), Condvar::new()));
    let queued_calls = Arc::new(AtomicUsize::new(0));
    let router_started = Arc::clone(&active_started);
    let router_release = Arc::clone(&active_release);
    let router_queued_calls = Arc::clone(&queued_calls);
    let router: Arc<RequestRouter> = Arc::new(move |_context, request| {
        if request.request_id == "active" {
            signal(&router_started);
            wait_until_released(&router_release);
        } else if request.request_id == "queued" {
            router_queued_calls.fetch_add(1, Ordering::Relaxed);
        }
        success_response(request.request_id)
    });
    let (mut client, server) = start_connection(&state, &executor, router);
    let codec = FrameCodec::default();

    write_request(&codec, &mut client, "active", "agent.tool.invoke");
    wait_until_started(&active_started);
    write_request(&codec, &mut client, "queued", "daemon.info");
    drop(client);
    server
        .join()
        .expect("connection thread joins")
        .expect("disconnect cleanup succeeds");
    release(&active_release);
    executor.shutdown_and_join().expect("executor joins");

    assert_eq!(queued_calls.load(Ordering::Relaxed), 0);
    assert_eq!(executor.disconnected_queued(), 1);
}

#[test]
fn seventeenth_in_flight_request_hits_connection_cap() {
    let state = test_state();
    let executor = test_executor();
    let active_started = Arc::new((Mutex::new(false), Condvar::new()));
    let active_release = Arc::new((Mutex::new(false), Condvar::new()));
    let queued_calls = Arc::new(AtomicUsize::new(0));
    let router_started = Arc::clone(&active_started);
    let router_release = Arc::clone(&active_release);
    let router_queued_calls = Arc::clone(&queued_calls);
    let router: Arc<RequestRouter> = Arc::new(move |_context, request| {
        if request.request_id == "request-0" {
            signal(&router_started);
            wait_until_released(&router_release);
        } else {
            router_queued_calls.fetch_add(1, Ordering::Relaxed);
        }
        success_response(request.request_id)
    });
    let (mut client, server) = start_connection(&state, &executor, router);
    let codec = FrameCodec::default();

    write_request(&codec, &mut client, "request-0", "sync.request");
    wait_until_started(&active_started);
    for index in 1..=MAX_IN_FLIGHT_REQUESTS {
        write_request(
            &codec,
            &mut client,
            &format!("request-{index}"),
            "sync.request",
        );
    }
    let overloaded = read_until_request(&codec, &mut client, "request-16");
    assert_eq!(overloaded["error"]["code"], "overloaded");
    assert_eq!(overloaded["error"]["retryable"], true);
    assert_eq!(overloaded["error"]["retryAfterMs"], 250);

    drop(client);
    server
        .join()
        .expect("connection thread joins")
        .expect("disconnect cleanup succeeds");
    release(&active_release);
    executor.shutdown_and_join().expect("executor joins");
    assert_eq!(queued_calls.load(Ordering::Relaxed), 0);
}

fn write_request(codec: &FrameCodec, stream: &mut UnixStream, request_id: &str, method: &str) {
    write_request_with_deadline(codec, stream, request_id, method, 2_000);
}

fn write_request_with_deadline(
    codec: &FrameCodec,
    stream: &mut UnixStream,
    request_id: &str,
    method: &str,
    deadline_ms: u32,
) {
    codec
        .write(
            stream,
            &DaemonRpcRequest {
                request_id: request_id.to_string(),
                method: method.to_string(),
                params: json!({}),
                deadline_ms: Some(deadline_ms),
            },
        )
        .expect("request writes");
    stream.flush().expect("request flushes");
}

fn write_cancel(codec: &FrameCodec, stream: &mut UnixStream, request_id: &str) {
    codec
        .write(
            stream,
            &DaemonRpcCancel {
                request_id: request_id.to_string(),
            },
        )
        .expect("cancel writes");
    stream.flush().expect("cancel flushes");
}

fn read_frame(codec: &FrameCodec, stream: &mut UnixStream) -> serde_json::Value {
    codec.read(stream).expect("frame reads")
}

fn wait_until_started(slow_started: &(Mutex<bool>, Condvar)) {
    let (started, changed) = slow_started;
    let started = changed
        .wait_timeout_while(
            started.lock().expect("started lock"),
            Duration::from_secs(2),
            |started| !*started,
        )
        .expect("started wait");
    assert!(*started.0, "slow request did not start");
}

fn signal(signal: &(Mutex<bool>, Condvar)) {
    let (started, changed) = signal;
    *started.lock().expect("signal lock") = true;
    changed.notify_all();
}

fn wait_until_released(release: &(Mutex<bool>, Condvar)) {
    let (released, changed) = release;
    let _released = changed
        .wait_while(released.lock().expect("release lock"), |released| {
            !*released
        })
        .expect("release wait");
}

fn release(release: &(Mutex<bool>, Condvar)) {
    let (released, changed) = release;
    *released.lock().expect("release lock") = true;
    changed.notify_all();
}

fn test_state() -> Arc<DaemonServerState> {
    Arc::new(
        DaemonServerState::new(&DaemonRuntime {
            sync: None,
            notify_approvals: false,
            notification_dedupe: Arc::new(Mutex::new(NotificationDedupe::default())),
            next_notification_poll: Instant::now(),
            pending_notification_status: None,
        })
        .expect("daemon state"),
    )
}

fn test_executor() -> Arc<RpcExecutor> {
    Arc::new(
        RpcExecutor::new(super::super::rpc_executor::RpcExecutorConfig::testing(2, 1))
            .expect("executor starts"),
    )
}

fn start_connection(
    state: &Arc<DaemonServerState>,
    executor: &Arc<RpcExecutor>,
    router: Arc<RequestRouter>,
) -> (UnixStream, thread::JoinHandle<io::Result<()>>) {
    let (server_stream, client_stream) = UnixStream::pair().expect("socket pair");
    client_stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("client read timeout");
    client_stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .expect("client write timeout");
    let server_state = Arc::clone(state);
    let server_executor = Arc::clone(executor);
    let connection_id = executor.next_connection_id();
    let server = thread::spawn(move || {
        run_connection_loop(
            server_stream,
            &server_state,
            FrameCodec::default(),
            Duration::from_millis(200),
            router,
            server_executor,
            connection_id,
        )
    });
    (client_stream, server)
}

fn close_connection(
    client: UnixStream,
    server: thread::JoinHandle<io::Result<()>>,
    executor: &RpcExecutor,
) {
    drop(client);
    server
        .join()
        .expect("connection thread joins")
        .expect("connection loop succeeds");
    executor.shutdown_and_join().expect("executor joins");
}

fn read_until_request(
    codec: &FrameCodec,
    stream: &mut UnixStream,
    request_id: &str,
) -> serde_json::Value {
    loop {
        let frame = read_frame(codec, stream);
        if frame["requestId"] == request_id {
            return frame;
        }
    }
}

fn success_response(request_id: String) -> DaemonRpcResponse {
    DaemonRpcResponse {
        request_id,
        result: Some(json!({"ok": true})),
        error: None,
    }
}

fn unique_state_root(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "bowline-p110-{label}-{}-{}",
        std::process::id(),
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    ))
}
