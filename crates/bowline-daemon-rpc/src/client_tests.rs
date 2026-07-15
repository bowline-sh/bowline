use std::{
    fs,
    io::Read,
    os::unix::net::UnixListener,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use bowline_core::wire::generated::{
    DaemonRpcErrorCode, MACHINE_CONTRACT_VERSION, WIRE_SCHEMA_HASH,
};
use serde_json::json;

use super::*;
use crate::{ServerNegotiation, negotiate};

static NEXT_SOCKET: AtomicU64 = AtomicU64::new(1);

struct TestSocket(PathBuf);

impl TestSocket {
    fn new(name: &str) -> Self {
        Self(std::env::temp_dir().join(format!(
            "bowline-rpc-{name}-{}-{}.sock",
            std::process::id(),
            NEXT_SOCKET.fetch_add(1, Ordering::Relaxed)
        )))
    }
}

impl Drop for TestSocket {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn serve_handshake(stream: &mut UnixStream, codec: FrameCodec) {
    codec.read_magic(stream).expect("client magic");
    let client: DaemonClientHello = codec.read(stream).expect("client hello");
    let hello = negotiate(
        &client,
        &ServerNegotiation {
            daemon_version: "test-daemon".to_string(),
            capabilities: vec!["test".to_string()],
            instance_id: "daemon-test".to_string(),
        },
    )
    .expect("negotiation succeeds");
    codec.write(stream, &hello).expect("server hello");
}

fn rejects_server_identity_before_opening_transport(
    name: &str,
    contract_version: u16,
    schema_hash: &str,
) -> ClientError {
    let socket = TestSocket::new(name);
    let listener = UnixListener::bind(&socket.0).expect("listener binds");
    let schema_hash = schema_hash.to_string();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client connects");
        let codec = FrameCodec::default();
        codec.read_magic(&mut stream).expect("client magic");
        let _: DaemonClientHello = codec.read(&mut stream).expect("client hello");
        codec
            .write(
                &mut stream,
                &DaemonServerHello {
                    protocol_version: DAEMON_RPC_PROTOCOL_VERSION,
                    contract_version,
                    schema_hash,
                    daemon_version: "test-daemon".to_string(),
                    capabilities: vec!["status.snapshot".to_string()],
                    instance_id: "daemon-test".to_string(),
                },
            )
            .expect("server hello writes");
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("read timeout sets");
        let mut byte = [0_u8; 1];
        assert_eq!(stream.read(&mut byte).expect("client closes cleanly"), 0);
    });
    let error = match DaemonClient::connect(&socket.0, ClientOptions::new("test", "1")) {
        Ok(_) => panic!("identity mismatch opened a transport"),
        Err(error) => error,
    };
    server.join().expect("server exits");
    error
}

#[test]
fn old_contract_is_rejected_before_transport_opens() {
    let error = rejects_server_identity_before_opening_transport(
        "old-contract",
        MACHINE_CONTRACT_VERSION - 1,
        WIRE_SCHEMA_HASH,
    );
    assert!(matches!(error, ClientError::ContractVersionMismatch { .. }));
}

#[test]
fn wrong_schema_hash_is_rejected_before_transport_opens() {
    let error = rejects_server_identity_before_opening_transport(
        "wrong-schema",
        MACHINE_CONTRACT_VERSION,
        "different-schema",
    );
    assert!(matches!(error, ClientError::SchemaHashMismatch { .. }));
}

#[test]
fn concurrent_calls_route_out_of_order_responses() {
    let socket = TestSocket::new("out-of-order");
    let listener = UnixListener::bind(&socket.0).expect("listener binds");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client connects");
        let codec = FrameCodec::default();
        serve_handshake(&mut stream, codec);
        let first: DaemonRpcRequest = codec.read(&mut stream).expect("first request");
        let second: DaemonRpcRequest = codec.read(&mut stream).expect("second request");
        for request in [second, first] {
            codec
                .write(
                    &mut stream,
                    &DaemonRpcResponse {
                        request_id: request.request_id,
                        result: Some(json!({"method": request.method})),
                        error: None,
                    },
                )
                .expect("response writes");
        }
    });
    let client =
        DaemonClient::connect(&socket.0, ClientOptions::new("test", "1")).expect("client connects");
    let left_client = client.clone();
    let left =
        thread::spawn(move || left_client.call::<_, serde_json::Value>("left", &json!({}), None));
    let right = client.call::<_, serde_json::Value>("right", &json!({}), None);
    assert_eq!(right.expect("right result")["method"], "right");
    assert_eq!(
        left.join().expect("left thread").expect("left result")["method"],
        "left"
    );
    server.join().expect("server exits");
}

#[test]
fn structured_remote_error_is_preserved() {
    let socket = TestSocket::new("remote-error");
    let listener = UnixListener::bind(&socket.0).expect("listener binds");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client connects");
        let codec = FrameCodec::default();
        serve_handshake(&mut stream, codec);
        let request: DaemonRpcRequest = codec.read(&mut stream).expect("request reads");
        codec
            .write(
                &mut stream,
                &DaemonRpcResponse {
                    request_id: request.request_id,
                    result: None,
                    error: Some(DaemonRpcError {
                        code: DaemonRpcErrorCode::MethodNotFound,
                        message: "missing".to_string(),
                        retryable: false,
                        retry_after_ms: None,
                        operation_id: None,
                        required_client_version: None,
                        details: None,
                    }),
                },
            )
            .expect("error writes");
    });
    let client =
        DaemonClient::connect(&socket.0, ClientOptions::new("test", "1")).expect("client connects");
    let error = client
        .call::<_, serde_json::Value>("missing", &json!({}), None)
        .expect_err("remote error returns");
    assert!(matches!(
        error,
        ClientError::Remote(remote) if remote.code == DaemonRpcErrorCode::MethodNotFound
    ));
    server.join().expect("server exits");
}

#[test]
fn slow_event_receiver_gets_newest_state_with_resync_marker() {
    let socket = TestSocket::new("event-coalescing");
    let listener = UnixListener::bind(&socket.0).expect("listener binds");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client connects");
        let codec = FrameCodec::default();
        serve_handshake(&mut stream, codec);
        let request: DaemonRpcRequest = codec.read(&mut stream).expect("request reads");
        codec
            .write(
                &mut stream,
                &DaemonRpcResponse {
                    request_id: request.request_id,
                    result: Some(json!({"ready": true})),
                    error: None,
                },
            )
            .expect("response writes");
        for sequence in 1..=3 {
            codec
                .write(
                    &mut stream,
                    &DaemonRpcEvent {
                        subscription_id: "subscription-test".to_string(),
                        sequence,
                        event_kind: "status.snapshot".to_string(),
                        payload: json!({
                            "snapshot": {"sequence": sequence},
                            "gap": false,
                            "resyncRequired": false,
                            "heartbeat": false
                        }),
                    },
                )
                .expect("event writes");
        }
        let barrier: DaemonRpcRequest = codec.read(&mut stream).expect("barrier reads");
        codec
            .write(
                &mut stream,
                &DaemonRpcResponse {
                    request_id: barrier.request_id,
                    result: Some(json!({"barrier": true})),
                    error: None,
                },
            )
            .expect("barrier response writes");
    });
    let client =
        DaemonClient::connect(&socket.0, ClientOptions::new("test", "1")).expect("client connects");
    let events = client
        .register_events("subscription-test", 1)
        .expect("event receiver registers");
    let _: serde_json::Value = client
        .call("trigger", &json!({}), None)
        .expect("trigger responds");
    let _: serde_json::Value = client
        .call("barrier", &json!({}), None)
        .expect("reader barrier responds");
    server.join().expect("server exits");
    let event = events
        .recv_timeout(Duration::from_secs(1))
        .expect("newest event remains available");
    assert_eq!(event.sequence, 3);
    assert_eq!(event.payload["snapshot"]["sequence"], 3);
    assert_eq!(event.payload["gap"], true);
    assert_eq!(event.payload["resyncRequired"], true);
}

#[test]
fn event_arriving_before_registration_is_retained() {
    let socket = TestSocket::new("event-before-registration");
    let listener = UnixListener::bind(&socket.0).expect("listener binds");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client connects");
        let codec = FrameCodec::default();
        serve_handshake(&mut stream, codec);
        let request: DaemonRpcRequest = codec.read(&mut stream).expect("request reads");
        codec
            .write(
                &mut stream,
                &DaemonRpcResponse {
                    request_id: request.request_id,
                    result: Some(json!({"subscriptionId": "subscription-early"})),
                    error: None,
                },
            )
            .expect("response writes");
        codec
            .write(
                &mut stream,
                &DaemonRpcEvent {
                    subscription_id: "subscription-early".to_string(),
                    sequence: 2,
                    event_kind: "status.snapshot".to_string(),
                    payload: json!({
                        "snapshot": {"sequence": 2},
                        "gap": false,
                        "resyncRequired": false,
                        "heartbeat": false
                    }),
                },
            )
            .expect("early event writes");
        let barrier: DaemonRpcRequest = codec.read(&mut stream).expect("barrier reads");
        codec
            .write(
                &mut stream,
                &DaemonRpcResponse {
                    request_id: barrier.request_id,
                    result: Some(json!({"barrier": true})),
                    error: None,
                },
            )
            .expect("barrier response writes");
    });
    let client =
        DaemonClient::connect(&socket.0, ClientOptions::new("test", "1")).expect("client connects");
    let _: serde_json::Value = client
        .call("subscribe", &json!({}), None)
        .expect("subscribe responds");
    let _: serde_json::Value = client
        .call("barrier", &json!({}), None)
        .expect("reader barrier responds");
    let events = client
        .register_events("subscription-early", 1)
        .expect("event receiver registers");

    let event = events
        .recv_timeout(Duration::from_secs(1))
        .expect("early event remains available");
    assert_eq!(event.sequence, 2);
    server.join().expect("server exits");
}

#[test]
fn timeout_sends_cancel_for_the_same_request_id() {
    let socket = TestSocket::new("timeout-cancel");
    let listener = UnixListener::bind(&socket.0).expect("listener binds");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client connects");
        let codec = FrameCodec::default();
        serve_handshake(&mut stream, codec);
        let request: DaemonRpcRequest = codec.read(&mut stream).expect("request reads");
        let cancel: DaemonRpcCancel = codec.read(&mut stream).expect("cancel reads");
        assert_eq!(cancel.request_id, request.request_id);
    });
    let client =
        DaemonClient::connect(&socket.0, ClientOptions::new("test", "1")).expect("client connects");
    let timeout = Duration::from_millis(20);
    let error = client
        .call::<_, serde_json::Value>("slow", &json!({}), Some(timeout))
        .expect_err("slow call times out");
    assert!(matches!(error, ClientError::Timeout { timeout: actual, .. } if actual == timeout));
    server.join().expect("server exits");
}
