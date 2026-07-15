use super::*;

use bowline_core::wire::generated::{
    DaemonClientHello, DaemonRpcErrorCode, DaemonRpcRequest, DaemonRpcResponse, DaemonServerHello,
    MACHINE_CONTRACT_VERSION,
};
use bowline_daemon_rpc::{DAEMON_RPC_PROTOCOL, DAEMON_RPC_PROTOCOL_VERSION, FrameCodec};

#[test]
fn blocking_acceptor_stops_via_self_connect_wake() {
    let socket_dir = unique_socket_dir("acceptor-wake");
    fs::create_dir_all(&socket_dir).expect("socket directory");
    let socket = socket_dir.join("daemon.sock");
    let listener = UnixListener::bind(&socket).expect("listener");
    let state = test_state();
    let acceptor = BlockingAcceptor::start(listener, &socket, state).expect("acceptor starts");

    acceptor.stop().expect("acceptor stop wake");
    let event = acceptor
        .events()
        .recv_timeout(Duration::from_secs(1))
        .expect("acceptor wakes");
    assert!(matches!(event, AcceptorEvent::Stopped));
    acceptor.join().expect("acceptor joins");
    let _cleanup = fs::remove_dir_all(socket_dir);
}

#[test]
fn overloaded_connection_negotiates_then_returns_typed_retry() {
    let state = test_state();
    let server_state = Arc::clone(&state);
    let (mut server, mut client) = UnixStream::pair().expect("socket pair");
    client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("read timeout");
    let worker = std::thread::spawn(move || {
        verify_connection_magic(&mut server).expect("connection magic");
        super::super::protocol_v2::reject_overloaded_connection(
            server,
            &server_state,
            None,
            CONNECTION_BUSY_RETRY_AFTER,
        )
        .expect("busy response");
    });
    client
        .write_all(&bowline_daemon_rpc::CONNECTION_MAGIC)
        .expect("magic writes");
    let codec = FrameCodec::default();
    codec
        .write(
            &mut client,
            &DaemonClientHello {
                protocol: DAEMON_RPC_PROTOCOL.to_string(),
                protocol_version: DAEMON_RPC_PROTOCOL_VERSION,
                contract_version: MACHINE_CONTRACT_VERSION,
                schema_hash: bowline_core::wire::generated::WIRE_SCHEMA_HASH.to_string(),
                client_kind: "acceptor-test".to_string(),
                client_version: "1".to_string(),
                capabilities: Vec::new(),
            },
        )
        .expect("hello writes");
    let hello: DaemonServerHello = codec.read(&mut client).expect("hello reads");
    assert_eq!(hello.instance_id, state.instance_id());
    codec
        .write(
            &mut client,
            &DaemonRpcRequest {
                request_id: "overloaded-request".to_string(),
                method: "daemon.ping".to_string(),
                params: serde_json::json!({}),
                deadline_ms: None,
            },
        )
        .expect("request writes");
    let response: DaemonRpcResponse = codec.read(&mut client).expect("busy response reads");
    let error = response.error.expect("busy error");
    assert_eq!(response.request_id, "overloaded-request");
    assert_eq!(error.code, DaemonRpcErrorCode::Overloaded);
    assert!(error.retryable);
    assert_eq!(error.retry_after_ms, Some(250));
    assert_eq!(error.details.expect("busy details")["scope"], "connection");
    worker.join().expect("busy worker joins");
}

#[test]
fn accept_path_has_no_short_poll_or_per_connection_spawn() {
    let protocol = include_str!("../protocol.rs");
    let executor = include_str!("connection_executor.rs");
    let accept_loop = protocol
        .split_once("fn run_accept_loop")
        .and_then(|(_, tail)| tail.split_once("fn admit_connection"))
        .map(|(accept_loop, _)| accept_loop)
        .expect("run_accept_loop source section");
    let submit = executor
        .split_once("pub(super) fn try_submit")
        .and_then(|(_, tail)| tail.split_once("pub(super) fn completions"))
        .map(|(submit, _)| submit)
        .expect("try_submit source section");

    assert!(!protocol.contains("listener.set_nonblocking(true)"));
    assert!(!accept_loop.contains("WouldBlock"));
    assert!(!accept_loop.contains("thread_sleep_short"));
    assert!(!submit.contains(".spawn("));
}

#[test]
fn connection_admission_is_bounded_by_the_fixed_worker_count() {
    let state = test_state();
    state
        .active_connections
        .store(MAX_CONCURRENT_CONNECTIONS - 1, Ordering::Release);
    assert!(reserve_connection(&state));
    assert_eq!(
        state.active_connections.load(Ordering::Acquire),
        MAX_CONCURRENT_CONNECTIONS
    );
    assert!(!reserve_connection(&state));
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

fn unique_socket_dir(label: &str) -> PathBuf {
    PathBuf::from("/tmp").join(format!(
        "bowline-{label}-{}-{}",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    ))
}
