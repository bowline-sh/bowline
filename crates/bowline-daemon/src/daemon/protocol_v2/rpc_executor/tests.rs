use super::*;
use crossbeam_channel::{Receiver, bounded};
use serde_json::json;
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Default)]
struct Gate {
    state: Mutex<GateState>,
    changed: Condvar,
}

#[derive(Default)]
struct GateState {
    started: usize,
    released: bool,
}

impl Gate {
    fn block(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.started += 1;
        self.changed.notify_all();
        while !state.released {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn wait_for_started(&self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while state.started < count {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let waited = self
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = waited.0;
            assert!(!waited.1.timed_out(), "worker did not reach the gate");
        }
    }

    fn release(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.released = true;
        self.changed.notify_all();
    }
}

#[test]
fn reserved_status_worker_survives_query_and_mutation_floods() {
    let executor = RpcExecutor::new(RpcExecutorConfig::testing(2, 1)).expect("executor starts");
    let query_gate = Arc::new(Gate::default());
    let mutation_gate = Arc::new(Gate::default());
    let router_query_gate = Arc::clone(&query_gate);
    let router_mutation_gate = Arc::clone(&mutation_gate);
    let router: Arc<RequestRouter> = Arc::new(move |_context, request| {
        if request.method == "daemon.info" {
            router_query_gate.block();
        } else if request.method == "work.accept" {
            router_mutation_gate.block();
        }
        success(request.request_id)
    });
    let (sender, receiver) = bounded(8);
    let query_connection = executor.next_connection_id();
    let mutation_connection = executor.next_connection_id();
    let status_connection = executor.next_connection_id();

    submit(
        &executor,
        query_connection,
        request("query", "daemon.info"),
        Arc::clone(&router),
        &sender,
    )
    .expect("query queues");
    submit(
        &executor,
        mutation_connection,
        request("mutation", "work.accept"),
        Arc::clone(&router),
        &sender,
    )
    .expect("mutation queues");
    query_gate.wait_for_started(1);
    mutation_gate.wait_for_started(1);
    submit(
        &executor,
        status_connection,
        request("status", "status.getSnapshot"),
        router,
        &sender,
    )
    .expect("status queues");

    let completion = receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("reserved status worker responds");
    assert_eq!(completion.request_id.as_str(), "status");
    query_gate.release();
    mutation_gate.release();
    receive_n(&receiver, 2);
    executor.shutdown_and_join().expect("workers join");

    let metrics = executor.metrics();
    assert_eq!(metrics.configured_query_workers, 2);
    assert_eq!(metrics.configured_mutation_workers, 1);
    assert!(metrics.max_active_query <= 2);
    assert!(metrics.max_active_mutation <= 1);
}

#[test]
fn mutation_queue_round_robins_connections() {
    let executor = RpcExecutor::new(RpcExecutorConfig::testing(2, 1)).expect("executor starts");
    let first_gate = Arc::new(Gate::default());
    let starts = Arc::new(Mutex::new(Vec::<String>::new()));
    let router_gate = Arc::clone(&first_gate);
    let router_starts = Arc::clone(&starts);
    let router: Arc<RequestRouter> = Arc::new(move |_context, request| {
        router_starts
            .lock()
            .expect("start order lock")
            .push(request.request_id.clone());
        if request.request_id == "a1" {
            router_gate.block();
        }
        success(request.request_id)
    });
    let (sender, receiver) = bounded(8);
    let connection_a = executor.next_connection_id();
    let connection_b = executor.next_connection_id();

    submit(
        &executor,
        connection_a,
        request("a1", "work.accept"),
        Arc::clone(&router),
        &sender,
    )
    .expect("a1 queues");
    first_gate.wait_for_started(1);
    for request_id in ["a2", "a3"] {
        submit(
            &executor,
            connection_a,
            request(request_id, "work.accept"),
            Arc::clone(&router),
            &sender,
        )
        .expect("a queues");
    }
    submit(
        &executor,
        connection_b,
        request("b1", "work.accept"),
        router,
        &sender,
    )
    .expect("b queues");
    first_gate.release();
    receive_n(&receiver, 4);
    executor.shutdown_and_join().expect("workers join");

    assert_eq!(
        *starts.lock().expect("start order lock"),
        ["a1", "a2", "b1", "a3"]
    );
}

#[test]
fn queue_caps_are_global_lane_and_connection_bounded() {
    let mut config = RpcExecutorConfig::testing(2, 1);
    config.global_queue_capacity = 2;
    config.mutation_queue_capacity = 2;
    config.per_connection_queue_capacity = 1;
    let executor = RpcExecutor::new(config).expect("executor starts");
    let gate = Arc::new(Gate::default());
    let router_gate = Arc::clone(&gate);
    let router: Arc<RequestRouter> = Arc::new(move |_context, request| {
        if request.request_id == "active" {
            router_gate.block();
        }
        success(request.request_id)
    });
    let (sender, receiver) = bounded(8);
    let connection_a = executor.next_connection_id();
    let connection_b = executor.next_connection_id();
    let connection_c = executor.next_connection_id();

    submit(
        &executor,
        connection_a,
        request("active", "work.accept"),
        Arc::clone(&router),
        &sender,
    )
    .expect("active request queues");
    gate.wait_for_started(1);
    submit(
        &executor,
        connection_a,
        request("a-queued", "work.accept"),
        Arc::clone(&router),
        &sender,
    )
    .expect("first queued request fits");
    assert_eq!(
        submit(
            &executor,
            connection_a,
            request("a-over", "work.accept"),
            Arc::clone(&router),
            &sender,
        ),
        Err(SubmissionError::ConnectionQueueFull)
    );
    submit(
        &executor,
        connection_b,
        request("b-queued", "work.accept"),
        Arc::clone(&router),
        &sender,
    )
    .expect("global final slot fits");
    assert_eq!(
        submit(
            &executor,
            connection_c,
            request("global-over", "work.accept"),
            router,
            &sender,
        ),
        Err(SubmissionError::GlobalQueueFull)
    );
    gate.release();
    receive_n(&receiver, 3);
    executor.shutdown_and_join().expect("workers join");
}

#[test]
fn panicked_handler_is_isolated_and_worker_continues() {
    let executor = RpcExecutor::new(RpcExecutorConfig::testing(2, 1)).expect("executor starts");
    let router: Arc<RequestRouter> = Arc::new(move |_context, request| {
        assert_ne!(request.request_id, "panic", "synthetic handler panic");
        success(request.request_id)
    });
    let (sender, receiver) = bounded(4);
    let connection = executor.next_connection_id();
    for request_id in ["panic", "after"] {
        submit(
            &executor,
            connection,
            request(request_id, "daemon.info"),
            Arc::clone(&router),
            &sender,
        )
        .expect("request queues");
    }
    let first = receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("panic completion");
    let second = receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("post-panic completion");
    assert_eq!(
        submit(
            &executor,
            connection,
            request("unknown", "unknown.method"),
            router,
            &sender,
        ),
        Err(SubmissionError::UnknownMethod)
    );
    executor.shutdown_and_join().expect("workers join");

    assert_eq!(first.request_id.as_str(), "panic");
    assert_eq!(
        first.response.error.expect("structured panic error").code,
        DaemonRpcErrorCode::Internal
    );
    assert_eq!(second.request_id.as_str(), "after");
    assert_eq!(second.response.result, Some(json!({"ok": true})));
    let metrics = executor.metrics();
    assert_eq!(metrics.panicked, 1);
    assert_eq!(metrics.completed, 2);
    assert_eq!(metrics.queue_delay_samples, 2);
    assert_eq!(metrics.execution_samples, 2);
    assert_eq!(metrics.active_query, 0);
    assert_eq!(metrics.active_mutation, 0);
    assert_eq!(metrics.queued_status, 0);
    assert_eq!(metrics.queued_query, 0);
    assert_eq!(metrics.queued_mutation, 0);
    assert_eq!(metrics.queued_global, 0);
    assert!(metrics.max_queued_query >= 1);
    assert_eq!(metrics.max_queued_status, 0);
    assert_eq!(metrics.max_queued_mutation, 0);
    assert!(metrics.max_queued_global >= 1);
    assert_eq!(metrics.rejected_busy, 0);
    assert_eq!(metrics.rejected_unknown_method, 1);
    assert_eq!(metrics.cancelled_responses, 0);
    assert_eq!(metrics.deadline_responses, 0);
    assert_eq!(metrics.disconnected_queued, 0);
    assert_eq!(metrics.cancelled_queued, 0);
    assert_eq!(metrics.completion_receivers_gone, 0);
    assert!(metrics.queue_delay_total_nanos >= metrics.queue_delay_max_nanos);
    assert!(metrics.execution_total_nanos >= metrics.execution_max_nanos);
    assert_eq!(metrics.cancellation_latency_total_nanos, 0);
    assert_eq!(metrics.cancellation_latency_max_nanos, 0);
}

#[test]
fn mutation_worker_survives_panicked_handler() {
    let executor = RpcExecutor::new(RpcExecutorConfig::testing(2, 1)).expect("executor starts");
    let router: Arc<RequestRouter> = Arc::new(move |_context, request| {
        assert_ne!(
            request.request_id, "mutation-panic",
            "synthetic mutation panic"
        );
        success(request.request_id)
    });
    let (sender, receiver) = bounded(4);
    let connection = executor.next_connection_id();
    for request_id in ["mutation-panic", "mutation-after"] {
        submit(
            &executor,
            connection,
            request(request_id, "work.accept"),
            Arc::clone(&router),
            &sender,
        )
        .expect("mutation queues");
    }

    let first = receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("panic completion");
    let second = receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("post-panic completion");
    executor.shutdown_and_join().expect("workers join");

    assert_eq!(first.request_id.as_str(), "mutation-panic");
    assert_eq!(
        first.response.error.expect("structured panic error").code,
        DaemonRpcErrorCode::Internal
    );
    assert_eq!(second.request_id.as_str(), "mutation-after");
    assert_eq!(second.response.result, Some(json!({"ok": true})));
    assert_eq!(executor.metrics().panicked, 1);
}

#[test]
fn configured_worker_bounds_hold_across_32_connections_by_16_requests() {
    let executor = RpcExecutor::new(RpcExecutorConfig::default()).expect("executor starts");
    let gate = Arc::new(Gate::default());
    let router_gate = Arc::clone(&gate);
    let router: Arc<RequestRouter> = Arc::new(move |_context, request| {
        router_gate.block();
        success(request.request_id)
    });
    let (sender, receiver) = bounded(512);
    let mut accepted = 0_usize;
    let mut rejected = 0_usize;

    for connection_index in 0..32 {
        let connection_id = executor.next_connection_id();
        for request_index in 0..16 {
            let method = match request_index % 3 {
                0 => "status.getSnapshot",
                1 => "daemon.info",
                _ => "work.accept",
            };
            let request_id = format!("c{connection_index}-r{request_index}");
            match submit(
                &executor,
                connection_id,
                request(&request_id, method),
                Arc::clone(&router),
                &sender,
            ) {
                Ok(()) => accepted += 1,
                Err(
                    SubmissionError::GlobalQueueFull
                    | SubmissionError::LaneQueueFull(_)
                    | SubmissionError::ConnectionQueueFull,
                ) => rejected += 1,
                Err(error) => panic!("unexpected submission error: {error:?}"),
            }
        }
    }
    gate.wait_for_started(QUERY_WORKERS + MUTATION_WORKERS);
    let saturated = executor.metrics();
    assert_eq!(saturated.max_active_query, QUERY_WORKERS);
    assert_eq!(saturated.max_active_mutation, MUTATION_WORKERS);
    assert!(saturated.queued_global <= GLOBAL_QUEUE_CAPACITY);
    assert_eq!(accepted + rejected, 32 * 16);
    gate.release();
    receive_n(&receiver, accepted);
    executor.shutdown_and_join().expect("workers join");

    let final_metrics = executor.metrics();
    assert_eq!(final_metrics.completed, accepted as u64);
    assert_eq!(final_metrics.rejected_busy, rejected as u64);
    assert_eq!(
        final_metrics.enqueued_query + final_metrics.enqueued_mutation,
        accepted as u64
    );
    assert!(final_metrics.max_queued_status <= STATUS_QUEUE_CAPACITY);
    assert!(final_metrics.max_queued_query <= QUERY_QUEUE_CAPACITY);
    assert!(final_metrics.max_queued_mutation <= MUTATION_QUEUE_CAPACITY);
    assert_eq!(final_metrics.max_active_query, QUERY_WORKERS);
    assert_eq!(final_metrics.max_active_mutation, MUTATION_WORKERS);
}

#[test]
fn cancellation_metrics_distinguish_cancel_and_deadline_terminals() {
    let executor = RpcExecutor::new(RpcExecutorConfig::testing(2, 1)).expect("executor starts");
    let connection = executor.next_connection_id();
    let cancelled =
        executor.request_context(connection, RpcRequestId::new("cancelled".to_string()), None);
    cancelled
        .cancellation()
        .cancel(CancellationReason::Cancelled);
    executor.record_terminal_cancellation(cancelled.cancellation());
    let deadline = executor.request_context(
        connection,
        RpcRequestId::new("deadline".to_string()),
        Some(Instant::now()),
    );
    deadline.request_cancellation(CancellationReason::DeadlineExceeded);
    executor.record_terminal_cancellation(deadline.cancellation());
    executor.shutdown_and_join().expect("workers join");

    let metrics = executor.metrics();
    assert_eq!(metrics.cancelled_responses, 1);
    assert_eq!(metrics.deadline_responses, 1);
    assert!(metrics.cancellation_latency_total_nanos >= metrics.cancellation_latency_max_nanos);
}

#[test]
fn strict_shutdown_records_forced_recovery_and_joins_blocked_worker() {
    let executor = RpcExecutor::new(RpcExecutorConfig::testing(2, 1)).expect("executor starts");
    let gate = Arc::new(Gate::default());
    let router_gate = Arc::clone(&gate);
    let router: Arc<RequestRouter> = Arc::new(move |_context, request| {
        router_gate.block();
        success(request.request_id)
    });
    let (sender, receiver) = bounded(2);
    let connection = executor.next_connection_id();
    submit(
        &executor,
        connection,
        request("blocked", "work.accept"),
        router,
        &sender,
    )
    .expect("blocked mutation queues");
    gate.wait_for_started(1);

    let release_gate = Arc::clone(&gate);
    let release = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        release_gate.release();
    });
    let report = executor
        .shutdown_strict(Duration::from_millis(5))
        .expect("strict shutdown joins all workers");
    assert!(report.forced_recovery);
    assert_eq!(report.joined, report.expected);
    receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("worker completes after cooperative release");
    release.join().expect("release thread joins");
}

fn submit(
    executor: &RpcExecutor,
    connection_id: RpcConnectionId,
    request: DaemonRpcRequest,
    router: Arc<RequestRouter>,
    sender: &Sender<RpcCompletion>,
) -> Result<(), SubmissionError> {
    let context = executor.request_context(
        connection_id,
        RpcRequestId::new(request.request_id.clone()),
        request
            .deadline_ms
            .map(|millis| Instant::now() + Duration::from_millis(u64::from(millis))),
    );
    executor.submit(connection_id, context, request, router, sender.clone())
}

fn request(request_id: &str, method: &str) -> DaemonRpcRequest {
    DaemonRpcRequest {
        request_id: request_id.to_string(),
        method: method.to_string(),
        params: json!({}),
        deadline_ms: Some(2_000),
    }
}

fn success(request_id: String) -> DaemonRpcResponse {
    DaemonRpcResponse {
        request_id,
        result: Some(json!({"ok": true})),
        error: None,
    }
}

fn receive_n(receiver: &Receiver<RpcCompletion>, count: usize) {
    for _ in 0..count {
        receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("completion arrives");
    }
}
